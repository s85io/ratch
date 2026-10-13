//! The interactive side, driven through a real pty: what a person at a
//! terminal sees and what their terminal is left like afterwards.

mod common;

use std::ffi::OsString;
use std::os::fd::OwnedFd;
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

use atch::sys::{self, PollFd, PollFlags, Winsize};
use common::{contains, eventually, Home, ATCH};

const SOON: Duration = Duration::from_secs(10);
const DETACH: &[u8] = b"\x1c";

/// `atch` running on a pty of its own, with us as the terminal.
struct Term {
    master: OwnedFd,
    child: Child,
    seen: Vec<u8>,
}

impl Term {
    fn spawn(home: &Home, args: &[&str]) -> Term {
        let size = Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let (master, slave) = sys::open_pty(None, Some(size)).unwrap();
        let mut argv = vec![OsString::from(ATCH)];
        argv.extend(args.iter().map(OsString::from));
        let env = [
            ("HOME".into(), home.0.clone().into_os_string()),
            // Empty counts as unset: the test itself may run inside a session.
            ("ATCH_SESSION".into(), OsString::new()),
            ("ATCH_SESSION_ID".into(), OsString::new()),
        ];
        let child = sys::spawn_on_pty(slave, &argv, &env).unwrap();
        Term {
            master,
            child,
            seen: Vec::new(),
        }
    }

    /// Read once, waiting at most `limit`. False when the pty has closed.
    fn pump(&mut self, limit: Duration) -> bool {
        let mut fds = [PollFd::new(&self.master, PollFlags::IN)];
        if sys::poll(&mut fds, Some(limit.as_millis() as u32)).unwrap() == 0 {
            return true;
        }
        let mut buf = [0u8; 4096];
        match rustix::io::read(&self.master, &mut buf) {
            Ok(0) | Err(_) => false,
            Ok(n) => {
                self.seen.extend_from_slice(&buf[..n]);
                true
            }
        }
    }

    /// Read until `needle` has been seen; panics with what was seen if not.
    fn expect(&mut self, needle: &str) {
        let deadline = Instant::now() + SOON;
        while !contains(&self.seen, needle.as_bytes()) {
            assert!(
                Instant::now() < deadline && self.pump(Duration::from_millis(100)),
                "never saw {needle:?} in {:?}",
                String::from_utf8_lossy(&self.seen)
            );
        }
    }

    fn send(&mut self, keys: &[u8]) {
        rustix::io::write(&self.master, keys).unwrap();
    }

    /// Forget what has been seen so far.
    fn mark(&mut self) {
        self.seen.clear();
    }

    /// Read to the end and collect the exit status.
    fn finish(mut self) -> (String, ExitStatus) {
        let deadline = Instant::now() + SOON;
        while Instant::now() < deadline && self.pump(Duration::from_millis(100)) {}
        let status = self.child.wait().unwrap();
        (String::from_utf8_lossy(&self.seen).into_owned(), status)
    }
}

#[test]
fn a_command_that_finishes_at_once_is_still_seen() {
    let home = Home::new("fast");
    let term = Term::spawn(&home, &["new", "s", "sh", "-c", "printf hello-and-goodbye"]);
    let (seen, status) = term.finish();
    assert!(seen.contains("session 's' created"), "{seen:?}");
    assert!(seen.contains("hello-and-goodbye"), "{seen:?}");
    assert!(seen.contains("session 's' exited after"), "{seen:?}");
    assert!(status.success());
}

#[test]
fn keys_reach_the_program_and_its_output_comes_back() {
    let home = Home::new("keys");
    let mut term = Term::spawn(
        &home,
        &[
            "new",
            "s",
            "sh",
            "-c",
            "read line; echo got-$line; sleep 1000",
        ],
    );
    // Typed ahead, before the session is even attached: must not be lost.
    term.send(b"ping\r");
    term.expect("got-ping");
    term.send(DETACH);
    let (seen, status) = term.finish();
    assert!(seen.contains("session 's' detached after"), "{seen:?}");
    assert!(status.success());
}

