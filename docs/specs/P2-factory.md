# P2: Factory

Product: Foundry, a dark software factory that also runs manual sessions.

Status: design, written 2026-11-22 and revised the same day after a review against P1's code on `main` (v0.2.0). Nothing here is built. The plan comes after the checks at the end of this spec and after P1's open fleet and session fixes land.

## Why

The Paradev runner (`original-solutions/factory-on-paradev`) builds every factory issue today, Foundry's own included. It depends on Paradev's CLI and app state, which is the dependency Foundry exists to remove. P1 gave Foundry its own machines, workspaces, sessions, placement and sync. P2 puts the factory on top of them, so the runner can be turned off.

## Goal

`foundryd` claims `factory:ready` issues, builds each one in a Foundry workspace on whichever machine placement picks, gates the draft pull request on a fresh checkout and carries it to `factory:pr-open`, the way the runner does now. Every run is a workspace with terminals, so it shows on the board, the timeline and the attention list while it runs.

Exit criterion: every repo the runner watches has moved to Foundry, foundry first, and the runner has then been stopped for one week. Over that week, the runner's `retro` over Foundry's exported runs shows a gate pass rate and a stuck rate no worse than over the runner's last 30 runs.

## Scope

In:

- The runner's behaviour, ported to a `factory` module of `foundryd`: the label lifecycle, instructor approval, dependency links and `## Base`, claims and claim resumes, the launch message and `/factory-build`, the result line, the gate with retries sent back to the same session, required checks, feedback from PR and issue comments, needs-info, stuck, interruption resumes, the limits in `.dw/config.yml`, pausing claims and stopping a run.
- Run events in the runner's ledger format, and an export the runner's `retro` reads.
- A factory switch per project, with the GitHub login the project acts as.
- GitHub tokens per machine, in each machine's own secret store.
- A lease per project, a ref in the project's repo.
- Factory attention items and a Factory view in the UI.
- The additions to P1 that the factory needs, listed under "What P1 gains".

Out, and where it goes:

- The review loop to 5 out of 5, the merge setting and the merge queue: P2b, with its own spec. P1's "Decisions already made for later pieces" keeps what the brainstorm settled for them.
- A native `retro`: later. The exit week uses the runner's.
- Linear: P4.
- Cloud targets, `default_project` inference, model fallbacks, `factory.owner` and the belt page: dropped. The Foundry UI replaces the belt.

Where this spec says "as the runner does" or is silent on a rule of the lifecycle, the runner's code and tests at commit `8ed9bef` decide.

## Decisions

| # | Question | Decision |
|---|---|---|
| F1 | Port or adapter | Port the runner's core to Rust inside `foundryd`. The runner's tests are the behaviour spec. Driving `foundryd` from the TypeScript runner through a new adapter was rejected: it keeps a second process, store and updater, and pins the factory to one machine while the coordinator moves |
| F2 | Who runs the factory | The acting coordinator, for each project whose lease it holds. Builds and gates run on any machine placement picks |
| F3 | How a run is hosted | A P1 workspace with a P1 session. A gate is a P1 workspace with a shell terminal. There is no second way of starting work |
| F4 | GitHub logins | Each machine holds tokens for the logins it can act as, and names one as its default. Turning a project's factory on writes the login it acts as into the project's setting, taken from the coordinator's default at that moment and editable. Tokens never leave the machine that holds them |
| F5 | Where an issue's state lives | GitHub: labels, the claim comment, the status comment, the pull request. Run steps are synced `run_event` records. A new coordinator rebuilds its view from both and needs nothing handed over |
| F6 | Order of run events | By lease generation, then by the writing machine's record version. No clock is compared |
| F7 | When to claim | Only when placement can place the run now. The runner claims first and waits for a slot; #96 sat in `triage` for 72 minutes on 2026-11-21 |
| F8 | Cutover order | foundry first, then the runner's other repos one at a time |
| F9 | `factory.owner` | Ignored, with a line in the Factory view when a repo sets it. The lease decides which machine acts. An owner that is not the acting coordinator would leave the project idle for good |

