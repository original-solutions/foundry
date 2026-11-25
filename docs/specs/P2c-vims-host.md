# P2c: Hosts, and the Vims host

Product: Foundry, a dark software factory that also runs manual sessions.

Status: draft 2026-11-25, tenth revision. Seven revisions went through a council review by four models, which ended with all four at `ready`. This one adds what a second council decided about the gate's tools, the naming of a host as one integration among others, and what the first checks found. Nothing is built. It rests on P2, which is not finished, and on the checks at the end.

## Why

A fleet run takes a session slot and a worktree on one of Tom's machines, and shares that machine's database and ports with every other run. Vims (`https://vims.carevicinity.com.au`) makes a workspace from a saved template in seconds, with the repo checked out, the database and services up, Claude Code signed in on Tom's subscriptions and a GitHub token for the one repo. A run in its own Vims workspace takes no fleet machine and shares nothing with the run beside it.

## Goal

For a project whose host is Vims, the factory builds each claimed issue in a fresh Vims workspace made from the project's template, gates the pull request in a second fresh workspace, and removes both. The lifecycle is P2's: labels, claims, the lease, the result line, retries, feedback, the ledger.

First project: `Trilogy-Care/tc-supplier-portal-v2`, with the template `tc-supplier-portal-v2` (4 vCPU, 8 GiB, repo at `/workspace/tc-supplier-portal-v2`).

Exit criterion: 10 runs of that repo on Vims have ended at `pr-open` or `stuck`, no factory bug is open, and they are no worse than the baseline on two counts. The baseline is t3fac's last 30 ended runs of the same repo, or as many as there are, read from `~/.config/t3fac/factory/runs` on `trilogy-mac` and copied into the cutover issue with their counts before the cutover.

- Gate pass: runs whose first gate passed, of runs that reached a gate. The 10 runs may have at most one fewer pass than the baseline's rate gives for their number.
- Stuck: runs that ended `stuck`, of runs that ended. The 10 runs may have at most one more than the baseline's rate gives for 10.

## Hosts

A host is somewhere a run's workspaces are made. The fleet is one host, and Vims is the second. Others may follow. This spec builds Vims and names the shared parts for what they are, so that a third host is a third implementation and nothing else.

- **What is shared.** P2's `Worker` is the one thing a host implements. A project's host setting, the record a new lease holder reads, the status fields and the claim marker name a host by its kind and are the same shape for any host.
- **What is a host's own.** Its options, such as a Vims template. Its ids, its commands, its checks, and what it must be told to keep a secret.
- **What a repo says.** `.dw/config.yml` names the checks, in plain commands that name tools only. A host supplies those tools at the versions the repo's CI pins, a checkout git accepts, and room to run them. A check that fails for want of these is fixed in the host. The config never names a host's containers or carries a command that makes sense on one host only.
- **What is not built.** No plugin loader, registry or host API beyond `Worker`. A host is a module of `foundryd`, chosen by a kind in a setting.

## Scope

In:

- A host setting per project. For Vims it holds a template name, and is accepted only for a repo whose owner is in the Vims host's fleet setting `host:vims.owners`, which starts as `[Trilogy-Care]`.
- A second implementation of P2's `Worker` that drives the `vims` command line, and the changes to P2 that make room for it.
- Claiming, gates, resumes and feedback rounds for a Vims project without a fleet machine.
- The cutover of tc-supplier-portal-v2 from t3fac.

Out:

- A Vims workspace joining the fleet as a machine. It never runs `foundryd`.
- A Vims run on the board, the timeline or P1's attention list, and its terminal in Foundry's UI. The Factory view lists the run and names its workspace, and `vims shell <workspace>` opens it.
- Manual sessions on Vims.
- Repos outside `host:vims.owners`.
- Vims lanes (`vims test --workspace`) as the gate. Worth a look later.
- A fleet with machines on both protocol 5 and protocol 6 running a Vims project.
- Merging. Trilogy repos stop at a pull request, as P2 does.

## Decisions

