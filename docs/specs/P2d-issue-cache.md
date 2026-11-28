# P2d: Coordinator issue cache

Product: Foundry, a dark software factory that also runs manual sessions.

Status: draft 2026-11-28. Written the same evening as the GraphQL rate-limit outage it answers. Nothing here is built. The plan comes after this is approved. No GitHub issue was filed with this document; the work is cut in the plan, then built as factory issues from that plan.

## Why

On 2026-11-28 at 9:18:41 PM Brisbane (AEST), the factory on the personal Mac, acting as coordinator, exhausted GitHub's GraphQL limit (5,000 points per hour) for the `tomkaczocha` account. From then until the window reset at 9:33:11 PM Brisbane (AEST), every factory pass failed, and the new worker `foundry-w2` received no jobs. The log, `~/Library/Logs/foundry/foundryd.log`, recorded 89 rate-limit warnings in those 15 minutes, across the three projects the factory watches: `original-solutions/people-hub`, `original-solutions/foundry` and `Trilogy-Care/tc-supplier-portal-v2`.

The first failure was a listing, not a write:

```
2026-11-28T11:18:41.222813Z  WARN foundryd::factory::pass: factory: the pass ended: `gh issue list --repo original-solutions/foundry --state open --label factory:building --limit 201 --json number,title,assignees,labels` hit GitHub's rate limit: GraphQL: API rate limit exceeded for user ID 330738380.
```

Passes then failed the same way on the other two repos, on `factory:triage`, `factory:ready` and `gh issue view`, and kept retrying every `poll_seconds`. The update check failed too, in the same window:

```
2026-11-28T11:26:49.663328Z  WARN foundryd::update: update check failed, trying again later: gh release list failed: GraphQL: API rate limit already exceeded for user ID 330738380.
```

The last failure in the window, at 9:33:11 PM Brisbane (AEST):

```
2026-11-28T11:33:11.377187Z  WARN foundryd::factory::pass: factory: the pass ended: `gh issue list --repo original-solutions/foundry --state open --label factory:triage --limit 201 --json number,title,assignees,labels` hit GitHub's rate limit: GraphQL: API rate limit already exceeded for user ID 330738380.
```

The path is as follows.

- The acting coordinator runs [`pass::run`](../../crates/foundryd/src/factory/pass.rs) for each project whose factory is on, every `poll_seconds` (default 30, range 10 to 60), from [`pass_task`](../../crates/foundryd/src/factory/mod.rs). The three projects run apart, so three passes can be in flight at once.
- Each pass lists open issues per `factory:*` label through [`list_issues`](../../crates/foundryd/src/factory/github/read.rs). That function runs `gh issue list --repo <repo> --state open --label <factory:state> --limit 201 --json number,title,assignees,labels`. `201` is [`LIST_LIMIT`](../../crates/foundryd/src/factory/github/read.rs) plus one, so a cut listing can be detected. `gh issue list` is a GraphQL call.
- Every pass lists `factory:triage` and `factory:building` (to carry Foundry's claims and to mark another's as foreign) and `factory:ready` (to claim). It also lists `factory:stuck` and `factory:needs-info` when a finished run of that kind is still waiting on the issue. With three projects, that is commonly nine GraphQL listings every 30 seconds, plus `gh issue view`, approval-timeline GraphQL, and the other reads a pass already makes.
- The pass does not back off when GitHub answers that the limit is already exceeded. It waits `poll_seconds` and tries the same listings again, which is what filled those 15 minutes with warnings and produced no new claims.
- Separately, every daemon that runs from `<home>/current` calls `gh release list --repo original-solutions/foundry` every ten minutes ([`CHECK_EVERY`](../../crates/foundryd/src/update.rs) is 600 seconds; [`update::run`](../../crates/foundryd/src/update.rs) is spawned from [`daemon.rs`](../../crates/foundryd/src/daemon.rs) on each machine). That list is also GraphQL. A fleet of seven machines therefore spends 42 listing calls an hour on updates alone, against the same account when they share its `gh` login.

