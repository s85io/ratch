//! Removes the sequences a terminal answers.
//!
//! Recorded output contains the questions a program once asked its terminal:
//! what are you, where is the cursor, what colour is the background. Played
//! back, each one makes the terminal answer again, and the answer arrives as
//! keyboard input for whatever is running now. Everything else passes through
//! byte for byte.
//!
//! The list is of known queries. A terminal with a private query of its own
//! will still answer it; the client covers that by discarding input while
//! history is being played.

#![forbid(unsafe_code)]

use super::parser::{Parser, StrKind, Token};

/// Does a terminal reply to this?
pub fn is_query(token: &Token<'_>) -> bool {
    match *token {
        Token::Enq => true,
        // DECID, the old form of "what are you".
        Token::Esc {
            inter: [],
            fin: b'Z',
            ..
        } => true,
        Token::Csi(csi) => match (csi.private, csi.inter, csi.fin) {
            // Device attributes: primary, secondary, tertiary.
            (0 | b'>' | b'=', 0, b'c') => true,
            // Device status reports, including cursor position.
            (0 | b'?', 0, b'n') => true,
            // Mode, presentation state and terminal state requests.
            (0 | b'?', b'$', b'p' | b'w' | b'u') => true,
            // Kitty keyboard flags, key modifier options, graphics attributes.
            (b'?', 0, b'u' | b'm' | b'S') => true,
            // XTVERSION.
            (b'>', 0, b'q') => true,
            // Terminal parameters.
            (0, 0, b'x') => true,
            // Checksum of a rectangle.
            (0, b'*', b'y') => true,
            // Window reports: state, position, size, title.
            (0, 0, b't') => matches!(csi.param(0), Some(11 | 13..=16 | 18..=21)),
            _ => false,
        },
        Token::Str { kind, payload, .. } => match kind {
            // Colour, clipboard and similar: a `?` where a value would go.
            StrKind::Osc => payload
                .split(|&b| b == b';')
                .skip(1)
                .any(|field| field == b"?"),
            // DECRQSS and XTGETTCAP.
            StrKind::Dcs => payload.starts_with(b"$q") || payload.starts_with(b"+q"),
            // Kitty graphics protocol query.
            StrKind::Apc => {
                payload.first() == Some(&b'G')
                    && payload[1..]
                        .split(|&b| b == b';')
                        .next()
                        .is_some_and(|keys| keys.split(|&b| b == b',').any(|k| k == b"a=q"))
            }
            StrKind::Pm | StrKind::Sos => false,
        },
        Token::Text(_) | Token::Esc { .. } | Token::Opaque(_) => false,
    }
}

#[derive(Debug, Default)]
pub struct Filter {
    parser: Parser,
}

impl Filter {
    pub fn new() -> Filter {
        Filter::default()
    }

    /// Append `input`, minus queries, to `out`.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) {
        self.parser.feed(input, |token| {
            if !is_query(&token) {
                out.extend_from_slice(token.raw());
            }
        });
    }

    /// The filtered stretch ends here; release any partial sequence as is, so
    /// that the unfiltered bytes that follow complete it.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        self.parser
            .flush(|token| out.extend_from_slice(token.raw()));
    }
}

#[cfg(test)]
mod tests {
    use super::super::parser::tests::{noise, SAMPLE};
    use super::*;

    fn filter(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut filter = Filter::new();
        filter.feed(input, &mut out);
        filter.finish(&mut out);
        out
    }

