# Claude Code contract

`sessions` takes a session's state from Claude Code's hooks. This page lists what it reads from them and what the contract test found when it ran a real session. The test is `crates/foundryd/tests/claude_contract.rs`. Run it after every Claude Code update and before every release.

Checked against **Claude Code 2.1.322** on macOS, on 2026-11-18, with the `sonnet` model through the gateway.

## Running it

```text
FOUNDRY_CONTRACT=1 cargo test -p foundryd --test claude_contract -- --ignored --nocapture
```

It needs `claude` on `PATH` and `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN` set. A run takes about 90 seconds and four short turns of model calls. Without `FOUNDRY_CONTRACT=1` it does nothing, so `cargo test` never spends a call.

Each run marks its temporary folder as trusted in your `~/.claude.json` (or `$CLAUDE_CONFIG_DIR/.claude.json`), as `sessions` does for a worktree. The entries stay.

## What `sessions` reads

| Field | On | What it decides |
|---|---|---|
| `hook_event_name` | every hook | which hook it is (`Hook::from_name`) |
| `reason` | `SessionEnd` | `clear` means `/clear`, and the session keeps running. Any other reason ends it |

The session comes from the URL the hook posts to, `/hooks/<session>`, not from the body. Nothing else in the body is read.

## The hooks as they arrived

Every field each hook carried on 2.1.322. Only the two above are read; the rest is here so a change shows up in a diff of this page.

| Hook | Fields |
|---|---|
| `SessionStart` | `cwd`, `hook_event_name`, `model`, `session_id`, `source` (`startup`), `transcript_path` |
| `UserPromptSubmit` | `cwd`, `hook_event_name`, `permission_mode`, `prompt`, `prompt_id`, `session_id`, `transcript_path` |
| `PreToolUse` | `cwd`, `effort`, `hook_event_name`, `permission_mode`, `prompt_id`, `session_id`, `tool_input`, `tool_name`, `tool_use_id`, `transcript_path` |
| `PostToolUse` | the `PreToolUse` fields, plus `duration_ms` and `tool_response` |
| `Notification` | `cwd`, `hook_event_name`, `message`, `notification_type`, `prompt_id`, `session_id`, `transcript_path`. For a permission dialog, `notification_type` is `permission_prompt` and `message` is "Claude needs your permission" |
| `Stop` | `background_tasks`, `cwd`, `effort`, `hook_event_name`, `last_assistant_message`, `permission_mode`, `prompt_id`, `session_crons`, `session_id`, `stop_hook_active`, `transcript_path` |
| `SessionEnd` | `cwd`, `hook_event_name`, `prompt_id`, `reason` (`prompt_input_exit` after `/exit`), `session_id`, `transcript_path` |

## Which hooks arrive after each step

The hooks are the seven `sessions` registers; the test sees no others. The state is what `next_state` makes of them, in order, from `starting`.

The test asserts each row exactly: these hooks, in this order, and no other of the seven by the end of the step. A step that ends on a key press waits 15 seconds after it before it checks.

| Step | Hooks, in order | State after |
|---|---|---|
| Start in a fresh folder | `SessionStart` | idle |
| A shell command the default mode runs without asking (`echo`) | `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop` | idle |
| A write, permission answered yes | `UserPromptSubmit`, `PreToolUse`, `Notification`, `PostToolUse`, `Stop` | idle |
| A write, permission denied with "No" | `UserPromptSubmit`, `PreToolUse`, `Notification`, then none of the seven in the 15 seconds after the answer | waiting for you |
| Esc while a shell command runs (`ping`, answered yes) | `UserPromptSubmit`, `PreToolUse`, `Notification`, then none of the seven in the 15 seconds after Esc | waiting for you |
| `/exit` | `SessionEnd`, then the process exits 0 | finished |

An early run left the session idle for three minutes and got a `Notification` after the `Stop`. `next_state` ignores a notification in idle, so it changes nothing.

## Gaps this leaves in `sessions`

1. **An interrupted turn posts none of the seven hooks.** There is no `Stop` after Esc, so the session keeps the state it had. The run saw `waiting for you`, because the interrupted `ping` had asked for permission. A tool that asks nothing would leave the session `working`, which turns `stuck` after 10 minutes; that follows from no hook arriving, and the run did not try it. `sessions` needs a rule for this.
2. **A denied permission posts none of the seven hooks.** The session shows `waiting for you` until the next prompt. The agent is at its prompt by then, so this is close to true, but it is not the dialog the state was set for.
3. **Answering yes posts none of the seven hooks.** From the answer until the tool's `PostToolUse`, the session shows `waiting for you` while the tool runs. For a long command that is the whole run.
4. **The permission mode is the machine's.** `sessions` passes no `--permission-mode`. On the machine the test was written on, 2.1.322 started in auto mode, which answers permission dialogs itself. Whether a session in auto mode ever posts `Notification`, and so ever shows `waiting for you`, was not checked.

Claude Code has hook events `sessions` does not register, such as `PermissionRequest` and `PostToolUseFailure`. The test cannot see them, so whether one marks an interrupt or a permission answer is unchecked. The `sessions` issue that adds a rule for gap 1 should register them and look before it picks one.

## The trust prompt

In a folder it has not seen, 2.1.322 opens with "Is this a project you created or one you trust?" and "No, exit" selected. `SessionStart` does not arrive until it is answered. `sessions` marks the worktree trusted in Claude Code's state file before it starts a session. The test starts in a fresh temporary folder with that mark and checks the prompt never shows. With the mark left out, the test fails at the start, naming the trust prompt.

## Where the test differs from a real session

The test writes the settings file and curl config with the daemon's own functions and starts `claude --settings <file> --model <model>` in a PTY with the gateway variables, as `sessions` does. It changes five things.

- It passes `--setting-sources ""`, so the user settings of the machine running it (their hooks, permission rules and `env`) cannot change what the session asks or posts.
- It passes `--permission-mode default`. On the machine the test was written on, 2.1.322 started in auto mode, which answers permission dialogs itself, so no `Notification` would come.
- It unsets every inherited `CLAUDE*` variable but `CLAUDE_CONFIG_DIR`, such as the `CLAUDECODE` and `CLAUDE_CODE_*` a parent Claude Code session sets, so it can run from inside Claude Code.
- The hooks post to a recorder in the test that checks the bearer token the way the daemon does, not to the daemon itself.
- It opens its own 120x40 PTY. A real session runs in the holder's 80x24 one.
