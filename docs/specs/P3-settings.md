# P3: Settings

Product: Foundry, a dark software factory that also runs manual sessions.

Status: design approved in chat on 2026-11-28. Nothing here is built. The plan comes next, then the factory issues listed at the end.

## Why

Settings are spread over five places. The factory's settings sit in a fold on the Factory view. Projects and their factory controls are in a dialog opened from the Composer. Machine limits, priority, rename, add and remove are mixed into the machines board with live status. Notification choices live in the alerts bell's popover. The theme is a toggle in the sidebar.

Some settings have no UI at all. A machine's GitHub tokens, its default GitHub login and its gateway keys can only be changed by running `foundryd` on that machine. When the factory reports that no machine holds a project's login, the fix today is to SSH into a machine.

## Goal

One Settings page, with categories on the left and a page of settings on the right. It manages every setting Foundry has, for every machine in the fleet, from a browser on any of them.

Done when every control listed under "What moves" works from Settings, the old spots hold only a summary and a link, and a GitHub token or gateway key can be stored on another machine from the UI.

## Scope

In:

- A `/settings` route with grouped categories, a summary of each category's values in the nav, and a review bar for changes to shared settings.
- Moving the existing controls listed under "What moves" into Settings.
- A page per machine, holding its shared settings, its waiting update, and what it keeps in its own secret store.
- Storing a GitHub token, a gateway key or a gateway management key on any machine from any machine, and naming a machine's default GitHub login. All write-only.
- Showing on each project which machines hold its GitHub login.

Out:

- Removing a token, token expiry and token scopes.
- One action that applies a value to every machine at once.
- Search across settings, and an overlay mode for Settings.
- Install-time configuration (`FOUNDRY_PORT`, `FOUNDRY_HOME`, `FOUNDRY_GATEWAY_URL`). It stays in the service's environment.

## Decisions

| # | Topic | Decision |
|---|-------|----------|
| S1 | Layout | Categories on the left in two groups, Fleet and This browser. A page of rows on the right: label and help on the left of each row, control on the right |
| S2 | Nav summaries | Each nav entry has a second line with its current values, built from state the page already holds. Nothing extra is fetched for it |
| S3 | Machines | A page per machine, listed under Machines in the nav. The Machines page itself holds only what spans the fleet |
| S4 | Login coverage | Each project row in Projects says which machines hold its login |
| S5 | Saving | Three rules, by where a value lives. See "Saving" |
| S6 | Old spots | Consolidate. Each old spot keeps a one-line summary or a link to its Settings page, not a second copy of the controls |
| S7 | Access | The same as P1: anyone on the tailnet who can open the UI can change settings, as they can already add or remove a machine |

## Navigation

Routes:

- `/settings`, which opens the first category, Factory.
- `/settings/$category`, where the category is `factory`, `projects`, `machines`, `notifications`, `appearance` or `keyboard`.
- `/settings/machines/$machine`, where `$machine` is the machine's id.

Ways in:

- A gear button at the foot of the sidebar and of the rail, beside the theme button.
- A "Settings" entry in the jump palette, and one entry per category.
- `⌘,` (Ctrl+, off macOS), unless the focus is in a terminal or a text field.

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

## What moves

| Today | Moves to | What stays behind |
|-------|----------|-------------------|
| Factory view's "Settings for every project" fold (`SettingsFold`, `FleetSettings`) | Settings → Factory | The fold's summary line, as a link to Settings → Factory |
| Projects dialog (`Projects.tsx`), opened from the Composer | Settings → Projects | The Composer's links that open the dialog open `/settings/projects` |
| Machines board: limits dialog, priority order, rename, Add machine, remove, install update | Settings → Machines and each machine's page | The board keeps status, sessions, load and Start task. Each machine card gets a "Settings" link to that machine's page |
| Alerts bell popover's notification choices (`NotifyButton`) | Settings → Notifications | The bell keeps its alerts and gets a link to Settings → Notifications |
| Theme toggle | Settings → Appearance mirrors it | The toggle stays where it is |
| KeySheet | Settings → Keyboard, read-only | `?` still opens the sheet |

