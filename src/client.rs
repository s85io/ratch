//! Layer 3: the client side of a session.
//!
//! A [`Link`] is a connection to a master. On top of it sit the interactive
//! attach loop and the one-shot requests: push, kill, clear, tail.

#![forbid(unsafe_code)]

use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant, SystemTime};

use crate::proto::{Attach, ClientMsg, Decoder, Exit, Redraw, Replay, ServerMsg, VERSION};
use crate::session::{Names, Probe, Session};
use crate::sys::{self, signal, PollFd, PollFlags, Signals};
use crate::vt::{Modes, Parser};

/// Why a session could not be reached.
pub enum Unreachable {
    Missing,
    /// The socket is there; the master is not.
    Stale,
    NotASocket,
    Failed(io::Error),
}

pub enum Event<'a> {
    Output(&'a [u8]),
    ReplayEnd,
    Skipped,
    Exit(Exit),
    Ack,
    Error(&'a str),
}

/// A connection to a master, either one of ours or the C implementation
/// this replaces (`v1`), which a session started before an upgrade still runs.
pub struct Link {
    stream: UnixStream,
    decoder: Decoder,
    v1: bool,
    /// The master's session id; a v1 master has none.
    pub session_id: Option<u64>,
}

const GREETING_TIMEOUT: Duration = Duration::from_secs(5);

impl Link {
    pub fn open(session: &Session) -> Result<Link, Unreachable> {
        let connect = || match session.probe() {
            Probe::Live(stream) => Ok(stream),
            Probe::Missing => Err(Unreachable::Missing),
            Probe::Stale => Err(Unreachable::Stale),
            Probe::NotASocket => Err(Unreachable::NotASocket),
            Probe::Failed(e) => Err(Unreachable::Failed(e)),
        };
        let stream = connect()?;
        let mut link = Link {
            stream,
            decoder: Decoder::new(),
            v1: false,
            session_id: None,
        };
        match link.greet() {
            Ok(true) => Ok(link),
            // It hung up on our greeting, as the C master does on anything
            // that is not one of its ten-byte packets. Start over, its way.
            Ok(false) => Ok(Link {
                stream: connect()?,
                decoder: Decoder::new(),
                v1: true,
                session_id: None,
            }),
            Err(e) => Err(Unreachable::Failed(e)),
        }
    }

    /// Returns false if the other side closed without answering.
    fn greet(&mut self) -> io::Result<bool> {
        self.send(ClientMsg::Hello { version: VERSION })?;
        self.stream.set_read_timeout(Some(GREETING_TIMEOUT))?;
        let mut buf = [0u8; 64];
        loop {
            if let Some(frame) = self.decoder.next().map_err(io::Error::other)? {
                return match ServerMsg::decode(&frame) {
                    Ok(ServerMsg::HelloAck { session, .. }) => {
                        self.session_id = Some(session);
                        let _ = self.stream.set_read_timeout(None);
                        Ok(true)
                    }
                    Ok(ServerMsg::Error(text)) => Err(io::Error::other(text.to_owned())),
                    _ => Err(io::Error::other("unexpected reply to greeting")),
                };
            }
            match self.stream.read(&mut buf) {
                Ok(0) => return Ok(false),
                Ok(n) => self.decoder.extend(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => return Ok(false),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return Err(io::Error::other("session is not responding"))
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub fn is_v1(&self) -> bool {
        self.v1
    }

    pub fn send(&mut self, msg: ClientMsg<'_>) -> io::Result<()> {
        let mut bytes = Vec::new();
        if self.v1 {
            v1::encode(&msg, &mut bytes);
        } else {
            msg.encode(&mut bytes);
        }
        self.stream.write_all(&bytes)
    }

    /// Read once from the master and hand over what arrived.
    /// Returns false when the master has closed the connection.
    pub fn read(&mut self, mut on: impl FnMut(Event<'_>)) -> io::Result<bool> {
        let mut buf = [0u8; 8192];
        let n = match self.stream.read(&mut buf) {
            Ok(0) => return Ok(false),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(true),
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => return Ok(false),
            Err(e) => return Err(e),
        };
        if self.v1 {
            on(Event::Output(&buf[..n]));
            return Ok(true);
        }
        self.decoder.extend(&buf[..n]);
        while let Some(frame) = self.decoder.next().map_err(io::Error::other)? {
            on(match ServerMsg::decode(&frame).map_err(io::Error::other)? {
                ServerMsg::Output(data) => Event::Output(data),
                ServerMsg::ReplayEnd => Event::ReplayEnd,
                ServerMsg::Skipped => Event::Skipped,
                ServerMsg::Exit(exit) => Event::Exit(exit),
                ServerMsg::Ack => Event::Ack,
                ServerMsg::Error(text) => Event::Error(text),
                ServerMsg::HelloAck { .. } => continue,
            });
        }
        Ok(true)
    }

    /// Wait for the master to close the connection, which it does when the
    /// session ends. Returns false if `limit` passes first.
    pub fn wait_closed(&mut self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            // macOS refuses to set a timeout on a socket whose peer is gone;
            // the read below then returns at once, which is the answer.
            let _ = self.stream.set_read_timeout(Some(left));
            match self.read(|_| {}) {
                Ok(false) => return true,
                Ok(true) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return false,
                Err(_) => return true,
            }
        }
    }
}

impl AsFd for Link {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.stream.as_fd()
    }
}

/// The packets of the C master: `struct packet { u8 type; u8 len; union {
/// u8 buf[8]; struct winsize ws; } }`, native byte order, ten bytes.
mod v1 {
    use super::ClientMsg;

    const PUSH: u8 = 0;
    const ATTACH: u8 = 1;
    const DETACH: u8 = 2;
    const WINCH: u8 = 3;
    const REDRAW: u8 = 4;
    const KILL: u8 = 5;

    fn packet(out: &mut Vec<u8>, kind: u8, len: u8, body: [u8; 8]) {
        out.extend_from_slice(&[kind, len]);
        out.extend_from_slice(&body);
    }

    fn winsize(rows: u16, cols: u16) -> [u8; 8] {
        let (r, c) = (rows.to_ne_bytes(), cols.to_ne_bytes());
        [r[0], r[1], c[0], c[1], 0, 0, 0, 0]
    }

    pub fn encode(msg: &ClientMsg<'_>, out: &mut Vec<u8>) {
        match *msg {
            ClientMsg::Input(data) => {
                for chunk in data.chunks(8) {
                    let mut body = [0u8; 8];
                    body[..chunk.len()].copy_from_slice(chunk);
                    packet(out, PUSH, chunk.len() as u8, body);
                }
            }
            ClientMsg::Attach(attach) => {
                // Length 0 asks for the master's own in-memory history.
                packet(out, ATTACH, 0, [0; 8]);
                packet(
                    out,
                    REDRAW,
                    attach.redraw as u8,
                    winsize(attach.rows, attach.cols),
                );
            }
            ClientMsg::Detach => packet(out, DETACH, 0, [0; 8]),
            ClientMsg::Resize { rows, cols } => packet(out, WINCH, 0, winsize(rows, cols)),
            ClientMsg::Redraw { method, rows, cols } => {
                packet(out, REDRAW, method as u8, winsize(rows, cols));
            }
            ClientMsg::Signal(sig) => packet(out, KILL, sig, [0; 8]),
            // Not something a v1 master understands.
            ClientMsg::Hello { .. } | ClientMsg::Clear => {}
        }
    }
}

/// What `-R` chose to do with the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Clear {
    #[default]
    Unspec,
    None,
    Move,
}

#[derive(Clone, Debug)]
pub struct Options {
    /// The key that detaches; `None` if disabled.
    pub detach: Option<u8>,
    pub no_suspend: bool,
    pub redraw: Redraw,
    pub clear: Clear,
    pub quiet: bool,
    /// Assume nothing about the terminal: send it no escape sequences.
    pub plain: bool,
    pub replay: Replay,
    /// Watch without typing: keys are not passed to the program.
    pub readonly: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            detach: Some(0x1c),
            no_suspend: false,
            redraw: Redraw::Unspec,
            clear: Clear::Unspec,
            quiet: false,
            plain: false,
            replay: Replay::Default,
            readonly: false,
        }
    }
}

/// Print a line to stdout, ignoring a closed pipe.
macro_rules! say {
    ($($arg:tt)*) => {{
        let _ = writeln!(io::stdout(), $($arg)*);
    }};
}

fn print_unreachable(names: &Names, session: &Session, why: &Unreachable) {
    let (prog, name) = (&names.progname, session.name());
    match why {
        Unreachable::Missing => say!("{prog}: session '{name}' does not exist"),
        Unreachable::Stale => say!("{prog}: session '{name}' is not running"),
        Unreachable::NotASocket => say!("{prog}: '{name}' is not a valid session"),
        Unreachable::Failed(e) => say!("{prog}: {}: {}", session.sock.display(), sys::describe(e)),
    }
}

/// Open a link, or say why not.
pub fn connect(names: &Names, session: &Session) -> Option<Link> {
    Link::open(session)
        .map_err(|why| print_unreachable(names, session, &why))
        .ok()
}

/// The user's terminal while we have it in raw mode.
///
/// It keeps its own record of the modes the program switched on, from the
/// bytes actually written, so that leaving undoes exactly those.
struct Terminal {
    original: sys::Termios,
    raw: sys::Termios,
    parser: Parser,
    modes: Modes,
    plain: bool,
    entered: bool,
}

impl Terminal {
    fn new(plain: bool) -> Option<Terminal> {
        let original = sys::termios_of(sys::stdin_fd())?;
        sys::remember_terminal(&original);
        let raw = sys::raw_termios(&original);
        Some(Terminal {
            original,
            raw,
            parser: Parser::new(),
            modes: Modes::new(),
            plain,
            entered: false,
        })
    }

    fn enter(&mut self) {
        let _ = sys::set_termios(sys::stdin_fd(), &self.raw);
        self.entered = true;
    }

    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        sys::write_stdout(data)?;
        let Terminal { parser, modes, .. } = self;
        parser.feed(data, |token| modes.apply(&token));
        Ok(())
    }

    /// Undo what the program did to the terminal's modes. The terminal stays
    /// raw, so that a parting message can still be placed.
    fn reset_modes(&mut self) {
        if self.plain {
            return;
        }
        // The first two end any sequence the program left unfinished.
        let mut bytes = b"\x18\x1b\\".to_vec();
        self.modes.reset(&mut bytes);
        bytes.extend_from_slice(b"\x1b[0m\x1b[?25h");
        let _ = sys::write_stdout(&bytes);
        self.modes = Modes::new();
        self.parser = Parser::new();
    }

    fn leave(&mut self) {
        if self.entered {
            self.entered = false;
            self.reset_modes();
            let _ = sys::set_termios(sys::stdin_fd(), &self.original);
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.leave();
    }
}

fn window() -> (u16, u16) {
    sys::winsize_of(sys::stdin_fd()).map_or((0, 0), |ws| (ws.ws_row, ws.ws_col))
}

fn age_since(start: SystemTime) -> String {
    crate::master::format_age(start.elapsed().map_or(0, |d| d.as_secs()))
}

/// Is this session one the calling process is running inside of?
fn in_own_ancestry(names: &Names, session: &Session, link: &Link) -> bool {
    let var = names.session_var();
    let chain_has = |var: String, wanted: &str| {
        std::env::var(var).is_ok_and(|chain| chain.split(':').any(|item| item == wanted))
    };
    match link.session_id {
        Some(id) => chain_has(format!("{var}_ID"), &format!("{id:016x}")),
        None => chain_has(var, &session.sock.to_string_lossy()),
    }
}

/// Attach the terminal to a session until the user detaches or it ends.
pub fn attach(names: &Names, session: &Session, mut link: Link, opts: &Options) -> i32 {
    let (prog, name) = (names.progname.as_str(), session.name());
    if in_own_ancestry(names, session, &link) {
        say!("{prog}: cannot attach to session '{name}' from within itself");
        return 1;
    }
    let Some(mut term) = Terminal::new(opts.plain) else {
        say!("{prog}: attaching to a session requires a terminal.");
        return 1;
    };
    let Ok(signals) = Signals::new(&[
        signal::SIGWINCH,
        signal::SIGHUP,
        signal::SIGINT,
        signal::SIGTERM,
        signal::SIGQUIT,
    ]) else {
        return 1;
    };
    let started = session.started().unwrap_or_else(SystemTime::now);
    // Where a parting message goes: the bottom of the screen, if we may move.
    let place = if opts.plain || opts.clear == Clear::None {
        "\r\n"
    } else {
        "\x1b[999H\r\n"
    };
    let suspend_key = sys::suspend_key(&term.original);

    term.enter();
    if opts.clear == Clear::Move && !opts.plain {
        let _ = term.write(b"\x1b[H\x1b[2J\x1b[3J");
    }
    let attach_msg = |replay| {
        let (rows, cols) = window();
        ClientMsg::Attach(Attach {
            replay,
            rows,
            cols,
            readonly: opts.readonly,
            redraw: opts.redraw,
        })
    };
    let mut ok = link.send(attach_msg(opts.replay)).is_ok();
    // While history is being played, the terminal may be answering queries
    // in it. Those answers arrive on stdin like typing and are picked out.
    // A v1 master never says when history ends, so nothing is filtered.
    let mut replies = (!link.is_v1()).then(crate::vt::Filter::replies);

    let farewell = |term: &mut Terminal, text: String| {
        term.reset_modes();
        let _ =
            sys::write_stdout(format!("{place}[{prog}: session '{name}' {text}]\r\n").as_bytes());
    };

    while ok {
        let mut fds = [
            PollFd::from_borrowed_fd(signals.fd(), PollFlags::IN),
            PollFd::from_borrowed_fd(sys::stdin_fd(), PollFlags::IN),
            PollFd::new(&link, PollFlags::IN),
        ];
        if sys::poll(&mut fds, None).is_err() {
            farewell(
                &mut term,
                format!("select failed after {}", age_since(started)),
            );
            return 1;
        }
        let ready = fds.map(|fd| !fd.revents().is_empty());

        if ready[0] {
            signals.drain();
            for (sig, detached) in [
                (signal::SIGHUP, true),
                (signal::SIGINT, true),
                (signal::SIGTERM, false),
                (signal::SIGQUIT, false),
            ] {
                if signals.take(sig) {
                    let age = age_since(started);
                    farewell(
                        &mut term,
                        match detached {
                            true => format!("detached after {age}"),
                            false => format!("got signal {sig} - exiting after {age}"),
                        },
                    );
                    return 1;
                }
            }
            if signals.take(signal::SIGWINCH) {
                let (rows, cols) = window();
                ok &= link.send(ClientMsg::Resize { rows, cols }).is_ok();
            }
        }

        if ready[2] {
            let mut output = Vec::new();
            let mut failed = None;
            let mut history_done = false;
            let open = link.read(|event| match event {
                Event::Output(data) => output.extend_from_slice(data),
                Event::ReplayEnd => history_done = true,
                Event::Error(text) => failed = Some(text.to_owned()),
                Event::Skipped | Event::Exit(_) | Event::Ack => {}
            });
            if term.write(&output).is_err() {
                farewell(
                    &mut term,
                    format!("write failed after {}", age_since(started)),
                );
                return 1;
            }
            if history_done {
                // Anything the filter was still holding is typing after all.
                if let Some(mut filter) = replies.take() {
                    let mut held = Vec::new();
                    filter.finish(&mut held);
                    ok &= link.send(ClientMsg::Input(&held)).is_ok();
                }
            }
            match (open, failed) {
                (_, Some(text)) => {
                    farewell(&mut term, format!("error: {text}"));
                    return 1;
                }
                (Ok(true), None) => {}
                (Ok(false), None) => {
                    if !opts.quiet {
                        farewell(&mut term, format!("exited after {}", age_since(started)));
                    }
                    return 0;
                }
                (Err(_), None) => {
                    farewell(
                        &mut term,
                        format!("read error after {}", age_since(started)),
                    );
                    return 1;
                }
            }
        }

        if ready[1] {
            let mut buf = [0u8; 4096];
            let n = match sys::read_stdin(&mut buf) {
                Ok(n) if n > 0 => n,
                _ => return 1,
            };
            let mut typed = Vec::new();
            let keys = match replies.as_mut() {
                Some(filter) => {
                    filter.feed(&buf[..n], &mut typed);
                    typed.as_slice()
                }
                None => &buf[..n],
            };
            if keys.is_empty() {
                continue;
            }
            // A key means something to us only when it arrives on its own,
            // as typed keys do; inside pasted text it is just text.
            if !opts.no_suspend && suspend_key != 0 && keys[0] == suspend_key {
                ok &= link.send(ClientMsg::Detach).is_ok();
                term.leave();
                let _ = sys::write_stdout(place.as_bytes());
                sys::suspend_self();
                // Back in the foreground.
                term.enter();
                ok &= link.send(attach_msg(opts.replay)).is_ok();
                replies = (!link.is_v1()).then(crate::vt::Filter::replies);
                continue;
            }
            if opts.detach == Some(keys[0]) {
                let _ = link.send(ClientMsg::Detach);
                farewell(&mut term, format!("detached after {}", age_since(started)));
                return 0;
            }
            if opts.readonly {
                continue;
            }
            ok &= link.send(ClientMsg::Input(keys)).is_ok();
            if keys[0] == 0x0c {
                // The user asked for a repaint; make sure the size is right.
                let (rows, cols) = window();
                ok &= link.send(ClientMsg::Resize { rows, cols }).is_ok();
            }
        }
    }
    farewell(
        &mut term,
        format!("write failed after {}", age_since(started)),
    );
    1
}

/// Copy stdin into the session as if it had been typed.
pub fn push(names: &Names, session: &Session) -> i32 {
    let fail = |e: &io::Error| {
        say!(
            "{}: {}: {}",
            names.progname,
            session.sock.display(),
            sys::describe(e)
        );
        1
    };
    let mut link = match Link::open(session) {
        Ok(link) => link,
        Err(Unreachable::Failed(e)) => return fail(&e),
        Err(Unreachable::Missing) => return fail(&io::ErrorKind::NotFound.into()),
        Err(_) => return fail(&io::ErrorKind::ConnectionRefused.into()),
    };
    let mut buf = [0u8; 4096];
    loop {
        match sys::read_stdin(&mut buf) {
            Ok(0) => return 0,
            Ok(n) => {
                if let Err(e) = link.send(ClientMsg::Input(&buf[..n])) {
                    return fail(&e);
                }
            }
            Err(e) => return fail(&e),
        }
    }
}

/// Stop a session: ask nicely, wait, then insist. With `force`, insist first.
pub fn kill(names: &Names, session: &Session, force: bool, quiet: bool) -> i32 {
    let (prog, name) = (&names.progname, session.name());
    let Some(mut link) = connect(names, session) else {
        return 1;
    };
    if !force && in_own_ancestry(names, session, &link) {
        say!("{prog}: cannot kill session '{name}' from within itself (use -f to do it anyway)");
        return 1;
    }
    let done = |how: &str| {
        if !quiet {
            say!("{prog}: session '{name}' {how}");
        }
        0
    };
    if !force {
        let _ = link.send(ClientMsg::Signal(sys::SIGNAL_TERM));
        if link.wait_closed(Duration::from_secs(5)) {
            return done("stopped");
        }
    }
    let _ = link.send(ClientMsg::Signal(sys::SIGNAL_KILL));
    if link.wait_closed(Duration::from_secs(2)) {
        return done("killed");
    }
    say!("{prog}: session '{name}' did not stop");
    1
}

/// Exit codes of `wait` that are not the program's own.
pub const WAIT_TIMED_OUT: i32 = 124;
pub const WAIT_UNKNOWN: i32 = 255;

fn exit_code(exit: Exit) -> i32 {
    match exit {
        Exit::Code(code) => i32::from(code),
        // As a shell reports a command killed by a signal.
        Exit::Signal(sig) => 128 + i32::from(sig),
    }
}

/// Block until a session ends and return the program's exit code. A session
/// that has already ended answers from what its master recorded.
pub fn wait(names: &Names, session: &Session, limit: Option<Duration>) -> i32 {
    let (prog, name) = (&names.progname, session.name());
    let mut link = match Link::open(session) {
        Ok(link) => link,
        Err(Unreachable::Missing) => {
            let recorded = crate::session::Info::read(session).filter(|info| info.ended.is_some());
            return match recorded {
                Some(info) => match (info.exit, info.signal) {
                    (Some(code), _) => i32::from(code),
                    (None, Some(sig)) => 128 + i32::from(sig),
                    (None, None) => WAIT_UNKNOWN,
                },
                None => {
                    say!("{prog}: session '{name}' does not exist");
                    1
                }
            };
        }
        Err(why) => {
            print_unreachable(names, session, &why);
            return 1;
        }
    };
    let deadline = limit.map(|limit| Instant::now() + limit);
    let mut exit = None;
    loop {
        if let Some(deadline) = deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                say!("{prog}: session '{name}' is still running");
                return WAIT_TIMED_OUT;
            }
            let _ = link.stream.set_read_timeout(Some(left));
        }
        match link.read(|event| {
            if let Event::Exit(how) = event {
                exit = Some(how);
            }
        }) {
            Ok(true) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            // The master is gone. If it did not say how the program ended,
            // it did not end in an orderly way and nobody knows.
            Ok(false) | Err(_) => return exit.map_or(WAIT_UNKNOWN, exit_code),
        }
    }
}

