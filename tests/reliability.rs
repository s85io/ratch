//! The properties the rewrite exists for: no client can hold a session up,
//! no output is lost or repeated, and a session can always be stopped.

mod common;

use std::time::{Duration, Instant};

use atch::proto::{Attach, ClientMsg, Exit, Replay};
use common::{contains, eventually, Ev, Home, Raw};

const SOON: Duration = Duration::from_secs(10);

fn attach(replay: Replay) -> ClientMsg<'static> {
    ClientMsg::Attach(Attach {
        replay,
        rows: 24,
        cols: 80,
        ..Attach::default()
    })
}

fn counter(home: &Home) -> u64 {
    std::fs::read_to_string(home.0.join("tick"))
        .ok()
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0)
}

/// The defect that started this: with the C master, a client that stopped
/// reading stopped the program.
#[test]
fn a_client_that_never_reads_does_not_stop_the_program() {
    let home = Home::new("stall");
    let tick = home.0.join("tick");
    home.start(
        "s",
        &format!(
            "i=0; while :; do i=$((i+1)); echo $i > {}; head -c 20000 /dev/zero | tr '\\0' x; echo; done",
            tick.display()
        ),
    );
    let mut deaf = Raw::connect(&home, "s");
    deaf.send(attach(Replay::None));
    assert!(eventually(SOON, || counter(&home) > 0));

    // Long enough to fill the socket and the whole ring several times over.
    std::thread::sleep(Duration::from_secs(1));
    let before = counter(&home);
    std::thread::sleep(Duration::from_secs(2));
    let after = counter(&home);
    assert!(after > before + 50, "program stalled: {before} -> {after}");

    // And when the deaf client finally listens, it is told it missed some.
    let (_, ev) = deaf.collect(SOON, |ev, _| *ev == Ev::Skipped);
    assert_eq!(ev, Ev::Skipped);
}

#[test]
fn a_client_that_keeps_up_gets_every_byte_once_and_the_exit_code() {
    let home = Home::new("exact");
    // The program waits for a key, so that we are attached before it speaks.
    home.start("s", "read x; seq 1 60000; exit 7");
    let mut client = Raw::connect(&home, "s");
    client.send(attach(Replay::None));
    client.send(ClientMsg::Input(b"\n"));
    let (output, ev) = client.collect(Duration::from_secs(30), |ev, _| matches!(ev, Ev::Exit(_)));
    assert_eq!(ev, Ev::Exit(Exit::Code(7)));
    assert_eq!(client.next(SOON), Ev::Closed);

    // What the master read from the pty is in the log, followed by a marker.
    // The client must have received exactly that: nothing lost, nothing twice.
    let log = std::fs::read(home.file("s.log")).unwrap();
    let end = log.len() - b"\r\n".len();
    let end = log[..end]
        .windows(3)
        .rposition(|w| w == b"\r\n[")
        .expect("end marker");
    let differ = output.iter().zip(&log[..end]).position(|(a, b)| a != b);
    assert_eq!(
        (output.len(), differ),
        (end, None),
        "client stream differs from the log"
    );

    // And the log is what the program wrote. (A pty on macOS now and then
    // emits a doubled carriage return under load; that is the kernel's.)
    let text = String::from_utf8(output).unwrap().replace("\r\r\n", "\r\n");
    let expected: String = std::iter::once("\r\n".to_owned())
        .chain((1..=60000).map(|i| format!("{i}\r\n")))
        .collect();
    assert!(
        text == expected,
        "the program's output did not arrive intact"
    );
}

#[test]
fn the_last_output_survives_the_program() {
    let home = Home::new("last");
    // Far more than a pty or a socket buffers, written right before exit.
    home.start(
        "s",
        "read x; head -c 300000 /dev/zero | tr '\\0' y; printf END",
    );
    let mut client = Raw::connect(&home, "s");
    client.send(attach(Replay::None));
    client.send(ClientMsg::Input(b"\n"));
    let (output, ev) = client.collect(Duration::from_secs(30), |ev, _| matches!(ev, Ev::Exit(_)));
    assert_eq!(ev, Ev::Exit(Exit::Code(0)));
    assert_eq!(output.len(), 2 + 300003);
    assert!(output.ends_with(b"END"));
}

