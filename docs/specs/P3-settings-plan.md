# P3 Settings implementation plan

**Goal:** One Settings page, with categories on the left and a page of settings on the right. It holds every control listed in the spec's "What moves" and stores GitHub tokens and gateway keys on any machine from any machine, and a stored gateway key works with no restart.

**Architecture:** A `/settings` route family in the web app. `Layout` renders it as it renders `/factory`. Each page is its own file, filled by the issue that owns it, inside a shell that issue 1 builds. Shared values save through a review bar that sends today's requests. Two new routes, carried like `/api/machines/{id}/update`, store write-only values on the machine they name. The daemon reads the gateway key at each session start. A session keeps the secrets it started with in a `0600` file for its life, so one hider can still hide them after a restart.

**Tech stack:** as P1 and P2. Rust with tokio, axum, serde. Web: React 19, TanStack Router and Query, Tailwind, Radix, Vitest with Testing Library, Playwright.

**Spec:** [P3-settings.md](P3-settings.md). Each task names the spec sections it implements. The builder reads both.

**How this plan is executed:** each task is one GitHub issue on `original-solutions/foundry`, filed with the `factory-issue` skill and built by the factory. Every issue is linked to the issues it depends on (in brackets after its title). Tasks 1 to 6 are the spec's issues 1, 4a, 2, 3, 4b and 5, in build order. The plan gives each task its files, its interfaces and its acceptance tests, and leaves the code to the builder, which works test-first.

## Global constraints

- P1's and P2's constraints still hold: one binary, Tailscale and localhost only, a machine writes only its own records, secrets in no log, error, record or API response, no `unsafe` without `// SAFETY:`.
- Tests first. No production change without a failing test before it.
- The gate is the one in `AGENTS.md`. Issue 1 also runs `npm --prefix web run e2e`.
- Edit only the files your issue names. Issue 1 makes every new web file the later issues fill.
- New wire fields are `#[serde(default)]` on the Rust side and optional on the TypeScript side. A record or message written before this plan still parses, and so does one from a later build.
- A secret's value is never echoed. Every refusal on the new routes is a fixed sentence.
- `PROTOCOL` becomes 7 in task 5 only. `UPDATES_SINCE` stays 6.
- On a case-insensitive disk `./Settings` and `./settings` are one file (see the comment at `web/src/App.tsx:5`). No new web file pairs a `.tsx` and a `.ts` that differ only in case.
- Web copy follows the spec's wording exactly where the spec quotes it.
- Nothing orders records by wall-clock time.

## Review focus

Conditions the spec implies that are most likely to hurt in use. Each has a test in the task named.

1. A GitHub token or gateway key pasted into the Login field, or a value with a trailing newline, tab or NUL. The route refuses it with a fixed sentence that does not contain it, nothing is stored, and the page shows no part of it (tasks 5 and 6).
2. Two browsers editing the same factory setting. When an incoming change touches a field being edited, the row says so and Save asks for a fresh look rather than sending a stale whole object (task 1).
3. A fleet halfway through an upgrade: a protocol 6 peer, and a held record written by an older build with no `default` or `gateway`. The page shows "unknown" and locks the section with the protocol reason. It never shows "Not set" (tasks 2 and 6).
4. Install on the page restarts `foundryd` while a session started with the old gateway key still runs. The old key stays out of that session's brief, turn files, ledger events and factory answers after the restart (task 5).
5. Leaving Settings with unsaved edits, and coming back to a Composer that had a half-typed task. The leave asks first. The task and the chosen project are still there (tasks 1 and 3).

## File structure

