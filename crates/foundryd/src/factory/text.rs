//! Cutting a tail, and hiding secrets in a comment.

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk9AbC3dEf5GhI7jKl1MnO2pQr4StU"; // 30 mixed letters and digits

    #[test]
    fn tail_keeps_the_last_characters() {
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("abc", 3), "abc");
        assert_eq!(tail("ab", 3), "ab");
        assert_eq!(tail("", 3), "");
        assert_eq!(tail("abc", 0), "");
    }

    #[test]
    fn tail_cuts_on_a_character_boundary() {
        // Each of these is more than one byte: the cut counts characters, never bytes.
        assert_eq!(tail("a€bé✓", 3), "bé✓");
        assert_eq!(tail("€€€€", 2), "€€");
        assert_eq!(tail("x🦀", 1), "🦀");
        assert_eq!(tail("😀😀", 5), "😀😀");
    }

    #[test]
    fn tail_of_megabytes_keeps_exactly_the_last_characters() {
        let text = "é".repeat(3_000_000);
        let cut = tail(&text, 20_000);
        assert_eq!(cut.chars().count(), 20_000);
        assert!(text.ends_with(cut));
    }

    #[test]
    fn for_comment_hides_a_30_character_key() {
        assert_eq!(KEY.len(), 30);
        let text = format!("the key is {KEY}, from .env");
        let shown = for_comment(&text);
        assert!(!shown.contains(KEY), "{shown}");
        assert_eq!(shown, "the key is •••, from .env");
    }

    #[test]
    fn for_comment_keeps_a_pull_request_url_whole() {
        let text = "Opened https://github.com/o/r/pull/123 for it.";
        assert_eq!(for_comment(text), text);
    }

    #[test]
    fn for_comment_keeps_a_url_with_a_long_path_whole() {
        let url = "https://github.com/original-solutions/foundry/blob/0123456789abcdef0123456789abcdef01234567/crates/foundryd/src/factory/text.rs#L12";
        let text = format!("See {url}\nand {KEY}");
        assert_eq!(for_comment(&text), format!("See {url}\nand •••"));
        let http = "http://example.com/runs/aB3dE5gH7jK9mN1pQ3sT5vX7zA9cD1/log";
        assert_eq!(for_comment(http), http);
    }

    #[test]
    fn for_comment_hides_a_key_that_follows_a_url() {
        let text = format!("<https://github.com/o/r/pull/123>{KEY} (https://x.example/a){KEY}");
        assert_eq!(
            for_comment(&text),
            "<https://github.com/o/r/pull/123>••• (https://x.example/a)•••"
        );
    }

    #[test]
    fn for_comment_hides_commit_ids_and_hashes() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(for_comment(&format!("at {sha}.")), "at •••.");
        let token = "ghp_AbCdEf0123456789AbCdEf0123456789";
        assert_eq!(for_comment(&format!("GH_TOKEN={token}")), "GH_TOKEN=•••");
    }

    #[test]
    fn for_comment_keeps_words_paths_and_short_tokens() {
        for text in [
            "crates/foundryd/src/factory/lifecycle/result_line.rs",
            "an_identifier_with_no_digits_at_all_that_is_long",
            "123456789012345678901234567890",
            "abc123 short",
            &KEY[..23],
            "a € multibyte line ✓",
        ] {
            assert_eq!(for_comment(text), text);
        }
        assert_eq!(for_comment(&KEY[..24]), "•••");
    }

    #[test]
    fn elapsed_ms_of_a_later_from_is_0() {
        assert_eq!(elapsed_ms(10_000, 4_000), 0);
        assert_eq!(elapsed_ms(u64::MAX, 0), 0);
    }

    #[test]
    fn elapsed_ms_counts_forward() {
        assert_eq!(elapsed_ms(4_000, 10_000), 6_000);
        assert_eq!(elapsed_ms(7, 7), 0);
    }

    // The rest of the runner's text.ts.

    #[test]
    fn sanitize_replaces_control_and_invisible_characters_with_a_space() {
        assert_eq!(sanitize("a\u{1b}[31mb", false), "a [31mb");
        assert_eq!(sanitize("a\u{0}b\u{7f}c\u{85}d", false), "a b c d");
        assert_eq!(
            sanitize(
                "a\u{2028}b\u{2029}c\u{200b}d\u{202e}e\u{2066}f\u{feff}g",
                false
            ),
            "a b c d e f g"
        );
        assert_eq!(sanitize("a\rb", true), "a b");
    }

    #[test]
    fn sanitize_keeps_newline_and_tab_only_for_layout() {
        assert_eq!(sanitize("a\nb\tc", true), "a\nb\tc");
        assert_eq!(sanitize("a\nb\tc", false), "a b c");
        assert_eq!(sanitize("é ✓ 🦀", false), "é ✓ 🦀");
    }

    #[test]
    fn command_line_is_one_line_cut_at_300_characters() {
        assert_eq!(command_line("cargo test\n--all"), "cargo test --all");
        let long = "é".repeat(301);
        assert_eq!(command_line(&long), format!("{}…", "é".repeat(300)));
        let exact = "x".repeat(300);
        assert_eq!(command_line(&exact), exact);
    }

    #[test]
    fn short_sha_is_the_first_seven_hex_digits() {
        assert_eq!(short_sha("0123456789abcdef"), "0123456");
        assert_eq!(short_sha("zz01-23ABcdef"), "0123ABc");
        assert_eq!(short_sha("abc"), "abc");
        assert_eq!(short_sha(""), "");
    }

    #[test]
    fn is_commit_id_takes_40_or_64_hex_digits() {
        assert!(is_commit_id(&"a".repeat(40)));
        assert!(is_commit_id(&"F".repeat(64)));
        assert!(!is_commit_id(&"a".repeat(39)));
        assert!(!is_commit_id(&"a".repeat(41)));
        assert!(!is_commit_id(&"g".repeat(40)));
        assert!(!is_commit_id(""));
    }

    #[test]
    fn sha256_is_lowercase_hex() {
        assert_eq!(
            sha256(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
