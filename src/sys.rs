//! Layer 0: everything that talks to the operating system.
//!
//! This is the only module allowed to use `unsafe`, and it needs it in exactly
//! one place: the hook that runs in the child between fork and exec.
#![allow(unsafe_code)]

use std::ffi::OsString;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

use rustix::fs::{Mode, OFlags};
use rustix::io::FdFlags;
use rustix::process::{Pid, Signal};
use rustix::pty::OpenptFlags;
use rustix::termios::OptionalActions;

pub use rustix::termios::{Termios, Winsize};

/// Open a pseudo-terminal pair, returning `(master, slave)`.
///
/// `termios` and `size` are applied to the slave before anything runs on it,
/// so the program starts with the creator's terminal settings.
pub fn open_pty(
    termios: Option<&Termios>,
    size: Option<Winsize>,
) -> io::Result<(OwnedFd, OwnedFd)> {
    let master = rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)?;
    rustix::io::fcntl_setfd(&master, FdFlags::CLOEXEC)?;
    rustix::pty::grantpt(&master)?;
    rustix::pty::unlockpt(&master)?;
    let name = rustix::pty::ptsname(&master, Vec::new())?;
    let slave = rustix::fs::open(
        name.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    if let Some(termios) = termios {
        rustix::termios::tcsetattr(&slave, OptionalActions::Now, termios)?;
    }
    if let Some(size) = size {
        rustix::termios::tcsetwinsize(&slave, size)?;
    }
    Ok((master, slave))
}

/// Run `argv` as a session leader with `slave` as its controlling terminal
/// and its stdin, stdout and stderr.
///
/// A failed exec is reported here as an error rather than by the child.
pub fn spawn_on_pty(
    slave: OwnedFd,
    argv: &[OsString],
    env: &[(OsString, OsString)],
) -> io::Result<Child> {
    let program = argv
        .first()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut cmd = Command::new(program);
    cmd.args(&argv[1..]);
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    // SAFETY: the hook runs in the forked child before exec. It makes two raw
    // system calls and nothing else: no allocation, no locks, no stdio. By
    // then the standard library has already made the slave fd 0.
    unsafe {
        cmd.pre_exec(|| {
            rustix::process::setsid()?;
            rustix::process::ioctl_tiocsctty(rustix::stdio::stdin())?;
            Ok(())
        });
    }
    cmd.spawn()
}

/// Send `sig` to whatever is in the foreground of the pty, falling back to
/// the process group of the program we started.
pub fn signal_foreground(master: BorrowedFd<'_>, child: Pid, sig: Signal) -> io::Result<()> {
    if let Ok(group) = rustix::termios::tcgetpgrp(master) {
        if rustix::process::kill_process_group(group, sig).is_ok() {
            return Ok(());
        }
    }
    Ok(rustix::process::kill_process_group(child, sig)?)
}

/// Change the size of the pty. The caller decides whether to follow up with
/// a `SIGWINCH`; the kernel only sends one when the size actually changes.
pub fn set_winsize(master: impl AsFd, size: Winsize) -> io::Result<()> {
    Ok(rustix::termios::tcsetwinsize(master, size)?)
}

pub fn set_nonblocking(fd: impl AsFd) -> io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(&fd)?;
    Ok(rustix::fs::fcntl_setfl(&fd, flags | OFlags::NONBLOCK)?)
}

/// Terminal settings of `fd`, or `None` if it is not a terminal.
pub fn termios_of(fd: impl AsFd) -> Option<Termios> {
    rustix::termios::tcgetattr(fd).ok()
}

pub fn winsize_of(fd: impl AsFd) -> Option<Winsize> {
    rustix::termios::tcgetwinsize(fd).ok()
}

pub fn set_termios(fd: impl AsFd, termios: &Termios) -> io::Result<()> {
    Ok(rustix::termios::tcsetattr(
        fd,
        OptionalActions::Drain,
        termios,
    )?)
}

/// The settings for a terminal that passes every key straight through.
pub fn raw_termios(original: &Termios) -> Termios {
    use rustix::termios::{ControlModes, InputModes, LocalModes, OutputModes, SpecialCodeIndex};
    let mut raw = original.clone();
    raw.input_modes.remove(
        InputModes::IGNBRK
            | InputModes::BRKINT
            | InputModes::PARMRK
            | InputModes::ISTRIP
            | InputModes::INLCR
            | InputModes::IGNCR
            | InputModes::ICRNL
            | InputModes::IXON
            | InputModes::IXOFF,
    );
    raw.output_modes.remove(OutputModes::OPOST);
    raw.local_modes.remove(
        LocalModes::ECHO
            | LocalModes::ECHONL
            | LocalModes::ICANON
            | LocalModes::ISIG
            | LocalModes::IEXTEN,
    );
    raw.control_modes
        .remove(ControlModes::CSIZE | ControlModes::PARENB);
    raw.control_modes.insert(ControlModes::CS8);
    raw.special_codes[SpecialCodeIndex::VLNEXT] = 0;
    raw.special_codes[SpecialCodeIndex::VMIN] = 1;
    raw.special_codes[SpecialCodeIndex::VTIME] = 0;
    raw
}