| # | Question | Decision |
|---|---|---|
| V1 | Fleet machine or driven from outside | Driven from outside. The lease holder runs `vims`. Joining each workspace to the fleet would need an unattended join, secrets on a new machine and a permanent machine-list entry per issue |
| V2 | P2's F3, "no second way of starting work" | Kept for fleet projects. A Vims run is the one exception. It sits behind `Worker`, and P2 changes to carry it (see "What P2 gains"). P1's rules that a session is a terminal held by a holder, takes its state from hooks and calls its model through the gateway (D5, D6, D9) hold for fleet sessions only |
| V3 | Model calls | A Vims session uses Vims's own proxy and Tom's subscriptions. Foundry sends no gateway key, and no effort: `vims session start` takes a model and nothing else, so a Vims run has Vims's default effort |
| V4 | GitHub identity | One login, two tokens. A Vims project's login is the workspace owner's GitHub login, `tomkaczocha`. The coordinator writes labels and comments with its `github:tomkaczocha` token, as in P2. The workspace pushes and opens the pull request with the token Vims's GitHub App gives it for that login. Foundry sends no token to a workspace. So P2's rules that count a claim marker and adopt a pull request by the project's login hold unchanged |
| V5 | Who owns the workspaces | Tom. Commits and pull requests are authored by him. No team agent |
| V6 | Gate | A fresh workspace made with `--egress closed`, at the pull request's head commit, running `provision` and `gate` from the default branch's `.dw/config.yml`. The fetch goes through a read-only grant that is gone before any pull request code runs. If check 5 shows that cannot be done, the plan stops and the choice goes back to Tom. Pull request code never runs beside a GitHub token |
| V7 | A stuck run | In every workspace the run has, its gate workspace included, anything still running is cancelled, the session is stopped, and the workspace is stopped with `vims stop`, which ends its compute and keeps its files. It is removed when the issue leaves `stuck`, or 14 days after the stop, whichever is first. The 14 days are Foundry's rule; Vims counts its own free 14 days from when the workspace last ran. The stuck comment says so |
| V8 | Finding what a lost answer or a dead coordinator left | Vims's idempotency key, one per call, finds the operation of a call whose answer was lost. A `host_run` record tells the next lease holder what each call was made with (see "Keys"). Foundry passes no name and never searches `vims ls` |
| V9 | Slots | Vims runs and gates count against the fleet's `max_runs` and `max_gates`. The subscriptions are the limit both hosts share |
| V10 | Credit and quota | No limit of Foundry's own. Tom's quota is 8 vCPU today and the template takes 4, so two workspaces run at once. When Vims refuses a create or a start for credit, a member's limit or quota, the run waits with the reason shown. A refusal never makes a run stuck and spends no retry |
| V11 | When Vims cannot be reached | The run waits. It is never an interruption, and no second workspace is started while the first may still run |
| V12 | A build workspace during its gate | Paused with `vims pause` when its session reports the pull request, so the gate has room in the quota. It stays the run's active workspace. A gate failure is `vims start`, then the send |
| V13 | Where the gate's tools come from | The host. One declared Vims base holds what the repos' CI pins: PHP 8.4 and 8.5 with CI's extensions, `pdo_sqlite` and `pcntl`, Composer 2, Node 26 beside Node 24, a compiler, and git's `safe.directory` set to `/workspace/*`, which covers each checkout under it. Templates rest on it and pick their versions in their start script. The gate does not run in the app's containers: a trial of that failed on file ownership, and its schema check passed while git could not read the checkout |
| V14 | A dev stack in the gate's way | A project may name a gate template, which rests on the same base and starts none of the app's services. Blank means the gate uses the build template. The Supplier Portal needs one: its nine services and a Next build do not fit in 8 GiB together, and the only larger Vims machine takes the whole quota |

## Keys

Every `vims` call that makes or changes something carries its own idempotency key: the repo, the run, the workspace's role and round (`build`, `g<k>`, `r<k>`, `f<k>`), the step, and an attempt number.

- Steps are `create`, `fetch`, `checkout`, `provision:<i>`, `gate:<i>`, `session`, `body:<n>`, `send:<n>`, `grant`, `ungrant`, `pause:<k>` and `start:<k>` for the k-th pause and the k-th start of that workspace, whatever caused it, `cancel:<operation>`, `stop-session`, `stop` and `remove`. Anything done more than once to one workspace has its count in the step, so no two actions share a key.
- After a lost answer, the operation is looked up by its key first. The call is sent again with the same key only while Vims still answers a repeat with the operation it already has (check 1 says for how long), or when the lookup shows Vims never took it.
- A call that Vims refused, or that could not run, is tried again under the next attempt number, and only where this spec says it is tried again. The same key would only return the old refusal. A `provision` or `gate` command that ran and exited with a failure is a result, as in P2, and is never run again by the worker.
- A setup step starts only when `operation show` says the step before it finished. V7's steps wait for nothing: a cancel is sent while its operation runs. When an outcome stays unknown, because Vims no longer knows the key or will not say, the run is made stuck with that as its reason, and nothing is sent again but V7's steps for a workspace whose Vims id is known.

Keys answer for a lost reply. They do not tell the next lease holder what a call was made with. For that, the lease holder writes a `host_run` factory record for each run, and writes it before it sends the call it describes. The record names the host's kind, and the rest is that host's own. For Vims: each workspace's role and round, the template source key and the commit it is checked out at, the commands of its setup, the attempt number each step is on, and the workspace's Vims id once Vims has given it. A workspace stays in the record until Vims has confirmed its removal. The record's key is the run and the lease generation, since no machine writes another's record. It is a factory record like P2's others, and no part of the export. It is a new kind of record, which a protocol 5 build refuses, so it is sent only on protocol 6 links (see "Settings and sync").

