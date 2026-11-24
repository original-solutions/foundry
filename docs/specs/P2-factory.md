# P2: Factory

Product: Foundry, a dark software factory that also runs manual sessions.

Status: approved 2026-11-22, after six revisions the same day from reviews against P1's code on `main` (v0.2.0). Nothing here is built. The plan comes after the checks at the end of this spec and after the P1 issues named under "Before P2 starts" are closed.

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
- Factory attention items and a Factory view in the UI, the same on every machine.
- The additions to P1 that the factory needs, listed under "What P1 gains".

Out, and where it goes:

- The review loop to 5 out of 5, the merge setting and the merge queue: P2b, with its own spec. P1's "Decisions already made for later pieces" keeps what the brainstorm settled for them.
- A native `retro`: later. The exit week uses the runner's.
- Linear: P4.
- Cloud targets, `default_project` inference, model fallbacks, `factory.owner` and the belt page: dropped. The Foundry UI replaces the belt.
- Repos reached over SSH. A project's factory can be turned on only when its repo URL is `https://github.com/...`.

This spec names a state without its prefix. On GitHub each is the label `factory:<state>`, as `factory:pr-open`.

Where this spec says "as the runner does" or is silent on a rule of the lifecycle, the runner's code and tests at commit `8ed9bef` decide.

## Before P2 starts

- #96 is closed. A session must keep taking its hooks after its daemon restarts. Today a restart, which every update causes, leaves the Stop hook refused, and a factory run would hang until `limits.minutes`.
- #85 and #87 are closed, so a fleet's machines all link and a removed machine's key is revoked.
- P1's acceptance run has passed.
- The `factory-build` skill is changed for Foundry and installed on every machine that will host a run. Today it is on this Mac only, and its source moves to `~/EA/skills/factory-build` first. Three changes:
  - It reads its brief from the file named on its command line (F12), also when the file is outside the worktree.
  - Its "GitHub account" step, which probes `gh` logins, runs `gh auth token` and swaps git's credential helper, is skipped when the brief says Foundry set `GH_TOKEN`. That step runs before anything else today and would pick an account against the one Foundry chose. factory-on-paradev holds a copy of the skill and a check (`plugins/factory/skills/factory/scripts/check.sh`) that fails when the "GitHub account" text is missing, so that check changes with it.
  - Its gate-failure paragraph says the command and its output are in the file named on the line, where today it says they are in the message.

## Decisions

| # | Question | Decision |
|---|---|---|
| F1 | Port or adapter | Port the runner's core to Rust inside `foundryd`. The runner's tests are the behaviour spec. Driving `foundryd` from the TypeScript runner through a new adapter was rejected: it keeps a second process, store and updater, and pins the factory to one machine while the coordinator moves |
| F2 | Who runs the factory | The acting coordinator, for each project whose lease it holds. Builds and gates run on any machine placement picks |
| F3 | How a run is hosted | A P1 workspace with a P1 session. A gate is a P1 workspace whose setup terminal runs the gate. There is no second way of starting work |
| F4 | GitHub logins | Each machine holds tokens for the logins it can act as, and names one as its default. Turning a project's factory on writes the login it acts as into the project's factory setting, taken from the coordinator's default at that moment and editable. Tokens never leave the machine that holds them |
| F5 | Where an issue's state lives | GitHub: labels, the claim comment, the status comment, the pull request. Run steps are synced `run_event` records. A new coordinator rebuilds its view from both and needs nothing handed over |
| F6 | Order of run events | By lease generation, then by the writing machine's factory version. No clock is compared |
| F7 | When to claim | Only when placement can place the run now. The runner claims first and waits for a slot; #96 sat in `triage` for 72 minutes on 2026-11-21 |
| F8 | Cutover order | foundry first, then the runner's other repos one at a time |
| F9 | `factory.owner` | Ignored, with a line in the Factory view when a repo sets it. The lease decides which machine acts. An owner that is not the acting coordinator would leave the project idle for good |
| F10 | What every machine knows of the factory | The acting coordinator writes one `factory_status` record per project each pass, marked as holding the lease or not. Every machine draws the Factory view and the factory's attention items from it, and a worker uses it to know the holder is still there |
| F11 | Where factory records are kept | In a table of their own with their own version per owner and their own sync cursor, apart from P1's records. Existing P1 records and settings values gain no field |
| F12 | How a message reaches a session | As one short line typed into the terminal, naming a file that holds the rest. Claude Code wraps a paste over three lines or about 800 characters in `<pasted_content>` tags (docs/claude-code-contract.md), so a long launch message would not begin with `/factory-build` and the skill would not load |
| F13 | Whose claim is whose | Foundry's claim comment carries a marker of its own, `<!-- foundry:claim <run id> <machine> -->`. A claim without it belongs to another factory |

## Architecture

