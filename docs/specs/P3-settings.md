# P3: Settings

Product: Foundry, a dark software factory that also runs manual sessions.

Status: design approved in chat on 2026-11-28, then reviewed the same day by a council of four models (Claude, Gemini, Grok, Sol) against `main` at v0.11.1. Their agreed changes are in. Nothing here is built. The plan comes next, then the factory issues listed at the end.

## Why

Settings are spread over five places. The factory's settings sit in a fold on the Factory view, and each project's factory controls appear there and in a dialog opened from the Composer. Machine limits, priority, rename, add and remove are mixed into the machines board with live status, and the sidebar renames a machine in place. Notification choices live in the alerts bell's popover. The update checklist sits in the header. The theme is a toggle in the sidebar.

Some settings have no UI at all. A machine's GitHub tokens, its default GitHub login and its gateway keys can only be changed by running `foundryd` on that machine. When the factory reports that no machine holds a project's login, the fix today is to SSH into a machine. Setting a gateway key also needs a restart of `foundryd` before sessions can start.

## Goal

One Settings page, with categories on the left and a page of settings on the right. It holds the controls listed under "What moves", and the credential controls of every machine in the fleet, from a browser on any of them.

Done when every control listed under "What moves" works from Settings, the old spots hold only a summary or a link, and a GitHub token or gateway key can be stored on another machine from the UI and used by its next session without a restart.

## Scope

In:

- A `/settings` route with grouped categories, a summary of each category's values in the nav, and a review bar for changes to shared settings.
- Moving the existing controls listed under "What moves" into Settings.
- A page per machine, holding its shared settings, its waiting update, and what it keeps in its own secret store.
- Storing a GitHub token, a gateway key or a gateway management key on any machine from any machine, and naming a machine's default GitHub login. All write-only.
- A gateway key that takes effect at the next session start, with no restart, and an old key that stays hidden while a session that started with it runs.
- Showing on each project which machines report holding its GitHub login.

Out:

- Removing a token, token expiry and token scopes.
- One action that applies a value to every machine at once.
- Search across settings, and an overlay mode for Settings.
- Install-time configuration (`FOUNDRY_PORT`, `FOUNDRY_HOME`, `FOUNDRY_GATEWAY_URL`) and a repo's `.dw/config.yml`. They stay where they are.

## Decisions

| # | Topic | Decision |
|---|-------|----------|
| S1 | Layout | Categories on the left in two groups, Fleet and This browser. A page of rows on the right: label and help on the left of each row, control on the right |
| S2 | Nav summaries | Each nav entry has a second line with its current values, built from state the page already holds. Nothing extra is fetched for it |
| S3 | Machines | A page per machine, listed under Machines in the nav. The Machines page itself holds only what spans the fleet |
| S4 | Login coverage | Each project row in Projects says which machines report holding its login |
| S5 | Saving | Three rules, by where a value lives. See "Saving" |
| S6 | Old spots | Consolidate. Each old spot keeps a one-line summary or a link to its Settings page, not a second copy of the controls |
| S7 | Access | The same as P1: anyone on the tailnet who can open the UI can change settings, as they can already add or remove a machine or open a shell on any machine |
| S8 | Replaced secrets | A session keeps the secrets it started with in a `0600` file in its own folder for its life, so an old value stays hidden after a restart that adopts the session |

## Navigation

Routes:

- `/settings`, which opens the first category, Factory.
- `/settings/$category`, where the category is `factory`, `projects`, `machines`, `notifications`, `appearance` or `keyboard`.
- `/settings/machines/$machine`, where `$machine` is the machine's id.

Ways in:

- A gear button beside the theme button, in the sidebar and in the rail.
- A "Settings" entry in the jump palette, and one entry per category.
- `⌘,` (Ctrl+, off macOS), with the same guard as `?`: not while the focus is in a text field or a terminal.

The nav:

```
FLEET
  Factory          opus · high · 3 runs · 2 gates
  Projects         3 projects · factory on 2
  Machines         3 machines · 1 offline
    personal-mac   coordinator
    studio-mini    update ready
    build-box      offline
THIS BROWSER
  Notifications    cards · catch-up
  Appearance       dark
  Keyboard
```

Factory's summary is today's `settingsLine`. A machine's chip is the first that applies of: offline, update ready, coordinator. Each chip is a word as well as a colour. A "← Board" link at the top of the nav goes back to `/`.

Below 720px the nav becomes a row of categories that scrolls sideways above the page. Summaries and machine entries are hidden there, and the Machines page lists the machines instead.

Browser settings belong to the address the page was opened from. Opening the UI from another machine's address can show different ones.

## What moves

| Today | Moves to | What stays behind |
|-------|----------|-------------------|
| Factory view's "Settings for every project" fold (`SettingsFold`, `FleetSettings`) | Settings → Factory | The fold's summary line, as a link to Settings → Factory |
| Factory view's per-project controls (`ProjectFactoryControls` in `Factory.tsx`) | Settings → Projects | Each project's status, with a link to its block in Settings → Projects |
| Projects dialog (`Projects.tsx`), opened from the Composer's Projects chip and its "Add a project" link | Settings → Projects | Both Composer links open `/settings/projects` |
| Machines board: limits dialog, priority order, Add machine, remove | Settings → Machines and each machine's page | The board keeps status, sessions, load and Start task. Each machine card gets a "Settings" link to that machine's page |
| Sidebar's rename control (`Sidebar.tsx`) | That machine's page | The machine's name links to its page |
| Header update checklist (`UpdateBadge.tsx`) | Settings → Machines | The header keeps its one-line summary, as a link |
| Alerts bell popover's notification choices (`NotifyButton`) | Settings → Notifications | The bell keeps its icon and alerts and links to Settings → Notifications |
| Theme toggle | Settings → Appearance shows the same choice | The toggle stays. Both use one theme state, so each shows the other's change at once |
| KeySheet | Settings → Keyboard, read-only | `?` still opens the sheet |

The launchpad's first step (`FirstRepo`) stays where it is. It is the first-run flow, not project management.

The Composer's draft (the task, base, model, machine and workspace name) and its project choice live above the route, so they are still there on return from Settings. A project added from Settings during that visit is the chosen project on return, as the dialog's `onAdded` does today.

## Pages

Factory: pause claims, model, effort, runs at once, gates at once, the poll interval (10 to 60 seconds) and the feedback interval, with today's checks.

Projects: one block per project with its repo, factory switch, GitHub login, pause and Remove, then the Add project form, with today's behaviour. Each project whose factory is on and has a login gets a coverage line under the login:

> os-factory · held on personal-mac, studio-mini · not on build-box

The line comes from `held` on the events state (see "What reaches the browser"). It uses the saved login, matched with the same case as placement, until a changed login is saved. A machine with no held record shows as "unknown", not "not on". Each machine name links to that machine's page. When no online machine reports holding the login, the line is amber and reads "No online machine reports holding this login." A project with no login has no line.

Machines: the priority order with up and down buttons, Add machine, and the update checklist, all as today. The checklist still installs on the machine serving the page last, and keeps its waiting and restart feedback.

A machine's page, with the same sections in the same order for every machine:

1. Update. Shown only while a release is ready on the machine: "v0.11.2 is ready", and Install, which sends that machine's ready tag to `POST /api/machines/{id}/update`. Installing on the machine serving the page restarts it, and the page says so before it sends.
2. In the shared settings. Name, and placement limits.
3. Kept on this machine.
   - GitHub logins: each login the machine holds, by name, with the default marked. "Make default" on any other held login. "Add token" takes a login and a token. Adding a login the machine already holds replaces its token. A default set with the CLI for a login with no token shows as "default, no token held".
   - Gateway key and gateway management key, each "Set" or "Not set", with Replace or Set.
