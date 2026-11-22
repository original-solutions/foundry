//! What Claude Code reads: the settings file that registers the hooks, the state file where
//! a folder is marked trusted so the trust prompt does not stop a session, and the keys that
//! type a session's first prompt.

use std::ffi::OsString;
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::SessionError;

/// The events registered. Claude Code calls each with the event as JSON on stdin.
const EVENTS: [&str; 7] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "Stop",
    "SessionEnd",
];

/// The events that take a tool matcher.
const TOOL_EVENTS: [&str; 2] = ["PreToolUse", "PostToolUse"];

/// What Claude Code writes once its screen takes keys: it turns bracketed paste on. The input
/// box turns it on just after `SessionStart`, but a screen before the session starts, such as
/// the trust prompt, turns it on too, so the sign counts only with `SessionStart`.
pub const TAKES_INPUT: &[u8] = b"\x1b[?2004h";

/// How long after `SessionStart` a first prompt waits when the only sign that Claude Code takes
/// input came before it, from an earlier screen. In a probe of 2.1.322 the input box turned
/// bracketed paste on 0.19 s after the hook ran, and the contract test checks the sign is on the
/// screen within half of this after it sees `SessionStart`.
pub const EARLIER_SIGN_GRACE: Duration = Duration::from_secs(1);

/// Whether `output`, read after `tail`, turns bracketed paste on ([`TAKES_INPUT`]). `tail` keeps
/// the end of what was read, so a sign cut between two reads is still found.
pub fn turns_paste_on(tail: &mut Vec<u8>, output: &[u8]) -> bool {
    tail.extend_from_slice(output);
    if tail.windows(TAKES_INPUT.len()).any(|w| w == TAKES_INPUT) {
        tail.clear();
        return true;
    }
    tail.drain(..tail.len().saturating_sub(TAKES_INPUT.len() - 1));
    false
}

/// The keys that give Claude Code `prompt` as one prompt, as a person who pastes it and
/// presses Enter: a bracketed paste, so its newlines stay in the prompt, then Enter. An escape
/// in `prompt` is dropped, so it cannot end the paste early and type the rest as keys.
pub fn typed_prompt(prompt: &str) -> Vec<u8> {
    let mut keys = b"\x1b[200~".to_vec();
    keys.extend(prompt.bytes().filter(|&b| b != 0x1b));
    keys.extend_from_slice(b"\x1b[201~\r");
    keys
}

/// `text` in single quotes for `sh`, each `'` in it written as `'\''`.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The settings that register a command hook for each event. The command posts the event on
/// its stdin with the URL and headers in the curl config file `curl_config`, so the token is
/// in a private file and never in a command line, which other users on the machine can read.
///
/// `-q` comes first so curl reads no `~/.curlrc`, and the config turns every proxy off, so
/// neither the user's curl settings nor a proxy variable in the session's environment can send
/// the token anywhere but this daemon.
///
/// The command's stdout goes to /dev/null, because Claude Code adds the stdout of some hooks
/// to the model's context. It ends in `|| true`, because a hook that exits 2 blocks the tool
/// call, and a post that fails must never stop the agent. While the daemon is down a post
/// holds the agent up for a few seconds: see [`write_curl_config`].
pub fn settings_json(curl_config: &Path) -> Value {
    let command = format!(
        "curl -q -sS -m 5 -K {} -X POST --data-binary @- >/dev/null 2>&1 || true",
        shell_quote(&curl_config.to_string_lossy())
    );
    let hooks: Map<String, Value> = EVENTS
        .iter()
        .map(|event| {
            let mut entry = json!({"hooks": [{"type": "command", "command": command}]});
            if TOOL_EVENTS.contains(event) {
                entry["matcher"] = json!("*");
            }
            ((*event).to_string(), json!([entry]))
        })
        .collect();
    json!({ "hooks": hooks })
}

/// Creates a new file at `path`, readable by this user only, holding `bytes`. An existing file
/// is an error.
fn create_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Writes `bytes` to a new private file at `path`, in a folder made private too.
fn write_new_private(path: &Path, bytes: &[u8]) -> Result<(), SessionError> {
    let io = |step: &str| {
        let what = format!("{}: {step}", path.display());
        move |source| SessionError::Io { what, source }
    };
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(io("create the folder"))?;
    }
    create_private(path, bytes).map_err(io("write the file"))
}

