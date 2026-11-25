# P2 Factory implementation plan

**Goal:** `foundryd` claims `factory:ready` issues, builds each in a Foundry workspace on a machine placement picks, gates the pull request on a fresh checkout and carries it to `factory:pr-open`, so the Paradev runner can be turned off.

**Architecture:** A `factory` module in `foundryd`, run by the acting coordinator for each project whose lease it holds. It is a Rust port of the runner's core (`original-solutions/factory-on-paradev` at `8ed9bef`). A run is a P1 workspace with a P1 session, and a gate is a P1 workspace whose setup terminal runs the gate. Factory records live in a table of their own and sync on a cursor of their own, on protocol 5.

**Tech stack:** as P1. Rust with tokio, axum, rusqlite, serde, `serde_yaml_ng`, `nix`; `gh` and `git` as child processes. Web: React, Tailwind, Radix, TanStack Query and Router.

**Spec:** [P2-factory.md](P2-factory.md). Each task below names the spec sections it implements. The builder reads both.

**How this plan is executed:** each task is one GitHub issue on `original-solutions/foundry`, filed with the `factory-issue` skill and built by the factory. Every issue is linked to the issues it depends on. The plan gives each task its files, its interfaces and its acceptance tests, and leaves the code to the builder, which works test-first.

**The runner's source.** A port task reads the runner at the pinned commit. It is public:

```bash
git clone --filter=blob:none https://github.com/original-solutions/factory-on-paradev /tmp/fop
git -C /tmp/fop checkout 8ed9bef
```

Paths in this plan that start with `src/` are in that checkout. Clone it outside the worktree and commit none of it, except the fixtures a task names.

## Global constraints

- P1's constraints still hold: one binary, Tailscale and localhost only, no headless agent, state from hooks, a machine writes only its own records, every model call through the gateway, secrets in no log or response, no `unsafe` without `// SAFETY:`.
- Existing P1 records and settings values gain no field (F11). Factory data goes in factory records and factory settings keys.
- Nothing orders records by wall-clock time. Run events order by lease generation, then the writer's factory version.
- Every age and wait the factory measures uses the sleep-counting clock of task 3, never `Instant` or `tokio::time`. Tests move a fake clock.
- A token reaches a child process only through its environment. It is never an argument, a log line, an error, a record or an API response.
- Text that leaves a machine's memory for a comment, a message to a session or an event first goes through `secrets::hide_known`. A comment also goes through `text::for_comment`, and the factory's own markers are added after it.
- Every `gh` call and every lease push ends within 30 seconds or counts as failed.
- The lease age is checked before every change the factory makes on GitHub or asks of a worker.
- No step of a pass waits on a worker. It records, returns, and looks again next pass.
- Every step records before it acts, as the runner does, so a step done again carries on.
- Timings from the spec: lease renewal 60 s, act within 120 s of the push's start, takeover after 180 s unchanged, cut-off after 10 minutes, unreachable build machine 15 minutes, unreachable gate machine 5 minutes, gate command limit 30 minutes, `poll_seconds` 10 to 60.
- Sizes from the spec: output in an event 64 KB, output in a comment its last 20,000 characters, a message body 64 KB, a typed line under 800 characters.
- For each ported module, the runner's tests become Rust tests with the same cases and names, written first and failing before the code lands. Where this plan is silent on a rule of the lifecycle, the runner at `8ed9bef` decides.
- No test calls GitHub and no test spends model calls, except the one contract test, ignored unless `FOUNDRY_CONTRACT=1`.
- The gate is `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`, `npm --prefix web run check`.
- Edit only the files your issue names. The scaffold registers every module and declares every dependency.

## Review focus

Conditions the spec implies that are most likely to hurt in use. Each has a test in the task named.

1. A comment that carries Foundry's claim marker but was written by someone else, or an issue body that quotes `FACTORY-DONE:`. A claim marker counts only in a comment by the project's login, and a result line only as the last line of a session's final message (tasks 13 and 9).
2. A pull request branch named with spaces, unicode, `..` or a leading dash, checked out for a feedback or resume workspace. The name is refused as the runner's `branchRefused` refuses it, and every git call passes names after `--` (tasks 14, 24 and 28).
3. Gate output of megabytes, with escape sequences, NUL bytes or a multi-byte character at the cut. The tail is cut on a character boundary, escapes are removed, and each line sent to a session starts with `| ` as untrusted text (tasks 16 and 11).
4. GitHub down, slow or rate-limiting in the middle of a pass. A `gh` call is killed at 30 seconds, the pass moves to the next project, no step is half-applied, and three failed passes raise "factory idle" (tasks 12, 28 and 30).
5. A machine's wall clock steps back by minutes while a run builds. `limits.minutes` never counts a negative span, and no event order changes (tasks 9 and 22).

## File structure

```
crates/foundry-proto/src/factory.rs       factory records, factory settings, run ids
crates/foundryd/src/
  clock.rs                                the clock that counts through sleep
  git_auth.rs                             a login's token for one git call, helpers and hooks off
  secrets.rs                              gains github:<login>, names(), hide_known()
  store/factory.rs                        the factory table, its versions and reads
  sync/factory.rs                         factory records on protocol 5 links
  terminals.rs                            gains tail()
  workspaces/mod.rs                       gains checkouts, setup kinds, folders by id
  sessions/factory.rs                     the factory start, messages, ending sessions
  sessions/turns.rs                       each turn's final message on disk, session_turn
  api/factory.rs                          carried worker routes, the Factory view's routes
  attention.rs                            gains the factory items
  factory/
    mod.rs                                the Factory handle and its service
    lifecycle/{mod,state,approval,depends_on,base,result_line}.rs   pure
    text.rs                               cutting a tail, hiding secrets in a comment
    config.rs                             the factory block of .dw/config.yml
    github/{mod,gh,read,write,fake}.rs    the tracker behind a trait
    github/fixtures/                      copied from the runner's tracker/fixtures
    lease.rs                              refs/factory/lease
    ledger/{mod,events,read,list,export}.rs
    messages.rs                           every message as a line and a body
    worker.rs                             what a run asks of a machine, and its fake
    fleet.rs                              the real Worker, over local calls and peer calls
    placing.rs                            which machine takes a run or a gate
    held.rs                               the logins and skill this machine holds
    gate.rs                               gate commands, results, backoff
    runs/{mod,progress,start,building,resume,gating,feedback,stop}.rs
    status.rs                             factory_status: writing it, choosing one to believe
    pass.rs                               one pass over one project
    cutoff.rs                             a cut-off machine ends its factory sessions
  tests/fleet/factory/{mod,github,build,failover,workers}.rs
  tests/fixtures/runner/                  a run file from the runner, for the export test
web/src/Factory.tsx, FactorySettings.tsx, factory.ts
```

## Tasks

Dependencies are in brackets. Tasks with no dependency between them can build at the same time.

### 1. Scaffold