The factory already runs its passes only on the acting coordinator. The waste is the volume of those calls, the per-label listings, the update check on every machine, and the refusal to wait until `resetAt`.

This has happened before the same day: earlier warnings at 8:29 AM Brisbane (AEST) (`2026-11-27T22:29:08Z`) show the same `API rate limit already exceeded` on people-hub listings and views. The evening outage is the one that left `foundry-w2` idle.

## Goal

The acting coordinator is the only machine that reads issue lists from GitHub. It keeps a cache of the open issues that carry any `factory:*` label, refreshes that cache once a minute by default, and serves it to the rest of the fleet over the tailnet foundry API. A pass reads listings from the cache. Peers never call GitHub to list issues. Claims and writes still go to GitHub live. When the GraphQL budget is low the coordinator slows down; when it is exhausted it waits until `resetAt` and says so once. A webhook relay on production-2 tells the coordinator about a change sooner than the minute, without holding a GitHub token. The update check lists releases at most once an hour, and only the coordinator lists them.

Exit criterion: on a fleet of five VPS workers and two Macs, watching the same three projects, GraphQL use stays under 500 points per hour at steady state, proven by a test or a measurement that counts points the way GitHub counts them. A limit outage like 2026-11-28 must not recur from listings or update checks.

## Scope

In:

- A coordinator-owned issue cache, refreshed on a configurable interval whose default is 60 seconds, served to peers over the tailnet foundry API.
- One listing query per project per refresh, for all open issues that carry any `factory:*` label, with incremental `updated-since` and ETag / conditional REST requests.
- A live re-fetch of the single issue before a claim, and live writes that also update the cache.
- Tracking GitHub's remaining GraphQL quota (and REST, when used), slowing under 20%, pausing at 0 until `resetAt`, logging the pause once, and showing the quota in `foundryd status` and the UI.
- Cache ownership moving with the coordinator role, to the next machine in coordinator order when the acting machine is offline.
- A small webhook relay at `https://foundry-relay.originalsolutions.com.au` on production-2, described here and built in-tree, not deployed by this work.
- The update check listing tags at most hourly, and only on the acting coordinator.

Out:

- Deploying the relay, creating its DNS or TLS, or storing its secrets on production-2. The spec describes those; a person does them later.
- Filing a GitHub issue for this work from this pull request.
- Changing the factory lifecycle, label names, claim comments, the lease, or `poll_seconds` as the cadence of a pass over runs.
- Spreading projects across more than one GitHub account, which remains a possible later step.
- Replacing the one-minute refresh with webhooks alone. The poll stays as the backstop.
- A native `retro`, Linear, or anything in P2b.

## Decisions

| # | Topic | Decision |
|---|-------|----------|
| D1 | Who talks to GitHub for lists | Only the acting coordinator. A peer answers a cache read from the coordinator, or says the cache is unavailable; it never runs `list_issues` or an equivalent GitHub listing |
| D2 | Refresh cadence | A new fleet setting, `cache_seconds`, default 60, from 30 to 300. `poll_seconds` stays the cadence of a pass over runs and claims, default 30 |
| D3 | One query per project | One GraphQL search document per project per refresh, with every `factory:*` label as an `OR`, results sorted into labels locally. Not one `gh issue list` per label. The same document asks `rateLimit { cost remaining limit resetAt }` |
| D4 | Freshness of a claim | `start::claim` already re-fetches the issue with `tracker.issue` ([`start.rs`](../../crates/foundryd/src/factory/runs/start.rs)). That live fetch stays. A claim is never taken from the cache alone |
| D5 | Webhook delivery | The coordinator long-polls the relay over the tailnet. The relay is a dumb inbox. See "Webhook relay" for why |
| D6 | Webhook truth | A webhook is a hint to refresh that issue, not a source of truth. The coordinator fetches the issue from GitHub (or evicts it) before the cache changes. A forged or replayed event cannot itself move a label in the cache |
| D7 | Update lists | Only the acting coordinator runs `gh release list` (or its successor), at most once an hour. Each machine downloads its own asset only when it learns of a tag it does not already have |
| D8 | Protocol | A new carried HTTP route for the cache, and a new `SINCE` constant, take the next protocol number that is free on `main` when this is built. P3 has reserved 7 and P2c has reserved 8 in their specs, while `main` at v0.13.0 still speaks [`PROTOCOL`](../../crates/foundry-proto/src/messages.rs) 6. The plan names the number then. An older peer is not sent the new call |

