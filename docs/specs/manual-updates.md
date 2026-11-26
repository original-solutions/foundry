# Manual updates

Product: Foundry. A change to P1's updates.

Status: drafted 2026-11-26 from a brainstorm the same day. Nothing here is built.

## Why

Every machine installs a newer release on its own within 10 minutes of the tag, and restarts. That takes three choices away from Tom:

- When a machine restarts. It may be in the middle of a factory run or a session he is watching.
- Which machine tries a release first. A bad release reaches the whole fleet in one check interval.
- What each machine runs. No machine knows the version of any other. The machine record carries the tag a machine refused, but not the version it runs.

## Goal

A release waits on every machine until someone installs it. Every Foundry UI shows which machines have a release waiting and which version each one runs. Any UI, on any machine and on a phone, can install the waiting release on any machines it picks.

Exit criterion: a tag pushed to the repository is installed by no machine until it is picked in the UI. Picking two of three machines installs it on those two, and the third keeps its version and its waiting release.

## Scope

In:

- The updater downloads and verifies a release, then stops before installing it.
- The machine record says which version the machine runs and which release it has waiting.
- One endpoint on each machine that installs its waiting release, carried to it from any machine.
- A badge in the UI with the waiting release, the machines it waits on, and a checklist to install it.

Out:

- Updating on its own, for any machine. There is no per-machine switch. A release is ready ahead of time, so installing it from a phone is one tap.
- Waiting for a machine to go idle before installing. A restart ends no terminal (see Decisions).
- Going back to an earlier version by hand. The automatic rollback covers a version that does not stay up. A version that runs but behaves badly stays on the one machine that tried it until a fixed tag ships.
- An attention item for a waiting release. The badge is the signal.
- Removing versions that ran before. Versions on disk build up as they do today.

## Decisions

| # | Question | Decision |
|---|---|---|
| U1 | Who installs | Only a person, from the UI. No machine installs a release on its own |
| U2 | When to fetch | Ahead of time. Each machine downloads, verifies and unpacks the highest tag at its 10-minute check, so an install is instant and a release that fails its checks never reaches the badge. Downloading on click was rejected: a bad signature or a binary that does not run would show up only after the click |
| U3 | Who fetches | Each machine fetches its own build, as today. One machine fetching for the fleet was rejected: it needs a new way to send binaries between machines, for each arch and OS |
| U4 | A busy machine | Installs at once, with no warning. Each terminal lives in its own holder, which the update does not restart, and since #96 a session started before a restart keeps reporting its state. The daemon is gone for a few seconds and the sessions keep running |
| U5 | Reaching another machine | The install is a carried route, so the carry layer sends it to the machine it names over the peer link, as it does for workspaces and terminals |
| U6 | The phone, and a mobile client later | Nothing new. Every machine holds every machine's record, so a UI on any machine shows the same badge, and its install calls are carried. A native mobile client later uses the same API on whichever machine it connects to |

## How it works

### The updater

The check every 10 minutes stays: it lists the tags, picks the highest one above the running version that is not refused, downloads the archive and its signature, checks the signature, unpacks it and checks `foundryd --version`. Each failure is handled as today. A bad release is refused, and any other failure is tried again at the next check.

Where the check now points `current` at the new version, starts a trial and stops the daemon, it instead records the tag as ready in `<home>/update.json`. It no longer changes `current`, starts no trial and does not stop the daemon.

A machine has at most one ready tag. A check that finds a higher tag than the ready one makes that the ready tag, and removes the version it replaces from `<home>/versions/`, since nothing ever ran it. Installing the ready tag clears it in the same write that starts the trial. A hand install of that version or a higher one clears it too.

### The machine record

`MachineSample` gains two fields, each `#[serde(default)]`:

- `version`: the version this machine runs, like `0.9.0`.
- `update_ready`: the tag ready on this machine, like `v0.9.1`, while it is above `version`.

A sample from a peer on an older release has neither. The UI shows its version as unknown and offers no install for it. `update_refused` is unchanged, and a refused tag is never ready.