A new holder reads the newest record for the run, by lease generation, and writes its own from then on. The old holder can die after Vims took a call and before the record of it synced, so the record can be behind, or missing. A key is made only from the repo, the run, the role and round, the step and the attempt, so the new holder can work out every key the old one could have sent. Before it sends anything, it looks each step up, starting at the attempt the record holds for it, or at attempt 1 when it has no record, and follows every refusal Vims confirms to the next attempt wherever this spec allows a retry, until it reaches an operation Vims took or a key Vims never had. It does the same again after each outcome it adopts, for the steps and workspaces that could follow. It sends a call only when Vims confirms that key was never taken and it knows what the call is made with. An operation Vims already has is that step's outcome: a success is carried on and written into the holder's record, a refusal is retried under the next attempt only where this spec already says so, and one still running is waited on. A `create` operation that names a workspace the record does not hold is taken. The operation does not say what the workspace was made from, but the workspace does: `vims show <workspace> --json` gives the digest of the build it rests on, as `sha256:<digest>`, and the holder writes `golden:<digest>` in its record as the source key of that workspace's role, build or gate, and of no other. A round with a gate template has two keys. A key for the round's other role that the record does not hold, and that no workspace of that role shows, is not copied from this one and is not filled from the template's current key: the holder waits, and after `limits.minutes` the run is stuck, with the workspace it found kept in the record for its removal. Where the holder cannot tell what was sent, it waits, and after `limits.minutes` the run is stuck. A create it cannot settle stays listed in the status, and no replacement workspace is made and the project's host is not changed until its workspace is found and removed or Vims confirms there is none. A new attempt is never sent because an old result came back.

## What P2 gains