## Architecture

```
acting coordinator                        any machine placement picks
┌─────────────────────────────┐           ┌───────────────────────────────┐
│ factory::pass, every poll   │──create──►│ workspace fac/<N>-<attempt>   │
│  ├ lease (refs/factory/)    │  (peer    │  └ session: claude with the   │
│  ├ github (gh, token)       │   link)   │    launch message, GH_TOKEN,  │
│  ├ lifecycle (pure)         │◄─records──│    skipped permissions        │
│  ├ runs                     │  (sync)   │                               │
│  └ ledger (run events)      │◄─calls────│ each turn's final message,    │
│                             │  (peer    │ kept on disk by its machine   │
│                             │   link)   │                               │
│                             │──create──►│ gate workspace                │
└─────────────────────────────┘           │  └ shell terminal: provision  │
                                          │    and gate; exit code from   │
                                          │    the holder                 │
                                          └───────────────────────────────┘
```

| Unit | Job | Ported from (`factory-on-paradev/src/`) |
|---|---|---|
| `factory::lifecycle` | Pure functions: the state machine, approval, `## Depends on`, `## Base`, the result line | `domain/` |
| `factory::github` | Issues, labels, comments, pull requests, checks and dependency links through `gh`, behind a trait | `tracker/` |
| `factory::lease` | Takes, renews and drops a project's lease | New |
| `factory::runs` | One run's steps: claim, place, launch, watch, gate, feedback, stop, resume | `runner/`, `claims.ts` |
| `factory::gate` | The gate in a fresh worktree of the pull request's head, with the runner's git hardening | `gate.ts`, `gitHardening.ts` |
| `factory::ledger` | `run_event` records and the export | `ledger/` |
| `factory::pass` | Each pass: reconcile with GitHub, advance the runs, then claim if placement has room | `pass.ts` |

`factory` depends on P1's `placement`, `workspaces`, `sessions`, `sync`, `coordinator`, `store` and `attention`. Nothing in P1 depends on `factory`.

A factory session is a P1 session with three additions:

- Claude Code starts with `--dangerously-skip-permissions`, and with the model and effort from the factory settings.
- `GH_TOKEN` is set from the worker's own secret store for the project's login.
- The launch message is its first prompt, typed as P1 types any first prompt. It carries the runner's text: `/factory-build <owner>/<repo>#<N>`, the rules for the branch and the push, and the `gh api user --jq .login` check. Its sentences about Paradev's `gh` login are rewritten to say Foundry set `GH_TOKEN` for the project's login.

## What P1 gains

P1 as built on `main` cannot carry a run that lives on another machine than the factory. Each item below is a change to a P1 unit, built before the factory unit that uses it.