## How it works

### The cache

The acting coordinator holds one cache for the fleet, keyed by project, then by issue number. Each issue entry holds at least: number, title, labels, assignees, state (`open` or `closed`), `updatedAt`, and the `factory:*` label (or labels) it carries. Issues with no `factory:*` label are not kept. The cache also holds, per GitHub login the factory uses: remaining GraphQL points, the limit, `resetAt`, the cost of the last refresh, when the last refresh ran, and whether listings are paused until `resetAt`.

A pass lists `ready`, `triage`, `building`, `stuck` and `needs-info` by filtering this cache, instead of calling [`list_issues`](../../crates/foundryd/src/factory/github/read.rs). The pass still runs every `poll_seconds`. Cut detection stays: if a project's open factory issues exceed the listing cap, the cache says so and the stuck / needs-info path may still use [`issue_labels`](../../crates/foundryd/src/factory/github/read.rs) for the numbers it is missing, as it does today when a listing is cut.

On start, or when this machine becomes the acting coordinator, it does one full refresh per on project before it claims anything from the cache. A restart does not require a file-backed cache; a full refresh is enough. A file-backed snapshot is allowed but must not contain a token.

### Serving the cache to peers

The cache is read through a carried HTTP route on the foundry API, for example `GET /api/factory/issues`, reachable on the tailnet the way other machine-to-machine routes already are. The acting coordinator answers from its in-process cache. Any other machine that is asked carries the request to the acting coordinator and does not call GitHub. A coordinator that is on a protocol older than the new `SINCE` constant, or that cannot be reached, yields the same kind of answer other carried routes already use (`503` or the existing "not linked" / "older" errors), with no GitHub fallback.

The Factory view and `foundryd status` may read this route. They show cache age and quota; they do not need a second GitHub listing.

### One query per project

A refresh of one project is one GraphQL document, not a family of `gh issue list` calls. The search is of the form:

```
repo:OWNER/NAME is:issue is:open (label:"factory:ready" OR label:"factory:triage" OR label:"factory:needs-info" OR label:"factory:building" OR label:"factory:pr-open" OR label:"factory:stuck" OR label:"factory:proposed")
```

The document also requests `rateLimit { cost remaining limit resetAt }`. Fields per issue: number, title, state, updatedAt, assignees, labels. Results are grouped by `factory:*` label in memory. An issue that carries more than one such label appears in each group, as today's per-label listings would have returned it more than once.

Pagination stays inside that one refresh: further pages are part of the same refresh, not a new listing style. The listing cap is the cache's, and a cut is reported the way [`Listing.cut`](../../crates/foundryd/src/factory/github/read.rs) is today.

The GraphQL document is issued through the existing `Tracker` / `gh` seam, as the project's login, so tests keep using the fake tracker and no test calls GitHub. [`list_issues`](../../crates/foundryd/src/factory/github/read.rs) may remain for tests and for any temporary compatibility, but the pass and the cache refresh must not use it for these listings once the cache is on.

### Incremental updates and conditional requests

After a full refresh, the next refresh of a project asks only for issues (open or closed) whose `updatedAt` is on or after the previous refresh, minus a short overlap (60 seconds) so a clock skew does not drop an edit. A closed issue, or one that has lost every `factory:*` label, is evicted. An issue that still qualifies is upserted.