/// Writes `value` to a new file at `path`, readable by this user only, in a folder made
/// readable by this user only. An existing file is an error.
pub fn write_settings(path: &Path, value: &Value) -> Result<(), SessionError> {
    write_new_private(path, value.to_string().as_bytes())
}

/// Writes the curl config a session's hooks read: the headers, with the bearer `token`, the
/// `url` to post to, and no proxy for any host. Same file rules as [`write_settings`]. `url`
/// and `token` sit inside double quotes, so they must hold none: a hex token and
/// `http://127.0.0.1:<port>/hooks/<uuid>` never do.
///
/// A hook posted while the daemon restarts is refused, so curl tries again each second for up
/// to [`HOOK_RETRY_SECS`] seconds: a restart that short loses no hook. While the daemon stays
/// down, each hook holds the agent up for that long.
pub fn write_curl_config(path: &Path, url: &str, token: &str) -> Result<(), SessionError> {
    debug_assert!(!url.contains(['"', '\\', '\n']) && !token.contains(['"', '\\', '\n']));
    let config = format!(
        "header = \"Content-Type: application/json\"\n\
         header = \"Authorization: Bearer {token}\"\n\
         url = \"{url}\"\n\
         noproxy = \"*\"\n\
         retry = {HOOK_RETRY_SECS}\n\
         retry-connrefused\n\
         retry-delay = 1\n\
         retry-max-time = {HOOK_RETRY_SECS}\n"
    );
    write_new_private(path, config.as_bytes())
}

/// How long a hook's curl tries again while nothing answers, in seconds.
pub const HOOK_RETRY_SECS: u32 = 10;

/// Replaces the curl config at `path` with one [`write_curl_config`] writes for `url` and
/// `token`. The new file is written beside it and renamed over it, so a hook reading it at
/// that moment reads the old config or the new one, never a part.
pub fn rewrite_curl_config(path: &Path, url: &str, token: &str) -> Result<(), SessionError> {
    let new = path.with_extension("curl.new");
    // One left by a write that stopped part way would refuse this one.
    let _ = std::fs::remove_file(&new);
    write_curl_config(&new, url, token)?;
    std::fs::rename(&new, path).map_err(|source| SessionError::Io {
        what: format!("{}: replace the file", path.display()),
        source,
    })
}

/// The bearer token in the curl config [`write_curl_config`] wrote at `path`, or `None` when
/// the file cannot be read or holds no `Authorization: Bearer` header line.
pub fn curl_token(path: &Path) -> Option<String> {
    let config = std::fs::read_to_string(path).ok()?;
    config.lines().find_map(|line| {
        line.strip_prefix("header = \"Authorization: Bearer ")?
            .strip_suffix('"')
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
    })
}

/// [`mark_trusted`] while holding `lock`, so two marks made by this process never read the file
/// at the same time and one overwrite the other.
pub fn mark_trusted_locked(
    lock: &std::sync::Mutex<()>,
    claude_json: &Path,
    worktree: &Path,
) -> Result<(), SessionError> {
    let _held = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    mark_trusted(claude_json, worktree)
}

/// How often a merge is redone from the file's new bytes when Claude Code wrote the file while
/// it was being merged.
const MERGE_TRIES: usize = 3;

/// Marks `worktree` as trusted in Claude Code's state file: `projects.<worktree>.
/// hasTrustDialogAccepted` is `true`. Without it Claude Code asks, with "No, exit" the default.
///
/// The mark stays after the session ends: a worktree is used by session after session.
///
/// The key is the canonical path of `worktree`, which Claude Code looks up. A file that already
/// trusts the folder is not written at all, as Claude Code rewrites it all the time. Otherwise
/// every other key stays, and the file is replaced by a rename so Claude Code never reads half
/// of it; a symlinked file has its target replaced and stays a link. A file that is not JSON,
/// or whose `projects` or entry is not an object, is an error and is left alone. The error
/// names the file and never quotes it, as it can hold credentials.
///
/// Claude Code writes the file too, so just before the rename the file is read again, and a merge
/// made from bytes that have changed is made again from the new ones, up to [`MERGE_TRIES`] times.
pub fn mark_trusted(claude_json: &Path, worktree: &Path) -> Result<(), SessionError> {
    mark_trusted_with(claude_json, worktree, |_| {})
}