## Pages

Factory: pause claims, model, effort, runs at once, gates at once, the poll interval (10 to 60 seconds) and the feedback interval, with today's checks.

Projects: one block per project with its repo, factory switch, GitHub login, pause and Remove, then the Add project form, with today's behaviour. Each project whose factory is on gets a coverage line under its login:

> os-factory · held on personal-mac, studio-mini · not on build-box

The line comes from the `MachineHeld` records already synced. Each machine name links to that machine's page. When no online machine holds the login, the line is amber and reads "No online machine can build this project".

Machines: the priority order with up and down buttons, and Add machine, both as today.

A machine's page, with the same sections in the same order for every machine:

1. Update. Shown only while a release is ready on the machine: "v0.11.2 is ready", and Install. It uses `POST /api/machines/{id}/update`.
2. In the shared settings. Name, and placement limits.
3. Kept on this machine.
   - GitHub logins: each login the machine holds, by name, with the default marked. "Make default" on any other held login. "Add token" takes a login and a token. Adding a login the machine already holds replaces its token.
   - Gateway key and gateway management key, each "Set" or "Not set", with Replace or Set.
4. Remove from the fleet, which asks once more. It isn't offered on the machine serving the page, as today.

Notifications: the three choices the bell has today (cards, desktop alerts, catch-up), with the browser's permission state beside desktop alerts.

Appearance: dark or light.

Keyboard: the KeySheet's list.

## Saving

A value saves in one of three ways, depending on where it lives.

Shared settings (the factory settings, a machine's name and limits, a project's factory switch, login and pause) collect in a review bar at the foot of the page. The bar lists each change as old → new:

> 2 changes · Pause claims off → on · Runs at once 3 → 4 · Discard · Save 2 changes

Save sends them through the acting coordinator, as today. Today's range checks run before sending, and the daemon's refusals show beside the field they name. A changed row is marked until it is saved or discarded. Leaving a page with unsaved changes asks first. Fields follow incoming settings while they are untouched, as `FleetSettings` does now.

Browser settings (notifications, appearance) save the moment they change, in this browser's storage, as today.

Values kept on one machine, and one-off actions (store a token, make a login the default, replace a key, install an update, add or remove a machine, move a machine in the priority order), send right away from their own button. They never join the review bar, because a secret has no old value to show and an action has no undo.

## Machine states

A machine's page reads its state from the records it already gets, and locks what it can't change:

| State | In the shared settings | Kept on this machine |
|-------|------------------------|----------------------|
| Online, speaks protocol 7 | Editable | Editable |
| Offline | Editable. The machine takes the change when it is back | Locked. Values show "as of" the machine's last sample, under one banner that says why |
| Online, older protocol | Editable | Locked: "Changing this from here needs a newer Foundry on <machine>. Install the update above." |

A pure function `machineAccess(machine)` returns, for each section, whether it is editable and the reason if not. The page and its tests use only that.

A carried store answers with the owner's status, as other carried calls do:

- `503` (offline, link down, older Foundry): nothing was sent. The row says why.
- `504` (the link dropped after sending): "May have been stored on <machine>. Its page will show it once the machine reports again."

## Backend

### What a machine reports

`MachineHeld` gains `default: Option<String>`, the machine's default GitHub login. The held check writes it whenever it changes, with the logins.

`MachineSample` gains `gateway_keys: Option<GatewayKeys>`, with `GatewayKeys { key: bool, management: bool }`. It says whether each key is set and never carries a value. `None` from an older peer shows as "unknown".

Both fields use `#[serde(default)]`, as `version` and `update_ready` did, so records from older peers still parse.

After a store from the UI, the machine writes its held record and its sample at once, so the page doesn't wait for the next periodic check.

### Routes

