#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_is_true_only_for_equal_bytes() {
        assert!(ct_eq(b"", b""));
        assert!(ct_eq(b"Bearer abc", b"Bearer abc"));
        assert!(!ct_eq(b"Bearer abc", b"Bearer abd"));
        assert!(!ct_eq(b"Bearer abc", b"Bearer ab"));
        assert!(!ct_eq(b"Bearer ab", b"Bearer abc"));
        assert!(!ct_eq(b"a", b""));
    }
}