If the implementation short-circuits with the REST Search API (or another REST listing of the same query), it sends `If-None-Match` with the last ETag. A `304` means the cache is still valid, spends none of the primary REST budget, and counts as a successful refresh. GraphQL has no ETag; incremental `updated-since` is the GraphQL path. A full refresh still runs at least every 15 minutes, and whenever the coordinator role is taken, so a missed incremental update cannot last more than that.

### Claims and writes

[`start::claim`](../../crates/foundryd/src/factory/runs/start.rs) already begins with `tracker.issue(repo, issue)`. That live fetch stays, and is the source of truth for whether the issue is still `factory:ready`, still open, and not postponed. If the live fetch shows the cache was stale, the claim is not taken, the cache entry is updated from the fetch, and the pass moves on. If listings are paused because the GraphQL budget is 0, no new claim is started: a live fetch would fail the same way, and a claim from the cache alone is forbidden.

Every other write (labels, comments, assignments, pull request operations) still goes straight to GitHub, as today. After a successful write, the cache is updated from what was written (optimistic), and a later refresh or webhook corrects it if GitHub disagrees. A failed write leaves the cache as it was, unless a following fetch shows otherwise.

### Rate-limit awareness

Every GitHub response the factory and the update check see is inspected for remaining quota. GraphQL: the `rateLimit` field on the document, and any error text of the form `API rate limit already exceeded` or `API rate limit exceeded` that [`gh.rs`](../../crates/foundryd/src/factory/github/gh.rs) already turns into a rate-limit error. REST: `X-RateLimit-Remaining`, `X-RateLimit-Limit` and `X-RateLimit-Reset`. Quota is tracked per GitHub login, not as one fleet-wide number.

When remaining GraphQL points fall below 20% of the limit, the coordinator stretches `cache_seconds` so the remaining budget, at the last refresh's cost, would last until `resetAt`. The stretch is never shorter than `cache_seconds`, and it is logged once per stretch. New claims may continue while a live fetch still succeeds.

When remaining is 0, or GitHub answers that the limit is already exceeded, the coordinator pauses list refreshes and new claims until `resetAt`. It logs that pause once per window (not every `poll_seconds`). Passes still run against the last cache: they advance runs they already hold, and they do not take a new claim. At `resetAt` the pause ends, a full refresh runs, and a single log line says that listings have resumed.

A write or a live fetch that hits the limit mid-run follows the pass's existing error path, and also enters the pause if the answer says the budget is exhausted.

### Failover

Coordinator order is already [`acting`](../../crates/foundryd/src/coordinator.rs): the first machine in the priority list that this machine can reach, else the lowest reachable id. Factory passes already start only on the acting machine and stop when it stops acting, so the next machine takes the leases after the wait ([`factory/mod.rs`](../../crates/foundryd/src/factory/mod.rs)).

Cache ownership follows that role. When this machine stops acting, it stops refreshing, stops long-polling the relay, and stops serving the cache as coordinator (it may still carry a read to the new one). When this machine starts acting, it does a full refresh of each on project, then serves the cache, then starts the long-poll. It does not wait for the previous coordinator's cache. Two machines must not refresh the same project's lists at once: a machine refreshes only while it still acts, and it re-checks that before each refresh.

### Webhook relay

A small service, built in this repo (for example `crates/foundry-relay`), runs on production-2 and is reached by GitHub at `https://foundry-relay.originalsolutions.com.au`. It is not `foundryd`. It holds no GitHub token.

