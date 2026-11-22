# P2: Factory

Product: Foundry, a dark software factory that also runs manual sessions.

Status: design, written 2026-11-22. Nothing here is built. The plan comes after the checks at the end of this spec and after P1's open fleet and session fixes land.

## Why

The Paradev runner (`original-solutions/factory-on-paradev`) builds every factory issue today, Foundry's own included. It depends on Paradev's CLI and app state, which is the dependency Foundry exists to remove. P1 gave Foundry its own machines, workspaces, sessions, placement and sync. P2 puts the factory on top of them, so the runner can be turned off.

## Goal

`foundryd` claims `factory:ready` issues, builds each one in a Foundry workspace on whichever machine placement picks, gates the draft pull request on a fresh checkout and carries it to `factory:pr-open`, the way the runner does now. Every run is a workspace with terminals, so it shows on the board, the timeline and the attention list while it runs.

Exit criterion: every repo the runner watches has moved to Foundry, foundry first, and the runner has then been stopped for one week. Over that week, the runner's `retro` over Foundry's exported runs shows a gate pass rate and a stuck rate no worse than over the runner's last 30 runs.

## Scope

In:

- The runner's behaviour, ported to a `factory` module of `foundryd`: the label lifecycle, instructor approval, dependency links and `## Base`, claims and claim resumes, the launch message and `/factory-build`, the result line, the gate with retries sent back to the same session, required checks, feedback from PR and issue comments, needs-info, stuck, interruption resumes, the limits in `.dw/config.yml`, pausing claims and stopping a run.
- Run events in the runner's ledger format, and an export the runner's `retro` reads.
- A factory switch per project, with a GitHub login that overrides the coordinator's default.
- GitHub tokens per machine, in each machine's own secret store.
- A lease per project, a ref in the project's repo.
- Factory attention items and a Factory view in the UI.

Out, and where it goes:

- The review loop to 5 out of 5, the merge setting and the merge queue: P2b, with its own spec. P1's "Decisions already made for later pieces" keeps what the brainstorm settled for them.
- A native `retro`: later. The exit week uses the runner's.
- Linear: P4.
- Cloud targets, `default_project` inference, model fallbacks and the belt page: dropped. The Foundry UI replaces the belt.

## Decisions

| # | Question | Decision |
|---|---|---|
| F1 | Port or adapter | Port the runner's core to Rust inside `foundryd`. The runner's tests are the behaviour spec. Driving `foundryd` from the TypeScript runner through a new adapter was rejected: it keeps a second process, store and updater, and pins the factory to one machine while the coordinator moves |
| F2 | Who runs the factory | The acting coordinator, for each project whose lease it holds. Builds and gates run on any machine placement picks |
| F3 | How a run is hosted | A P1 workspace with a P1 session. A gate is a P1 workspace with a shell terminal. There is no second way of starting work |
| F4 | GitHub logins | Each machine holds tokens for the logins it can act as, and names one as its default. A project uses the coordinator's default login unless it names another. Tokens never leave the machine that holds them |
| F5 | Where an issue's state lives | GitHub: labels, the claim comment, the status comment, the pull request. Run steps are synced `run_event` records. A new coordinator rebuilds its view from both and needs nothing handed over |
| F6 | Order of run events | By lease generation, then by the writing machine's record version. No clock is compared |
| F7 | When to claim | Only when placement can place the run now. The runner claims first and waits for a slot; #96 sat in `triage` for 72 minutes on 2026-11-21 |
| F8 | Cutover order | foundry first, then the runner's other repos one at a time |

## Architecture

