//! Full-screen programs people actually run, through a detach and a
//! reattach. Each test is skipped where its program is not installed.
//!
//! What can be checked without eyes: the program is told to repaint and
//! does, and the terminal is put into the modes the program uses and taken
//! out of them again. (That replayed history asks the terminal nothing is
//! checked in `reliability.rs`, where history and live output can be told
//! apart; a repainting program may well ask its terminal something anew.)

mod common;

use std::ffi::OsString;
use std::os::fd::OwnedFd;
use std::process::Child;
use std::time::{Duration, Instant};

use common::{contains, Home, RATCH};
use ratch::sys::{self, PollFd, PollFlags, Winsize};
use ratch::vt::{Modes, Parser};

const DETACH: &[u8] = b"\x1c";

fn installed(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

struct Term {
    master: OwnedFd,
    child: Child,
    seen: Vec<u8>,
}

impl Term {
    fn spawn(home: &Home, args: &[&str]) -> Term {
        let size = Winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let (master, slave) = sys::open_pty(None, Some(size)).unwrap();
        let mut argv = vec![OsString::from(RATCH)];
        argv.extend(args.iter().map(OsString::from));
        let env = [
            ("HOME".into(), home.0.clone().into_os_string()),
            ("TERM".into(), OsString::from("xterm-256color")),
            ("RATCH_SESSION".into(), OsString::new()),
            ("RATCH_SESSION_ID".into(), OsString::new()),
        ];
        let child = sys::spawn_on_pty(slave, &argv, &env).unwrap();
        Term {
            master,
            child,
            seen: Vec::new(),
        }
    }

    /// Read until nothing has arrived for `quiet`, or `limit` is up.
    fn settle(&mut self, quiet: Duration, limit: Duration) {
        let deadline = Instant::now() + limit;
        let mut last = Instant::now();
        while Instant::now() < deadline && last.elapsed() < quiet {
            let mut fds = [PollFd::new(&self.master, PollFlags::IN)];
            if sys::poll(&mut fds, Some(50)).unwrap() == 0 {
                continue;
            }
            let mut buf = [0u8; 8192];
            match rustix::io::read(&self.master, &mut buf) {
                Ok(n) if n > 0 => {
                    self.seen.extend_from_slice(&buf[..n]);
                    last = Instant::now();
                }
                _ => return,
            }
        }
    }

    /// Read until `done` holds for what has been seen, or `limit` is up.
    fn until(&mut self, limit: Duration, done: impl Fn(&[u8]) -> bool) {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline && !done(&self.seen) {
            self.settle(Duration::from_millis(60), Duration::from_millis(60));
        }
    }

    fn send(&mut self, keys: &[u8]) {
        rustix::io::write(&self.master, keys).unwrap();
    }

    fn finish(mut self) -> Vec<u8> {
        self.settle(Duration::from_secs(2), Duration::from_secs(10));
        let _ = self.child.wait();
        self.seen
    }
}

/// The modes a terminal would be in after receiving `bytes`.
fn modes_after(bytes: &[u8]) -> Modes {
    let mut modes = Modes::new();
    Parser::new().feed(bytes, |token| modes.apply(&token));
    modes
}

/// Start `program` in a session, detach, attach from a second terminal,
/// detach again; check each terminal at each step.
fn round_trip(tag: &str, program: &[&str], quit: &[u8]) {
    let home = Home::new(tag);
    let mut args = vec!["new", "s"];
    args.extend_from_slice(program);

    let mut first = Term::spawn(&home, &args);
    // Wait for the program to take over the screen, then for it to finish
    // setting itself up.
    first.until(Duration::from_secs(20), |seen| {
        modes_after(seen).alt_screen()
    });
    first.settle(Duration::from_millis(700), Duration::from_secs(5));
    let running = modes_after(&first.seen);
    assert!(
        running.alt_screen(),
        "{tag} did not go full-screen: {:?}",
        String::from_utf8_lossy(&first.seen)
    );
    first.send(DETACH);
    let first_seen = first.finish();
    assert!(
        contains(&first_seen, b"detached"),
        "{tag}: no detach message"
    );
    assert_eq!(
        modes_after(&first_seen),
        Modes::new(),
        "{tag}: first terminal left in the program's modes"
    );

    // A terminal that has never seen this program.
    let mut second = Term::spawn(&home, &["attach", "s"]);
    let mut restore = Vec::new();
    running.restore(&mut restore);
    // Some programs repaint on their own clock, not the instant they are told.
    second.until(Duration::from_secs(10), |seen| {
        seen.len() > restore.len() + 200
    });
    second.settle(Duration::from_millis(300), Duration::from_secs(2));
    assert_eq!(
        modes_after(&second.seen),
        running,
        "{tag}: second terminal is not in the modes the program set up"
    );
    // Told the window (may have) changed, a full-screen program repaints:
    // more arrives than the handful of bytes that set the modes.
    assert!(
        second.seen.len() > restore.len() + 200,
        "{tag} did not repaint for the new terminal ({} bytes)",
        second.seen.len()
    );
    second.send(DETACH);
    let second_seen = second.finish();
    assert_eq!(
        modes_after(&second_seen),
        Modes::new(),
        "{tag}: second terminal left in the program's modes"
    );

    // And the program is still alive and takes keys: ask it to quit.
    let mut third = Term::spawn(&home, &["attach", "s"]);
    third.until(Duration::from_secs(10), |seen| {
        modes_after(seen).alt_screen()
    });
    third.settle(Duration::from_millis(300), Duration::from_secs(2));
    third.send(quit);
    let third_seen = third.finish();
    assert!(
        contains(&third_seen, b"exited after"),
        "{tag} did not quit: {:?}",
        String::from_utf8_lossy(&third_seen[third_seen.len().saturating_sub(300)..])
    );
    assert_eq!(
        modes_after(&third_seen),
        Modes::new(),
        "{tag}: terminal not clean after the program quit"
    );
}

#[test]
fn vim() {
    if !installed("vim") {
        eprintln!("skipped: vim is not installed");
        return;
    }
    round_trip(
        "vim",
        &["vim", "-u", "NONE", "-N", "-c", "set mouse=a"],
        b"\x1b:q!\r",
    );
}

#[test]
fn neovim() {
    if !installed("nvim") {
        eprintln!("skipped: nvim is not installed");
        return;
    }
    round_trip(
        "nvim",
        &["nvim", "-u", "NONE", "-c", "set mouse=a"],
        b"\x1b:q!\r",
    );
}

#[test]
fn htop() {
    if !installed("htop") {
        eprintln!("skipped: htop is not installed");
        return;
    }
    round_trip("htop", &["htop"], b"q");
}

// FreeBSD's terminal description for xterm has no alternate screen, so its
// less is not a full-screen program in the sense tested here.
#[cfg(not(target_os = "freebsd"))]
#[test]
fn less() {
    if !installed("less") {
        eprintln!("skipped: less is not installed");
        return;
    }
    round_trip("less", &["less", "/etc/services"], b"q");
}
