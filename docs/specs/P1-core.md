# P1: Core

Product: Foundry, a dark software factory that also runs manual sessions.

Status: built and running on one machine, except joining a fleet, machine limits and removing a machine, which are being built. The acceptance run (plan task 19) comes after them. To install it, see [install.md](../install.md).

## Why

Three factories came before this one. Rust softfac drove a separate T3 Code server over an undocumented socket. t3fac moved the factory inside a fork of T3 Code. factory-on-paradev drives Paradev through its CLI. Each one depends on a host app it does not control, and each has stopped building because the host's state changed underneath it. On 2026-11-17 the runner skipped a whole repo for three hours because a project had been detached inside Paradev.

Foundry owns the agent session itself. There is no host app. It replaces Paradev and T3 Code.

## The whole product, in four pieces

| Piece | What it adds | When it is done |
|---|---|---|
| P1 Core (this spec) | The daemon on every machine, workspaces, terminals, agent sessions, load, placement, sync, a moving coordinator, a minimal UI | Manual sessions run on any machine and Paradev is no longer needed for them |
| P2 Factory | GitHub accounts, projects with instructors, the label state machine, claims, dependencies, the gate, the review loop to 5 out of 5, the merge setting and merge queue, project leases, the ledger, attention items | The current runner is turned off |
| P3 Full UI | The Paradev-like layout, diffs, the PR view, run history, a scoreboard, the Tauri wrapper | The browser tab is no longer the main way in |
| P4 Later | A phone app, sandboxed runs on rented machines, Linear, machines started on demand, a preview per PR | |

Each piece gets its own spec and plan. This spec covers P1 only. "Decisions already made for later pieces" at the end records what the brainstorm settled for P2, so it is not lost.

## Goal

One Rust binary, `foundryd`, runs on every machine and the machines find each other over Tailscale. From a browser on any of them Tom can create a workspace on a machine he picks, or let Foundry pick one by load, and work in a terminal running Claude Code through the model gateway. Every machine, workspace and session shows its state at all times.

Exit criterion: for one week Tom runs his manual agent sessions in Foundry on at least two machines and does not open Paradev for them.

## Scope

In:

- `foundryd` as a background service on macOS and Linux.
- Machines joining each other over Tailscale.
- Projects as a repo URL and a name.
- Workspaces: a git worktree and branch on one machine, with setup commands.
- Terminals: PTYs that outlive the UI and the daemon.
- Agent sessions: Claude Code in a terminal, with the gateway address and key set.
- Session state from Claude Code's hooks.
- Load, memory, disk and session counts per machine, with limits.
- Placement by headroom, with a visible wait queue.
- Sync of every machine's records to every other machine.
- A coordinator role that moves by a priority order.
- An attention list.
- A minimal web UI: a sidebar of machines and workspaces, and the terminal.
- Self-update from a tag, with rollback.

Out, and where it goes:

- GitHub accounts, issues, labels, gates, reviews, merging, leases, the ledger: P2.
- Diffs, the PR view, run history, the desktop wrapper: P3.
- File editing in the app: never. Code changes go through a terminal.
- Plugins, automations, tasks, pages: never.
- Containers or VMs per run, machines started on demand, moving a live session between machines: P4.
- Logins per user. Access is anyone on the tailnet until a second person needs the app.

## Decisions

| # | Question | Decision |
|---|---|---|
| D1 | Language | Rust, built with Cargo. One binary per machine, no runtime to install |
| D2 | Form of the app | A web UI served by the daemon. A thin desktop wrapper comes in P3 |
| D3 | UI libraries | React, Tailwind, Radix, TanStack Query and Router, xterm.js with its serialize add-on. Chosen to match what Paradev ships |
| D4 | Daemon to UI | One WebSocket carrying JSON events and terminal bytes. No tRPC, because that needs a TypeScript server |
| D5 | How a session is hosted | A real terminal running interactive Claude Code. No headless mode. A session Tom cannot open and look at does not exist |
| D6 | Session state | From Claude Code's hooks calling the local daemon, never from reading the screen |
| D7 | Topology | Every machine is a worker. One is the acting coordinator. The role moves by a priority order when the coordinator disappears |
| D8 | Who owns a record | The machine a workspace, terminal or session lives on. No two machines write the same record |
| D9 | Models | Every session goes through the gateway (`ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN`), so every machine behaves like the `ccp` alias |
| D10 | Local data | SQLite on each machine |
| D11 | Network | Tailscale only. The daemon never listens on a public interface |