```
acting coordinator                        any machine placement picks
┌─────────────────────────────┐           ┌───────────────────────────────┐
│ factory::pass, every poll   │──create──►│ workspace fac/<N>-<attempt>   │
│  ├ lease (refs/factory/)    │  (peer    │  └ session: claude with the   │
│  ├ github (gh, token)       │   link)   │    launch message, GH_TOKEN,  │
│  ├ lifecycle (pure)         │◄─records──│    skipped permissions; the   │
│  ├ runs                     │  (sync)   │    Stop hook carries the      │
│  └ ledger (run events)      │           │    result line                │
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

A factory session is a P1 session with four additions:

- Claude Code starts with `--dangerously-skip-permissions`, and with the model and effort from the factory settings.
- `GH_TOKEN` is set from the worker's own secret store for the project's login.
- The launch message is its first prompt, typed as P1 types any first prompt. It carries the runner's text: `/factory-build <owner>/<repo>#<N>`, the rules for the branch and the push, and the `gh api user --jq .login` check. Its sentences about Paradev's `gh` login are rewritten to say Foundry set `GH_TOKEN` for the project's login.
- The factory reads the Stop hook's `last_assistant_message` whole. P1's brief cuts it to 600 characters, which can cut the result line.

## Config

### Repo config

Read from `.dw/config.yml` on the project's default branch, with the runner's keys and rules: `factory.instructors`, `base`, `provision`, `gate`, `required_checks`, `autonomy` (only `pr`), `owner`, and `limits` with `minutes`, `gate_retries`, `checks_minutes` and `interruption_resumes`. `owner` names the only machine that may hold the project's lease. A file that does not load leaves the project idle with an attention item.

### Shared settings

Changed through the acting coordinator, like every P1 shared setting.

| Setting | Scope | Default |
|---|---|---|
| factory on | project | off |
| GitHub login | project | the coordinator's default login |
| claims paused | project, and fleet | not paused |
| `poll_seconds` | fleet | 30, at least 10 |
| `feedback_seconds` | fleet | 600 |
| `max_runs` | fleet | 3 |
| `max_gates` | fleet | 2 |
| model, effort | fleet | `opus`, `high` |

A run holds one of `max_runs` from its claim until it reaches `pr-open` or ends. A gate holds one of `max_gates` while it runs. Both sit on top of each machine's P1 limits, which placement already applies.

### GitHub tokens

- `foundryd secret set github:<login>` stores a token in the machine's secret store. `foundryd github default <login>` names the machine's default.
- The machine's record lists the logins it holds, never a token.
- A token reaches a child process only through its environment: never an argument, a log line, an error, a record or the UI.
- Commits keep the worktree's git identity, as the runner's do.

## A run

1. **Pass.** The coordinator holds the project's lease and reads its `factory:ready` issues. As the runner does, it skips an issue that no instructor approved, that has an assignee, or that has an open blocker.
2. **Claim.** Only when placement has a machine under its limits that holds the project's login, and a run slot is free. The claim swaps `ready` for `triage`, assigns the login, and posts the claim comment and status comment in the runner's format, naming the machine. The runner and Foundry can each read the other's claims.
3. **Launch.** The coordinator asks that machine, over the peer link, for workspace `fac/<N>-<attempt>` from `## Base`, or from `factory.base` when the issue has none. Setup runs `provision`. A failed setup makes the run stuck, with the setup terminal's output in the comment. The session then starts with the launch message. The coordinator checks the approval again, then moves the label to `building`.
4. **Build.** The run follows the session's synced state.
   - `FACTORY-DONE: <PR URL>`, with a pull request on this repo, moves the run to the gate.
   - `FACTORY-STOP: needs-info` posts the questions and moves the label to `needs-info`.
   - Any other stop, a malformed result line, or `limits.minutes` running out makes the run stuck, with the reason posted.
   - A session that goes idle with no result line gets the runner's nudge once. A second idle with no result line makes the run stuck.
5. **Gate.** Placement puts a gate workspace on any machine under its limits while a gate slot is free, with a fresh worktree of the pull request's head. One shell terminal runs `provision` and then `gate`, and the holder keeps its exit code.
   - On a pass, the run waits for `required_checks` up to `checks_minutes`, then marks the pull request ready for review and moves the label to `pr-open`.
   - On a fail, the tail of the output goes back to the same session as the runner sends it, up to `gate_retries` times. Then the run is stuck.
   - The gate workspace is archived when the gate ends. Its output is kept in the run's events.
6. **Feedback.** In `pr-open`, every `feedback_seconds`, the factory reads new instructor comments on the pull request and the issue. As the runner does, it starts a new session in a new workspace on the pull request's branch with the runner's resumed message and the comments. A new head is gated again.
7. **End.** A merged pull request or a closed issue ends the run and archives its workspaces.

