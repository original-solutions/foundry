//! What a session is for and what it is doing, read from its hooks' payloads: the first prompt
//! it was given, and the newest prompt, tool call or question. Kept in memory for the session's
//! life and never written to a record, so it does not sync and does not outlive the daemon.
//!
//! Prompts and commands can hold anything a person typed, so every text is cut to one short
//! line, the secrets this daemon knows are hidden, and so is any word shaped like a token.

use serde::Serialize;

/// The longest text kept, in characters, before an ellipsis.
const LONGEST: usize = 120;
/// The shortest run of letters, digits, `-` and `_` taken for a token when it mixes letters and
/// digits.
const TOKEN_LENGTH: usize = 24;
/// What a hidden secret or token shows as.
pub const HIDDEN: &str = "•••";

/// A session's brief, as the UI is sent it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Brief {
    /// The first prompt since the session started or was cleared, slash commands left out.
    pub title: Option<String>,
    /// The newest step of the turn, until the turn ends.
    pub now: Option<Step>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Step {
    pub kind: StepKind,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// A prompt was submitted.
    Prompt,
    /// A tool is about to run.
    Tool,
    /// Claude Code asks the person something, as its notification says.
    Ask,
}

impl Brief {
    /// Takes in one hook payload. `secrets` are hidden wherever they appear.
    pub fn note(&mut self, payload: &serde_json::Value, secrets: &[&str]) {
        let field = |name: &str| payload.get(name).and_then(|v| v.as_str());
        let step = |kind, text: &str| {
            let text = shown(text, secrets);
            (!text.is_empty()).then_some(Step { kind, text })
        };
        match field("hook_event_name").unwrap_or_default() {
            "SessionStart" if field("source") == Some("clear") => *self = Self::default(),
            "UserPromptSubmit" => {
                let prompt = field("prompt").unwrap_or_default();
                if self.title.is_none() && !prompt.trim_start().starts_with('/') {
                    self.title = step(StepKind::Prompt, prompt).map(|s| s.text);
                }
                self.now = step(StepKind::Prompt, prompt);
            }
            "PreToolUse" => {
                let text = tool(payload);
                self.now = step(StepKind::Tool, &text);
            }
            "Notification" => {
                if let Some(message) = field("message") {
                    self.now = step(StepKind::Ask, message);
                }
            }
            "Stop" | "SessionEnd" => self.now = None,
            _ => {}
        }
    }
}

/// A tool call as one line: the tool and what it acts on, a path shown from the session's
/// folder.
fn tool(payload: &serde_json::Value) -> String {
    let name = payload
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("tool");
    let input = |key: &str| {
        payload
            .get("tool_input")
            .and_then(|i| i.get(key))
            .and_then(|v| v.as_str())
    };
    let cwd = payload.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
    let path = |p: &str| {
        let inside = p
            .strip_prefix(cwd)
            .filter(|_| !cwd.is_empty())
            .and_then(|rest| rest.strip_prefix('/'));
        inside.unwrap_or(p).to_string()
    };
    let detail = match name {
        "Bash" => input("command").map(|c| format!("$ {c}")),
        "Grep" | "Glob" => input("pattern").map(|p| format!("{name} {p}")),
        "WebFetch" => input("url").map(|u| format!("Fetch {u}")),
        "WebSearch" => input("query").map(|q| format!("Search {q}")),
        "Task" | "Agent" => input("description").map(|d| format!("Agent: {d}")),
        _ => input("file_path")
            .or_else(|| input("notebook_path"))
            .or_else(|| input("path"))
            .map(|p| format!("{name} {}", path(p))),
    };
    detail.unwrap_or_else(|| name.to_string())
}

/// `text` on one line, with `secrets` and token-shaped words hidden, cut to [`LONGEST`].
pub fn shown(text: &str, secrets: &[&str]) -> String {
    let mut line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        line = line.replace(secret, HIDDEN);
    }
    let line = tokens_hidden(&line);
    if line.chars().count() <= LONGEST {
        return line;
    }
    let cut: String = line.chars().take(LONGEST - 1).collect();
    format!("{}…", cut.trim_end())
}