- **Files:** `crates/foundryd/src/lib.rs`, `crates/foundryd/src/main.rs`, `crates/foundryd/tests/cli.rs`, `Cargo.toml`, `Cargo.lock`, a stub for every new file in the file structure, `crates/foundryd/src/api/mod.rs` (mounts `api::factory::router()`), `crates/foundryd/tests/fleet/main.rs` (registers `mod factory`), `AGENTS.md`.
- **Produces:** every module registered, so later tasks fill their own files. `nix` gains its `time` feature. New commands that exit 2 with "not built yet" and each call one function a later task fills: `foundryd github default <login>` (`secrets::github_default_command`), `foundryd factory export <dir>` (`factory::ledger::export::command`). `AGENTS.md` names the P2 spec and this plan beside P1's.
- **Tests:** in `crates/foundryd/tests/cli.rs`, one test per new command, named for it, that it exits 2 and prints "not built yet". The task that builds a command removes that command's test and no other. The gate passes.

### 2. Factory types and protocol 5 [1]

- **Spec:** Factory records, Shared settings, G6, G7, F11.
- **Files:** `crates/foundry-proto/src/factory.rs`, `crates/foundry-proto/src/messages.rs`, `crates/foundry-proto/src/settings.rs`, `crates/foundry-proto/src/lib.rs`.
- **Produces:**
  - `RunId { repo: String, issue: u64, attempt: u32 }`, shown and parsed as `<owner>__<repo>__<N>__<attempt>`.
  - `FactoryRecord { owner: MachineId, version: u64, body: FactoryBody }` with `FactoryBody::{RunEvent, Status, Held, Turn}` and a `key()` of kind and id.
  - `RunEvent { run, generation: u64, ts: String, ms: u64, event: String, data: Value, cause: Option<Value> }`. `ts`, `ms`, `event`, `data` and `cause` are the runner's ledger line, unchanged.
  - `FactoryStatus { project, pass: u64, holds: bool, generation: u64, not_holding: Option<NotHolding>, idle: Option<String>, owner_ignored: bool, ready: Vec<Waiting>, feedback_waiting: Vec<Waiting>, runs: Vec<LiveRun>, held_up: Vec<HeldUp>, gate_infra: Vec<RunId>, finished: Vec<FinishedRun>, foreign: Vec<ForeignClaim> }`. `NotHolding::{NoToken { login }, HeldBy { machine, generation }}`. `LiveRun { run, title, step, machine, gate_retries_used, minutes_used, workspaces: Vec<WorkspaceId>, sessions: Vec<SessionId> }`. `HeldUp { run, kind: Stuck | NeedsInfo, reason }`.
  - `MachineHeld { logins: Vec<String>, skill: bool }`. `SessionTurn { session, workspace, project, turn: u64, prompts: u64, question: bool }`.
  - `FactorySettings { paused, poll_seconds, feedback_seconds, max_runs, max_gates, model, effort }` with the spec's defaults, and `ProjectFactory { on, login, paused }`. `SharedSettings` holds them as `factory: Option<FactorySettings>` and `project_factory: BTreeMap<ProjectId, ProjectFactory>`, both defaulted, with `SettingKey::Factory` (`factory`) and `SettingKey::ProjectFactory(ProjectId)` (`factory:<id>`).
  - `PROTOCOL = 5`. `PeerMessage::Hello` carries `factory_versions`, defaulted. `PeerMessage::FactoryRecords { protocol, records }`. `UiMessage::FactoryStatus { protocol, status }`.
  - `SharedSettings::for_protocol(n)`: a copy for a peer on protocol `n`, with factory keys, their stamps and their overwritten entries removed when `n < 5`.
- **Tests:** JSON round trip for every type. A `RunEvent` serialises to the same keys as a line of `src/ledger/ledger.test.ts`. `poll_seconds` outside 10 to 60 does not parse. `for_protocol(4)` output parses with the settings parser as it stood at `PROTOCOL = 4` (a copy of the old key parser kept in the test). A `SharedSettings` written before this task still parses.

### 3. The sleep-counting clock [1]

- **Spec:** The lease (ages), check 9.
- **Files:** `crates/foundryd/src/clock.rs`.
- **Produces:** `trait Clock: Send + Sync { fn now(&self) -> Duration; }`. `SystemClock` reads `CLOCK_MONOTONIC` on macOS and `CLOCK_BOOTTIME` on Linux through `nix::time::clock_gettime`. `FakeClock` with `advance(Duration)`. `clock::system() -> Arc<dyn Clock>`.
- **Tests:** two reads never go back. `FakeClock` advances by what it is told. A comment in the file says why `Instant` is not used, and a test asserts the clock id per OS.

### 4. Factory store [2]

- **Spec:** Factory records, G6.
- **Files:** `crates/foundryd/src/store/factory.rs`, `crates/foundryd/src/store/mod.rs` (one migration), `crates/foundryd/src/store/tests.rs`.
- **Produces:** tables `factory_records (kind, id, owner, version, record)` and `own_factory_version`. `Store::apply_factory(record, from) -> Applied`, `own_next_factory_version()`, `factory_since(owner, version, limit)`, `factory_versions()`, `run_events(&RunId) -> Vec<RunEvent>` in generation then version order, `runs_of(repo) -> Vec<RunId>`, `statuses(project) -> Vec<(MachineId, FactoryStatus)>`, `held(machine) -> Option<MachineHeld>`, `session_turn(session) -> Option<SessionTurn>`, `highest_generation(project) -> u64`.
- **Tests:** the owner and version rules of P1's `apply`, for factory records. `Store::since`, `versions` and `live_session_count` never return or count a factory record. Two events of one run from two owners at generations 1 and 2 read back in generation order whatever their versions. The migration runs on an empty file and on a P1 database, and leaves `records` untouched. No query orders by `ts` or `ms`.

### 5. Factory records on the link [4]

- **Spec:** G6, G8 (the events socket).
- **Files:** `crates/foundryd/src/sync/factory.rs`, `crates/foundryd/src/sync/link.rs`, `crates/foundryd/src/sync/mod.rs`, `crates/foundryd/tests/fleet/sync.rs`.
- **Produces:** on a link whose two ends speak 5, a catch-up of factory records from the `factory_versions` in the hello, then live sends, on a cursor apart from P1's. Nothing factory is sent on a protocol 4 link. `Peers::factory_changes() -> watch::Receiver<u64>` for readers.
- **Tests:** three daemons converge on a `run_event`. A peer that says hello on protocol 4 receives no factory record and its P1 catch-up is unchanged; when it says hello again on 5 with empty `factory_versions` it receives every one (review of the upgrade hole). A factory record from a machine that is not its owner is refused.

### 6. Factory settings through the coordinator [5]

- **Spec:** G7, Shared settings.
- **Files:** `crates/foundryd/src/coordinator.rs`, `crates/foundryd/src/sync/link.rs`, `crates/foundryd/tests/fleet/coordinator.rs`.
- **Produces:** edits of `SettingKey::Factory` and `SettingKey::ProjectFactory` numbered like any setting. Settings sent to a protocol 4 peer go through `for_protocol(4)`. A copy merged from a protocol 4 peer leaves this machine's factory keys and stamps as they are. An edit of a factory key is refused, with the reason, while the acting coordinator is on protocol 4.
- **Tests:** in the fleet harness, a factory setting edited on one machine reaches the others. A protocol 4 peer's copy merged in does not remove it. A split with a factory setting edited on both sides raises "setting overwritten" as P1 does.

### 7. GitHub logins and what a machine holds [4]