GitHub is configured (later, by a person) to send `issues`, `label`, `pull_request`, `check_suite` and `check_run` events for `original-solutions/people-hub`, `original-solutions/foundry` and `Trilogy-Care/tc-supplier-portal-v2` to that URL. The relay accepts `POST` of a GitHub webhook, checks `X-Hub-Signature-256` as an HMAC-SHA256 of the raw body with the webhook secret, and refuses a request that does not match, with `401` and no body that echoes the secret. A valid event is stored in a short queue (on disk or in memory, with a cap and a TTL of at least one hour) and acknowledged with `202`. The relay does not interpret labels or call GitHub.

The acting coordinator long-polls the relay over the tailnet, for example `GET /events?after=<cursor>` on the relay's Tailscale address, with a timeout of about 25 seconds. New events are returned as a batch with a new cursor. The coordinator treats each event as a hint: it refreshes the one issue (or pull request's issue) named in the event, or evicts it, then continues. The one-minute (or stretched) poll stays as the backstop, so a lost webhook or a relay outage delays a cache update by at most one refresh interval.

Why the coordinator long-polls, rather than the relay pushing to foundryd:

- The relay stays a dumb inbox. It does not need to discover which machine currently acts, and it does not authenticate to the foundry API or hold a foundry or GitHub token.
- When the coordinator role moves, the new acting machine starts the long-poll and the previous one stops. Events queued while the old coordinator was down are delivered to the new one, so a brief outage does not drop a label change.
- A push from the relay would have to track the acting machine and would couple the relay to foundry's join and coordinator state.

Authentication of the long-poll: the `/events` route binds to the tailnet only, not to the public interface. GitHub's `POST` is the only public route. The long-poll also carries a `relay` secret, stored on production-2 in a `0600` file and on each foundry machine with `foundryd secret set relay`. That secret is not a GitHub token. A machine long-polls only while it acts. The secret never appears in a log line, an error, or a response.

Deploy, described here and not done by this work:

- DNS: `foundry-relay.originalsolutions.com.au` points at production-2.
- TLS: terminated on production-2 the way other `originalsolutions.com.au` hosts on that server already are.
- Process: a systemd unit (for example `foundry-relay.service`) running as an unprivileged user, restarting on failure, with the webhook secret and the relay-auth secret in `0600` files or in a systemd credential, never in the unit file in this repo, never committed.
- production-2 is on the tailnet, so the coordinator can reach the long-poll route.
- GitHub webhook registrations for the three repos are created by a person, with the webhook secret, after the URL serves HTTPS.

### Update checks

[`CHECK_EVERY`](../../crates/foundryd/src/update.rs) becomes one hour, not ten minutes. Only the acting coordinator runs `gh release list` (or the equivalent releases query). It publishes the newest eligible tag through the existing update-ready machinery so every machine can see it. Each machine, coordinator or not, downloads and verifies its own asset only when that tag is newer than what it already has; macOS and Linux assets differ, so the download cannot be done only once for the fleet. A machine that does not act must not list releases. A coordinator that has listed within the last hour does not list again, including after a restart if it still remembers the time; if it cannot remember, one list on taking the role is allowed.

### Status and UI

`foundryd status` prints, for each GitHub login the factory uses, a line with remaining points, the limit, and when the window resets, plus whether listings are stretched or paused. No token, no secret, no raw header dump.

The Factory view (and the Factory category in Settings, as a one-line summary if that is the quieter place) shows the same: remaining / limit, reset time in the browser's local zone, cache age, and a paused or stretched state when that applies. A missing quota (an older coordinator, or no refresh yet) reads as unknown, not as zero.

## Non-goals

- Deploying or operating production-2, DNS, TLS, systemd, Tailscale on production-2, or the GitHub webhook registrations.
- Changing which machine acts as coordinator, or the lease.
- Auto-merging, review loops, or anything in P2b.
- Caching pull request review comments, check-run logs, or `gh api` reads that are not issue listings, except that a webhook may trigger a targeted refresh of one issue.
- Using webhooks as the only source of issue state.
- Putting a GitHub token on production-2, or committing a webhook secret, a relay secret, or a token.

## Rollout