/// The key that suspends, as configured on this terminal.
pub fn suspend_key(termios: &Termios) -> u8 {
    termios.special_codes[rustix::termios::SpecialCodeIndex::VSUSP]
}

/// Is the program on this pty reading keys itself, one at a time and without
/// echo? Only then is a form feed a safe way to ask it to repaint.
pub fn pty_is_cbreak(master: impl AsFd) -> bool {
    use rustix::termios::{LocalModes, SpecialCodeIndex};
    termios_of(master).is_some_and(|t| {
        !t.local_modes
            .intersects(LocalModes::ECHO | LocalModes::ICANON)
            && t.special_codes[SpecialCodeIndex::VMIN] == 1
    })
}

/// Detach from the terminal we were started on; failure means we already are.
pub fn new_session() {
    let _ = rustix::process::setsid();
}

/// Point stdin, stdout and stderr at `/dev/null`.
pub fn detach_stdio() -> io::Result<()> {
    let null = rustix::fs::open("/dev/null", OFlags::RDWR, Mode::empty())?;
    rustix::stdio::dup2_stdin(&null)?;
    rustix::stdio::dup2_stdout(&null)?;
    rustix::stdio::dup2_stderr(&null)?;
    Ok(())
}

pub fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

pub fn pid_of(child: &Child) -> Pid {
    Pid::from_child(child)
}

/// Stop the calling process the way the terminal's suspend key would.
pub fn suspend_self() {
    let _ = rustix::process::kill_process(rustix::process::getpid(), Signal::TSTP);
}

/// Is whoever is on the other end of this socket running as us?
/// Where the system cannot say, the socket's permissions are the only guard.
pub fn peer_is_us(_stream: &std::os::unix::net::UnixStream) -> bool {
    #[cfg(target_os = "linux")]
    {
        match rustix::net::sockopt::socket_peercred(_stream) {
            Ok(cred) => cred.uid.as_raw() == uid(),
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// Run `f` with the working directory set to `dir`, then put it back.
/// Unix socket paths are short; this is how a long one gets bound or
/// connected, by its last component.
pub fn in_dir<T>(dir: impl AsFd, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let here = rustix::fs::open(
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    rustix::process::fchdir(dir)?;
    let result = f();
    // Getting back matters more than what `f` returned.
    rustix::process::fchdir(&here)?;
    result
}

/// Run `f` with a file creation mask that leaves new sockets private.
pub fn with_private_umask<T>(f: impl FnOnce() -> T) -> T {
    let old = rustix::process::umask(Mode::from_raw_mode(0o177));
    let result = f();
    rustix::process::umask(old);
    result
}

/// Eight bytes nobody can guess, for telling sessions apart.
pub fn random_u64() -> u64 {
    use std::io::Read;
    let mut bytes = [0u8; 8];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)) {
        Ok(()) => u64::from_ne_bytes(bytes),
        Err(_) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64);
            now ^ u64::from(std::process::id()) << 32
        }
    }
}

