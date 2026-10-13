//! Layer 3: the master, the process that owns a session.
//!
//! It holds the pty the program runs on, keeps the recent output in a ring,
//! and serves any number of clients from it. One thread, one poll loop.
//!
//! The rule everything here follows: the program is never made to wait for a
//! client. Each client is a cursor into the ring; a client that cannot keep
//! up loses output, and nobody else notices.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustix::fs::OFlags;

use crate::proto::{
    Attach, ClientMsg, Decoder, Exit, Redraw, Replay, ServerMsg, MAX_PAYLOAD, VERSION,
};
use crate::ring::Ring;
use crate::session::{Info, Lock, Names, Probe, Session, SockId};
use crate::sys::{self, signal, PollFd, PollFlags, Signals, Winsize};
use crate::vt::{Filter, Modes, Parser, Token};

/// Output kept in memory. Independent of the log: a session without a log
/// still has history to show.
pub const RING_SIZE: usize = 1 << 20;
/// History shown to a client that does not ask for a specific amount.
pub const REPLAY_DEFAULT: u64 = 128 * 1024;
/// Keyboard input waiting for the program to read it.
const INPUT_MAX: usize = 64 * 1024;
const MAX_CLIENTS: usize = 64;
/// Encoded output a client may have waiting before we stop preparing more.
const OUT_HIGH: usize = 16 * 1024;
/// How long the creator of a session has to attach before the program is
/// allowed to run ahead without it.
const FIRST_ATTACH: Duration = Duration::from_secs(5);
/// How long clients get to read the last output after the program is gone.
const DRAIN: Duration = Duration::from_secs(2);

pub struct Config {
    pub names: Names,
    pub argv: Vec<OsString>,
    /// Hold the program's output until the first client attaches.
    pub wait_attach: bool,
    pub redraw: Redraw,
    /// Size limit of the log file; 0 means no log.
    pub log_cap: u64,
    /// Print `ok` once the session is up, for a parent that is waiting.
    pub announce: bool,
}

/// The log file: raw output, appended, trimmed when it outgrows its cap.
struct Log {
    file: File,
    size: u64,
    cap: u64,
}

impl Log {
    fn open(session: &Session, cap: u64) -> Option<Log> {
        if cap == 0 {
            return None;
        }
        let file = session
            .open_file(".log", OFlags::RDWR | OFlags::CREATE | OFlags::APPEND)
            .ok()?;
        let size = file.metadata().ok()?.len();
        let mut log = Log { file, size, cap };
        if size > cap {
            log.trim(session);
        }
        Some(log)
    }

    /// The last `max` bytes of the log.
    fn tail(&mut self, max: u64) -> Vec<u8> {
        let mut tail = Vec::new();
        let start = self.size.saturating_sub(max);
        if self.file.seek(SeekFrom::Start(start)).is_ok() {
            let _ = (&self.file).take(max).read_to_end(&mut tail);
        }
        tail
    }

    /// Cut the file down to its cap by writing the tail to a new file and
    /// renaming it into place, so a crash leaves one whole log or the other.
    fn trim(&mut self, session: &Session) -> bool {
        let tail = self.tail(self.cap);
        if session.replace_file(".log", &tail).is_err() {
            return false;
        }
        match session.open_file(".log", OFlags::RDWR | OFlags::APPEND) {
            Ok(file) => {
                self.file = file;
                self.size = tail.len() as u64;
                true
            }
            Err(_) => false,
        }
    }

    /// Returns false when the log has failed and should be given up on.
    fn append(&mut self, session: &Session, data: &[u8]) -> bool {
        if self.file.write_all(data).is_err() {
            return false;
        }
        self.size += data.len() as u64;
        // Let it overshoot before trimming, or every write would copy it.
        if self.size >= self.cap + self.cap / 4 + 4096 {
            return self.trim(session);
        }
        true
    }

    fn truncate(&mut self) {
        if self.file.set_len(0).is_ok() {
            self.size = 0;
        }
    }
}

enum Phase {
    /// Connected, not receiving output.
    Idle,
    /// Receiving history up to `end`, with terminal queries removed.
    Replay {
        end: u64,
        filter: Filter,
    },
    Live,
}