/// [`mark_trusted`], calling `before_rename` with the try's number (from 1) after the merged
/// file is written and before it is read again: a test's stand-in for Claude Code writing the
/// file at that moment.
fn mark_trusted_with(
    claude_json: &Path,
    worktree: &Path,
    mut before_rename: impl FnMut(usize),
) -> Result<(), SessionError> {
    let io = |what: String| move |source| SessionError::Io { what, source };
    let key = std::fs::canonicalize(worktree)
        .map_err(io(format!("{}: resolve the worktree", worktree.display())))?;
    let key = key.to_str().ok_or_else(|| SessionError::Io {
        what: format!("{}: the worktree path is not UTF-8", worktree.display()),
        source: io::ErrorKind::InvalidData.into(),
    })?;
    let unusable = |reason: String| SessionError::ClaudeState {
        path: claude_json.to_path_buf(),
        reason,
    };
    let trusted =
        |state: &Value| state["projects"][key]["hasTrustDialogAccepted"] == Value::Bool(true);

    // A symlink is followed to the file it names, so the link stays.
    let target = match std::fs::canonicalize(claude_json) {
        Ok(target) => target,
        Err(e) if e.kind() == io::ErrorKind::NotFound => claude_json.to_path_buf(),
        Err(e) => return Err(io(format!("{}: resolve", claude_json.display()))(e)),
    };
    let read = || match std::fs::read(&target) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(format!("{}: read", target.display()))(e)),
    };
    let write_err = |step: &str| io(format!("{}: {step}", target.display()));

    for attempt in 1..=MERGE_TRIES {
        let Some(bytes) = read()? else {
            let state = json!({"projects": {key: {"hasTrustDialogAccepted": true}}});
            return create_private(&target, state.to_string().as_bytes())
                .map_err(write_err("create"));
        };

        let mut state: Value = serde_json::from_slice(&bytes).map_err(|e| {
            unusable(format!(
                "not valid JSON (line {}, column {})",
                e.line(),
                e.column()
            ))
        })?;
        if trusted(&state) {
            return Ok(());
        }
        let root = state
            .as_object_mut()
            .ok_or_else(|| unusable("the top level is not an object".into()))?;
        let projects = root
            .entry("projects")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| unusable("`projects` is not an object".into()))?;
        let entry = projects
            .entry(key)
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| unusable("the entry for the worktree is not an object".into()))?;
        entry.insert("hasTrustDialogAccepted".into(), Value::Bool(true));

        let mode = std::fs::metadata(&target)
            .map_err(write_err("read the mode"))?
            .permissions()
            .mode()
            & 0o7777;
        let dir = target.parent().unwrap_or(Path::new("."));
        let temp = dir.join(format!(".claude.json.foundry-{}.tmp", uuid::Uuid::new_v4()));
        let written = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&temp)?;
            file.write_all(serde_json::to_string_pretty(&state)?.as_bytes())?;
            file.sync_all()?;
            // `mode` above is cut by the umask; the file's own mode is what is kept.
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode))?;
            before_rename(attempt);
            // Claude Code rewrites the file too. When it did while this merge was made, the
            // rename would undo its write, so the merge is made again from the new bytes.
            // The last try goes ahead: the window left is the time between this read and the
            // rename.
            if attempt < MERGE_TRIES && read().ok().flatten().as_deref() != Some(bytes.as_slice()) {
                return Ok(false);
            }
            std::fs::rename(&temp, &target).map(|()| true)
        })();
        match written {
            Ok(true) => return Ok(()),
            Ok(false) => {
                let _ = std::fs::remove_file(&temp);
            }
            Err(source) => {
                let _ = std::fs::remove_file(&temp);
                return Err(SessionError::Io {
                    what: format!("{}: replace", target.display()),
                    source,
                });
            }
        }
    }
    unreachable!("the last try returns")
}

