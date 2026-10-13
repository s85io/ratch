//! Layer 1: the messages a client and a master exchange.
//!
//! Both directions use the same frame: a four-byte header (type, flags,
//! payload length as little-endian u16) and up to [`MAX_PAYLOAD`] bytes.
//! Program output and keyboard input travel inside frames unchanged.
//!
//! A conversation opens with `Hello` from the client and `HelloAck` from the
//! master.

#![forbid(unsafe_code)]

use std::fmt;

pub const VERSION: u16 = 2;
pub const MAX_PAYLOAD: usize = 4096;
const HEADER: usize = 4;
const HELLO_MAGIC: [u8; 2] = *b"at";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtoError {
    UnknownType(u8),
    /// The payload does not fit the message type.
    Malformed(u8),
    TooLong(usize),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::UnknownType(t) => write!(f, "unknown message type {t}"),
            ProtoError::Malformed(t) => write!(f, "malformed message of type {t}"),
            ProtoError::TooLong(n) => write!(f, "message of {n} bytes is too long"),
        }
    }
}

impl std::error::Error for ProtoError {}

/// How the program should be made to repaint after an attach.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Redraw {
    /// Whatever the session was started with.
    #[default]
    Unspec = 0,
    None = 1,
    CtrlL = 2,
    Winch = 3,
}

impl Redraw {
    fn from_u8(v: u8) -> Option<Redraw> {
        Some(match v {
            0 => Redraw::Unspec,
            1 => Redraw::None,
            2 => Redraw::CtrlL,
            3 => Redraw::Winch,
            _ => return None,
        })
    }
}

