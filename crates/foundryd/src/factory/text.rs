//! Cutting a tail, and hiding secrets in a comment.
//!
//! Also the rest of the runner's `src/text.ts`: text from a session, a config or a repo, made
//! safe to store and to show.

use ring::digest;

use crate::sessions::brief::HIDDEN;

/// The shortest run of letters, digits, `-` and `_` [`for_comment`] hides when it mixes letters
/// and digits, as P1 hides them in a session's brief.
const TOKEN_LENGTH: usize = 24;
/// The longest command shown in a gate message or a results table, in characters.
const MAX_COMMAND_CHARS: usize = 300;

/// The last `max_chars` characters of `text`, cut on a character boundary: the whole text when
/// it is no longer than that.
pub fn tail(text: &str, max_chars: usize) -> &str {
    if max_chars == 0 {
        return "";
    }
    match text.char_indices().rev().nth(max_chars - 1) {
        Some((at, _)) => &text[at..],
        None => text,
    }
}

/// The milliseconds from `from_ms` to `to_ms`, never negative: a clock that stepped back
/// counts 0, not a negative span.
pub fn elapsed_ms(from_ms: u64, to_ms: u64) -> u64 {
    to_ms.saturating_sub(from_ms)
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Where a URL ends: at whitespace, or at a character that closes or quotes it in prose or
/// markdown.
fn ends_url(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '<' | '>' | '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
        )
}

/// The length in bytes of the URL `text` starts with, or 0 when it does not start with one.
fn url_len(text: &str) -> usize {
    let lower = |prefix: &str| {
        text.get(..prefix.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
    };
    if !(lower("https://") || lower("http://")) {
        return 0;
    }
    text.find(ends_url).unwrap_or(text.len())
}

fn push_word(out: &mut String, word: &str) {
    let letters = word.bytes().any(|b| b.is_ascii_alphabetic());
    let digits = word.bytes().any(|b| b.is_ascii_digit());
    if word.len() >= TOKEN_LENGTH && letters && digits {
        out.push_str(HIDDEN);
    } else {
        out.push_str(word);
    }
}

/// `text` as a comment the factory posts on GitHub may show it: every run of letters, digits,
/// `-` and `_` that is at least 24 long and mixes letters and digits is hidden, as P1 hides
/// them, except inside an `http://` or `https://` URL, which is kept whole. Keys, tokens,
/// commit ids and hashes look like that; words, paths and pull request URLs do not. Known
/// secrets are hidden before this, by `secrets::hide_known`.
pub fn for_comment(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word_start: Option<usize> = None;
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        let url = url_len(rest);
        if url > 0 {
            if let Some(start) = word_start.take() {
                push_word(&mut out, &text[start..at]);
            }
            out.push_str(&rest[..url]);
            at += url;
            continue;
        }
        let Some(c) = rest.chars().next() else {
            break;
        };
        if is_word(c) {
            word_start.get_or_insert(at);
        } else {
            if let Some(start) = word_start.take() {
                push_word(&mut out, &text[start..at]);
            }
            out.push(c);
        }
        at += c.len_utf8();
    }
    if let Some(start) = word_start {
        push_word(&mut out, &text[start..]);
    }
    out
}

fn is_control(c: char) -> bool {
    let cp = u32::from(c);
    cp <= 0x1f || (0x7f..=0x9f).contains(&cp)
}

/// Characters that could fake a line break or hide text, besides the control characters.
fn is_invisible_or_separator(c: char) -> bool {
    matches!(
        u32::from(c),
        0x2028 | 0x2029 | 0x200b..=0x200f | 0x202a..=0x202e | 0x2066..=0x2069 | 0xfeff
    )
}