- **Spec:** GitHub tokens, G9, G11.
- **Files:** `crates/foundryd/src/secrets.rs`, `crates/foundryd/src/factory/held.rs`, `crates/foundryd/src/daemon.rs` (one line under `// services`), `crates/foundryd/tests/cli.rs` (its own command's test).
- **Produces:** `secret set github:<login>` accepted; a login is letters, digits and `-`, at most 39 characters (GitHub's own limit). `Secrets::names() -> Result<Vec<String>, SecretsError>`. `secrets::github_logins(&dyn Secrets) -> Result<Vec<String>, SecretsError>`, `secrets::github_token(&dyn Secrets, login) -> Result<Option<String>, SecretsError>`. `secrets::hide_known(text, &dyn Secrets) -> String`, which hides the gateway keys and every `github:*` token the store holds at the time of the call, and URL passwords as `redact` does. A held value of at least 16 characters is matched also across gaps inside it, on its characters that are no gap, each at most 32 characters of space, tab, LF, CR, VT, FF, NEL, the other C0 and C1 controls, DEL, the bidi and zero-width characters U+200B to U+200F, U+202A to U+202E, U+2066 to U+2069 and U+FEFF (no other white space, bidi or zero-width character), or whole ANSI escape sequences (a lone escape introducer is a gap of its own), and only the matched span is replaced; any other value is hidden only whole. `secrets::github_default_command(login)` stores the machine's default login, and `github_default(&dyn Secrets) -> Option<String>` reads it. `held::run(...)`: writes this machine's `MachineHeld` at start and whenever the logins or the skill change, checked every 5 seconds; `skill` is true when `~/.claude/skills/factory-build/SKILL.md` exists for the service user.
- **Tests:** `secret set github:bad/name` is refused. `names()` lists what was set and no value. `hide_known` hides a token set after the daemon started, leaves a 40-character commit id alone, and leaves a pull request URL whole. A token appears in no log line of the test. `MachineHeld` changes within 5 seconds of a `secret set`.

### 8. Git credentials for one call [7]

- **Spec:** G5, check 3.
- **Files:** `crates/foundryd/src/git_auth.rs`.
- **Produces:** `GitAuth::for_login(secrets, login, data_dir) -> Result<GitAuth, NoToken>`. `GitAuth::apply(&mut Command)`: sets `GIT_ASKPASS` to a 0700 script in the data directory that prints the token from a variable set on that child only, `GIT_TERMINAL_PROMPT=0`, `-c credential.helper=`, `-c core.hooksPath=/dev/null`, `GIT_CONFIG_NOSYSTEM=1` and `GIT_CONFIG_GLOBAL=/dev/null`, as `src/gitHardening.ts` does. `git_auth::helpers_off_env() -> Vec<(String, String)>`: the environment that turns credential helpers off for a terminal, with no token.
- **Tests:** on the argv and environment of a child the test starts, with no network. The token is in the child's environment and in no argument. The askpass script holds no token. `helpers_off_env` holds no token and, given to `git config --get-all credential.helper` in a repo whose global config sets a helper, prints nothing.

### 9. Lifecycle and text [1]

- **Spec:** `factory::lifecycle`, GitHub tokens (comments), F13.
- **Files:** `crates/foundryd/src/factory/lifecycle/*`, `crates/foundryd/src/factory/text.rs`.
- **Ports:** `src/domain/{lifecycle,approval,dependsOn,base,resultLine}.ts` with their tests, and `src/text.ts`.
- **Produces:** `State` with the seven states and `label()`; `transition(state, event) -> Result<(State, Vec<Action>), InvalidTransition>`; `approval::Refusal` with `refusal_reason(&Refusal)` and `unapproved_comment(&Refusal)`; `depends_on::blockers(body)`; `base::of(body)`; `result_line::read(message, repo) -> SessionResult::{Done { pr, url }, NeedsInfo { questions }, Stuck { reason, session_text }}`. `text::tail(text, max_chars)` cut on a character boundary. `text::for_comment(text)`: hides every word of 24 or more mixed letters and digits, inside a URL too; the only such word it keeps is a commit id or a comment, review or diff anchor in a `github.com` link, at the place GitHub's own links put one. `text::elapsed_ms(from_ms, to_ms) -> u64`, never negative.
- **Tests:** the runner's cases, same names. A `FACTORY-DONE` line that is not the last line is not a result (review focus 1). A pull request URL on another repo is `Stuck`. `for_comment` hides a 30-character key and keeps `https://github.com/o/r/pull/123` and a URL with a long path whole. `elapsed_ms` of a later `from` is 0 (review focus 5).

### 10. Repo config [1]

- **Spec:** Repo config, F9.
- **Files:** `crates/foundryd/src/factory/config.rs`.
- **Ports:** the project half of `src/config.ts` (`loadProject` and what it calls) with its tests. Not the machine config, `default_project` or targets.
- **Produces:** `ProjectConfig { instructors, base, provision, gate, required_checks, limits: Limits { minutes, gate_retries, checks_minutes, interruption_resumes }, owner_set: bool }` and `config::parse(yaml: &str) -> Result<ProjectConfig, ConfigErrors>`, with the runner's defaults and error texts.
- **Tests:** the runner's cases for the factory block. `factory.owner` parses, sets `owner_set` and changes nothing else. A file with no factory block is an error that says so.

### 11. Messages [9]

- **Spec:** Architecture (F12, the factory session bullets).
- **Files:** `crates/foundryd/src/factory/messages.rs`.
- **Ports:** `src/runner/messages.ts` with its tests.
- **Produces:** `Message { line: String, body: Option<String> }`. `launch(repo, issue, login, body_path)`, `resumed(...)`, `feedback(...)`, `gate_failed(...)`, `check_failed(...)`, `reminder()`, and the comment texts (`stuck`, `questions`, `unapproved`, status). A launch, resume or feedback line is `/factory-build <owner>/<repo>#<N> <path>`. A gate or check failure line is `The factory gate failed. Read <path>`. The reminder has no body. Output quoted in a comment text is cut to its last 20,000 characters. The launch body says Foundry set `GH_TOKEN` for `<login>`, where the runner's says Paradev started the session with this Mac's `gh` login.
- **Tests:** the runner's cases, with the expected text split into line and body. Every line is under 800 characters and one line. A stuck comment quoting 64 KB of output is under 65,536 characters. A body over 64 KB is cut to its tail. Output lines in a gate failure body each start with `| ` (review focus 3). No message names Paradev.

### 12. GitHub: the trait, `gh` and reads [9]

- **Spec:** `factory::github`, Testing (the fake).
- **Files:** `crates/foundryd/src/factory/github/{mod,gh,read,fake}.rs`, `crates/foundryd/src/factory/github/fixtures/*` (copied from `src/tracker/fixtures/`).
- **Ports:** the reading half of `src/tracker/github.ts` and `src/tracker/approvalHistory.ts`, with their tests.
- **Produces:** `trait Tracker` with the reads the factory uses: ready issues, one issue with labels, assignees and body, approval history, blockers, comments since a time, the pull request for a branch or URL with its head and state, checks on a commit, the default branch's head, and a file's content at a ref. `Gh::new(login, token, clock)` runs `gh` with `GH_TOKEN` in the environment, kills it at 30 seconds, and maps failures to `TrackerError::{Timeout, Auth, RateLimited, NotFound, Other}`. `FakeTracker`, scripted by argv as the runner's tests script `gh`, and usable by other tasks' tests.
- **Tests:** the runner's read cases, same names, from the copied fixtures. A `gh` that sleeps is killed at 30 seconds on the fake clock and is `Timeout` (review focus 4). The token is in no argument.

### 13. GitHub: writes and claims [12]

- **Spec:** A run (claim), F13, Two factories at once.
- **Files:** `crates/foundryd/src/factory/github/write.rs`, `crates/foundryd/src/factory/github/mod.rs`, `crates/foundryd/src/factory/github/fake.rs`.
- **Ports:** the writing half of `src/tracker/github.ts` with its tests.
- **Produces:** on `Tracker`: swap a state label, assign and unassign, post and update comments, mark a pull request ready or draft, ensure the labels exist. `claim_comment(run, machine)` with `<!-- foundry:claim <run id> <machine> -->` as its last line, and `newest_claim(comments, login) -> Option<Claim::{Foundry { run, machine }, Foreign { comment_id }}>`. The status comment keeps the runner's `<!-- softfac:status -->` marker and format. Every comment body goes through the hider the tracker was built with (`secrets::hide_known`) and then `text::for_comment`; the factory's own markers (the claim line, `<!-- softfac:status -->`) are added after that, since `for_comment` would hide the run id and the machine in the claim line.
- **Tests:** the runner's write cases. A comment with Foundry's marker written by another login is not Foundry's claim (review focus 1). The runner's `<!-- softfac:claim … -->` comment is `Foreign`. Of two claim comments the newest counts.

### 14. Workspaces: checkouts, setup kinds and folders by id [8]

- **Spec:** G4, G5, A run (the workspace table), check 10.
- **Files:** `crates/foundryd/src/workspaces/mod.rs`, `crates/foundryd/src/workspaces/tests.rs`.
- **Produces:** `Workspaces::create_with(project, name, Checkout, Setup, factory: Option<FactoryTag>) -> Result<WorkspaceId, WorkspaceError>`. `Checkout::{NewBranch { base, branch: Option<String> }, Branch { name }, Commit { sha }}`. `Setup::{Own, Commands { commands, limit_secs }, None}`. `FactoryTag { project, run: String }`. `create` keeps its behaviour and calls it.
  - A given branch is used exactly, `/` kept, after the runner's `branchRefused` checks. `Branch` fetches and runs `git worktree add -B <name> <dir> origin/<name>`. `Commit` fetches and adds a detached worktree; its record has `branch: ""` and `base: <sha>`.
  - A new worktree's folder is `worktrees/<project>/<workspace id>`. Folders made before stay, and sweep and archive find either.
  - `Commands` runs the list in the setup terminal one by one, each killed at `limit_secs`, stopping at the first that fails; the terminal's exit code is that command's, or 124 for one killed.
  - A workspace with a `FactoryTag` is marked in a file beside its folder, runs its setup terminal with `git_auth::helpers_off_env()` and no token, and a second `create_with` with the same tag and name answers with the first one's id.
  - For a project whose factory is on, the clone and fetch go through `GitAuth` for the project's login.
- **Tests:** against local bare repos. `fac/12-1` is a branch of that name. A second worktree on a branch is refused while the first exists, and `Branch` moves a stale local branch to `origin`'s once the first is archived. `Commit` leaves no branch. A pull request branch named `-x` or `a..b` is refused before git runs (review focus 2). A command that sleeps past its limit is killed and the exit code is 124. The setup terminal's environment holds no token and `GIT_ASKPASS` is unset. The same tag twice makes one workspace. A P1 workspace made before this task still archives.

### 15. Sessions: the factory start [8]

- **Spec:** G12, Architecture (the factory session bullets), checks 1 and 8.
- **Files:** `crates/foundryd/src/sessions/factory.rs`, `crates/foundryd/src/sessions/mod.rs`, `crates/foundryd/src/sessions/claude.rs`, `crates/foundryd/src/daemon.rs` (the `Sessions::new` call gains the secret store).
- **Produces:** `Sessions::start_factory(workspace, worktree, FactoryStart { project, login, model, effort }, first: Message) -> Result<SessionId, SessionError>`. Claude Code is started with `--dangerously-skip-permissions --effort <effort>`; its settings file holds `skipDangerousModePermissionPrompt: true`; its environment holds `GH_TOKEN` and a `GIT_ASKPASS` that answers with it, read from the store at this start. A file in the session's folder marks it a factory session of the project. `Sessions::is_factory(session) -> Option<ProjectId>`, `Sessions::factory_sessions(project) -> Vec<SessionId>`, `Sessions::end_factory(workspace)`, which closes each of the workspace's factory terminals. `adopt` keeps the mark across a restart.
- **Tests:** with the fake agent as `claude`: the argv holds the two flags, the settings file holds the key, the environment holds the token and no log line does. A start for a login the machine holds no token for is refused and leaves nothing behind. After `adopt`, `is_factory` still answers. `end_factory` ends only factory sessions of that workspace. A P1 `start` is unchanged.

### 16. Terminal tail [1]

- **Spec:** G3.
- **Files:** `crates/foundryd/src/terminals.rs`.
- **Produces:** `Terminals::tail(id, max_bytes: usize) -> Result<String, TerminalError>`: the end of the terminal's scrollback and screen as plain text, escape sequences and NUL bytes removed, cut to `max_bytes` on a character boundary, for a running or an exited terminal.
- **Tests:** a child that prints coloured output gives text with no escape byte. Two megabytes of output give at most 64 KB that end with the child's last line. A multi-byte character at the cut is not split (review focus 3). An exited terminal still answers.

### 17. Sessions: turns and messages [4, 15]

- **Spec:** G1, G2 (send), A run (step 3, the resend rule), F12.
- **Files:** `crates/foundryd/src/sessions/turns.rs`, `crates/foundryd/src/sessions/factory.rs`, `crates/foundryd/src/sessions/mod.rs`, `crates/foundryd/src/sessions/hooks.rs`, `crates/foundry-fake-agent/src/main.rs`.
- **Produces:**
  - For a factory session, each `Stop` writes `turns/<n>.txt` in the session's folder (0600) with the whole `last_assistant_message` after `secrets::hide_known`, then writes the session's `SessionTurn` with the new `turn`. Each `UserPromptSubmit` adds one to `prompts`. A `Notification` writes `question.txt` and sets `question`; the next prompt clears it. Every `Stop` writes the record, also one that changes no state. The files go when the workspace is archived.
  - `Sessions::turn_text(session, turn) -> Option<TurnText { message, question }>`.
  - `Sessions::send_factory(session, Message, only_if_prompts: Option<u64>) -> Result<u64, SendRefused::{NotIdle, CountRose { now }, NoSuchSession, BodyWithoutPath, SecretsUnreadable, Failed(String)}>`: writes the body to a 0600 file in the session's folder, types the line with each `{body}` (`BODY_PATH`) replaced by the file's absolute path, and answers with the count of prompts before the send. The path is typed in POSIX single quotes when it holds whitespace or a quote, each `'` in it as `'\''`, and as it is otherwise. The path is read as UTF-8 with any byte that is not valid replaced, so a path that is not UTF-8 is not typed byte for byte. A message with no body writes no file, and its line is typed as it is. The first message of `start_factory` goes the same way. A message is sent again only when it never became a prompt (the resend rule):
    - `NotIdle`: the session is not idle, or its first message is not yet typed. A question is not answered through this. The coordinator may send again once the session is idle.
    - `CountRose { now }`: the caller named a count of prompts, and the session's count, `now`, is above it. Nothing is written or typed: the line sent before was typed after all, so it is not sent again.
    - `NoSuchSession`: the session is not a live factory session of this daemon. There is nothing to send to again.
    - `BodyWithoutPath`: the message has a body, and its line has no `{body}` to name the file. The same message is refused again, so it is not one to send again.
    - `SecretsUnreadable`: the message has a body, and the machine's secret store cannot be read, so its secrets cannot be hidden. Nothing was written or typed, so the message may be sent again.
    - `Failed(String)`: the session's count of prompts could not be read, the body could not be written, the terminal gave an error, or the send did not finish. The reason names no secret. A terminal error can come after the terminal took the line, when its holder does not answer in time or the connection breaks, so a `Failed` send may have been typed: it is sent again only as the resend rule says, with `only_if_prompts`.
  - The fake agent's script gains: `stop` with a `last_assistant_message`, `notify` with a message, and `wait_line`, which blocks until a line is typed and can match its start.
- **Tests:** the turn file is on disk before a reader sees the new `turn`. A final message of 8,000 characters with a pull request URL and a 40-character id is kept whole; a `github:*` token in it is hidden. A `Stop` while already idle still raises `turn`. `send_factory` to a working session is `NotIdle`. A send with `only_if_prompts` below the count is `CountRose` and types nothing. A send whose line was lost (the fake agent never reports a prompt) and is sent again with the same count is typed once more. After a daemon restart, `turn` and `prompts` carry on from their last values. The body file is outside the worktree.

### 18. Worker routes [14, 16, 17]

- **Spec:** G2.
- **Files:** `crates/foundryd/src/api/factory.rs`, `crates/foundryd/src/api/carry.rs`, `crates/foundryd/tests/fleet/call.rs`.
- **Produces:** `POST` routes that answer only a carried call, and `404` to a browser:
  - `/api/factory/workspaces` with `{ machine, project, name, checkout, setup, tag }`, carried to `machine`.
  - `/api/workspaces/{id}/factory/session` with `{ project, login, model, effort, message }`.
  - `/api/workspaces/{id}/factory/end`. A forced archive uses P1's carried archive route as it is.
  - `/api/sessions/{id}/factory/message` with `{ message, only_if_prompts }`, answering `{ prompts }` or `409` with the reason.
  - `/api/sessions/{id}/factory/turn` with `{ turn }`, answering `{ message, question }`.
  - `/api/terminals/{id}/tail`, answering text of at most 64 KB.
  `carry` learns the session target, found through the session's record.
- **Tests:** in the fleet harness, each route called on machine A for something machine B owns reaches B and the answer comes back. A call without the carried mark is `404`. No answer holds a token.

### 19. The worker seam [2]

- **Spec:** F3, Architecture.
- **Files:** `crates/foundryd/src/factory/worker.rs`.
- **Produces:** what `runs` asks of the fleet, so the port can be tested with no daemon:
  - `trait Worker` with `create(machine, CreateWorkspace) -> WorkspaceId`, `start_session(workspace, FactoryStart, Message) -> SessionId`, `send(session, Message, only_if_prompts) -> Result<u64, SendRefused>`, `turn(session, n) -> TurnText`, `tail(terminal) -> String`, `end_sessions(workspace)`, `archive(workspace)` (forced).
  - `trait Guard { fn check(&self) -> Result<u64, LeaseLost>; }`, which answers with the lease generation. Code that changes anything on GitHub or asks anything of a worker calls it first. Task 21 gives the real one; `AlwaysHeld(generation)` and `Lost` are here for tests.
  - `FleetView`: a snapshot read from the store for one pass. `workspace_named(project, name)`, `session_of(workspace)`, `session_state(session)`, `terminal(terminal) -> (TerminalState, exit_code)`, `session_turn(session)`, `online(machine)`, `unreachable_for(machine) -> Duration`, `samples()`, `held(machine)`.
  - `FakeWorker` and a `FleetView` builder for tests, scripted like the runner's fake sessions.
- **Tests:** the fake records each call in order. `FleetView::workspace_named` ignores an archived workspace and finds a factory workspace whose record arrived after its create was sent.

### 20. Ledger [4, 9]

- **Spec:** Factory records (`run_event`), F5, F6.
- **Files:** `crates/foundryd/src/factory/ledger/{mod,events,read,list}.rs`.
- **Ports:** `src/ledger/{events,ledger,read,listRuns,runId,time,limits}.ts` with their tests. Not `appendFile.ts` or `machineLog.ts`: the store replaces the files.
- **Produces:** the runner's run events as typed values that serialise to the runner's `event` and `data`. `Ledger::write(run, event, cause)`, which stamps the lease generation it is given and the time, hides known secrets, cuts output to 64 KB, and stores a `run_event`. `Ledger::read(run) -> Vec<RunEvent>` in order. `list::runs(repo) -> Vec<RunInfo>` and `holds_slot(&RunInfo)`.
- **Tests:** the runner's cases, same names, against the store. Events written by two machines at generations 1 and 2 read in that order with the second machine's clock an hour behind. An event's output over 64 KB is cut to its tail.

### 21. The lease [3, 8, 19]

- **Spec:** The lease, checks 4 and 5.
- **Files:** `crates/foundryd/src/factory/lease.rs`.
- **Produces:** `Lease::new(project, remote_url, git_auth, clock, me, lease_dir)`. `tick(highest_seen_generation) -> LeaseState::{Held { generation }, HeldBy { machine, generation }, NoToken}`: fetches only `refs/factory/lease` into a bare directory that is no clone, renews when it holds and 60 seconds have passed, takes a missing ref at once at one more than the highest generation seen, takes over after seeing one commit unchanged for 180 seconds with the generation raised by one, and renews at once when the ref already names this machine. Each lease commit has no parent and a message of holder, generation and renewal count. `lease.guard()` gives the real `Guard` of task 19, whose `check` passes only while less than 120 seconds have passed since the start of the last successful push. A push is killed at 30 seconds. `lease.release()` for a machine that stops acting.
- **Tests:** against a local bare repo with the fake clock, each case of the spec's lease tests: two machines racing, a takeover after the wait, a first take of a missing ref, a ref deleted under a holder and taken at a higher generation, a restart that renews its own lease, a holder that lost a push race and whose `check` fails at once, a clock that jumps 20 minutes and a `check` that fails, a push whose answer comes at 50 seconds with the age still counted from its start, a split where one side's pushes fail. The lease directory never holds a branch of the repo.

### 22. Runs: progress, claim and launch [3, 10, 11, 13, 19, 20]

- **Spec:** A run (steps 1 to 3), F7, When things go wrong (records before it acts).
- **Files:** `crates/foundryd/src/factory/runs/{mod,progress,start}.rs`.
- **Ports:** `src/runner/{progress,run,start,launchTarget}.ts` and `src/claims.ts` with their tests.
- **Produces:** `Seen`, a run's progress read from its events, with `worked_ms` counted through `text::elapsed_ms` and less the time gates ran and checks were waited for. `RunContext { tracker, worker, view, ledger, guard, project, config, settings, clock }`. `start::claim(ctx, issue, machine) -> ClaimOutcome`: the claim comment first, then the label, the assignee and the status comment. `start::advance(ctx, run) -> StartStep`: asks for the build workspace, looks at its setup terminal on later passes, makes the run stuck with the terminal's tail when setup failed, checks the approval again before the session starts and ends the run as the runner refuses an unapproved issue when it was taken back, starts the session with the launch message, moves the label to `building`, and asks for the launch again by count when the session is idle a pass later.
- **Tests:** the runner's `start.test.ts` and `progress.test.ts` cases, with the fakes of tasks 12 and 19. A claim cut short after the comment leaves the issue `ready`, and the next claim's comment is the newest. A claim with no workspace is launched on the machine its comment names. A create whose record arrives a pass later is not asked for twice. The approval is checked before the session starts, not after. An issue set `ready` again that has an open pull request of the project's login carries on from that pull request's branch, as the runner's `pr_adopted` does. A wall clock that steps back adds no worked time (review focus 5).

### 23. Runs: building [22]

- **Spec:** A run (step 4), the `limits.minutes` paragraph.
- **Files:** `crates/foundryd/src/factory/runs/building.rs`.
- **Ports:** the result, reminder, questions and time limit parts of `src/runner/building.ts` with their tests.
- **Produces:** `building::advance(ctx, run) -> BuildStep`. It reads a turn's final message once per turn number. `Done` moves the run to the gate; `NeedsInfo` posts the questions, ends the session, archives the workspace by force and ends the run; any other stop is stuck. A turn with no result line gets the reminder once, then stuck. A session that waits for a person is stuck at once with its question. A session P1 marks stuck is left alone. `limits.minutes` ends a run in `triage` or `building`, also on a gate or check failure sent back.
- **Tests:** the runner's cases. A turn number already recorded is not acted on again. A build session idle through a whole gate is sent nothing. A session that hangs on a gate failure sent back is ended by `limits.minutes`, where the runner would not end it.

### 24. Runs: interruptions and resumes [23]

- **Spec:** When things go wrong (interruptions, the claim whose machine is gone), A run (the workspace table).
- **Files:** `crates/foundryd/src/factory/runs/resume.rs`.
- **Ports:** `src/runner/relaunch.ts` and the interruption parts of `src/runner/building.ts`, with `interruption.test.ts`.
- **Produces:** `resume::advance(ctx, run) -> ResumeStep`. A session whose process ended before a result line, or whose machine has been unreachable for 15 minutes, is an interruption. One pass records the archive of the earlier workspace; a later pass asks for `fac/<N>-<attempt>-r<k>` once it shows archived or its machine has been unreachable for 15 minutes, on the pull request's branch when pushed and else on a new branch from the base, up to `interruption_resumes`, then stuck. A claim whose named machine is unreachable for 15 minutes with no workspace is launched on another machine and spends no resume. `orphans(view, run) -> Vec<WorkspaceId>`: workspaces of a run that has moved on, for a machine that is heard from again.
- **Tests:** the runner's interruption cases. The archive and the new workspace are never asked for in one pass. A pull request branch named `-x` is refused before any workspace is asked for (review focus 2). A resume on the machine that built the run moves the local branch to `origin`'s. The cap ends the run stuck.

### 25. Placing runs and gates [7, 19]

- **Spec:** A run (claim, gate), Shared settings (the three bullets), What P1 gains (the last paragraph).
- **Files:** `crates/foundryd/src/factory/placing.rs`.
- **Produces:** `place_run(view, limits, login, reserved: &mut Reserved) -> Result<MachineId, Vec<String>>`: among online machines that hold the login, have the skill and have published a `MachineHeld`, P1's `placement::place`, with each machine already picked this pass counted one session fuller. `place_gate(...)`: the same without the session limit. `slots(runs, settings) -> Slots { runs_free, gates_free }`, counted as `holds_slot` counts.
- **Tests:** a machine with no token for the login is never picked and its reason names the login. A machine with no `MachineHeld` (protocol 4) is never picked. Two claims in one pass do not both land on the machine with one free session. A gate is placed on a machine at its session limit.

### 26. The gate [10, 19, 20]

- **Spec:** A run (step 5, first paragraph), When things go wrong (the last two bullets).
- **Files:** `crates/foundryd/src/factory/gate.rs`.
- **Ports:** the command, result and wording parts of `src/gate.ts` with their tests. Not its checkout, which task 14 does.
- **Produces:** `gate::request(config, sha, run, k) -> CreateWorkspace` named `fac/<N>-<attempt>-g<k>`, with `Checkout::Commit` and `Setup::Commands` of the default branch's `provision` then `gate`, 30 minutes each. `gate::result(terminal_state, exit_code, tail) -> GateResult::{Running, Passed, Failed { command, how, output }}`. `gate::could_not_run(...)` for a create that failed or a machine gone for 5 minutes, and `backoff(failures) -> Option<u32>` giving 1, 2, 4, 8 passes then `None`. `gate::config_differs(default, at_head) -> bool`.
- **Tests:** the runner's cases for results and wording. The commands come from the config it is given, never from the worktree. Exit code 124 reads as "ran longer than 30 minutes". The fifth failure to run gives `None`. A failed `provision` is `Failed`, not "could not run".

### 27. Runs: gating [23, 25, 26]

- **Spec:** A run (step 5).
- **Files:** `crates/foundryd/src/factory/runs/gating.rs`.
- **Ports:** `src/runner/gating.ts` with `gating.test.ts`.
- **Produces:** `gating::advance(ctx, run, slots) -> GateStep`: places a gate while a gate slot is free, records the commit gated, reads the result on later passes, archives the gate workspace by force and keeps its tail in an event. On a pass it waits for `required_checks` on that commit up to `checks_minutes`, sends a failed check back at once, gates a moved head again, and on success marks the pull request ready, moves the label to `pr-open`, ends the build session and archives its workspace by force. On a fail it sends the output back to the session. `gate_retries` is one counter per round, spent by every gate after the round's first. With none left, a failed gate or a moved head is stuck. A failure sent back and a stuck comment say when `.dw/config.yml` at the head differs from the default branch's.
- **Tests:** the runner's cases. A gate failure fixed in one cycle spends one retry. A head that keeps moving ends stuck. Reaching `pr-open` leaves no live session and no workspace of the run. A gate that could not run backs off and is not counted against `gate_retries`.

### 28. Runs: feedback, stop and the end [27]

- **Spec:** A run (steps 6 and 7), When things go wrong (a person's edit, Stop, stuck workspaces).
- **Files:** `crates/foundryd/src/factory/runs/feedback.rs`, `crates/foundryd/src/factory/runs/stop.rs`.
- **Ports:** `src/runner/feedback.ts`, `src/runner/stop.ts` and `src/control.ts`, with `feedback.test.ts`, `forward.test.ts` and `stop.test.ts`.
- **Produces:** `feedback::advance(ctx, run, slots, reserved) -> FeedbackStep`: every `feedback_seconds` it reads new instructor comments; a round starts only when a run slot is free and `place_run` has a machine, else it is `Waiting { reason }` and the issue stays `pr-open`; a round moves the label to `building`, asks for `fac/<N>-<attempt>-f<k>` on the pull request's branch and starts a session with the resumed message; `minutes`, `gate_retries` and `interruption_resumes` start again. `stop::request(run)` and `stop::advance(ctx, run)`: a requested stop is stuck on the next pass. `stop::reconcile(ctx, run)`: a merged pull request or closed issue ends the run and archives its workspaces by force; an edited title, a removed label or an assignee stops it as the runner does. A stuck run's session is ended and its workspace kept until the issue leaves `stuck`.
- **Tests:** the runner's cases. A round with every run slot held waits in `pr-open` and adds nothing to worked time. A pull request branch named `a..b` is refused before any workspace is asked for (review focus 2). An issue the runner left in `pr-open`, with no Foundry run, is not read for feedback. A `gh` failure in the middle of a round leaves it to be carried on, with no second session (review focus 4).

### 29. Factory status [2, 3, 4]

- **Spec:** Factory records (`factory_status`), F10, Attention items in P2.
- **Files:** `crates/foundryd/src/factory/status.rs`.
- **Produces:** `status::believed(statuses, online, acting) -> Option<FactoryStatus>`: among writers that are online, the one that holds the lease at the highest generation, else the acting coordinator's. `Freshness`: per project, when this machine last held the lease itself (`held(project)`, called on each successful push) or received a holding status with a higher `pass` at the highest generation it has seen, on the task 3 clock; `since_holding(project) -> Duration`. `status::build(...)` from a pass's results, and `status::not_holding(project, NotHolding)`.
- **Tests:** a dead holder's status at generation 5 is not believed over the acting coordinator's "no token". A holding status beats a not-holding one at the same generation. A not-holding status never resets `since_holding`.

### 30. The pass and the service [6, 18, 21, 24, 28, 29]

- **Spec:** A run (step 1), `factory::pass`, Two factories at once, When things go wrong (takeover), F2.
- **Files:** `crates/foundryd/src/factory/pass.rs`, `crates/foundryd/src/factory/fleet.rs`, `crates/foundryd/src/factory/mod.rs`, `crates/foundryd/src/daemon.rs` (one line under `// services`).
- **Ports:** `src/pass.ts` and `src/cooldown.ts` as far as they apply, with their tests. Not hosts, cloud, the Paradev hold or the belt.
- **Produces:** `FleetWorker`, the real `Worker`: a direct call when the machine is this one, else `Peers::call` to the routes of task 18. `Factory::run(...)`: while this machine acts as coordinator and speaks protocol 5, for each project whose factory is on, a lease task that ticks every 10 seconds and calls `Freshness::held` on each successful push, and a pass every `poll_seconds`. A machine that stops acting releases its leases. `pass::run(project) -> PassReport`:
  1. Reads `.dw/config.yml` from the default branch through the tracker, again only when the head moved.
  2. Without the lease, writes a not-holding status and stops.
  3. Finds runs by Foundry's claim comments and by workspace names before any event, and carries each on.
  4. Lists a foreign claim for an issue in `triage` or `building` whose newest claim comment is not Foundry's, and leaves it alone.
  5. Advances every run through tasks 22 to 28.
  6. Claims ready issues while a slot is free, claims are not paused and `place_run` has a machine, oldest first, as the runner orders them.
  7. Has a machine that is heard from again archive the orphans of task 24.
  8. Writes the holding status.
  A tracker error ends that project's pass and the next project still runs; three failed passes running set the status's idle reason.
- **Tests:** the runner's `pass.test.ts` cases that apply. With the fakes: a project with no token writes "no token" and touches nothing on GitHub. A `Guard` that fails in the middle of a pass stops every further change. A paused project claims nothing and its runs carry on. A foreign claim is listed and its issue untouched. A slow `gh` in one project does not hold up the next (review focus 4).

### 31. Cut-off [15, 29, 30]

- **Spec:** G10, When things go wrong (the second bullet and its five cases).
- **Files:** `crates/foundryd/src/factory/cutoff.rs`, `crates/foundryd/src/daemon.rs` (one line under `// services`).
- **Produces:** `cutoff::run(...)`: every 30 seconds, for each project this machine has factory sessions of, ends them through `Sessions::end_factory` when `Freshness::since_holding(project)` is 10 minutes or more, which covers both holding the lease and hearing from its holder. A project's timer starts when its first factory session starts.
- **Tests:** with the fake clock: a machine with no holding status for 10 minutes ends its sessions and one at 9 minutes does not. A machine that holds the lease ends nothing with no peer at all. A not-holding status from its own coordinator saves nothing. A clock that jumps 20 minutes ends the sessions on the next tick. A P1 session is never ended.

### 32. Factory attention items [29]

- **Spec:** Attention items in P2, G13.
- **Files:** `crates/foundryd/src/attention.rs`.
- **Produces:** the five items, worked out on every machine from the believed status and `Freshness`: run stuck, run needs info, factory idle, foreign claim, gate infra. P1's stuck, failed and waiting session items and its setup failed item are not raised for a session or workspace a believed status names or that this machine marks as factory.
- **Tests:** one test for each row's raise and one for its clear. "Factory idle" is raised after 10 minutes with no holding status for a project whose factory is on, and not for one whose factory is off. A factory session that P1 marks stuck raises nothing; a P1 session still does.

### 33. Factory view routes and settings [18, 30]

- **Spec:** G8, UI, GitHub tokens (the default login), Scope (HTTPS only), When things go wrong (Stop).
- **Files:** `crates/foundryd/src/api/factory.rs`, `crates/foundryd/src/api/carry.rs`, `crates/foundryd/src/api/mod.rs` (the events socket).
- **Produces:** `GET /api/factory`: each project's believed status, its factory setting, and the fleet's factory settings. `GET /api/factory/runs/{id}`: the run's events in order. `POST /api/factory/runs/{id}/stop`, carried to the machine whose status holds the lease. `POST /api/factory/settings` and `POST /api/projects/{id}/factory`, sent through the coordinator like P1's project edits; turning a project on is refused unless its URL starts `https://github.com/`, and writes the coordinator's default login when the edit names none. The events socket sends a `UiMessage::FactoryStatus` when a believed status changes, and never a run event.
- **Tests:** `GET /api/factory` on a machine that is not the coordinator gives the holder's status. Stop sent to a machine that is not the holder reaches the holder. Turning on a project with an SSH URL is `422` with the reason. `GET /api/state` and `/api/events` hold no run event. No answer holds a token.

### 34. Web: the Factory view and its settings [33]

- **Spec:** UI.
- **Files:** `web/src/Factory.tsx`, `web/src/FactorySettings.tsx`, `web/src/factory.ts`, `web/src/Projects.tsx`, `web/src/api.ts`, `web/src/Rail.tsx`, `web/src/App.tsx`, their tests.
- **Produces:** the Projects dialog's factory switch, login and pause for each project. A Factory view at `/factory`, beside the board, timeline and machines: a header with the fleet's settings, and for each project its lease holder and generation or why it is idle, a line when `factory.owner` is set and ignored, the ready queue with each issue's reason, feedback rounds that wait, live runs each with a link to its workspace and Stop, and the last 20 finished runs. A stuck run's row says its workspace's uncommitted work goes when the issue leaves `stuck`.
- **Tests:** component tests from fixed statuses for a holding project, an idle one with each reason, and an empty one. Stop asks before it sends. A setting out of range is refused in the form with the allowed range.

### 35. Web: factory runs on the board [32, 34]

- **Spec:** UI (the last bullet), G14.
- **Files:** `web/src/Board.tsx`, `web/src/Sidebar.tsx`, `web/src/api.ts`, their tests, `web/e2e/ui.spec.ts`.
- **Produces:** a workspace a believed status names shows a "factory" mark and a link to its issue. A factory session P1 marks stuck stays in the working lane, shown as quiet with how long. A run's card moves to "needs you" only for a stuck or needs-info run. A workspace with an empty branch shows its commit.
- **Tests:** component tests for each rule. The e2e test opens the Factory view on a daemon with no factory project and sees the empty state.

### 36. Export [20]

- **Spec:** Factory records (the export), Testing (the export test).
- **Files:** `crates/foundryd/src/factory/ledger/export.rs`, `crates/foundryd/tests/fixtures/runner/README.md`, `crates/foundryd/tests/cli.rs` (its own command's test).
- **Produces:** `foundryd factory export <dir>`: every run this machine holds as `runs/<owner>__<repo>__<N>__<attempt>.jsonl`, one line per event with the runner's keys in the runner's key order, in generation then version order. The lease generation is not written.
- **Tests:** the events of the run file in `tests/fixtures/runner/` (committed before this issue is filed, see "Not factory issues"), loaded into a store and exported, equal that file byte for byte. A run written by two machines exports in order.

### 37. Fleet tests: a run from claim to `pr-open` [30, 31]

- **Spec:** Testing (the fleet tests).
- **Files:** `crates/foundryd/tests/fleet/factory/{mod,github,build}.rs`.
- **Produces:** the factory harness: three daemons with a local bare repo as each project's `origin` and lease remote, a `FakeTracker` shared by the daemons that keeps issues, labels, comments and pull requests in memory, a fake clock, and the fake agent scripted per run. Tests:
  - an issue reaching `pr-open`, with its build session ended and its workspace gone
  - a failed gate sent back and passed on the retry, spending one of `gate_retries`
  - needs-info
  - a feedback round that waits while every run slot is held, then starts
  - a feedback round on the machine that built the run
  - a pull request whose branch changes `provision` and `gate`: the default branch's commands ran, and the failure sent back says the configs differ
  - a gate terminal whose environment holds no token and whose `git` gets none from a helper set for the machine
  - a token and a made-up 30-character key in gate output and in a session's questions: the token is in no comment, message or event; the key is hidden in the comment and whole in the event; a `github.com` pull request or commit link in the same comment is whole
- **Tests:** those.

### 38. Fleet tests: coordinators and leases [37]

- **Files:** `crates/foundryd/tests/fleet/factory/failover.rs`.
- **Tests:** the coordinator killed during a build and the next carrying the run on, with its events in order. A coordinator killed between the claim comment and the label, and one between the label and the status comment. One killed between the claim and its first event: with no workspace, with a workspace, and with a workspace whose record arrives a pass later. None is taken for a foreign claim. A split where only one side acts and the other side's status keeps no session alive. A lone coordinator hosting its own run and ending nothing. The Factory view, its attention items and Stop from a machine that is not the coordinator. A peer on protocol 4 that receives no factory record, then updates and receives them all.

### 39. Fleet tests: workers [37]

- **Files:** `crates/foundryd/tests/fleet/factory/workers.rs`.
- **Tests:** a worker's daemon restarted before the launch message was typed, and the message sent again. A turn that ends before its `session_turn` reaches the coordinator, with the message not typed twice. A worker's daemon restarted during a build, and the result line still read. A worker cut off from the lease holder ending its session, and the run resuming elsewhere. A claim whose machine never answers, launched on another. A gate whose machine goes offline, run again on another. A gate command that hangs, killed at its limit, counted as a gate failure with its slot freed. A session that hangs on a gate failure sent back, ended by `limits.minutes`.

### 40. Contract test for a factory session [17]

- **Spec:** Testing (the last bullet), checks 1, 2 and 6.
- **Files:** `crates/foundryd/tests/claude_contract.rs`, `docs/claude-code-contract.md`.
- **Produces:** one more ignored test, run with `FOUNDRY_CONTRACT=1`: a factory start in a fresh config folder with a first message whose body asks for a 150-line reply ending in a result line.
- **Tests:** the session starts with no confirmation. `UserPromptSubmit` reports the typed line exactly. The turn file equals the whole reply and ends in the result line. The doc records the Claude Code version and what was seen.

### 41. Install and cutover docs [33]

- **Spec:** Before P2 starts, Cutover, GitHub tokens.
- **Files:** `docs/install.md`, `README.md`, `crates/foundryd/tests/cli.rs`.
- **Produces:** how to set a machine's tokens and default login, how to install the `factory-build` skill on a machine, how to turn a project's factory on, the cutover steps for one repo with the way back, and the `retro` comparison command over an export.
- **Tests:** a test reads `docs/install.md` and checks every `foundryd` command it names is one `foundryd --help` lists.

## Not factory issues

Done by a person, or outside this repo.

- **#87** closes, and **P1's acceptance run** passes and is recorded in `docs/acceptance/P1.md`. Both are needed before the cutover, not before the issues are filed.
- **The `factory-build` skill.** Its source moves to `~/EA/skills/factory-build`. It reads its brief from the file on its command line, skips its "GitHub account" step when the brief says Foundry set `GH_TOKEN`, and reads a gate failure from the file the line names. factory-on-paradev's `plugins/factory/skills/factory/scripts/check.sh` changes with it. It is installed on every machine before that machine's `MachineHeld` can say it has the skill.
- **The runner fixture for task 36.** One finished run file from `~/.config/factory-runner/runs/`, read for anything private, committed as `crates/foundryd/tests/fixtures/runner/<run id>.jsonl` with the runner commit beside it, before task 36 is filed.
- **The open checks.** Check 4's workflow question, check 5, check 7's two leftovers, and the Linux halves of checks 3 and 9. A failure of check 5 changes only the lease's numbers in task 21.
- **The cutover**, steps 3 to 7 of the spec.

## Order

```
1 ── 2 ── 4 ─┬─ 5 ── 6
             ├─ 7 ── 8 ─┬─ 14
             │          └─ 15 ── 17 (with 4)
             └─ 20 (with 9) ── 36
1 ── 3        1 ── 10        1 ── 16
1 ── 9 ─┬─ 11
        └─ 12 ── 13
2 ── 19       7, 19 ── 25       2, 3, 4 ── 29 ── 32
3, 8, 19 ── 21
14, 16, 17 ── 18
3, 10, 11, 13, 19, 20 ── 22 ── 23 ── 24
10, 19, 20 ── 26
23, 25, 26 ── 27 ── 28
6, 18, 21, 24, 28, 29 ── 30 ── 33 ─┬─ 34 ── 35 (with 32)
                                   └─ 41
15, 29, 30 ── 31
30, 31 ── 37 ─┬─ 38
              └─ 39
17 ── 40
```

Three lines can build at once from the start: the P1 gains (2, 4 to 8, 14 to 18), the pure port (9 to 13), and the small pieces that depend on neither (3, 10, 16). The port of the runs (22 to 28) needs only the seams of tasks 19 and 20 and the fakes, so it does not wait for the worker routes. Everything meets at task 30.