/// How much history a newly attached client wants to see.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Replay {
    #[default]
    Default,
    None,
    Lines(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Attach {
    pub replay: Replay,
    pub rows: u16,
    pub cols: u16,
    /// Watch only: this client's input is not passed to the program.
    pub readonly: bool,
    /// Just the bytes: history whatever the program is doing now, and no
    /// attempt to set a terminal up for it. For reading output, not viewing.
    pub dump: bool,
    pub redraw: Redraw,
}

/// How a session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Code(u8),
    Signal(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientMsg<'a> {
    Hello {
        version: u16,
    },
    Attach(Attach),
    Detach,
    Input(&'a [u8]),
    Resize {
        rows: u16,
        cols: u16,
    },
    Redraw {
        method: Redraw,
        rows: u16,
        cols: u16,
    },
    Signal(u8),
    /// Forget the history, in memory and on disk.
    Clear,
    /// Take a new name.
    Rename(&'a [u8]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerMsg<'a> {
    HelloAck {
        version: u16,
        session: u64,
    },
    Output(&'a [u8]),
    /// History is over; what follows is live.
    ReplayEnd,
    /// This client fell too far behind and some output was dropped for it.
    Skipped,
    Exit(Exit),
    Ack,
    Error(&'a str),
}

mod kind {
    pub const HELLO: u8 = 1;
    pub const ATTACH: u8 = 2;
    pub const DETACH: u8 = 3;
    pub const INPUT: u8 = 4;
    pub const RESIZE: u8 = 5;
    pub const REDRAW: u8 = 6;
    pub const SIGNAL: u8 = 7;
    pub const CLEAR: u8 = 8;
    pub const RENAME: u8 = 9;

    pub const HELLO_ACK: u8 = 0x81;
    pub const OUTPUT: u8 = 0x82;
    pub const REPLAY_END: u8 = 0x83;
    pub const SKIPPED: u8 = 0x84;
    pub const EXIT: u8 = 0x85;
    pub const ACK: u8 = 0x86;
    pub const ERROR: u8 = 0x87;
}

const FLAG_READONLY: u8 = 1;
const FLAG_DUMP: u8 = 2;
const FLAG_SIGNALED: u8 = 1;

fn frame(out: &mut Vec<u8>, kind: u8, flags: u8, payload: &[u8]) {
    debug_assert!(payload.len() <= MAX_PAYLOAD);
    out.extend_from_slice(&[kind, flags]);
    out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    out.extend_from_slice(payload);
}

/// Bulk data is split across as many frames as it needs.
fn frames(out: &mut Vec<u8>, kind: u8, data: &[u8]) {
    for chunk in data.chunks(MAX_PAYLOAD) {
        frame(out, kind, 0, chunk);
    }
}

fn u16_at(p: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([p[at], p[at + 1]])
}

impl ClientMsg<'_> {
    pub fn encode(&self, out: &mut Vec<u8>) {
        match *self {
            ClientMsg::Hello { version } => {
                let v = version.to_le_bytes();
                frame(
                    out,
                    kind::HELLO,
                    0,
                    &[HELLO_MAGIC[0], HELLO_MAGIC[1], v[0], v[1]],
                );
            }
            ClientMsg::Attach(a) => {
                let replay: u32 = match a.replay {
                    Replay::Default => u32::MAX,
                    Replay::None => 0,
                    Replay::Lines(n) => n.clamp(1, u32::MAX - 1),
                };
                let mut p = [0u8; 9];
                p[..4].copy_from_slice(&replay.to_le_bytes());
                p[4..6].copy_from_slice(&a.rows.to_le_bytes());
                p[6..8].copy_from_slice(&a.cols.to_le_bytes());
                p[8] = a.redraw as u8;
                let flags = (u8::from(a.readonly) * FLAG_READONLY) | (u8::from(a.dump) * FLAG_DUMP);
                frame(out, kind::ATTACH, flags, &p);
            }
            ClientMsg::Detach => frame(out, kind::DETACH, 0, &[]),
            ClientMsg::Input(data) => frames(out, kind::INPUT, data),
            ClientMsg::Resize { rows, cols } => {
                let (r, c) = (rows.to_le_bytes(), cols.to_le_bytes());
                frame(out, kind::RESIZE, 0, &[r[0], r[1], c[0], c[1]]);
            }
            ClientMsg::Redraw { method, rows, cols } => {
                let (r, c) = (rows.to_le_bytes(), cols.to_le_bytes());
                frame(
                    out,
                    kind::REDRAW,
                    0,
                    &[r[0], r[1], c[0], c[1], method as u8],
                );
            }
            ClientMsg::Signal(sig) => frame(out, kind::SIGNAL, 0, &[sig]),
            ClientMsg::Clear => frame(out, kind::CLEAR, 0, &[]),
            ClientMsg::Rename(name) => frame(out, kind::RENAME, 0, &name[..name.len().min(255)]),
        }
    }

    pub fn decode<'a>(f: &Frame<'a>) -> Result<ClientMsg<'a>, ProtoError> {
        let p = f.payload;
        let bad = ProtoError::Malformed(f.kind);
        Ok(match (f.kind, p.len()) {
            (kind::HELLO, 4) if p[..2] == HELLO_MAGIC => ClientMsg::Hello {
                version: u16_at(p, 2),
            },
            (kind::ATTACH, 9) => ClientMsg::Attach(Attach {
                replay: match u32::from_le_bytes([p[0], p[1], p[2], p[3]]) {
                    u32::MAX => Replay::Default,
                    0 => Replay::None,
                    n => Replay::Lines(n),
                },
                rows: u16_at(p, 4),
                cols: u16_at(p, 6),
                readonly: f.flags & FLAG_READONLY != 0,
                dump: f.flags & FLAG_DUMP != 0,
                redraw: Redraw::from_u8(p[8]).ok_or(bad)?,
            }),
            (kind::DETACH, 0) => ClientMsg::Detach,
            (kind::INPUT, _) => ClientMsg::Input(p),
            (kind::RESIZE, 4) => ClientMsg::Resize {
                rows: u16_at(p, 0),
                cols: u16_at(p, 2),
            },
            (kind::REDRAW, 5) => ClientMsg::Redraw {
                method: Redraw::from_u8(p[4]).ok_or(bad)?,
                rows: u16_at(p, 0),
                cols: u16_at(p, 2),
            },
            (kind::SIGNAL, 1) => ClientMsg::Signal(p[0]),
            (kind::CLEAR, 0) => ClientMsg::Clear,
            (kind::RENAME, 1..=255) => ClientMsg::Rename(p),
            (kind::HELLO..=kind::RENAME, _) => return Err(bad),
            (other, _) => return Err(ProtoError::UnknownType(other)),
        })
    }
}

impl ServerMsg<'_> {
    pub fn encode(&self, out: &mut Vec<u8>) {
        match *self {
            ServerMsg::HelloAck { version, session } => {
                let mut p = [0u8; 10];
                p[..2].copy_from_slice(&version.to_le_bytes());
                p[2..].copy_from_slice(&session.to_le_bytes());
                frame(out, kind::HELLO_ACK, 0, &p);
            }
            ServerMsg::Output(data) => frames(out, kind::OUTPUT, data),
            ServerMsg::ReplayEnd => frame(out, kind::REPLAY_END, 0, &[]),
            ServerMsg::Skipped => frame(out, kind::SKIPPED, 0, &[]),
            ServerMsg::Exit(Exit::Code(code)) => frame(out, kind::EXIT, 0, &[code]),
            ServerMsg::Exit(Exit::Signal(sig)) => frame(out, kind::EXIT, FLAG_SIGNALED, &[sig]),
            ServerMsg::Ack => frame(out, kind::ACK, 0, &[]),
            ServerMsg::Error(text) => {
                let mut end = text.len().min(MAX_PAYLOAD);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                frame(out, kind::ERROR, 0, &text.as_bytes()[..end]);
            }
        }
    }

    pub fn decode<'a>(f: &Frame<'a>) -> Result<ServerMsg<'a>, ProtoError> {
        let p = f.payload;
        let bad = ProtoError::Malformed(f.kind);
        Ok(match (f.kind, p.len()) {
            (kind::HELLO_ACK, 10) => ServerMsg::HelloAck {
                version: u16_at(p, 0),
                session: u64::from_le_bytes([p[2], p[3], p[4], p[5], p[6], p[7], p[8], p[9]]),
            },
            (kind::OUTPUT, _) => ServerMsg::Output(p),
            (kind::REPLAY_END, 0) => ServerMsg::ReplayEnd,
            (kind::SKIPPED, 0) => ServerMsg::Skipped,
            (kind::EXIT, 1) if f.flags & FLAG_SIGNALED != 0 => ServerMsg::Exit(Exit::Signal(p[0])),
            (kind::EXIT, 1) => ServerMsg::Exit(Exit::Code(p[0])),
            (kind::ACK, 0) => ServerMsg::Ack,
            (kind::ERROR, _) => ServerMsg::Error(std::str::from_utf8(p).map_err(|_| bad)?),
            (kind::HELLO_ACK..=kind::ERROR, _) => return Err(bad),
            (other, _) => return Err(ProtoError::UnknownType(other)),
        })
    }
}

