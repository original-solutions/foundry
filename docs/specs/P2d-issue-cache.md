# P2d: Coordinator issue snapshot

Product: Foundry, a dark software factory that also runs manual sessions.

Status: draft 2026-11-28, second version. The first version (#405) cached the label listings and added a webhook relay, a peer cache route and incremental refresh. A council review ([#405 comment](https://github.com/original-solutions/foundry/pull/405#issuecomment-7239215311)) measured the spend and found that listings are the smaller part. This version replaces it. Nothing here is built. The plan comes after this is approved, and the work is then built as factory issues from that plan.

## Why

On 2026-11-28 at 9:18:41 PM Brisbane (AEST), the factory on the personal Mac, acting as coordinator, used up GitHub's GraphQL limit (5,000 points an hour) for the `tomkaczocha` account. Every factory pass failed until the window reset at 9:33:11 PM, and the new worker `foundry-w2` got no jobs. `~/Library/Logs/foundry/foundryd.log` has 89 rate-limit warnings in those 15 minutes, across `original-solutions/people-hub`, `original-solutions/foundry` and `Trilogy-Care/tc-supplier-portal-v2`. The same thing had happened at 8:29 AM that morning.

The first failure was a listing:

```
2026-11-28T11:18:41.222813Z  WARN foundryd::factory::pass: factory: the pass ended: `gh issue list --repo original-solutions/foundry --state open --label factory:building --limit 201 --json number,title,assignees,labels` hit GitHub's rate limit: GraphQL: API rate limit exceeded for user ID 330738380.
```

A listing failed first because the pass lists before it does anything else. The points went on a wider set of reads. These numbers were measured after the reset, with the same three projects:

- The window that opened at 11:33:16Z had spent 3,165 points by 12:02Z, with no warning logged. That is about 6,500 an hour against a limit of 5,000. The limit ran out again before 12:25Z.
- The open `factory:*` issues were 44 ready, 10 building, 1 pr-open, 10 stuck and 9 needs-info.
- A `gh issue list` for one label costs 2 points. The pass lists `triage`, `building` and `ready` for each project, and `stuck` and `needs-info` when a finished run waits on one, every `poll_seconds` (default 30). That is about 2,200 to 3,600 points an hour.
- Whenever a run slot is free, [`claims`](../../crates/foundryd/src/factory/pass.rs) reads blockers for every ready issue on every pass. That is REST `dependencies/blocked_by`, then `gh issue view` (GraphQL) for the body when there is no linked blocker, then REST for each issue the body names. An issue held back by a blocker is read again 30 seconds later. With 44 ready issues that is up to 5,280 GraphQL calls an hour.
- On every pass, each building run can make up to four GraphQL reads:
  - [`stop::building`](../../crates/foundryd/src/factory/runs/stop.rs) runs `text_stop`, which runs the approval timeline query (1 point), and then [`closed_issue`](../../crates/foundryd/src/factory/runs/mod.rs), which runs `gh issue view`.
  - [`feedback::forward`](../../crates/foundryd/src/factory/runs/feedback.rs) runs `closed_issue` again in the same pass, then the GraphQL half of `activity_since`.
  - Each pr-open run reads its pull request and its issue on every pass.

  Guards skip some of these reads: a running gate, a recorded pass, a missing session, a pending relaunch.
- Every machine runs `gh release list` every ten minutes ([`CHECK_EVERY`](../../crates/foundryd/src/update.rs)). Seven machines on one login spend about 42 points an hour on this. That is small, and this spec leaves it alone.
- At the same moment, REST `/rate_limit` reported 67 GraphQL points used and GraphQL's own `rateLimit` field reported 3,165. For GraphQL, only the `rateLimit` field can be trusted.

After a limit error, the pass makes the same calls again every `poll_seconds`. Those calls fail and cost nothing, but they filled the log and gave no sign of when the limit would reset.

## Goal

The factory's own GitHub reads stay well inside the limit, and a spent limit stops calls until it resets.

- The acting coordinator keeps an in-memory snapshot of each project's open `factory:*` issues. It refreshes the snapshot with one search a minute, and passes read the snapshot instead of listing per label.
- A per-issue read repeats only when that issue has changed, or when its own slow timer comes due.
- When GraphQL is spent for a login, that login makes no GraphQL calls until `resetAt`, and this is logged once.

Exit criterion: in a simulated hour at the shape measured above, foundryd's GraphQL use is under 500 points. That covers the coordinator's passes plus the update checks of seven machines. A counter at the `gh` seam then confirms the bound on the live coordinator.

## Scope

In:

- A per-project snapshot on the acting coordinator, refreshed by one GraphQL search when it is a minute old.
- Ready-issue blocker reads, building-run reads and pr-open-run reads gated on the issue's `updatedAt` in the snapshot, each with its own timer for what `updatedAt` cannot show.
- `postponed` and blockers checked on the live fetch at claim.
- A pause per login at the `gh` seam when a budget is spent.
- A counter at the `gh` seam, logged once an hour.
- A whole-workload budget test.

Out:

- A webhook relay, or any public endpoint.
- Serving the snapshot to peers. No peer lists issues today, and no new route or protocol number is needed.
- Incremental `updated-since` refresh, ETags, and a file-backed snapshot.
- Stretching the refresh interval as the budget runs low.
- A `cache_seconds` setting. The interval is a constant.
- New Factory view or Settings UI.
- Changing the update check.
- Changing the factory lifecycle, label names, claim comments, the lease, or `poll_seconds`.
- Spreading projects across more than one GitHub account.

## Decisions

| # | Topic | Decision |
|---|-------|----------|
| D1 | Where the snapshot lives | In the project's pass [`Memo`](../../crates/foundryd/src/factory/pass.rs), in memory, on the acting coordinator only. It is dropped when this machine stops acting. A new coordinator starts with none and refreshes on its first pass |
| D2 | Refresh | A pass refreshes the snapshot when it is 60 seconds old or marked stale. With `poll_seconds` at 30, every other pass refreshes. There is no separate task |
| D3 | Query | One GraphQL `search` per project: `repo:OWNER/NAME is:issue is:open label:factory:ready,factory:triage,factory:needs-info,factory:building,factory:pr-open,factory:stuck,factory:proposed`. The comma is GitHub's OR. Fields per issue: number, title, body, author, labels, assignees, `updatedAt`. The same document asks for `rateLimit { cost remaining limit resetAt }` |
| D4 | Claims | A claim never trusts the snapshot. [`start::claim`](../../crates/foundryd/src/factory/runs/start.rs) already fetches the issue live. That fetch now also refuses a `postponed` issue, and the blocker check runs live on the issue about to be claimed |
| D5 | Writes | A successful label write by the pass marks that project's snapshot stale, so the next pass refreshes it. Comments do not. There is no optimistic write-through |
| D6 | Pause | Per login, at the `gh` seam, per budget. A spent GraphQL budget refuses GraphQL commands, and a spent REST budget refuses REST commands, until reset |
| D7 | Quota source | GraphQL: the `rateLimit` field on the snapshot query, and the rate-limit error [`gh.rs`](../../crates/foundryd/src/factory/github/gh.rs) already recognises. Never REST `/rate_limit` |

## How it works

### The snapshot

Each project's snapshot holds its open issues that carry any `factory:*` label, keyed by number, plus the time it was read. A pass gets `ready`, `triage`, `building`, `stuck` and `needs-info` by filtering the snapshot, in place of [`list_issues`](../../crates/foundryd/src/factory/github/read.rs). An issue with two `factory:*` labels shows up under both, as the per-label listings would show it.

The search reads pages of 100. Each page costs points, and every page belongs to the same refresh. The snapshot holds at most [`LIST_LIMIT`](../../crates/foundryd/src/factory/github/read.rs) issues per state, and reports a cut per state the way `Listing.cut` does today. The plan measures `rateLimit.cost` of the query with `body` included before the test locks a number. If `body` makes the search expensive, or the search index lags too far behind, one document with one alias per label is the fallback. That is still one request per project.

If a refresh fails, the pass keeps the last snapshot and ends as it does today. A pass starts no claim from a snapshot older than two minutes.

### Per-issue reads

The rule is that a read about one issue repeats only when that issue's `updatedAt` in the snapshot has moved since the last read. A label change, a body edit and a new comment all move `updatedAt`. The pass keeps each issue's last answer and the `updatedAt` it was read at in the `Memo`.

- **Text blockers.** `text_blockers` parses the snapshot's `body`. The steady path runs no `gh issue view`.
- **Linked blockers and named issues.** A blocker can close without moving the waiting issue's `updatedAt`. A ready issue known to be blocked is therefore rechecked every five minutes, and on its `updatedAt` moving. These reads are REST.
- **Closed or relabelled.** An issue that is in the snapshot is open and still carries its `factory:*` label. `closed_issue` answers from the snapshot. Only an issue that has left the snapshot is fetched live, to tell a close from a relabel. The two `closed_issue` reads per building run per pass become none.
- **Building runs.** `text_stop`'s approval timeline read and the GraphQL half of `activity_since` run when `updatedAt` moves. As a backstop they also run every 30 minutes, so a missed change cannot last a whole build.
- **pr-open runs.** The issue's state comes from the snapshot. A pull request can merge or close without moving the issue's `updatedAt`, so the pull request is read every five minutes.
- **Claims found on unheld issues.** `found_claim`'s comment read follows the same rule.
- **Approval.** `approval` and `claimed_approval` stay live at claim, launch, takeover and resume. These happen once per run step, not on every pass.

### Pause

The `gh` seam of each login knows each budget's state. A GraphQL command is `gh issue …`, `gh pr …`, `gh release …` or `gh api graphql`. A REST command is any other `gh api <path>`.

- The snapshot query's `rateLimit` gives remaining points and `resetAt`. A rate-limit error on a GraphQL command means GraphQL has nothing left. A rate-limit error on a REST command means REST has nothing left.
- While a budget is spent, the seam refuses that budget's commands for that login without calling GitHub, and returns the existing rate-limit error. The pass ends at the refusal, as it does today on a rate-limit error. Only the wasted calls and the repeated warnings stop.
- When `resetAt` is known, the pause lasts until then. When it is not known, for example after a restart or an error with no reset time, the pause lasts five minutes, and one call then probes.
- The log gets one line when a pause starts, naming the login, the budget and the reset time, and one line when it ends. Ten passes inside a pause write no more lines.
- The reason the pass ended already reaches `foundryd status` and the Factory view. It now reads, for example, "GitHub's GraphQL limit for tomkaczocha is spent until 9:33 PM". No record gains a field.
- Another login's projects carry on.

### Counter

Every `gh` call through the seam is counted by login, by budget and by command (`issue list`, `issue view`, `api graphql`, `api repos/…/blocked_by`, and so on). GraphQL `cost` is added when the document returns it. Once an hour the coordinator logs one line per login with those counts and the points spent, taken from `rateLimit.remaining` at the start and end of the hour. The line holds no token and no issue text.

### Budget at steady state

| Spend | Points an hour |
|-------|---------------:|
| Snapshot: 3 projects, 60 refreshes, 2 points a page | 360 |
| Update checks: 7 machines, 6 lists | 42 |
| pr-open pull request read every 5 minutes, one run | 12 |
| Building-run backstop every 30 minutes, 10 runs, 2 reads | 40 |
| Change-driven reads and claims | the rest, under 46 |

Blocker rechecks are REST: about 530 an hour for 44 ready issues, against a separate REST limit of 5,000.

## Non-goals

- Anything that needs production-2, DNS, TLS or a GitHub webhook.
- Changing which machine acts as coordinator, or the lease.
- Worker sessions' own `gh` use. If they share the `tomkaczocha` login, they spend from the same hour. Checking that is a follow-up, and it does not change this design.
- Auto-merging, review loops, or anything in P2b.

## Rollout

1. The counter ships first and is logged on the live coordinator. Its numbers check the arithmetic above before the rest lands.
2. The snapshot, the gated reads, the live claim checks and the pause ship in one release. The pass stops calling `list_issues` for these listings. There is no feature flag, and the per-label path is removed from the pass.
3. An hour of the counter's log on the live coordinator under 500 points meets the exit criterion.

To go back, revert the release. That brings back the per-label listings and the outage risk, so fix forward unless a claim goes wrong.

Until this ships, raising `poll_seconds` to 60 roughly halves the spend.

## Risks

- A stale snapshot can offer an issue that is no longer ready or is now postponed. The live fetch at claim is the guard, and tests prove it refuses.
- GitHub's search index can lag a label change by seconds. A newly ready issue may wait one more refresh. A removed label is caught by the live fetch at claim.
- `updatedAt` might not move for some change the factory cares about. The five- and thirty-minute timers bound how long that can last.
- The search has a secondary rate limit of its own. Three searches a minute is far below it.

## Open questions

- Whether a pass should carry on past a refused read to do ledger-only work. Today it ends there, and this spec keeps that.
- The measured cost of the search with `body`, against the alias document. The plan picks the cheaper one, and the test locks it.

## Testing

Tests come first, per `AGENTS.md`. No test calls GitHub. The fake `Tracker` records every `gh` call, answers the snapshot document with a `rateLimit` the test sets, and can answer with a rate-limit error. No token appears in a log line, an error or a status in any test.

### 1. Snapshot

- A pass on a project with no snapshot sends one search document, and the pass reads `ready`, `triage` and `building` from it without calling `list_issues`.
- The document's query names every `factory:*` label with commas, asks for body and `updatedAt`, and asks for `rateLimit`.
- An issue with `factory:ready` lands in ready, one with `factory:triage` lands in triage, and one with both lands in both.
- On a test clock with `poll_seconds` at 30, two passes 30 seconds apart send one search, and a third at 60 seconds sends a second one.
- A successful label write marks the snapshot stale, and the next pass searches. A comment does not mark it stale. A refused write leaves the snapshot as it was.
- A search with two pages counts as one refresh. Passing `LIST_LIMIT` in a state sets that state's cut.
- A failed refresh keeps the last snapshot. When that snapshot is more than two minutes old, the pass starts no claim.
- A machine that stops acting drops its snapshot. When it acts again, its first pass searches.

### 2. Per-issue reads

- A ready issue whose body names an open blocker makes no `gh issue view`. Ten passes on an unchanged `updatedAt` make no blocker read. The read comes back after five minutes, or as soon as `updatedAt` moves.
- A building run in the snapshot makes no `closed_issue` read in `stop::building` or in `feedback::forward`. A building run whose issue has left the snapshot makes one live `issue` read, and a closed issue then abandons the run as today.
- `text_stop`'s approval read and `activity_since`'s GraphQL read run once when `updatedAt` moves. They do not run on ten passes with no change. They run again after 30 minutes with no change.
- A pr-open run reads its pull request once in five minutes, not on every pass. A merged pull request still ends the run on the next read.
- `found_claim` does not read comments again for an issue whose `updatedAt` has not moved.

### 3. Claims

- `start::claim` fetches the issue live once. A live answer of `factory:ready`, open, not postponed and unblocked claims as today.
- A live answer of `factory:building`, closed or `postponed` takes no claim, even though the snapshot said ready.
- A live blocker check that finds an open blocker takes no claim.

### 4. Pause

- A snapshot answer with `rateLimit.remaining` at 0 pauses GraphQL for that login until its `resetAt`. Ten passes inside the window send no GraphQL command, refuse at the seam, and write one pause line. At `resetAt`, the next pass searches and one resume line is written.
- A `gh` rate-limit error on a GraphQL command, with no known reset, pauses GraphQL for five minutes and then lets one call through.
- A spent GraphQL budget still lets REST commands through, and a spent REST budget still lets GraphQL through.
- A second login's projects keep refreshing while the first login is paused.
- The pass status during a pause reads as spent until the reset time, and holds no token.

### 5. Counter

- Calls are counted by login, budget and command, and GraphQL cost is summed from the fake's `rateLimit`.
- One line per login is logged per hour on the test clock, with no token and no issue text.

### 6. Budget

- A simulated hour with three projects, 44 ready issues (some blocked by text, some by link), a free slot, 10 building runs, 1 pr-open run, `poll_seconds` at 30, and each document at its measured cost. The test sums every GraphQL point the pass spends, adds 42 for the update checks, and passes only under 500. It fails if the steady path calls `list_issues` or makes a `gh issue view` for a blocker.

## Acceptance criteria

1. The simulated hour in §6 is under 500 GraphQL points, and so is an hour of the counter's log on the live coordinator.
2. A pass never lists issues per label.
3. A newly ready issue can be claimed within one refresh, plus the search index's lag.
4. A claim is never taken on an issue whose live fetch is not ready, is closed, is postponed or is blocked.
5. While a login's GraphQL budget is spent, that login sends no GraphQL command until reset. The log has one pause line and one resume line, and `foundryd status` names the reset time.

## Cut for the plan

| # | Slice | Depends on |
|---|-------|------------|
| 1 | Counter at the `gh` seam and the hourly line. Tests of §5 | none |
| 2 | Snapshot query, `Memo` snapshot, pass listings from the snapshot, staleness on write. Tests of §1 | 1 |
| 3 | Pause at the seam, per login and per budget. Tests of §4 | 1 |
| 4 | Per-issue reads gated on `updatedAt`, with their timers. Tests of §2 | 2 |
| 5 | Live `postponed` and blocker checks at claim. Tests of §3 | 2 |
| 6 | Budget test of §6, and docs | 2 to 5 |

## Changes to other documents

When this is built, not in this pull request:

- `AGENTS.md` points at this spec and its plan, the way it points at P2 and P3.
- `docs/guide/factory.md` says that passes read a one-minute snapshot, and describes the pause.
- `docs/specs/P2-factory.md` says `poll_seconds` is the pass cadence and that listings follow the snapshot.