## Architecture

```
   browser ──────────────► any machine's foundryd (serves the UI)
                                   │
        ┌──────────────────────────┼──────────────────────────┐
        ▼                          ▼                          ▼
   foundryd                   foundryd                   foundryd
   this Mac                   trilogy-mac                a VPS
   worker                     worker + acting            worker
                              coordinator
        │                          │                          │
   holder ─ PTY ─ claude      holder ─ PTY ─ claude      holder ─ PTY ─ shell
        └────────── every machine syncs its records to every other ─────────┘
                                   │
                          model gateway :8317
```

Units, each with one job:

| Unit | Job | Depends on |
|---|---|---|
| `holder` | Owns one PTY and its child process. Keeps a scrollback buffer. Speaks a small protocol on a Unix socket | Nothing but the OS |
| `terminals` | Starts holders, reattaches to them after a restart, relays bytes | `holder` |
| `sessions` | Starts Claude Code in a terminal, receives hook calls, keeps each session's state | `terminals` |
| `workspaces` | Bare clones, worktrees, branches, setup commands, archiving | git, `terminals` |
| `machine` | Reads load, memory, disk. Applies the machine's limits | The OS |
| `placement` | Picks a machine for a workspace, or queues it with a reason | `machine` records from sync |
| `sync` | Sends this machine's records to peers, accepts theirs | The store |
| `coordinator` | Decides which machine holds the role. Owns shared settings | `sync` |
| `store` | SQLite, one database per machine | |
| `api` | The WebSocket and HTTP the UI talks to | All of the above |
| `attention` | The list of things a person should look at | All of the above |
| `web` | The UI | `api` |

### Terminals

- Each terminal is a PTY owned by a `holder` process. `foundryd` starts the holder detached, so the terminal survives the UI closing and the daemon restarting for an update.
- The holder keeps the last 10,000 lines of scrollback and the current screen.
- After a restart `foundryd` finds its holders by their sockets in the data directory and reattaches.
- A UI that opens a terminal first gets the saved screen and scrollback, then live bytes.
- Terminal bytes are not synced between machines. They stream from the owning machine, relayed through the machine the browser is connected to.
- When the child exits, the holder keeps the exit code and the final screen until the terminal is closed in the UI.

### Sessions

- A session is a terminal whose child is `claude`, started in the workspace's worktree with the gateway variables set.
- `foundryd` passes Claude Code a settings file that registers hooks. Each hook calls the local daemon with the session's id.
- States a session can show:

| State | Meaning | Set by |
|---|---|---|
| starting | The process has been launched and has not reported yet | Launch |
| working | The agent is running a turn | A prompt was submitted or a tool is running |
| waiting for you | The agent needs a permission answer or other input | The notification hook |
| idle | The turn ended and the agent waits for the next prompt | The stop hook |
| finished | The session ended normally | The session-end hook, unless its reason is `clear` or `resume` (`/clear` and `/resume` keep the session running), or exit code 0 |
| failed | The process died or exited non-zero | The holder |
| stuck | Working, with no terminal output and no hook event for 10 minutes | A timer in `sessions` |
| unreachable | The owning machine is offline | `sync` |

- A shell terminal has only running, exited and unreachable.
- The exact hook-to-state mapping is pinned by a contract test against the real Claude Code (see Testing). If a Claude Code update changes the hooks, that test fails and no live session does.

### State is always visible

This is a rule for every piece, not only P1: nothing runs without the UI showing that it runs and what state it is in.

- Every session and terminal shows its state in the sidebar and on its tab.
- Every machine shows online or offline, load per core, free memory, free disk and its session count.
- A machine that has not been heard from for 15 seconds is offline, and its workspaces and sessions show as unreachable. They are never hidden.
- The gateway's health is on screen, including whether any account is cooling down, read from its management API.
- Anything that needs a person is an item in the attention list, with what happened and what to do.

