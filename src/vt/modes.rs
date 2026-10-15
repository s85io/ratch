//! Remembers which terminal modes a program has switched on.
//!
//! A program sets its modes once, at startup, and assumes they stay. Two
//! places need that memory: the master, to bring a newly attached terminal
//! into the state the program expects, and the client, to hand the user's
//! terminal back the way it found it.
//!
//! Only modes are tracked. What is on the screen is the program's business;
//! it repaints when it is told the window changed.

#![forbid(unsafe_code)]

use std::io::Write;

use super::parser::{Csi, Token};

/// DEC private modes worth carrying across an attach, in the order they are
/// restored. The alternate screen comes first because entering it clears it.
const MODES: [u16; 20] = [
    47, 1047, 1049, // alternate screen
    1, 7, // cursor keys, autowrap
    9, 1000, 1002, 1003, // mouse reporting
    1005, 1006, 1015, 1016, // mouse encoding
    1004, 1007, 2004, // focus events, alternate scroll, bracketed paste
    // Reports the terminal sends unasked: colour scheme changes, window size
    // changes, pastes (kitty). Left on, they turn up as junk in the shell.
    2031, 2048, 5522, 25, // cursor visibility, last so the cursor does not flicker
];

const ALT_SCREEN: [u16; 3] = [47, 1047, 1049];

/// Modes a terminal has on before any program touches it.
const DEFAULT_ON: [u16; 2] = [7, 25];

/// Kitty keyboard flags are kept on a stack per screen; the protocol caps it.
const KITTY_DEPTH: usize = 16;

fn bit(mode: u16) -> Option<u32> {
    MODES.iter().position(|&m| m == mode).map(|i| 1 << i)
}