| # | Unit | Today | Gains |
|---|---|---|---|
| G1 | `sessions` | The Stop hook's last message is kept in memory only, cut to 600 characters, never leaves its machine, and has every word of 24 or more mixed letters and digits hidden, which can break a URL | Each turn's whole final message is written to disk by the owning machine under a turn number that only rises. The session's record carries the newest turn number. Any machine reads a turn's message over the peer link by session id and turn number. Only known secrets are hidden: the gateway keys, session tokens and every `github:*` token the machine holds |
| G2 | `api` (carried routes) | Carries workspace create, sessions, shells, archive, and terminal close | Also carries "send a prompt to a session", typed as P1 types a first prompt and refused unless the session is idle |
| G3 | `api`, `terminals` | A terminal of another machine can only be read as a raw byte stream | A carried read of the last lines of a terminal as plain text, escapes removed, at most 64 KB |
| G4 | `workspaces` | `create` always makes a new branch from `origin/<base>`, refuses a branch that exists, and runs `factory.provision` from the worktree's own `.dw/config.yml` | Three ways to make a worktree: a new branch from a base (as now), an existing branch of `origin` (a local branch of that name is moved to it), and one commit with no branch. Setup is one of: the worktree's own provision (as now), a command list the caller gives, or none |
| G5 | `workspaces` | git runs with whatever credentials the machine has | For a project with a login, every git call of that project on that machine gets the login's token through `GIT_ASKPASS`, with other credential helpers turned off for the call |
| G6 | `attention` | Only "setting overwritten" can be dismissed | "Foreign claim" can be dismissed too, per issue and marker comment, so a later pass does not raise the same one again |
| G7 | `foundry-proto`, `sync`, `store` | A record of an unknown kind is an error, in the store and on the link | `PROTOCOL` goes up by one. `run_event` records, the new shared settings and the new machine fields are sent only to a peer that speaks the new protocol |
| G8 | `api` | `GET /api/state` returns every record | `run_event` records are left out of it and served by `GET /api/factory/runs` and `GET /api/factory/runs/{id}` |
| G9 | `machine` | The machine's record has load, memory, disk and sessions | It also lists the GitHub logins the machine holds, so `foundryd secret set github:<login>` shows on every machine within one sample |
| G10 | `sessions`, `workspaces` | A session cut off from the fleet runs on | A machine that has reached no peer for 10 minutes ends its factory sessions (see "When things go wrong") |

A fleet in the middle of an update has machines on both protocols. The factory runs only when the acting coordinator speaks the new one, and it places runs and gates only on machines that do.

## Config

### Repo config

The coordinator reads `.dw/config.yml` from the project's default branch through `gh api`, as the runner does, and reads it again only when the branch's head moves. It needs no clone. The keys and rules are the runner's: `factory.instructors`, `base`, `provision`, `gate`, `required_checks`, `autonomy` (only `pr`), and `limits` with `minutes`, `gate_retries`, `checks_minutes` and `interruption_resumes`. A file that does not load leaves the project idle with an attention item.

### Shared settings

Changed through the acting coordinator, like every P1 shared setting.

| Setting | Scope | Default |
|---|---|---|
| factory on | project | off |
| GitHub login | project | written when the factory is turned on |
| claims paused | project, and fleet | not paused |
| `poll_seconds` | fleet | 30, from 10 to 60 |
| `feedback_seconds` | fleet | 600 |
| `max_runs` | fleet | 3 |
| `max_gates` | fleet | 2 |
| model, effort | fleet | `opus`, `high` |

A run holds one of `max_runs` from its claim until its session reports its pull request or the run ends, as the runner counts it. A gate holds one of `max_gates` while it runs. Both sit on top of each machine's P1 limits. A pass that claims more than one issue counts each machine it picked as one session fuller for the rest of the pass, as P1's placement does for creates in flight.

### GitHub tokens

- `foundryd secret set github:<login>` stores a token in the machine's secret store. `foundryd github default <login>` names the machine's default, which is only read when a project's factory is turned on from that machine as coordinator.
- A project's login is a shared setting, so it does not change when the coordinator role moves. A coordinator that holds no token for it leaves the project idle and says so.
- A token reaches a child process only through its environment: never an argument, a log line, an error, a record or the UI.
- Every `github:*` token and gateway key a machine holds is hidden from all text the factory records or posts: comments, gate and setup output, messages sent to a session and `run_event` data.
- Commits keep the worktree's git identity, as the runner's do.

## A run