/// Wait until one of `fds` is ready or `timeout_ms` passes. An interrupted
/// wait reports nothing ready; the caller's loop looks again.
pub fn poll(fds: &mut [PollFd<'_>], timeout_ms: Option<u32>) -> io::Result<usize> {
    let timeout = timeout_ms.map(|ms| Timespec {
        tv_sec: i64::from(ms / 1000),
        tv_nsec: ((ms % 1000) * 1_000_000).into(),
    });
    match rustix::event::poll(fds, timeout.as_ref()) {
        Ok(n) => Ok(n),
        Err(rustix::io::Errno::INTR) => Ok(0),
        Err(e) => Err(e.into()),
    }
}

use rustix::event::Timespec;
pub use rustix::event::{PollFd, PollFlags};

/// Signals turned into something a poll loop can wait on.
///
/// The pipe is a doorbell, not a queue: several signals may ring it once.
/// After it rings, ask about every signal with [`Signals::take`].
pub struct Signals {
    rx: std::os::unix::net::UnixStream,
    flags: Vec<(i32, std::sync::Arc<std::sync::atomic::AtomicBool>)>,
}

impl Signals {
    pub fn new(signals: &[i32]) -> io::Result<Signals> {
        use std::os::unix::net::UnixStream;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let (rx, tx) = UnixStream::pair()?;
        rx.set_nonblocking(true)?;
        tx.set_nonblocking(true)?;
        let mut flags = Vec::new();
        for &sig in signals {
            // Flag first: by the time the doorbell rings, the flag is set.
            let flag = Arc::new(AtomicBool::new(false));
            signal_hook::flag::register(sig, Arc::clone(&flag))?;
            signal_hook::low_level::pipe::register(sig, tx.try_clone()?)?;
            flags.push((sig, flag));
        }
        Ok(Signals { rx, flags })
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.rx.as_fd()
    }

    /// Silence the doorbell.
    pub fn drain(&self) {
        use std::io::Read;
        let mut buf = [0u8; 64];
        while matches!((&self.rx).read(&mut buf), Ok(n) if n > 0) {}
    }

    /// Has `signal` arrived since the last time this was asked?
    pub fn take(&self, signal: i32) -> bool {
        self.flags
            .iter()
            .find(|(sig, _)| *sig == signal)
            .is_some_and(|(_, flag)| flag.swap(false, std::sync::atomic::Ordering::SeqCst))
    }
}

pub mod signal {
    pub use signal_hook::consts::{
        SIGCHLD, SIGCONT, SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGTTIN, SIGTTOU, SIGWINCH, SIGXFSZ,
    };
}

/// The signals a client may ask the master to deliver.
pub fn signal_from_number(number: u8) -> Option<Signal> {
    Some(match i32::from(number) {
        n if n == Signal::TERM.as_raw() => Signal::TERM,
        n if n == Signal::KILL.as_raw() => Signal::KILL,
        n if n == Signal::INT.as_raw() => Signal::INT,
        n if n == Signal::HUP.as_raw() => Signal::HUP,
        n if n == Signal::QUIT.as_raw() => Signal::QUIT,
        n if n == Signal::WINCH.as_raw() => Signal::WINCH,
        _ => return None,
    })
}

pub const SIGNAL_TERM: u8 = 15;
pub const SIGNAL_KILL: u8 = 9;

/// An error as a person would say it: "No such file or directory", without
/// the "(os error 2)" the standard library appends.
pub fn describe(error: &io::Error) -> String {
    let text = error.to_string();
    match text.find(" (os error") {
        Some(end) => text[..end].to_owned(),
        None => text,
    }
}

static SAVED_TERMINAL: std::sync::OnceLock<Termios> = std::sync::OnceLock::new();

/// Keep a copy of the user's terminal settings where a panic can find them.
pub fn remember_terminal(original: &Termios) {
    let _ = SAVED_TERMINAL.set(original.clone());
}

/// Put the user's terminal back, if we ever changed it. For the panic hook:
/// the process is about to abort and nothing else will run.
pub fn restore_remembered_terminal() {
    if let Some(original) = SAVED_TERMINAL.get() {
        let _ = rustix::termios::tcsetattr(rustix::stdio::stdin(), OptionalActions::Now, original);
    }
}

/// Write all of `data` to stdout, which is a terminal that may make us wait.
pub fn write_stdout(mut data: &[u8]) -> io::Result<()> {
    let out = rustix::stdio::stdout();
    while !data.is_empty() {
        match rustix::io::write(out, data) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => data = &data[n..],
            Err(rustix::io::Errno::INTR) => {}
            Err(rustix::io::Errno::AGAIN) => {
                poll(&mut [PollFd::from_borrowed_fd(out, PollFlags::OUT)], None)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub fn read_stdin(buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match rustix::io::read(rustix::stdio::stdin(), &mut *buf) {
            Err(rustix::io::Errno::INTR) => {}
            other => return Ok(other?),
        }
    }
}

pub fn stdin_fd() -> BorrowedFd<'static> {
    rustix::stdio::stdin()
}

pub fn stdout_is_terminal() -> bool {
    rustix::termios::isatty(rustix::stdio::stdout())
}

/// One field of the caller's line in `/etc/passwd`: 5 is the home directory,
/// 6 the shell. Systems that keep users elsewhere simply have no answer.
pub fn passwd_field(index: usize) -> Option<String> {
    let uid = uid().to_string();
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        (fields.get(2) == Some(&uid.as_str()))
            .then(|| fields.get(index).map(|f| (*f).to_owned()))
            .flatten()
            .filter(|f| !f.is_empty())
    })
}

/// Close every descriptor above stderr that this process was started with.
///
/// For a process about to become a daemon: whatever its parent happened to
/// have open (a pipe someone is waiting on, a file under a lock) must not be
/// kept open for the life of the session, nor handed on to the program.
///
/// Call it first thing, before this process has opened anything itself.
pub fn close_inherited_fds() {
    let listed: Vec<i32> = std::fs::read_dir("/dev/fd")
        .map(|dir| {
            dir.flatten()
                .filter_map(|e| e.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    // Some systems list only the first three there; fall back to a sweep.
    let fds = if listed.iter().any(|&fd| fd > 2) {
        listed
    } else {
        (3..256).collect()
    };
    for fd in fds.into_iter().filter(|&fd| fd > 2) {
        // SAFETY: the caller has opened nothing yet, so no value in this
        // process owns any of these descriptors. Some are not open at all
        // (the sweep, or the handle just used for listing), and closing one
        // of those is treated as a bug by the libraries, so ask first.
        unsafe {
            if rustix::io::fcntl_getfd(BorrowedFd::borrow_raw(fd)).is_ok() {
                rustix::io::close(fd);
            }
        }
    }
}