/// `text` with each run of letters, digits, `-` and `_` that is at least [`TOKEN_LENGTH`] long
/// and mixes letters and digits hidden: API keys, bearer tokens and hashes look like that, words
/// and paths do not.
fn tokens_hidden(text: &str) -> String {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        let letters = run.chars().any(|c| c.is_ascii_alphabetic());
        let digits = run.chars().any(|c| c.is_ascii_digit());
        if run.len() >= TOKEN_LENGTH && letters && digits {
            out.push_str(HIDDEN);
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in text.chars() {
        if word(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn noted(payloads: &[serde_json::Value]) -> Brief {
        let mut brief = Brief::default();
        for p in payloads {
            brief.note(p, &["sk-gateway"]);
        }
        brief
    }

    fn prompt(text: &str) -> serde_json::Value {
        json!({"hook_event_name": "UserPromptSubmit", "prompt": text})
    }

    fn tool_use(name: &str, input: serde_json::Value) -> serde_json::Value {
        json!({"hook_event_name": "PreToolUse", "cwd": "/w/tidy", "tool_name": name, "tool_input": input})
    }

    fn now(brief: &Brief) -> Option<(StepKind, &str)> {
        brief.now.as_ref().map(|s| (s.kind, s.text.as_str()))
    }

    #[test]
    fn the_first_prompt_is_the_title_and_the_newest_is_now() {
        let brief = noted(&[
            prompt("fix the login\nbug please"),
            prompt("and add a test"),
        ]);
        assert_eq!(brief.title.as_deref(), Some("fix the login bug please"));
        assert_eq!(now(&brief), Some((StepKind::Prompt, "and add a test")));
    }

    #[test]
    fn a_slash_command_is_never_the_title() {
        let brief = noted(&[prompt("/model opus"), prompt("tidy the readme")]);
        assert_eq!(brief.title.as_deref(), Some("tidy the readme"));
    }

    #[test]
    fn clear_forgets_the_title_and_other_session_starts_do_not() {
        let start = |source| json!({"hook_event_name": "SessionStart", "source": source});
        let brief = noted(&[prompt("one"), start("compact"), start("resume")]);
        assert_eq!(brief.title.as_deref(), Some("one"));
        let brief = noted(&[prompt("one"), start("clear"), prompt("two")]);
        assert_eq!(brief.title.as_deref(), Some("two"));
    }

    #[test]
    fn a_tool_call_names_the_tool_and_what_it_acts_on() {
        let cases = [
            (
                tool_use("Edit", json!({"file_path": "/w/tidy/README.md"})),
                "Edit README.md",
            ),
            (
                tool_use("Read", json!({"file_path": "/elsewhere/x.rs"})),
                "Read /elsewhere/x.rs",
            ),
            (
                tool_use("Bash", json!({"command": "npm test\n  -- --watch"})),
                "$ npm test -- --watch",
            ),
            (
                tool_use("Grep", json!({"pattern": "fn main"})),
                "Grep fn main",
            ),
            (
                tool_use("WebFetch", json!({"url": "https://example.com/a"})),
                "Fetch https://example.com/a",
            ),
            (
                tool_use("Task", json!({"description": "find the bug"})),
                "Agent: find the bug",
            ),
            (tool_use("TodoWrite", json!({"todos": []})), "TodoWrite"),
            (json!({"hook_event_name": "PreToolUse"}), "tool"),
        ];
        for (payload, want) in cases {
            assert_eq!(now(&noted(&[payload])), Some((StepKind::Tool, want)));
        }
    }

    #[test]
    fn a_notification_is_an_ask_and_the_end_of_a_turn_clears_now() {
        let ask = json!({"hook_event_name": "Notification", "message": "Claude needs your permission to use Bash"});
        let brief = noted(&[prompt("go"), ask.clone()]);
        assert_eq!(
            now(&brief),
            Some((StepKind::Ask, "Claude needs your permission to use Bash"))
        );
        let stop = json!({"hook_event_name": "Stop"});
        let brief = noted(&[prompt("go"), ask, stop]);
        assert_eq!(brief.now, None);
        assert_eq!(brief.title.as_deref(), Some("go"));
    }

    #[test]
    fn post_tool_use_and_unknown_hooks_leave_the_brief_alone() {
        let brief = noted(&[
            tool_use("Edit", json!({"file_path": "/w/tidy/a.md"})),
            json!({"hook_event_name": "PostToolUse", "tool_name": "Edit"}),
            json!({"hook_event_name": "SubagentStop"}),
            json!({"no": "name"}),
        ]);
        assert_eq!(now(&brief), Some((StepKind::Tool, "Edit a.md")));
    }

    #[test]
    fn known_secrets_and_token_shaped_words_are_hidden() {
        let brief = noted(&[prompt("use sk-gateway and abcdefghij0123456789ABCDEF here")]);
        assert_eq!(brief.title.as_deref(), Some("use ••• and ••• here"));
        let payload = tool_use(
            "Bash",
            json!({"command": "curl -H 'Authorization: Bearer 3f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c' x"}),
        );
        assert_eq!(
            now(&noted(&[payload])),
            Some((StepKind::Tool, "$ curl -H 'Authorization: Bearer •••' x"))
        );
    }

    #[test]
    fn words_and_paths_are_not_taken_for_tokens() {
        let text = "internationalization_of_everything src/components/VeryLongComponentName.tsx v2";
        assert_eq!(shown(text, &[]), text);
    }

    #[test]
    fn a_long_text_is_cut_to_one_short_line() {
        let long = "word ".repeat(60);
        let cut = shown(&long, &[]);
        assert_eq!(cut.chars().count(), LONGEST);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn an_empty_prompt_leaves_no_title() {
        let brief = noted(&[prompt("   ")]);
        assert_eq!(brief.title, None);
        assert_eq!(brief.now, None);
    }
}