1. The cache, the one-query refresh, rate-limit backoff and pause, the carried cache route, and the quota in `foundryd status` and the UI ship in a Foundry release. On first start of that release, the acting coordinator does a full refresh and the pass stops calling `list_issues` for those listings. No feature flag is required once the tests pass; the old per-label listing path is removed from the pass, not left as a switch.
2. Every machine is on the new protocol before a peer is expected to carry a cache read. An older peer simply cannot read the cache and does not list issues itself; the coordinator's passes do not depend on peers reading the cache.
3. The update-check change ships in the same release: one list per hour, coordinator only.
4. The relay binary and its unit file (with secret paths, not secret values) ship in the repo. A person then deploys it on production-2, points DNS, installs TLS, puts the secrets in place, and registers the webhooks. Until that is done, the one-minute poll is the only refresh, which already meets the point budget. The factory is not blocked on the relay.
5. After a quiet week at under 500 GraphQL points per hour on the three-project, seven-machine fleet, the exit criterion is met.

The way back: a release that restores `list_issues` in the pass and the ten-minute update list would return the outage risk; do not ship one. If the cache refresh is wrong, fix it. If the relay misbehaves, stop long-polling and turn the webhooks off; the poll continues.

## Risks

- A stale cache could offer the same `factory:ready` issue twice. The live fetch in `start::claim` is the guard; tests must prove a claim is abandoned when that fetch shows a different label or a closed issue.
- GitHub's search (GraphQL or REST) has its own cost and a secondary rate limit. The first implementation must measure `rateLimit.cost` of the one-query document and stay inside the 500-point hour with margin. If search is too expensive, aliases in one GraphQL document (one field per label) are an allowed fallback, still one HTTP request per project, with the cost recorded in the same way.
- A webhook is not trusted. The cache changes only after a GitHub fetch, or after an eviction confirmed by a fetch or a later refresh.
- Two coordinators briefly both acting, during a split, could double the listing rate. Each refresh re-checks that this machine still acts; the lease already stops a split from claiming twice.
- Stretching or pausing listings delays a newly ready issue until the next refresh or until `resetAt`. That is the point of the backstop, and is better than a 15-minute outage of every pass.
- The long-poll or the relay can drop events. The timed refresh covers that.
- Showing quota in the UI must never show a token, a secret, or a webhook payload.

## Open questions

- The measured `rateLimit.cost` of the one-query search versus a single document with one alias per label. The plan picks whichever stays further under 500 points per hour; the tests lock that choice.
- Whether a 15-minute full-refresh backstop is short enough, or should be configurable.
- The exact bind and reverse-proxy layout on production-2 (Caddy, nginx, or something already there). The deploy is a person's job; the binary only needs a listen address and the two secret paths.
- Whether the `relay` secret is also rotated from Settings, like other machine secrets, or only through `foundryd secret set relay`. Settings already has a machine secrets page in P3; following that is preferred once P3 is on the machine.
- How `rawphp` (or any second login) is shown when only some projects use it. Quota is per login; the UI must name the login.
- Whether `poll_seconds`'s help text in Settings should mention that issue listings use `cache_seconds`, so the two are not confused.

## Testing

Tests first, per `AGENTS.md`. No test calls GitHub. The fake `Tracker` records each GitHub document it was asked for, its imagined `rateLimit`, and its ETag / 304 behaviour. No test spends model calls. Secrets (a webhook secret, a relay secret, a GitHub token) never appear in a log line, an error, or a response, including in these tests.

### 1. Coordinator-owned cache

