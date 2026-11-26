# Manual updates

Product: Foundry. A change to P1's updates.

Status: drafted 2026-11-26 from a brainstorm the same day, revised after three council rounds. Nothing here is built.

## Why

Every machine installs a newer release on its own within 10 minutes of the tag, and restarts. That takes three choices away from Tom:

- When a machine restarts. It may be in the middle of a factory run or a session he is watching.
- Which machine tries a release first. A bad release reaches the whole fleet in one check interval.
- What each machine runs. No machine knows the version of any other. The machine record carries the tag a machine refused, but not the version it runs.

## Goal

A release waits on every machine until someone installs it. Every Foundry UI shows which machines have a release waiting and which version each one runs. Any UI, on any machine and on a phone, can install the waiting release on any machines it picks.

Exit criterion: once every machine runs this release, a newly published release is installed by no machine until it is picked in the UI. Picking two of three machines installs it on those two, and the third keeps its version and its waiting release.

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
- Removing any version from `<home>/versions/`. Versions build up as they do today.
- Keeping the checklist across a page reload.

## Decisions

| # | Question | Decision |
|---|---|---|
| U1 | Who installs | Only a person, from the UI. No machine installs a release on its own |
| U2 | When to fetch | Ahead of time. Each machine downloads, verifies and unpacks the highest tag at its 10-minute check, so an install is instant and a release that fails its checks never reaches the badge. Downloading on click was rejected: a bad signature or a binary that does not run would show up only after the click |
| U3 | Who fetches | Each machine fetches its own build, as today. One machine fetching for the fleet was rejected: it needs a new way to send binaries between machines, for each arch and OS |
| U4 | A busy machine | Installs at once, with no warning. Each terminal lives in its own holder, which the update does not restart, and since #96 a session started before a restart keeps reporting its state. The daemon is gone for a few seconds and the sessions keep running |
| U5 | Reaching another machine | The install is a carried route, so the carry layer sends it to the machine it names over the peer link, as it does for workspaces and terminals |
| U6 | The phone, and a mobile client later | Nothing new. Every machine holds every machine's record, so a UI on any machine shows the same badge, and its install calls are carried. A native mobile client later uses the same API on whichever machine it connects to |
| U7 | A peer on a release without the route | The peer protocol goes from 5 to 6, and the carry layer sends the install route only on a link in protocol 6 or later. Any other carried route keeps its current minimum. Without this, a protocol-5 peer receives the call and answers `400` |
| U8 | What the UI remembers | The open checklist holds, in the page's memory only, the machine, the tag and the answer of each call it sent, until it is closed or the page reloads. Everything else is read from records |

## How it works

### Versions and tags

A version is three numbers, like `0.9.1`, as `foundryd --version` prints it and as `current` names it. A tag is `v` and a version, like `v0.9.1`. A tag matches a version when its numbers equal the version's, and a tag is above a version when its numbers are higher, compared as the updater compares them today.

### The updater

The check every 10 minutes keeps today's rule for which tag it wants: the highest tag, and none if that one is refused or is not above the running version. It never falls back to a lower tag.

- If the wanted tag is already the ready tag, the check does nothing more. It downloads nothing and touches nothing on disk.
- Otherwise it downloads the archive and its signature, checks the signature, unpacks it and checks `foundryd --version`, as today. A bad release is refused, as today, and the ready tag stays as it was. Any other failure is tried again at the next check, and the ready tag stays as it was.
- A release that passes is moved into `<home>/versions/<version>` and recorded as the ready tag in `<home>/update.json`, in one step under the updater's lock. That step re-reads the state under the lock, and goes ahead only if `current` still names the running version and no trial is on. Otherwise it gives up and leaves `current`, the state and the versions on disk as they were.
- Making a tag ready leaves `refused` as it is.
- The check no longer points `current`, starts no trial and does not stop the daemon.