struct Client {
    stream: UnixStream,
    decoder: Decoder,
    /// Encoded frames not yet written, from `out_at` on.
    out: Vec<u8>,
    out_at: usize,
    /// Next byte of the ring this client is owed.
    pos: u64,
    phase: Phase,
    greeted: bool,
    readonly: bool,
    /// Its input did not fit; not read again until the program catches up.
    paused: bool,
    /// The other side hung up; kept only until its last frames are handled.
    eof: bool,
    /// To be dropped once `out` has been written.
    closing: bool,
    dead: bool,
}

impl Client {
    fn new(stream: UnixStream) -> Client {
        Client {
            stream,
            decoder: Decoder::new(),
            out: Vec::new(),
            out_at: 0,
            pos: 0,
            phase: Phase::Idle,
            greeted: false,
            readonly: false,
            paused: false,
            eof: false,
            closing: false,
            dead: false,
        }
    }

    fn send(&mut self, msg: ServerMsg<'_>) {
        msg.encode(&mut self.out);
    }

    fn has_output(&self) -> bool {
        self.out_at < self.out.len()
    }

    fn attached(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }

    /// Write as much pending output as the socket takes.
    fn flush(&mut self) {
        while self.has_output() {
            match self.stream.write(&self.out[self.out_at..]) {
                Ok(0) => {
                    self.dead = true;
                    return;
                }
                Ok(n) => self.out_at += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.dead = true;
                    return;
                }
            }
        }
        self.out.clear();
        self.out_at = 0;
    }

    /// This client fell off the back of the ring. Move it to a point where
    /// the stream can be picked up, and say what happened.
    fn skip(&mut self, ring: &Ring, modes: &Modes, ground: bool) {
        self.pos = if ground {
            ring.total()
        } else {
            ring.last_mark()
                .filter(|&m| m >= ring.oldest())
                .unwrap_or(ring.total())
        };
        // Its terminal may be in the middle of a sequence we never finished:
        // CAN cancels one, ST ends a string, and both are harmless otherwise.
        let mut resume = b"\x18\x1b\\".to_vec();
        self.send(ServerMsg::Output(&resume));
        self.send(ServerMsg::Skipped);
        resume.clear();
        modes.restore(&mut resume);
        self.send(ServerMsg::Output(&resume));
        if matches!(self.phase, Phase::Replay { .. }) {
            self.send(ServerMsg::ReplayEnd);
        }
        self.phase = Phase::Live;
    }

    /// Turn ring bytes this client is owed into frames, up to a limit.
    fn fill(&mut self, ring: &Ring, modes: &Modes, ground: bool) {
        while self.out.len() - self.out_at < OUT_HIGH {
            let limit = match &self.phase {
                Phase::Idle => return,
                Phase::Replay { end, .. } => *end,
                Phase::Live => ring.total(),
            };
            if self.pos >= limit {
                let Phase::Replay { filter, .. } = &mut self.phase else {
                    return;
                };
                // History is done. Release what the filter held back, bring
                // the terminal into the program's modes, and go live.
                let mut tail = Vec::new();
                filter.finish(&mut tail);
                modes.restore(&mut tail);
                self.send(ServerMsg::Output(&tail));
                self.send(ServerMsg::ReplayEnd);
                self.phase = Phase::Live;
                continue;
            }
            let Some(chunk) = ring.read(self.pos) else {
                self.skip(ring, modes, ground);
                continue;
            };
            let len = chunk
                .len()
                .min((limit - self.pos) as usize)
                .min(MAX_PAYLOAD);
            let chunk = &chunk[..len];
            self.pos += len as u64;
            if let Phase::Replay { filter, .. } = &mut self.phase {
                let mut kept = Vec::with_capacity(len);
                filter.feed(chunk, &mut kept);
                ServerMsg::Output(&kept).encode(&mut self.out);
            } else {
                ServerMsg::Output(chunk).encode(&mut self.out);
            }
        }
    }
}

