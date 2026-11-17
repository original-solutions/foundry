# P1 Core implementation plan

**Goal:** Build `foundryd`, one Rust daemon per machine that hosts workspaces, terminals and Claude Code sessions, syncs state between machines over Tailscale and serves a minimal web UI.

**Architecture:** A Cargo workspace with a shared types crate, a small `holder` binary that owns one PTY, and the `foundryd` daemon made of modules with one job each. Each machine owns its own records in SQLite and sends them to every peer. The web UI is built once and embedded in the binary.

**Tech stack:** Rust (stable, pinned in `rust-toolchain.toml`), tokio, axum for HTTP and WebSocket, rusqlite, serde, clap, sysinfo. Web: React, Tailwind, Radix, TanStack Query and Router, xterm.js with the serialize add-on, built with Vite.

**Spec:** [P1-core.md](P1-core.md). Each issue below names the spec sections it implements. The builder reads both.

**How this plan is executed:** each task is one GitHub issue on `original-solutions/foundry`, filed with the `factory-issue` skill and built by the factory. Every issue is linked to the issues it depends on and all are labelled `factory:ready` together. The factory holds an issue until its blockers are closed. The plan therefore gives each task its files, its interfaces and its acceptance tests, and leaves the code to the builder, which works test-first.

## Global constraints

- One binary per machine with no runtime to install. No Node on any machine at run time.
- The daemon binds only to the machine's Tailscale address and to localhost.
- No headless agent mode. Every session is a terminal a person can open.
- Session state comes from Claude Code's hooks, never from reading the screen.
- A machine writes only its own records. A receiver accepts a record only from its owner and only when its version is newer.
- Every model call goes through the gateway: `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN` are set on every session.
- Secrets never reach a log or an API response.
- Timings from the spec: load every 5 seconds, offline after 15 seconds, stuck after 10 minutes, coordinator takeover within 15 seconds, scrollback 10,000 lines.
- No production change without a failing test first. The gate is `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`, `npm --prefix web run check`.
- No test spends model calls, except the one contract test, which is ignored unless `FOUNDRY_CONTRACT=1`.
- No `unsafe` without a `// SAFETY:` comment (`clippy::undocumented_unsafe_blocks = "deny"`).

## Review focus

Conditions the spec implies that are most likely to hurt in use. Each has a test in the task named.

1. A session prints megabytes of output faster than any UI reads it. The holder must not block the child or grow without limit (task 4).
2. A worktree path or branch name with spaces, unicode or a leading dash. Branch names are made safe and every git call passes paths after `--` (task 9).
3. Two machines' clocks disagree by minutes. Nothing may order records by wall time, only by version (tasks 3 and 12).
4. A hook call arrives for a session the daemon has forgotten, or with a wrong token. It is refused and changes nothing (task 6).
5. A machine rejoins holding records for a workspace that was archived while it was away. The archive wins and no worktree is recreated (task 12).

## File structure

```
Cargo.toml                     workspace
rust-toolchain.toml
crates/
  foundry-proto/               shared types, no I/O
    src/{ids,record,state,settings,messages}.rs
  foundry-holder/              bin `foundry-holder`: one PTY, one socket
    src/{main,pty,scrollback,protocol}.rs
  foundry-fake-agent/          bin used only by tests
  foundryd/
    src/main.rs                clap commands: run, install, uninstall, status, join
    src/store/                 SQLite schema, migrations, record upsert
    src/terminals.rs           start and reattach holders, relay bytes
    src/sessions/              launch, hooks endpoint, state rules, stuck timer
    src/workspaces/            clones, worktrees, setup, archive
    src/machine.rs             load, memory, disk, limits
    src/placement.rs           pure: pick a machine or give reasons
    src/sync/                  peer links, record exchange, catch-up
    src/coordinator.rs         priority order, shared settings sequence
    src/join.rs                one-time codes, machine keys
    src/secrets.rs             keychain on macOS, 0600 file on Linux
    src/gateway.rs             health and cooldown read
    src/attention.rs           raise and clear items
    src/api/                   HTTP and WebSocket for the UI
    src/update.rs              tag install, rollback
    src/service.rs             launchd and systemd units
    tests/                     fleet harness: three daemons, cuttable links
web/                           the UI, embedded into foundryd at build time
```

## Tasks

Dependencies are in brackets. Tasks with no dependency between them can build at the same time.

### 1. Workspace scaffold