## Workspaces

### Projects

- A project is a repo URL and a name, stored as a shared setting.
- A project is added and removed in the UI's Projects dialog, opened from the task box. Remove asks once more before it sends.
- A new project cannot take a name another project has, compared trimmed and in any case. The machine serving the dialog checks its own copy, and the acting coordinator checks again as it numbers the add, so in a linked fleet on current builds a machine that has not yet heard of an add cannot make a second project of that name.
- The machine serving the dialog refuses a remove when it knows of a live workspace made from the project, and the acting coordinator refuses it when its store holds one. A workspace create checks that the project is still there as it writes the workspace's record. If the project is gone, the create takes back its worktree and branch and fails.
- A linked fleet on current builds still has one gap. A create on a machine that does not act checks only its own copy. A remove that the acting coordinator numbers before the create's record reaches it, and that reaches the creating machine only after the record is written, still lands. The workspace stays live without its project, and nothing shows it.
- A split fleet holds neither rule. Each side numbers its own edits, so both sides can add a project of one name, and one side can remove a project while the other makes a workspace of it. The heal keeps both projects, or leaves the workspace without its project, and nothing shows it. Neither rule is checked while the acting machine runs a build from before these checks, and a create on such a build does not check its project. The acting coordinator also numbers an add or remove without either check when it comes from a machine on a build from before edits carried an id: nobody waits for that edit's answer, so a refusal would drop it unseen.
- Each machine keeps one bare clone per project and makes worktrees from it.
- A machine clones a project the first time it hosts a workspace for it. Adding a machine needs no setup per repo.
- In P1 a machine clones with whatever git credentials it already has. Credentials handed out by the coordinator come with GitHub accounts in P2.
- A project may name setup commands, read from the repo's `.dw/config.yml` `provision` key when the file has one. This is the same step the gate uses in P2.

### Creating a workspace

1. Tom picks a project, a base branch, a name, and a machine or "let Foundry choose".
2. The worker fetches, then makes a worktree on a new branch from the base.
3. It runs the setup commands in a terminal inside the workspace. A failed setup stays visible there and raises an attention item. The workspace is still usable, so the failure can be fixed by hand.
4. The workspace opens with one agent session. Shells and more sessions can be added.

A session can be started with a first prompt (`POST /api/workspaces/{id}/sessions` takes `prompt` beside `model`): the daemon types it into the session's terminal as one paste and Enter once `SessionStart` has come and Claude Code has then turned bracketed paste on, which it does when its input box takes keys (a sign seen only before the hook counts a second after it), so the agent starts on it at once, its newlines stay in one prompt, and its hooks report it as any typed prompt. Like any paste, one over three lines or about 800 characters reaches the agent, its hooks and so its brief inside Claude Code's `<pasted_content>` tags. It is never an argument: a process's arguments are readable by every user on the machine, and the prompt may hold a token or a URL with a password, so it gets the same care as the hook token. Until it is typed the prompt is held in the daemon's memory only, so a daemon that restarts in that moment loses it and the session waits at an empty input box. A blank prompt is none; one with a NUL or over 20,000 characters is refused. The new-session dialog offers it as an optional box; a new workspace is made from the task box below, whose task is its first prompt.

Any machine's UI can make a workspace on another machine and act on another machine's workspaces: a create that picks another machine, and a session, shell, archive or close on a workspace or terminal another machine owns, is carried to its owner over the keyed peer link, and the owner's answer comes back unchanged.

Branch names are `<name>` as typed, made safe for git. Worktrees live under the data directory at `worktrees/<project>/<branch>`.

### Archiving