1. **Pass.** The coordinator holds the project's lease and reads its `factory:ready` issues. As the runner does, it skips an issue that no instructor approved, that has an assignee, or that has an open blocker.
2. **Claim.** Only when placement has a machine under its limits that holds the project's login, and a run slot is free. The claim swaps `ready` for `triage`, assigns the login, and posts the claim comment and status comment in the runner's format, naming the machine. `limits.minutes` counts from here, so a setup that hangs ends the run.
3. **Launch.** The coordinator asks that machine, over the peer link, for workspace `fac/<N>-<attempt>` on a new branch from `## Base`, or from `factory.base` when the issue has none, with the worktree's own provision as setup. It waits for the setup terminal to exit. A failed setup makes the run stuck, with the tail of the setup terminal in the comment (G3). The coordinator then checks the approval again. An approval that was taken back ends the run the way the runner refuses an unapproved issue, and the workspace is archived. Otherwise the session starts with the launch message and the label moves to `building`.
4. **Build.** The run follows the session's synced state. When a turn ends, the coordinator reads that turn's final message (G1) once per turn number.
   - `FACTORY-DONE: <PR URL>`, with a pull request on this repo, moves the run to the gate.
   - `FACTORY-STOP: needs-info` posts the questions and moves the label to `needs-info`. The run ends: its session is ended, its workspace archived with its branch kept, its slot freed. When an instructor answers and sets `factory:ready`, the next attempt is a new run, as with the runner.
   - Any other stop, a malformed result line, or `limits.minutes` running out makes the run stuck, with the reason posted.
   - A turn that ends with no result line gets the runner's reminder once (G2). A second one makes the run stuck.
   - A session that waits for a person makes the run stuck at once, with its question in the comment. Permissions are skipped, so nothing should ask.
   - P1 marks a session stuck after 10 minutes of silence. The factory does nothing on that; a long build is silent for longer. `limits.minutes` ends such a run.
5. **Gate.** Placement puts a gate workspace on a machine under its limits that holds the project's login, while a gate slot is free. The worktree is the pull request's head commit with no branch and no setup of its own. One shell terminal runs `provision` and then `gate`, both taken from the default branch's `.dw/config.yml` and given by the coordinator, so a pull request cannot change what gates it. The holder keeps the exit code. The run records the commit it gated.
   - On a pass, the run waits for `required_checks` on that same commit, up to `checks_minutes`. A check that fails is sent back to the session at once, as the runner does. When all pass, the pull request is marked ready for review and the label moves to `pr-open`.
   - A head that moved since the gate is gated again before anything is marked ready.
   - On a fail, the tail of the output (G3) goes back to the same session as the runner sends it (G2), up to `gate_retries` times. Then the run is stuck.
   - The gate workspace is archived by force when the gate ends. Its output tail is kept in the run's events.
6. **Feedback.** In `pr-open`, every `feedback_seconds`, the factory reads new instructor comments on the pull request and the issue. As the runner does, it moves the label back to `building` and starts a new session in a new workspace on the pull request's branch (G4), with the runner's resumed message and the comments. That session takes a run slot and waits for placement like a claim, and its limits start fresh. Its new head is gated again.
7. **End.** A merged pull request or a closed issue ends the run and archives its workspaces by force.

A factory session's "stuck", "failed" and "waiting for you" raise no P1 attention item of their own. The run's item covers them.

### When things go wrong

- A session that fails, or whose machine stays unreachable for 15 minutes, is an interruption. The run resumes on any machine that qualifies, from the pull request's branch when it was pushed and from the base when not, up to `interruption_resumes` times. Then it is stuck.
- The machine that was cut off must not push after the run has moved on. A machine that has reached no peer for 10 minutes on its own clock ends its factory sessions (G10), five minutes before any other machine may resume them. When it is heard from again, the coordinator archives every workspace of a run that has since moved to another workspace.
- Every step records before it acts on what the runner records, so doing a step again carries on from where it stopped. A coordinator that takes over finds a run by its claim comment on GitHub and by the workspace records named `fac/<N>-<attempt>`, before any `run_event`. A claim with a workspace and no event is carried on, never launched again.
- A person who edits the title, removes the label or assigns someone stops the run at the next reconcile, as with the runner.
- Stop, in the Factory view, makes the run stuck on the next pass, as the runner's `runs stop` does.
- A stuck run's session is ended and its workspace kept, so it can be looked at. It is archived by force when the issue leaves `stuck`. Work a person left there uncommitted is lost then, and the Factory view says so beside the run.
- Paused claims stop new claims for a project or the fleet. Runs already going carry on.
- A gate that cannot run at all (fetch, provision of the gate checkout, disk) is not a gate failure. It is tried again after 1 pass, then 2, 4 and 8. The fifth failure makes the run stuck.

