//! The interactive side, driven through a real pty: what a person at a
//! terminal sees and what their terminal is left like afterwards.

mod common;

use std::ffi::OsString;
use std::os::fd::OwnedFd;
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

use common::{contains, eventually, Home, RATCH};
use ratch::sys::{self, PollFd, PollFlags, Winsize};

const SOON: Duration = Duration::from_secs(10);
const DETACH: &[u8] = b"\x1c";

/// `ratch` running on a pty of its own, with us as the terminal.
struct Term {
    master: OwnedFd,
    child: Child,
    seen: Vec<u8>,
}

impl Term {
    fn spawn(home: &Home, args: &[&str]) -> Term {
        Term::sized(home, args, 24, 80)
    }

    fn sized(home: &Home, args: &[&str], rows: u16, cols: u16) -> Term {
        let size = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let (master, slave) = sys::open_pty(None, Some(size)).unwrap();
        let mut argv = vec![OsString::from(RATCH)];
        argv.extend(args.iter().map(OsString::from));
        let env = [
            ("HOME".into(), home.0.clone().into_os_string()),
            // Empty counts as unset: the test itself may run inside a session.
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
        String::from_utf8_lossy(&home.ratch(&["list"]).stdout).contains("[attached]")
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
        !String::from_utf8_lossy(&home.ratch(&["list"]).stdout).contains("[attached]")
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

/// Programs on Node or Bun, Claude Code among them, ignore a SIGWINCH that
/// comes without a new size. Attaching at the size the program already has
/// must still get it to repaint, and leave the size as it was.
#[test]
fn a_program_that_repaints_only_for_a_new_size_repaints_on_attach() {
    let home = Home::new("samesize");
    let script = "last=$(stty size); echo \"size $last\"; \
        trap 'now=$(stty size); [ \"$now\" = \"$last\" ] || { last=$now; echo \"size $now\"; }' WINCH; \
        while :; do sleep 0.05; done";
    let mut term = Term::spawn(&home, &["new", "s", "sh", "-c", script]);
    term.expect("size 24 80");
    term.send(DETACH);
    term.finish();

    // Without history, so that what is seen is new; a watcher too.
    for args in [
        &["attach", "-n", "0", "s"][..],
        &["attach", "--ro", "-n", "0", "s"],
    ] {
        let mut term = Term::spawn(&home, args);
        term.expect("size 24 80");
        // Long enough for the real size to have come back.
        std::thread::sleep(Duration::from_millis(1500));
        for _ in 0..10 {
            term.pump(Duration::from_millis(50));
        }
        let seen = String::from_utf8_lossy(&term.seen).into_owned();
        let last = seen.rmatch_indices("size ").next().map(|(i, _)| &seen[i..]);
        assert!(
            last.is_some_and(|l| l.starts_with("size 24 80")),
            "{args:?}: {seen:?}"
        );
        term.send(DETACH);
        term.finish();
    }
}

/// A desktop and a phone on one session: the window has the size of the
/// terminal in use, and gets the other's back when that one leaves.
#[test]
fn the_size_follows_the_terminal_in_use() {
    let home = Home::new("owner");
    let script = "trap 'stty size' WINCH; stty size; while :; do sleep 0.05; done";
    let mut desk = Term::spawn(&home, &["new", "s", "sh", "-c", script]);
    desk.expect("24 80");

    let mut phone = Term::sized(&home, &["attach", "-n", "0", "s"], 20, 45);
    phone.expect("20 45");
    // Leaving gives the window back to the desktop.
    phone.send(DETACH);
    phone.finish();
    desk.mark();
    desk.expect("24 80");

    let mut phone = Term::sized(&home, &["attach", "-n", "0", "s"], 20, 45);
    phone.expect("20 45");
    // A key on the desktop takes the window back.
    desk.mark();
    desk.send(b"\x1b[I");
    desk.expect("24 80");
    // The phone's terminal changing size while it is not in use changes
    // nothing; nor does a phone going into the background.
    desk.mark();
    let smaller = Winsize {
        ws_row: 18,
        ws_col: 40,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    sys::set_winsize(&phone.master, smaller).unwrap();
    phone.send(b"\x1b[O");
    std::thread::sleep(Duration::from_millis(500));
    for _ in 0..5 {
        desk.pump(Duration::from_millis(50));
    }
    assert!(
        !contains(&desk.seen, b"18 40"),
        "{:?}",
        String::from_utf8_lossy(&desk.seen)
    );
    // Until the desktop leaves.
    desk.send(DETACH);
    desk.finish();
    phone.expect("18 40");
    phone.send(DETACH);
    phone.finish();
}

/// With the kitty keyboard protocol on, which agents such as omp switch on,
/// Ctrl-\ arrives as an escape sequence.
#[test]
fn the_detach_key_works_in_the_kitty_keyboard_protocol() {
    let home = Home::new("kitty");
    let mut term = Term::spawn(
        &home,
        &[
            "new",
            "s",
            "sh",
            "-c",
            "printf '\\033[>1u'; echo ready; sleep 1000",
        ],
    );
    term.expect("ready");
    term.send(b"\x1b[92;5u");
    let (seen, _) = term.finish();
    assert!(seen.contains("detached"), "{seen:?}");
}

/// History and the program's repaint go in while the terminal holds its
/// picture, and are shown at once.
#[test]
fn attaching_holds_the_picture_until_the_program_has_drawn() {
    let home = Home::new("still");
    home.start(
        "s",
        "echo old-output; trap 'printf \"\\033[?2026hframe\\033[?2026l\"' WINCH; while :; do sleep 0.05; done",
    );
    assert!(eventually(SOON, || {
        std::fs::read(home.file("s.log")).is_ok_and(|log| contains(&log, b"old-output"))
    }));
    std::thread::sleep(Duration::from_millis(2100));
    let mut term = Term::spawn(&home, &["attach", "s"]);
    term.expect("frame");
    std::thread::sleep(Duration::from_millis(300));
    term.pump(Duration::from_millis(100));
    let seen = String::from_utf8_lossy(&term.seen).into_owned();
    let begin = seen.find("\x1b[?2026h").expect("no hold");
    let status = seen.find("restoring session 's'").expect("no status");
    let old = seen.find("old-output").unwrap();
    let frame = seen.find("frame").unwrap();
    let end = seen.rfind("\x1b[?2026l").expect("not released");
    assert!(
        status < begin && begin < old && old < frame && frame < end,
        "{seen:?}"
    );
    // The program's own update did not end the hold early.
    assert_eq!(seen.matches("\x1b[?2026l").count(), 1, "{seen:?}");
    term.send(DETACH);
    term.finish();
}

#[test]
fn a_session_cannot_be_attached_from_inside_itself() {
    let home = Home::new("inside");
    let inner = format!("{RATCH} attach s; echo inner-status-$?; sleep 1000");
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

#[test]
fn a_read_only_attach_watches_and_cannot_type() {
    let home = Home::new("readonly");
    home.start("s", "cat");
    let mut term = Term::spawn(&home, &["attach", "--ro", "s"]);
    assert!(eventually(SOON, || home.file("s").exists()));
    term.send(b"typed-by-the-watcher\r");
    let mut other = common::Raw::connect(&home, "s");
    other.send(ratch::proto::ClientMsg::Input(b"typed-by-someone-else\n"));
    term.expect("typed-by-someone-else");
    // Our own pty may echo what we typed; what counts is what the program got.
    let log = std::fs::read(home.file("s.log")).unwrap();
    assert!(contains(&log, b"typed-by-someone-else") && !contains(&log, b"typed-by-the-watcher"));
    // A watcher does not count as someone being attached.
    assert!(!String::from_utf8_lossy(&home.ratch(&["list"]).stdout).contains("[attached]"));
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

#[test]
fn a_held_session_outlives_its_command() {
    let home = Home::new("hold");
    let mut term = Term::spawn(
        &home,
        &["new", "-H", "s", "sh", "-c", "echo did-the-work; exit 3"],
    );
    term.expect("did-the-work");
    term.expect("command exited with status 3");
    // A shell is there now; the session is still running.
    assert!(String::from_utf8_lossy(&home.ratch(&["list"]).stdout).contains("s "));
    term.send(DETACH);
    let (seen, _) = term.finish();
    assert!(seen.contains("detached"), "{seen:?}");
}

#[test]
fn the_screen_can_be_wiped_before_attaching() {
    let home = Home::new("wipe");
    home.start("s", "echo hello; sleep 1000");
    let mut term = Term::spawn(&home, &["attach", "-R", "clear", "s"]);
    term.expect("hello");
    let seen = String::from_utf8_lossy(&term.seen).into_owned();
    assert!(
        seen.find("\x1b[H\x1b[2J\x1b[3J") < seen.find("hello"),
        "{seen:?}"
    );
    term.send(DETACH);
    term.finish();
}

#[test]
fn options_can_come_from_the_environment() {
    let home = Home::new("envopts");
    home.start("s", "echo READY; cat");
    let size = Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let (master, slave) = sys::open_pty(None, Some(size)).unwrap();
    let argv: Vec<OsString> = [RATCH, "attach", "s"].iter().map(OsString::from).collect();
    let env = [
        ("HOME".into(), home.0.clone().into_os_string()),
        ("RATCH_SESSION".into(), OsString::new()),
        ("RATCH_SESSION_ID".into(), OsString::new()),
        ("RATCH_OPTS".into(), OsString::from("-e ^A -q")),
    ];
    let child = sys::spawn_on_pty(slave, &argv, &env).unwrap();
    let mut term = Term {
        master,
        child,
        seen: Vec::new(),
    };
    term.expect("READY");
    term.send(b"\x01");
    let (seen, status) = term.finish();
    // Detached by the key from the environment, and quietly? No: the parting
    // message is always shown; -q silences only notices like "created".
    assert!(seen.contains("detached"), "{seen:?}");
    assert!(status.success());
}

#[test]
fn attaching_records_the_terminals_environment_for_env() {
    let home = Home::new("envcmd");
    home.start("s", "sleep 1000");
    let out = home.ratch(&["env", "s"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "nothing recorded before anyone attached"
    );

    let size = Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let (master, slave) = sys::open_pty(None, Some(size)).unwrap();
    let argv: Vec<OsString> = [RATCH, "attach", "s"].iter().map(OsString::from).collect();
    let env = [
        ("HOME".into(), home.0.clone().into_os_string()),
        ("RATCH_SESSION".into(), OsString::new()),
        ("RATCH_SESSION_ID".into(), OsString::new()),
        (
            "SSH_AUTH_SOCK".into(),
            OsString::from("/tmp/it's an agent.sock"),
        ),
        ("TERM".into(), OsString::from("xterm-test")),
        ("RATCH_ENV_VARS".into(), OsString::from("MY_VAR, 9bad")),
        ("MY_VAR".into(), OsString::from("mine")),
    ];
    let child = sys::spawn_on_pty(slave, &argv, &env).unwrap();
    let mut term = Term {
        master,
        child,
        seen: Vec::new(),
    };
    assert!(eventually(SOON, || home.file("s.env").exists()));
    term.send(DETACH);
    term.finish();

    let sh = String::from_utf8_lossy(&home.ratch(&["env", "--sh", "s"]).stdout).into_owned();
    assert!(sh.contains("export TERM='xterm-test'\n"), "{sh}");
    assert!(
        sh.contains("export SSH_AUTH_SOCK='/tmp/it'\\''s an agent.sock'\n"),
        "{sh}"
    );
    assert!(
        sh.contains("export MY_VAR='mine'\n") && !sh.contains("9bad"),
        "{sh}"
    );
    let fish = String::from_utf8_lossy(&home.ratch(&["env", "--fish", "s"]).stdout).into_owned();
    assert!(fish.contains("set -gx TERM 'xterm-test'\n"), "{fish}");
    assert!(
        fish.contains("set -gx SSH_AUTH_SOCK '/tmp/it\\'s an agent.sock'\n"),
        "{fish}"
    );
}