P2 gives its `Worker`, its `FleetView` and its status record a fleet machine throughout. Task 19 (#146) is not built, and the tasks that read it are not either. The claim marker's parser (`factory/github/write.rs`) and `LiveRun` (`foundry-proto/src/factory.rs`) are built, so those two are changes to code on `main`.

- **Where a run goes.** `Worker::create` and the claim take a `Target` of `Machine(MachineId)` or `Host(kind)` where they take a machine. `vims` is the one kind this spec adds.
- **The claim marker.** A run on a host reads `<!-- foundry:claim <run id> <kind> -->`, here `vims`, and the parser learns a kind beside a machine id. A build that does not know it treats the claim as another factory's and leaves the issue alone.
- **Ids.** A Vims workspace or session has a Foundry `WorkspaceId` or `SessionId` that is a version 5 UUID, in one namespace fixed in the code, of its `create` or `session` key without the attempt number, so it does not change when a create is tried again. The Vims ids (`wsp_`, `ags_`) are kept in the run's `host_run` record (see "Keys"), never in a run event.
- **`FleetView`.** A trait with two sources: the store for a fleet run, and one read of Vims per pass for a Vims run. `workspace_named` answers from the `host_run` record. `online` is true while `vims` answers. `unreachable_for` counts only while it does not, and starts no resume (V11).
- **Setup that takes many passes.** On the fleet a create starts one setup terminal and later passes only read it. On Vims a create is a row of steps, each its own call. `Worker` gains `advance(workspace)`, which `runs` calls each pass under the lease guard for every workspace still being set up: it reads the step under way and, when that has finished, sends the next. On the fleet it does nothing.
- **Rest, wake and keep.** `Worker` gains `rest(workspace)` and `wake(workspace)`, called by `gating` when a session reports its pull request and before a gate or check failure is sent, and `keep(workspace)`, called when a run goes stuck. On Vims they are `vims pause`, `vims start` and `vims stop`. On the fleet they do nothing.
- **The ledger.** Run events are the runner's, with nothing added, and the export does not change. What a pass reads from Vims is not recorded. A new coordinator rebuilds from the claim comment, the run's events and its `host_run` record.
- **Status.** `LiveRun` keeps its shape, so a protocol 5 build still reads it. For a run on a host, `machine` is the lease holder's id, and an added `host` field that is absent for a fleet run holds the host's kind, the workspace's name on that host, whether the host is unreachable and what the run waits on. `FactoryStatus` gains a list, absent when empty, of the project's workspaces still to be removed, since a run that has ended is no longer a live run.
- **Placement** is not asked for a Vims project (see "Claiming").

## How a run maps

Each step is one `vims` call with `--json` and its key. A call that can outlast one pass is sent with `--no-wait` where check 2 shows the command takes it, returns an operation id, and is read on later passes with `vims operation show`. A call that gets no answer within 30 seconds has failed for that pass, and the next pass finds its operation by the key. No pass waits on a workspace.

A command from `.dw/config.yml` is a shell string, and `vims exec` runs an argv with no shell. So each runs as `vims exec <workspace> --cwd <checkout> --timeout 1800 -- bash -lc <command>`, the command as one argument. The checkout path is the template's source path for the repo, from `vims templates show --json`, given to `--cwd` without its leading `/`, which Vims requires. `operation show` gives the exit code, and whatever it gives for a command that ran past its limit is read as P2's timed-out command. `operation logs` gives the output, which is the `tail`.

| P2 `Worker` call | On Vims |
|---|---|
| `create`, a new branch (build) | `vims new <source key>`, then fetch and make `fac/<N>-<attempt>` from the base, then the base's own `provision`, which the coordinator reads from `.dw/config.yml` at the base's head through `gh api` |
| `create`, an existing branch (feedback, a resume after a push, a pull request adopted when an issue goes back to `ready`) | `vims new <source key>`, then fetch and check out the pull request's branch, then that branch's `provision`, read through `gh api` as a build's is |
| `create`, a resume before any push | As a build, on P2's resume branch `fac/<N>-<attempt>-r<k>` from the base |
| `create`, one commit (gate) | `vims new <the gate template's source key> --egress closed`. A read-only grant with a short `--for`. Fetch the head commit. Take the grant away and see that it is gone. Check out the commit with no branch. Then the default branch's `provision` and `gate`, from the coordinator's own read of that branch and never from the file in the checkout. The first exit code that is not 0 is the gate's result. A create, grant, fetch or checkout that fails is a gate that could not run, as in P2 |
| `start_session` | Uses the session named `build` on that workspace when there is one, else `vims session start <workspace> --name build --model <model>`. Then writes the body and sends the line (below) |
| `send` | `vims session send <session> <line>`, once per ledger message |
| `turn(session, n)` | `vims session read <session> --all --json`, the n-th turn by position. Never the plain `read`, which shows what is new "since this Mac last read" and so differs on the next coordinator |
| `tail` | `vims operation logs <op>` |
| `rest`, `wake`, `keep` | `vims pause`, `vims start` (V12), `vims stop` (V7) |
| `end_sessions` | `vims operation cancel` for anything still running, then `vims session stop` |
| `archive` | `end_sessions`, then `vims remove <workspace> --in-use`, looked at each pass and sent again under the next attempt after a refusal, until it has gone. A remove still refused does not make the run stuck; the status lists the workspace as still to be removed |

On `pr-open` the build workspace is removed, as P2 archives it. A gate workspace is removed when its gate ends, and a failed gate's workspace has gone before its build workspace is woken, so the wake has room. A stuck run is not archived: `end_sessions`, then `keep` (V7).

**Source key.** The coordinator reads the template's source key (`golden:<digest>`) before a round's first create, writes it in the `host_run` record, and makes that round's workspace from it. It does the same for the gate template, when the project names one, and makes the round's gates from that; with none named, the gates use the build template's key. Both keys are in the record before the round's first create. A gate's checkout path comes from the template its workspace is made from. So a template that moves between a build and its gate changes nothing, and a new lease holder makes the same request. A later feedback or resume round reads the template again, since Vims keeps an older build only while something rests on it.

**Git in a workspace.** `vims exec` runs as root. Where the session runs as another user, Foundry's commands leave nothing in the checkout, and no body file, that the session's user cannot read and write. Foundry's own fetch and checkout commands keep P2's rules: a branch name P2 refuses is refused here, every name goes after `--`, and hooks are off for the call. The session starts in the checkout, not in `/workspace` (check 4).

**Messages.** P2's line and body stay. The body is written into the workspace by `vims exec` with the bytes on standard input, never as an argument, to a file outside the checkout readable by the workspace user only. The line is `/factory-build <owner>/<repo>#<N> <path>`. The body keeps the sentence the skill reads to skip its own GitHub account step, and says the token is Vims's for `tomkaczocha`. The `factory-build` skill itself does not change.

**Sends.** P2 refuses a send on the worker unless the session is idle and its prompt count has not risen. Vims has no count, and it keeps a message until the turn under way ends. So a Vims send is never repeated on a guess: whether it arrived is read from the operation its key names, and it is sent again only under the rules of "Keys". Once a session's stop has been asked for, nothing more is sent to it.

**Session state.** Working, turn ended, waiting for a person and process ended are read from `vims session list --json` and `read --all --json` (check 3). A build workspace paused for its gate is neither an interruption nor waiting for a person.

## Claiming

A Vims project's claim, gate, wake, resume and feedback round each start only when:

- a run slot or gate slot is free (V9), for the ones that take one,
- the lease holder's Vims sign-in works and its GitHub login is the project's,
- the template, and the gate template when one is named, each exist, are not retired, and have this project's repo as their source,
- the Vims profile holds the `factory-build` skill, and
- `vims quota show` has room for the machine of the template the step makes or starts a workspace from, counting the creates and starts this pass has already sent.

In a pass, room goes first to wakes, then gates, then resumes and feedback rounds, then claims. A gate is made only on a pass that has seen its build workspace's pause finish.

A claim is P2's, in P2's order: the comment, the label to `triage`, then the workspace. Nothing is made for an issue still in `ready`, so no workspace exists without a claimed run to answer for it.

When Vims refuses a create or a start for credit, a member's limit or quota (V10), the step waits where it is: a new run in `triage`, a round in `pr-open`, a gate or wake in `building`. The Factory view gives Vims's reason. The step is tried again, under the next attempt number, on the first pass where the quota has room, and every `feedback_seconds` after a refusal for credit or a limit. None of the wait counts toward `limits.minutes`. A run that waits holds its slot.

## When things go wrong

- **No stale push.** P2 has a cut-off worker end its own sessions (G10). A Vims workspace cannot see the lease, keeps running while Foundry is away, and renews its own GitHub token. So the rule moves to the coordinator: a resume, feedback or replacement workspace is made only after the earlier one's `end_sessions` and `remove` have both succeeded. A timeout never counts as success.
- **Vims unreachable** (V11). The run waits where it is. After three failed passes the project raises "factory idle". `limits.minutes` still counts, and when it runs out the run is marked stuck, but nothing in its workspace can be ended until Vims answers. The status says the workspace may still be running, and V7's steps are sent on the first pass that reaches Vims.
- **Stop**, from the Factory view, and a run going stuck, are V7's three steps on every workspace of the run. Stop shows as pending until each has succeeded, and no replacement is made before then. A stuck run's workspace that a person started again with `vims shell` is left to that person.
- **A new lease holder.** It carries on the same workspace and session, found in the run's `host_run` record. The lease guard is checked before every `vims` call that changes anything, as before every `gh` call.
- **Credit or a subscription limit running out mid-run.** A session whose model calls fail ends its turn with no result line, or ends, and P2's rules for those apply. A workspace Vims has put to sleep for want of credit is not an interruption: it is still there, so the run waits on it, spends no resume, and starts it again with `vims start` once Vims accepts one (V10, V11). The wait does not count toward `limits.minutes`, as no wait on a refusal does.
- **Workspaces left behind.** Foundry knows its own workspaces by its `host_run` records and never touches another. Each pass asks again for the removal of every workspace of a run that has ended or moved on, until Vims says it is gone. The status lists each one still there, since a running workspace costs money by the hour.
- **Nobody watching.** With every coordinator down, a session runs on until its turn ends and its workspace sleeps. P2's G10 would have ended it. This spec accepts that: the pull request it may open is a draft, and the next holder gates whatever head it finds.
- **Removing a project, turning Vims off or changing either template** is refused while the project has a run that has not ended, `pr-open` and `stuck` included, or a workspace still to be removed.
- **Secrets in output.** Vims issues tokens Foundry never sees, so `hide_known` cannot hide them. Output from `operation logs`, and a session's text from `session read`, go through P2's rule for comments (every word of 24 or more mixed letters and digits hidden, a `github.com` pull request link kept whole) before they reach an event as well as a comment. The result line is read first, from the text as it came.
- **What a gate workspace holds.** It is made from Tom's template and profile, so it may hold his MCP keys and reach the model proxy and his connections. Pull request code runs there, and its output comes back to Foundry and into comments. Check 5 fails if that code can read a live secret of Tom's or use one of his connections, and the plan stops until a gate workspace can be made without them.

## Settings and sync

A build that does not know about Vims must never build a Vims project on the fleet, and must never be sent a record it cannot read.

- A project on a host has a setting of its own, `host:<project>`, holding the host's `kind`, `on`, `paused`, `login`, and that host's options. For Vims the options are `template` and `gate_template`. Its P2 switch, `factory:<project>`, stays off. An older build sees a project with no factory.
- `login` is written at turn-on from `host:vims.login`. Turn-on is refused unless `vims github --json` gives that login for the repo's organisation and the coordinator holds `github:<login>` for it. Each pass checks both again and leaves the project idle when either fails. A Vims project's login is not edited on the project.
- A host's fleet settings are one key, `host:<kind>`. `host:vims` holds `owners` and `login`, the one GitHub login Vims runs act as, which starts as `tomkaczocha`.
- These keys come with protocol 6 and follow P2's G7: never sent to a peer on an older protocol, and left alone when a copy from such a peer is merged.
- Turning Vims on is refused unless every machine in the machine list speaks protocol 6.
- A Vims project's statuses and run events have the shapes protocol 5 reads, and sync as every factory record does. Only `host_run` records are held back from a protocol 5 link. Factory records share one version per owner, so a record held back would be skipped for good once a later one went through. So a machine that speaks 6 on a link for the first time asks that peer for its factory records from zero. It keeps asking from zero on that link until one catch-up has run to its end, so a link that drops part way loses nothing. Records it already has are ignored by the version rule.
- While any Vims switch is on, every machine in the fleet stays on protocol 6. That is an operating rule, since a protocol 5 coordinator sees no Vims switch and can enforce nothing about one: it could turn the project's fleet switch on, or remove the project. A machine is added only on a build that speaks 6. A machine whose store holds a `host_run` record does not go back to a build below 6, which could not read it. Going back from Vims to the fleet or to t3fac is a change of host, made with every machine still on 6.
- A protocol 6 coordinator that finds both of a project's switches on leaves the project idle and says why.
- Turning Vims on is refused while the project has a fleet run that has not ended. P2's "factory idle" item is raised for a project whose Vims switch is on, by the same rules.
- A project is not removed while its Vims switch is on.
- Turning Vims on needs a template that is not blank, and checks it and the gate template as a claim does. The repo's owner is matched against `host:vims.owners` in any case. Taking an owner out of `host:vims.owners` is refused while a project of that owner has Vims on.
- `MachineHeld` gains nothing. A lease holder that cannot sign in to Vims says so as the project's idle reason.

`foundryd secret set vims` stores a Vims access token in the machine's secret store. The daemon passes it to `vims` as `--token-ref env:<NAME>` with the token in the child's environment, never as an argument, and `hide_known` hides it.

## UI and API

- The Projects dialog gains, for a repo whose owner is in `host:vims.owners`, the Vims switch, its template, its gate template and its pause. `host:vims.owners` is edited in the Factory view's header. Both go through P2's factory settings routes.
- The Factory view shows a Vims run with its Vims workspace name where a fleet run has its machine and workspace link, what it waits on, and the workspaces still to be removed. Stop works as for any run and shows as pending until it is complete.

## Testing

- `factory::vims` sits behind a trait, as `factory::github` does. Its fake is scripted per command and keeps operations by idempotency key. No test calls Vims.
- The fleet tests of P2 that take an issue to `pr-open`, send a failed gate back, and kill the coordinator mid-build each run a second time against the fake Vims host.
- Tests of its own:
  - a create whose answer is lost, found by its key
  - a refused step tried again under the next attempt, a second gate that makes a second workspace, and a second gate round that pauses the build workspace again
  - a claim that waits on quota across passes in `triage`, with one comment and one workspace
  - a coordinator killed after Vims took a `new` and before its `host_run` record synced, with the next one finding the workspace by its key and making no second; the same after two refusals and a third attempt that Vims took, none of them synced
  - a takeover with no record that finds the build workspace by its key and reads its source key from the workspace, and one where the gate's key cannot be recovered and the run waits and goes stuck
  - a takeover after the template moved, making the gate from the source key the first holder wrote
  - a coordinator killed between two setup commands, with the next one running neither twice
  - a send whose answer is lost, not delivered twice
  - a takeover that reads the same turns
  - a resume held back until the earlier workspace is removed
  - Stop pending while Vims is unreachable, and complete only after all three steps
  - a full quota that leaves a run waiting and spends no gate retry, and a wake that takes room before a claim
  - a gate whose grant is gone before the checkout
  - a protocol 5 link that carries no `host_run` record and reads a Vims run's status, a machine that moves to protocol 6 and then receives every `host_run` record, and a fleet with one protocol 5 machine where Vims cannot be turned on
- One `#[ignore]` contract test, run with `FOUNDRY_VIMS_CONTRACT=1`, makes a real workspace, runs a command, reads its result by operation and removes it. It spends credit and no model call.

## Before it starts

- P2 is finished and the runner's cutover is done, as P2's "After the runner: t3fac" orders it.
- Every machine in the fleet speaks protocol 6.
- The Vims organisation's limit on declared bases has room for one more. It stood at 11 of 10 on 2026-11-25 and refused a build.
- The shared base of V13 is built, and the project's build and gate templates rest on it.
- `vims` is installed, and `foundryd secret set vims` done, on every machine that can act as coordinator.
- The `factory-build` skill is in Tom's Vims profile (`vims profile save`), so every workspace has it.
- The project is added in Foundry with the URL `https://github.com/Trilogy-Care/tc-supplier-portal-v2`. P2 refuses a repo reached over SSH.
- Every coordinator holds a `github:tomkaczocha` token that can write labels and comments on the repo, mark a pull request ready or draft, and push `refs/factory/lease`.
- The baseline is copied from `trilogy-mac`.

## Cutover

t3fac on `trilogy-mac` builds this repo today: the repo is in its `watched-projects.json`, and it wrote run files for it on 2026-11-25. t3fac is being retired, so this move is one-way once t3fac is uninstalled.

1. t3fac stops claiming the repo's issues. Its runs, gates and feedback rounds drain, and then the repo is taken out of its watched projects. The drain is what keeps two factories off one issue. P2's foreign-claim check is no backstop here: two issues t3fac claimed on 2026-11-25 (#1680, #1686) carry a status comment and no claim comment.
2. Every `factory:ready` issue but one is set to `factory:proposed` by hand. P2's pause stops every claim of a project and lets none through, so the labels do it.
3. The project's Vims switch goes on. When the one issue reaches `pr-open`, the others are set `factory:ready` again.
4. An issue t3fac left in `pr-open` is not read for feedback, as P2 says of the runner's. Setting it `factory:ready` starts a Foundry run on its pull request's branch.

The way back, while t3fac is still installed:

1. The Vims switch is paused, which stops new claims as P2's pause does.
2. Runs under way end or are stopped.
3. Each issue left in `pr-open` or `stuck`, and any whose feedback round started meanwhile, is handed over by hand: its `factory:` label is taken off, which ends its run in Foundry and removes its workspace.
4. When the status shows no run and no workspace still to be removed, the switch goes off and the repo goes back into t3fac's watched projects.
5. The issues from step 3 are set `factory:ready` for t3fac to take up.

The repo's gate was written for `trilogy-mac`: it reads its toolchain from `~/.config/t3fac/env`, which no workspace has, and its comment names Node 24 where CI pins Node 26. Its commands do not change. At cutover its comment is rewritten to say what a host must supply: PHP 8.5 with `pdo_sqlite`, Composer and Node 26. Moving the suite to MySQL is not part of this spec.

Two faults found on 2026-11-25 are the repo's own and are fixed there before cutover, whatever the host:

- `source/api-schema.sh --check` reads `git status` inside a test for empty output, so when git refuses the checkout the script says the schema is up to date. It must fail when git fails.
- 180 of 18,393 backend tests fail on SQLite 3.46.1 at commit `d6bf734e`, in seeder and demo tests, with or without the template's `.env`, the container's variables or parallel workers. They are compared with the Mac at the same commit, and the factory switch stays off while the suite is red.

## To check before the plan is written

Each check that fails stops the plan, and this spec is revised. All but 9, 10 and 13 make a workspace and spend a few cents of credit.

| # | Assumption | Why it matters |
|---|---|---|
| 1 | `vims new` takes `--idempotency-key`. A second `new` with the same key makes nothing and answers with the first one's operation, also when that operation failed. `operation list` with the key returns the workspace's id, also from another machine signed in as the same user. What `operation list` answers for a key that was never sent, and for one Vims has forgotten, and how long it keeps one. A key answered with a refusal keeps that answer after a later attempt succeeded. `vims show <workspace> --json` gives the digest of the build a workspace was made from, the same digest as the source key it was made with, also after the template has moved to another build, and for a build and a gate template that rest on one shared base it gives two different digests. A `vims new` of a recorded `golden:<digest>` still makes a workspace after its template has moved, also for a gate key that no workspace or checkpoint rests on; if it does not, this spec is revised so that a gate's key is read when the gate is made. The check fails if any of this cannot be relied on for the length of a run and its clean-up | V8, "Keys", and every takeover |
| 2 | Each of `new`, `exec`, `pause`, `start`, `stop`, `remove`, `operation cancel`, `workspace turn-grant put` and `rm`, `session start`, `session send` and `session stop` takes an idempotency key and gives an outcome or an operation id within 30 seconds. After an answer is thrown away, a second machine signed in as the same user finds the outcome by the key, and a repeat with the same key makes nothing new. Both hold for as long as check 1 requires a key to be kept, not only for a repeat sent at once. The check fails for any call where one of these does not hold or holds for a shorter time. A lookup that gives no clear answer never allows a repeat. `vims exec --no-wait --timeout --json` returns an operation whose `show` has the exit code, and whose `logs` has the output. What `show` gives for a command that ran past its limit. `exec` passes standard input to the command | V8, "How a run maps" and "Messages". No pass waits. The body is never an argument |
| 3 | `vims session read --all --json` gives the whole session from any signed-in machine, with turns that can be told apart and keep their order, each turn's whole final message with the result line last, and a waiting question. `session list --json` tells working, turn ended, waiting for a person and ended apart | "How a run maps" (`turn`) and "Session state". The run acts on turns |
| 4 | A session in a fresh workspace, with Tom's profile as it is (its hooks and plugins included), starts in the checkout and runs with no permission prompt. A sent line starting `/factory-build` loads the skill from the profile and reads a file by path outside the checkout. A send whose answer is lost is found by its key, also while Vims holds it for a turn under way, and a repeat with the same key is not delivered twice | "Git in a workspace", "Messages" and "Sends". Every launch and every retry |
| 5 | In a workspace of the gate template made with `--egress closed`, a read-only grant, with the connection id `vims workspace turn-grant put --read-only` needs found by a command, lets `vims exec` fetch the pull request's head, though the help speaks of a grant for a turn. Once the grant is removed, `composer install` and `npm ci` complete from the package caches, with `vendor` and `node_modules` empty and a changed lockfile, so that both really fetch and a native module is compiled. From inside the workspace then, no credential helper, file, variable or profile entry gives a token that GitHub accepts, and a command run there can read no live secret of Tom's and use none of his connections or MCP servers | V6 |
| 6 | In an open workspace, after Foundry's fetch, branch and `provision` have run through `vims exec`, the session's user can read the body file, commit, and rewrite `vendor` and `node_modules`. `git push` of a new `fac/...` branch works, `gh pr create --draft` works, and `gh api user --jq .login` answers `tomkaczocha`, with the "Re-run and cancel CI" allowance off as it is today | V4. An installation token would fail the login check and stop every session |
| 7 | A workspace put to sleep with `vims pause` is woken by `vims start`, with the session's context kept and MySQL up, and a `session send` then starts a turn. The same after its node stopped it while paused, which the help says a node may do. Whether `vims exec` wakes a sleeping workspace. If it does not, a body is written only after the `vims start` has been taken. The same with another workspace filling the quota: the start is refused with a reason Foundry can read | V12, and a gate failure sent back |
| 8 | The repo's `provision` and `gate` pass in a workspace of the gate template made with `--egress closed`, after the grant is removed, run as Foundry runs them from the default branch's config on a real pull request's head commit, with no app service started, no system package or container image pulled, and the repo's own dependencies fetched from Vims's package caches only: PHP 8.5 with `pdo_sqlite` and `pcntl`, Composer and Node 26 on the login path with no `~/.config/t3fac/env`; `git status` works for the user the commands run as; `api-schema.sh --check` fails when a generated file is changed; `npm run build` finishes inside 8 GiB, with its peak memory noted; the parallel backend suite has no failure; and every command is under 30 minutes (Vitest took 708 s and the parallel suite 797 s in the trial). `provision` also passes in the build template without disturbing its running services | V6, V13 and V14. Otherwise every gate fails, or passes blind |
| 9 | The coordinator's token can create and renew `refs/factory/lease` on this repo, and no ruleset blocks it. P2 checked this on another repo only | "Before it starts". The lease |
| 10 | `vims --token-ref env:NAME` works for a service with no keychain, and the token it names lasts long enough or can be renewed without a person | "Settings and sync" (`foundryd secret set vims`). The daemon's sign-in |
| 11 | `vims remove --in-use` removes a workspace whose session was stopped. A workspace stopped with `vims stop` keeps its files and runs nothing. After `session stop`, a send that Vims accepted earlier starts no turn. Nothing starts a stopped workspace again unless a person or Foundry asks for it | Clean-up, V7 and Stop |
| 12 | A workspace paused with `vims pause` does not count against the quota `vims quota show` reports, and neither does one stopped with `vims stop` | V12. Otherwise two builds at the gate block each other |
| 13 | Foundry leaves alone an issue in `triage` or `building` that has no claim comment at all, as t3fac's newest claims have none | A second guard behind the drain of cutover step 1 |
| 14 | Several templates resting on one declared base count as one base against the organisation's limit, and take a rebuild of that base. A template's start script picks the Node and PHP that `vims exec`'s login environment then sees. `vims templates show` lets Foundry see that a project's two templates rest on the same build of that base | V13, V14 |

## What the checks have found so far

Run on 2026-11-25 in one throwaway workspace of the `tc-supplier-portal-v2` template. Checks 5, 6, 9, 10, 13 and 14 are not run.

| Check | Result |
|---|---|
| 1 | Passes in part. A second `vims new` with the same key answers with the first operation and makes nothing. `operation list` finds it by key, and gives an empty list for a key never sent. A key used again with a different request is refused with `IDEMPOTENCY_KEY_REUSED`. An operation of a `new` does not say what template it was made from, which the check first asked for. `vims show --json` does: the trial workspace's `versions.base.variantDigest` is `sha256:<digest>` for the same digest as its template's `golden:<digest>` source key. That template does not rest on a shared base. Not tried: a second machine, a failed create, a template moved after the create, a create from a build its template has left, and how long a key is kept |
| 2 | Passes for `new` and `exec`. `exec --no-wait` gives an operation whose result has the exit code and the end of the output, and `operation logs` has the output. A command past its limit ends as a failed operation with the error `HOST_COMMAND_TIMEOUT`, with no exit code, which the worker reads as P2's timed-out command. `exec` passes standard input. Commands run as root. Not tried: the other ten calls |
| 3, 4 | **Fail.** A session started with `vims session start` runs its start hooks and then stays at `working` with no tokens used. A message sent to it is held and never delivered, also after `session stop` and a second send. Two sessions, on two models, did the same. Inside the workspace Claude Code runs headless and waits for input. A send makes no operation, so `operation list` finds nothing for its key. The cause is not known and is Vims's to find. No build can run until a session takes a turn, and no plan is written while these two fail. The Claude Code process in the workspace ran as root |
| 8 | **Fails** as the template stands: the host has no PHP or Composer and has Node 24. Run inside the app's `api` container instead, 7 of 11 commands passed and 4 failed, which is what V13 and V14 answer |
| 7, 11 | Pass in part. `vims start` of a workspace put to sleep with `vims pause` answered in under two seconds, with its containers and files as they were. `vims stop` left it `Stopped`, and a `vims exec` then was refused with `PRECONDITION_FAILED`, so `exec` does not wake a stopped workspace. `vims remove` of the stopped workspace removed it. Not tried: a session's context after a wake, since no session takes a turn; a node that stops a paused workspace; a start refused for quota; `remove --in-use` |
| 12 | **Open, and it puts the quota read in doubt.** `vims quota show --json` gave `actual` and `reserved` of 0 vCPU both while the 4 vCPU trial workspace ran and after `vims pause`. So the read did not count a running workspace, and it cannot yet be what "Claiming" tests for room. Added after the council's last round; no advisor has seen this row |
