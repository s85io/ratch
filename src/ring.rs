//! Layer 1: the last stretch of a session's output, addressed by absolute
//! offset.
//!
//! Every byte the program ever wrote has an offset, counted from the start of
//! the session. The ring keeps the most recent ones. A reader is just an
//! offset into it: a reader that is behind catches up from the ring, a reader
//! that fell off the back is told so, and no reader can hold up the writer.

#![forbid(unsafe_code)]

use std::collections::VecDeque;

/// Marks closer together than this are not worth keeping.
const MARK_SPACING: u64 = 256;

#[derive(Debug)]
pub struct Ring {
    buf: Box<[u8]>,
    total: u64,
    /// History before this offset has been cleared and is not replayed.
    floor: u64,
    /// Offsets where the stream may be cut or joined: between escape
    /// sequences, between characters, ideally at the start of a line.
    marks: VecDeque<u64>,
}

impl Ring {
    pub fn new(capacity: usize) -> Ring {
        assert!(capacity > 0, "a ring needs room");
        Ring {
            buf: vec![0; capacity].into_boxed_slice(),
            total: 0,
            floor: 0,
            marks: VecDeque::from([0]),
        }
    }

    /// Offset one past the newest byte.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Offset of the oldest byte still held.
    pub fn oldest(&self) -> u64 {
        self.total.saturating_sub(self.buf.len() as u64)
    }

    /// Offset of the oldest byte that may be shown as history.
    pub fn history_start(&self) -> u64 {
        self.oldest().max(self.floor)
    }

    /// Forget history: nothing written so far will be replayed.
    pub fn clear_history(&mut self) {
        self.floor = self.total;
    }

    pub fn push(&mut self, mut data: &[u8]) {
        let cap = self.buf.len();
        self.total += data.len() as u64;
        if data.len() > cap {
            data = &data[data.len() - cap..];
        }
        let at = ((self.total - data.len() as u64) % cap as u64) as usize;
        let first = data.len().min(cap - at);
        self.buf[at..at + first].copy_from_slice(&data[..first]);
        self.buf[..data.len() - first].copy_from_slice(&data[first..]);
        let oldest = self.oldest();
        while self.marks.front().is_some_and(|&m| m < oldest) {
            self.marks.pop_front();
        }
    }

    /// The bytes from `from` up to the newest, or up to where the buffer
    /// wraps; call again for the rest. `None` if `from` is no longer held.
    pub fn read(&self, from: u64) -> Option<&[u8]> {
        if from < self.oldest() || from > self.total {
            return None;
        }
        let cap = self.buf.len() as u64;
        let at = (from % cap) as usize;
        let len = (self.total - from).min(cap - at as u64) as usize;
        Some(&self.buf[at..at + len])
    }

    /// Record that the stream may be cut at `offset`.
    pub fn mark(&mut self, offset: u64) {
        debug_assert!(offset <= self.total);
        match self.marks.back() {
            Some(&last) if offset < last + MARK_SPACING => {}
            _ => self.marks.push_back(offset),
        }
    }

    /// The first safe cut at or after `offset`.
    pub fn mark_from(&self, offset: u64) -> Option<u64> {
        let i = self.marks.partition_point(|&m| m < offset);
        self.marks.get(i).copied()
    }

    /// The newest safe cut.
    pub fn last_mark(&self) -> Option<u64> {
        self.marks.back().copied()
    }

    /// Offset where the last `lines` lines of history begin.
    pub fn lines_back(&self, lines: u32) -> u64 {
        let start = self.history_start();
        if lines == 0 {
            return self.total;
        }
        let mut seen = 0;
        let mut at = self.total;
        // A newline that ends the output does not start a line.
        if at > start && self.byte(at - 1) == b'\n' {
            at -= 1;
        }
        while at > start {
            if self.byte(at - 1) == b'\n' {
                seen += 1;
                if seen == lines {
                    return at;
                }
            }
            at -= 1;
        }
        start
    }