```
acting coordinator                        any machine placement picks
┌─────────────────────────────┐           ┌───────────────────────────────┐
│ factory::pass, every poll   │──create──►│ build workspace               │
│  ├ lease (refs/factory/)    │  (peer    │  └ session: claude with the   │
│  ├ github (gh, token)       │   link)   │    launch message, GH_TOKEN,  │
│  ├ lifecycle (pure)         │◄─records──│    skipped permissions        │
│  ├ runs                     │  (sync)   │                               │
│  └ ledger (run events)      │◄─calls────│ each turn's final message,    │
│                             │  (peer    │ kept on disk by its machine   │
│ writes factory_status ──────┼──sync────►│                               │
│ to every machine            │──create──►│ gate workspace                │
└─────────────────────────────┘           │  └ setup terminal: provision  │
                                          │    and gate; exit code from   │
                                          │    the holder                 │
                                          └───────────────────────────────┘
```

| Unit | Job | Ported from (`factory-on-paradev/src/`) |
|---|---|---|
| `factory::lifecycle` | Pure functions: the state machine, approval, `## Depends on`, `## Base`, the result line | `domain/` |
| `factory::github` | Issues, labels, comments, pull requests, checks and dependency links through `gh`, behind a trait. Every call ends within 30 seconds | `tracker/` |
| `factory::lease` | Takes, renews and drops a project's lease | New |
| `factory::runs` | One run's steps: claim, place, launch, watch, gate, feedback, stop, resume | `runner/`, `claims.ts` |
| `factory::gate` | The gate in a fresh worktree of the pull request's head, with the runner's git hardening | `gate.ts`, `gitHardening.ts` |
| `factory::ledger` | `run_event` and `factory_status` records, and the export | `ledger/` |
| `factory::pass` | Each pass: reconcile with GitHub, advance the runs, then claim if placement has room. No step of a pass waits on a worker's setup or build; it looks again next pass | `pass.ts` |

`factory` depends on P1's `placement`, `workspaces`, `sessions`, `sync`, `coordinator`, `store` and `attention`. P1's `attention` and `web` read `factory_status`. Nothing else in P1 depends on `factory`.

A factory session is a P1 session started with factory options (G12):

- Claude Code starts with `--dangerously-skip-permissions`, and with the model and effort from the factory settings. The effort goes in as `--effort`. The settings file `sessions` writes for it holds `skipDangerousModePermissionPrompt: true`, because a machine that never accepted the mode stops at a confirmation without it (check 1).
- `GH_TOKEN` is set from the worker's own secret store for the project's login.
- Its environment also carries a `GIT_ASKPASS` that answers with that token, so its `git push` acts as the same login.
- Its first prompt is the line `/factory-build <owner>/<repo>#<N> <path>` (F12). The file at the path holds the runner's launch message: the rules for the branch and the push, and the `gh api user --jq .login` check. Its sentences about Paradev's `gh` login are rewritten to say Foundry set `GH_TOKEN` for the project's login.

Every message the factory sends is a line and a body. The worker writes the body to a file in the session's own folder, outside the worktree and readable by the service user only, and types the line with the file's path at its end. A launch, a resume and a feedback round begin with `/factory-build`, which loads the skill. A gate or check failure begins with `The factory gate failed`. That line loads nothing: the session already has the skill, which tells it to open the file the line names. A body is at most 64 KB. The reminder for a missing result line is short and has no body.

## What P1 gains

P1 as built on `main` cannot carry a run that lives on another machine than the factory. Each item below is a change to a P1 unit, built before the factory unit that uses it.

