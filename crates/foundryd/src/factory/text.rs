//! Cutting a tail, and hiding secrets in a comment.
//!
//! Also the rest of the runner's `src/text.ts`: text from a session, a config or a repo, made
//! safe to store and to show.

use ring::digest;

use crate::secrets::redact;
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

/// Whether `c` can be part of a URL as [`for_comment`] reads one: an ASCII letter or digit, or
/// one of RFC 3986's other characters except the quotes, brackets and parentheses that close a
/// URL in prose or markdown, and `,` and `;`, which end one in a list. Whitespace, controls,
/// every non-ASCII character, `\` and `^` end a URL too, since no URL holds them.
fn in_url(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '.'
                | '_'
                | '~'
                | ':'
                | '/'
                | '?'
                | '#'
                | '@'
                | '!'
                | '$'
                | '&'
                | '*'
                | '+'
                | '='
                | '%'
        )
}

/// The length in bytes of the URL `text` starts with: `http://` or `https://`, in any case,
/// then every URL character up to the first that is not one. 0 when `text` does not start with
/// a URL.
fn url_len(text: &str) -> usize {
    let Some(scheme) = ["https://", "http://"].into_iter().find(|scheme| {
        text.get(..scheme.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(scheme))
    }) else {
        return 0;
    };
    let rest = &text[scheme.len()..];
    scheme.len() + rest.find(|c| !in_url(c)).unwrap_or(rest.len())
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

/// `text` with every run of letters, digits, `-` and `_` that is at least 24 long and mixes
/// letters and digits hidden, and everything else as it is.
fn push_hidden(out: &mut String, text: &str) {
    let mut word_start: Option<usize> = None;
    for (at, c) in text.char_indices() {
        if is_word(c) {
            word_start.get_or_insert(at);
        } else {
            if let Some(start) = word_start.take() {
                push_word(out, &text[start..at]);
            }
            out.push(c);
        }
    }
    if let Some(start) = word_start {
        push_word(out, &text[start..]);
    }
}

/// The path segments a GitHub link puts a commit id after, as in `commit/<id>`, `blob/<id>/…`
/// or `compare/<id>...<id>`.
const GITHUB_COMMIT_AFTER: [&str; 7] = [
    "commit", "commits", "blob", "tree", "blame", "raw", "compare",
];

/// The anchors a GitHub page links to that end in a number: an issue comment, a review comment
/// and a review.
const GITHUB_NUMBERED_ANCHORS: [&str; 3] = ["issuecomment-", "discussion_r", "pullrequestreview-"];

fn is_number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// Whether `lines` is empty or a line of a diff, `L12` or `R12`, or a range of them, `L3-R20`.
fn is_diff_lines(lines: &str) -> bool {
    let line = |line: &str| line.strip_prefix(['L', 'R']).is_some_and(is_number);
    lines.is_empty()
        || match lines.split_once('-') {
            Some((from, to)) => line(from) && line(to),
            None => line(lines),
        }
}

/// Whether the whole of `fragment`, after its `#`, is an anchor a GitHub page links to: a
/// numbered comment or review, or a file of a diff, `diff-<id>`, with an optional line.
fn is_github_anchor(fragment: &str) -> bool {
    if let Some(number) = GITHUB_NUMBERED_ANCHORS
        .iter()
        .find_map(|name| fragment.strip_prefix(name))
    {
        return is_number(number);
    }
    let Some(diff) = fragment.strip_prefix("diff-") else {
        return false;
    };
    let (id, lines) = diff.split_at(diff.find(['L', 'R']).unwrap_or(diff.len()));
    is_commit_id(id) && is_diff_lines(lines)
}

/// The path, query and fragment of a `github.com` URL as a comment may show them. A commit id
/// is kept whole only as the whole path segment after one of [`GITHUB_COMMIT_AFTER`], or as
/// one side of a compare's `...`; a fragment only when the whole of it is a GitHub anchor.
/// Everything else, the query included, goes through the word rule.
fn push_github(out: &mut String, rest: &str) {
    let (path, rest) = rest.split_at(rest.find(['?', '#']).unwrap_or(rest.len()));
    let mut after = "";
    for (n, segment) in path.split('/').enumerate() {
        if n > 0 {
            out.push('/');
        }
        if GITHUB_COMMIT_AFTER.contains(&after) {
            for (n, side) in segment.split("...").enumerate() {
                if n > 0 {
                    out.push_str("...");
                }
                if is_commit_id(side) {
                    out.push_str(side);
                } else {
                    push_hidden(out, side);
                }
            }
        } else {
            push_hidden(out, segment);
        }
        after = segment;
    }
    let (query, fragment) = rest.split_at(rest.find('#').unwrap_or(rest.len()));
    push_hidden(out, query);
    if let Some(anchor) = fragment.strip_prefix('#') {
        out.push('#');
        if is_github_anchor(anchor) {
            out.push_str(anchor);
        } else {
            push_hidden(out, anchor);
        }
    }
}

/// A URL, its userinfo already hidden by [`redact`](crate::secrets::redact), as a comment may
/// show it. Every part goes through the word rule like the rest of the text, since a webhook,
/// a signed link or an `?access_token=` carries its secret there. Only a `github.com` URL keeps
/// the ids its commit, blob, compare and review links hold ([`push_github`]).
fn push_url(out: &mut String, url: &str) {
    let scheme = url.find("://").map_or(0, |at| at + 3);
    out.push_str(&url[..scheme]);
    let rest = &url[scheme..];
    let (authority, rest) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    let host = authority.rfind('@').map_or(0, |at| at + 1);
    let (userinfo, host) = authority.split_at(host);
    push_hidden(out, userinfo);
    push_hidden(out, host);
    if host.eq_ignore_ascii_case("github.com") {
        push_github(out, rest);
    } else {
        push_hidden(out, rest);
    }
}

/// `text` as a comment the factory posts on GitHub may show it: every run of letters, digits,
/// `-` and `_` that is at least 24 long and mixes letters and digits is hidden, as P1 hides
/// them. Keys, tokens, commit ids and hashes look like that; words and paths do not. Inside an
/// `http://` or `https://` URL the same holds, except that a `github.com` URL keeps a commit id
/// where its commit, blob, tree and compare links put one, and an anchor its review and diff
/// links end in, so those links come through whole. A URL's userinfo is hidden as
/// [`redact`](crate::secrets::redact) hides it. A URL ends at the first character no URL holds,
/// and at `,` and `;`.
///
/// The caller hides the secrets it knows (held tokens, gateway keys, URL passwords) before
/// this, and adds the factory's own markers, such as `<!-- foundry:claim <run id> <machine> -->`,
/// after it: a run id and a machine id are token-shaped, and this would hide them.
pub fn for_comment(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut plain = 0;
    let mut at = 0;
    while at < text.len() {
        let url = url_len(&text[at..]);
        if url > 0 {
            push_hidden(&mut out, &text[plain..at]);
            push_url(&mut out, &redact(&text[at..at + url]));
            at += url;
            plain = at;
        } else {
            at += text[at..].chars().next().map_or(1, char::len_utf8);
        }
    }
    push_hidden(&mut out, &text[plain..]);
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
        let upper = "HTTPS://GitHub.com/o/r/commit/0123456789abcdef0123456789abcdef01234567";
        assert_eq!(for_comment(upper), upper);
    }

    #[test]
    fn for_comment_keeps_the_ids_a_github_link_holds() {
        let sha256 = "0123456789abcdef".repeat(4);
        for url in [
            "https://github.com/o/r/pull/123#discussion_r1234567890123".to_string(),
            "https://github.com/o/r/pull/123#issuecomment-71015707501234".to_string(),
            "https://github.com/o/r/pull/123#pullrequestreview-12345678901234".to_string(),
            format!("https://github.com/o/r/pull/123/files#diff-{sha256}"),
            format!("https://github.com/o/r/pull/123/files#diff-{sha256}R12"),
            format!("https://github.com/o/r/pull/123/files#diff-{sha256}L3-R20"),
            format!(
                "https://github.com/o/r/pull/123/files#diff-{}",
                "abcdef0123456789".repeat(4)
            ),
            "https://github.com/o/r/compare/0123456789abcdef0123456789abcdef01234567...main"
                .to_string(),
            "https://github.com/o/r/commit/abcdef0123456789abcdef0123456789abcdef01".to_string(),
            "https://github.com/o/r/pull/1/commits/abcdef0123456789abcdef0123456789abcdef01"
                .to_string(),
            "https://github.com/o/r/blame/0123456789abcdef0123456789abcdef01234567/a.rs#L3-L9"
                .to_string(),
            format!("https://github.com/o/r/tree/{sha256}/docs"),
        ] {
            assert_eq!(for_comment(&url), url);
        }
    }

    #[test]
    fn for_comment_hides_a_key_glued_to_a_github_link_by_url_characters() {
        for (text, shown) in [
            (
                "curl -d \"pr=https://github.com/o/r/pull/1&token=KEY\"",
                "curl -d \"pr=https://github.com/o/r/pull/1&token=•••\"",
            ),
            (
                "https://github.com/o/r/pull/1&access_token=KEY",
                "https://github.com/o/r/pull/1&access_token=•••",
            ),
            (
                "repo=https://github.com/o/r&api_key=KEY",
                "repo=https://github.com/o/r&api_key=•••",
            ),
            (
                "https://github.com/o/r/pull/1https://hooks.slack.com/services/T0/B0/KEY",
                "https://github.com/o/r/pull/1https://hooks.slack.com/services/T0/B0/•••",
            ),
            (
                "https://github.com/o/r/pull/1%0AGH_TOKEN%3DKEY",
                "https://github.com/o/r/pull/1%0AGH_TOKEN%•••",
            ),
            (
                "https://github.com/o/r/pull/1#access_token=KEY",
                "https://github.com/o/r/pull/1#access_token=•••",
            ),
            (
                "https://github.com/o/r/pull/1.KEY",
                "https://github.com/o/r/pull/1.•••",
            ),
            (
                "https://github.com/o/r/pull/1/KEY",
                "https://github.com/o/r/pull/1/•••",
            ),
            (
                "https://github.com/o/r/pull/1#ghp_AbCdEf0123456789AbCdEf0123456789",
                "https://github.com/o/r/pull/1#•••",
            ),
        ] {
            assert_eq!(for_comment(&text.replace("KEY", KEY)), shown, "{text}");
        }
    }

    #[test]
    fn for_comment_hides_a_hex_key_in_a_github_link_where_no_commit_id_goes() {
        let hex40 = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b";
        let hex64 = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        for hex in [hex40, hex64] {
            for (text, shown) in [
                (
                    "https://github.com/o/r/pull/1?access_token=HEX",
                    "https://github.com/o/r/pull/1?access_token=•••",
                ),
                (
                    "curl -d \"pr=https://github.com/o/r/pull/1&dd_app_key=HEX\"",
                    "curl -d \"pr=https://github.com/o/r/pull/1&dd_app_key=•••\"",
                ),
                (
                    "https://github.com/o/r/pull/1#secret=HEX",
                    "https://github.com/o/r/pull/1#secret=•••",
                ),
                (
                    "https://github.com/o/r/pull/1.HEX",
                    "https://github.com/o/r/pull/1.•••",
                ),
                (
                    "https://github.com/o/r/pull/1/HEX",
                    "https://github.com/o/r/pull/1/•••",
                ),
                (
                    "https://github.com/o/r/pull/1https://hooks.example.com/x/HEX",
                    "https://github.com/o/r/pull/1https://hooks.example.com/x/•••",
                ),
                (
                    "https://github.com/o/r/pull/1?x=HEXL12",
                    "https://github.com/o/r/pull/1?x=•••",
                ),
                (
                    "https://github.com/o/r/commit/HEX&token=HEX",
                    "https://github.com/o/r/commit/•••&token=•••",
                ),
                (
                    "https://github.com/o/r/commit/tokenHEX",
                    "https://github.com/o/r/commit/•••",
                ),
                (
                    "https://github.com/o/r/pull/1#HEX",
                    "https://github.com/o/r/pull/1#•••",
                ),
            ] {
                assert_eq!(for_comment(&text.replace("HEX", hex)), shown, "{text}");
            }
        }
    }

    #[test]
    fn for_comment_keeps_only_whole_github_anchors_and_commit_ids() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let hex32 = "0123456789abcdef".repeat(2);
        for (text, shown) in [
            (
                format!("https://github.com/o/r/commit/{hex32}"),
                "https://github.com/o/r/commit/•••",
            ),
            (
                format!("https://github.com/o/r/pull/1#diff-{sha}R"),
                "https://github.com/o/r/pull/1#•••",
            ),
            (
                format!("https://github.com/o/r/pull/1#diff-{sha}X12"),
                "https://github.com/o/r/pull/1#•••",
            ),
            (
                format!("https://github.com/o/r/pull/1#diff-{hex32}"),
                "https://github.com/o/r/pull/1#•••",
            ),
            (
                "https://github.com/o/r/pull/1#ABCDEFGHIJKLMNOPQRSTUVWL123".to_string(),
                "https://github.com/o/r/pull/1#•••",
            ),
            (
                "https://github.com/o/r/pull/1#issuecomment-1234567890abcdefghijk".to_string(),
                "https://github.com/o/r/pull/1#•••",
            ),
            (
                format!("https://github.com/o/r/pull/1#issuecomment-{sha}x"),
                "https://github.com/o/r/pull/1#•••",
            ),
            (
                format!("https://github.com/o/r/pull/1#discussion_r1&{KEY}"),
                "https://github.com/o/r/pull/1#discussion_r1&•••",
            ),
            (
                format!("https://github.com/o/r/blob/main/docs/{sha}/x"),
                "https://github.com/o/r/blob/main/docs/•••/x",
            ),
            (
                format!(
                    "https://github.com/o/r/pull/1/files/{}{}",
                    "a".repeat(99),
                    "1".repeat(30)
                ),
                "https://github.com/o/r/pull/1/files/•••",
            ),
        ] {
            assert_eq!(for_comment(&text), shown, "{text}");
        }
        let compare = format!("https://github.com/o/r/compare/{KEY}...{sha}");
        assert_eq!(
            for_comment(&compare),
            format!("https://github.com/o/r/compare/•••...{sha}")
        );
    }

    #[test]
    fn for_comment_ends_a_github_link_before_a_character_no_url_holds() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        for end in [
            " ", "\t", "\n", "(", ")", "[", "]", "{", "}", "<", ">", "`", "\"", "'", "|", "\\",
            "^", ",", ";", "\u{200b}", "\u{1b}", "\u{3067}", "\u{2192}",
        ] {
            let text = format!("https://github.com/o/r{end}/commit/{sha}");
            assert_eq!(
                for_comment(&text),
                format!("https://github.com/o/r{end}/commit/•••"),
                "{text:?}"
            );
        }
    }

    #[test]
    fn for_comment_keeps_github_ids_whole_only_on_github_com() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        for host in [
            "notgithub.com",
            "github.com.evil.example",
            "gist.github.com",
            "github.com:443",
            "github.com@evil.example",
        ] {
            let url = format!("https://{host}/o/r/commit/{sha}");
            let shown = for_comment(&url);
            assert!(shown.ends_with("/o/r/commit/•••"), "{url} -> {shown}");
        }
        let url = format!("https://github.com/o/r/commit/{sha}");
        assert_eq!(for_comment(&url), url);
    }

    #[test]
    fn for_comment_ends_a_url_at_a_character_no_url_holds() {
        for (before, after) in [
            ("https://a.example\\nKEY=", ""),
            ("https://github.com/o/r/pull/1\u{200b}", ""),
            ("https://github.com/o/r/pull/1\u{1b}", ""),
            ("https://github.com/o/r/pull/1\u{2192}", ""),
            ("https://github.com/o/r/pull/1\\", ""),
            ("https://github.com/o/r/pull/1^", ""),
            ("https://github.com/o/r/pull/1\t", ""),
            ("https://github.com/o/r/pull/1 ", ""),
            ("https://github.com/o/r/pull/1|", ""),
            ("https://github.com/o/r/pull/1}", ""),
            ("https://github.com/o/r/pull/1'", ""),
            (
                "PR\u{306f}https://github.com/o/r/pull/1\u{3067}\u{3059}\u{3002}",
                "\u{3067}\u{3059}",
            ),
            ("endpoint=https://api.example.com,key=", ""),
            ("https://github.com/o/r/pull/1,GH_TOKEN=", ""),
            ("https://github.com/o/r/pull/1;", ""),
            ("`https://github.com/o/r/pull/1`", ""),
            ("\"https://github.com/o/r/pull/1\"", ""),
            ("[https://github.com/o/r/pull/1]", ""),
            ("(https://github.com/o/r/pull/1)", ""),
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
        let nested = "https://example.com/login?next=https://bot:hunter2@github.com/o/r";
        assert_eq!(
            for_comment(nested),
            "https://example.com/login?next=https://***@github.com/o/r"
        );
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