/// One frame, borrowed from the decoder that holds its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    pub kind: u8,
    pub flags: u8,
    pub payload: &'a [u8],
}

/// Reassembles frames from a byte stream that may deliver them in any pieces.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
    /// Bytes at the front of `buf` already handed out as frames.
    done: usize,
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder::default()
    }

    pub fn extend(&mut self, bytes: &[u8]) {
        if self.done > 0 {
            self.buf.drain(..self.done);
            self.done = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Bytes received and not yet returned as a frame.
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.done
    }

    /// The next complete frame, if one has arrived.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Frame<'_>>, ProtoError> {
        let rest = &self.buf[self.done..];
        if rest.len() < HEADER {
            return Ok(None);
        }
        let len = usize::from(u16_at(rest, 2));
        if len > MAX_PAYLOAD {
            return Err(ProtoError::TooLong(len));
        }
        if rest.len() < HEADER + len {
            return Ok(None);
        }
        let start = self.done;
        self.done += HEADER + len;
        let frame = &self.buf[start..self.done];
        Ok(Some(Frame {
            kind: frame[0],
            flags: frame[1],
            payload: &frame[HEADER..],
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_samples() -> Vec<ClientMsg<'static>> {
        vec![
            ClientMsg::Hello { version: VERSION },
            ClientMsg::Attach(Attach::default()),
            ClientMsg::Attach(Attach {
                replay: Replay::Lines(100),
                rows: 50,
                cols: 200,
                readonly: true,
                dump: true,
                redraw: Redraw::CtrlL,
            }),
            ClientMsg::Attach(Attach {
                replay: Replay::None,
                ..Attach::default()
            }),
            ClientMsg::Detach,
            ClientMsg::Input(b"ls -la\r"),
            ClientMsg::Input(b""),
            ClientMsg::Resize { rows: 24, cols: 80 },
            ClientMsg::Redraw {
                method: Redraw::Winch,
                rows: 1,
                cols: 65535,
            },
            ClientMsg::Signal(15),
            ClientMsg::Clear,
            ClientMsg::Rename(b"new-name"),
        ]
    }

    fn server_samples() -> Vec<ServerMsg<'static>> {
        vec![
            ServerMsg::HelloAck {
                version: VERSION,
                session: 0x0123_4567_89ab_cdef,
            },
            ServerMsg::Output(b"\x1b[1mhello\x1b[m\r\n"),
            ServerMsg::ReplayEnd,
            ServerMsg::Skipped,
            ServerMsg::Exit(Exit::Code(0)),
            ServerMsg::Exit(Exit::Code(127)),
            ServerMsg::Exit(Exit::Signal(9)),
            ServerMsg::Ack,
            ServerMsg::Error("session is full"),
        ]
    }

    #[test]
    fn client_messages_round_trip() {
        for msg in client_samples() {
            let mut bytes = Vec::new();
            msg.encode(&mut bytes);
            let mut decoder = Decoder::new();
            decoder.extend(&bytes);
            if msg == ClientMsg::Input(b"") {
                assert_eq!(decoder.next().unwrap(), None, "empty input sends nothing");
                continue;
            }
            let frame = decoder.next().unwrap().unwrap();
            assert_eq!(ClientMsg::decode(&frame).unwrap(), msg);
            assert_eq!(decoder.next().unwrap(), None);
        }
    }

    #[test]
    fn server_messages_round_trip() {
        for msg in server_samples() {
            let mut bytes = Vec::new();
            msg.encode(&mut bytes);
            let mut decoder = Decoder::new();
            decoder.extend(&bytes);
            let frame = decoder.next().unwrap().unwrap();
            assert_eq!(ServerMsg::decode(&frame).unwrap(), msg);
        }
    }

    #[test]
    fn a_stream_delivered_one_byte_at_a_time_decodes_the_same() {
        let mut stream = Vec::new();
        for msg in client_samples() {
            msg.encode(&mut stream);
        }
        let expected: Vec<_> = client_samples()
            .into_iter()
            .filter(|m| *m != ClientMsg::Input(b""))
            .collect();
        let mut decoder = Decoder::new();
        let mut got = 0;
        for byte in &stream {
            decoder.extend(std::slice::from_ref(byte));
            while let Some(frame) = decoder.next().unwrap() {
                assert_eq!(ClientMsg::decode(&frame).unwrap(), expected[got]);
                got += 1;
            }
        }
        assert_eq!(got, expected.len());
        assert_eq!(decoder.buffered(), 0);
    }

    #[test]
    fn bulk_data_is_split_and_reassembles() {
        let data: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        let mut bytes = Vec::new();
        ServerMsg::Output(&data).encode(&mut bytes);
        let mut decoder = Decoder::new();
        decoder.extend(&bytes);
        let mut back = Vec::new();
        let mut count = 0;
        while let Some(frame) = decoder.next().unwrap() {
            match ServerMsg::decode(&frame).unwrap() {
                ServerMsg::Output(chunk) => back.extend_from_slice(chunk),
                other => panic!("unexpected {other:?}"),
            }
            count += 1;
        }
        assert_eq!(back, data);
        assert_eq!(count, 3);
    }

    #[test]
    fn bad_frames_are_errors_not_panics() {
        let cases: &[(&[u8], ProtoError)] = &[
            (&[99, 0, 0, 0], ProtoError::UnknownType(99)),
            (
                &[kind::RESIZE, 0, 1, 0, 7],
                ProtoError::Malformed(kind::RESIZE),
            ),
            (
                &[kind::HELLO, 0, 4, 0, b'x', b'x', 2, 0],
                ProtoError::Malformed(kind::HELLO),
            ),
            (
                &[kind::REDRAW, 0, 5, 0, 1, 0, 1, 0, 9],
                ProtoError::Malformed(kind::REDRAW),
            ),
            (
                &[kind::DETACH, 0, 1, 0, 7],
                ProtoError::Malformed(kind::DETACH),
            ),
        ];
        for (bytes, expected) in cases {
            let mut decoder = Decoder::new();
            decoder.extend(bytes);
            let frame = decoder.next().unwrap().unwrap();
            assert_eq!(ClientMsg::decode(&frame).unwrap_err(), *expected);
        }
        let mut decoder = Decoder::new();
        decoder.extend(&[kind::INPUT, 0, 0xff, 0xff]);
        assert_eq!(decoder.next().unwrap_err(), ProtoError::TooLong(0xffff));
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..2000 {
            let mut decoder = Decoder::new();
            let len = (state % 64) as usize;
            let bytes: Vec<u8> = (0..len)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    (state >> 32) as u8
                })
                .collect();
            decoder.extend(&bytes);
            while let Ok(Some(frame)) = decoder.next() {
                let _ = ClientMsg::decode(&frame);
                let _ = ServerMsg::decode(&frame);
            }
        }
    }
}
