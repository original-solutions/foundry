//! Stands in for Claude Code in tests. It reads a script of steps and acts them out: waits for a
//! line of input, sleeps, prints, and fires hooks the way Claude Code does, by running every
//! command registered for the event in the settings file with the payload on stdin. Used only by
//! tests.
//!
//! ```yaml
//! - wait_input                          # read one line from stdin; EOF carries on
//! - wait_prompt                         # take one prompt as Claude Code does (below)
//! - hook: {event: PreToolUse, tool_name: Bash}   # extra keys join the payload
//! - stop: {last_assistant_message: "done"}       # fires Stop with that message
//! - notify: "may I?"                    # fires Notification with that message
//! - wait_line: ""                       # read one line or paste, report no hook (below)
//! - wait_line: "/factory-build"         # read lines until one starts with that text
//! - sleep_ms: 200
//! - print: "some text"
//! - print_env: ANTHROPIC_AUTH_TOKEN     # prints NAME=value, or "NAME unset"
//! - print_args                          # one ARG=<argument> line per argument
//! - exit: 3
//! ```
//!
//! The end of the script exits 0.
//!
//! `wait_prompt` acts out Claude Code's input box: it turns bracketed paste on, as Claude Code
//! does once its box takes keys, then reads one prompt, a bracketed paste and the Enter after
//! it, or else one typed line. It prints `PROMPT=` and the prompt as a JSON string, and fires
//! `UserPromptSubmit` with it as `prompt`.
//!
//! `wait_line` reads a prompt as `wait_prompt` does and prints `LINE=` and it as a JSON string,
//! but fires no hook: it stands for a line Claude Code never reported as a prompt. With a
//! non-empty argument it keeps reading, printing `LINE=` for each line, until one starts with
//! that text. The end of the input ends the wait.

use std::collections::BTreeMap;
use std::io::{BufRead as _, Write as _};
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use clap::Parser;
use serde::Deserialize;