    #[test]
    fn queries_are_removed() {
        let queries: &[&[u8]] = &[
            b"\x05",
            b"\x1bZ",
            b"\x1b[c",
            b"\x1b[0c",
            b"\x1b[>c",
            b"\x1b[>0c",
            b"\x1b[=c",
            b"\x1b[5n",
            b"\x1b[6n",
            b"\x1b[?6n",
            b"\x1b[?996n",
            b"\x1b[?2026$p",
            b"\x1b[4$p",
            b"\x1b[1$w",
            b"\x1b[?1$u",
            b"\x1b[?u",
            b"\x1b[?4m",
            b"\x1b[?1;1;0S",
            b"\x1b[>q",
            b"\x1b[>0q",
            b"\x1b[x",
            b"\x1b[1;1;1;1;1;1*y",
            b"\x1b[14t",
            b"\x1b[18t",
            b"\x1b[21t",
            b"\x1b]10;?\x07",
            b"\x1b]11;?\x1b\\",
            b"\x1b]4;1;?\x07",
            b"\x1b]52;c;?\x07",
            b"\x1bP$qm\x1b\\",
            b"\x1bP+q544e\x1b\\",
            b"\x1b_Gi=1,a=q;AAAA\x1b\\",
        ];
        for query in queries {
            assert_eq!(filter(query), b"", "{:?}", String::from_utf8_lossy(query));
            let mut wrapped = b"before".to_vec();
            wrapped.extend_from_slice(query);
            wrapped.extend_from_slice(b"after");
            assert_eq!(filter(&wrapped), b"beforeafter");
        }
    }

    #[test]
    fn everything_else_is_untouched() {
        let kept: &[&[u8]] = &[
            b"plain \xe2\x82\xac text\r\n\t\x08",
            b"\x1b[1;31m\x1b[0m\x1b[H\x1b[2J\x1b[10;20H",
            b"\x1b[?1049h\x1b[?25l\x1b[?1000;1006h",
            b"\x1b[>1u\x1b[<u\x1b[=1;1u",
            b"\x1b[>4;2m\x1b[2 q\x1b[!p\x1b[r\x1b[s\x1b[u",
            b"\x1b[8;24;80t\x1b[22;0t\x1b[23;0t",
            b"\x1b]0;window title\x07\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\",
            b"\x1b]10;#ffffff\x07\x1b]52;c;aGVsbG8=\x07\x1b]4;1;rgb:00/00/00\x07",
            b"\x1b]0;what?\x07",
            b"\x1bPq#0;2;0;0;0#0~~\x1b\\",
            b"\x1b_Ga=T,f=100;AAAA\x1b\\",
            b"\x1b=\x1b>\x1b(B\x1b7\x1b8\x1bM\x1bc",
            b"\x1b[1\x18\x1b[",
        ];
        for bytes in kept {
            assert_eq!(
                filter(bytes),
                *bytes,
                "{:?}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    #[test]
    fn cutting_the_input_anywhere_changes_nothing() {
        let whole = filter(SAMPLE);
        assert!(whole.len() < SAMPLE.len());
        for cut in 1..SAMPLE.len() {
            let mut out = Vec::new();
            let mut filter = Filter::new();
            filter.feed(&SAMPLE[..cut], &mut out);
            filter.feed(&SAMPLE[cut..], &mut out);
            filter.finish(&mut out);
            assert_eq!(out, whole, "cut at {cut}");
        }
    }

    #[test]
    fn stopping_mid_sequence_releases_it() {
        let mut out = Vec::new();
        let mut filter = Filter::new();
        filter.feed(b"abc\x1b[?10", &mut out);
        assert_eq!(out, b"abc");
        filter.finish(&mut out);
        assert_eq!(out, b"abc\x1b[?10");
    }

    #[test]
    fn output_is_always_the_input_with_pieces_removed() {
        for seed in 0..300 {
            let input = noise(seed, 400);
            let output = filter(&input);
            // Subsequence check: nothing invented, nothing reordered.
            let mut rest = input.iter();
            for byte in &output {
                assert!(
                    rest.any(|b| b == byte),
                    "seed {seed}: output is not a subsequence"
                );
            }
            // Filtering what is already filtered removes nothing more than
            // what new adjacency can create; it must at least not grow.
            assert!(filter(&output).len() <= output.len());
        }
    }
}
