//! Launching a program on a pty: the part of the design that has to behave
//! the same on Linux, macOS and FreeBSD.

use std::ffi::OsString;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::time::{Duration, Instant};

use ratch::sys;
use rustix::event::{poll, PollFd, PollFlags, Timespec};
use rustix::process::{Pid, Signal};
use rustix::termios::{LocalModes, Winsize};

/// Run `argv` on `slave`, keeping the caller's handle on it. Without one,
/// some systems discard what a short-lived program wrote before it is read.
fn spawn(slave: &OwnedFd, argv: &[OsString]) -> std::process::Child {
    sys::spawn_on_pty(slave.try_clone().unwrap(), argv, &[]).unwrap()
}

fn sh(script: &str) -> Vec<OsString> {
    ["/bin/sh", "-c", script]
        .iter()
        .map(OsString::from)
        .collect()
}

fn winsize(rows: u16, cols: u16) -> Winsize {
    Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// Read from the pty until `done` says so, the pty closes, or 10 s pass.
/// Returns what was read and how the read ended.
fn read_until(master: &OwnedFd, done: impl Fn(&str) -> bool) -> (String, &'static str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut out = Vec::new();
    loop {
        let text = String::from_utf8_lossy(&out).into_owned();
        if done(&text) {
            return (text, "matched");
        }
        if Instant::now() > deadline {
            return (text, "timeout");
        }
        let mut fds = [PollFd::new(master, PollFlags::IN)];
        let tick = Timespec {
            tv_sec: 0,
            tv_nsec: 100_000_000,
        };
        if poll(&mut fds, Some(&tick)).unwrap() == 0 {
            continue;
        }
        let mut buf = [0u8; 4096];
        match rustix::io::read(master, &mut buf) {
            Ok(0) => return (String::from_utf8_lossy(&out).into_owned(), "eof"),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => return (String::from_utf8_lossy(&out).into_owned(), "error"),
        }
    }
}

#[test]
fn child_gets_a_controlling_terminal() {
    let (master, slave) = sys::open_pty(None, None).unwrap();
    let mut child = spawn(
        &slave,
        &sh("if ( : </dev/tty ) 2>/dev/null; then echo CTTY_OK; else echo CTTY_MISSING; fi"),
    );
    let (out, how) = read_until(&master, |s| s.contains("CTTY_OK"));
    assert_eq!(how, "matched", "output: {out:?}");
    assert!(child.wait().unwrap().success());
}

#[test]
fn termios_and_size_reach_the_child() {
    let (probe, probe_slave) = sys::open_pty(None, None).unwrap();
    let mut termios = rustix::termios::tcgetattr(&probe_slave).unwrap();
    termios.local_modes.remove(LocalModes::ECHO);
    drop((probe, probe_slave));

    let (master, slave) = sys::open_pty(Some(&termios), Some(winsize(33, 77))).unwrap();
    let mut child = spawn(&slave, &sh("stty size; stty -a; echo END"));
    let (out, how) = read_until(&master, |s| s.contains("END"));
    assert_eq!(how, "matched", "output: {out:?}");
    assert!(out.contains("33 77"), "size not applied: {out:?}");
    assert!(out.contains("-echo "), "termios not applied: {out:?}");
    child.wait().unwrap();
}

#[test]
fn resize_and_winch_reach_the_foreground() {
    let (master, slave) = sys::open_pty(None, Some(winsize(24, 80))).unwrap();
    let script = "trap 'stty size; exit 0' WINCH; echo READY; while :; do sleep 0.05; done";
    let mut child = spawn(&slave, &sh(script));
    let (_, how) = read_until(&master, |s| s.contains("READY"));
    assert_eq!(how, "matched");

    sys::set_winsize(&master, winsize(40, 100)).unwrap();
    let pid = Pid::from_child(&child);
    sys::signal_foreground(master.as_fd(), pid, Signal::WINCH).unwrap();
    let (out, how) = read_until(&master, |s| s.contains("40 100"));
    assert_eq!(how, "matched", "output: {out:?}");
    assert!(child.wait().unwrap().success());
}

#[test]
fn signal_foreground_terminates_the_program() {
    let (master, slave) = sys::open_pty(None, None).unwrap();
    let mut child = spawn(&slave, &sh("echo READY; sleep 1000"));
    let (_, how) = read_until(&master, |s| s.contains("READY"));
    assert_eq!(how, "matched");
    sys::signal_foreground(master.as_fd(), Pid::from_child(&child), Signal::TERM).unwrap();
    assert_eq!(child.wait().unwrap().signal(), Some(Signal::TERM.as_raw()));
}

#[test]
fn child_starts_with_default_sigpipe() {
    let (master, slave) = sys::open_pty(None, None).unwrap();
    let mut child = spawn(&slave, &sh("kill -PIPE $$; echo SURVIVED"));
    assert_eq!(child.wait().unwrap().signal(), Some(Signal::PIPE.as_raw()));
    drop(slave);
    let (out, _) = read_until(&master, |_| false);
    assert!(
        !out.contains("SURVIVED"),
        "SIGPIPE was ignored in the child"
    );
}

#[test]
fn exec_failure_is_reported_to_the_caller() {
    let (_master, slave) = sys::open_pty(None, None).unwrap();
    let argv = [OsString::from("/nonexistent/ratch-test-program")];
    let err = sys::spawn_on_pty(slave, &argv, &[]).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn pty_reports_the_end_after_final_output() {
    let (master, slave) = sys::open_pty(None, None).unwrap();
    let mut child = spawn(&slave, &sh("echo LAST"));
    // Two systems, two rules, and a reader has to follow both. On macOS a
    // program cannot finish exiting while its output is unread: read before
    // waiting. On FreeBSD output still unread when the last handle on the
    // program's side closes may be discarded: keep one until it is read.
    let (out, how) = read_until(&master, |s| s.contains("LAST"));
    assert_eq!(how, "matched", "final output lost: {out:?}");
    assert!(child.wait().unwrap().success());
    drop(slave);
    let (_, how) = read_until(&master, |_| false);
    assert!(how == "eof" || how == "error", "pty never closed: {how}");
}
