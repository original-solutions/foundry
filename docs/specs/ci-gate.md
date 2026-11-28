# CI as the gate

Product: Foundry. A change to P2's gate.

Status: approved 2026-11-28. Drafted from a chat about slow `factory:pr-open` labels and revised over four council rounds (Claude, Gemini, Grok, Sol). Three seats said ready. Sol's last points are in: a base change restarts the wait, and the deadline holds when GitHub cannot be read. Tom accepted the one hole C6 names. Nothing here is built.

## Why

A factory pull request is gated three times today. The build session runs `factory.gate` in its worktree before it reports. The factory then runs `provision` and `gate` again in a gate workspace on a gate machine. CI runs on every push to the draft as well.

The factory's gate is the slow one, and the one most likely to fail for reasons that have nothing to do with the change:

- It takes 14 minutes on foundry and 17 to 32 minutes on people-hub. CI on foundry takes 3 minutes. It runs the gate's commands and also the release-cfg tests and the browser test, but only on Linux. The factory's gate runs on macOS, and foundry has macOS-only code and tests.
- The pull request stays a draft, and the issue stays `factory:building`, for the whole gate. The UI shows the session as finished, so the run looks stuck.
- Of the last five gates on record on 2026-11-28, one passed. Three of the four failures were the gate machine's own setup: `make provision`, its MySQL, and one timeout. A failure goes back to a session that cannot fix the machine, so the run ends stuck and the draft stays a draft.
- Each gate holds a gate slot (`max_gates`, 2 by default), so one slow gate holds up the next run.

Foundry already reads CI. With `required_checks` set, a run waits for the named checks on the commit it gated after the local gate passes. Foundry does not set it, so CI never gates a foundry run, and on a repo that does set it the two run one after the other.

## Goal

A repo whose CI covers its gate can say so. The factory then gates a pull request on its CI checks alone, with no gate workspace, whenever CI cannot have been changed by the pull request.

Exit criterion: on foundry, over 10 runs after this ships, every run gated in CI is marked ready and labelled `factory:pr-open` within 2 passes of its required checks all passing, and none of them creates a gate workspace.

## Scope

In:

- A `gate_in_ci` key in `.dw/config.yml` that makes the required checks the gate.
- The factory skips the gate workspace for a pull request that CI cannot have been changed by, and runs the local gate as today for any other.
- A macOS job in foundry's `check.yml`, then foundry's own `.dw/config.yml` turning the key on.

Out:

- Any change to the build session or the `factory-build` skill. The session reports after it pushes, as today, and the factory alone waits for CI. A session that waits for CI itself may come later, once runs show red CI checks spending retries. It would need the same REST read the factory uses, one time budget shared with the factory's, and the skill's source back under `~/EA/skills/factory-build`.
- Trimming the build session's own local gate run.
- Turning the key on for people-hub or any other repo. Each repo does that in its own config change.
- Judging the issue's acceptance criteria in the factory. The session's Check step owns that, as today.
- What happens when someone merges a pull request by hand before the gate has passed. A run already ends when its pull request is merged.
- The send-back stall seen on PR #400 on 2026-11-28. A red check goes back through the same path, so that stall needs its own fix.

## Decisions

| # | Question | Decision |
|---|---|---|
| C1 | How a repo opts in | A new key, `gate_in_ci: true`. `required_checks` keeps its meaning. Reading a set `required_checks` as "CI is the gate" was rejected, because it would quietly drop the local gate on a repo that sets both today |
| C2 | Which pull requests need the local gate | One that changes anything under `.github/`, or whose base is not the default branch. CI runs the workflow files from the pull request's own commit, while the local gate runs the default branch's commands, so a session could otherwise edit a workflow until a check passes without running anything. A base other than the default branch runs that base's workflows, and the changed-file list is against that base. Tests and build scripts can be edited under either gate, so they add no new risk |
| C3 | The guarded path | Fixed at `.github/`, not configurable. A list would need its own validation so that a bad entry could never leave workflows unguarded, and no repo has asked for one |
| C4 | `gate` with `gate_in_ci` | Still required and non-empty. It is what runs when C2 applies |
| C5 | Which check runs count on a CI-gated attempt | Only runs from the GitHub Actions app (`app.slug` is `github-actions`) with the conclusion `success`. A build session can hold a GitHub App token (`factory-build`, "GitHub account"), and a check run that token creates carries that App's name. On the CI path such a run would be the whole gate. The local-gate path, and every repo without `gate_in_ci`, read checks exactly as today |
| C6 | Binding to one workflow, event or merge commit | Not needed. Required check names must be unique across the workflows on the default branch. With C2 and C5, a run that matches can then only come from the default branch's own workflow. Foundry meets this: `release.yml` runs on tags only, and none of its job names is a `check.yml` job name. One hole is accepted, on Tom's decision of 2026-11-28. A session that pushes the same commit, opens a second pull request into a branch with a gutted `check.yml`, and gets green runs there, has those runs land on the same commit. That takes a deliberately dishonest session, which could as easily delete tests under the local gate. The guard stops a session taking a shortcut, not a hostile one |
| C7 | Who waits for CI | The factory alone, with its existing REST read of the head commit's check runs. A wait in the session as well would stack two `checks_minutes` budgets, count against `limits.minutes`, hold a `max_runs` slot, and add GitHub calls to an account that ran out of GraphQL quota on 2026-11-28 |
| C8 | Acceptance criteria | Not a factory check. The factory cannot judge them, and the skill already makes the session check them before it ships |
| C9 | macOS on foundry | A macOS job in `check.yml`, as a required check, before foundry sets the key. The repo is public and `release.yml` already runs on `macos-15`. Building the key does not wait on the job. Turning it on for foundry does |

