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