#[test]
fn detaching_gives_the_terminal_back_and_attaching_sets_it_up_again() {
    let home = Home::new("giveback");
    let fullscreen = "printf '\\033[?1049h\\033[?1000h\\033[?1006h\\033[?2004h\\033[?25l\\033[>1uREADY'; sleep 1000";
    let mut term = Term::spawn(&home, &["new", "s", "sh", "-c", fullscreen]);
    term.expect("READY");
    assert!(eventually(SOON, || {
        String::from_utf8_lossy(&home.atch(&["list"]).stdout).contains("[attached]")
    }));
    term.mark();
    term.send(DETACH);
    let (seen, status) = term.finish();
    assert!(status.success());
    for undo in [
        "\x1b[?1000l",
        "\x1b[?1006l",
        "\x1b[?2004l",
        "\x1b[<1u",
        "\x1b[?1049l",
        "\x1b[?25h",
        "\x1b[0m",
    ] {
        assert!(
            seen.contains(undo),
            "terminal not reset ({undo:?} missing): {seen:?}"
        );
    }
    // The alternate screen is left before the message, or it would vanish.
    assert!(seen.find("\x1b[?1049l") < seen.find("detached"), "{seen:?}");
    assert!(eventually(SOON, || {
        !String::from_utf8_lossy(&home.atch(&["list"]).stdout).contains("[attached]")
    }));

    // A different terminal attaches: it must be put in the program's modes.
    let mut term = Term::spawn(&home, &["attach", "s"]);
    term.expect("\x1b[?1049h");
    term.expect("\x1b[?25l");
    for redo in ["\x1b[?1000h", "\x1b[?1006h", "\x1b[?2004h", "\x1b[>1u"] {
        assert!(
            contains(&term.seen, redo.as_bytes()),
            "{redo:?} not restored: {:?}",
            String::from_utf8_lossy(&term.seen)
        );
    }
    assert!(
        !contains(&term.seen, b"READY"),
        "old screen contents were replayed under a full-screen program"
    );
    term.send(DETACH);
    let (seen, _) = term.finish();
    assert!(seen.contains("\x1b[?1049l"), "{seen:?}");
}

#[test]
fn a_session_ending_while_attached_also_gives_the_terminal_back() {
    let home = Home::new("ends");
    let mut term = Term::spawn(
        &home,
        &[
            "new",
            "s",
            "sh",
            "-c",
            "printf '\\033[?1049h\\033[?1003hUP'; read x",
        ],
    );
    term.expect("UP");
    term.mark();
    term.send(b"\r");
    let (seen, status) = term.finish();
    assert!(status.success());
    assert!(
        seen.contains("\x1b[?1003l") && seen.contains("\x1b[?1049l"),
        "{seen:?}"
    );
    assert!(seen.contains("exited after"), "{seen:?}");
}