/// A client message, detached from the buffer it was decoded from.
enum Cmd {
    Hello,
    Attach(Attach),
    Detach,
    Resize(u16, u16),
    Redraw(Redraw, u16, u16),
    Signal(u8),
    Clear,
    Bad(String),
}

struct Master<'a> {
    session: &'a Session,
    cfg: &'a Config,
    listener: UnixListener,
    sock_id: SockId,
    pty: OwnedFd,
    pty_open: bool,
    child: Child,
    status: Option<ExitStatus>,
    ring: Ring,
    parser: Parser,
    modes: Modes,
    /// Offset of the next byte the parser will turn into a token.
    parsed: u64,
    input: VecDeque<u8>,
    clients: Vec<Client>,
    log: Option<Log>,
    info: Info,
    redraw: Redraw,
    first_attach: Option<Instant>,
    attached_bit: bool,
    drain_until: Option<Instant>,
    signals: Signals,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `1d 2h 3m 4s`, leaving out the leading zero units.
pub fn format_age(secs: u64) -> String {
    let (d, h, m, s) = (
        secs / 86400,
        secs % 86400 / 3600,
        secs % 3600 / 60,
        secs % 60,
    );
    if d > 0 {
        format!("{d}d {h}h {m}m {s}s")
    } else if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// Take the name: under the lock, make sure nobody live has it, clear away a
/// dead socket, and bind.
fn claim(session: &Session, names: &Names) -> Result<(UnixListener, SockId, Lock), String> {
    let fail = |e: &io::Error| {
        format!(
            "{}: {}: {}",
            names.progname,
            session.sock.display(),
            sys::describe(e)
        )
    };
    let lock = session.lock().map_err(|e| fail(&e))?;
    match session.probe() {
        Probe::Live(_) => {
            return Err(format!(
                "{}: session '{}' is already running",
                names.progname,
                session.name()
            ))
        }
        Probe::Stale => {
            session.remove_stale_socket();
        }
        Probe::Missing => {}
        Probe::NotASocket => {
            return Err(fail(&io::Error::from(io::ErrorKind::AlreadyExists)));
        }
        Probe::Failed(e) => return Err(fail(&e)),
    }
    let (listener, id) = session.bind().map_err(|e| fail(&e))?;
    Ok((listener, id, lock))
}

/// The environment the program gets: the chain of sessions it is nested in,
/// by socket path and, in step with it, by session id.
fn session_env(names: &Names, session: &Session, id: u64) -> Vec<(OsString, OsString)> {
    let chain = |var: String, value: OsString| {
        let mut full = std::env::var_os(&var)
            .filter(|v| !v.is_empty())
            .unwrap_or_default();
        if !full.is_empty() {
            full.push(":");
        }
        full.push(value);
        (OsString::from(var), full)
    };
    let var = names.session_var();
    vec![
        chain(format!("{var}_ID"), format!("{id:016x}").into()),
        chain(var, session.sock.clone().into_os_string()),
    ]
}

/// Run a session's master in this process until the session ends.
///
/// Setup failures are printed to stdout and return 1. Once the session is
/// up, `ok` is printed, stdio is pointed at `/dev/null`, and the return value
/// is the program's exit code.
pub fn run(session: &Session, cfg: &Config) -> i32 {
    let names = &cfg.names;
    // Ours is the terminal the session is created from; the program starts
    // with its settings. Look before we let go of it.
    let termios = sys::termios_of(io::stdin());
    let size = sys::winsize_of(io::stdin());
    sys::new_session();

    let (listener, sock_id, lock) = match claim(session, names) {
        Ok(claimed) => claimed,
        Err(message) => {
            let _ = writeln!(io::stdout(), "{message}");
            return 1;
        }
    };
    let id = sys::random_u64();
    let abandon = |message: String| {
        let _ = writeln!(io::stdout(), "{message}");
        session.remove_socket(sock_id);
        1
    };

    let mut log = Log::open(session, cfg.log_cap);
    let (pty, slave) = match sys::open_pty(termios.as_ref(), size) {
        Ok(pair) => pair,
        Err(e) => {
            return abandon(format!(
                "{}: init_pty: {}",
                names.progname,
                sys::describe(&e)
            ))
        }
    };
    let child = match sys::spawn_on_pty(slave, &cfg.argv, &session_env(names, session, id)) {
        Ok(child) => child,
        Err(e) => {
            return abandon(format!(
                "{}: could not execute {}: {}",
                names.progname,
                cfg.argv[0].to_string_lossy(),
                sys::describe(&e)
            ))
        }
    };
    let signals = match Signals::new(&[
        signal::SIGCHLD,
        signal::SIGTERM,
        signal::SIGINT,
        // Caught so that they do not kill us; nothing acts on them.
        signal::SIGHUP,
        signal::SIGTTIN,
        signal::SIGTTOU,
        signal::SIGXFSZ,
    ]) {
        Ok(signals) => signals,
        Err(e) => {
            return abandon(format!(
                "{}: signals: {}",
                names.progname,
                sys::describe(&e)
            ))
        }
    };
    let _ = sys::set_nonblocking(&pty);

    let info = Info {
        id,
        pid: std::process::id(),
        child: child.id(),
        started: unix_now(),
        command: cfg
            .argv
            .iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" "),
        ..Info::default()
    };
    let _ = info.write(session);

    // History from an earlier session of the same name is still history.
    let mut ring = Ring::new(RING_SIZE);
    if let Some(log) = log.as_mut() {
        preload(&mut ring, &log.tail(RING_SIZE as u64));
    }
    let parsed = ring.total();

    if cfg.announce {
        let _ = writeln!(io::stdout(), "ok");
        let _ = io::stdout().flush();
    }
    let _ = sys::detach_stdio();
    drop(lock);

    let mut master = Master {
        session,
        cfg,
        listener,
        sock_id,
        pty,
        pty_open: true,
        child,
        status: None,
        ring,
        parser: Parser::new(),
        modes: Modes::new(),
        parsed,
        input: VecDeque::new(),
        clients: Vec::new(),
        log,
        info,
        redraw: match cfg.redraw {
            Redraw::Unspec if termios.is_some() => Redraw::Winch,
            Redraw::Unspec => Redraw::None,
            other => other,
        },
        first_attach: cfg.wait_attach.then(|| Instant::now() + FIRST_ATTACH),
        attached_bit: false,
        drain_until: None,
        signals,
    };
    let killed = master.serve();
    master.finish(killed)
}