- A coordinator refresh of three projects issues one listing document per project and not one per label; a pass that follows reads `ready`, `triage` and `building` from the cache and does not call `list_issues`.
- A peer asked for the cache carries the request to the acting coordinator and issues no GitHub listing, including when the coordinator is slow.
- A peer that cannot reach the coordinator, or whose coordinator speaks a protocol before the new `SINCE`, answers with the carried-route refusal and still issues no GitHub listing.
- Two refreshes `cache_seconds` apart on a clock the test moves: the second does not start before the interval. A `cache_seconds` of 30 is accepted; 29 and 301 are refused the way an out-of-range `poll_seconds` is refused.
- `poll_seconds` still governs how often a pass runs; a test with `poll_seconds` 30 and `cache_seconds` 60 sees two passes per refresh.

### 2. One query per project, incremental and conditional

- The listing document's search covers every `factory:*` label as an `OR`, and the cache sorts one issue with `factory:ready` into the ready list and one with `factory:triage` into triage. An issue that carries both appears in both lists.
- A full refresh of a project with more than one page still counts as one refresh of that project, and `cut` is set once the cap is passed.
- After a full refresh, the next refresh asks only for issues updated since the last refresh minus the overlap, upserts a changed title and label, evicts a closed issue, and evicts an issue that no longer carries any `factory:*` label.
- A REST listing (when used) sends `If-None-Match`; a `304` leaves the cache unchanged, counts as a success, and adds no GraphQL cost.
- A full refresh still runs when the coordinator role is taken, and when the clock has moved 15 minutes since the last full refresh, even if incrementals have run in between.

### 3. Claims and writes

- `start::claim` still calls `tracker.issue` once for the issue it would claim. When that fetch shows `factory:ready` and open, the claim proceeds as today.
- When the live fetch shows `factory:building`, or a closed issue, or a `postponed` label, no claim is taken, and the cache entry matches the fetch afterwards.
- When listings are paused at remaining 0, `start::claim` is not started; the tracker records no issue fetch for a new claim, and the issue stays unclaimed.
- A successful label change on GitHub updates the cache entry without waiting for the next refresh. A refused write leaves the cache entry as it was.

### 4. Rate-limit awareness

- A GraphQL answer with `rateLimit.remaining` at 19% of `limit` stretches the next refresh interval, and the log holds one stretch line, not one per pass.
- A GraphQL answer with remaining 0, or a `gh` error `API rate limit already exceeded`, pauses listings and new claims until the supplied `resetAt`. Ten passes inside the window produce one pause log line, and no listing document. At `resetAt` a full refresh runs and one resume line is logged.
- Remaining, limit and `resetAt` are kept per login: a second login's budget is not paused when the first is exhausted, and its projects still refresh.
- `foundryd status`, from a fixture state, prints a quota line per login with remaining, limit and reset, and prints a paused or stretched clause when that applies. A fixture token does not appear.
- The Factory view (web unit / component test) shows remaining / limit, a reset time, cache age, and a paused state from the same fixture, and shows unknown when the quota is missing. No token is rendered.

### 5. Failover

- A three-daemon fleet test: the acting coordinator holds the cache and refreshes; it is stopped; the next machine in priority does a full refresh and then serves the cache; the stopped machine, if started again as a peer, carries reads and does not list.
- A machine that loses the role mid-refresh does not start the next project's listing.
- Two machines that both believe they act (a split the test arranges) re-check before each refresh; a project is not listed by both in the same interval once each has re-checked.

### 6. Webhook relay

- A POST with a valid `X-Hub-Signature-256` for the raw body is queued and answered `202`. A POST with a wrong signature, a missing signature, or a body that does not match the signature is `401`, and the queue is empty. The secret does not appear in the response or in logs.
- A long-poll with the relay secret, after the tailnet bind, receives the queued event and a cursor; a second poll with that cursor receives nothing until another event is posted. A long-poll with the wrong secret is refused.
- The public listen address serves the webhook POST and does not serve `/events`. The tailnet listen address serves `/events`.
- The acting coordinator, on receiving an `issues` labeled event, fetches that one issue and updates the cache without waiting for `cache_seconds`. A forged event that never matches a real fetch (the fake tracker returns not-found) does not insert an issue.
- While the relay is down, the timed refresh still updates the cache at `cache_seconds`.
- No path in the relay crate is given a GitHub token; a test of its config refuses a GitHub token field if one is offered.
- Unit file and example config in the repo contain paths and environment variable names, not secret values.