/// Command line: the script, and the settings file Claude Code would have been given. The
/// model is accepted and ignored.
#[derive(Parser)]
struct Cli {
    #[arg(long)]
    script: PathBuf,
    #[arg(long)]
    settings: PathBuf,
    #[arg(long)]
    #[allow(dead_code)]
    model: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Step {
    WaitInput,
    WaitPrompt,
    Stop {
        last_assistant_message: String,
    },
    Notify(String),
    WaitLine(String),
    Hook {
        event: String,
        #[serde(flatten)]
        extra: BTreeMap<String, serde_json::Value>,
    },
    SleepMs(u64),
    Print(String),
    PrintEnv(String),
    PrintArgs,
    Exit(u8),
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            eprintln!("foundry-fake-agent: {message}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<u8, String> {
    let script = std::fs::read_to_string(&cli.script)
        .map_err(|e| format!("cannot read {}: {e}", cli.script.display()))?;
    let steps: Vec<Step> = serde_yaml_ng::with::singleton_map_recursive::deserialize(
        serde_yaml_ng::Deserializer::from_str(&script),
    )
    .map_err(|e| format!("cannot parse {}: {e}", cli.script.display()))?;
    let settings: serde_json::Value = std::fs::read(&cli.settings)
        .map_err(|e| format!("cannot read {}: {e}", cli.settings.display()))
        .and_then(|bytes| {
            serde_json::from_slice(&bytes)
                .map_err(|e| format!("cannot parse {}: {e}", cli.settings.display()))
        })?;

    for step in steps {
        match step {
            Step::WaitInput => {
                let mut line = String::new();
                let _ = std::io::stdin().lock().read_line(&mut line);
            }
            Step::WaitPrompt => {
                print!("\x1b[?2004h");
                let _ = std::io::stdout().flush();
                let prompt = read_prompt().unwrap_or_default();
                say(&format!(
                    "PROMPT={}",
                    serde_json::Value::from(prompt.as_str())
                ));
                let extra = BTreeMap::from([("prompt".to_string(), prompt.into())]);
                fire(&settings, "UserPromptSubmit", extra);
            }
            Step::Stop {
                last_assistant_message,
            } => {
                let extra = BTreeMap::from([(
                    "last_assistant_message".to_string(),
                    last_assistant_message.into(),
                )]);
                fire(&settings, "Stop", extra);
            }
            Step::Notify(message) => {
                let extra = BTreeMap::from([("message".to_string(), message.into())]);
                fire(&settings, "Notification", extra);
            }
            Step::WaitLine(start) => {
                while let Some(line) = read_prompt() {
                    say(&format!("LINE={}", serde_json::Value::from(line.as_str())));
                    if line.starts_with(&start) {
                        break;
                    }
                }
            }
            Step::Hook { event, extra } => fire(&settings, &event, extra),
            Step::SleepMs(ms) => std::thread::sleep(Duration::from_millis(ms)),
            Step::Print(text) => say(&text),
            Step::PrintEnv(name) => match std::env::var(&name) {
                Ok(value) => say(&format!("{name}={value}")),
                Err(_) => say(&format!("{name} unset")),
            },
            Step::PrintArgs => {
                for arg in std::env::args().skip(1) {
                    say(&format!("ARG={arg}"));
                }
            }
            Step::Exit(code) => return Ok(code),
        }
    }
    Ok(0)
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// One prompt from stdin, as Claude Code's input box takes it: a bracketed paste with its
/// newlines kept, up to the Enter after it, or else one typed line, which Enter ends. The
/// terminal is left as it is, so Enter arrives as a newline. `None` at the end of the input.
fn read_prompt() -> Option<String> {
    let mut stdin = std::io::stdin().lock();
    let mut read = Vec::new();
    if stdin.read_until(b'\n', &mut read).unwrap_or(0) == 0 {
        return None;
    }
    let Some(pasted) = read.strip_prefix(PASTE_START) else {
        return Some(
            String::from_utf8_lossy(&read)
                .trim_end_matches(['\r', '\n'])
                .to_string(),
        );
    };
    let mut pasted = pasted.to_vec();
    loop {
        if let Some(end) = pasted.windows(PASTE_END.len()).position(|w| w == PASTE_END) {
            pasted.truncate(end);
            return Some(String::from_utf8_lossy(&pasted).into_owned());
        }
        let mut more = Vec::new();
        if stdin.read_until(b'\n', &mut more).unwrap_or(0) == 0 {
            return Some(String::from_utf8_lossy(&pasted).into_owned());
        }
        pasted.extend(more);
    }
}

/// Prints a line and flushes it.
fn say(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{text}");
    let _ = out.flush();
}

/// Runs each command hook registered for `event`, with the payload on its stdin, and waits for
/// it. A command that fails is reported on stderr and the script carries on.
fn fire(settings: &serde_json::Value, event: &str, extra: BTreeMap<String, serde_json::Value>) {
    let mut payload = serde_json::Map::new();
    payload.insert("hook_event_name".into(), event.into());
    payload.insert("session_id".into(), "fake-session".into());
    let cwd = std::env::current_dir().unwrap_or_default();
    payload.insert("cwd".into(), cwd.to_string_lossy().into_owned().into());
    payload.extend(extra);
    let payload = serde_json::Value::Object(payload).to_string();

    let entries = settings["hooks"][event].as_array().into_iter().flatten();
    for hook in entries.flat_map(|e| e["hooks"].as_array().into_iter().flatten()) {
        if hook["type"] != "command" {
            continue;
        }
        let Some(command) = hook["command"].as_str() else {
            continue;
        };
        if let Err(message) = send(command, &payload) {
            eprintln!("foundry-fake-agent: hook {event}: {message}");
        }
    }
}

fn send(command: &str, payload: &str) -> Result<(), String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run sh: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A command that does not read its stdin closes it early; that is not an error here.
        let _ = stdin.write_all(payload.as_bytes());
    }
    let status = child.wait().map_err(|e| format!("cannot wait: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("the command {status}"))
    }
}
