//! Splits a terminal byte stream into tokens.
//!
//! This is a tokenizer, not a terminal: it knows where escape sequences start
//! and end and nothing about what they do. Its contract is that the raw bytes
//! of the tokens it emits, concatenated, are exactly the bytes it was fed
//! (minus whatever is still buffered waiting for the rest of a sequence).

#![forbid(unsafe_code)]

/// Longest sequence buffered whole. Anything longer (a sixel image, a big
/// clipboard write) is passed through as opaque bytes.
const SEQ_MAX: usize = 256;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const ENQ: u8 = 0x05;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

/// The control strings that run until a string terminator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrKind {
    Osc,
    Dcs,
    Apc,
    Pm,
    Sos,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token<'a> {
    /// Printable text and C0 controls other than ESC and ENQ.
    Text(&'a [u8]),
    /// The ENQ control, which asks the terminal for its answerback.
    Enq,
    /// `ESC` + intermediates + final byte.
    Esc {
        raw: &'a [u8],
        inter: &'a [u8],
        fin: u8,
    },
    /// `ESC [` + parameters + intermediates + final byte.
    Csi(Csi<'a>),
    /// `ESC ]`, `ESC P`, `ESC _`, `ESC ^` or `ESC X` up to its terminator.
    Str {
        raw: &'a [u8],
        kind: StrKind,
        payload: &'a [u8],
    },
    /// Bytes of an aborted, malformed or over-long sequence, passed through.
    Opaque(&'a [u8]),
}

impl<'a> Token<'a> {
    pub fn raw(&self) -> &'a [u8] {
        match *self {
            Token::Text(raw) | Token::Opaque(raw) => raw,
            Token::Enq => &[ENQ],
            Token::Esc { raw, .. } | Token::Str { raw, .. } => raw,
            Token::Csi(csi) => csi.raw,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Csi<'a> {
    pub raw: &'a [u8],
    /// The private marker (`?`, `>`, `=` or `<`), or 0.
    pub private: u8,
    /// The intermediate byte (such as `$` or space), or 0.
    pub inter: u8,
    pub fin: u8,
    params: &'a [u8],
}

impl<'a> Csi<'a> {
    fn parse(raw: &'a [u8]) -> Csi<'a> {
        let fin = raw[raw.len() - 1];
        let mut body = &raw[2..raw.len() - 1];
        let mut private = 0;
        if let Some(&first) = body.first() {
            if (0x3c..=0x3f).contains(&first) {
                private = first;
                body = &body[1..];
            }
        }
        let split = body
            .iter()
            .position(|b| !(0x30..=0x3b).contains(b))
            .unwrap_or(body.len());
        let (params, inter) = body.split_at(split);
        Csi {
            raw,
            private,
            inter: inter.last().copied().unwrap_or(0),
            fin,
            params,
        }
    }

    /// Numeric parameters; an omitted one reads as 0. Sub-parameters
    /// (separated by `:`) are flattened into the same list.
    pub fn params(&self) -> impl Iterator<Item = u16> + 'a {
        let params = self.params;
        let empty = params.is_empty();
        params
            .split(|&b| b == b';' || b == b':')
            .filter(move |_| !empty)
            .map(|digits| {
                digits.iter().fold(0u16, |n, &d| {
                    n.saturating_mul(10).saturating_add(u16::from(d - b'0'))
                })
            })
    }

    pub fn param(&self, index: usize) -> Option<u16> {
        self.params().nth(index)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    EscInter,
    Csi,
    Str(StrKind),
    /// Inside a string, just saw ESC: a `\` ends it.
    StrEsc(StrKind),
    /// Inside a string too long to buffer.
    Pass,
    PassEsc,
}

#[derive(Debug)]
pub struct Parser {
    state: State,
    seq: Vec<u8>,
    /// Continuation bytes still owed by the UTF-8 character in progress.
    utf8: u8,
}

impl Default for Parser {
    fn default() -> Self {
        Parser::new()
    }
}

impl Parser {
    pub fn new() -> Parser {
        Parser {
            state: State::Ground,
            seq: Vec::with_capacity(SEQ_MAX),
            utf8: 0,
        }
    }

    /// True between sequences and between characters: a point where the
    /// stream can be cut, or picked up, without confusing a terminal.
    pub fn is_ground(&self) -> bool {
        self.state == State::Ground && self.utf8 == 0
    }

    /// Bytes consumed but not yet emitted as a token.
    pub fn pending(&self) -> usize {
        match self.state {
            State::PassEsc => 1,
            _ => self.seq.len(),
        }
    }

    pub fn feed<'a>(&'a mut self, input: &'a [u8], mut sink: impl FnMut(Token<'_>)) {
        let mut i = 0;
        while i < input.len() {
            let b = input[i];
            match self.state {
                State::Ground => {
                    let end = input[i..]
                        .iter()
                        .position(|&b| b == ESC || b == ENQ)
                        .map_or(input.len(), |n| i + n);
                    if end > i {
                        self.track_utf8(&input[i..end]);
                        sink(Token::Text(&input[i..end]));
                        i = end;
                        continue;
                    }
                    self.utf8 = 0;
                    if b == ENQ {
                        sink(Token::Enq);
                    } else {
                        self.seq.push(ESC);
                        self.state = State::Esc;
                    }
                    i += 1;
                }
                State::Esc => match b {
                    b'[' => self.enter(b, State::Csi, &mut i),
                    b']' => self.enter(b, State::Str(StrKind::Osc), &mut i),
                    b'P' => self.enter(b, State::Str(StrKind::Dcs), &mut i),
                    b'_' => self.enter(b, State::Str(StrKind::Apc), &mut i),
                    b'^' => self.enter(b, State::Str(StrKind::Pm), &mut i),
                    b'X' => self.enter(b, State::Str(StrKind::Sos), &mut i),
                    0x20..=0x2f => self.enter(b, State::EscInter, &mut i),
                    0x30..=0x7e => {
                        self.seq.push(b);
                        self.emit_esc(&mut sink);
                        i += 1;
                    }
                    _ => self.abort(b, &mut i, &mut sink),
                },
                State::EscInter => match b {
                    0x20..=0x2f if self.seq.len() < SEQ_MAX => {
                        self.seq.push(b);
                        i += 1;
                    }
                    0x30..=0x7e => {
                        self.seq.push(b);
                        self.emit_esc(&mut sink);
                        i += 1;
                    }
                    _ => self.abort(b, &mut i, &mut sink),
                },
                State::Csi => match b {
                    0x20..=0x3f if self.seq.len() < SEQ_MAX => {
                        self.seq.push(b);
                        i += 1;
                    }
                    0x40..=0x7e => {
                        self.seq.push(b);
                        sink(Token::Csi(Csi::parse(&self.seq)));
                        self.reset();
                        i += 1;
                    }
                    _ => self.abort(b, &mut i, &mut sink),
                },
                State::Str(kind) => match b {
                    BEL => {
                        self.seq.push(b);
                        let payload = &self.seq[2..self.seq.len() - 1];
                        sink(Token::Str {
                            raw: &self.seq,
                            kind,
                            payload,
                        });
                        self.reset();
                        i += 1;
                    }
                    ESC => {
                        self.seq.push(b);
                        self.state = State::StrEsc(kind);
                        i += 1;
                    }
                    CAN | SUB => self.abort(b, &mut i, &mut sink),
                    _ if self.seq.len() + 2 >= SEQ_MAX => {
                        sink(Token::Opaque(&self.seq));
                        self.seq.clear();
                        self.state = State::Pass;
                    }
                    _ => {
                        self.seq.push(b);
                        i += 1;
                    }
                },
                State::StrEsc(kind) => {
                    if b == b'\\' {
                        self.seq.push(b);
                        let payload = &self.seq[2..self.seq.len() - 2];
                        sink(Token::Str {
                            raw: &self.seq,
                            kind,
                            payload,
                        });
                        self.reset();
                        i += 1;
                    } else {
                        // The ESC starts a new sequence and cuts the string short.
                        self.seq.pop();
                        sink(Token::Opaque(&self.seq));
                        self.seq.clear();
                        self.seq.push(ESC);
                        self.state = State::Esc;
                    }
                }
                State::Pass => {
                    let end = input[i..]
                        .iter()
                        .position(|&b| matches!(b, BEL | ESC | CAN | SUB))
                        .map_or(input.len(), |n| i + n);
                    if end == input.len() {
                        sink(Token::Opaque(&input[i..]));
                        i = end;
                    } else if input[end] == ESC {
                        if end > i {
                            sink(Token::Opaque(&input[i..end]));
                        }
                        self.state = State::PassEsc;
                        i = end + 1;
                    } else {
                        sink(Token::Opaque(&input[i..=end]));
                        self.state = State::Ground;
                        i = end + 1;
                    }
                }
                State::PassEsc => {
                    if b == b'\\' {
                        sink(Token::Opaque(b"\x1b\\"));
                        self.state = State::Ground;
                        i += 1;
                    } else {
                        self.seq.push(ESC);
                        self.state = State::Esc;
                    }
                }
            }
        }
    }

    /// Give up on whatever sequence is in progress: emit the bytes held so
    /// far as they came and return to the ground state. For a reader that
    /// stops interpreting the stream here and lets the rest through raw.
    pub fn flush(&mut self, mut sink: impl FnMut(Token<'_>)) {
        if self.state == State::PassEsc {
            sink(Token::Opaque(&[ESC]));
        } else if !self.seq.is_empty() {
            sink(Token::Opaque(&self.seq));
        }
        self.reset();
        self.utf8 = 0;
    }

    fn enter(&mut self, b: u8, state: State, i: &mut usize) {
        self.seq.push(b);
        self.state = state;
        *i += 1;
    }

    fn reset(&mut self) {
        self.seq.clear();
        self.state = State::Ground;
    }

    fn emit_esc(&mut self, sink: &mut impl FnMut(Token<'_>)) {
        let last = self.seq.len() - 1;
        sink(Token::Esc {
            raw: &self.seq,
            inter: &self.seq[1..last],
            fin: self.seq[last],
        });
        self.reset();
    }

    /// The sequence in progress ends here without a valid final byte.
    /// CAN and SUB belong to it; an ESC starts the next one; anything else
    /// is handed back to be read again as ordinary input.
    fn abort(&mut self, b: u8, i: &mut usize, sink: &mut impl FnMut(Token<'_>)) {
        if b == CAN || b == SUB {
            self.seq.push(b);
            *i += 1;
        }
        sink(Token::Opaque(&self.seq));
        self.reset();
        if b == ESC {
            self.seq.push(ESC);
            self.state = State::Esc;
            *i += 1;
        }
    }

    fn track_utf8(&mut self, text: &[u8]) {
        for &b in text {
            if self.utf8 > 0 && b & 0xc0 == 0x80 {
                self.utf8 -= 1;
                continue;
            }
            self.utf8 = match b {
                0xc0..=0xdf => 1,
                0xe0..=0xef => 2,
                0xf0..=0xf7 => 3,
                _ => 0,
            };
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// What a token looked like, detached from the parser's buffers.
    #[derive(Debug, PartialEq, Eq, Clone)]
    pub(crate) enum Seen {
        Text(Vec<u8>),
        Enq,
        Esc(Vec<u8>, u8),
        Csi {
            private: u8,
            params: Vec<u16>,
            inter: u8,
            fin: u8,
        },
        Str(StrKind, Vec<u8>),
        Opaque(Vec<u8>),
    }

    /// Feed `input` in pieces cut at `cuts`; return the tokens, with adjacent
    /// text and opaque runs merged, and the concatenated raw bytes.
    pub(crate) fn run(input: &[u8], cuts: &[usize]) -> (Vec<Seen>, Vec<u8>, Parser) {
        let mut parser = Parser::new();
        let mut seen: Vec<Seen> = Vec::new();
        let mut raw = Vec::new();
        let mut start = 0;
        let mut pieces: Vec<&[u8]> = Vec::new();
        for &cut in cuts {
            pieces.push(&input[start..cut]);
            start = cut;
        }
        pieces.push(&input[start..]);
        for piece in pieces {
            parser.feed(piece, |token| {
                raw.extend_from_slice(token.raw());
                let next = match token {
                    Token::Text(t) => Seen::Text(t.to_vec()),
                    Token::Enq => Seen::Enq,
                    Token::Esc { inter, fin, .. } => Seen::Esc(inter.to_vec(), fin),
                    Token::Csi(c) => Seen::Csi {
                        private: c.private,
                        params: c.params().collect(),
                        inter: c.inter,
                        fin: c.fin,
                    },
                    Token::Str { kind, payload, .. } => Seen::Str(kind, payload.to_vec()),
                    Token::Opaque(o) => Seen::Opaque(o.to_vec()),
                };
                match (seen.last_mut(), &next) {
                    (Some(Seen::Text(a)), Seen::Text(b)) => a.extend_from_slice(b),
                    (Some(Seen::Opaque(a)), Seen::Opaque(b)) => a.extend_from_slice(b),
                    _ => seen.push(next),
                }
            });
        }
        (seen, raw, parser)
    }

    fn tokens(input: &[u8]) -> Vec<Seen> {
        run(input, &[]).0
    }

    fn csi(private: u8, params: &[u16], inter: u8, fin: u8) -> Seen {
        Seen::Csi {
            private,
            params: params.to_vec(),
            inter,
            fin,
        }
    }

    #[test]
    fn plain_text_is_one_token() {
        assert_eq!(tokens(b"hello\r\n"), [Seen::Text(b"hello\r\n".to_vec())]);
    }

    #[test]
    fn csi_forms() {
        assert_eq!(tokens(b"\x1b[?1049h"), [csi(b'?', &[1049], 0, b'h')]);
        assert_eq!(tokens(b"\x1b[1;31m"), [csi(0, &[1, 31], 0, b'm')]);
        assert_eq!(tokens(b"\x1b[m"), [csi(0, &[], 0, b'm')]);
        assert_eq!(tokens(b"\x1b[;5H"), [csi(0, &[0, 5], 0, b'H')]);
        assert_eq!(tokens(b"\x1b[2 q"), [csi(0, &[2], b' ', b'q')]);
        assert_eq!(tokens(b"\x1b[?2026$p"), [csi(b'?', &[2026], b'$', b'p')]);
        assert_eq!(tokens(b"\x1b[>1u"), [csi(b'>', &[1], 0, b'u')]);
        assert_eq!(
            tokens(b"\x1b[38:2:1:2:3m"),
            [csi(0, &[38, 2, 1, 2, 3], 0, b'm')]
        );
    }

    #[test]
    fn esc_forms() {
        assert_eq!(tokens(b"\x1b="), [Seen::Esc(vec![], b'=')]);
        assert_eq!(tokens(b"\x1bc"), [Seen::Esc(vec![], b'c')]);
        assert_eq!(tokens(b"\x1b(B"), [Seen::Esc(b"(".to_vec(), b'B')]);
    }

    #[test]
    fn strings_end_with_bel_or_st() {
        assert_eq!(
            tokens(b"\x1b]0;title\x07"),
            [Seen::Str(StrKind::Osc, b"0;title".to_vec())]
        );
        assert_eq!(
            tokens(b"\x1b]11;?\x1b\\"),
            [Seen::Str(StrKind::Osc, b"11;?".to_vec())]
        );
        assert_eq!(
            tokens(b"\x1bP$qm\x1b\\"),
            [Seen::Str(StrKind::Dcs, b"$qm".to_vec())]
        );
        assert_eq!(
            tokens(b"\x1b_Ga=q\x1b\\"),
            [Seen::Str(StrKind::Apc, b"Ga=q".to_vec())]
        );
    }

    #[test]
    fn enq_is_its_own_token() {
        assert_eq!(
            tokens(b"a\x05b"),
            [
                Seen::Text(b"a".to_vec()),
                Seen::Enq,
                Seen::Text(b"b".to_vec())
            ]
        );
    }

    #[test]
    fn broken_sequences_pass_through() {
        // CAN cancels; a control inside a CSI ends it; ESC restarts.
        assert_eq!(
            tokens(b"\x1b[12\x18x"),
            [
                Seen::Opaque(b"\x1b[12\x18".to_vec()),
                Seen::Text(b"x".to_vec())
            ]
        );
        assert_eq!(
            tokens(b"\x1b[1\nm"),
            [
                Seen::Opaque(b"\x1b[1".to_vec()),
                Seen::Text(b"\nm".to_vec())
            ]
        );
        assert_eq!(
            tokens(b"\x1b[1\x1b[2m"),
            [Seen::Opaque(b"\x1b[1".to_vec()), csi(0, &[2], 0, b'm')]
        );
        assert_eq!(
            tokens(b"\x1b]0;x\x1b[m"),
            [Seen::Opaque(b"\x1b]0;x".to_vec()), csi(0, &[], 0, b'm')]
        );
    }

    #[test]
    fn long_strings_become_opaque_and_still_end() {
        let mut input = b"\x1bPq".to_vec();
        input.extend(std::iter::repeat_n(b'#', 5000));
        input.extend_from_slice(b"\x1b\\after\x1b[m");
        let (seen, raw, parser) = run(&input, &[]);
        assert_eq!(raw, input);
        assert!(parser.is_ground());
        assert_eq!(seen.len(), 3);
        assert!(matches!(&seen[0], Seen::Opaque(o) if o.len() == 5005));
        assert_eq!(seen[1], Seen::Text(b"after".to_vec()));
        assert_eq!(seen[2], csi(0, &[], 0, b'm'));
    }

    #[test]
    fn ground_means_between_characters_too() {
        let mut parser = Parser::new();
        parser.feed(b"ok", |_| {});
        assert!(parser.is_ground());
        parser.feed(&"é".as_bytes()[..1], |_| {});
        assert!(!parser.is_ground());
        parser.feed(&"é".as_bytes()[1..], |_| {});
        assert!(parser.is_ground());
        parser.feed(b"\x1b[", |_| {});
        assert!(!parser.is_ground());
        parser.feed(b"0m", |_| {});
        assert!(parser.is_ground());
    }

    /// A stream with one of everything, used by the cut-everywhere tests here
    /// and in the sibling modules.
    pub(crate) const SAMPLE: &[u8] = b"text \xc3\xa9\xe2\x82\xac\x1b[?1049h\x1b[?1000;1006h\x1b=\x1b[>1u\x1b[c\
        \x1b]11;?\x07\x1b]0;t\x1b\\\x05\x1bP$qm\x1b\\\x1b[2 q\x1b[6n\x1b(B\x1b[1\x18\x1b[?25l tail\r\n";

    #[test]
    fn cutting_the_input_anywhere_changes_nothing() {
        let (whole, raw, _) = run(SAMPLE, &[]);
        assert_eq!(raw, SAMPLE);
        for cut in 1..SAMPLE.len() {
            let (seen, raw, _) = run(SAMPLE, &[cut]);
            assert_eq!(raw, SAMPLE, "raw bytes differ, cut at {cut}");
            assert_eq!(seen, whole, "tokens differ, cut at {cut}");
        }
        let every: Vec<usize> = (1..SAMPLE.len()).collect();
        assert_eq!(run(SAMPLE, &every).0, whole);
    }

    /// Small deterministic generator, biased toward bytes that matter.
    pub(crate) fn noise(seed: u64, len: usize) -> Vec<u8> {
        const ALPHABET: &[u8] =
            b"\x1b\x1b\x1b[[]]P_^X\\\x07\x05\x18\x1a?>=<$ +;:0123456789hlcnpqumtrs\n\xc3\xa9ab";
        let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                if state % 11 == 0 {
                    (state >> 24) as u8
                } else {
                    ALPHABET[(state >> 16) as usize % ALPHABET.len()]
                }
            })
            .collect()
    }

    #[test]
    fn random_input_is_never_lost_or_changed() {
        for seed in 0..300 {
            let input = noise(seed, 400);
            let (whole, raw, parser) = run(&input, &[]);
            let consumed = input.len() - parser.pending();
            assert_eq!(raw, input[..consumed], "seed {seed}");
            let cuts: Vec<usize> = (1..input.len()).step_by(1 + seed as usize % 7).collect();
            let (cut, raw_cut, _) = run(&input, &cuts);
            assert_eq!(raw_cut, raw, "seed {seed}");
            assert_eq!(cut, whole, "seed {seed}");
        }
    }
}