### Installing

`POST /api/machines/{id}/update` with `{"tag":"v0.9.1"}`.

- The carry layer sends it to machine `{id}`. A machine not in the list is `404`. One that is offline, whose link is down or that runs a release without this route is `503` and names it, and nothing was sent to it.
- On the machine itself, a tag that is not its ready tag is `409`, with the ready tag in the body, or none. Nothing changes. Two UIs that click at once, or a higher tag that became ready since the page last looked, end here.
- A tag that matches points `current` at that version, starts a trial and clears the ready tag, as one write under the updater's lock. The caller gets `202`, and only after that does the daemon stop. The service starts the new version, and the trial and rollback run as they do now.

### The UI

The badge shows while any machine has `update_ready`. It names each waiting tag and how many machines it waits on, grouped by tag. Machines between checks can wait on different tags, so "v0.9.1 on 2 machines, v0.9.0 on 1" is a normal state. Each machine row in the Machines view shows the version it runs and its waiting tag.

Opening the badge shows a checklist of the machines with a release waiting. All are ticked at first. Install sends one call per ticked machine. Each row then shows where it got to, read from the records the fleet already syncs, so the UI keeps no state of its own about it:

| Row says | When |
|---|---|
| Restarting | The call answered `202` and the machine's `version` is not yet the tag |
| Done | Its `version` is the tag |
| Rolled back | Its `update_refused` is the tag. The "Update refused" attention item is raised as today |
| Offline, or on an older Foundry | The call answered `503`. Nothing was sent |
| Already changed | The call answered `409`. The row shows the tag now ready, if any |

Installing on the machine that serves the page drops the page for a few seconds. It reconnects as it does after any restart, and the row then reads from the new records.

## First rollout

No hand install is needed. The release that adds this reaches each machine through that machine's current updater, which still installs on its own. From that release on, nothing installs on its own. Until every machine runs it, the older ones show their version as unknown and get no install button.

## Changes to other documents

- `P1-core.md`: the scope line "Self-update from a tag, with rollback" becomes "Updates from a tag, installed from the UI, with rollback". The first line under `## Updates` becomes "A release is a `v*` tag. Each machine downloads and verifies a newer tag on its own, and installs it when someone picks it in the UI." Acceptance criterion 12 becomes "A release tag installed from one machine's UI onto all three machines ends no terminal on any of them".
- `docs/install.md`, `## Updates`: the daemon downloads and verifies a release and waits. It installs once picked in the UI. A hand install still works as described.

## Testing

Tests come first, as for every change.

- `update.rs`: a check that finds a higher tag downloads, verifies and unpacks it, records it as ready, and leaves `current`, the trial and the daemon alone. A refused tag never becomes ready. A higher tag replaces the ready one and removes its version. Installing clears the ready tag, and so does a hand install of that version or a higher one.
- The endpoint: a tag that is not the ready one is `409` and changes nothing. The ready tag points `current`, starts the trial, answers `202` and then stops the daemon once.
- Carry: the route runs on the machine it names, and a peer on an older protocol gets `503` with nothing sent.
- `machine.rs`: the sample carries `version` and `update_ready`, and an older peer's sample without them still reads.
- Fleet, three daemons: a UI on the first installs on the third. The third's sample shows the new version, its terminals keep running, and the second keeps its version and its ready tag.
- Web: the badge shows and hides with the records and groups machines by tag. The checklist starts with every machine ticked. Each row moves through the states in the table above.

## Acceptance criteria

1. A pushed tag is ready on every machine within one check interval, and no machine installs it.
2. Every UI shows every machine's version and its waiting tag, on a desktop and on a phone.
3. Installing from the UI on machine A onto machine C restarts only C, ends no terminal on C, and C's row reads "Done".
4. A release that fails its trial on C reads "Rolled back" in the UI and raises "Update refused".
5. A second install of the same tag, sent after the first, is `409` and changes nothing.