/// Forget a session's history. A running session is asked to do it itself,
/// or what it holds in memory would bring the history back.
pub fn clear(names: &Names, session: &Session, quiet: bool) -> i32 {
    let (prog, name) = (&names.progname, session.name());
    let mut cleared = false;
    if let Ok(mut link) = Link::open(session) {
        if !link.is_v1() && link.send(ClientMsg::Clear).is_ok() {
            let _ = link.read(|event| cleared |= matches!(event, Event::Ack));
        }
    }
    if !cleared {
        use rustix::fs::OFlags;
        match session.open_file(".log", OFlags::WRONLY | OFlags::TRUNC) {
            Ok(_) => cleared = true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return 0,
            Err(e) => {
                say!(
                    "{prog}: {}.log: {}",
                    session.sock.display(),
                    sys::describe(&e)
                );
                return 1;
            }
        }
    }
    if cleared && !quiet {
        say!("{prog}: session '{name}' log cleared");
    }
    0
}

/// Print the last `lines` lines of a session's output, and with `follow`,
/// what comes after. A running session is asked; a finished one is read
/// from its log.
pub fn tail(names: &Names, session: &Session, lines: u32, follow: bool) -> i32 {
    let (prog, name) = (&names.progname, session.name());
    if let Ok(mut link) = Link::open(session) {
        if !link.is_v1() {
            let watch = Attach {
                replay: Replay::Lines(lines),
                readonly: true,
                ..Attach::default()
            };
            if link.send(ClientMsg::Attach(watch)).is_err() {
                return 1;
            }
            let mut live = false;
            loop {
                let mut output = Vec::new();
                let open = link.read(|event| match event {
                    Event::Output(data) if follow || !live => output.extend_from_slice(data),
                    Event::ReplayEnd => live = true,
                    _ => {}
                });
                if sys::write_stdout(&output).is_err() {
                    return 0;
                }
                match open {
                    Ok(true) if follow || !live => {}
                    _ => return 0,
                }
            }
        }
    }

    use rustix::fs::OFlags;
    use std::io::{Seek, SeekFrom};
    let mut file = match session.open_file(".log", OFlags::RDONLY) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            say!("{prog}: no log for session '{name}'");
            return 1;
        }
        Err(e) => {
            say!(
                "{prog}: {}.log: {}",
                session.sock.display(),
                sys::describe(&e)
            );
            return 1;
        }
    };
    let mut log = Vec::new();
    if file.read_to_end(&mut log).is_err() {
        return 1;
    }
    let mut filter = sys::stdout_is_terminal().then(crate::vt::Filter::queries);
    let mut show = |bytes: &[u8]| match filter.as_mut() {
        Some(filter) => {
            let mut kept = Vec::new();
            filter.feed(bytes, &mut kept);
            sys::write_stdout(&kept)
        }
        None => sys::write_stdout(bytes),
    };
    if show(&log[tail_start(&log, lines)..]).is_err() || !follow {
        return 0;
    }
    // A log is only still growing if a v1 master is writing it.
    let mut at = log.len() as u64;
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let len = file.metadata().map_or(at, |m| m.len());
        if len < at {
            at = 0; // rotated
        }
        let mut more = Vec::new();
        if file.seek(SeekFrom::Start(at)).is_err() || file.read_to_end(&mut more).is_err() {
            return 0;
        }
        at += more.len() as u64;
        if show(&more).is_err() {
            return 0;
        }
        if more.is_empty() && !matches!(session.probe(), Probe::Live(_)) {
            return 0;
        }
    }
}

