//! Shared by the integration tests: a private HOME per test, the binary under
//! test, and a bare protocol client that does only what a test tells it to.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use ratch::proto::{ClientMsg, Decoder, Exit, ServerMsg, VERSION};

pub const RATCH: &str = env!("CARGO_BIN_EXE_ratch");

/// A throwaway home directory. Sessions left running in it are killed on drop.
pub struct Home(pub PathBuf);

impl Home {
    pub fn new(tag: &str) -> Home {
        // Short on purpose: socket paths must fit in sun_path.
        let path = PathBuf::from(format!("/tmp/ratch-i{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).unwrap();
        Home(path)
    }

    pub fn command(&self) -> Command {
        let mut cmd = Command::new(RATCH);
        cmd.env("HOME", &self.0)
            .env_remove("RATCH_SESSION")
            .env_remove("RATCH_SESSION_ID");
        cmd
    }

    pub fn ratch(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// Start a detached session running `sh -c script`.
    pub fn start(&self, name: &str, script: &str) {
        let out = self.ratch(&["start", name, "sh", "-c", script]);
        assert!(
            out.status.success(),
            "start failed: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    pub fn dir(&self) -> PathBuf {
        self.0.join(".cache/ratch")
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.dir().join(name)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        if let Ok(entries) = std::fs::read_dir(self.dir()) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if !name.to_string_lossy().contains('.') {
                    let _ = self.command().arg("kill").arg("-f").arg(&name).output();
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Ev {
    Output(Vec<u8>),
    ReplayEnd,
    Skipped,
    Exit(Exit),
    Ack,
    Error(String),
    Closed,
    /// Nothing arrived in time.
    Quiet,
}

pub struct Raw {
    pub stream: UnixStream,
    decoder: Decoder,
}

impl Raw {
    /// Connect and exchange greetings.
    pub fn connect(home: &Home, name: &str) -> Raw {
        let stream = UnixStream::connect(home.file(name)).expect("session socket");
        let mut raw = Raw {
            stream,
            decoder: Decoder::new(),
        };
        raw.send(ClientMsg::Hello { version: VERSION });
        raw
    }

    pub fn send(&mut self, msg: ClientMsg<'_>) {
        let mut bytes = Vec::new();
        msg.encode(&mut bytes);
        // A master that turned us away shows up as `Closed` on the next read.
        let _ = self.stream.write_all(&bytes);
    }

    /// The next message from the master, waiting at most `limit`.
    pub fn next(&mut self, limit: Duration) -> Ev {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(frame) = self.decoder.next().unwrap() {
                return match ServerMsg::decode(&frame).unwrap() {
                    // Only the terminal hold in the real client cares.
                    ServerMsg::HelloAck { .. } | ServerMsg::Settled => continue,
                    ServerMsg::Output(data) => Ev::Output(data.to_vec()),
                    ServerMsg::ReplayEnd => Ev::ReplayEnd,
                    ServerMsg::Skipped => Ev::Skipped,
                    ServerMsg::Exit(exit) => Ev::Exit(exit),
                    ServerMsg::Ack => Ev::Ack,
                    ServerMsg::Error(text) => Ev::Error(text.to_owned()),
                };
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ev::Quiet;
            }
            // Fails on macOS once the peer has closed; the read then says so.
            let _ = self.stream.set_read_timeout(Some(left));
            let mut buf = [0u8; 8192];
            match self.stream.read(&mut buf) {
                Ok(0) => return Ev::Closed,
                Ok(n) => self.decoder.extend(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ev::Quiet
                }
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return Ev::Closed,
                Err(e) => panic!("read: {e}"),
            }
        }
    }

    /// Collect output until `stop` matches an event or `limit` passes.
    /// Returns the output and the event that ended the wait.
    pub fn collect(&mut self, limit: Duration, stop: impl Fn(&Ev, &[u8]) -> bool) -> (Vec<u8>, Ev) {
        let deadline = Instant::now() + limit;
        let mut output = Vec::new();
        loop {
            let ev = self.next(deadline.saturating_duration_since(Instant::now()));
            if let Ev::Output(data) = &ev {
                output.extend_from_slice(data);
            }
            if stop(&ev, &output) || matches!(ev, Ev::Closed | Ev::Quiet) {
                return (output, ev);
            }
        }
    }
}

/// Wait up to `limit` for `check` to hold.
pub fn eventually(limit: Duration, check: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    check()
}

pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
