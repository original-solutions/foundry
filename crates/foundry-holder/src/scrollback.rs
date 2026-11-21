//! The current screen and the last 10,000 lines of scrollback.
//!
//! The holder is not a terminal emulator. It keeps the tail of the raw bytes the PTY produced, cut
//! at a line, and a client replays them into its own emulator (xterm.js) to rebuild the screen and
//! history. Terminal modes set before the kept tail began are not replayed.

use std::collections::VecDeque;

/// Lines of scrollback kept.
pub const MAX_LINES: usize = 10_000;

/// Bytes kept, whatever the line count, so a child that never prints a newline cannot grow the
/// holder without limit.
pub const MAX_BYTES: usize = 8 * 1024 * 1024;

/// The tail of a terminal's output: at most `max_lines` newlines and `max_bytes` bytes.
#[derive(Debug)]
pub struct Scrollback {
    bytes: VecDeque<u8>,
    /// Stream offset of `bytes[0]`.
    start: u64,
    /// Stream offset of every newline in `bytes`, oldest first.
    newlines: VecDeque<u64>,
    /// False when a byte cap cut the oldest line, so `bytes` starts part way through it.
    on_line: bool,
    max_lines: usize,
    max_bytes: usize,
}

impl Default for Scrollback {
    fn default() -> Self {
        Self::new()
    }
}

impl Scrollback {
    /// A scrollback of [`MAX_LINES`] lines and at most [`MAX_BYTES`] bytes.
    pub fn new() -> Self {
        Self::with_limits(MAX_LINES, MAX_BYTES)
    }

    pub fn with_limits(max_lines: usize, max_bytes: usize) -> Self {
        Scrollback {
            bytes: VecDeque::new(),
            start: 0,
            newlines: VecDeque::new(),
            on_line: true,
            max_lines,
            max_bytes,
        }
    }

    /// Appends output, then drops the oldest lines over either limit.
    pub fn push(&mut self, mut data: &[u8]) {
        if data.len() > self.max_bytes {
            let skipped = data.len() - self.max_bytes;
            self.start += (self.bytes.len() + skipped) as u64;
            self.bytes.clear();
            self.newlines.clear();
            self.on_line = data[skipped - 1] == b'\n';
            data = &data[skipped..];
        }
        let over = (self.bytes.len() + data.len()).saturating_sub(self.max_bytes);
        self.drop_front(over);
        let end = self.start + self.bytes.len() as u64;
        push_newlines(&mut self.newlines, data, end);
        self.bytes.extend(data);
        if !self.on_line {
            self.drop_to_next_line();
        }
        if self.newlines.len() > self.max_lines {
            let excess = self.newlines.len() - self.max_lines;
            let at = self.newlines[excess - 1];
            let n = usize::try_from(at + 1 - self.start).unwrap_or(self.bytes.len());
            self.drop_front(n);
        }
    }

    /// The offset of the oldest kept byte in everything pushed.
    pub fn start(&self) -> u64 {
        self.start
    }

    /// Number of kept bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// True when no bytes are kept.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Appends everything kept, oldest first, into `dst`.
    pub fn snapshot_into(&self, dst: &mut Vec<u8>) {
        let (front, back) = self.bytes.as_slices();
        dst.reserve(front.len() + back.len());
        dst.extend_from_slice(front);
        dst.extend_from_slice(back);
    }

    /// Everything kept, oldest first.
    pub fn snapshot(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.bytes.len());
        self.snapshot_into(&mut out);
        out
    }

    fn drop_front(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        self.on_line = self.bytes[n - 1] == b'\n';
        self.bytes.drain(..n);
        self.start += n as u64;
        while self.newlines.front().is_some_and(|&at| at < self.start) {
            self.newlines.pop_front();
        }
    }

    /// Drops through the oldest newline kept, so what is left starts on a line.
    fn drop_to_next_line(&mut self) {
        if let Some(&at) = self.newlines.front() {
            let n = usize::try_from(at + 1 - self.start).unwrap_or(self.bytes.len());
            self.drop_front(n);
        }
    }
}