#[test]
fn a_session_can_be_killed_while_input_is_backed_up() {
    let home = Home::new("backed");
    home.start("s", "sleep 1000");
    assert!(eventually(SOON, || home.file("s").exists()));

    // Nobody reads this input; the pusher will block, and should.
    let mut pusher = Raw::connect(&home, "s");
    let flood = std::thread::spawn(move || {
        let chunk = vec![b'x'; 4096];
        for _ in 0..2048 {
            let mut bytes = Vec::new();
            ClientMsg::Input(&chunk).encode(&mut bytes);
            if std::io::Write::write_all(&mut pusher.stream, &bytes).is_err() {
                break;
            }
        }
    });
    std::thread::sleep(Duration::from_millis(500));

    let started = Instant::now();
    let out = home.atch(&["kill", "s"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "kill failed: {text}");
    assert!(text.contains("stopped"), "{text}");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "kill had to wait for the grace period"
    );
    flood.join().unwrap();
}

#[test]
fn only_one_of_many_simultaneous_creators_wins() {
    let home = Home::new("race");
    let home = &home;
    let results: Vec<String> = std::thread::scope(|scope| {
        let racers: Vec<_> = (0..24)
            .map(|_| {
                scope.spawn(move || {
                    let out = home.atch(&["start", "s", "sleep", "1000"]);
                    String::from_utf8_lossy(&out.stdout).into_owned()
                })
            })
            .collect();
        racers.into_iter().map(|r| r.join().unwrap()).collect()
    });
    let started = results.iter().filter(|r| r.contains("started")).count();
    let refused = results
        .iter()
        .filter(|r| r.contains("already running"))
        .count();
    assert_eq!((started, refused), (1, 23), "{results:?}");
    // And the one session is reachable under its name.
    let out = home.atch(&["kill", "s"]);
    assert!(out.status.success());
}

#[test]
fn a_dead_masters_socket_is_replaced_not_obeyed() {
    let home = Home::new("stale");
    let mut master = home
        .command()
        .args(["run", "s", "sleep", "1000"])
        .spawn()
        .unwrap();
    // The info file is written once the program is running.
    assert!(eventually(SOON, || home.file("s.info").exists()));
    master.kill().unwrap();
    master.wait().unwrap();
    let listing = String::from_utf8_lossy(&home.atch(&["list"]).stdout).into_owned();
    assert!(listing.contains("[stale]"), "{listing}");

    home.start("s", "sleep 1000");
    let listing = String::from_utf8_lossy(&home.atch(&["list"]).stdout).into_owned();
    assert!(
        listing.contains('s') && !listing.contains("[stale]"),
        "{listing}"
    );
}

#[test]
fn history_is_served_from_memory_even_without_a_log() {
    let home = Home::new("nolog");
    let out = home.atch(&[
        "start",
        "-C",
        "0",
        "s",
        "sh",
        "-c",
        "echo remembered; sleep 1000",
    ]);
    assert!(out.status.success());
    assert!(eventually(SOON, || {
        String::from_utf8_lossy(&home.atch(&["tail", "s"]).stdout).contains("remembered")
    }));
    assert!(!home.file("s.log").exists());

    let mut client = Raw::connect(&home, "s");
    client.send(attach(Replay::Default));
    let (output, ev) = client.collect(SOON, |ev, _| *ev == Ev::ReplayEnd);
    assert_eq!(ev, Ev::ReplayEnd);
    assert!(contains(&output, b"remembered"));
}