A machine has at most one ready tag. A daemon that does not run from `<home>/current`, such as one started with `cargo run`, makes no tag ready, as it installs nothing today.

A ready tag counts only while both binaries of its version are on disk. One whose binaries are gone counts as not ready, so the next check fetches it again.

A hand install clears the ready tag, whatever version it installs. It may unpack into a private stage first, then holds the updater's lock from clearing the ready tag through moving its files into `<home>/versions/` and pointing `current`, so no check or click runs between those steps.

A trial is on only while `current` names the trial's version. A trial whose version `current` does not name, as a crash between the install's two writes leaves, is cleared under the lock by the next check or install, which then goes on with the state that remains. Its tag is then no longer ready, so the next check fetches and stages it again.

### The machine record

`MachineSample` gains two fields, each `#[serde(default)]`:

- `version`: the version the running daemon was built as, like `0.9.0`. It is read from the binary, not from `current`.
- `update_ready`: the ready tag, like `v0.9.1`, while it is above `version` and the daemon runs from `<home>/current`. Otherwise none.

A sample from a peer on an older release has neither. The UI shows its version as unknown and offers no install for it. `update_refused` is unchanged, and a refused tag is never ready.

### Installing

`POST /api/machines/{id}/update` with `{"tag":"v0.9.1"}`.

- The carry layer sends it to machine `{id}`. A machine not in the list is `404`. One that is offline, whose link is down or whose link is below protocol 6 is `503` and names it, and nothing was sent to it. One whose link went down after the call was sent is `504`, as for every carried route, and the install may have happened.
- On the machine, the handler acts only when `{id}` is its own id. It takes the updater's lock and re-reads the state. It answers `409` with `{"error": "...", "ready": "v0.9.2"}`, or `"ready": null`, and changes nothing, when any of these holds: the tag is not the ready tag (as counted above), the tag is not above the running version, the tag is the refused one, the daemon does not run from `<home>/current`, `current` does not name the running version, or a trial is on (as defined above). `ready` in the body is the tag `update_ready` would report, or `null`.
- Otherwise, in this order: it saves the trial with the running version as `previous`, clears the ready tag in the same write, and clears `refused` only when the installed tag is above the refused one. Then it points `current` at the new version. This is the order `commit_version` uses today, so a crash between the two writes never starts a version with no trial.
- If pointing `current` fails, it writes the state back as it was before, answers `500` with the error, and the daemon keeps running.
- Once both writes succeed it answers `202`. The daemon stops only after that answer has left, whether it went to a browser or as the answer of a carried call on the peer link, and stops anyway 5 seconds after the writes if it could not be sent. The service starts the new version, and the trial and rollback run as they do now.

### The UI

The badge sits in the page's top bar, so it shows on a phone without opening the sidebar. It shows while any listed machine has `update_ready`, or while the checklist is open. It names each waiting tag and how many machines it waits on, grouped by tag. Machines between checks can wait on different tags, so "v0.9.1 on 2 machines, v0.9.0 on 1" is a normal state. Removed machines are left out. Each machine in the Machines view shows the version it runs and its waiting tag.

Opening the badge shows a checklist of the listed machines with a release waiting. All are ticked at first. Install takes, at the moment of the click, each ticked machine with the `update_ready` tag shown for it, and sends exactly those pairs for the whole batch. A machine whose ready tag changes meanwhile answers `409`. It sends the calls for every machine except the one serving the page first, and the serving machine's call only once every other call has finished, with an answer, a timeout or a dropped connection. That way a restart of the serving machine never cuts off calls still on their way to the others.

Each row keeps its machine, its tag and its call's answer until the checklist is closed, after the machine's `update_ready` clears. Its state is decided in this order, the first match winning:

| Row says | When |
|---|---|
| Refused | Its `update_refused` is the tag: it rolled the tag back after a failed trial, or a hand install refused it. The "Update refused" attention item is raised as today |
| Running the tag | Its `version` matches the tag. This can still turn into Refused until the trial has stayed up, which can take up to three starts |
| Restarting | The call answered `202` |
| Sent, outcome unknown | The call answered `504`, or the connection dropped after it was sent. The row follows the records from here, and nothing is sent again on its own |
| Not sent | The call answered `503` or `404`, with the reason given |
| Already changed | The call answered `409`. The row shows the error text and the tag now ready, if any |
| Failed | Any other answer, with its error text |

When the serving machine restarts on a new version, the page reconnects and reloads with the new bundle as it does today, which closes the checklist. By then every other call has answered. The Machines view then shows what each machine runs.

## First rollout

No hand install is needed. The release that adds this reaches each machine through that machine's current updater, which still installs on its own. From that release on, nothing installs on its own. Until every machine runs it, the older ones show their version as unknown, get no install button, and keep installing on their own. A machine that rolls this release back is again a machine that installs on its own, and takes the next higher tag by itself. A Mac that still needs the keychain copy described in `docs/install.md` needs the same hand install as today.

## Changes to other documents

- `P1-core.md`: the scope line "Self-update from a tag, with rollback" becomes "Updates from a tag, installed from the UI, with rollback". The first line under `## Updates` becomes "A release is a `v*` tag. Each machine downloads and verifies a newer tag on its own, and installs it when someone picks it in the UI." Acceptance criterion 12 becomes "A release tag installed from one machine's UI onto all three machines ends no terminal on any of them".
- `docs/install.md`: the opening line "and updates itself from release tags" becomes "and downloads release tags, which install when picked in the UI". In `## Updates`, the daemon downloads and verifies a release and waits. It installs once picked in the UI. A hand install still works as described. The keychain paragraphs say "an update the daemon installs itself" and become "an update installed from the UI".

## Testing

Tests come first, as for every change.

- `update.rs`: a check that finds a higher tag downloads, verifies and unpacks it, records it as ready, and leaves `current`, the trial and the daemon alone. A check whose wanted tag is already ready downloads nothing. A refused highest tag makes nothing ready and leaves the ready tag. A higher tag replaces the ready one. A check that finishes after an install moved `current` records nothing and removes nothing. Making a tag ready leaves `refused` alone. A ready tag whose binaries are gone is fetched again. A hand install clears the ready tag and places its files under one hold of the lock. A trial whose version `current` does not name is cleared, and the check goes on.
- The endpoint: each `409` case changes nothing, including a stale ready tag after a hand install of a higher version. The ready tag saves the trial and clears the ready tag before `current` moves, keeps a refusal of a higher tag, answers `202` and then stops the daemon once. A failure to point `current` restores the state, answers `500` and does not stop.
- Carry: the route runs on the machine it names. A peer on protocol 5 gets `503` with nothing sent. A carried install's `202` reaches the caller before the target stops.
- `machine.rs`: the sample carries `version` and `update_ready`, and an older peer's sample without them still reads. A daemon not run from `current` reports no `update_ready`.
- Fleet, three daemons: a UI on the first installs on the second and the third, the serving machine last. The third's sample shows the new version, its terminals keep running, and a machine left unticked keeps its version and its ready tag.
- Web: the badge shows and hides with the records, groups machines by tag and leaves out removed machines. The checklist starts with every machine ticked, sends the serving machine's call last, and sends the tags shown at the click even when a record changes before a call goes. Each row follows the table above, in its order.

## Acceptance criteria

1. On a fleet that runs this release, a published release is ready on every reachable machine within one check interval, and no machine installs it.
2. Every UI shows every machine's version and its waiting tag, on a desktop and on a phone.
3. Installing from the UI on machine A onto machine C restarts only C, ends no terminal on C, and C's row reads "Running the tag".
4. Installing on all three machines from machine A's UI updates all three, A last.
5. A release that fails its trial on C reads "Refused" in the UI and raises "Update refused".
6. A second install of the same tag, sent once the machine is back after the first, is `409` and changes nothing.