4. Remove from the fleet, which asks once more. It isn't offered on the machine serving the page, as today.

Notifications: the three choices the bell has today (cards, desktop alerts, catch-up), with the browser's permission state beside desktop alerts, and the bell's note that the tab's title and icon always show news.

Appearance: dark or light.

Keyboard: the KeySheet's list.

## Saving

A value saves in one of three ways, depending on where it lives.

Shared settings (the factory settings, a machine's name and limits including Clear limits, a project's factory switch, login and pause) collect in a review bar at the foot of the page. The bar lists each change as old → new:

> 2 changes · Pause claims off → on · Runs at once 3 → 4 · Discard · Save 2 changes

Save sends today's requests, one per changed item: `POST /api/factory/settings` for the factory settings, `POST /api/projects/{id}/factory` for each changed project, `POST /api/machines/{id}/rename`, and `POST /api/machines/{id}/limits` or `/limits/clear`. Each is built from the newest values the page holds plus the edits, since these routes take whole objects. A change whose request succeeds leaves the bar. A refused one stays, with the daemon's reason beside its field, or beside Save when it names no field. Discard puts back the newest saved values.

Today's range checks run before sending. A changed row is marked until it is saved or discarded. Leaving a page with unsaved changes asks first. Fields follow incoming settings while they are untouched, as `FleetSettings` does now. When an incoming change touches a field being edited, the row says so and the bar asks for a fresh look before Save.

Browser settings (notifications, appearance) save the moment they change, in this browser's storage, as today.

Values kept on one machine, and one-off actions (store a token, make a login the default, replace a key, install an update, add or remove a machine, add or remove a project, move a machine in the priority order), send right away from their own button. They never join the review bar, because a secret has no old value to show and an action has no undo.

## Machine states

A machine's page reads its state from the events state, and locks what it can't change. A pure function `machineAccess(machine, protocols, held)` returns, for each section, whether it is editable and the reason if not. The page and its tests use only that.

| State | In the shared settings | Kept on this machine |
|-------|------------------------|----------------------|
| Online, link speaks protocol 7 or later | Editable | Editable |
| Offline (no open link) | Editable. Name and limits are fleet settings and apply through the coordinator at once | Locked. Logins, default and key flags show "as last reported", under one banner that says why |
| Online, link speaks protocol 6 or earlier | Editable | Locked: "Changing this from here needs a newer Foundry on <machine>." Followed by "Install the update above." only while the Update section shows |

The machine serving the page is always "online, protocol 7".

A store answers as other carried calls do:

- `503` (offline, link down, protocol 6 or earlier): nothing was sent. The row says why.
- `504`, or no answer reaching the browser: "Save could not be confirmed. Check <machine>'s page once it reports again." A presence flag can't confirm a replacement, so the page doesn't claim one.

## Backend

### What a machine reports

`MachineHeld` gains two fields, both `#[serde(default)]`:

- `default: Option<String>`, the machine's default GitHub login.
- `gateway: Option<GatewayKeys>`, with `GatewayKeys { key: bool, management: bool }`. It says whether each key is set and never carries a value.

`None` from an older peer shows as "unknown". The held check in `factory/held.rs` stays the only writer of `MachineHeld`, on its 5-second loop. `MachineSample` gains nothing. A `204` confirms a store, and the page shows the new flags within one held check.

### What reaches the browser

`UiMessage::State` (`foundry-proto/src/messages.rs`) gains, both `#[serde(default)]`:

- `held`: each machine's `MachineHeld`, by machine id.
- `protocols`: for each peer with an open link, the protocol that link speaks, and for this machine, `PROTOCOL`. A machine with no open link has no entry.

The events socket sends a machine's `held` again when its held record changes, and sends `State` again when a link opens or closes, as it does today. The page fetches nothing new.

### Routes