| # | Unit | Today | Gains |
|---|---|---|---|
| G1 | `sessions` | The Stop hook's last message and a notification's question are kept in memory only, cut short, never leave their machine, and have every word of 24 or more mixed letters and digits hidden, which can break a URL | For a factory session, each turn's whole final message and the newest question are written to disk by the owning machine. Turns are numbered, and the number only rises. The number is published in a factory record the machine owns (`session_turn`), never in P1's session record, so F11 holds. The file is on disk before that record carries the new number, and every Stop writes it, also one that changes no state. The files go when the workspace is archived. Any machine reads a turn's message or the question through a carried call (G2). Known secrets are hidden (the gateway keys, session tokens, every `github:*` token the machine holds) and URL passwords, as P1 hides them. Nothing else is hidden in what is kept on disk, so the result line and its URL stay whole. What the factory posts to GitHub is hidden further (see "GitHub tokens") |
| G2 | `api` (carried routes) | Carries workspace create, sessions, shells, archive, and terminal close, all as `POST` | Also carries: start a factory session (G12), send a factory message to a session (a line and a body, F12; refused unless the session is idle; it answers with the session's count of submitted prompts, and a send that names a count is refused when the count has risen), read a turn's message or question (G1), read a terminal's tail (G3), end a workspace's factory sessions (each terminal is closed as P1 closes one, which ends Claude Code and drops the terminal's scrollback; the holder's protocol does not change), and stop a run (carried to the lease holder). All are `POST`, since a carried call is one |
| G3 | `terminals` | A terminal of another machine can only be read as a raw byte stream | A read of the last lines of a terminal as plain text, escapes removed, at most 64 KB |
| G4 | `workspaces` | `create` always makes a new branch from `origin/<base>`, refuses a branch that exists, runs `factory.provision` from the worktree's own `.dw/config.yml`, turns `/` in a branch into `-`, and keeps each worktree in a folder named after its branch | Three ways to make a worktree: a new branch from a base (as now), an existing branch of `origin` (a local branch of that name is moved to it), and one commit with no branch. The caller may give the branch exactly, `/` kept. The worktree's folder is named by the workspace's id, so it no longer depends on the branch; folders made before this stay where they are. Setup is one of: the worktree's own provision (as now), a command list the caller gives, with a time limit for each command, or none. A create that names a workspace a factory create already made on that machine answers with that workspace and makes nothing |
| G5 | `workspaces` | git runs with whatever credentials the machine has | For a project whose factory is on, the clone and fetch `foundryd` itself runs get the login's token through `GIT_ASKPASS`, with other credential helpers, hooks and the repo's own git config turned off for the call, as the runner's git hardening does. No terminal gets the token this way: a setup or gate terminal of a factory workspace has neither it nor `GH_TOKEN`, and runs with git's credential helpers turned off in its environment, so the machine's keychain answers for nothing either |
| G6 | `foundry-proto`, `sync`, `store` | One table and one version per owner for every record. A record of an unknown kind is an error, in the store and on the link | `PROTOCOL` goes from 4 to 5. Factory records (`run_event`, `factory_status`, logins held, `session_turn`) get a table, a version per owner and a sync cursor of their own, and are sent only on a link whose two ends speak 5. A machine that updates later asks for them from zero and misses none. P1's record table, its catch-up, `GET /api/state`, `GET /api/events` and the attention list's record cache never hold a `run_event` |
| G7 | `coordinator`, settings | A settings key of an unknown kind does not parse | Factory settings are keys of their own (`factory` for the fleet, `factory:<project>` for a project), so no existing value gains a field. They are never sent to a peer on protocol 4, and a settings copy merged from such a peer leaves them as they are. They cannot be edited while the acting coordinator is on protocol 4. Known limit while a fleet has machines on both protocols: a protocol 4 machine still passes on the `seen` of each copy it is sent, so two protocol 5 machines split from each other that both reach it and both edit the same factory setting can lose one of the two edits with no "setting overwritten" item; it ends when every machine speaks protocol 5. The protocol a factory edit is checked against is that of the acting coordinator's last settings copy, so in the moment after it links again on another protocol a factory edit can be refused on a protocol 5 link, or sent on a protocol 4 link, which then takes nothing more from the sender for a minute |
| G8 | `api`, `web` | | `GET /api/factory` returns each project's `factory_status`. `GET /api/factory/runs/{id}` returns a run's events. The events socket sends `factory_status` changes, never run events |
| G9 | `machine` | The machine's record has load, memory, disk and sessions | The logins a machine holds, and whether it has the `factory-build` skill, are written in a factory record it owns, so `foundryd secret set github:<login>` shows on every machine at once and P1's machine record does not change |
| G10 | `sessions` | A session cut off from the fleet runs on | A machine ends its factory sessions of a project when it has neither held that project's lease nor received a fresh `factory_status` marked as holding it for 10 minutes (see "When things go wrong") |
| G11 | `secrets` | `secret set` takes only `gateway` and `gateway-management`. The store cannot list its names. `sessions` reads no secret after start | `secret set` takes `github:<login>`. The store lists its names. `sessions` reads the store at each session start and each time it hides secrets, so a new token needs no restart |
| G12 | `sessions`, `api` | A session start takes a model and a prompt | A factory start, made only by the factory and never offered in the UI, also takes: skip permissions, effort, the login whose token becomes `GH_TOKEN`, and the project. The machine keeps "factory session of project P" with the session's files on disk |
| G13 | `attention` | A stuck, failed or waiting session and a failed setup each raise an item | They raise none for a session or workspace a `factory_status` names, or that its own machine knows as a factory session |
| G14 | `web` | The board puts a session P1 marks stuck under "needs you" | A factory session P1 marks stuck stays under "working", shown as quiet with how long. Only the run's own state moves its card to "needs you" |

A fleet in the middle of an update has machines on both protocols. The factory runs only when the acting coordinator speaks protocol 5, and it places runs and gates only on machines that do.

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

- A run holds one of `max_runs` from its claim until its session reports its pull request or the run ends, as the runner counts it. A gate failure sent back to such a session takes no slot, as with the runner. Several runs can be in that state at once, so more sessions than `max_runs` can be at work. Each is a session its machine already counts, so machine limits still hold.
- A gate holds one of `max_gates` while it runs. A gate has no session, so it does not count toward a machine's session limit. Its machine's load and disk limits apply.
- A pass that claims more than one issue counts each machine it picked as one session fuller for the rest of the pass, as P1's placement does for creates in flight.

### GitHub tokens

- `foundryd secret set github:<login>` stores a token in the machine's secret store (G11). `foundryd github default <login>` names the machine's default, which is only read when a project's factory is turned on while that machine is coordinator.
- A project's login is a shared setting, so it does not change when the coordinator role moves. A coordinator that holds no token for it leaves the project idle and says so.
- A token reaches a child process only through its environment: never an argument, a log line, an error, a record or the UI.
- Every `github:*` token and gateway key a machine holds is hidden from all text the factory records or posts: comments, gate and setup output, messages sent to a session and `run_event` data.
- A comment the factory posts on GitHub also has every word of 24 or more mixed letters and digits hidden, as P1 hides them, inside a URL as well as outside one, since a webhook path, a signed link or an `?access_token=` carries its secret in the URL. The only such word a comment keeps is a commit id or a comment, review or diff anchor in a `github.com` link, at the place GitHub's own links put one. So a pull request, commit or review link comes through whole unless its owner, repo, branch or file name holds such a word. A session can quote a secret its machine does not hold, such as a key it read from a `.env`, and a comment cannot be taken back. Commit ids and hashes in output posted to a comment are hidden with it. The run's events and the messages sent to a session keep them whole.
- Commits keep the worktree's git identity, as the runner's do.

## A run

A run's workspaces are named from the run, and the name alone says what each is for:

| Workspace | Name | Branch |
|---|---|---|
| Build | `fac/<N>-<attempt>` | new branch `fac/<N>-<attempt>` |
| Resume after an interruption, the k-th | `fac/<N>-<attempt>-r<k>` | the pull request's branch when pushed, else a new branch of this name from the base |
| Feedback round, the k-th | `fac/<N>-<attempt>-f<k>` | the pull request's branch |
| Gate, the k-th | `fac/<N>-<attempt>-g<k>` | none: the pull request's head commit |

The newest build, resume or feedback workspace is the run's active one. A run never has two workspaces on one branch on one machine. Before a resume or a feedback workspace is made, the earlier workspace is archived: one pass records the archive, and a later pass makes the new workspace once the earlier one shows as archived, or once its machine has been unreachable for 15 minutes. No pass waits on a worker.

1. **Pass.** The coordinator holds the project's lease and reads its `factory:ready` issues. As the runner does, it skips an issue that no instructor approved, that has an assignee, or that has an open blocker.
2. **Claim.** Only when placement has a machine under its limits that holds the project's login, and a run slot is free. The claim posts the claim comment with Foundry's marker (F13) first, then swaps `ready` for `triage`, assigns the login, and posts the status comment in the runner's format, naming the machine. So an issue Foundry moved to `triage` always has Foundry's comment as its newest claim comment. A claim cut short after the comment and before the label leaves the issue in `ready`, and a later pass claims it again; the newest claim comment is the one that counts. Placement takes only a machine that has the `factory-build` skill.
3. **Launch.** The coordinator asks that machine, over the peer link, for the build workspace from `## Base`, or from `factory.base` when the issue has none, with the worktree's own provision as setup. Later passes look at the setup terminal until it has exited. A failed setup makes the run stuck, with the tail of the setup terminal in the comment (G3). Output in any comment is cut to its last 20,000 characters, since GitHub takes at most 65,536 in one; the event keeps the full 64 KB. The coordinator then checks the approval again. An approval that was taken back ends the run the way the runner refuses an unapproved issue, and the workspace is archived by force. Otherwise the session starts with the launch message and the label moves to `building`. P1 holds a first prompt in memory until it is typed, so a daemon restart in that moment loses it. A message is sent again only when it never became a prompt. Every send answers with the session's count of submitted prompts at that moment, read on the session's own machine. When the session shows idle a full pass after the send, the coordinator asks for the message again and names that count. The worker types it only when the session is idle and its count has not risen since, and refuses it otherwise. The worker decides because the session's state and `session_turn` sync apart (G6), so the coordinator can see a turn's end before the count that turn raised. A session that is idle because its turn ended, as a build session is for the whole gate, has taken its message and gets nothing again.
4. **Build.** The run follows the session's synced state. When a turn ends, the coordinator reads that turn's final message (G1) and records the turn number, so no turn is acted on twice.
   - `FACTORY-DONE: <PR URL>`, with a pull request on this repo, moves the run to the gate.
   - `FACTORY-STOP: needs-info` posts the questions and moves the label to `needs-info`. The run ends: its session is ended, its workspace archived by force with its branch kept, its slot freed. When an instructor answers and sets `factory:ready`, the next attempt is a new run, as with the runner.
   - Any other stop, a malformed result line, or `limits.minutes` running out makes the run stuck, with the reason posted.
   - A turn that ends with no result line gets the runner's reminder once (G2). A second one makes the run stuck.
   - A session that waits for a person makes the run stuck at once, with its question (G1) in the comment. Permissions are skipped, so nothing should ask.
   - A session whose process ends, with any exit code, before a result line was read is an interruption.
   - P1 marks a session stuck after 10 minutes of silence. The factory does nothing on that; a long build is silent for longer. `limits.minutes` ends such a run.
5. **Gate.** Placement puts a gate workspace on a machine under its load and disk limits that holds the project's login, while a gate slot is free. The worktree is the pull request's head commit with no branch. Its setup (G4) is the command list the coordinator gives: `provision` and then `gate`, both from the default branch's `.dw/config.yml`, so a pull request cannot change what gates it. The setup terminal's exit code is the gate's result. Each `provision` and `gate` command has the runner's limit of 30 minutes. The worker kills one that runs longer, and the gate has failed. The run records the commit it gated. The gate runs the pull request's code, so its terminal gets no token (G5): a gate command that calls `gh` or fetches a private dependency from GitHub fails.
   - On a pass, the run waits for `required_checks` on that same commit, up to `checks_minutes`. A check that fails is sent back to the session at once, as the runner does. When all pass, the pull request is marked ready for review and the label moves to `pr-open`.
   - A head that moved since the gate is gated again before anything is marked ready.
   - `gate_retries` is one counter per round: every gate after the round's first spends one, whether the gate before it failed or the head moved. A failure sent back and fixed is one gate more, so it spends one, not two. With none left, a failed gate or a moved head makes the run stuck, so a pull request that keeps moving ends.
   - On a fail, the gate's output goes back to the same session as the runner sends it (F12).
   - The gate workspace is archived by force when the gate ends. Its output tail is kept in the run's events.
   - On reaching `pr-open`, the build session is ended and its workspace archived by force. Its branch is on `origin`. An idle session would count against its machine's session limit for as long as the pull request waits for review.
6. **Feedback.** In `pr-open`, every `feedback_seconds`, the factory reads new instructor comments on the pull request and the issue. A round starts only when a run slot is free and placement can place its session now, as a claim does (F7). Until then the issue stays in `pr-open`, the Factory view lists the round as waiting with the reason, and none of the wait counts toward `limits.minutes`. When the round starts, the factory moves the label back to `building` and starts a new session in a feedback workspace on the pull request's branch (G4), with the runner's resumed message and the comments, as the runner does. That session holds a run slot. `minutes`, `gate_retries` and `interruption_resumes` all start again for the round. Its new head is gated again.
7. **End.** A merged pull request or a closed issue ends the run and archives its workspaces by force.

`limits.minutes` counts the time a run spends in `triage` and `building`, less the time a gate runs and the time required checks are waited for. So a setup that hangs ends the run, and so does a session that hangs on a gate failure or a failed check sent back to it. The runner stops counting once a pull request is reported, which leaves that last case with no limit; this spec does not copy that. The time is taken from the timestamps the events carry, as in the runner's ledger. A machine with a wrong clock ends a run early or late. Timestamps never order events.

### When things go wrong

- A session that fails, or whose machine stays unreachable for 15 minutes on the coordinator's sleep-counting clock, is an interruption. The run resumes on any machine that qualifies, in a resume workspace, up to `interruption_resumes` times. Then it is stuck.
- A machine that was cut off must not push after the run has moved on. The rule (G10): a machine ends its factory sessions of a project when, for 10 minutes on its sleep-counting clock, it has neither held that project's lease nor received a fresh `factory_status` marked as holding it, at the highest lease generation it has seen. That is five minutes before any other machine may resume them.
  - A coordinator that hosts a run itself, alone in the fleet or not, holds the lease and ends nothing.
  - In a split of two against two, the side without the lease holder gets only its own coordinator's status, marked as not holding, and ends its sessions.
  - When the role moves, the new holder's status arrives within about three minutes, and nothing is ended.
  - A Mac that slept for 20 minutes wakes with the 10 minutes long past and ends its sessions at once. In the seconds before it does, a session that was mid-push can still push. The coordinator then finds a head it did not gate and gates it again, so nothing ungated is marked ready.
  - When a machine is heard from again, the coordinator has it archive, by force, every workspace of a run that has since moved to another workspace.
- Every step records before it acts on what the runner records, so doing a step again carries on from where it stopped. A coordinator that takes over finds a run by Foundry's claim comment on GitHub (F13), which names the run and its machine, and by the workspace records with the run's names, before any `run_event`. A claim with a build workspace and no event is carried on. A claim with no workspace yet is launched on the machine the comment names. If that machine is unreachable for 15 minutes, placement picks another, the status comment names it, and this spends no `interruption_resumes`, since nothing ran. A workspace the first machine did make is archived when it is heard from again. Otherwise, if the named machine made the workspace before the old coordinator died, the create answers with it (G4), also when its record arrives a pass later. Neither is a foreign claim.
- A call the coordinator sent to a worker can land after its lease has lapsed, since a create may take minutes. That is safe because every such call names a workspace by the run's names, and the next holder finds and carries on what it made.
- A person who edits the title, removes the label or assigns someone stops the run at the next reconcile, as with the runner.
- Stop, in the Factory view of any machine, is carried to the lease holder (G2) and makes the run stuck on its next pass, as the runner's `runs stop` does.
- A stuck run's session is ended and its workspace kept, so it can be looked at: the worktree, each turn's final message and the run's events stay. The terminal's scrollback does not. It is archived by force when the issue leaves `stuck`. Work a person left there uncommitted is lost then, and the Factory view says so beside the run.
- Paused claims stop new claims for a project or the fleet. Runs already going carry on.
- A gate whose machine is unreachable for 5 minutes on the coordinator's sleep-counting clock could not run: its slot is freed and the gate is tried on another machine. When the machine is heard from again, it archives the orphaned gate workspace by force.
- A gate that cannot run at all (fetch, disk, no machine holding the login, its machine gone) is not a gate failure. It is tried again after 1 pass, then 2, 4 and 8. The fifth failure makes the run stuck. A `provision` that fails in the gate checkout is a gate failure, as with the runner. The build ran its worktree's own `provision` and the gate runs the default branch's. The two differ when the issue's `## Base` holds another `.dw/config.yml` or the pull request changes it. A pull request that needs its own change to `provision` or `gate` cannot pass its gate until that change is on the default branch, and its session cannot put it there. So when the `.dw/config.yml` at the pull request's head differs from the default branch's, the failure sent back and the stuck comment say so.

## The lease

- Each project's repo has one ref, `refs/factory/lease`. It points at a commit with no parent, whose message holds the holder's machine id, a generation number and a renewal count. The ref never grows a history.
- The coordinator keeps a small bare git directory per project for the lease alone. It holds the empty tree and lease commits, fetches only that ref, and is no clone of the repo.
- The holder renews every 60 seconds, from a task of its own beside the pass, with a compare-and-swap push: `git push --force-with-lease=refs/factory/lease:<sha it read>`. A renewal keeps the generation and adds one to the count, which makes a new commit. A takeover adds one to the generation. A push ends within 30 seconds or counts as failed.
- A machine acts on a project only while less than 120 seconds have passed since it started its last successful push. The age is counted from before the push began, since GitHub may take the push long before the answer arrives. The machine checks the age before every change it makes on GitHub or asks of a worker, not once per pass.
- Another machine takes over only after it has seen the same lease commit unchanged for 180 seconds. With every `gh` call and push ending within 30 seconds, a change the old holder started in time has landed before a takeover can.
- A ref that does not exist is taken at once, with the push expecting no ref. Its generation is one more than the highest this machine has seen for the project, in the ref, in a `run_event` or in a `factory_status`, so a ref that was deleted does not start the count again. A project with none of these starts at 1.
- A coordinator that starts and finds its own machine id in the lease renews at once, so an update's restart does not stall the factory for three minutes. P1's daemon lock allows one daemon per machine, and the compare-and-swap decides any race with a machine that was about to take over.
- All ages are measured on the machine's own clock that keeps counting while the machine sleeps (`CLOCK_MONOTONIC` on macOS, `CLOCK_BOOTTIME` on Linux), never Rust's `Instant`, which stops during sleep on macOS. A Mac that slept through its lease wakes with it expired. No two clocks are compared, so clock skew cannot make two holders.
- Only the acting coordinator takes or renews leases. A machine that stops acting stops renewing, and the next coordinator takes over after the wait. A move of the role therefore pauses the factory for about three minutes and stops no run.
- A machine that loses a push race stops acting on that project at once. That is not an error.
- In a split, only the side whose push lands can act. The other side's Factory view says who holds the lease and at which generation.

## Factory records

Kept and synced apart from P1's records (G6). Each is owned by the machine that wrote it, so P1's rule holds: no two machines write the same record.

- **`run_event`.** One per step of a run: run id, event name, data, a timestamp and the lease generation it was written under. The event names and their data are the runner's ledger events, unchanged.
  - A run's events are ordered by lease generation, then by the writer's factory version. Within one generation only one machine writes, and generations only rise.
  - Output kept in an event (a gate or setup tail) is at most 64 KB.
  - No factory record is deleted in P2, so a machine that updates late receives every run and `foundryd factory export` is complete on any machine. Removing old runs waits until the ledger has a reader of its own.
- **`factory_status`.** One per project and writing machine, written by the acting coordinator each pass. It says whether the writer holds the lease. A writer that does not says why (no token for the project's login, or the holder and generation it sees in the ref) and nothing more. A writer that does gives: the generation, why the factory is idle if it is, the ready queue with each issue's reason for waiting, each feedback round that waits to start, each live run (issue, step, machine, attempt, retries and minutes used, its workspaces and sessions), whether the repo sets a `factory.owner` that is ignored (F9), the last 20 finished runs, and any foreign claims. A machine reads only the statuses of writers that are online by P1's 15-second rule. Among those it believes the one that holds the lease at the highest generation, and when none holds it, the acting coordinator's. So a holder that died does not hide the next coordinator's reason for being idle. Only a status that holds the lease counts for the cut-off rule (G10).
- **Logins held.** One per machine: the GitHub logins it holds a token for (G9).
- **`session_turn`.** One per factory session, written by its machine: the newest turn number, how many prompts the session has had submitted (from the `UserPromptSubmit` hook), and whether a question waits (G1).

`foundryd factory export <dir>` writes every run this machine holds as `runs/<owner>__<repo>__<N>__<attempt>.jsonl` in the runner's layout, so `factory-runner retro --data-dir <dir>` reads it.

## Two factories at once

During the cutover the runner and Foundry never watch the same repo. A repo moves when the runner has no run of it in `triage` or `building`: it is taken out of the runner's `repos:`, then its switch goes on in Foundry.

Foundry also checks. An issue of a watched project in `triage` or `building` whose newest claim comment lacks Foundry's marker (F13) is a foreign claim: it raises an attention item and Foundry leaves the issue alone.

An issue the runner left in `pr-open` is not read for feedback. Setting it `factory:ready` again starts a Foundry run that carries on from its pull request's branch, as the runner does with a pull request of its own.

## UI

Runs are already workspaces on the board, so the UI adds little. Everything here is drawn from `factory_status`, so it is the same on every machine.

- The Projects dialog gains the factory switch, the GitHub login and pause for each project.
- The Factory view's header holds the fleet's settings: pause, model, effort, `max_runs`, `max_gates`, `poll_seconds` and `feedback_seconds`.
- A Factory view lists each project with:
  - its lease holder and generation, or why its factory is idle
  - a line when the repo sets `factory.owner`, saying it is ignored
  - its ready queue in claim order, each issue with why it waits: blocked, not approved, no room, paused
  - its feedback rounds that wait to start for want of room
  - its live runs: issue, step, machine, attempt, gate retries used and minutes used, each linked to its workspace and with Stop
  - its last 20 finished runs and how each ended
- A factory run's workspace card says it is a factory run and links to its issue.

## Attention items in P2

Every machine works these out from `factory_status`, as it works out P1's items from records.

| Item | Raised when | Clears when |
|---|---|---|
| Run stuck | A run goes stuck | The issue leaves `stuck` |
| Run needs info | A run posts questions | The issue leaves `needs-info` |
| Factory idle | The status gives a reason: the holder has no token for the project's login, or `.dw/config.yml` does not load, or `gh` failed three passes running. Or the newest status does not hold the lease, or no fresh status has arrived, for 10 minutes for a project whose factory is on | The cause clears |
| Foreign claim | The status lists one | The issue is closed or leaves `triage` and `building` |
| Gate infra | A gate could not run at all | A gate of that run starts, or the run ends |

A factory session's "stuck", "failed" and "waiting for you", and a failed setup of a factory workspace, raise no P1 item of their own (G13). The run's item covers them.

## Testing

- The runner's tests are the spec. For each ported module, its tests become Rust tests with the same cases and names, written first and failing before the code lands.
- `factory::github` sits behind a trait. Its fake answers from the runner's `tracker/fixtures/`. No test calls GitHub.
- Lease tests push to a local bare repo with a clock the test moves: two machines racing, a takeover after the wait, a first take of a ref that does not exist, a ref deleted under a live holder and taken again at a higher generation, a restart that renews its own lease, a holder that lost the lease and stops, a holder whose clock jumps past the limit (a sleep) and does not act, a push whose answer comes late, a split.
- Fleet tests use P1's three daemons and the fake agent, which ends a turn with a result line in its Stop hook. They cover:
  - an issue reaching `pr-open`, with its build session ended and workspace gone
  - a failed gate sent back and passed on the retry
  - needs-info
  - a feedback round that waits in `pr-open` while every run slot is held, uses none of `limits.minutes`, and starts when one frees
  - a feedback round with one machine that qualifies, the one that built the run, where the local branch is moved to `origin`'s
  - the coordinator killed during a build and the next one carrying the run on with its events in order
  - a coordinator killed between the claim comment and the label, with the issue claimed again, and one killed between the label and the status comment, with the claim carried on; neither is taken for a foreign claim
  - a coordinator killed between the claim and its first event, with no workspace, with a workspace, and with a workspace whose record arrives a pass later; none is taken for a foreign claim
  - a worker's daemon restarted before the launch message was typed, and the message sent again
  - a build session idle through a whole gate, sent nothing
  - a turn that ends before its `session_turn` record reaches the coordinator, with the message not typed a second time
  - a gate failure fixed in one cycle, spending one of `gate_retries`
  - a claim whose machine never answers, launched on another
  - a pull request whose head keeps moving, ended stuck by `gate_retries`
  - a worker's daemon restarted during a build, and the result line still read
  - a worker cut off from the lease holder ending its own session, and the run resuming on another machine
  - a lone coordinator hosting its own run and ending nothing
  - a split where only one side acts, and the other side's status, marked as not holding, keeps no session alive
  - a gate whose machine goes offline, run again on another machine
  - a gate command that hangs, killed at its time limit, counted as a gate failure and its gate slot freed
  - a session that hangs on a gate failure sent back, ended by `limits.minutes`
  - a peer on protocol 4 that receives no factory record, then updates and receives them all
  - the Factory view and its attention items read from a machine that is not the coordinator, and Stop sent from it
- A gate test opens a pull request whose branch changes `provision` and `gate`, and checks the default branch's commands ran, and that the failure sent back says the two configs differ.
- A gate test checks the gate terminal's environment holds no token, and that `git` run by a gate command gets none from a credential helper set for the machine.
- A redaction test puts a token in gate output and checks it reaches no comment, message or event, and that a `github.com` pull request or commit link with a long id comes through whole. A second puts a made-up key of 30 mixed letters and digits, held by no machine, in a session's questions and in gate output, and checks that the comment hides it, the event keeps it, and a `github.com` pull request or commit link in the same comment comes through whole.
- The export test compares `foundryd factory export` of a fixture run with a golden file copied from the runner's own ledger output, in this repo, with the runner commit it came from named beside it. CI needs no Node and no second checkout.
- No test spends model calls. One `#[ignore]` contract test, run with `FOUNDRY_CONTRACT=1`, starts a real factory session with skipped permissions and a first prompt, and checks the whole result line arrives.

## Cutover

1. The issues under "Before P2 starts" are closed and P1's acceptance run passes.
2. P2 ships in a release, and every machine is on protocol 5.
3. When the runner has no foundry run in `triage` or `building`, foundry leaves the runner's `repos:`, and its factory switch goes on in Foundry.
4. Foundry builds foundry until 10 runs have reached `pr-open` or `stuck` and no factory bug is open.
5. The runner's other repos move one at a time, each after the last moved repo has 5 clean runs: factory-paradev-spike, spec-reader, clockwork, factory-on-paradev, people-hub.
6. The runner stays stopped, still installed, for a week.
7. `retro` compares the two. Then the runner is uninstalled.

The way back, per repo: turn its switch off in Foundry and put it back in the runner's `repos:`. While Foundry's factory cannot build foundry, the runner builds the fix.

## To check before the plan is written

| # | Assumption | Why it matters |
|---|---|---|
| 1 | Interactive Claude Code started with `--dangerously-skip-permissions` in a P1 terminal does not stop at a one-time confirmation of the bypass mode. Not true as written. Checked on 2026-11-22 with Claude Code 2.1.322 in a PTY: under a config folder that never accepted the mode, it stops at "WARNING: Claude Code running in Bypass Permissions mode" with "No, exit" selected, and `SessionStart` never comes. With `skipDangerousModePermissionPrompt: true` in the file given to `--settings`, the same config folder starts with no confirmation. This Mac's user settings already hold that key, so here it starts either way | A factory session cannot wait for a person |
| 2 | The Stop hook's `last_assistant_message` holds the whole final message. Confirmed on 2026-11-22: a final message of 150 lines and 7,772 characters arrived whole and equal to the transcript's last assistant text, with the result line last. The field holds the text after the turn's last tool call only: a word the model wrote before the tool call was not in it | The result line and the pull request URL come from it |
| 3 | With `GIT_ASKPASS` giving the token and `credential.helper` emptied for the call, `git push` over HTTPS acts as the project's login on macOS and Linux, on a Mac whose keychain holds another GitHub account. `GH_TOKEN` alone covers `gh` but not git. Confirmed on this Mac on 2026-11-22 with dry-run pushes: with helpers emptied, a made-up token from `GIT_ASKPASS` is refused and a real one accepted, and with no askpass nothing answers, so neither the keychain nor `gh`'s helper leaks in. Linux is open | Otherwise the keychain's account pushes |
| 4 | GitHub accepts force-with-lease pushes to `refs/factory/lease` on these repos, also when the ref does not exist yet, no ruleset blocks them, and they start no workflow. Confirmed on factory-paradev-spike on 2026-11-22: a take of a missing ref, a renewal, a refused stale renewal, a refused second take, a read by fetching that ref alone, and a delete. Still open: that the push starts no workflow, since that repo has none, and rulesets, which the API does not show for a private repo on this plan | The lease rests on it |
| 5 | A push a minute per repo, 1,440 a day each across six repos, trips no GitHub rate limit, and parentless commits that the ref has left do not pile up in the repo | The lease's cadence rests on it. If it fails, renew less often and wait longer before a takeover |
| 6 | A line of under 800 characters starting `/factory-build`, typed as P1 types a first prompt, loads the skill, the model reads the file the line names when it is outside the worktree, and `UserPromptSubmit` fires for it. Confirmed on this Mac on 2026-11-22, with opus at high effort and the skill as it is today: a line of 109 characters loaded the skill, `UserPromptSubmit` reported the line exactly, and the model's first tool call read the file, which sat in a folder outside the worktree with mode 600. It read it with `cat`, not the Read tool. One run, with a body that told the session to build nothing; it ended on the result line the body asked for. A body that starts a real build was not tried | Every launch rests on it (F12), and the resend rule counts that hook |
| 7 | No gate command of the six repos calls `gh` or fetches a private dependency from GitHub. Read on 2026-11-22: no `provision` or `gate` command, Makefile, script or dependency manifest in five of the repos does, from the runner's clones, which are up to ten days old. Still open: factory-paradev-spike's scripts, and whether any test starts a real `gh` | The gate terminal has no token |
| 8 | Interactive Claude Code takes the effort setting by a flag or variable, and the gateway passes it through. Confirmed on 2026-11-22: `--effort <level>` and the variable `CLAUDE_CODE_EFFORT_LEVEL` each set `output_config.effort` on the requests Claude Code sends (`low` and `max` by flag, `medium` by variable), and the Stop hook's `effort.level` names the same level. With neither, this Mac sent `high`, the level its user settings name for the model. The gateway passes the field on: a made-up level sent to it came back as Anthropic's 400 for `output_config.effort`. The request for a session's title carries `medium` whatever the setting | The factory setting names an effort |
| 9 | `CLOCK_MONOTONIC` on macOS and `CLOCK_BOOTTIME` on Linux count through sleep on the fleet's machines. Confirmed for macOS on 2026-11-22; Linux is open | The lease's ages and the cut-off rule rest on it |
| 10 | Two worktrees of one bare clone cannot hold the same branch, and moving a local branch to `origin`'s for a feedback workspace works once the build workspace is archived. Confirmed on 2026-11-22 with git on this Mac: the second worktree is refused while the first holds the branch, `git worktree add -B` moves the branch once the first is removed, and `--detach` gives a commit with no branch | The workspace table rests on it |
| 11 | A session that has the skill, sent the line `The factory gate failed` with a path, opens the file and acts on it. Confirmed on 2026-11-22, in the session of check 6: sent the line with the path of a file in the runner's gate-failure wording, its first tool call read the file, then it fixed the cause, ran the gate command and committed. The scratch repo had no remote and no pull request, so it ended with `FACTORY-STOP: stuck`. The push and the `FACTORY-DONE` reply were not seen | Gate retries rest on it |