    fn byte(&self, offset: u64) -> u8 {
        self.buf[(offset % self.buf.len() as u64) as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything readable from `from`, across the wrap.
    fn read_all(ring: &Ring, mut from: u64) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let chunk = ring.read(from)?;
            if chunk.is_empty() {
                return Some(out);
            }
            out.extend_from_slice(chunk);
            from += chunk.len() as u64;
        }
    }

    #[test]
    fn reads_back_what_was_written() {
        let mut ring = Ring::new(8);
        ring.push(b"abc");
        assert_eq!(read_all(&ring, 0).unwrap(), b"abc");
        assert_eq!(read_all(&ring, 2).unwrap(), b"c");
        assert_eq!(read_all(&ring, 3).unwrap(), b"");
        assert!(ring.read(4).is_none());
    }

    #[test]
    fn old_bytes_fall_off_the_back() {
        let mut ring = Ring::new(8);
        ring.push(b"0123456789");
        assert_eq!((ring.oldest(), ring.total()), (2, 10));
        assert!(ring.read(1).is_none());
        assert_eq!(read_all(&ring, 2).unwrap(), b"23456789");
        ring.push(b"abc");
        assert_eq!(read_all(&ring, 5).unwrap(), b"56789abc");
        assert!(ring.read(4).is_none());
    }

    #[test]
    fn a_write_larger_than_the_ring_keeps_its_tail() {
        let mut ring = Ring::new(4);
        ring.push(b"ab");
        ring.push(b"0123456789");
        assert_eq!((ring.oldest(), ring.total()), (8, 12));
        assert_eq!(read_all(&ring, 8).unwrap(), b"6789");
    }

    #[test]
    fn matches_a_plain_vector_for_any_chunking() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 % 251) as u8).collect();
        for cap in [1, 3, 16, 64] {
            for chunk in [1, 2, 5, 16, 17, 200] {
                let mut ring = Ring::new(cap);
                for piece in data.chunks(chunk) {
                    ring.push(piece);
                    let total = ring.total() as usize;
                    let oldest = ring.oldest() as usize;
                    assert_eq!(oldest, total.saturating_sub(cap));
                    assert_eq!(
                        read_all(&ring, oldest as u64).unwrap(),
                        &data[oldest..total]
                    );
                }
            }
        }
    }

    #[test]
    fn marks_are_sparse_ordered_and_trimmed() {
        let mut ring = Ring::new(1024);
        assert_eq!(ring.mark_from(0), Some(0));
        ring.push(&[b'x'; 600]);
        ring.mark(100); // too close to 0
        ring.mark(300);
        ring.mark(600);
        assert_eq!(ring.mark_from(1), Some(300));
        assert_eq!(ring.mark_from(300), Some(300));
        assert_eq!(ring.mark_from(301), Some(600));
        assert_eq!(ring.mark_from(601), None);
        assert_eq!(ring.last_mark(), Some(600));
        ring.push(&[b'x'; 1000]); // oldest is now 576
        assert_eq!(ring.mark_from(0), Some(600));
    }

    #[test]
    fn lines_back_counts_from_the_end() {
        let mut ring = Ring::new(64);
        ring.push(b"one\ntwo\nthree\n");
        assert_eq!(ring.lines_back(0), 14);
        assert_eq!(ring.lines_back(1), 8);
        assert_eq!(ring.lines_back(2), 4);
        assert_eq!(ring.lines_back(3), 0);
        assert_eq!(ring.lines_back(99), 0);
        ring.push(b"partial");
        assert_eq!(ring.lines_back(1), 14);
        assert_eq!(ring.lines_back(2), 8);
    }

    #[test]
    fn cleared_history_is_not_replayed_but_still_readable() {
        let mut ring = Ring::new(64);
        ring.push(b"old\n");
        ring.clear_history();
        ring.push(b"new\n");
        assert_eq!(ring.history_start(), 4);
        assert_eq!(ring.lines_back(5), 4);
        // A reader already past the floor is unaffected.
        assert_eq!(read_all(&ring, 2).unwrap(), b"d\nnew\n");
    }
}