#[test]
fn cleared_history_does_not_come_back() {
    let home = Home::new("clear");
    home.start("s", "echo secret-before; read x; echo after-$x; sleep 1000");
    assert!(eventually(SOON, || {
        std::fs::read_to_string(home.file("s.log")).is_ok_and(|log| log.contains("secret-before"))
    }));
    let out = home.atch(&["clear", "s"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("log cleared"));
    assert_eq!(std::fs::read(home.file("s.log")).unwrap(), b"");

    let mut client = Raw::connect(&home, "s");
    client.send(attach(Replay::Default));
    let (output, _) = client.collect(SOON, |ev, _| *ev == Ev::ReplayEnd);
    assert!(
        !contains(&output, b"secret-before"),
        "cleared history was replayed"
    );

    client.send(ClientMsg::Input(b"go\n"));
    let (output, _) = client.collect(SOON, |_, out| contains(out, b"after-go"));
    assert!(contains(&output, b"after-go"));
}

#[test]
fn history_survives_a_restart_under_the_same_name() {
    let home = Home::new("again");
    home.start("s", "echo first-life");
    assert!(eventually(SOON, || !home.file("s").exists()
        && home.file("s.log").exists()));
    home.start("s", "echo second-life; sleep 1000");
    let mut client = Raw::connect(&home, "s");
    client.send(attach(Replay::Default));
    let (output, _) = client.collect(SOON, |_, out| contains(out, b"second-life"));
    let text = String::from_utf8_lossy(&output);
    let (first, second) = (text.find("first-life"), text.find("second-life"));
    assert!(first.is_some() && first < second, "{text:?}");
}

#[test]
fn replayed_history_has_no_terminal_queries_but_live_output_does() {
    let home = Home::new("query");
    // A program that asks its terminal what it is, twice: once before we
    // attach (history) and once after (live).
    home.start(
        "s",
        "printf 'A\\033[cB\\033]11;?\\007C\\n'; read x; printf 'D\\033[cE\\n'; sleep 1000",
    );
    assert!(eventually(SOON, || {
        std::fs::read(home.file("s.log")).is_ok_and(|log| contains(&log, b"C\r\n"))
    }));
    let mut client = Raw::connect(&home, "s");
    client.send(attach(Replay::Default));
    let (history, ev) = client.collect(SOON, |ev, _| *ev == Ev::ReplayEnd);
    assert_eq!(ev, Ev::ReplayEnd);
    assert!(
        contains(&history, b"ABC\r\n"),
        "{:?}",
        String::from_utf8_lossy(&history)
    );

    client.send(ClientMsg::Input(b"\n"));
    let (live, _) = client.collect(SOON, |_, out| contains(out, b"E\r\n"));
    assert!(
        contains(&live, b"D\x1b[cE"),
        "{:?}",
        String::from_utf8_lossy(&live)
    );
}

#[test]
fn a_new_terminal_is_brought_into_the_programs_modes() {
    let home = Home::new("modes");
    home.start("s", "printf 'shell output\\n\\033[?1049h\\033[?1000h\\033[?1006h\\033[?25lFULLSCREEN'; sleep 1000");
    assert!(eventually(SOON, || {
        std::fs::read(home.file("s.log")).is_ok_and(|log| contains(&log, b"FULLSCREEN"))
    }));
    let mut client = Raw::connect(&home, "s");
    client.send(attach(Replay::Default));
    let (output, ev) = client.collect(SOON, |ev, _| *ev == Ev::ReplayEnd);
    assert_eq!(ev, Ev::ReplayEnd);
    // A full-screen program repaints itself; its old output is not replayed
    // underneath it, only the modes it depends on.
    assert_eq!(
        String::from_utf8_lossy(&output),
        "\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?25l"
    );
}

#[test]
fn garbage_on_the_socket_gets_an_error_not_a_crash() {
    let home = Home::new("garbage");
    home.start("s", "sleep 1000");
    assert!(eventually(SOON, || home.file("s").exists()));
    // A packet of the old C protocol.
    let mut old = std::os::unix::net::UnixStream::connect(home.file("s")).unwrap();
    std::io::Write::write_all(&mut old, &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap();
    let mut reply = Vec::new();
    old.set_read_timeout(Some(SOON)).unwrap();
    let _ = std::io::Read::read_to_end(&mut old, &mut reply);
    assert!(contains(&reply, b"expected a greeting") || contains(&reply, b"malformed"));

    // The session is unharmed.
    let mut client = Raw::connect(&home, "s");
    client.send(ClientMsg::Clear);
    assert_eq!(client.next(SOON), Ev::Ack);
}

#[test]
fn more_clients_than_the_limit_are_turned_away_and_the_rest_served() {
    let home = Home::new("many");
    home.start("s", "sleep 1000");
    assert!(eventually(SOON, || home.file("s").exists()));
    let mut clients: Vec<Raw> = (0..80).map(|_| Raw::connect(&home, "s")).collect();
    let mut served = 0;
    for client in &mut clients {
        client.send(ClientMsg::Clear);
    }
    for client in &mut clients {
        if client.next(Duration::from_secs(2)) == Ev::Ack {
            served += 1;
        }
    }
    assert_eq!(served, 64);
    drop(clients);
    // Room again once they leave.
    assert!(eventually(SOON, || {
        let mut client = Raw::connect(&home, "s");
        client.send(ClientMsg::Clear);
        client.next(Duration::from_secs(1)) == Ev::Ack
    }));
}