```
crates/foundry-proto/src/factory.rs       MachineHeld gains default and gateway; GatewayKeys
crates/foundry-proto/src/messages.rs      UiMessage::State gains held and protocols; UiMessage::Held; PROTOCOL 7
crates/foundryd/src/
  factory/held.rs                         reads the default login and the key flags
  sync/call.rs                            Peers::protocols(); needs() for the two new routes
  api/mod.rs                              state and events carry held and protocols; sessions_refused goes
  api/carry.rs                            the two routes are carried; a 4 KiB cap for them
  api/machine_settings.rs                 POST /api/machines/{id}/secrets and /github/default
  secrets.rs                              Secrets::hidden, hold, release; Kept; hide_known reads hidden()
  sessions/mod.rs                         the key read at each start; the given-secrets file
  gateway.rs                              Reader::stored
  daemon.rs                               wiring
web/src/
  SettingsPage.tsx                        the shell: nav, page slot, ⌘, key
  settingsNav.ts                          nav entries and summaries, pure
  ReviewBar.tsx                           the review bar and the leave guard
  reviewDiff.ts                           Change, diff(), pure
  SettingsFactory.tsx                     Factory page
  SettingsNotifications.tsx               Notifications page
  SettingsAppearance.tsx                  Appearance page
  SettingsKeyboard.tsx                    Keyboard page
  SettingsProjects.tsx                    Projects page
  coverage.ts                             the coverage line, pure
  composerDraft.ts                        the Composer's draft, kept above the route
  projectApi.ts                           addProject, removeProject, newId (moved from Projects.tsx)
  SettingsMachines.tsx                    Machines page
  SettingsMachine.tsx                     one machine's page
  machineApi.ts                           rename, limits, remove, add machine (moved from Machines.tsx)
  UpdateChecklist.tsx                     the install checklist (moved from UpdateBadge.tsx)
  MachineKept.tsx                         "Kept on this machine"
  machineAccess.ts                        machineAccess(), pure
  machineSecrets.ts                       storeSecret, setDefaultLogin
```

## Wire shapes shared by tasks

Tasks 1 and 2 are built at the same time and must agree on these. The Rust side is in task 2, the TypeScript side in task 1.

```rust
// foundry-proto/src/factory.rs
pub struct MachineHeld {
    pub logins: Vec<String>,
    pub skill: bool,
    #[serde(default)] pub default: Option<String>,
    #[serde(default)] pub gateway: Option<GatewayKeys>,
}
pub struct GatewayKeys { pub key: bool, pub management: bool }

// foundry-proto/src/messages.rs, UiMessage
State { protocol, records, settings, online,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")] held: BTreeMap<MachineId, MachineHeld>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")] protocols: BTreeMap<MachineId, u16> },
Held { protocol: u16, machine: MachineId, held: MachineHeld },
```

```ts
// web/src/api.ts
export interface GatewayKeys { key: boolean; management: boolean }
export interface MachineHeld { logins: string[]; skill: boolean; default?: string | null; gateway?: GatewayKeys | null }
// UiMessage: "state" gains held?: Record<string, MachineHeld>; protocols?: Record<string, number>
//            and a new member { type: "held"; protocol: number; machine: string; held: MachineHeld }
```

On the wire: `{"type":"held","protocol":6,"machine":"<id>","held":{"logins":["os-factory"],"skill":true,"default":"os-factory","gateway":{"key":true,"management":false}}}`.

## Tasks

### 1. Settings shell, Factory, Notifications, Appearance and Keyboard pages []

- **Spec:** Navigation, What moves (Factory fold, bell, theme, KeySheet), Pages (Factory, Notifications, Appearance, Keyboard), Saving, Testing (web).
- **Files:**
  - Create: `web/src/SettingsPage.tsx`, `web/src/settingsNav.ts`, `web/src/ReviewBar.tsx`, `web/src/reviewDiff.ts`, `web/src/SettingsFactory.tsx`, `web/src/SettingsNotifications.tsx`, `web/src/SettingsAppearance.tsx`, `web/src/SettingsKeyboard.tsx`, and their `.test.tsx`/`.test.ts` files.
  - Create as stubs for later tasks, each rendering one line and a link to the old spot (the dialog, the board or `/machines`): `web/src/SettingsProjects.tsx`, `web/src/SettingsMachines.tsx`, `web/src/SettingsMachine.tsx`, and `web/src/MachineKept.tsx`, which renders nothing.
  - Modify: `web/src/App.tsx` (routes, `onSettings`, mount), `web/src/api.ts` (wire types above), `web/src/fleet.ts` (`FleetState.held`, `FleetState.protocols`, `apply` for `state` and `held`), `web/src/theme.ts`, `web/src/ThemeButton.tsx`, `web/src/shortcut.ts`, `web/src/KeySheet.tsx`, `web/src/commands.tsx`, `web/src/AlertsBell.tsx`, `web/src/FactoryLine.tsx`, `web/src/Sidebar.tsx` (gear only), `web/src/Rail.tsx` (gear only), `web/src/test-utils.tsx` (settings routes in `renderApp`), `web/e2e/ui.spec.ts`, `AGENTS.md` (names the P3 spec and this plan beside P1's and P2's), and the tests of each modified file.
