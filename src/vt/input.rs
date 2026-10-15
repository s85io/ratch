//! What a terminal sends: keys, and reports nobody typed.
//!
//! A program can ask the terminal to encode keys in a richer way (the kitty
//! keyboard protocol, xterm's modifyOtherKeys). Then Ctrl-\ no longer
//! arrives as the byte 0x1c but as `ESC [ 92 ; 5 u`, and a key of ours must
//! be recognised in every form.

#![forbid(unsafe_code)]

use std::ops::Range;

use super::parser::{Parser, Token};

const SHIFT: u16 = 1;
const CTRL: u16 = 4;
/// Caps Lock and Num Lock, which say nothing about the key.
const LOCKS: u16 = 64 | 128;

/// The key codes that, held with Ctrl and maybe Shift, type `ctl`.
fn key_codes(ctl: u8) -> (&'static [u8], bool) {
    const LETTERS: &[u8; 26] = b"abcdefghijklmnopqrstuvwxyz";
    match ctl {
        0x01..=0x1a => {
            let i = usize::from(ctl - 1);
            (&LETTERS[i..=i], false)
        }
        0x00 => (b" @2", true),
        0x1b => (b"[", true),
        0x1c => (b"\\4", true),
        0x1d => (b"]5", true),
        0x1e => (b"^6", true),
        0x1f => (b"_-7/", true),
        _ => (&[], false),
    }
}

/// Fields of a `CSI ... u` or `CSI ... ~` key report: each `;`-separated
/// field as its `:`-separated numbers.
fn fields(raw: &[u8]) -> Option<Vec<Vec<u16>>> {
    let body = raw.strip_prefix(b"\x1b[")?;
    let body = &body[..body.len().checked_sub(1)?];
    body.split(|&b| b == b';')
        .map(|field| {
            field
                .split(|&b| b == b':')
                .map(|n| {
                    if n.is_empty() {
                        Some(0)
                    } else if n.iter().all(u8::is_ascii_digit) && n.len() <= 5 {
                        std::str::from_utf8(n)
                            .ok()?
                            .parse::<u32>()
                            .ok()?
                            .try_into()
                            .ok()
                    } else {
                        None
                    }
                })
                .collect()
        })
        .collect()
}

/// Does `keys` begin with a press of the key that types `key`?
pub fn starts_with_key(keys: &[u8], key: u8) -> bool {
    if keys.first() == Some(&key) {
        return true;
    }
    if keys.first() != Some(&0x1b) {
        return false;
    }
    let mut first = None;
    Parser::new().feed(keys, |token| {
        if first.is_none() {
            first = Some(match token {
                Token::Csi(csi) if csi.private == 0 && csi.inter == 0 => (csi.fin, fields(csi.raw)),
                _ => (0, None),
            });
        }
    });
    let Some((fin, Some(fields))) = first else {
        return false;
    };
    // The code, the modifiers, and for kitty whether pressed (1, or not
    // given), repeated (2) or released (3).
    let (code, mods, event) = match (fin, fields.as_slice()) {
        (b'u', [code, rest @ ..]) => {
            let mods = rest.first();
            (
                code.first().copied(),
                mods.and_then(|m| m.first().copied()).unwrap_or(1),
                mods.and_then(|m| m.get(1).copied()).unwrap_or(1),
            )
        }
        (b'~', [prefix, mods, code]) if prefix.first() == Some(&27) => {
            (code.first().copied(), mods.first().copied().unwrap_or(1), 1)
        }
        _ => return false,
    };
    let (Some(code), true) = (code, event <= 1) else {
        return false;
    };
    let held = mods.saturating_sub(1) & !LOCKS;
    if key >= 0x20 {
        return code == u16::from(key) && (held == 0 || held == SHIFT);
    }
    let (codes, shift_ok) = key_codes(key);
    codes.iter().any(|&c| u16::from(c) == code)
        && (held == CTRL || shift_ok && held == CTRL | SHIFT)
}

/// What a piece of keyboard input amounts to.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Scan {
    /// Someone is at this terminal: anything but the reports below.
    pub active: bool,
    /// Where the terminal reported its size in-band (mode 2048).
    pub size_reports: Vec<Range<usize>>,
}

/// Look through keyboard input for what a person did. A terminal also sends
/// things on its own: its new size, and that it lost focus (a phone's
/// terminal going into the background, say). Those do not count; gaining
/// focus does, since it is what clicking on a window does.
pub fn scan(input: &[u8]) -> Scan {
    let mut scan = Scan::default();
    let mut parser = Parser::new();
    let mut at = 0;
    parser.feed(input, |token| {
        let len = token.raw().len();
        match token {
            Token::Csi(csi)
                if (csi.private, csi.inter, csi.fin) == (0, 0, b't')
                    && csi.param(0) == Some(48) =>
            {
                scan.size_reports.push(at..at + len);
            }
            Token::Csi(csi) if csi.raw == b"\x1b[O" => {}
            _ => scan.active = true,
        }
        at += len;
    });
    scan.active |= parser.pending() > 0;
    scan
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_detach_key_in_every_encoding() {
        for keys in [
            &b"\x1c"[..],
            b"\x1c more",
            b"\x1b[92;5u",
            b"\x1b[92;5:1u",
            b"\x1b[92;69u",
            b"\x1b[92;133u",
            b"\x1b[92:124;5u",
            b"\x1b[92;5;28u",
            b"\x1b[27;5;92~",
        ] {
            assert!(
                starts_with_key(keys, 0x1c),
                "{:?}",
                String::from_utf8_lossy(keys)
            );
        }
        for keys in [
            &b""[..],
            b"x\x1c",
            b"\x1b[92u",
            b"\x1b[92;5:2u",
            b"\x1b[92;5:3u",
            b"\x1b[92;7u",
            b"\x1b[93;5u",
            b"\x1b[92;5~",
            b"\x1b[27;5;93~",
            b"\x1b",
            b"\x1b[92;5",
            b"\x1b[?92;5u",
        ] {
            assert!(
                !starts_with_key(keys, 0x1c),
                "{:?}",
                String::from_utf8_lossy(keys)
            );
        }
    }

    #[test]
    fn letters_and_other_keys() {
        assert!(starts_with_key(b"\x1b[97;5u", 0x01));
        assert!(starts_with_key(b"\x1b[122;5u", 0x1a));
        // Ctrl-Shift-A is a different key from Ctrl-A.
        assert!(!starts_with_key(b"\x1b[97;6u", 0x01));
        assert!(starts_with_key(b"\x1b[95;6u", 0x1f));
        assert!(starts_with_key(b"\x1b[120u", b'x'));
        assert!(starts_with_key(b"x", b'x'));
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn reports_are_not_a_person() {
        assert_eq!(
            scan(b"\x1b[48;24;80;480;640t\x1b[O"),
            Scan {
                active: false,
                size_reports: vec![0..19],
            }
        );
        assert!(scan(b"\x1b[I").active);
        assert!(scan(b"a").active);
        assert!(scan(b"\x1b").active);
        assert!(scan(b"\x1b[<35;10;5M").active);
        let mixed = b"k\x1b[48;24;80;480;640t";
        assert_eq!(
            scan(mixed),
            Scan {
                active: true,
                size_reports: vec![1..20],
            }
        );
        assert_eq!(scan(b""), Scan::default());
    }
}