/// Load old output into the ring, marking the line starts so that replay has
/// somewhere safe to begin.
fn preload(ring: &mut Ring, old: &[u8]) {
    let mut parser = Parser::new();
    for chunk in old.chunks(4096) {
        let base = ring.total();
        ring.push(chunk);
        if let Some(mark) = safe_cut(&mut parser, chunk, base) {
            ring.mark(mark);
        }
    }
}

/// Feed `chunk`, which starts at offset `base`, to `parser` and return the
/// last offset in it where the stream may be cut: after a newline in plain
/// text, or at the very end if no sequence or character is left open.
fn safe_cut(parser: &mut Parser, chunk: &[u8], base: u64) -> Option<u64> {
    let mut at = base - parser.pending() as u64;
    let mut cut = None;
    parser.feed(chunk, |token| {
        if let Token::Text(text) = token {
            if let Some(i) = text.iter().rposition(|&b| b == b'\n') {
                cut = Some(at + i as u64 + 1);
            }
        }
        at += token.raw().len() as u64;
    });
    if parser.is_ground() {
        cut = Some(base + chunk.len() as u64);
    }
    cut
}

impl Master<'_> {
    /// The poll loop. Returns true if a signal told us to stop.
    fn serve(&mut self) -> bool {
        loop {
            self.advance_clients();
            self.update_attached_bit();
            if self.finished() {
                return false;
            }

            if self.first_attach.is_some_and(|t| Instant::now() >= t) {
                self.first_attach = None;
            }
            let reading = self.pty_open && self.first_attach.is_none();
            let mut pty_events = PollFlags::empty();
            if reading {
                pty_events |= PollFlags::IN;
            }
            if self.pty_open && !self.input.is_empty() {
                pty_events |= PollFlags::OUT;
            }
            let timeout = [self.first_attach, self.drain_until]
                .into_iter()
                .flatten()
                .map(|t| t.saturating_duration_since(Instant::now()).as_millis() as u32 + 1)
                .min();

            let mut fds = Vec::with_capacity(self.clients.len() + 3);
            fds.push(PollFd::from_borrowed_fd(self.signals.fd(), PollFlags::IN));
            fds.push(PollFd::new(&self.listener, PollFlags::IN));
            fds.push(PollFd::new(&self.pty, pty_events));
            for client in &self.clients {
                let mut events = PollFlags::empty();
                if !client.paused && !client.eof && !client.closing {
                    events |= PollFlags::IN;
                }
                if client.has_output() {
                    events |= PollFlags::OUT;
                }
                fds.push(PollFd::new(&client.stream, events));
            }
            if sys::poll(&mut fds, timeout).is_err() {
                return true;
            }
            let ready: Vec<PollFlags> = fds.iter().map(|fd| fd.revents()).collect();
            drop(fds);

            if !ready[0].is_empty() {
                self.signals.drain();
                if self.signals.take(signal::SIGTERM) || self.signals.take(signal::SIGINT) {
                    return true;
                }
            }
            // Asked about on every turn, not only when the doorbell rang:
            // it is cheap, and a missed child is a session that never ends.
            if self.status.is_none() {
                if let Ok(Some(status)) = self.child.try_wait() {
                    self.status = Some(status);
                }
            }
            if !ready[1].is_empty() {
                self.accept();
            }
            let bad = PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL;
            if ready[2].intersects(PollFlags::OUT) {
                self.write_input();
            }
            if reading && ready[2].intersects(PollFlags::IN | bad) {
                self.read_pty();
            }
            for (i, events) in ready[3..].iter().enumerate() {
                if events.intersects(PollFlags::IN | bad) {
                    self.read_client(i);
                }
                if events.intersects(PollFlags::OUT) {
                    self.clients[i].flush();
                }
            }
            self.clients.retain(|c| !c.dead);
        }
    }

    /// Handle what clients have sent and prepare what they are owed.
    fn advance_clients(&mut self) {
        if self.input.len() < INPUT_MAX / 2 {
            for client in &mut self.clients {
                client.paused = false;
            }
        }
        for i in 0..self.clients.len() {
            self.handle_frames(i);
        }
        let ground = self.parser.is_ground();
        for client in &mut self.clients {
            // Prepare and write in turns until the client is caught up or
            // its socket is full; stopping earlier would leave it waiting
            // for an event that is not coming.
            loop {
                client.fill(&self.ring, &self.modes, ground);
                if !client.has_output() {
                    break;
                }
                client.flush();
                if client.has_output() || client.dead {
                    break;
                }
            }
            if client.closing && !client.has_output() {
                client.dead = true;
            }
        }
        self.clients.retain(|c| !c.dead);
    }

    fn update_attached_bit(&mut self) {
        let attached = self.clients.iter().any(|c| c.attached() && !c.readonly);
        if attached != self.attached_bit {
            self.session.set_attached(attached);
            self.attached_bit = attached;
        }
    }

    /// The session is over when the pty has closed, the program has been
    /// reaped, and attached clients have read what there was to read.
    fn finished(&mut self) -> bool {
        if self.pty_open || self.status.is_none() {
            return false;
        }
        let total = self.ring.total();
        let caught_up = |c: &Client| !c.attached() || (c.pos >= total && !c.has_output());
        let until = *self
            .drain_until
            .get_or_insert_with(|| Instant::now() + DRAIN);
        self.clients.iter().all(caught_up) || Instant::now() >= until
    }

    fn accept(&mut self) {
        while let Ok((stream, _)) = self.listener.accept() {
            if self.clients.len() >= MAX_CLIENTS || !sys::peer_is_us(&stream) {
                continue;
            }
            if stream.set_nonblocking(true).is_ok() {
                self.clients.push(Client::new(stream));
            }
        }
    }

    fn read_pty(&mut self) {
        let mut buf = [0u8; 8192];
        match rustix::io::read(&self.pty, &mut buf) {
            Ok(0) => self.pty_open = false,
            Ok(n) => self.on_output(&buf[..n]),
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {}
            // Linux reports the end of a pty as an I/O error.
            Err(_) => self.pty_open = false,
        }
    }

    fn on_output(&mut self, data: &[u8]) {
        let base = self.ring.total();
        debug_assert_eq!(self.parsed + self.parser.pending() as u64, base);
        self.ring.push(data);

        let Master {
            parser,
            modes,
            parsed,
            ..
        } = self;
        let mut at = *parsed;
        let mut cut = None;
        parser.feed(data, |token| {
            modes.apply(&token);
            if let Token::Text(text) = token {
                if let Some(i) = text.iter().rposition(|&b| b == b'\n') {
                    cut = Some(at + i as u64 + 1);
                }
            }
            at += token.raw().len() as u64;
        });
        *parsed = at;
        if let Some(cut) = cut {
            self.ring.mark(cut);
        }
        if self.parser.is_ground() {
            self.ring.mark(self.ring.total());
        }

        if let Some(log) = self.log.as_mut() {
            if !log.append(self.session, data) {
                // A full disk costs the log, not the session.
                self.log = None;
            }
        }
    }

    fn write_input(&mut self) {
        while !self.input.is_empty() {
            let (front, _) = self.input.as_slices();
            match rustix::io::write(&self.pty, front) {
                Ok(0) => return,
                Ok(n) => {
                    self.input.drain(..n);
                }
                Err(rustix::io::Errno::AGAIN) => return,
                Err(rustix::io::Errno::INTR) => {}
                Err(_) => {
                    // The program is not there to read it.
                    self.input.clear();
                    return;
                }
            }
        }
    }

    fn read_client(&mut self, i: usize) {
        let client = &mut self.clients[i];
        let mut buf = [0u8; 8192];
        match client.stream.read(&mut buf) {
            Ok(0) => client.eof = true,
            Ok(n) => client.decoder.extend(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => client.dead = true,
        }
    }

    fn handle_frames(&mut self, i: usize) {
        loop {
            let client = &mut self.clients[i];
            if client.paused || client.closing || client.dead {
                return;
            }
            let cmd = match client.decoder.next() {
                Ok(None) => {
                    // Nothing more will come; a partial frame is abandoned.
                    client.dead |= client.eof;
                    return;
                }
                Err(e) => Cmd::Bad(e.to_string()),
                Ok(Some(frame)) => match ClientMsg::decode(&frame) {
                    Err(e) => Cmd::Bad(e.to_string()),
                    Ok(msg) if !client.greeted && !matches!(msg, ClientMsg::Hello { .. }) => {
                        Cmd::Bad("expected a greeting".into())
                    }
                    Ok(ClientMsg::Input(data)) => {
                        if !client.readonly {
                            self.input.extend(data);
                            client.paused = self.input.len() >= INPUT_MAX;
                        }
                        continue;
                    }
                    Ok(ClientMsg::Hello { .. }) => Cmd::Hello,
                    Ok(ClientMsg::Attach(attach)) => Cmd::Attach(attach),
                    Ok(ClientMsg::Detach) => Cmd::Detach,
                    Ok(ClientMsg::Resize { rows, cols }) => Cmd::Resize(rows, cols),
                    Ok(ClientMsg::Redraw { method, rows, cols }) => Cmd::Redraw(method, rows, cols),
                    Ok(ClientMsg::Signal(sig)) => Cmd::Signal(sig),
                    Ok(ClientMsg::Clear) => Cmd::Clear,
                },
            };
            self.apply(i, cmd);
        }
    }

    fn apply(&mut self, i: usize, cmd: Cmd) {
        match cmd {
            Cmd::Hello => {
                let client = &mut self.clients[i];
                client.greeted = true;
                client.send(ServerMsg::HelloAck {
                    version: VERSION,
                    session: self.info.id,
                });
            }
            Cmd::Attach(attach) => {
                let total = self.ring.total();
                let start = if self.modes.alt_screen() {
                    // A full-screen program repaints; old output would only
                    // flash by underneath it.
                    total
                } else {
                    match attach.replay {
                        Replay::None => total,
                        // After a newline, which is as safe as a mark.
                        Replay::Lines(n) => self.ring.lines_back(n),
                        Replay::Default => {
                            let want = self
                                .ring
                                .history_start()
                                .max(total.saturating_sub(REPLAY_DEFAULT));
                            self.ring.mark_from(want).unwrap_or(total)
                        }
                    }
                };
                let client = &mut self.clients[i];
                client.readonly = attach.readonly;
                client.pos = start;
                client.phase = Phase::Replay {
                    end: total,
                    filter: Filter::queries(),
                };
                if !attach.readonly {
                    self.first_attach = None;
                    self.do_redraw(attach.redraw, attach.rows, attach.cols);
                }
            }
            Cmd::Detach => self.clients[i].phase = Phase::Idle,
            Cmd::Resize(rows, cols) => {
                if !self.clients[i].readonly {
                    self.resize(rows, cols);
                }
            }
            Cmd::Redraw(method, rows, cols) => {
                if !self.clients[i].readonly {
                    self.do_redraw(method, rows, cols);
                }
            }
            Cmd::Signal(number) => {
                if let Some(sig) = sys::signal_from_number(number) {
                    let _ = sys::signal_foreground(self.pty.as_fd(), sys::pid_of(&self.child), sig);
                }
            }
            Cmd::Clear => {
                self.ring.clear_history();
                if let Some(log) = self.log.as_mut() {
                    log.truncate();
                }
                self.clients[i].send(ServerMsg::Ack);
            }
            Cmd::Bad(why) => {
                let client = &mut self.clients[i];
                client.send(ServerMsg::Error(&why));
                client.closing = true;
            }
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        if rows > 0 && cols > 0 {
            let size = Winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            let _ = sys::set_winsize(&self.pty, size);
        }
    }

    /// Get the program to repaint for a terminal that has just attached.
    fn do_redraw(&mut self, method: Redraw, rows: u16, cols: u16) {
        let method = if method == Redraw::Unspec {
            self.redraw
        } else {
            method
        };
        if method == Redraw::None {
            return;
        }
        self.resize(rows, cols);
        match method {
            Redraw::CtrlL if sys::pty_is_cbreak(&self.pty) => self.input.push_back(0x0c),
            Redraw::Winch => {
                let _ = sys::signal_foreground(
                    self.pty.as_fd(),
                    sys::pid_of(&self.child),
                    rustix::process::Signal::WINCH,
                );
            }
            _ => {}
        }
    }

    /// Say goodbye, record how it ended, and remove the socket.
    fn finish(mut self, killed: bool) -> i32 {
        let exit = self
            .status
            .filter(|_| !killed)
            .map(|status| match status.signal() {
                Some(sig) => Exit::Signal(sig as u8),
                None => Exit::Code(status.code().unwrap_or(1) as u8),
            });
        for client in &mut self.clients {
            if !client.greeted {
                continue;
            }
            if let Some(exit) = exit {
                client.send(ServerMsg::Exit(exit));
            }
            // One last blocking push, briefly; the client may be gone.
            let _ = client.stream.set_nonblocking(false);
            let _ = client
                .stream
                .set_write_timeout(Some(Duration::from_millis(500)));
            let _ = client.stream.write_all(&client.out[client.out_at..]);
        }

        let now = unix_now();
        if let Some(log) = self.log.as_mut() {
            let marker = format!(
                "\r\n[{}: session '{}' ended after {}]\r\n",
                self.cfg.names.progname,
                self.session.name(),
                format_age(now.saturating_sub(self.info.started))
            );
            log.append(self.session, marker.as_bytes());
        }
        self.info.ended = Some(now);
        match exit {
            Some(Exit::Code(code)) => self.info.exit = Some(code),
            Some(Exit::Signal(sig)) => self.info.signal = Some(sig),
            None => {}
        }
        let _ = self.info.write(self.session);
        if let Ok(_lock) = self.session.lock() {
            self.session.remove_socket(self.sock_id);
        }
        match exit {
            Some(Exit::Code(code)) => i32::from(code),
            _ => 1,
        }
    }
}