### 7. Update check

- A seven-daemon test (or a cheaper equivalent with seven fake update loops): in one hour on the test clock, `gh release list` (or its fake) is called once, and only on the acting coordinator.
- A peer never lists tags. When the coordinator publishes a newer tag, a peer downloads its own asset once and not again for that tag.
- A coordinator restart inside the hour, with the last list time remembered, does not list again. Without a remembered time, one list is allowed.
- The ten-minute `CHECK_EVERY` of 600 seconds is gone: the constant is 3,600 seconds, and a test asserts that.

### 8. Acceptance: point budget

- A measurement test simulates one hour at steady state: two Macs and five VPS workers, three on projects, `cache_seconds` 60, `poll_seconds` 30, no webhook events required, one update list, incrementals after the first full refresh, and the fake tracker's `rateLimit.cost` set to the measured or a conservatively high documented cost of the chosen document. Total GraphQL points for listings and the update list are under 500. The test fails if a per-label `list_issues` path is used, or if a peer lists issues or releases.
- After rollout, a one-hour reading of `rateLimit` (remaining at the start and end of a quiet hour, or the sum of `cost`) on the live coordinator is enough to confirm the same bound; that reading is not a CI test.

## Acceptance criteria

1. On a fleet of five VPS workers and two Macs, with the three projects on, GraphQL use at steady state stays under 500 points per hour, as the measurement test (and, after rollout, a live hour) shows.
2. A peer never calls GitHub to list issues or to list releases.
3. A new `factory:ready` issue is visible to a pass within one `cache_seconds` interval, or sooner when the relay is on and the webhook has been delivered.
4. A claim is not taken on an issue whose live fetch is no longer `factory:ready`.
5. When GraphQL remaining hits 0, `foundryd status` and the Factory view say listings are paused until reset, the log has one pause line for the window, and no listing document is sent until `resetAt`.
6. Killing the acting coordinator causes the next machine in priority to take the cache and continue claims after its full refresh, without a person intervening.
7. The relay, when deployed, accepts only HMAC-valid webhooks, holds no GitHub token, and is not required for the factory to meet criteria 1 to 6.

## Cut for the plan

These slices are for the implementation plan and the factory issues that follow it. They are not filed with this pull request.

| # | Slice | Depends on |
|---|-------|------------|
| 1 | Cache types, `cache_seconds`, the one-query listing on the tracker, incremental / ETag behaviour, and pass listings read from the cache. Unit tests of §1 and §2 | none |
| 2 | Live claim / write rules against the cache, including the pause that skips new claims. Tests of §3 | 1 |
| 3 | Quota tracking, stretch and pause, `foundryd status` and the Factory UI. Tests of §4 | 1 |
| 4 | Carried cache route, the new protocol `SINCE`, and fleet failover tests of §5 | 1, 3 |
| 5 | Update check: hourly, coordinator-only list, peers download on a new tag. Tests of §7 | 4 |
| 6 | `foundry-relay` crate, HMAC, queue, tailnet long-poll, coordinator consumer, tests of §6, unit file without secrets | 2, 4 |
| 7 | Measurement test of §8, docs in `docs/guide/factory.md` and a pointer in `AGENTS.md` | 1 to 6 |

## Changes to other documents

When this is built, not in this pull request:

- `AGENTS.md` should point at this spec and its plan, the way it points at P2 and P3.
- `docs/guide/factory.md` should say that listings come from the coordinator cache, and should describe the quota line and the relay as an optional backstop accelerator.
- P2's `poll_seconds` wording in `docs/specs/P2-factory.md` should be clarified so it remains the pass cadence, not the GitHub listing cadence.