- Archiving stops the workspace's terminals, removes the worktree and keeps the branch.
- It refuses when the worktree has uncommitted or unpushed work, and says which. Tom can confirm to archive anyway.
- A workspace on an offline machine shows as unreachable. If its branch was pushed, Tom can open a new workspace from that branch on another machine.
- Archive is offered for this machine's workspaces from the sidebar row, the board card and any of its terminals' actions (menu and jump palette). Before anything is sent it shows what archiving means, from one read of the workspace's changes: the commits kept on its branch, the uncommitted files lost with the worktree, and each terminal that stops, marking a turn it cuts short. The daemon still decides; its refusal lists what it holds and offers to archive anyway.

### Changes

A P3 diff view, built early on a read the daemon already had. `GET /api/workspaces/{id}/changes` lists what a workspace of this machine changed since it left `origin/<base>`: the commits on its branch and each changed file, committed or not, new files included, with its hunks. The read changes nothing in the worktree, never follows a symlink, and hides credentials in URLs. The UI shows it as a sheet opened from the terminal header or the workspace's row, reading again every few seconds while open. A line number in the sheet takes a comment for Claude on that line; the comments are sent together to one of the workspace's idle sessions as a single message, typed into its terminal with Enter, each with its file, line and the line's text. A session that is working or waiting for an answer cannot be sent to. Unsent comments are kept in the open page only. `GET /api/workspaces/{id}/commits/{hash}` reads one of the branch's commits, by the short hash the changes read lists, as the same files and hunks against its first parent; any other hash is not found and never reaches git. The sheet lists the commits oldest first under "All changes", and a commit picked shows only what it changed, read once since a commit never changes; it takes no Viewed tick and no comments, which belong to the work as it stands.

### Pull requests

A P3 PR view, small. `GET /api/workspaces/{id}/pr` says where a workspace of this machine stands past it: whether its branch is on `origin` and how many commits on its `HEAD` no remote branch has (from git, with no fetch), and, once the branch is on `origin`, the newest pull request whose head commit is one of the branch's own commits: number, title, link, open, merged or closed, draft, its checks rolled up (failing, running, passing or none) and its review decision. The branch counts as on `origin` only when `origin`'s branch of that name points at `HEAD` or at one of the branch's own commits since its base, so an older branch that once had the same name is never taken for this one, and neither is its pull request. Once the base has merged the branch with a merge commit, its own commits are counted from where the base stood before that merge, so a merged pull request still shows after the base is fetched. The pull request is asked of GitHub with `gh`, signed in as the machine's user; a `gh` that is missing, signed out or offline makes it unknown, never an error, and a branch never pushed is not looked up. Its title and link are hidden as a changes read is. The UI shows it as one line under a board card of this machine and in the changes sheet's header, linked to GitHub, read when the card comes on screen, when one of its terminals changes state, every minute while its checks run, and every half minute while the sheet is open. A pull request that merged with nothing after it offers to archive the workspace from that line. Nothing shows for a branch that never left the machine and has nothing to push.

### The board

A P3 overview on the same read. With no terminal open, the UI shows every workspace as a card in three lanes by its most urgent terminal: needs you (waiting, stuck or failed), working (working or starting) and quiet. A card lists its terminals with their states and what the workspace changed since its base. Under an idle or finished session it shows what Claude said at the end of its turn, three lines until opened; an idle one's opens a reply box, and the reply is typed into its terminal with Enter, so the next turn starts from the board. A card reads the changes only while it is on screen: when it comes on screen, when one of its terminals changes state, and every half minute while an agent in it works. A quiet card is not read on a timer, so a fleet of dozens of workspaces does not keep git running. A workspace whose changes cannot be read from this machine says so.

Above the lanes, one line takes the next task, beside the way to jump. Once used it opens into the first run's composer: the task names the workspace (numbered when its project already has that branch; a typed name the project has is refused), Start makes it on this machine, or on another machine picked in the box when the fleet has one, and lands in its session already at work.

That task box is the one way a workspace is made. The sidebar's New task button opens it over any screen, and the jump palette offers what was typed as a new task in it. With no task written, a typed workspace name makes the workspace with its session idle.

On a phone the board is the home too. A line above the task box says how many workspaces need you (or that nothing does), then how many work and rest. A bar at the bottom switches between board, timeline and machines, marks the board with the count that needs you, and opens the workspaces as a drawer; the drawer's title leads back to the board from a terminal.