/// Where Claude Code keeps its state: `$CLAUDE_CONFIG_DIR/.claude.json` when that is set and
/// not empty, else `$HOME/.claude.json`. Takes the two variables' values.
pub fn claude_json(config_dir: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let set = |v: Option<OsString>| v.filter(|v| !v.is_empty()).map(PathBuf::from);
    set(config_dir)
        .or_else(|| set(home))
        .map(|dir| dir.join(".claude.json"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use serde_json::{Value, json};

    use super::*;

    const URL: &str = "http://127.0.0.1:4100/hooks/67e55044-10b1-426f-9247-bb680e5fe0c8";
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn read(path: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    fn command(settings: &Value, event: &str) -> String {
        settings["hooks"][event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn bracketed_paste_turned_on_is_found_when_it_is_cut_between_reads() {
        let mut tail = Vec::new();
        assert!(!turns_paste_on(&mut tail, b"drawing \x1b[?20"));
        assert!(turns_paste_on(&mut tail, b"04h box"));

        let mut tail = Vec::new();
        assert!(turns_paste_on(&mut tail, b"\x1b[?2004h"));

        // A long run of output with no sign keeps only what could start one.
        let mut tail = Vec::new();
        assert!(!turns_paste_on(&mut tail, &[b'x'; 10_000]));
        assert!(tail.len() < TAKES_INPUT.len());
        assert!(!turns_paste_on(&mut tail, b"\x1b[?2004l"));
    }

    #[test]
    fn a_prompt_is_typed_as_one_bracketed_paste_then_enter_with_no_escape_in_it() {
        assert_eq!(
            typed_prompt("fix it\nthen \u{1b}[201~\rstop é"),
            "\u{1b}[200~fix it\nthen [201~\rstop é\u{1b}[201~\r".as_bytes()
        );
    }

    #[test]
    fn settings_register_the_seven_events_each_with_one_command() {
        let settings = settings_json(Path::new(CURL));
        let hooks = settings["hooks"].as_object().unwrap();
        let mut names: Vec<&str> = hooks.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "Notification",
                "PostToolUse",
                "PreToolUse",
                "SessionEnd",
                "SessionStart",
                "Stop",
                "UserPromptSubmit"
            ]
        );
        for (event, entries) in hooks {
            let entries = entries.as_array().unwrap();
            assert_eq!(entries.len(), 1, "{event}");
            let commands = entries[0]["hooks"].as_array().unwrap();
            assert_eq!(commands.len(), 1, "{event}");
            assert_eq!(commands[0]["type"], "command");
            match event.as_str() {
                "PreToolUse" | "PostToolUse" => assert_eq!(entries[0]["matcher"], "*"),
                _ => assert!(entries[0].get("matcher").is_none(), "{event}"),
            }
        }
    }

    const CURL: &str = "/data/sessions/67e55044-10b1-426f-9247-bb680e5fe0c8.curl";

    #[test]
    fn the_hook_command_posts_stdin_through_a_curl_config_quietly_and_never_blocks_the_agent() {
        let settings = settings_json(Path::new(CURL));
        let want = format!(
            "curl -q -sS -m 5 -K '{CURL}' -X POST --data-binary @- >/dev/null 2>&1 || true"
        );
        for event in ["SessionStart", "PreToolUse", "SessionEnd"] {
            assert_eq!(command(&settings, event), want);
        }
    }

    #[test]
    fn the_settings_hold_neither_the_token_nor_a_bearer_header() {
        let text = settings_json(Path::new(CURL)).to_string();
        assert!(!text.contains(TOKEN) && !text.contains("Bearer"), "{text}");
    }

    /// Runs a session's hook command through `sh`, with its curl config pointing at a listener
    /// on loopback. A `~/.curlrc` adds a header and names a proxy, and every proxy variable names
    /// it too; nothing listens there. Gives the request the listener got, if any.
    fn hook_request_with_a_curlrc_and_proxies_set() -> Option<String> {
        use std::io::{Read as _, Write as _};
        let dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let curl = dir.path().join("sessions").join("s.curl");
        let url = format!("http://127.0.0.1:{port}/hooks/67e55044-10b1-426f-9247-bb680e5fe0c8");
        write_curl_config(&curl, &url, TOKEN).unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir(&home).unwrap();
        // Nothing listens on port 9, the discard port, on loopback.
        let proxy = "http://127.0.0.1:9";
        std::fs::write(
            home.join(".curlrc"),
            format!("proxy = \"{proxy}\"\nheader = \"X-From-Curlrc: 1\"\n"),
        )
        .unwrap();

        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(command(&settings_json(&curl), "Stop"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", &home)
            .envs(["ALL_PROXY", "all_proxy", "HTTP_PROXY", "http_proxy"].map(|k| (k, proxy)))
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(br#"{"hook_event_name":"Stop"}"#)
            .unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let request = loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                        .unwrap();
                    let mut seen = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !String::from_utf8_lossy(&seen).contains(r#""Stop"}"#) {
                        let n = stream.read(&mut chunk).unwrap();
                        assert!(n > 0, "the request ended early");
                        seen.extend_from_slice(&chunk[..n]);
                    }
                    stream
                        .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                        .unwrap();
                    break Some(String::from_utf8_lossy(&seen).into_owned());
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // curl gave up without calling: give a late connection a moment, then stop.
                    if child.try_wait().unwrap().is_some() {
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        if let Ok((_, _)) = listener.accept() {
                            panic!("a connection came after curl exited");
                        }
                        break None;
                    }
                    assert!(std::time::Instant::now() < deadline, "curl did not finish");
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => panic!("accept: {e}"),
            }
        };
        assert!(child.wait().unwrap().success(), "the hook command fails");
        request
    }

    #[test]
    fn the_hook_command_reads_no_curlrc_and_uses_no_proxy() {
        let request = hook_request_with_a_curlrc_and_proxies_set()
            .expect("the hook reached the daemon straight, not through a proxy");
        assert!(
            request.starts_with("POST /hooks/67e55044-10b1-426f-9247-bb680e5fe0c8 HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(
            request.contains(&format!("Authorization: Bearer {TOKEN}\r\n")),
            "{request}"
        );
        assert!(!request.contains("X-From-Curlrc"), "{request}");
    }

    #[test]
    fn a_path_is_quoted_so_sh_reads_it_back_whatever_it_holds() {
        for path in ["/a/b", "/a b/c", "/it's/x", "/'''/'", "/a\"b$c`d`/e"] {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {}", shell_quote(path)))
                .output()
                .unwrap();
            assert_eq!(String::from_utf8(out.stdout).unwrap(), path);
        }
    }

    #[test]
    fn the_hook_command_quotes_a_config_path_with_a_space_and_a_quote() {
        let settings = settings_json(Path::new("/home/me/it's here/sessions/x.curl"));
        let command = command(&settings, "Stop");
        assert!(
            command.contains(r"-K '/home/me/it'\''s here/sessions/x.curl' "),
            "{command}"
        );
    }

    #[test]
    fn the_curl_config_holds_the_headers_and_the_url_and_is_written_new_with_mode_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions").join("s.curl");

        write_curl_config(&path, URL, TOKEN).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!(
                "header = \"Content-Type: application/json\"\n\
                 header = \"Authorization: Bearer {TOKEN}\"\n\
                 url = \"{URL}\"\n\
                 noproxy = \"*\"\n\
                 retry = 10\n\
                 retry-connrefused\n\
                 retry-delay = 1\n\
                 retry-max-time = 10\n"
            )
        );
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert!(
            write_curl_config(&path, URL, TOKEN).is_err(),
            "a second write"
        );
    }

    #[test]
    fn a_rewritten_curl_config_holds_the_new_url_and_stays_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions").join("s.curl");
        write_curl_config(&path, "http://127.0.0.1:1/hooks/old", TOKEN).unwrap();
        std::fs::write(path.with_extension("curl.new"), "left part way").unwrap();

        rewrite_curl_config(&path, URL, TOKEN).unwrap();

        let fresh = dir.path().join("fresh.curl");
        write_curl_config(&fresh, URL, TOKEN).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            std::fs::read_to_string(&fresh).unwrap()
        );
        assert_eq!(mode(&path), 0o600);
        assert!(!path.with_extension("curl.new").exists());
    }

    #[test]
    fn the_token_is_read_back_from_the_curl_config_and_is_none_with_no_bearer_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions").join("s.curl");
        write_curl_config(&path, URL, TOKEN).unwrap();
        assert_eq!(curl_token(&path).as_deref(), Some(TOKEN));

        let bare = dir.path().join("bare.curl");
        std::fs::write(
            &bare,
            format!("header = \"Content-Type: application/json\"\nurl = \"{URL}\"\n"),
        )
        .unwrap();
        assert_eq!(curl_token(&bare), None);
        let empty = dir.path().join("empty.curl");
        std::fs::write(&empty, "header = \"Authorization: Bearer \"\n").unwrap();
        assert_eq!(curl_token(&empty), None, "an empty bearer value");
        assert_eq!(curl_token(&dir.path().join("missing.curl")), None);
    }

    #[test]
    fn trust_marks_made_at_once_through_one_lock_are_all_kept() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(".claude.json");
        std::fs::write(&file, json!({"keep": 1}).to_string()).unwrap();
        let lock = std::sync::Mutex::new(());
        let worktrees: Vec<_> = (0..8).map(|_| tempfile::tempdir().unwrap()).collect();

        std::thread::scope(|scope| {
            for worktree in &worktrees {
                let (lock, file) = (&lock, &file);
                scope.spawn(move || mark_trusted_locked(lock, file, worktree.path()).unwrap());
            }
        });

        let got = read(&file);
        assert_eq!(got["keep"], 1);
        for worktree in &worktrees {
            let key = std::fs::canonicalize(worktree.path()).unwrap();
            assert_eq!(
                got["projects"][key.to_str().unwrap()]["hasTrustDialogAccepted"],
                true,
                "{key:?}"
            );
        }
    }

    #[test]
    fn settings_are_written_new_with_mode_0600_in_a_0700_folder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions").join("s.json");
        let value = settings_json(Path::new(CURL));

        write_settings(&path, &value).unwrap();

        assert_eq!(read(&path), value);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert!(write_settings(&path, &value).is_err(), "a second write");
    }

    #[test]
    fn a_missing_claude_json_is_created_with_mode_0600_and_just_the_trust() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let file = dir.path().join(".claude.json");

        mark_trusted(&file, worktree.path()).unwrap();

        let key = std::fs::canonicalize(worktree.path()).unwrap();
        assert_eq!(
            read(&file),
            json!({"projects": {key.to_str().unwrap(): {"hasTrustDialogAccepted": true}}})
        );
        assert_eq!(mode(&file), 0o600);
    }

    #[test]
    fn other_keys_and_the_entrys_other_fields_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let key = std::fs::canonicalize(worktree.path()).unwrap();
        let key = key.to_str().unwrap();
        let file = dir.path().join(".claude.json");
        let before = json!({
            "oauthAccount": {"emailAddress": "a@b.c"},
            "projects": {
                key: {"allowedTools": ["Bash"], "hasTrustDialogAccepted": false},
                "/other": {"hasTrustDialogAccepted": false}
            }
        });
        std::fs::write(&file, before.to_string()).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o646)).unwrap();

        mark_trusted(&file, worktree.path()).unwrap();

        let mut want = before.clone();
        want["projects"][key]["hasTrustDialogAccepted"] = json!(true);
        assert_eq!(read(&file), want);
        assert_eq!(mode(&file), 0o646, "the file's mode is kept");
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(left.len(), 1, "no temporary file is left behind");
    }

    #[test]
    fn a_new_project_entry_is_added_beside_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let file = dir.path().join(".claude.json");
        std::fs::write(&file, json!({"projects": {"/other": {"x": 1}}}).to_string()).unwrap();

        mark_trusted(&file, worktree.path()).unwrap();

        let key = std::fs::canonicalize(worktree.path()).unwrap();
        let got = read(&file);
        assert_eq!(got["projects"]["/other"], json!({"x": 1}));
        assert_eq!(
            got["projects"][key.to_str().unwrap()]["hasTrustDialogAccepted"],
            true
        );
    }

    fn only_the_file_is_left(dir: &Path) {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, [".claude.json"], "no temp file is left");
    }

    #[test]
    fn a_file_changed_while_merging_is_merged_again_and_keeps_the_change() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let file = dir.path().join(".claude.json");
        std::fs::write(&file, json!({"keep": 1}).to_string()).unwrap();

        mark_trusted_with(&file, worktree.path(), |attempt| {
            if attempt == 1 {
                std::fs::write(&file, json!({"keep": 1, "added": 2}).to_string()).unwrap();
            }
        })
        .unwrap();

        let key = std::fs::canonicalize(worktree.path()).unwrap();
        let got = read(&file);
        assert_eq!(got["keep"], 1);
        assert_eq!(got["added"], 2, "the write made during the merge");
        assert_eq!(
            got["projects"][key.to_str().unwrap()]["hasTrustDialogAccepted"],
            true
        );
        only_the_file_is_left(dir.path());
    }

    #[test]
    fn a_file_that_changes_on_every_try_still_gets_the_mark_after_the_last_try() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let file = dir.path().join(".claude.json");
        std::fs::write(&file, json!({"keep": 1}).to_string()).unwrap();
        let mut tries = 0;

        mark_trusted_with(&file, worktree.path(), |attempt| {
            tries = attempt;
            std::fs::write(&file, json!({"keep": 1, "n": attempt}).to_string()).unwrap();
        })
        .unwrap();

        assert_eq!(tries, MERGE_TRIES);
        let key = std::fs::canonicalize(worktree.path()).unwrap();
        assert_eq!(
            read(&file)["projects"][key.to_str().unwrap()]["hasTrustDialogAccepted"],
            true
        );
        only_the_file_is_left(dir.path());
    }

    #[test]
    fn an_already_trusted_file_is_not_written_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let key = std::fs::canonicalize(worktree.path()).unwrap();
        let file = dir.path().join(".claude.json");
        // Odd spacing and key order a rewrite would change.
        let text = format!(
            "{{ \"z\": 1,\n \"projects\": {{ {:?}: {{\"hasTrustDialogAccepted\":true}} }} }}",
            key.to_str().unwrap()
        );
        std::fs::write(&file, &text).unwrap();
        let before = std::fs::metadata(&file).unwrap().modified().unwrap();

        mark_trusted(&file, worktree.path()).unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
        assert_eq!(
            std::fs::metadata(&file).unwrap().modified().unwrap(),
            before
        );
    }

    #[test]
    fn a_file_that_is_not_json_is_an_error_that_leaves_it_alone_and_does_not_quote_it() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let file = dir.path().join(".claude.json");
        let text = r#"{"oauthToken": "SECRET-VALUE" nope"#;
        std::fs::write(&file, text).unwrap();

        let error = mark_trusted(&file, worktree.path()).unwrap_err();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains("SECRET-VALUE"), "{shown}");
        assert!(shown.contains(".claude.json"), "{shown}");
    }

    #[test]
    fn projects_or_the_entry_that_is_not_an_object_is_an_error_that_leaves_the_file_alone() {
        let worktree = tempfile::tempdir().unwrap();
        let key = std::fs::canonicalize(worktree.path()).unwrap();
        for text in [
            json!([1]).to_string(),
            json!({"projects": []}).to_string(),
            json!({"projects": {key.to_str().unwrap(): "yes"}}).to_string(),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join(".claude.json");
            std::fs::write(&file, &text).unwrap();

            assert!(mark_trusted(&file, worktree.path()).is_err(), "{text}");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
        }
    }

    #[test]
    fn a_worktree_given_through_a_symlink_is_trusted_under_its_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let link = dir.path().join("link");
        symlink(worktree.path(), &link).unwrap();
        let file = dir.path().join(".claude.json");

        mark_trusted(&file, &link).unwrap();

        let key = std::fs::canonicalize(worktree.path()).unwrap();
        let got = read(&file);
        let projects = got["projects"].as_object().unwrap();
        assert_eq!(projects.keys().collect::<Vec<_>>(), [key.to_str().unwrap()]);
    }

    #[test]
    fn a_symlinked_claude_json_has_its_target_updated_and_stays_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.json");
        std::fs::write(&target, json!({"keep": true}).to_string()).unwrap();
        let link = dir.path().join(".claude.json");
        symlink(&target, &link).unwrap();

        mark_trusted(&link, worktree.path()).unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        let key = std::fs::canonicalize(worktree.path()).unwrap();
        let got = read(&target);
        assert_eq!(got["keep"], true);
        assert_eq!(
            got["projects"][key.to_str().unwrap()]["hasTrustDialogAccepted"],
            true
        );
    }

    #[test]
    fn claude_json_is_in_the_config_dir_when_set_else_in_home_else_nowhere() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(
            claude_json(os("/cfg"), os("/home/me")),
            Some(PathBuf::from("/cfg/.claude.json"))
        );
        assert_eq!(
            claude_json(os(""), os("/home/me")),
            Some(PathBuf::from("/home/me/.claude.json"))
        );
        assert_eq!(
            claude_json(None, os("/home/me")),
            Some(PathBuf::from("/home/me/.claude.json"))
        );
        assert_eq!(claude_json(None, None), None);
        assert_eq!(claude_json(os(""), os("")), None);
    }
}