/// Offset in `log` where its last `lines` lines begin.
fn tail_start(log: &[u8], lines: u32) -> usize {
    let mut seen = 0;
    let end = log.len() - usize::from(log.ends_with(b"\n"));
    for i in (0..end).rev() {
        if log[i] == b'\n' {
            seen += 1;
            if seen == lines {
                return i + 1;
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_start_counts_lines_from_the_end() {
        let log = b"one\ntwo\nthree\n";
        assert_eq!(tail_start(log, 1), 8);
        assert_eq!(tail_start(log, 2), 4);
        assert_eq!(tail_start(log, 3), 0);
        assert_eq!(tail_start(log, 9), 0);
        assert_eq!(tail_start(b"one\ntwo", 1), 4);
        assert_eq!(tail_start(b"", 5), 0);
    }

    #[test]
    fn v1_packets_are_ten_bytes_and_carry_at_most_eight() {
        let mut out = Vec::new();
        v1::encode(&ClientMsg::Input(b"0123456789"), &mut out);
        assert_eq!(
            out,
            [
                0, 8, b'0', b'1', b'2', b'3', b'4', b'5', b'6', b'7', 0, 2, b'8', b'9', 0, 0, 0, 0,
                0, 0
            ]
        );
        out.clear();
        v1::encode(&ClientMsg::Signal(9), &mut out);
        assert_eq!(out, [5, 9, 0, 0, 0, 0, 0, 0, 0, 0]);
        out.clear();
        v1::encode(&ClientMsg::Resize { rows: 24, cols: 80 }, &mut out);
        assert_eq!(out.len(), 10);
        assert_eq!(u16::from_ne_bytes([out[2], out[3]]), 24);
        assert_eq!(u16::from_ne_bytes([out[4], out[5]]), 80);
    }
}