#[test]
fn resizing_the_terminal_resizes_the_program() {
    let home = Home::new("resize");
    let script = "trap 'stty size' WINCH; stty size; while :; do sleep 0.05; done";
    let mut term = Term::spawn(&home, &["new", "s", "sh", "-c", script]);
    term.expect("24 80");
    let size = Winsize {
        ws_row: 40,
        ws_col: 132,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    sys::set_winsize(&term.master, size).unwrap();
    term.expect("40 132");
    term.send(DETACH);
    term.finish();
}

#[test]
fn a_session_cannot_be_attached_from_inside_itself() {
    let home = Home::new("inside");
    let inner = format!("{ATCH} attach s; echo inner-status-$?; sleep 1000");
    let mut term = Term::spawn(&home, &["new", "s", "sh", "-c", &inner]);
    term.expect("cannot attach to session 's' from within itself");
    term.expect("inner-status-1");
    term.send(DETACH);
    term.finish();
}

#[test]
fn attach_to_a_finished_session_is_an_error_and_shows_no_old_output() {
    let home = Home::new("gone");
    home.start("s", "echo old-output");
    assert!(eventually(SOON, || !home.file("s").exists()
        && home.file("s.log").exists()));
    let (seen, status) = Term::spawn(&home, &["attach", "s"]).finish();
    assert!(seen.contains("session 's' does not exist"), "{seen:?}");
    assert!(!seen.contains("old-output"), "{seen:?}");
    assert_eq!(status.code(), Some(1));
}

#[test]
fn the_detach_key_can_be_changed_or_turned_off() {
    let home = Home::new("detachkey");
    let mut term = Term::spawn(
        &home,
        &["new", "-e", "^A", "s", "sh", "-c", "echo READY; cat"],
    );
    // Wait for the program: before that our terminal is not yet raw, and
    // the old detach key would be its quit key.
    term.expect("READY");
    term.send(b"\x1c");
    term.send(b"still-here\r");
    term.expect("still-here");
    term.send(b"\x01");
    let (seen, _) = term.finish();
    assert!(seen.contains("detached"), "{seen:?}");
}

/// A session started by the C implementation must stay usable after the
/// binary is upgraded. Runs when `ATCH_C_REFERENCE` points at a C build.
#[test]
fn a_session_held_by_a_c_master_can_be_attached_pushed_and_killed() {
    let Some(c_atch) = std::env::var_os("ATCH_C_REFERENCE") else {
        eprintln!("skipped: set ATCH_C_REFERENCE to a C build of atch");
        return;
    };
    let home = Home::new("cmaster");
    // The C binary must be invoked as `atch` to share the session directory.
    let bin = home.0.join("atch");
    std::fs::copy(&c_atch, &bin).unwrap();
    let started = std::process::Command::new(&bin)
        .args(["start", "s", "cat"])
        .env("HOME", &home.0)
        .env_remove("ATCH_SESSION")
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stdout)
    );

    let mut push = home
        .command()
        .args(["push", "s"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(push.stdin.as_mut().unwrap(), b"pushed-by-rust\n").unwrap();
    drop(push.stdin.take());
    assert!(push.wait().unwrap().success());
    assert!(eventually(SOON, || {
        std::fs::read(home.file("s.log")).is_ok_and(|log| contains(&log, b"pushed-by-rust"))
    }));

    let mut term = Term::spawn(&home, &["attach", "s"]);
    term.expect("pushed-by-rust");
    term.send(b"typed-by-rust\r");
    term.expect("typed-by-rust");
    term.send(DETACH);
    let (seen, status) = term.finish();
    assert!(seen.contains("detached"), "{seen:?}");
    assert!(status.success());

    let out = home.atch(&["kill", "s"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success() && (text.contains("stopped") || text.contains("killed")),
        "{text}"
    );
}

#[test]
fn a_read_only_attach_watches_and_cannot_type() {
    let home = Home::new("readonly");
    home.start("s", "cat");
    let mut term = Term::spawn(&home, &["attach", "--ro", "s"]);
    assert!(eventually(SOON, || home.file("s").exists()));
    term.send(b"typed-by-the-watcher\r");
    let mut other = common::Raw::connect(&home, "s");
    other.send(atch::proto::ClientMsg::Input(b"typed-by-someone-else\n"));
    term.expect("typed-by-someone-else");
    // Our own pty may echo what we typed; what counts is what the program got.
    let log = std::fs::read(home.file("s.log")).unwrap();
    assert!(contains(&log, b"typed-by-someone-else") && !contains(&log, b"typed-by-the-watcher"));
    // A watcher does not count as someone being attached.
    assert!(!String::from_utf8_lossy(&home.atch(&["list"]).stdout).contains("[attached]"));
    term.send(DETACH);
    let (seen, _) = term.finish();
    assert!(seen.contains("detached"), "{seen:?}");
}

#[test]
fn the_amount_of_history_shown_on_attach_can_be_chosen() {
    let home = Home::new("history");
    home.start(
        "s",
        "i=1; while [ $i -le 50 ]; do printf 'line%02d\\n' $i; i=$((i+1)); done; sleep 1000",
    );
    assert!(eventually(SOON, || {
        std::fs::read(home.file("s.log")).is_ok_and(|log| contains(&log, b"line50"))
    }));
    let mut term = Term::spawn(&home, &["attach", "s", "-n", "2"]);
    term.expect("line50");
    assert!(contains(&term.seen, b"line49") && !contains(&term.seen, b"line48"));
    term.send(DETACH);
    term.finish();

    let mut term = Term::spawn(&home, &["attach", "s", "-n", "0"]);
    std::thread::sleep(Duration::from_millis(300));
    term.send(DETACH);
    let (seen, _) = term.finish();
    assert!(!seen.contains("line50"), "{seen:?}");
}