### First run

A fleet with no workspaces opens on one question, what Claude should work on, and a box to answer it in, instead of an empty board. The task names the workspace and its branch as it is typed (its first words, editable); the project, base and model sit under the box, and the machine too once the fleet has another one. Start makes the workspace on the machine picked, this one unless another is, and starts its session with the task as its first prompt, then opens it. Under the box, the three states a task shows first (working, needs you, ready) are explained with their glyphs. Nothing shows before the daemon's state has arrived, so the page never flashes a first run at a fleet that has workspaces.

### Briefs

A session is named by what it was asked. The daemon keeps, in memory and for its own sessions only, each session's first prompt since it started, was cleared or resumed another conversation with `/resume` (slash commands left out) and the newest step of its turn: the prompt, the tool about to run with what it acts on, or the question a notification asks. `GET /api/briefs` returns them by session id. Nothing is written to a record, so briefs do not sync and a peer's sessions show their model alone. Each text is one line of at most 120 characters, with the gateway key, every session token, token-shaped words and URL passwords hidden. The UI shows the first prompt beside the model wherever a session is listed, and the newest step under it while the session works, waits or is stuck. When a turn ends the brief also keeps what Claude said, from the stop hook's last message, until the next prompt: its lines kept, blank runs cut to one, at most 600 characters, hidden the same way.

### Steps

A P3 run history on the same hooks. The daemon also keeps each of its sessions' trail: every prompt, tool call and question, and the end of each turn, the newest 200, with the time each hook arrived (for showing; the order is the order they arrived). `GET /api/sessions/{id}/trail` returns one session's trail, cut and hidden as briefs are; a session of another machine is `404`. The UI shows it in a Steps column beside the open terminal (the whole screen on a narrow one), opened from the terminal header: one card per turn, earlier turns folded to their prompt, what they did and how long they took, the newest open and growing while the session works.

### Timeline

A P3 run history on the same trails, across the fleet. The board switches to a timeline: every session of this machine that took a turn in the window (15 minutes to 12 hours, ending now) is a lane, each turn a bar from its prompt to its end, with a tick for each tool call and a dot for each question. A turn still going runs to the "now" edge in its session's state colour. A line above sums the window: turns, agent time, tool calls and questions. Pointing at a bar shows its prompt, what it did and how long it took; clicking it opens the terminal with its Steps column. Only sessions this daemon has briefs of are read, so a peer's session is never asked; a busy session's trail is read again every few seconds, a quiet one on a change of state.

A session you are not looking at that finishes its turn or comes to need you is told once, by one rule: as a desktop alert while the page is not in front (when turned on and the browser allows it), else as a card in the page while it shows; an alert left on the desktop is taken back when you return to the page. The tab's title and icon, and a screen reader, always have it. The sidebar's bell holds the choices in one place, kept for the browser: cards (on), desktop alerts (off) and the catch-up below (on). The catch-up and the cards share one stack at the top right (the top on a phone), the catch-up first; a session the catch-up lists gets no card of its own.

Coming back to the page after it was hidden for two minutes or more (another tab, another app on a phone, the tab closed and opened again) shows a catch-up card: how long you were away, and a line for each session whose record moved meanwhile and each that needs you, most urgent first, with its state, the turns it ended and the tools it ran since, and its question or what Claude said. A line opens its terminal. Which sessions moved is read from record versions kept in the browser when the page was hidden; what they did, from their trails. Nothing shows when no session moved.

A news card says the session's workspace, what happened, and what Claude said or asks. Open goes to its terminal; a session resting on this machine can be answered in the card, the answer typed into its terminal as if there. A finished turn's card goes after twelve seconds unless the pointer, the keyboard or a begun answer is on it; one that needs you stays while the session waits. At most three show, newest on top.

### Shipping