- **Files:** `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, every crate in the file structure with a stub file for every module, `web/` as a minimal Vite and React app, `.github/workflows/check.yml`.
- **Produces:** a workspace that passes the gate, with every dependency the later tasks need already declared and every module file already registered, so later tasks fill their own files and do not edit shared ones. `foundryd --version` prints the crate version. The other commands (`run`, `install`, `uninstall`, `status`, `join`) exist and exit 2 with "not built yet". `web/` has `build` and `check` scripts, and `foundryd` embeds `web/dist`.
- **Tests:** an integration test runs the binary with `--version` and matches the version in `Cargo.toml`. CI runs the provision and gate commands on macOS and Linux.

### 2. Shared types [1]

- **Spec:** Sessions (the state table), Sync and the coordinator (Records).
- **Files:** `crates/foundry-proto/src/*`.
- **Produces:** `MachineId`, `ProjectId`, `WorkspaceId`, `TerminalId`, `SessionId` (UUID newtypes); `Record<T> { owner: MachineId, version: u64, deleted: bool, body: T }`; bodies `Workspace`, `Terminal`, `Session`, `MachineSample`; `SessionState` with the eight states and `TerminalState` with three; `SharedSettings { seq: u64, machines, priority, projects, limits }`; the `PeerMessage` and `UiMessage` enums, each carrying `protocol: u16`.
- **Tests:** JSON round trip for every type. An unknown field is ignored and an unknown enum variant is an error that names it. `Record::supersedes(&other)` is true only for the same owner and a higher version.

### 3. Store [2]

- **Spec:** Sync and the coordinator (Records), D8, D10.
- **Files:** `crates/foundryd/src/store/*`.
- **Produces:** `Store::open(path)`, `apply(record, from: MachineId) -> Applied { Stored, Stale, NotOwner }`, `own_next_version()`, `since(owner, version) -> Vec<Record>`, `versions() -> Map<MachineId, u64>`, `settings()`, `apply_settings(SharedSettings) -> bool`.
- **Tests:** a record from a machine that is not its owner is `NotOwner` and stores nothing. An equal or lower version is `Stale`. Applying the same record twice changes nothing. A deleted record stays deleted when an older live version arrives. Settings with a lower `seq` are refused. Migrations run on an empty file and on the previous schema. No query orders by a timestamp (review focus 3).

### 4. Holder [1]

- **Spec:** Terminals.
- **Files:** `crates/foundry-holder/src/*`.
- **Produces:** `foundry-holder --socket <path> --cwd <dir> --env-file <path> -- <argv>`. It detaches into its own session, owns the PTY, and serves a length-prefixed protocol on the Unix socket: `Attach` returns the current screen and scrollback then live bytes, `Input(bytes)`, `Resize(cols, rows)`, `Status` returns running or the exit code, `Close`.
- **Tests:** output is delivered byte for byte to an attached client. A client that attaches late gets the earlier output first. Scrollback holds 10,000 lines and drops the oldest. With no client attached, a child writing 200 MB finishes and holder memory stays under 50 MB (review focus 1). Killing the process that started the holder leaves the child running. After the child exits, `Status` gives the exit code and `Attach` still gives the final screen. The env file is deleted once read.

### 5. Terminals [3, 4]

- **Spec:** Terminals.
- **Files:** `crates/foundryd/src/terminals.rs`.
- **Produces:** `Terminals::start(workspace, argv, env) -> TerminalId`, `attach(id) -> (Snapshot, Stream<Bytes>)`, `input`, `resize`, `close`, `reattach_all()` run at start, and a `Terminal` record kept current in the store.
- **Tests:** a terminal started, then the daemon's task dropped and `reattach_all` run on a new one: the stream continues with no byte lost or repeated, checked with a child that prints a counter. A holder whose socket is gone is recorded as exited with an unknown code. A terminal record's state follows running to exited.

### 6. Sessions and the fake agent [5]

- **Spec:** Sessions, Security (hook calls), the trust prompt finding.
- **Files:** `crates/foundryd/src/sessions/*`, `crates/foundry-fake-agent/src/main.rs`.
- **Produces:** `Sessions::start(workspace, model) -> SessionId`, which writes a settings file registering hooks that `POST http://127.0.0.1:<port>/hooks/<session>` with a per-session token, marks the worktree trusted for Claude Code, and starts `claude --settings <file>` in a terminal with the gateway variables. A pure `next_state(state, event) -> state`. A stuck timer. The fake agent reads a script of hook events and delays and posts them.
- **Tests:** every row of the spec's state table is reached with the fake agent (acceptance 6). A hook with a wrong token or an unknown session gets 403 and changes nothing (review focus 4). A hook call from a non-loopback address is refused. Ten minutes of working with no output and no hook, with the clock faked, gives stuck, and one byte of output clears it. A child that exits non-zero gives failed. The gateway key appears in the child's environment and in no log line.

### 7. Contract test against Claude Code [6]

- **Spec:** Testing, Checked before the plan is written (rows 1 and 2).
- **Files:** `crates/foundryd/tests/claude_contract.rs`, `docs/claude-code-contract.md`.
- **Produces:** one ignored test, run with `FOUNDRY_CONTRACT=1`, that starts a real session in a fresh folder, sends a prompt that runs one shell command, then a prompt that needs a permission answer, then exits.
- **Tests:** the session starts with no trust prompt. `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`, `Notification` and `SessionEnd` each arrive with the fields `sessions` reads. The doc lists those fields and the Claude Code version checked.

### 8. Machine samples and limits [3]

- **Spec:** Placement (first three bullets), State is always visible.
- **Files:** `crates/foundryd/src/machine.rs`.
- **Produces:** a `MachineSample { load_per_core, free_memory, free_disk, sessions, at_version }` record written every 5 seconds, and `over_limits(sample, limits) -> Vec<Reason>`.
- **Tests:** load per core is the 1-minute load divided by logical cores. Free disk is read for the volume holding the data directory. Each limit gives its own reason text in the spec's form, `trilogy-mac: load 1.8 over limit 1.0`.

### 9. Workspaces [5]

- **Spec:** Workspaces.
- **Files:** `crates/foundryd/src/workspaces/*`.
- **Produces:** `Workspaces::create(project, base, name) -> WorkspaceId`, `archive(id, force: bool) -> Result<(), Dirty>`, `safe_branch(name) -> String`. Create makes the bare clone when missing, fetches, adds a worktree at `worktrees/<project>/<branch>`, and runs the `provision` commands from `.dw/config.yml` in a terminal.
- **Tests:** all against local bare repos, no network. A name with spaces, unicode, `..` or a leading dash becomes a valid branch and the worktree is made (review focus 2). A second workspace of the same project reuses the clone. A failing setup command leaves the workspace usable and reports the failure. Archive with an uncommitted file or an unpushed commit returns `Dirty` listing each. Archive with `force` removes the worktree and the branch still exists. A repo with no `.dw/config.yml` runs no setup.

### 10. API [6, 8, 9]

- **Spec:** D4, State is always visible.
- **Files:** `crates/foundryd/src/api/*`.
- **Produces:** `GET /api/state` (the full synced state as JSON), a WebSocket at `/api/events` that sends every record change, a WebSocket at `/api/terminals/<id>` carrying the snapshot then bytes both ways, and `POST` routes to create and archive a workspace, start a session or shell, and close a terminal. The server refuses to bind to any address that is not loopback or in Tailscale's range.
- **Tests:** a client connected to `/api/events` sees a workspace appear and its session change state. A terminal socket echoes typed input from a `cat` child. Binding to `0.0.0.0` fails with an error that names the rule. No response contains a secret.

### 11. Minimal web UI [10]

- **Spec:** Scope (minimal UI), D2, D3.
- **Files:** `web/*`.
- **Produces:** a sidebar listing machines with their numbers and state, and under each its workspaces, terminals and sessions with their states; a main pane with xterm.js attached to the selected terminal; a dialog to create a workspace with the machine picker; an archive action. The scaffold already embeds `web/dist` and the gate already runs `npm --prefix web run check`.
- **Tests:** component tests for the sidebar from a fixed state and for each session state's label. One browser test against a real daemon with the fake agent: create a workspace, see the session reach idle, type in a shell and read the output, reload the page and see the same screen (acceptance 5).

### 12. Sync [3]

- **Spec:** Sync and the coordinator (Records), State is always visible (offline, unreachable).
- **Files:** `crates/foundryd/src/sync/*`, `crates/foundryd/tests/fleet/*`.
- **Produces:** a link to each peer in the machine list, a hello that exchanges `versions()`, a catch-up of everything newer, then live record sends. A peer silent for 15 seconds is offline and its sessions read as unreachable. The test harness starts N daemons in one process with links a test can cut and heal.
- **Tests:** three daemons converge on the same state. A daemon cut off for a simulated two hours has the full state within 30 seconds of healing (acceptance 10). A cut-off machine shows offline on the others within 15 seconds and its sessions unreachable (acceptance 9). A workspace archived while its record's old copy sits on a returning peer stays archived (review focus 5). A peer speaking an older protocol number is sent the older message form.

### 13. Joining and secrets [12]

- **Spec:** Security.
- **Files:** `crates/foundryd/src/join.rs`, `crates/foundryd/src/secrets.rs`.
- **Produces:** `foundryd join <address> <code>`; a one-time code valid for 10 minutes; a key per machine checked on every peer link; revoking a machine; `Secrets::get` and `set` over the macOS keychain or a 0600 file.
- **Tests:** a code works once and not after 10 minutes. A peer with a revoked key is refused and shown as revoked. A link with no key is refused. The Linux secrets file is created 0600 and a wider mode is refused at read.

### 14. Coordinator [12, 13]

- **Spec:** The coordinator role.
- **Files:** `crates/foundryd/src/coordinator.rs`.
- **Produces:** a pure `acting(priority, reachable) -> MachineId`; settings edits accepted only on the acting coordinator, numbered in one sequence and sent to all; a returning higher-priority machine that catches up before it takes the role.
- **Tests:** in the fleet harness, stopping the coordinator moves the role within 15 seconds while a session on another machine keeps producing output (acceptance 8). An edit sent to a machine that is not acting is forwarded. After a split with one edit on each side, the later edit holds and a "setting overwritten" item shows both values.

### 15. Placement [8, 12, 14]

- **Spec:** Placement.
- **Files:** `crates/foundryd/src/placement.rs`.
- **Produces:** a pure `place(samples, limits, online) -> Result<MachineId, Vec<Reason>>`, a queue of waiting requests retried on every new sample, and the "let Foundry choose" path in workspace creation.
- **Tests:** the lowest load per core among machines under their limits wins, with fewer sessions breaking a tie (acceptance 2). All over a limit gives one reason per machine, and the request is placed within 10 seconds of a machine qualifying (acceptance 3). An offline machine is never picked. A machine named by the person is used over its limits and the response carries a warning.

### 16. Terminals on other machines [10, 12]

- **Spec:** Terminals (bytes relayed through the machine the browser is on).
- **Files:** `crates/foundryd/src/sync/relay.rs`, `crates/foundryd/src/api/terminals.rs`.
- **Produces:** a terminal socket opened on machine A for a terminal owned by machine B, carried over the A to B peer link.
- **Tests:** in the fleet harness, input typed through A reaches a `cat` child on B and its output returns. Cutting the link closes the socket with a reason, and healing it gives the snapshot again.

### 17. Attention list and gateway health [6, 9, 11, 12, 14]

- **Spec:** Attention items in P1, State is always visible (gateway).
- **Files:** `crates/foundryd/src/attention.rs`, `crates/foundryd/src/gateway.rs`, `web/src/attention/*`.
- **Produces:** each of the ten items in the spec's table raised and cleared by its rule, the list in the UI, and a gateway reader that uses the management key.
- **Tests:** one test per row of the table for the raise and one for the clear. A "waiting for you" item appears within 5 seconds of the hook (acceptance 7). A gateway that refuses the connection, and one with every account cooling down, each raise the item.

### 18. Service and self-update [5, 12]

- **Spec:** Updates, Checked before the plan is written (rows 3 and 4).
- **Files:** `crates/foundryd/src/service.rs`, `crates/foundryd/src/update.rs`, `docs/install.md`.
- **Produces:** `foundryd install` and `uninstall` for a launchd agent on macOS and a systemd unit under a non-root user on Linux, both set so a restart does not kill holders; versions under `versions/<v>` with a `current` link; install of a newer `v*` tag; rollback after three starts without becoming healthy.
- **Tests:** the unit files are checked as text for the settings that spare child processes. Three starts that never report healthy point `current` back and record the refused tag. A refused tag is skipped while it is the highest. A manual check, written in `docs/install.md` and run on a Mac and a Linux machine before release: restart the service with a session open and see it continue (acceptance 5 and 12).

### 19. Acceptance run [all]

- Not a factory issue. Foundry is installed on this Mac, trilogy-mac and one Linux machine, and each of the twelve acceptance criteria in the spec is checked by hand and recorded in `docs/acceptance/P1.md`.

## Order

```
1 ─┬─ 2 ── 3 ─┬─ 5 ── 6 ── 7
   │          ├─ 8
   └─ 4 ──────┘ (5 needs 3 and 4)
5 ── 9
6, 8, 9 ── 10 ── 11
3 ── 12 ── 13 ── 14 ── 15
10, 12 ── 16
6, 9, 11, 12, 14 ── 17
5, 12 ── 18
```

Up to three issues can build at once for most of the run: after task 3, the terminal line (4, 5, 6), the machine line (8) and the sync line (12, 13, 14) do not touch the same files.