fn mask(modes: &[u16]) -> u32 {
    modes
        .iter()
        .filter_map(|&m| bit(m))
        .fold(0, |acc, b| acc | b)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Kitty {
    stack: Vec<u8>,
    current: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Modes {
    on: u32,
    /// Values put aside by `CSI ? Pm s`, taken back by `CSI ? Pm r`.
    saved: u32,
    keypad_application: bool,
    modify_other_keys: Option<u8>,
    cursor_style: Option<u8>,
    /// Keyboard flags for the main screen and the alternate screen.
    kitty: [Kitty; 2],
}

impl Default for Modes {
    fn default() -> Self {
        Modes::new()
    }
}

impl Modes {
    pub fn new() -> Modes {
        Modes {
            on: mask(&DEFAULT_ON),
            saved: mask(&DEFAULT_ON),
            keypad_application: false,
            modify_other_keys: None,
            cursor_style: None,
            kitty: Default::default(),
        }
    }

    pub fn alt_screen(&self) -> bool {
        self.on & mask(&ALT_SCREEN) != 0
    }

    fn set(&mut self, mode: u16, on: bool) {
        if let Some(b) = bit(mode) {
            if on {
                self.on |= b;
            } else {
                self.on &= !b;
            }
        }
    }

    fn kitty_mut(&mut self) -> &mut Kitty {
        let screen = usize::from(self.alt_screen());
        &mut self.kitty[screen]
    }

    /// Update the record from one token of program output.
    pub fn apply(&mut self, token: &Token<'_>) {
        match *token {
            Token::Csi(csi) => self.apply_csi(&csi),
            Token::Esc {
                inter: [],
                fin: b'=',
                ..
            } => self.keypad_application = true,
            Token::Esc {
                inter: [],
                fin: b'>',
                ..
            } => self.keypad_application = false,
            Token::Esc {
                inter: [],
                fin: b'c',
                ..
            } => *self = Modes::new(),
            _ => {}
        }
    }

    fn apply_csi(&mut self, csi: &Csi<'_>) {
        match (csi.private, csi.inter, csi.fin) {
            (b'?', 0, b'h') => csi.params().for_each(|m| self.set(m, true)),
            (b'?', 0, b'l') => csi.params().for_each(|m| self.set(m, false)),
            (b'?', 0, b's') => {
                for b in csi.params().filter_map(bit) {
                    self.saved = self.saved & !b | self.on & b;
                }
            }
            (b'?', 0, b'r') => {
                for b in csi.params().filter_map(bit) {
                    self.on = self.on & !b | self.saved & b;
                }
            }
            (b'>', 0, b'u') => {
                let flags = csi.param(0).unwrap_or(0) as u8;
                let kitty = self.kitty_mut();
                if kitty.stack.len() == KITTY_DEPTH {
                    kitty.stack.remove(0);
                }
                kitty.stack.push(kitty.current);
                kitty.current = flags;
            }
            (b'<', 0, b'u') => {
                let count = csi.param(0).unwrap_or(1).max(1);
                let kitty = self.kitty_mut();
                for _ in 0..count {
                    kitty.current = kitty.stack.pop().unwrap_or(0);
                }
            }
            (b'=', 0, b'u') => {
                let flags = csi.param(0).unwrap_or(0) as u8;
                let kitty = self.kitty_mut();
                match csi.param(1).unwrap_or(1) {
                    2 => kitty.current |= flags,
                    3 => kitty.current &= !flags,
                    _ => kitty.current = flags,
                }
            }
            (b'>', 0, b'm') if csi.param(0) == Some(4) => {
                self.modify_other_keys = csi.param(1).filter(|&v| v != 0).map(|v| v as u8);
            }
            (0, b' ', b'q') => {
                self.cursor_style = csi.param(0).filter(|&v| v != 0).map(|v| v as u8);
            }
            // Soft reset: keys and cursor go back to normal, the screen stays.
            (0, b'!', b'p') => {
                self.set(1, false);
                self.set(25, true);
                self.keypad_application = false;
            }
            _ => {}
        }
    }

    /// Append what a fresh terminal must be sent to match this record.
    pub fn restore(&self, out: &mut Vec<u8>) {
        let defaults = mask(&DEFAULT_ON);
        // Keyboard flags of the main screen have to go in before leaving it.
        self.kitty[0].restore(out);
        for &mode in &MODES {
            let b = bit(mode).unwrap_or(0);
            if (self.on ^ defaults) & b != 0 {
                let _ = write!(
                    out,
                    "\x1b[?{}{}",
                    mode,
                    if self.on & b != 0 { 'h' } else { 'l' }
                );
            }
            if mode == ALT_SCREEN[ALT_SCREEN.len() - 1] && self.alt_screen() {
                self.kitty[1].restore(out);
            }
        }
        if self.keypad_application {
            out.extend_from_slice(b"\x1b=");
        }
        if let Some(level) = self.modify_other_keys {
            let _ = write!(out, "\x1b[>4;{level}m");
        }
        if let Some(style) = self.cursor_style {
            let _ = write!(out, "\x1b[{style} q");
        }
    }

    /// Append what undoes this record on a terminal that is in it.
    pub fn reset(&self, out: &mut Vec<u8>) {
        let defaults = mask(&DEFAULT_ON);
        if self.alt_screen() {
            self.kitty[1].reset(out);
        }
        // Reverse order puts the alternate screen last, so leaving it is the
        // final thing the user sees happen.
        for &mode in MODES.iter().rev() {
            let b = bit(mode).unwrap_or(0);
            if (self.on ^ defaults) & b != 0 {
                let _ = write!(
                    out,
                    "\x1b[?{}{}",
                    mode,
                    if defaults & b != 0 { 'h' } else { 'l' }
                );
            }
        }
        self.kitty[0].reset(out);
        if self.keypad_application {
            out.extend_from_slice(b"\x1b>");
        }
        if self.modify_other_keys.is_some() {
            out.extend_from_slice(b"\x1b[>4m");
        }
        if self.cursor_style.is_some() {
            out.extend_from_slice(b"\x1b[0 q");
        }
    }
}

impl Kitty {
    fn restore(&self, out: &mut Vec<u8>) {
        // The stack holds the flags that were current before each push.
        let mut levels = self.stack.iter().copied().chain([self.current]);
        let base = levels.next().unwrap_or(0);
        if base != 0 {
            let _ = write!(out, "\x1b[={base};1u");
        }
        for flags in levels {
            let _ = write!(out, "\x1b[>{flags}u");
        }
    }

    fn reset(&self, out: &mut Vec<u8>) {
        if !self.stack.is_empty() {
            let _ = write!(out, "\x1b[<{}u", self.stack.len());
        }
        if self.stack.first().copied().unwrap_or(self.current) != 0 {
            out.extend_from_slice(b"\x1b[=0;1u");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::parser::tests::SAMPLE;
    use super::super::parser::Parser;
    use super::*;

    fn track(input: &[u8]) -> Modes {
        let mut modes = Modes::new();
        Parser::new().feed(input, |t| modes.apply(&t));
        modes
    }

    fn restore(input: &[u8]) -> String {
        let mut out = Vec::new();
        track(input).restore(&mut out);
        String::from_utf8(out).unwrap()
    }

    fn reset(input: &[u8]) -> String {
        let mut out = Vec::new();
        track(input).reset(&mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn untouched_terminal_needs_nothing() {
        assert_eq!(restore(b"plain output\r\n\x1b[1;31mred\x1b[m"), "");
        assert_eq!(reset(b"plain output\r\n\x1b[1;31mred\x1b[m"), "");
    }

    #[test]
    fn a_full_screen_program() {
        let vim = b"\x1b[?1049h\x1b[?1h\x1b=\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[?25l";
        assert_eq!(
            restore(vim),
            "\x1b[?1049h\x1b[?1h\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[?25l\x1b="
        );
        assert_eq!(
            reset(vim),
            "\x1b[?25h\x1b[?2004l\x1b[?1006l\x1b[?1000l\x1b[?1l\x1b[?1049l\x1b>"
        );
        assert!(track(vim).alt_screen());
    }

    #[test]
    fn switching_a_mode_back_off_leaves_no_trace() {
        let input =
            b"\x1b[?1049h\x1b[?1000;1006h\x1b[?25l ... \x1b[?25h\x1b[?1000;1006l\x1b[?1049l";
        assert_eq!(track(input), Modes::new());
    }

    #[test]
    fn full_reset_forgets_everything() {
        assert_eq!(
            track(b"\x1b[?1049h\x1b[?1003h\x1b[>5u\x1b[4 q\x1bc"),
            Modes::new()
        );
    }

    #[test]
    fn soft_reset_keeps_the_screen_and_mouse() {
        assert_eq!(
            restore(b"\x1b[?1049h\x1b[?1h\x1b[?1000h\x1b[?25l\x1b=\x1b[!p"),
            "\x1b[?1049h\x1b[?1000h"
        );
    }

    #[test]
    fn save_and_restore_of_private_modes() {
        // Save "mouse off", turn it on, restore: off again.
        assert_eq!(restore(b"\x1b[?1000s\x1b[?1000h\x1b[?1000r"), "");
        // Save "mouse on", turn it off, restore: on again.
        assert_eq!(
            restore(b"\x1b[?1000h\x1b[?1000s\x1b[?1000l\x1b[?1000r"),
            "\x1b[?1000h"
        );
    }

    #[test]
    fn kitty_keyboard_stack() {
        assert_eq!(restore(b"\x1b[>1u"), "\x1b[>1u");
        assert_eq!(reset(b"\x1b[>1u"), "\x1b[<1u");
        assert_eq!(restore(b"\x1b[>1u\x1b[>3u"), "\x1b[>1u\x1b[>3u");
        assert_eq!(reset(b"\x1b[>1u\x1b[>3u"), "\x1b[<2u");
        assert_eq!(restore(b"\x1b[>1u\x1b[>3u\x1b[<u"), "\x1b[>1u");
        assert_eq!(restore(b"\x1b[>1u\x1b[<5u"), "");
        // Setting without pushing changes the level in place.
        assert_eq!(restore(b"\x1b[=5u"), "\x1b[=5;1u");
        assert_eq!(reset(b"\x1b[=5u"), "\x1b[=0;1u");
        assert_eq!(restore(b"\x1b[>1u\x1b[=8;2u"), "\x1b[>9u");
        assert_eq!(restore(b"\x1b[>9u\x1b[=8;3u"), "\x1b[>1u");
    }

    #[test]
    fn kitty_flags_belong_to_a_screen() {
        // Pushed on the main screen, then again inside the alternate screen.
        let input = b"\x1b[>1u\x1b[?1049h\x1b[>7u";
        assert_eq!(restore(input), "\x1b[>1u\x1b[?1049h\x1b[>7u");
        assert_eq!(reset(input), "\x1b[<1u\x1b[?1049l\x1b[<1u");
        // Leaving the alternate screen brings the main screen's flags back.
        assert_eq!(
            restore(b"\x1b[>1u\x1b[?1049h\x1b[>7u\x1b[<u\x1b[?1049l"),
            "\x1b[>1u"
        );
    }

    #[test]
    fn reports_the_terminal_sends_unasked() {
        let omp = b"\x1b[?2004h\x1b[?2031h\x1b[?2048h\x1b[?5522h";
        assert_eq!(restore(omp), "\x1b[?2004h\x1b[?2031h\x1b[?2048h\x1b[?5522h");
        assert_eq!(reset(omp), "\x1b[?5522l\x1b[?2048l\x1b[?2031l\x1b[?2004l");
    }

    #[test]
    fn keys_and_cursor_shape() {
        assert_eq!(restore(b"\x1b[>4;2m\x1b[6 q"), "\x1b[>4;2m\x1b[6 q");
        assert_eq!(reset(b"\x1b[>4;2m\x1b[6 q"), "\x1b[>4m\x1b[0 q");
        assert_eq!(restore(b"\x1b[>4;2m\x1b[>4m\x1b[6 q\x1b[0 q"), "");
    }

    #[test]
    fn unknown_modes_and_other_sequences_are_ignored() {
        assert_eq!(
            track(b"\x1b[?2026h\x1b[?12h\x1b[4h\x1b[20h\x1b[1000h"),
            Modes::new()
        );
    }

    #[test]
    fn restore_reproduces_the_record() {
        for input in [
            &b"\x1b[?1049h\x1b[?1h\x1b=\x1b[?2004h\x1b[?1002h\x1b[?1006h\x1b[?25l\x1b[>1u\x1b[>4;1m\x1b[2 q"[..],
            b"\x1b[>1u\x1b[?47h\x1b[=3u\x1b[?7l",
            b"\x1b[=2u\x1b[>1u\x1b[>5u",
            SAMPLE,
        ] {
            let modes = track(input);
            let mut bytes = Vec::new();
            modes.restore(&mut bytes);
            assert_eq!(track(&bytes), modes, "restore of {:?}", String::from_utf8_lossy(input));
            // And reset, applied on top, lands on a fresh terminal.
            modes.reset(&mut bytes);
            assert_eq!(track(&bytes), Modes::new(), "reset of {:?}", String::from_utf8_lossy(input));
        }
    }
}