## How it works

### Config

```yaml
factory:
  gate: [...]                # unchanged, still required
  required_checks: [lint, "test (1)", web]
  gate_in_ci: true
```

- `gate_in_ci` is optional and defaults to false. Without it nothing changes.
- With it, `required_checks` must be non-empty, or the file does not load. A file that does not load leaves the project idle with an attention item, as today.
- No two workflows on the default branch may produce a check with the same name as a required check (C6). The factory does not check this. It is a rule for whoever edits the workflows.

The coordinator reads `gate_in_ci` from the default branch's `.dw/config.yml`, like every other key, so a pull request cannot turn it on.

### Choosing the gate

When a run reaches the gate, a project with `gate_in_ci` decides for each attempt, before it places anything, whether the attempt is gated in CI or locally:

1. It reads the pull request's head commit and base branch.
2. A base other than the default branch: gated locally.
3. Otherwise it reads the files the pull request changes, all pages. For a renamed file it reads both the old and the new path.
4. It reads the head and the base again. If either changed, it records nothing, and the next pass starts again at step 1.
5. Any path under `.github/`, or a list of 3,000 files or more (where GitHub stops listing): gated locally.
6. Otherwise: gated in CI, on the head commit read in step 1.

A read that fails records nothing and is asked again next pass. An attempt gated locally runs exactly as today, required checks included.

### An attempt gated in CI

The run records that the attempt is waiting for CI on that commit, and when the wait began. It places no gate workspace and takes no gate slot. Waiting is not a pass: nothing records a gate pass until the checks have all passed. The time spent waiting stays out of `limits.minutes`, as the time a gate runs does today.

On each pass while it waits, the run reads the pull request and the check runs on the recorded commit. It asks GitHub for the Actions app's runs only (the check-runs read's `app_id` filter), so another app's later run of the same name cannot hide an Actions run. It then takes the first of these that holds, in this order:

- The pull request was merged or closed: the run ends, as a run does when its pull request ends today. Nothing is recorded as a pass.
- The head or the base moved: the next attempt is chosen again from step 1 of "Choosing the gate". A pull request moved off the run's base goes through the existing check that it is still the run's pull request. This comes before a failed check, because `check.yml` cancels an old head's runs on every push, and a cancelled run must not spend a retry.
- GitHub says the pull request conflicts with its base: a failed attempt at once, sent back with the conflict message, as the local gate's conflict check does today. An unknown `mergeable` state is not a conflict and holds nothing back.
- A required check completed as anything but `success` (`failure`, `cancelled`, `timed_out`, `neutral`, any other): the failure goes back to the session as a check failure, as it does today after a local gate, and spends one of `gate_retries`. With none left, the run is stuck.
- A required check is pending or missing: the run keeps waiting, up to `limits.checks_minutes` from the start of the wait, then is stuck (`checks-timed-out`), as today. A skipped run counts as missing. So a check that never runs, and an Actions outage, end the run stuck, never ready.
- Every required check passed: the attempt passes.

A pass whose reads fail decides nothing and is asked again next pass, and the `checks_minutes` deadline keeps running from the start of the wait. A wait that GitHub never answers ends stuck at the deadline, with the stuck comment naming what could not be read.

When every required check passed, the run records the pass and the check results that decided it, then finishes the pass as today. The pull request is marked ready, the build session is ended, the results are posted and the label moves to `factory:pr-open`.