`POST /api/machines/{id}/secrets` with `{ "name": ..., "value": ... }`. The name is `gateway`, `gateway-management` or `github:<login>`. It runs `set_from`'s checks, with the value taken from the body in place of stdin, and answers `204` with no body. An unknown name or an empty value is `400`, and the reason never contains the value.

`POST /api/machines/{id}/github/default` with `{ "login": ... }`. A login the machine holds no token for is refused with `409` and the reason. It does what `foundryd github default` does.

Both are carried by `api/carry.rs` to the machine `{id}` names, as `/api/machines/{id}/update` is. `PROTOCOL` becomes 7, and `MACHINE_SETTINGS_SINCE = 7` gates the carry. A machine on protocol 6 is `503` before anything is sent.

### Gateway key without a restart

The daemon reads the gateway key once at start (`daemon.rs`). It will read it from the secret store each time it starts a session instead, so a replaced key reaches every session started after the replace. Running sessions keep the key they started with. After a replace, the page says so: "Sessions started from now on use the new key. Running sessions keep the old one until restarted."

## Security

The access model is P1's (S7). What keeps it safe:

- No route returns a secret value, and no response, refusal or log line contains one. Errors go through `redact` and `hide_known`.
- Neither the carry layer nor these handlers log request bodies. A test with `test_log` proves the value never appears.
- The daemon's cross-site refusal (an `Origin` that is not the `Host`) covers both routes, as it covers every route.
- A carried call goes over the authenticated peer link, as other carried calls do.
- The token field is `type="password"` with `autocomplete="off"`, and the form clears it after a send, whether it succeeded or not.

## Testing

Tests first, per `AGENTS.md`.

Web unit tests:

- Nav summaries for each category, from fixtures.
- The review bar's diff: old → new for each changed field, unchanged fields left out, numbers typed as "05" equal to 5.
- `machineAccess` for each row of "Machine states".
- `coverage(project, held, online)`: held on all, on some, on none online, and no login set.

Web component tests:

- Each page renders today's values and sends what today's controls send.
- The review bar appears on the first change, Discard restores the page, and Save sends one change.
- A machine page locks its "Kept on this machine" section offline and on an older protocol, with the reason.
- The token field is empty after a send, success or failure.
- The old spots render their summary and link, not the controls.

Rust tests:

- A stored value never appears in a response body, a refusal or a log line.
- An unknown name and an empty value are refused.
- The default is refused for a login the machine doesn't hold, and set for one it does.
- A store carried to a protocol 6 peer is `503` and is never sent.
- A `MachineHeld` and a `MachineSample` written before this change still parse.
- A session started after a replace gets the new key, and one started before keeps the old.

One Playwright journey: open Settings with `⌘,`, change "Runs at once", see the review bar, Save, see the nav summary change.

The gate is the four commands in `AGENTS.md`.

## Issues

| # | Issue | Depends on |
|---|-------|------------|
| 1 | Settings shell: the routes, the nav with summaries, the review bar, the ways in, and the Factory, Notifications, Appearance and Keyboard pages. The Factory fold and the bell keep a link | none |
| 2 | Projects page: the dialog and the per-project factory controls move in, with the coverage line. The Composer opens `/settings/projects` | 1 |
| 3 | Machines page and machine pages: priority, Add machine, name, limits, update and remove move in. The board keeps status and links each card to its page | 1 |
| 4 | Backend: `MachineHeld.default`, `MachineSample.gateway_keys`, the two carried routes, protocol 7, and the gateway key read at each session start | none |
| 5 | "Kept on this machine" on machine pages, with its lock states | 3, 4 |

Issues 1 and 4 can be built at the same time.

## To check before the plan is written

1. Whether anything besides `daemon.rs` keeps the gateway key from start, such as the gateway health check or the briefs' redaction. Each place needs to read the current key.
2. Whether `hide_known` reads the secret store each time or keeps the values it saw. A replaced token has to be hidden too.
3. Whether the Composer's two links to the Projects dialog (`Composer.tsx`, `setManaging(true)`) are used on the first-run screen, where leaving the page would lose a half-typed task.
