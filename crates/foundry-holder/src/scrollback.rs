//! The current screen and the last 10,000 lines of scrollback. Filled by plan task 4.

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
        scrollback.push(b"\nx");
        assert_eq!(scrollback.snapshot(), b"x");
    }

    #[test]
    fn a_byte_cap_that_cuts_exactly_at_a_line_keeps_that_line() {
        let mut scrollback = Scrollback::with_limits(100, 8);
        scrollback.push(b"abc\ndef\n");
        scrollback.push(b"ghi\n");
        assert_eq!(scrollback.snapshot(), b"def\nghi\n");
    }
}