The results comment lists each required check with its result and a link to its run, from the read that decided the pass. A run that resumes after a restart posts the same comment. It does not say the factory ran a fresh checkout, because it did not.

`retro` counts the attempt as a gate pass or failure like any other.

Feedback rounds after `pr-open` gate their new head the same way.

### Renaming a required check

A pull request that renames or removes a required check changes `.github/`, so it is gated locally. After the local gate it still waits for the names in the default branch's `required_checks`, which it no longer produces, so it ends `checks-timed-out`. A change like that is merged by hand, with the workflow and the config change together, or keeps the old name until the config has moved.

### Foundry's own config

Before foundry sets the key, `check.yml` gains a job named `macos-check` on `macos-15`. It runs the factory gate's Rust commands on macOS: `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo nextest run --workspace --locked` and `cargo test --workspace --doc --locked`. It must not be named `macos`, because `release.yml` already has a job of that name (C6). That change touches `.github/`, so it is gated locally, and it lands before the config change below.

Every machine then runs a `foundryd` release that has `gate_in_ci`. A machine on an older release ignores the key, gates locally and then waits for the checks, which is safe but slow, and would make the exit criterion fail for a reason that has nothing to do with this change.

Foundry's `.dw/config.yml` then sets `gate_in_ci: true`, and lists every job in `check.yml` as a required check: `lint`, `test (1)` to `test (8)`, `release-cfg (1)` to `release-cfg (4)`, `e2e`, `web` and `macos-check`. A person makes this change on `main`, since a factory session may not change `.dw/config.yml`.

The first factory runs after the change are the canary. One of them should have a red check, to show the failure reaches the session once and the retry passes, given the send-back stall seen on PR #400. The change is undone by removing the key from the config.

## Changes to other documents

- `docs/specs/P2-factory.md`, "Repo config": add `gate_in_ci`. "A run", step 5: an attempt can be gated in CI, as above.

## Testing

Unit tests in `factory::config`:

- `gate_in_ci` absent, or false, loads as today. True with an empty `required_checks` does not load.

Gating tests, through the fake tracker:

- A pull request on the default branch that changes nothing under `.github/` places no gate workspace and takes no gate slot. With all checks passed, it is marked ready and moved to `pr-open` within 2 passes.
- A pull request that changes `.github/workflows/check.yml` is gated locally, then waits for checks, as today.
- A rename out of `.github/` is gated locally.
- A pull request whose base is not the default branch is gated locally.
- A head that moves between the two head reads records nothing, and the next pass decides again.
- A changed-files read that fails places nothing and records nothing, and the next pass asks again.
- A list of 3,000 files is gated locally.
- On a CI-gated attempt, a `success` run from another app does not pass a check, and a `failure` run from another app does not fail it. A `neutral` Actions run does not pass. A repo without `gate_in_ci` still counts any app's `success` and `neutral`.
- A pull request closed while it waits for CI ends the run with no pass recorded and no label moved.
- A pull request that GitHub reports as conflicting after the first pass is sent back with the conflict message on the next pass.
- A failed check is sent back once and spends a retry. With none left the run is stuck.
- A pending check past `checks_minutes` is stuck with `checks-timed-out`. A skipped check counts as missing.
- A head that moves while checks are waited for is chosen again, with its changed files read again, and a `cancelled` run on the old head spends no retry.
- A base that changes while checks are waited for is chosen again.
- A `cancelled` or `timed_out` Actions run on the recorded head is a failed check.
- An unknown `mergeable` state with every check passed still passes.
- Reads that keep failing through the wait end the run stuck at `checks_minutes`.
- The results comment lists each required check, its result and its link, and is the same when posted after a restart.

On foundry, after the key is turned on, the canary runs (see "Foundry's own config") are read from their run records: the timestamps of `FACTORY-DONE`, the checks read and the label change, one run with a red check sent back and retried, and one run whose pull request changes `.github/` gated locally.

## Acceptance criteria

- A project without `gate_in_ci` gates exactly as before. The existing gating tests pass unchanged.
- With `gate_in_ci`, a pull request on the default branch that changes nothing under `.github/` is gated without a gate workspace, and is marked ready and labelled `factory:pr-open` within 2 passes of its required checks all passing.
- Any other pull request is gated locally.
- On a CI-gated attempt, only a GitHub Actions `success` counts, and nothing is marked ready on a commit whose required checks have not all passed.
- A pull request merged or closed while it waits for CI records no pass.
- Foundry's `check.yml` has a required macOS job before foundry sets the key.
- The exit criterion above holds over 10 foundry runs.