`POST /api/machines/{id}/secrets` with `{ "name": ..., "value": ... }`. The name is `gateway`, `gateway-management` or `github:<login>`, with the CLI's name rules. The body is read as raw bytes, at most 4 KiB, capped before the carry layer buffers it and again at the owner. Unknown fields are refused. The value must be one line, not blank, with no control characters. The handler stores the value through the secret store directly, not through `set_from`, which reads a line from stdin. Success is `204` with no body.

`POST /api/machines/{id}/github/default` with `{ "login": ... }`. A login that fails `is_login` is `400`. A login the machine holds no token for is `409`. It does what `foundryd github default` does. The CLI may still name a default with no token.

Every `400` and `409` on these routes is a fixed sentence that never contains what was sent. Malformed JSON and a store that can't be written get fixed sentences too.

Both routes are carried by `api/carry.rs` to the machine `{id}` names. `needs` in `sync/call.rs` returns `MACHINE_SETTINGS_SINCE = 7` for both paths, so a protocol 6 peer is `503` and the body never crosses its link. `UPDATES_SINCE` stays 6, and the pin `PROTOCOL == UPDATES_SINCE` becomes `PROTOCOL == MACHINE_SETTINGS_SINCE`. A request that reaches a machine whose id is not `{id}` is `421` and stores nothing. Only the owner writes the value. The machine serving the page holds it only in transit.

The factory's `422` that tells the reader to run `foundryd github default` names the machine's settings page too.

### Gateway key at each start

Today the daemon reads the gateway key once at start (`daemon.rs`). When the key is missing it drops `FOUNDRY_GATEWAY_URL` and sets `api.sessions_refused` for the life of the process.

Instead, the daemon keeps `FOUNDRY_GATEWAY_URL` whether or not the key is set. Each manual and factory session start reads the key from the secret store and decides there whether to refuse. A missing URL refuses the start. A missing key refuses it only while it is still missing. `api.sessions_refused` goes.

A running session keeps the key it started with. After a replace, the page says: "Sessions started from now on use the new key. Running sessions keep the old one until they end. Revoke the old key at the gateway."

`foundryd secret set gateway` no longer says a restart is needed.

### Hiding replaced secrets (S8)

A session that is still running can print a key that is no longer in the store, and sessions outlive a daemon restart (`Sessions::adopt`). So:

- Each session writes the gateway key it was given, and a factory session also its `GH_TOKEN`, to a `0600` file in its own folder, beside its curl config and with the same helper. `adopt` reads it back. The file is deleted with the session's other files: when it ends, when its start fails, and when an adopted session's terminal has already exited.
- One hider is used by `hide_known`, by `memory_secrets` (briefs, rename, turn files) and by the factory, API and ledger paths. It hides every value in the store, every value a live session was given, and every value this process replaced since it started. The last set is held in memory only.
- The comment on `Gateway.key` that the key is in no file changes to name this file and its lifetime.
- A session already running when this build first starts has no file. After that first upgrade, every session has one.

### Management key

The health reader (`gateway.rs`) keeps the management key it read. After a successful store of `gateway-management`, the reader's cache goes back to unread when it is `Unreadable` or holds a value different from the one stored. A cache in `Refused` with the same value keeps its backoff, because the gateway bans an address after five refused keys. When the cache was cleared, the page says the next health check uses the new key.

## Security

The access model is P1's (S7). Anyone who can open the UI can already open a shell on any machine through the carried `/shells` route, and read its secret store there. So these routes grant nothing new. Write-only is hygiene: values stay out of the browser, logs, answers and synced records.

What the routes guarantee:

- No route returns a secret value. No response, refusal or log line contains one. Errors go through the hider.
- The carry layer, the peer call (`Call`) and these handlers never log or format a body. `Call` gets a `Debug` that leaves the body out.
- The daemon's cross-site refusal (an `Origin` that is not the `Host`) covers both routes, as it covers every route. Both take JSON only.
- The browser reaches a daemon over Tailscale or loopback. A carried call goes over the authenticated peer link, which Tailscale encrypts.
- The token field is `type="password"` with `autocomplete="new-password"`. The form clears it after a send, whether it succeeded or not. The value never goes into browser storage or a query cache, and a failed send is not retried by itself.