A P3 scoreboard on the changes and PR reads, as a grouping of the board rather than a view of its own. Above the lanes, the board groups its cards either by attention (the three lanes above) or by shipping: every workspace of this machine in a lane by how far its work has gone (only here: edits or commits GitHub does not have; pushed: its branch on GitHub with commits of its own and no open pull request, or one that closed; in review: an open pull request; merged), then the ones that changed nothing yet, then another machine's and any that could not be read, named as not read from here. Each lane is counted, and the lines and commits of all the work are summed beside the grouping. A card that moves forward lights up once in its new lane. While grouped by shipping, each workspace of this machine is read when the board opens and when one of its terminals changes state, its changes every half minute while an agent in it works and its pull request every minute while its checks run; grouped by attention, the board reads as above. The grouping is kept for the browser.

In either grouping, under an idle session of this machine a card offers the next step that takes its workspace's work further (as the terminal bar's next steps: commit, push and open a pull request, fix failing checks, answer a review), typed into its terminal, drawn from the reads the card already made.

### Machines

The board also switches to its machines (`/machines`, or a machine's name in the sidebar). Each machine is a card: online, offline or unknown as the sidebar says it, when this page last heard a new sample from it, its load per core as a line over the samples this page received (up to ten minutes; the browser keeps them in memory only), its free memory and disk and its session count, its terminals counted by state, and its workspaces, each opening its most urgent terminal. An offline machine keeps its card with its last known numbers and says how many of its terminals are unreachable. No new read: it is drawn from the samples and records on the events socket.

### Ending terminals

A terminal is ended or cleared from its menu (right-click or long press on its row, or the header's More button) through `POST /api/terminals/{id}/close`, which only the owning machine answers. `GET /api/machine` returns `{"id"}` of the machine serving the UI, and the menu offers End and Clear only for that machine's terminals. Ending one that still runs asks first; clearing one that ended does not, so End and Clear are picked only by a click on them or the keyboard, never by the release of the press that opened the menu.

### Handing a terminal files

A file dropped on a terminal of this machine, pasted into it (a screenshot on the clipboard), or picked with the phone key bar's clip is sent with `POST /api/terminals/{id}/files?name=<name>` (at most 25 MB). The daemon keeps it in a folder of that terminal readable by its user only, under a name cut to letters, digits, `.`, `_` and `-` that never overwrites an earlier one, and answers its path. The UI types the path into the terminal without Enter, as dragging a file into a desktop terminal does; Claude Code reads a pasted image path as the image. The folder goes when the terminal is closed. Another machine's terminal takes no files and says so.

## Placement

- Each worker records CPU load per core, free memory, free disk and its session count every 5 seconds.
- Load is the whole machine's, not only Foundry's work, so another program filling a machine counts.
- Each machine has limits, set in shared settings: maximum sessions, maximum load per core, minimum free disk.
- "Let Foundry choose" picks, among online machines under all their limits, the one with the lowest load per core. Ties go to the one with fewer sessions.
- When no machine qualifies, the request waits in a queue shown in the UI with each machine's reason, for example `trilogy-mac: load 1.8 over limit 1.0`. It is placed as soon as a machine qualifies.
- A machine Tom picks by name is used even over its limits, after a warning.

## Sync and the coordinator

### Records

- Each machine writes only its own records: its workspaces, terminals, session states and load.
- Every record has an owner machine id and a version that only goes up.
- A machine sends changes to every peer over a WebSocket it opens to them. A receiver accepts a record only from its owner, and only when the version is newer than what it has.
- A machine that was away asks each peer for everything newer than its last known version per owner.
- Every machine therefore holds a full copy of the fleet's state, and any machine can serve the UI.

### The coordinator role

- Shared settings are the machine list, the priority order, projects and machine limits. They are changed only through the acting coordinator, which gives each change the next number in one sequence and sends it to everyone.
- A machine adds itself to the machine list under its Tailscale (MagicDNS) short name, such as `personal-mac`, and keeps its Tailscale address there up to date. Anyone can rename a machine from the UI: with the Rename button beside its name in the sidebar, or by clicking its name on its card on the machines board. The machine never replaces a name it has seen set. Each edit carries the whole entry, so until edits name one field, an edit numbered between another machine's read of the settings and its send is overwritten: a rename can be undone, or an old address put back until the machine's next look moves it again.
- The acting coordinator is the highest-priority machine that a machine can reach. Each machine works this out from who it can reach, and the UI shows which machine holds the role.
- When the coordinator disappears, the next machine in the order takes the role within 15 seconds, using its synced copy.
- When a higher-priority machine comes back, it catches up first, then takes the role.
- If the network splits, each side may have its own acting coordinator for a while. In P1 the only harm is two edits to the same shared setting. The later edit wins when the sides rejoin, and an attention item says a setting was overwritten and shows both values.
- Sessions are never affected by a change of coordinator. They belong to their machines.

P2 adds a lease per project on GitHub, so that claiming issues and merging can never happen from two machines at once. P1 has nothing that needs it.

## Security

- The daemon binds to the machine's Tailscale address and to localhost. It refuses to start bound to anything else.
- A new machine joins with a one-time code made for it, by its Tailscale name, in the Add machine dialog of a machine already in the fleet. The code works only from an address that name resolves to. As with all access in P1, anyone on the tailnet who can reach a fleet machine's UI can add a machine. Joining gives it a key of its own. Revoking a key comes with removing a machine, which is being built; until then a machine keeps every key it holds. A machine whose list holds only itself, or is empty, takes a record link from any machine whose key it holds, whenever that is, not only just after it joined. Once its list holds another machine, it follows the machine list.
- Each machine keeps the gateway key and the gateway's management key in its own secret store, set with `foundryd secret set gateway` and `foundryd secret set gateway-management`: a file readable only by the service user, on every OS. A Mac that kept them in the login keychain before copies them into that file once, never while an update is on trial (so it takes a hand install, and an automatic rollback never crosses the copy), and from then on a rebuilt or updated binary never stops at a keychain prompt. They are never written to a log or shown in the UI after they are entered.
- Hook calls are accepted only on localhost and only with the per-session token the daemon wrote for that session. The token is kept in a 0600 curl config (`<data dir>/sessions/<id>.curl`), which the hook command in the session's settings file reads with `curl -K`, so it never sits on a command line.
- Sessions run with the permissions of the service user, so a session can read and change the secrets file, the machine's identity and key table included, on macOS as on Linux. Isolation stronger than that is P4.

## Updates

- A release is a `v*` tag. A machine installs a newer tag on its own and restarts. Holders are separate processes, so terminals survive.
- The rollback from factory-on-paradev carries over: a version that starts three times without becoming healthy puts the previous version back and refuses that tag until a higher one exists.
- Sync messages carry a protocol version. A newer machine speaks the older form to an older peer, so a fleet can update one machine at a time.
- The holder's protocol is versioned separately and changes rarely, since a running holder cannot be updated without ending its terminal.

## Attention items in P1

| Item | Raised when | Clears when |
|---|---|---|
| Machine offline | No word from it for 15 seconds | It is heard from |
| Session stuck | Working with no output or hook event for 10 minutes | Output or an event arrives, or the session ends |
| Session waiting for you | The agent needs input | It gets it |
| Session failed | The process died or exited non-zero | The terminal is closed |
| Setup failed | A setup command exited non-zero | The workspace is archived or its setup terminal is closed |
| Gateway unhealthy | The gateway does not answer, or every account is cooling down | It answers with an account free |
| Disk low | Free disk is under the machine's limit | It is back over |
| Placement waiting | A request has waited over 5 minutes | It is placed or cancelled |
| Setting overwritten | Two sides of a split edited the same setting | Tom dismisses it |
| Update refused | A tag was rolled back | A higher tag installs |

## Testing

- Unit tests for the pure parts: placement, the session state rules, the record version rules, the coordinator order.
- Integration tests that start three daemons on one machine with network links the test can cut. They cover: a peer leaving and returning, the coordinator dying, a split with edits on both sides, a daemon restart with live terminals.
- A fake agent that stands in for Claude Code in those tests. It is a small program that calls the hooks in a scripted order, so no test spends model calls.
- One contract test against the real Claude Code that starts a session, sends one prompt and checks each hook arrives with the fields the daemon reads. It runs on every Claude Code update and before every release. This is the lesson from softfac, whose hand-written T3 socket messages could break on any T3 update.
- A holder test that kills the daemon mid-output and checks the reattached terminal has lost no bytes.
- Tests come first. No production change lands without a failing test before it.

## Checked before the plan is written

These are the assumptions the design rests on, with what a check of each found on 2026-11-17.

| # | Assumption | Result |
|---|---|---|
| 1 | Claude Code's hooks report the state transitions when it runs interactively in a PTY | Confirmed on 2.1.322 for `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop` and `SessionEnd` (with a `reason`). `Notification`, which drives "waiting for you", was not exercised and is covered by the contract test |
| 2 | A settings file passed with `--settings` registers hooks for one session, with no change to the user's own settings | Confirmed |
| 3 | A detached holder survives a `launchd` or `systemd` restart of the daemon | Not checked. The service issue in the plan owns it |
| 4 | Claude Code runs as root on Linux with the permission mode the factory needs | Known to fail: mel-dev refused `--dangerously-skip-permissions` as root on 2026-11-05. `foundryd` runs as a non-root service user on Linux |
| 5 | The gateway's management API can be read with the shared key | No. It takes its own management key, so that key is a second shared secret |

One thing the check found that the design did not expect: in a folder Claude Code has not seen, it opens with a trust prompt whose default answer is "No, exit". `sessions` marks each worktree as trusted in Claude Code's own state before it starts a session there, and the contract test starts in a fresh folder to prove it.

## Acceptance criteria

1. `foundryd` installed on two Macs and one Linux machine shows all three in the UI of each, with live load, memory, disk and session counts.
2. A workspace created with "let Foundry choose" lands on the machine with the lowest load per core among those under their limits.
3. With every machine over a limit, the request waits in the queue with each machine's reason, and is placed within 10 seconds of a machine qualifying.
4. A session started in a workspace runs Claude Code through the gateway, checked by the environment of the running process.
5. Closing the browser and restarting `foundryd` loses no session. Reopening the terminal shows the screen as it was and continues live.
6. Each state in the session table is reached and shown in a scripted run with the fake agent.
7. A session that needs input raises a "waiting for you" item within 5 seconds.
8. Stopping the coordinator's daemon moves the role to the next machine within 15 seconds, and sessions on the other machines keep running throughout.
9. A machine taken off the network shows as offline on the others within 15 seconds, with its sessions shown as unreachable.
10. A machine back from two hours offline shows the full current state within 30 seconds of rejoining.
11. Archiving a workspace with uncommitted work is refused with the list of what is uncommitted.
12. A release tag is installed by all three machines without ending any terminal.

## Decisions already made for later pieces

Settled in the brainstorm on 2026-11-17. They belong to P2 and P3 and are recorded here until those specs exist.

- **GitHub accounts.** Several accounts are connected to the app. Each project uses the one that owns its repo. The coordinator holds the credentials and gives a worker the right one for each run. Instructors stay a list per project.
- **The unattended loop.** Claim, build with the `/factory` skill, run the gate on a fresh checkout, then run the review gate and fix until it scores 5 out of 5. A cap on rounds ends in the `stuck` state.
- **Review independence, from `wow.md`.** The builder never reviews its own work. Each review is a fresh session with only the spec and the diff. Findings go back to the same builder session. A moved head is gated again. A merge names the exact head that was scored.
- **Merge setting per project.** Either wait for a person, which is the default, or squash-merge when the gate is green and the review is 5 out of 5.
- **Merge queue per project.** One merge at a time: update the PR onto the current base, gate that result, merge. A failure goes back to the PR's session.
- **Project lease.** A machine acts on a project only while it holds that project's lease, a ref under `refs/factory/` in the repo updated by compare-and-swap push, with a holder and an expiry. The acting coordinator holds the leases in the normal case.
- **Any machine can run a gate.**
- **Reconcile against GitHub every pass.** A merged PR or a closed issue ends the run.
- **Kept from the earlier factories.** The label state machine, dependency links, the JSONL run ledger and `retro`, attention items, questions posted to the issue, PR comments fed back to the session, Linear in P4.
- **Phone.** The same web UI over Tailscale first, a native app in P4.