## The lease

- Each project's repo has one ref, `refs/factory/lease`. It points at a commit with no parent, whose message holds the holder's machine id, a generation number and a renewal count. The ref never grows a history.
- The holder renews it every 60 seconds, from a task of its own beside the pass, with a compare-and-swap push: `git push --force-with-lease=refs/factory/lease:<sha it read>`. A renewal keeps the generation and adds one to the count, which makes a new commit. A takeover adds one to the generation.
- A machine acts on a project only while its last successful push is less than 120 seconds old. It checks that before every change it makes on GitHub or asks of a worker, not once per pass. A create or a `gh` call can outlast a pass.
- Another machine takes over only after it has seen the same lease commit unchanged for 180 seconds.
- Both ages are measured on the machine's own clock that keeps counting while the machine sleeps (`CLOCK_MONOTONIC` on macOS, `CLOCK_BOOTTIME` on Linux), never Rust's `Instant`, which stops during sleep on macOS. A Mac that slept through its lease wakes with it expired. No two clocks are compared, so clock skew cannot make two holders.
- Only the acting coordinator takes or renews leases. A machine that stops acting stops renewing, and the next coordinator takes over after the wait. A move of the role therefore pauses the factory for about three minutes and stops no run.
- A machine that loses a push race stops acting on that project at once. That is not an error.
- In a split, only the side whose push lands can act. The other side's Factory view says who holds the lease and at which generation.

## Run events

- Each step of a run is a `run_event` record: run id, event name, data, and the lease generation it was written under. The event names and their data are the runner's ledger events, unchanged.
- A record is owned by the machine that wrote it. P1's rule holds: no two machines write the same record.
- A run's events are ordered by lease generation, then by the writer's record version. Within one generation only one machine writes, and generations only rise.
- Output kept in an event (a gate or setup tail) is at most 64 KB.
- Each machine keeps the events of live runs and of the newest 200 finished runs per project. Older finished runs are removed, oldest first by lease generation and version. `foundryd factory export` before the exit week's comparison keeps what `retro` needs.
- `foundryd factory export <dir>` writes every run as `runs/<owner>__<repo>__<N>__<attempt>.jsonl` in the runner's layout, so `factory-runner retro --data-dir <dir>` reads it.

## Two factories at once

During the cutover the runner and Foundry never watch the same repo: a repo is taken out of the runner's `repos:` before its switch goes on in Foundry. Foundry also checks. An issue in a watched project carrying another factory's claim marker raises an attention item, and Foundry leaves that issue alone.

## UI

Runs are already workspaces on the board, so the UI adds little.

- The Projects dialog gains the factory switch, the GitHub login and pause for each project.
- A Factory view lists each project with:
  - its lease holder and generation, or why its factory is idle
  - its ready queue in claim order, each issue with why it waits: blocked, not approved, no room, paused
  - its live runs: issue, step, machine, attempt, gate retries used and minutes used, each linked to its workspace and with Stop
  - its last 20 finished runs and how each ended
- A factory run's workspace card says it is a factory run and links to its issue.

## Attention items in P2

| Item | Raised when | Clears when |
|---|---|---|
| Run stuck | A run goes stuck | The issue leaves `stuck` |
| Run needs info | A run posts questions | The issue leaves `needs-info` |
| Factory idle | Raised by the acting coordinator only: it holds no token for the project's login, or it has failed to take the lease for over 10 minutes, or `.dw/config.yml` does not load, or `gh` failed three passes running | The cause clears |
| Foreign claim | Another factory's claim marker is on an issue of a watched project | Tom dismisses it (G6), or the marker's issue is closed |
| Gate infra | A gate could not run at all | A gate of that run starts, or the run ends |