`docs/install.md` says a token never leaves the machine that holds it. That changes to: a token is kept only on the machine that holds it, and passes through the machine serving the page on its way there.

## Testing

Tests first, per `AGENTS.md`.

Web unit tests:

- Nav summaries for each category, from fixtures.
- The review bar's diff: old → new for each changed field, unchanged fields left out, numbers typed as "05" equal to 5.
- The review bar's save: one request per changed item, a refused one stays, a saved one leaves.
- `machineAccess` for each row of "Machine states".
- `coverage(project, held, protocols)`: held on all, on some, on none online, unknown, and no login.

Web component tests:

- Each page renders today's values and sends what today's controls send.
- The review bar appears on the first change, Discard restores the page, and Save sends each change.
- A machine page locks "Kept on this machine" offline and on protocol 6 or earlier, with the reason.
- The token field is empty after a send, success or failure.
- The old spots render their summary and link, not the controls.
- The Composer's draft and a project added in Settings are there on return.

Rust tests:

- A stored value never appears in a response body, a refusal or a log line, raw or hex-encoded, on the local and the carried path.
- Only the owner's store holds the value. The serving machine's store and every record don't.
- Unknown names, unknown fields, blank, multi-line, control-character and oversized values are refused with fixed sentences.
- The default is refused for a login the machine doesn't hold, and set for one it does.
- A store carried to a protocol 6 peer is `503` and is never sent. A request for another machine's id is `421`.
- `MachineHeld` and `UiMessage::State` written before this change still parse, and new ones parse on a protocol 6 peer.
- A daemon started with no gateway key starts a session once the key is stored, with no restart.
- A session started after a replace gets the new key, and one started before keeps the old.
- After a replace and a restart, an adopted session's old key and old token stay out of a brief, a turn file, a ledger event and a factory answer. The session's file is gone when it ends.
- A management-key store clears an `Unreadable` or different cached key, and leaves an unchanged refused one on its backoff.

A two-daemon test: a store reaches only the machine named, a protocol 6 peer receives nothing, and the browser state shows the new flags without the value.

One Playwright journey, in issue 1: open Settings with `⌘,`, change "Runs at once", see the review bar, Save, see the nav summary change.

The gate is the four commands in `AGENTS.md`.

## Issues

| # | Issue | Depends on |
|---|-------|------------|
| 1 | Web: the routes with a slot for every page, the nav with summaries, the review bar (it takes a list of `{label, before, after}` and a save function from the page), the ways in, the shared theme state, the `held` and `protocols` types on the events state, and the Factory, Notifications, Appearance and Keyboard pages. The Factory fold, the bell and the header checklist keep their summary or link. The Playwright journey | none |
| 4a | Rust: the `MachineHeld` fields and `held` and `protocols` on the events state | none |
| 2 | Web: the Projects page with the coverage line. The dialog and the Factory view's project controls move in. The Composer's draft moves above the route and both its links open Settings | 1, 4a |
| 3 | Web: the Machines page and the machine pages without "Kept on this machine": priority, Add machine, update checklist, name, limits, Install, remove. The board, the sidebar rename and the header checklist become links | 1 |
| 4b | Rust: protocol 7 and `needs`, both routes, the gateway key read at each start, the session secrets file and the one hider, the management-key cache rule, the CLI line, and the `install.md` change | 4a |
| 5 | Web: "Kept on this machine" on machine pages, with `machineAccess` and its lock states | 3, 4b |

Issues 1 and 4a start together. 4a and 4b both touch `daemon.rs`, so 4b waits for 4a. The write routes land in the same issue as the hiding that makes Replace safe, never before it. Each issue keeps its new web API calls in its own file, not `api.ts`. Old entry points keep working until the issue that replaces them lands. The plan names each issue's files and the interfaces they share before the issues are filed.