### When things go wrong

- A session that fails, or whose machine stays unreachable for 15 minutes, is an interruption. The run resumes on any machine that qualifies, from the pull request's branch when it was pushed and from the base when not, up to `interruption_resumes` times. Then it is stuck.
- A person who edits the title, removes the label or assigns someone stops the run at the next reconcile, as with the runner.
- Stop, in the Factory view, makes the run stuck on the next pass, as the runner's `runs stop` does.
- A stuck run's session is ended and its workspace kept, so it can be looked at. It is archived when the issue leaves `stuck`.
- Paused claims stop new claims for a project or the fleet. Runs already going carry on.
- A gate that cannot run at all (clone, provision of the gate checkout, disk) is not a gate failure. It is retried on the next pass and raises the gate infra item.

## The lease

- Each project's repo has one ref, `refs/factory/lease`. It points at a commit whose message holds the holder's machine id and a generation number.
- The holder renews it every pass with a compare-and-swap push: `git push --force-with-lease=refs/factory/lease:<sha it read>`. A renewal keeps the generation. A takeover adds one.
- A machine acts on a project (claims, labels, comments, launches, gates) only while its last successful push is less than 90 seconds old on its own clock.
- Another machine takes over only after it has seen the same lease commit unchanged for 120 seconds on its own clock. No two clocks are compared, so clock skew cannot make two holders.
- Only the acting coordinator takes or renews leases. A machine that stops acting stops renewing, and the next coordinator takes over after the wait. A move of the role therefore pauses the factory for about two minutes and stops no run.
- A machine that loses a push race stops acting on that project at once. That is not an error.
- In a split, only the side whose push lands can act. The other side's Factory view says who holds the lease and at which generation.

## Run events

- Each step of a run is a `run_event` record: run id, event name, data, and the lease generation it was written under. The event names and their data are the runner's ledger events, unchanged.
- A record is owned by the machine that wrote it. P1's rule holds: no two machines write the same record.
- A run's events are ordered by lease generation, then by the writer's record version. Within one generation only one machine writes, and generations only rise.
- A new lease holder carries a live run on from its last event with the runner's claim resume.
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
| Factory idle | The coordinator holds no token for the project's login, or another machine has held the lease for over 10 minutes, or `.dw/config.yml` does not load, or `gh` failed three passes running | The cause clears |
| Foreign claim | Another factory's claim marker is on an issue of a watched project | Tom dismisses it |
| Gate infra | A gate could not run at all | The next gate runs |

## Testing

- The runner's tests are the spec. For each ported module, its tests become Rust tests with the same cases and names, written first and failing before the code lands.
- `factory::github` sits behind a trait. Its fake answers from the runner's `tracker/fixtures/`. No test calls GitHub.
- Lease tests push to a local bare repo: two machines racing, a takeover after the wait, a holder that lost the lease and stops, a split.
- Fleet tests use P1's three daemons and the fake agent, which ends a turn with a result line in its Stop hook. They cover: an issue reaching `pr-open`; a failed gate sent back and passed on the retry; needs-info; the coordinator killed during a build and the next one carrying the run on with its events in order; a worker going offline and the run resuming on another machine; a split where only one side acts.
- An export test feeds a fixture run through `foundryd factory export` and parses the result with the runner's ledger reader, in CI.
- No test spends model calls. One `#[ignore]` contract test, run with `FOUNDRY_CONTRACT=1`, starts a real factory session with skipped permissions and a first prompt, and checks the whole result line arrives in the Stop hook.

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
| 3 | `GH_TOKEN` in the environment is enough for `gh` and for `git push` over HTTPS on every machine | Otherwise a machine's own credential helper acts as another login |
| 4 | GitHub accepts force-with-lease pushes to `refs/factory/lease` on these repos, no ruleset blocks them, and they start no workflow | The lease rests on it |
| 5 | `/factory-build` is installed on every machine that can host a run | Today it is in `~/.claude/skills/factory-build` on this Mac only |
| 6 | The runner's claim and status comments can be parsed both ways | The cutover and the foreign claim item rest on it |