## Testing

- The runner's tests are the spec. For each ported module, its tests become Rust tests with the same cases and names, written first and failing before the code lands.
- `factory::github` sits behind a trait. Its fake answers from the runner's `tracker/fixtures/`. No test calls GitHub.
- Lease tests push to a local bare repo with a clock the test moves: two machines racing, a takeover after the wait, a holder that lost the lease and stops, a holder whose clock jumps past the limit (a sleep) and does not act, a split.
- Fleet tests use P1's three daemons and the fake agent, which ends a turn with a result line in its Stop hook. They cover: an issue reaching `pr-open`; a failed gate sent back and passed on the retry; needs-info; the coordinator killed during a build and the next one carrying the run on with its events in order; a coordinator killed between the claim and its first event; a worker going offline, ending its own session, and the run resuming on another machine; a split where only one side acts; a peer on the old protocol that receives no `run_event`.
- A gate test opens a pull request whose branch changes `provision` and `gate`, and checks the default branch's commands ran.
- A redaction test puts a token in gate output and checks it reaches no comment, message or event.
- The export test compares `foundryd factory export` of a fixture run with a golden file copied from the runner's own ledger output, in this repo, with the runner commit it came from named beside it. CI needs no Node and no second checkout.
- No test spends model calls. One `#[ignore]` contract test, run with `FOUNDRY_CONTRACT=1`, starts a real factory session with skipped permissions and a first prompt, and checks the whole result line arrives.

## Cutover

1. P1's acceptance run passes.
2. P2 ships in a release.
3. foundry leaves the runner's `repos:`, and its factory switch goes on in Foundry.
4. Foundry builds foundry until 10 runs have reached `pr-open` or `stuck` and no factory bug is open.
5. The runner's other repos move one at a time, each after the last moved repo has 5 clean runs: factory-paradev-spike, spec-reader, clockwork, factory-on-paradev, people-hub.
6. The runner stays stopped, still installed, for a week.
7. `retro` compares the two. Then the runner is uninstalled.

The way back, per repo: turn its switch off in Foundry and put it back in the runner's `repos:`. While Foundry's factory cannot build foundry, the runner builds the fix.

## To check before the plan is written

| # | Assumption | Why it matters |
|---|---|---|
| 1 | Interactive Claude Code started with `--dangerously-skip-permissions` in a P1 terminal does not stop at a one-time confirmation of the bypass mode | A factory session cannot wait for a person |
| 2 | The Stop hook's `last_assistant_message` holds the whole final message | The result line and the pull request URL come from it |
| 3 | With `GIT_ASKPASS` giving the token and `credential.helper` emptied for the call, `git push` over HTTPS acts as the project's login on macOS and Linux, on a Mac whose keychain holds another GitHub account. `GH_TOKEN` alone covers `gh` but not git | Otherwise the keychain's account pushes |
| 4 | GitHub accepts force-with-lease pushes to `refs/factory/lease` on these repos, no ruleset blocks them, and they start no workflow | The lease rests on it |
| 5 | A push a minute per repo, 1,440 a day each across six repos, trips no GitHub rate limit, and parentless commits that the ref has left do not pile up in the repo | The lease's cadence rests on it. If it fails, renew less often and wait longer before a takeover |
| 6 | `/factory-build` is installed on every machine that can host a run | Today it is in `~/.claude/skills/factory-build` on this Mac only |
| 7 | The runner's claim and status comments can be parsed both ways | The cutover and the foreign claim item rest on it |
| 8 | Interactive Claude Code takes the effort setting by a flag or variable, and the gateway passes it through | The factory setting names an effort |
| 9 | `CLOCK_MONOTONIC` on macOS and `CLOCK_BOOTTIME` on Linux count through sleep on the fleet's machines | The lease's ages rest on it |