- **Produces:**
  - Routes in `App.tsx`: `settingsRoute` `/settings`, `settingsCategoryRoute` `/settings/$category`, `settingsMachineRoute` `/settings/machines/$machine`, all with no component, as today's routes. `Layout` gains `onSettings` (path starts with `/settings`), excluded from `firstRun` and `board` as `onFactory` is, and mounts `<SettingsPage notify={notify} machines={machines} here={here} state={state} />` in the pane. `/settings` shows Factory. An unknown category shows Factory.
  - `type Category = "factory" | "projects" | "machines" | "notifications" | "appearance" | "keyboard"`.
  - `interface PageProps { state: FleetState; machines: MachineView[]; here: string | undefined; notify: { notify: Notify; desktop: DesktopStatus; onChoose: (change: Partial<Notify>) => void } }`, exported from `SettingsPage.tsx`. Every page takes `PageProps`. `SettingsMachine` also takes `machineId: string`. The stubs have these signatures, and `MachineKept({ machine, held, protocols, here }: { machine: MachineView; held?: MachineHeld; protocols: Record<string, number>; here?: string })` renders nothing.
  - `settingsNav.ts`: `navEntries(input: NavInput): NavGroup[]` with `NavGroup { title: "Fleet" | "This browser"; entries: NavEntry[] }` and `NavEntry { category: Category; label: string; summary?: string; machines?: { id: string; name: string; chip?: "offline" | "update ready" | "coordinator" }[] }`. `NavInput { factory?: FactorySettings; projects: Project[]; projectFactory: Record<string, ProjectFactory>; machines: MachineView[]; notify: Notify; theme: Theme }`. Factory's summary is `settingsLine`. A machine's chip is the first that applies of offline, update ready (`sample.update_ready`), coordinator (`acting`).
  - `SettingsPage({ notify, machines, here, state })`: the nav as `<nav aria-label="Settings">` with "← Board", the page for the route, the narrow layout below 720px.
  - `reviewDiff.ts`: `interface Change { key: string; label: string; before: string; after: string }`, `interface Refusal { key?: string; message: string }`, `diff<T>(saved: T, draft: T, fields: Field<T>[]): Change[]` with `Field<T> { key: keyof T & string; label: string; show?: (value: T[keyof T]) => string }`. Numbers compare as numbers, so "05" equals 5.
  - `ReviewBar({ changes, onSave, onDiscard, stale })`, with `onSave: () => Promise<Refusal[]>`. It is hidden while `changes` is empty and reads "{n} changes · {label} {before} → {after} · Discard · Save {n} changes". After a save it shows each refusal beside its key, or beside Save when it names none. `stale: string[]` names fields changed elsewhere since they were edited. While it is not empty, Save reads "Look again" and asks once more. `useLeaveGuard(dirty: boolean)` uses TanStack Router's `useBlocker` to ask before leaving a page with unsaved changes.
  - `SettingsFactory`: today's `FleetSettings` fields and checks, with the review bar in place of its Save button. Values come from `factoryOverview` under `factoryKey`, as the Factory view reads them. Save runs `checkSettings` and then one `saveFactorySettings`, built from the newest overview plus the edits.
  - `SettingsNotifications`: today's three `Choice`s and the "Always on" note, wired to `notify.onChoose`.
  - `theme.ts` gains `useTheme(): Theme` and `setTheme(theme: Theme)` on `useSyncExternalStore` with module listeners, as `home.ts` does. `ThemeButton` and `SettingsAppearance` both use them.
  - `KeySheet.tsx` exports `keyGroups` (today's `groups`) and adds `[[mod, ","], "Settings"]` to "Anywhere". `typing(target)` moves to `shortcut.ts` and is exported. `useHelpKey` imports it. `SettingsKeyboard` renders `keyGroups`.
  - `shortcut.ts`: `useSettingsKey(run: () => void)` fires on `,` with Meta (Ctrl off macOS) and no other modifier, only when `!typing(event.target)`.
  - `commands.tsx`: `screen` gains `"settings"`. A "Settings" command in "Go to", and one per category in "Do" titled "Settings: {label}".
  - `NotifyButton` becomes a link to `/settings/notifications` with today's icon and `aria-label="Notifications"`. `SettingsFold` becomes a link to `/settings/factory` that keeps its summary line. A gear `Link` with `aria-label="Settings"` goes beside `ThemeButton` in the sidebar header and the rail foot.
- **Tests:**
  - `settingsNav.test.ts`: each category's summary from fixtures, and the chip order for a machine that is offline and has an update ready (offline wins).
  - `reviewDiff.test.ts`: only changed fields are listed, "05" against 5 is unchanged, and `show` formats both sides.
  - `ReviewBar.test.tsx`: hidden with no changes. Discard calls `onDiscard`. Save lists each refusal beside its key and one with no key beside Save. A non-empty `stale` makes Save ask again before calling `onSave`.
  - `SettingsFactory.test.tsx`: the first edit shows the bar. Save sends one `POST /api/factory/settings` with the whole object. A refusal stays beside its field. An overview that changes "Max runs" while it is being edited marks that row and makes Save ask again (review focus 2). Untouched fields follow the overview.
  - `App.test.tsx`: `/settings` shows Factory. `⌘,` opens Settings from the board, but not while a text field or terminal has focus. The palette's "Settings" goes there. Leaving with an unsaved edit asks first, and Cancel stays (review focus 5). The attention strip shows on `/settings` as it does on `/factory`.
  - `theme.test.tsx`: flipping `ThemeButton` changes Appearance at once, and the other way round.
  - `AlertsBell.test.tsx` and `FactoryLine.test.tsx`: the bell and the fold render links, not their controls. `fleet.test.ts`: `apply` keeps `held` and `protocols` from `state` and replaces one machine's held on `held`. A `state` without them gives empty maps.
  - `e2e/ui.spec.ts`: a journey that opens Settings with `⌘,` (Ctrl on Linux), changes "Runs at once" from 3 to 4, sees "Runs at once 3 → 4" in the bar, saves, and sees "4 runs at once" in the nav summary.

### 2. What a machine holds reaches the browser []

- **Spec:** Backend → What a machine reports, What reaches the browser.
- **Files:** `crates/foundry-proto/src/factory.rs`, `crates/foundry-proto/src/messages.rs`, `crates/foundryd/src/factory/held.rs`, `crates/foundryd/src/sync/call.rs`, `crates/foundryd/src/api/mod.rs`, `crates/foundryd/src/status.rs`. Each site that builds `MachineHeld` or `UiMessage::State` by struct literal gains the new fields (`..Default::default()` or `held: BTreeMap::new(), protocols: BTreeMap::new()`): `crates/foundryd/src/store/tests.rs`, `crates/foundryd/src/factory/worker.rs`, `crates/foundryd/src/factory/placing.rs`, `crates/foundryd/src/factory/pass.rs`, `crates/foundryd/src/factory/runs/gating.rs`, `crates/foundryd/src/factory/runs/feedback.rs`, `crates/foundryd/src/api/tests.rs`, `crates/foundryd/tests/fleet/factory/mod.rs`, `crates/foundryd/tests/daemon.rs`.
- **Produces:**
  - The wire shapes above. `MachineHeld` keeps `Default`.
  - `held::check` reads `secrets::github_default(secrets)` into `default`, and reads `secrets.names()` for whether `gateway` and `gateway-management` are present into `gateway`. It reads no value. A name list it cannot read leaves `gateway: None`. The 5-second loop is still the only writer.
  - `Peers::protocols(&self) -> BTreeMap<MachineId, u16>`: the protocol each open link speaks (the highest per machine, as `speaks` reads it), plus this machine at `PROTOCOL`.
  - `full_state_for` fills `held` from `store.helds()` and `protocols` from `api.peers.protocols()`. `send_events` sends `UiMessage::Held` for each machine whose held record differs from what it last sent, on the tick where it already reads `factory_versions`. It sends `State` again when `protocols()` differs from what it sent, as it does for `online`. Held values go out as they are, because they hold no secret.
  - `status.rs` builds `State` with the new fields.
- **Tests:**
  - Proto: a `MachineHeld` written before this task (`{"logins":["a"],"skill":false}`) parses with `default` and `gateway` as `None`. A new one round-trips. A `UiMessage::State` with no `held` or `protocols` parses to empty maps. `UiMessage::Held` matches the JSON line above.
  - `held.rs`: after `github default` names a login, the next check writes it. After `gateway-management` is set, `gateway.management` is true within one check. No log line holds a token or key value (`test_log`).
  - `sync/call.rs`: `protocols()` names a peer on a link whose hello said 5 as 5, and leaves out a machine with no link.
  - `api/tests.rs`: the first events frame holds `held` and `protocols`. A held change arrives as one `held` frame. A link opening sends `State` again with the new `protocols` (review focus 3).

### 3. Projects page and the coverage line [1, 2]

- **Spec:** What moves (Factory view's project controls, Projects dialog, Composer), Pages → Projects, Saving.
- **Files:**
  - Fill: `web/src/SettingsProjects.tsx`.
  - Create: `web/src/coverage.ts`, `web/src/composerDraft.ts`, `web/src/projectApi.ts`, and their tests.
  - Modify: `web/src/Composer.tsx`, `web/src/FirstRepo.tsx` (imports only), `web/src/Factory.tsx`, `web/src/FactorySettings.tsx` (`ProjectFactoryControls` is removed once nothing uses it), `web/src/NewTask.tsx`, `web/src/Board.tsx` (Composer props only), `web/src/Launchpad.tsx` (Composer props only), and the tests of each.
  - Delete: `web/src/Projects.tsx` and `web/src/Projects.test.tsx`.
- **Produces:**
  - `projectApi.ts`: `addProject` and `newId` moved unchanged from `Projects.tsx`, and `removeProject` exported.
  - `coverage(login: string, held: Record<string, MachineHeld>, machines: MachineView[]): Coverage | null`, with `Coverage { holding: string[]; missing: string[]; unknown: string[]; none: boolean }`. It returns null for a blank login. Matching is case-sensitive, as `placing.rs` `not_held` is. A machine with no held record is `unknown`. `none` is true when no online machine holds it.
  - `SettingsProjects`: a block per project, with its repo, factory switch, login and pause in the review bar, then Remove and the Add form, both immediate. Save sends one `setProjectFactory` per changed project, built from the newest `FactoryContext` value plus the edits. Under the saved login it shows the coverage line in the spec's wording, amber when `none`. Each block has `id="project-{id}"` so a link can open it.
  - `composerDraft.ts`: `useComposerDraft(id: string): [Draft, (change: Partial<Draft>) => void]` with `Draft { task; name; base; model; projectId; machine }`, kept in a module store keyed by the Composer's `id` and cleared when a task starts. `Composer` reads its draft from it in place of its `useState`s, and both its project links navigate to `/settings/projects`. A project added from Settings during that visit becomes the chosen project, through `setComposerProject(id)`, which sets `projectId` on every kept draft and which `SettingsProjects` calls after an add.
  - `Factory.tsx`: each project's controls become its status and a link to `/settings/projects#project-{id}`. The empty-state copy says to add a project in Settings → Projects.
- **Tests:**
  - `coverage.test.ts`: held on all, on some, on none online, unknown, a blank login, and case (`OS-Factory` does not match `os-factory`).
  - `SettingsProjects.test.tsx`: the switch and login join the bar. Save sends one request per changed project. A refused project stays with its reason. Add and Remove send at once. The coverage line reads as the spec says and turns amber with no online holder. A machine whose held record has no `gateway` and no `default` is "unknown", not "not on" (review focus 3).
  - `Composer.test.tsx`: a typed task and a chosen project survive going to `/settings/projects` and back, and a project added there is chosen on return (review focus 5).
  - `Factory.test.tsx`: project sections show the link, not the controls.

### 4. Machines page and machine pages [1]

- **Spec:** What moves (machines board, sidebar rename, header checklist), Pages → Machines and a machine's page sections 1, 2 and 4, Machine states (shared column), Saving.
- **Files:**
  - Fill: `web/src/SettingsMachines.tsx`, `web/src/SettingsMachine.tsx`.
  - Create: `web/src/machineApi.ts`, `web/src/UpdateChecklist.tsx`, and their tests.
  - Modify: `web/src/Machines.tsx`, `web/src/UpdateBadge.tsx`, `web/src/Sidebar.tsx`, `web/src/Rail.tsx` (machine dot link only), and their tests.
- **Produces:**
  - `machineApi.ts`: `renameMachine`, `sendLimits` with `LimitsAction`, `removeMachine`, `addMachine` and `limitsText`, moved unchanged from `Machines.tsx`. `movePriority(order: string[])` posts `/api/machines/priority`.
  - `UpdateChecklist`: today's dialog body from `UpdateBadge`, with the same ordering (the serving machine last), row texts (`rowSays`) and restart state, as a component with no dialog around it. `UpdateBadge` keeps its pill and becomes a link to `/settings/machines`.
  - `SettingsMachines`: the priority order with ↑/↓, the "Add machine" dialog, and `UpdateChecklist`.
  - `SettingsMachine({ machineId, ...page }: PageProps & { machineId: string })` for `/settings/machines/$machine`, in the spec's section order:
    - Update: only while `sample.update_ready` is set. Install sends `installUpdate(id, tag)` and shows the same row text. On the serving machine it first says that the machine restarts.
    - In the shared settings: name and limits in the review bar. Clear limits is a change shown as "limits … → none". Save sends `renameMachine` and `sendLimits` per change.
    - `<MachineKept machine={machine} held={state.held[machine.id]} protocols={state.protocols} here={here} />`, which task 6 fills.
    - Remove: today's two-step dialog, not on the serving machine.
    - An unknown machine id shows "No machine {id} in this fleet" and a link to Settings → Machines.
  - `Machines.tsx`: the board's cards lose the limits dialog, the priority buttons, the remove dialog, the click-to-rename and Add machine. Each card gets a "Settings" link to `/settings/machines/{id}`. Status, version, the next-task head and "New task here" stay.
  - `Sidebar.tsx`: the rename pencil goes, and the machine name links to `/settings/machines/{id}`.
- **Tests:**
  - `SettingsMachine.test.tsx`: name and limits join the bar and save with one request each. Clear limits sends `/limits/clear`. While the machine is offline the shared section stays editable and saves. Install shows "Restarting" on a `202`. Remove is absent on the serving machine.
  - `UpdateChecklist.test.tsx`: today's `UpdateBadge` cases, moved.
  - `Machines.test.tsx`: cards show the Settings link and no config controls. `Sidebar.test.tsx`: no rename control, and the name links to the machine's page.

### 5. Stored from any machine, and used at the next start [2]

- **Spec:** Backend → Routes, Gateway key at each start, Hiding replaced secrets (S8), Management key; Security; Testing (Rust).
- **Files:**
  - Create: `crates/foundryd/src/api/machine_settings.rs`.
  - Modify:
    - `crates/foundry-proto/src/messages.rs`: `PROTOCOL = 7` and `MACHINE_SETTINGS_SINCE = 7`. The pin `PROTOCOL == UPDATES_SINCE` becomes `PROTOCOL == MACHINE_SETTINGS_SINCE`, and a `Debug` for `Call` leaves out `body`.
    - `crates/foundryd/src/sync/call.rs`: `needs`, and `Frame`'s `Debug` leaves out `body`.
    - `crates/foundryd/src/api/carry.rs`, `crates/foundryd/src/api/mod.rs` (the module doc's route list, and removing `sessions_refused`) and `crates/foundryd/src/api/factory.rs` (the start refusal and the 422 text).
    - `crates/foundryd/src/secrets.rs`, `crates/foundryd/src/gateway.rs` and `crates/foundryd/src/daemon.rs`.
    - `crates/foundryd/src/sessions/mod.rs`, `crates/foundryd/src/sessions/turns.rs`, `crates/foundryd/src/sessions/factory.rs` and `crates/foundryd/src/sessions/claude.rs`.
    - Tests: `crates/foundryd/src/sessions/tests.rs`, `crates/foundryd/src/api/tests.rs`, `crates/foundryd/tests/fleet/call.rs`, `crates/foundryd/tests/fleet/update.rs`, `crates/foundryd/tests/fleet/main.rs` (register `mod machine_settings`), a new `crates/foundryd/tests/fleet/machine_settings.rs`, `crates/foundryd/tests/daemon.rs` and `crates/foundryd/tests/cli.rs`.
    - Docs: `docs/install.md`.
- **Produces:**
  - **Secrets** (`secrets.rs`):
    - The `Secrets` trait gains `fn hidden(&self) -> Result<Vec<String>, SecretsError>`. Its default is today's `known_values`.
    - It also gains `fn hold(&self, holder: &str, values: Vec<String>)` and `fn release(&self, holder: &str)`, both defaulting to doing nothing.
    - `Kept<S: Secrets>` wraps a store. `set` and `update` remember the value they replace, for this process only. `hold` and `release` keep the values a live session was given, by session id. `hidden()` is the inner store's values plus both of those lists.
    - `hide_known` reads `secrets.hidden()` in place of `known_values`.
    - `pub fn store_value(secrets: &dyn Secrets, name: &str, value: &str) -> Result<(), StoreError>` applies the name rules of `set_from` and the value rules of the spec (not blank, one line, no control characters), then `set`.
    - `StoreError::{UnknownName, Refused, Secrets}`, each with fixed text.
    - `set_from` calls `store_value` after reading its line.
    - The success line of `set_command` for the gateway names no restart.
  - **Daemon wiring** (`daemon.rs`): the store is wrapped once, `Arc::new(Kept::new(...))`, and every holder gets that one `Arc`. `gateway(url, key)` and `Missing` move to `sessions` as `pub(crate)`.
  - **Gateway key at each start** (`sessions`):
    - `Config.gateway` becomes `gateway_url: Option<String>`.
    - `Inner::start` reads `GATEWAY` from the store and builds the `Gateway` there. When the URL or the key is missing it returns `SessionError::Refused(said)` with today's `said` text, and starts nothing.
    - `launch` takes that `Gateway`.
    - `Api.sessions_refused` goes. The three start paths answer `SessionError::Refused` with `503` and the text.
    - The test fakes that set `sessions_refused` get the same effect from a store with no gateway key.
  - **Given-secrets file** (`sessions`):
    - `files(id)` returns a third path, `{id}.given`. `Inner::start` writes the gateway key, and a factory start also its `GH_TOKEN`, as JSON with `claude::write_new_private` (`0600`), then calls `hold(id, values)`.
    - `take_up` reads the file back and holds it. `remove_files` and `discard` delete it and call `release`.
    - `memory_secrets` includes each live session's given values. The `Gateway.key` comment names the file and its lifetime.
    - A session adopted with no given file (started before this build) holds nothing more.
  - **Management key** (`gateway.rs`): `Reader::stored(&self, value: &str) -> bool` puts the cache back to `Unread` when it is `Unreadable`, or when it holds a different value in `Held` or `Refused`. It leaves an equal `Refused` on its backoff, and returns whether it cleared. The attention texts stop saying to restart `foundryd`.
  - **Routes** (`api/machine_settings.rs`): `router(api: Api, reader: Arc<gateway::Reader>) -> Router` for `POST /api/machines/{id}/secrets` and `POST /api/machines/{id}/github/default`. Each:
    - parses raw `Bytes` with `deny_unknown_fields`;
    - answers `421` when `{id}` is not this machine;
    - stores through the shared `Kept`;
    - answers `204`.
  - **Answers:**
    - A `gateway-management` store calls `reader.stored(value)` and answers `{"health_check": "next"}` with `200` when it cleared the cache.
    - The default route answers `400` for a login that fails `is_login` and `409` when `github:<login>` is not held, both fixed text, then stores `GITHUB_DEFAULT`.
    - Malformed JSON and a store that fails get fixed sentences.
  - **Carrying:**
    - `carry.rs`: `target` arms for both paths as `Target::Machine(id)`. Their body is buffered with a 4 KiB limit in place of `BODY_LIMIT`, and a longer one is `413` with a fixed sentence.
    - `needs` returns `MACHINE_SETTINGS_SINCE` for both.
    - `daemon.rs` merges the router behind `same_site`, with the carry layer inside, as `machine_update` is merged.
  - **Copy:**
    - `api/factory.rs`'s 422 names "the coordinator's settings page" beside `foundryd github default <login>`.
    - `docs/install.md:144` reads: "A token is kept only on the machine that holds it. One stored from the Settings page passes through the machine serving the page on its way there."
- **Tests:**
  - `secrets.rs`: `store_value` refuses an unknown name, blank, `"a\nb"`, `"a\tb"`, `"a\0b"` and a token pasted as a login name (`github:ghp_x`), and no refusal text contains the value (review focus 1). `Kept::hidden` includes a value replaced since start and a held value until it is released. `hide_known` hides a replaced value.
  - `gateway.rs`: `stored` clears `Unreadable` and a different `Held`, and keeps an equal `Refused` on its backoff.
  - `sessions/tests.rs`:
    - a start with no key is refused with today's text, and one after the key is stored starts;
    - a session started before a replace keeps the old key and one after gets the new;
    - the given file is `0600` and is gone when the session ends and when the start fails;
    - after `adopt` in a new `Inner`, the old key and `GH_TOKEN` are hidden in a brief and a turn file (review focus 4).
  - `api/tests.rs`: `204` for a store, `421` for another id, `400` for each bad value with fixed text, `409` for an unheld default, `413` over 4 KiB, and `200` `health_check` after a management-key store that cleared the cache. No answer and no log line contains the value.
  - `tests/fleet/machine_settings.rs`:
    - a store from machine A naming B reaches only B's store, and A's store and every record lack the value;
    - a call naming a protocol 6 peer is `503` and is never sent, with no dial;
    - the log of both daemons holds neither the value nor its hex.
  - `tests/daemon.rs`: a daemon started with `FOUNDRY_GATEWAY_URL` and no key starts a session once `POST /api/machines/{me}/secrets` stores the key, with no restart.
  - Proto: `Call`'s `Debug` holds no body. `PROTOCOL == MACHINE_SETTINGS_SINCE == 7`.

### 6. Kept on this machine [4, 5]

- **Spec:** Pages → a machine's page section 3, Machine states, Saving (immediate values), Security (the token field).
- **Files:** Fill `web/src/MachineKept.tsx`. Create `web/src/machineAccess.ts`, `web/src/machineSecrets.ts` and their tests.
- **Produces:**
  - `machineAccess(machine: MachineView, protocols: Record<string, number>, here: string | undefined): Access` with `Access { shared: { editable: true }; kept: { editable: boolean; reason?: "offline" | "older" } }`.
    - The machine serving the page is always editable.
    - With no `protocols` entry, the reason is `offline`.
    - With an entry below 7, it is `older`.
  - `machineSecrets.ts`: `storeSecret(machine: string, name: string, value: string): Promise<Stored>` and `setDefaultLogin(machine: string, login: string): Promise<void>`. `Stored` is `{ healthCheckNext: boolean }`.
    - They post to the two routes.
    - They throw `ApiError` with the daemon's text.
    - They never retry.
    - A `504` or a dropped answer is `Unconfirmed`.
  - `MachineKept({ machine, held, protocols, here })`:
    - **GitHub logins:** each held login, with "default" marked. A default with no token reads "default, no token held". There is "Make default" on any other held login, and "Add token" with Login and Token fields (`type="password"`, `autocomplete="new-password"`).
    - **Keys:** "Gateway key" and "Gateway management key", each "Set", "Not set" or "unknown", with Replace or Set and one password field.
    - **When the section is locked:** each row shows "as last reported" under one banner. The banner says why, in the spec's words. "Install the update above." appears only while an update is ready.
    - **After a send:**
      - The value field is cleared whatever the answer, and the value is kept in no query cache.
      - After a gateway-key store the page shows the spec's sentence about running sessions.
      - After a management-key store that answered `healthCheckNext`, it shows "The next health check uses the new key."
      - An unconfirmed send shows "Save could not be confirmed. Check {name}'s page once it reports again."
- **Tests:**
  - `machineAccess.test.ts`: each row of "Machine states", and the serving machine with no entry.
  - `MachineKept.test.tsx`:
    - offline and protocol 6 lock the section with the right banner;
    - Add token sends one request and clears both fields on success and on a `400`;
    - the `400` text shows and the typed token is nowhere in the DOM after (review focus 1);
    - a held record with no `gateway` shows "unknown" (review focus 3);
    - a `504` shows the unconfirmed text;
    - Make default sends `setDefaultLogin`.

## Notes against the spec

- The spec puts the header checklist in issue 1's row and in issue 3's. It belongs to task 4, which builds the Machines page it moves to.
- The spec says the daemon "drops `FOUNDRY_GATEWAY_URL`" when the key is missing. The code builds an empty `Gateway` instead. Task 5 replaces both with the read at each start, so the result is what the spec asks for.
- The spec's nav sample shortens the factory summary. `settingsLine` is longer, and the nav cuts it with an ellipsis and shows it whole in a `title`.
