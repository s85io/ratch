//! Layer 3: the client side of a session.
//!
//! A [`Link`] is a connection to a master. On top of it sit the interactive
//! attach loop and the one-shot requests: push, kill, clear, tail.

#![forbid(unsafe_code)]

use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant, SystemTime};

use crate::proto::{
    Attach, ClientMsg, Decoder, Exit, Redraw, Replay, ServerMsg, VERSION, VERSION_SETTLED,
};
use crate::session::{Names, Probe, Session};
use crate::sys::{self, signal, PollFd, PollFlags, Signals};
use crate::vt::{input, Filter, Modes, Parser};

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
    Settled,
    Exit(Exit),
    Ack,
    Error(&'a str),
}

/// A connection to a master.
pub struct Link {
    stream: UnixStream,
    decoder: Decoder,
    /// Which session this is, whatever it is called now.
    pub session_id: u64,
    /// The protocol version the master speaks.
    pub version: u16,
}

const GREETING_TIMEOUT: Duration = Duration::from_secs(5);

impl Link {
    pub fn open(session: &Session) -> Result<Link, Unreachable> {
        let stream = match session.probe() {
            Probe::Live(stream) => stream,
            Probe::Missing => return Err(Unreachable::Missing),
            Probe::Stale => return Err(Unreachable::Stale),
            Probe::NotASocket => return Err(Unreachable::NotASocket),
            Probe::Failed(e) => return Err(Unreachable::Failed(e)),
        };
        let mut link = Link {
            stream,
            decoder: Decoder::new(),
            session_id: 0,
            version: 0,
        };
        match link.greet() {
            Ok(true) => Ok(link),
            Ok(false) => Err(Unreachable::Failed(io::Error::other(
                "session did not answer",
            ))),
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
                    Ok(ServerMsg::HelloAck { version, session }) => {
                        self.session_id = session;
                        self.version = version;
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

    pub fn send(&mut self, msg: ClientMsg<'_>) -> io::Result<()> {
        let mut bytes = Vec::new();
        msg.encode(&mut bytes);
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
        self.decoder.extend(&buf[..n]);
        while let Some(frame) = self.decoder.next().map_err(io::Error::other)? {
            on(match ServerMsg::decode(&frame).map_err(io::Error::other)? {
                ServerMsg::Output(data) => Event::Output(data),
                ServerMsg::ReplayEnd => Event::ReplayEnd,
                ServerMsg::Skipped => Event::Skipped,
                ServerMsg::Settled => Event::Settled,
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

#[derive(Clone, Debug)]
pub struct Options {
    /// The key that detaches; `None` if disabled.
    pub detach: Option<u8>,
    pub no_suspend: bool,
    pub redraw: Redraw,
    /// Wipe the screen and its scrollback before showing the session.
    pub clear: bool,
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
            clear: false,
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
        // The first two end any sequence the program left unfinished, the
        // third a synchronized update, which would keep the screen frozen.
        let mut bytes = b"\x18\x1b\\\x1b[?2026l".to_vec();
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

/// The terminal's size: rows, columns, and pixels if the master can take them.
fn window(link: &Link) -> (u16, u16, u16, u16) {
    let Some(ws) = sys::winsize_of(sys::stdin_fd()) else {
        return (0, 0, 0, 0);
    };
    if link.version < 3 {
        return (ws.ws_row, ws.ws_col, 0, 0);
    }
    (ws.ws_row, ws.ws_col, ws.ws_xpixel, ws.ws_ypixel)
}

fn resize_msg(link: &Link) -> ClientMsg<'static> {
    let (rows, cols, xpixel, ypixel) = window(link);
    ClientMsg::Resize {
        rows,
        cols,
        xpixel,
        ypixel,
    }
}

fn age_since(start: SystemTime) -> String {
    crate::master::format_age(start.elapsed().map_or(0, |d| d.as_secs()))
}

/// Is this session one the calling process is running inside of?
fn in_own_ancestry(names: &Names, link: &Link) -> bool {
    let wanted = format!("{:016x}", link.session_id);
    std::env::var(names.id_var()).is_ok_and(|chain| chain.split(':').any(|id| id == wanted))
}

/// Keeps the picture on the terminal still while an attach is under way:
/// history and the program's repaint go in unseen and appear at once,
/// instead of scrolling and flickering past. Terminals that do not know
/// synchronized output (mode 2026) ignore it and show it all as it comes.
struct Hold {
    /// Takes out the program's own synchronized updates, whose end would
    /// let the picture go early.
    filter: Filter,
    until: Instant,
}

/// Longer than this and a terminal shows what it has anyway.
const HOLD_MAX: Duration = Duration::from_millis(1500);

impl Hold {
    fn start(term: &mut Terminal, status: Option<&str>) -> Hold {
        if let Some(text) = status {
            // Shown while the picture is held, gone once it moves on.
            let _ = sys::write_stdout(format!("\r\x1b[K\x1b[2m{text}\x1b[m").as_bytes());
            // Give the terminal a frame to draw it before it is held.
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = term.write(b"\x1b[?2026h");
        if status.is_some() {
            let _ = term.write(b"\r\x1b[K");
        }
        Hold {
            filter: Filter::sync(),
            until: Instant::now() + HOLD_MAX,
        }
    }

    fn pass(&mut self, output: &[u8]) -> Vec<u8> {
        let mut kept = Vec::with_capacity(output.len());
        self.filter.feed(output, &mut kept);
        kept
    }

    fn release(mut self, term: &mut Terminal) {
        let mut rest = Vec::new();
        self.filter.finish(&mut rest);
        rest.extend_from_slice(b"\x1b[?2026l");
        let _ = term.write(&rest);
    }
}

/// Attach the terminal to a session until the user detaches or it ends.
pub fn attach(names: &Names, session: &Session, mut link: Link, opts: &Options) -> i32 {
    let (prog, name) = (names.progname.as_str(), session.name());
    if in_own_ancestry(names, &link) {
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
    let place = if opts.plain { "\r\n" } else { "\x1b[999H\r\n" };
    let suspend_key = sys::suspend_key(&term.original);

    term.enter();
    if opts.clear && !opts.plain {
        let _ = term.write(b"\x1b[H\x1b[2J\x1b[3J");
    }
    let attach_msg = |link: &Link, replay| {
        let (rows, cols, xpixel, ypixel) = window(link);
        ClientMsg::Attach(Attach {
            replay,
            rows,
            cols,
            xpixel,
            ypixel,
            readonly: opts.readonly,
            dump: false,
            redraw: opts.redraw,
        })
    };
    // A session just created has nothing to restore.
    let restoring = started
        .elapsed()
        .map_or(true, |age| age > Duration::from_secs(2));
    let status = format!("[{prog}: restoring session '{name}']");
    let status = (restoring && !opts.quiet).then_some(status.as_str());
    let holds = !opts.plain && link.version >= VERSION_SETTLED;
    let mut hold = holds.then(|| Hold::start(&mut term, status));
    let mut ok = link.send(attach_msg(&link, opts.replay)).is_ok();
    // While history is being played, the terminal may be answering queries
    // in it. Those answers arrive on stdin like typing and are picked out.
    let mut replies = Some(crate::vt::Filter::replies());
    if !opts.readonly {
        // For `env`: what this terminal's surroundings look like.
        let _ = crate::session::Env::capture(names).write(session);
    }

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
        let timeout = hold.as_ref().map(|h| {
            h.until
                .saturating_duration_since(Instant::now())
                .as_millis() as u32
                + 1
        });
        if sys::poll(&mut fds, timeout).is_err() {
            farewell(
                &mut term,
                format!("select failed after {}", age_since(started)),
            );
            return 1;
        }
        let ready = fds.map(|fd| !fd.revents().is_empty());
        if hold.as_ref().is_some_and(|h| Instant::now() >= h.until) {
            if let Some(h) = hold.take() {
                h.release(&mut term);
            }
        }

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
                ok &= link.send(resize_msg(&link)).is_ok();
            }
        }

        if ready[2] {
            let mut output = Vec::new();
            let mut failed = None;
            let mut history_done = false;
            let mut settled = false;
            let open = link.read(|event| match event {
                Event::Output(data) => output.extend_from_slice(data),
                Event::ReplayEnd => history_done = true,
                Event::Error(text) => failed = Some(text.to_owned()),
                Event::Settled => settled = true,
                Event::Skipped | Event::Exit(_) | Event::Ack => {}
            });
            if let Some(h) = hold.as_mut() {
                output = h.pass(&output);
            }
            if term.write(&output).is_err() {
                farewell(
                    &mut term,
                    format!("write failed after {}", age_since(started)),
                );
                return 1;
            }
            if settled {
                if let Some(h) = hold.take() {
                    h.release(&mut term);
                }
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
            if !opts.no_suspend && suspend_key != 0 && input::starts_with_key(keys, suspend_key) {
                ok &= link.send(ClientMsg::Detach).is_ok();
                term.leave();
                let _ = sys::write_stdout(place.as_bytes());
                sys::suspend_self();
                // Back in the foreground.
                term.enter();
                hold = holds.then(|| Hold::start(&mut term, status));
                ok &= link.send(attach_msg(&link, opts.replay)).is_ok();
                replies = Some(crate::vt::Filter::replies());
                continue;
            }
            if opts
                .detach
                .is_some_and(|key| input::starts_with_key(keys, key))
            {
                let _ = link.send(ClientMsg::Detach);
                farewell(&mut term, format!("detached after {}", age_since(started)));
                return 0;
            }
            if opts.readonly {
                continue;
            }
            ok &= link.send(ClientMsg::Input(keys)).is_ok();
            if input::starts_with_key(keys, 0x0c) {
                // The user asked for a repaint; make sure the size is right.
                ok &= link.send(resize_msg(&link)).is_ok();
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
    if !force && in_own_ancestry(names, &link) {
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

/// Give a running session a new name.
pub fn rename(names: &Names, session: &Session, new_name: &std::ffi::OsStr, quiet: bool) -> i32 {
    use std::os::unix::ffi::OsStrExt;
    let (prog, name) = (&names.progname, session.name());
    let Some(mut link) = connect(names, session) else {
        return 1;
    };
    let mut answer = None;
    if link.send(ClientMsg::Rename(new_name.as_bytes())).is_ok() {
        while answer.is_none() {
            let open = link.read(|event| match event {
                Event::Ack => answer = Some(Ok(())),
                Event::Error(text) => answer = Some(Err(text.to_owned())),
                _ => {}
            });
            if !matches!(open, Ok(true)) {
                break;
            }
        }
    }
    match answer {
        Some(Ok(())) => {
            if !quiet {
                say!(
                    "{prog}: session '{name}' renamed to '{}'",
                    new_name.to_string_lossy()
                );
            }
            0
        }
        Some(Err(why)) => {
            say!("{prog}: cannot rename session '{name}': {why}");
            1
        }
        None => {
            say!("{prog}: session '{name}' did not answer");
            1
        }
    }
}

/// Forget a session's history. A running session is asked to do it itself,
/// or what it holds in memory would bring the history back.
pub fn clear(names: &Names, session: &Session, quiet: bool) -> i32 {
    let (prog, name) = (&names.progname, session.name());
    let mut cleared = false;
    if let Ok(mut link) = Link::open(session) {
        if link.send(ClientMsg::Clear).is_ok() {
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

/// Where `log` and `tail` write. Recorded output is full of instructions to
/// a terminal; when stdout is one, the questions are taken out and whatever
/// modes the output switches on are switched off again at the end.
struct Printer {
    terminal: Option<(crate::vt::Filter, Parser, Modes)>,
}

impl Printer {
    fn new() -> Printer {
        let terminal = sys::stdout_is_terminal()
            .then(|| (crate::vt::Filter::queries(), Parser::new(), Modes::new()));
        Printer { terminal }
    }

    fn print(&mut self, bytes: &[u8]) -> io::Result<()> {
        let Some((filter, parser, modes)) = self.terminal.as_mut() else {
            return sys::write_stdout(bytes);
        };
        let mut kept = Vec::new();
        filter.feed(bytes, &mut kept);
        parser.feed(&kept, |token| modes.apply(&token));
        sys::write_stdout(&kept)
    }

    fn finish(&mut self) {
        if let Some((_, _, modes)) = self.terminal.take() {
            let mut bytes = b"\x18\x1b\\\x1b[?2026l".to_vec();
            modes.reset(&mut bytes);
            bytes.extend_from_slice(b"\x1b[0m\x1b[?25h");
            let _ = sys::write_stdout(&bytes);
        }
    }
}

/// Print the last `lines` lines of a session's output, and with `follow`,
/// what comes after. A running session is asked; a finished one is read
/// from its log.
pub fn tail(names: &Names, session: &Session, lines: u32, follow: bool) -> i32 {
    let (prog, name) = (&names.progname, session.name());
    let mut printer = Printer::new();
    if let Ok(mut link) = Link::open(session) {
        let watch = Attach {
            replay: Replay::Lines(lines),
            readonly: true,
            dump: true,
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
            let printed = printer.print(&output);
            if printed.is_err() || !matches!(open, Ok(true)) || (live && !follow) {
                printer.finish();
                return 0;
            }
        }
    }

    // Nobody is writing a finished session's log; there is nothing to follow.
    use rustix::fs::OFlags;
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
    let _ = printer.print(&log[tail_start(&log, lines)..]);
    printer.finish();
    0
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
}