/// `text` with what could fake a line break, hide text or drive a terminal replaced with a
/// space. Newline and tab are kept only when `keep_layout` is set.
pub fn sanitize(text: &str, keep_layout: bool) -> String {
    text.chars()
        .map(|c| {
            let layout = c == '\n' || c == '\t';
            if (is_control(c) && !(keep_layout && layout)) || is_invisible_or_separator(c) {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// A command as one short line: sanitised, newlines and tabs turned into spaces, cut at 300
/// characters.
pub fn command_line(command: &str) -> String {
    let clean = sanitize(command, false);
    match clean.char_indices().nth(MAX_COMMAND_CHARS) {
        Some((at, _)) => format!("{}\u{2026}", &clean[..at]),
        None => clean,
    }
}

/// The first seven hex digits of a commit id, as the words for people and the session name it.
pub fn short_sha(sha: &str) -> String {
    sha.chars()
        .filter(char::is_ascii_hexdigit)
        .take(7)
        .collect()
}

/// Whether `text` is a full commit id: 40 hex digits (64 in a SHA-256 repo).
pub fn is_commit_id(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The sha256 of `text`, as lowercase hex.
pub fn sha256(text: &str) -> String {
    digest::digest(&digest::SHA256, text.as_bytes())
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

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
        let review = "https://github.com/o/r/pull/123#discussion_r1234567890abcdefgh";
        assert_eq!(for_comment(review), review);
        let upper = "HTTPS://GitHub.com/o/r/commit/0123456789abcdef0123456789abcdef01234567";
        assert_eq!(for_comment(upper), upper);
    }

    #[test]
    fn for_comment_ends_a_url_at_a_character_no_url_holds() {
        for (before, after) in [
            ("https://a.example\\nKEY=", ""),
            ("https://github.com/o/r/pull/1\u{200b}", ""),
            ("https://github.com/o/r/pull/1\u{1b}", ""),
            ("https://x.example/a\u{2192}", ""),
            ("https://x.example/a\\", ""),
            ("https://x.example/a^", ""),
            (
                "PR\u{306f}https://github.com/o/r/pull/1\u{3067}\u{3059}\u{3002}",
                "\u{3067}\u{3059}",
            ),
            ("endpoint=https://api.example.com,key=", ""),
            ("https://github.com/o/r/pull/1,GH_TOKEN=", ""),
            ("https://github.com/o/r/pull/1;", ""),
            ("`https://x/a`", ""),
            ("\"https://x/a\"", ""),
            ("[https://x/a]", ""),
        ] {
            let text = format!("{before}{KEY}{after}");
            assert_eq!(for_comment(&text), format!("{before}•••{after}"), "{text}");
        }
    }

    #[test]
    fn for_comment_hides_a_token_in_a_url_query_userinfo_or_another_hosts_path() {
        let webhook = format!("https://hooks.slack.com/services/T0000/B0000/{KEY}");
        assert_eq!(
            for_comment(&webhook),
            "https://hooks.slack.com/services/T0000/B0000/•••"
        );
        let log = "http://example.com/runs/aB3dE5gH7jK9mN1pQ3sT5vX7zA9cD1/log";
        assert_eq!(for_comment(log), "http://example.com/runs/•••/log");
        let query = format!("https://github.com/o/r/pull/1?access_token={KEY}#top");
        assert_eq!(
            for_comment(&query),
            "https://github.com/o/r/pull/1?access_token=•••#top"
        );
        let signed = format!("https://bucket.s3.amazonaws.com/a?X-Amz-Signature={KEY}");
        assert_eq!(
            for_comment(&signed),
            "https://bucket.s3.amazonaws.com/a?X-Amz-Signature=•••"
        );
        let userinfo = "git push https://bot:short@github.com/o/r";
        assert_eq!(for_comment(userinfo), "git push https://***@github.com/o/r");
        let token_user = format!("https://{KEY}@github.com/o/r/pull/1");
        assert_eq!(
            for_comment(&token_user),
            "https://***@github.com/o/r/pull/1"
        );
    }

    #[test]
    fn for_comment_keeps_a_word_glued_to_the_scheme() {
        assert_eq!(for_comment("xhttp://a.b"), "xhttp://a.b");
        assert_eq!(
            for_comment(&format!("{KEY}https://github.com/o/r/pull/1")),
            "•••https://github.com/o/r/pull/1"
        );
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

    /// The runner's case, from its `src/gate.test.ts`.
    #[test]
    fn sanitize_turns_control_and_hidden_text_characters_into_spaces() {
        assert_eq!(sanitize("a\u{1b}b\u{0}c\u{7f}d\u{85}e", false), "a b c d e");
        assert_eq!(sanitize("a\nb\tc", false), "a b c");
        assert_eq!(sanitize("a\nb\tc", true), "a\nb\tc");
        assert_eq!(
            sanitize(
                "x\u{2028}y\u{2029}z\u{200b}w\u{200f}\u{202a}\u{2069}\u{feff}v",
                true
            ),
            "x y z w    v"
        );
    }

    #[test]
    fn sanitize_replaces_both_ends_of_each_range_and_keeps_their_neighbours() {
        for c in [
            '\u{0}', '\u{1f}', '\u{7f}', '\u{9f}', '\u{200b}', '\u{200f}', '\u{202a}', '\u{202e}',
            '\u{2066}', '\u{2069}', '\u{2028}', '\u{2029}', '\u{feff}',
        ] {
            assert_eq!(
                sanitize(&format!("a{c}b"), false),
                "a b",
                "{:x}",
                u32::from(c)
            );
        }
        for c in [
            '\u{20}', '\u{7e}', '\u{a0}', '\u{200a}', '\u{2010}', '\u{2027}', '\u{202f}',
            '\u{2065}', '\u{206a}', '\u{fefe}',
        ] {
            let text = format!("a{c}b");
            assert_eq!(sanitize(&text, false), text, "{:x}", u32::from(c));
        }
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