/// Records the stream offset of every `b'\n'` in `data`, scanning 8 bytes at a time via SWAR so
/// chunks without newlines skip 8 bytes per iteration.
fn push_newlines(newlines: &mut VecDeque<u64>, data: &[u8], base: u64) {
    const LF_MASK: u64 = u64::from_ne_bytes([b'\n'; 8]);
    const LO_BITS: u64 = 0x0101_0101_0101_0101;
    const HI_BITS: u64 = 0x8080_8080_8080_8080;

    let (chunks, tail) = data.as_chunks::<8>();
    let mut offset = 0usize;
    for chunk in chunks {
        let x = u64::from_ne_bytes(*chunk) ^ LF_MASK;
        if x.wrapping_sub(LO_BITS) & !x & HI_BITS != 0 {
            for (j, &byte) in chunk.iter().enumerate() {
                if byte == b'\n' {
                    newlines.push_back(base + (offset + j) as u64);
                }
            }
        }
        offset += 8;
    }
    for (j, &byte) in tail.iter().enumerate() {
        if byte == b'\n' {
            newlines.push_back(base + (offset + j) as u64);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(from: usize, to: usize) -> Vec<u8> {
        (from..=to)
            .flat_map(|n| format!("{n}\n").into_bytes())
            .collect()
    }

    #[test]
    fn keeps_the_last_10000_lines_and_drops_the_oldest() {
        let mut scrollback = Scrollback::new();
        scrollback.push(&numbered(1, 10_050));
        assert_eq!(scrollback.snapshot(), numbered(51, 10_050));
    }

    #[test]
    fn lines_split_across_pushes_are_counted_once() {
        let mut scrollback = Scrollback::with_limits(3, 1024);
        for byte in numbered(1, 5) {
            scrollback.push(&[byte]);
        }
        assert_eq!(scrollback.snapshot(), numbered(3, 5));
    }

    #[test]
    fn an_unfinished_line_is_kept() {
        let mut scrollback = Scrollback::with_limits(2, 1024);
        scrollback.push(b"one\ntwo\nthree\n$ ");
        assert_eq!(scrollback.snapshot(), b"two\nthree\n$ ");
    }

    #[test]
    fn long_lines_are_capped_by_bytes_and_start_on_a_line() {
        let mut scrollback = Scrollback::with_limits(100, 16);
        scrollback.push(b"aaaaaaaaaa\nbbbbbbbbbb\ncc");
        assert_eq!(scrollback.snapshot(), b"bbbbbbbbbb\ncc");
        assert!(scrollback.snapshot().len() <= 16);
    }

    #[test]
    fn a_chunk_larger_than_the_cap_keeps_its_tail() {
        let mut scrollback = Scrollback::with_limits(100, 4);
        scrollback.push(b"abc\n");
        scrollback.push(b"0123456789");
        assert_eq!(scrollback.snapshot(), b"6789");
        // The 4 bytes held and the 6 skipped are behind the snapshot now.
        assert_eq!(scrollback.start(), 10);
        scrollback.push(b"\nx");
        assert_eq!(scrollback.snapshot(), b"x");
        assert_eq!(scrollback.start(), 15);
    }

    #[test]
    fn a_byte_cap_that_cuts_exactly_at_a_line_keeps_that_line() {
        let mut scrollback = Scrollback::with_limits(100, 8);
        scrollback.push(b"abc\ndef\n");
        scrollback.push(b"ghi\n");
        assert_eq!(scrollback.snapshot(), b"def\nghi\n");
    }

    #[test]
    fn snapshot_into_matches_snapshot_across_wrapped_ring_buffer() {
        let mut scrollback = Scrollback::with_limits(10, 64);
        // Push and evict several times so VecDeque wraps around internally.
        for i in 0..40 {
            scrollback.push(format!("line-{i:02}\n").as_bytes());
        }
        let mut dst = b"prefix:".to_vec();
        scrollback.snapshot_into(&mut dst);
        let mut expected = b"prefix:".to_vec();
        expected.extend_from_slice(&scrollback.snapshot());
        assert_eq!(dst, expected);
        assert_eq!(scrollback.len(), scrollback.snapshot().len());
    }

    #[test]
    fn bulk_push_over_line_limit_keeps_exact_tail() {
        let mut scrollback = Scrollback::with_limits(100, 1024 * 1024);
        scrollback.push(&numbered(1, 5_000));
        assert_eq!(scrollback.snapshot(), numbered(4_901, 5_000));
    }

    #[test]
    fn newline_scan_handles_every_word_lane_and_high_bytes() {
        for pos in 0..16 {
            let mut buf = vec![0x8au8; 24]; // 0x8a has low 7 bits equal to 0x0a ('\n')
            buf[pos] = b'\n';
            let mut sb = Scrollback::with_limits(1, 1024);
            sb.push(&buf);
            sb.push(b"tail\n");
            let mut expected = buf[pos + 1..].to_vec();
            expected.extend_from_slice(b"tail\n");
            assert_eq!(sb.snapshot(), expected, "failed at newline position {pos}");
        }
    }
}
