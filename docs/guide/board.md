# The board and the sidebar

The board is the home of Foundry once any workspace exists. It shows every workspace as a card, with what each session is doing and what the workspace has changed. The sidebar beside it lists the same workspaces and terminals, and stays with you when you open a terminal.

![The board grouped by attention: a task line, the counts, and workspace cards in Needs you, Working and Quiet](images/board-attention.png)

To get back to the board from anywhere, click **Foundry** at the top of the sidebar, or the mark at the top of the rail.

## Reading a card

Each workspace is one card. The coloured bar on its left edge takes the colour of its most urgent terminal.

- **Title.** The workspace name. Click it to open its most urgent terminal. Below it, the project and the branch.
- **Machine name.** Shown at the right of the title when the fleet has more than one machine.
- **Terminals.** One row per session or shell, most urgent first. A row has its state glyph, its name, and its state label. For a session on this machine, the row also has the title it took from its first prompt, and underneath what it is doing right now, such as "Edit src/auth/redirect.ts" or the question it asked you. Click a row to open that terminal. A card shows four rows, then "+N more". A workspace with none says "No terminals".
- **What Claude said.** When a session on this machine is **Ready** or **Ended**, what Claude said at the end of its last turn shows under its row, three lines at a time. Click it to read it all.
- **Reply.** For a **Ready** session, reading it all also opens a reply box. Type and press `Enter` to answer without leaving the board. The reply goes into the session's terminal as if you typed it there. Since what Claude said shows only for sessions on this machine, so does the box.
- **Next step.** The first **Ready** session of a workspace on this machine also gets buttons under its row, such as **Commit 1 file**, **Push 1 commit**, **Open a PR**, **Fix failing checks** or **Address the review**. Press one and Foundry types that instruction to Claude for you. [Next steps for an idle session](sessions.md#next-steps-for-an-idle-session) lists them all.
- **Archive.** The small box button at the top right of a card on this machine. With a mouse it shows when you point at the card. See [End, clear and archive](sessions.md#end-clear-and-archive).

### The changes line

The footer of each card says what the workspace changed since it left its base branch. It shows lines added and removed, the number of files, how many are uncommitted and how many commits, then the three files with the most changed lines and the newest commit. A card with nothing says "No changes yet". Click the footer to open the full changes sheet for the workspace.

The line reads the changes only while the card is on screen. It reads them when the card comes on screen, whenever one of the workspace's terminals changes state, and every 30 seconds while an agent in it is working. So the end of a turn shows what the turn wrote. Only the machine that holds a workspace can read its changes, so a card for another machine's workspace says "Its changes can't be read from here". A failed read says the same.

### Where the work has got to

On cards of this machine, a line under the changes says how far the branch has travelled. It reads, for example, **Not pushed** with "1 commit only here", **Pushed** with "No PR yet", or the pull request number with its checks and review, such as "Checks pass" and "Approved". The pull request number links to GitHub. When the pull request is merged and no commit came after it, the line offers **Archive**.

## Group the board

**Group by** above the cards switches between two ways to sort them. The choice is kept in this browser.

**Attention** is the default. It sorts by what each workspace needs, and leaves out empty groups.

- **Needs you.** A session asked something, is stuck or failed.
- **Working.** An agent is working or starting.
- **Quiet.** Nothing is happening.

Inside a group, the most urgent card comes first. **Go through them**, beside the **Needs you** count, opens the first session that needs you; from there **Next** in its header leads to the rest. See [Go through what needs you](sessions.md#go-through-what-needs-you).

**Shipping** sorts by how far each workspace's work has gone on its way to the main branch.

![The board grouped by shipping, with a count line and workspaces in Only here and Pushed](images/board-shipping.png)

- **Only here.** Edits or commits GitHub does not have yet.
- **Pushed.** The branch is on GitHub with no open pull request.
- **In review.** A pull request is open.
- **Merged.** Its pull request merged.
- **No changes yet.** No edits or commits since the workspace left its base branch.
- **Not read from here.** Another machine's workspace, or one whose changes could not be read.

Beside the buttons, a line counts each stage and sums the work, for example "3 only here › 1 pushed › 0 in review › 0 merged · +136 −0 · 2 commits". While Foundry is still reading the workspaces it says "Reading where each workspace's work stands…".

## Counts and views

Under the task line, a line counts what the fleet holds, such as "5 sessions · 0 shells · 5 workspaces · 1 machine". When a machine is offline, it adds "1 offline". The line is hidden on a narrow screen.

At its right, three buttons switch the view of the whole home.

- **Board.** The cards described above.
- **Timeline.** Every session's turns on one clock.
- **Machines.** One card per machine. See [Machines](machines.md).

This browser remembers whether you last chose **Board** or **Timeline**. **Machines** has an address of its own instead. The task line stays where it is while the view changes, so a task half written survives a switch.

## Timeline

The timeline answers "what has each agent been doing". Each session on this machine that took a turn in the window gets a row, and each turn is a bar from its prompt to its end. Sessions on other machines do not show.

![The timeline: a row per session with a bar for each turn and tick marks along the clock](images/board-timeline.png)

- A bar that is still going runs up to the **now** edge, in its session's colour. A finished turn is paler.
- A tick on a bar is a tool Claude ran. An amber dot is a question it asked.
- Point at a bar to see what the turn was and did. Click it to open the terminal with its steps column.
- The line above the rows sums the window: turns, agent time, tool calls and questions.
- **15m**, **1h**, **4h** and **12h** pick the window. The choice is kept in this browser.
- Sessions with no turn in the window are counted below, for example "1 more session took no turns in the last hour".

The timeline only has turns from after this machine's daemon started.

## The sidebar

The sidebar is the same fleet as a list. It sits at the left on a wide screen. From the top:

- **Foundry** goes to the board.
- The buttons in the header are, in order, **Collapse sidebar**, a theme switch, the bell for notifications, **Jump to** and **New task**. **Collapse sidebar** shows only on a wide screen.
- Each machine has a status dot, its name, and its status. Click the name to open the Machines view. Under it, a load bar and one line of numbers, such as "2.88/core · 8.1 GB mem · 59.8 GB disk · 4 sessions". Point at the line for fuller numbers. The bar turns amber at 0.7 load per core and red at 1.
- Under a machine, its workspaces. **Fold all** at the right of the machine line folds every workspace, and then reads **Unfold all**.
- **Keyboard** at the bottom lists every key the app answers. `?` opens it too.

### Folding

A folded workspace is one line, its name and a strip of state glyphs, most urgent first. The strip shows six, then "+N". Click a glyph to open that terminal. Click the name, or the arrow, to unfold.

An unfolded workspace shows its branch and project, a row of actions, and every terminal with its state label and what it is doing.

The workspace you are in unfolds by itself and folds again when you leave. One you unfolded yourself stays open. Which are open is kept in this browser.

![The sidebar with two workspaces unfolded and the actions of one shown](images/board-sidebar.png)

### A workspace's actions

Point at an unfolded workspace and a row of buttons shows at the right of its branch line. On a touch screen they are always shown.

- **Changes.** Opens the changes sheet.
- **New session.** Asks for a model and a first prompt, then starts Claude Code in this workspace.
- **New shell.** Opens a shell in this workspace. It shows in the list, where you can open it.
- **Clear N ended.** Only on this machine, and only when some terminals have ended.
- **Archive.** Only on this machine.

[End, clear and archive](sessions.md#end-clear-and-archive) says what the last two do.

Right-click a terminal anywhere it is listed, or press and hold it on a touch screen, for its menu. That is in the sidebar, on a board card, in the rail, or in a tab. [The terminal's menu](sessions.md#the-terminals-menu) lists what it offers.

### Using the keyboard

The workspace list is one Tab stop. Once inside:

- `Up` and `Down` move between workspaces and terminals.
- `Right` unfolds a workspace or goes into it. `Left` folds it, or from a terminal goes up to its workspace.
- `Home` and `End` go to the first and last.
- Typing letters goes to the next entry whose name starts with them.
- `Enter` opens a terminal, or folds and unfolds a workspace.

## The rail

The rail is the sidebar squeezed to a strip as narrow as a button. It gives a terminal more room and still shows every state.

![The rail: a strip of buttons, then a column of state glyphs for each workspace](images/board-rail.png)

Press the **Collapse sidebar** button in the sidebar header, or `Cmd+B` (`Ctrl+B` on Windows and Linux, outside a terminal). **Expand sidebar** on the rail, or the same key, brings the list back.

On the rail, from the top:

- The Foundry mark goes to the board, and the factory icon goes to the Factory view. See [The factory](factory.md).
- **Expand sidebar**, **Jump to** and **New task**.
- A dot per machine for its status. Click it to open the Machines view.
- A column of glyphs for each workspace. The workspace you are in is outlined. A glyph is a link to its terminal, and pointing at it names the workspace, terminal and state. A column shows six at most, then "+N".
- The bell and the theme switch at the bottom.

`Up`, `Down`, `Home` and `End` move along the rail's glyphs.

The choice of rail or list is kept in this browser. With none kept, a window narrower than 1024 pixels starts on the rail.

## Jump to anything

The jump palette is how you reach any session or shell without going through the lists. Press `Cmd+K` on a Mac, or `Ctrl+K` elsewhere. Inside a terminal the shell keeps `Ctrl+K` for itself, so use a **Jump to** button there: in the sidebar header, on the rail, or in the terminal's header on a phone. The search icon in the board's task line opens it too.

![The jump palette with the query "retry", one match, and a preview of that session on the right](images/board-jump.png)

Type a few words. A row matches when it holds every word, in any of its workspace name, session or shell name, first prompt, state, project, branch or machine. So `storefront working` finds working sessions in that project.

- `Up` and `Down` move through the list and `Enter` opens the picked terminal.
- `Shift+Enter`, or `Shift` and a click, opens it beside the terminal you already have open, in a split. See [Two terminals side by side](sessions.md#two-terminals-side-by-side).
- `Tab` lists what you can do with the picked terminal, as its right-click menu does. Type to narrow that list and press `Enter` to do the picked one. `Esc`, `Shift+Tab` or `Backspace` in an empty box goes back.
- On a wide screen, a preview beside the list shows what the highlighted session was asked, its last turn, what Claude said and what its workspace changed. For a session on another machine it says which machine keeps them.
- After the sessions and shells come **New shell in** each workspace. Pick one to open a shell there.
- The last row, **New task "your words"**, opens the New task dialog with what you typed. If nothing matches, the palette says "Nothing matches" and that `Enter` makes it a task for Claude.

`Esc` closes the palette.

## Notifications

You do not have to watch the board. Foundry tells you once when a session finishes its turn, and once when one needs you. A session needs you when it is waiting for you, stuck after ten minutes with no output, or failed. You get no toast for a terminal that is on screen.

**Notifications**, the bell in the sidebar header and at the foot of the rail, chooses how the news reaches you.

![The Notifications panel with three switches](images/board-notifications.png)

- **Cards in the page.** On by default. While you are at Foundry, a toast slides in at the top right, with what Claude said or is doing.
- **Desktop alerts.** Off by default. When Foundry is in the background, you get a desktop alert instead of a toast. Turning it on asks your browser for permission. A click on the alert opens that session. Returning to the page clears the alerts. If your browser blocks them, the switch is disabled and says to allow notifications in the site settings.
- **Catch-up when you come back.** On by default. After two minutes away, a card says what each session did meanwhile.

The three choices are kept in this browser. The bell is filled when desktop alerts are on, and crossed out when all three are off.

The tab always carries the news. Its title counts what needs you first, such as "(2) Foundry · 2 need you", then what works, "Foundry · 3 working", and is plain "Foundry" when all is calm. Its icon is the fleet's most urgent state, and ripples while an agent works. A screen reader hears each piece of news too.

### Toasts

A toast shows the workspace, the session and what happened, then what Claude said or what it is doing. **Open** goes to its terminal.

![A toast for a session that needs you, with its message and an Open button, under the attention card](images/board-toast.png)

If the session is **Ready** and on this machine, the toast also has a reply box. Type and press `Enter`, and your reply goes into the session as if you typed it in its terminal, without leaving where you are.

![A toast for a session that finished its turn, with a reply box and Open](images/sessions-toast.png)

A toast for a finished turn goes away after 12 seconds, unless the pointer or the keyboard is on it, or you started a reply. A toast for a session that needs you stays until the session moves on. The X button, or `Esc` with the toast focused, closes it. A toast also leaves once you open the terminal or the session moves on.

### The catch-up

Come back to the page after two minutes or more, in another tab or app, or after closing it, and a **While you were away** card heads the stack. It says how long you were away, then gives one line for each session that changed or needs you, most urgent first. A line shows the state, the turns and tool calls since, and the question or last words. A session that started while you were away is marked **New**. Click a line to open it. The X button, or `Esc`, closes the card. If nothing moved, nothing shows.

## When something needs you

Some things need a person and are not a session's question. They gather in a card at the top right, headed with a count, such as "1 needs attention". Click the header to fold it.

Each item has a title, the machine it is about, what happened, and what to do. Items with a terminal have **Open terminal**. The titles are Machine offline, Session stuck, Waiting for you, Session failed, Setup failed, Gateway unhealthy, Disk low, Placement waiting, Setting overwritten and Update refused. [The factory](factory.md) adds five more, which show as a raw id instead of a title: `run_stuck` (a run is stuck), `run_needs_info` (a run asked questions on its issue), `gate_infra` (a run's gate could not run at all), `foreign_claim` (an issue is in a factory label under a claim that is not Foundry's) and `factory_idle` (a project's factory is on but idle, or has gone 10 minutes without word from the machine that holds it). They clear themselves when the cause is gone. Only **Setting overwritten** has a **Dismiss** button.

A second card, "N waiting for a machine", lists the tasks you asked Foundry to place that no machine can take yet. [Let Foundry choose a machine](machines.md#let-foundry-choose-a-machine) explains it.

## Theme

The sun or moon button in the sidebar header, at the foot of the rail, and on the connection error page switches between dark and light. Dark is the default. The choice is kept in this browser.

## When the connection drops

Foundry says so on screen in a card at the top right, so you never read a frozen board as a quiet one.

- **Connecting…** When you reopen the page, it shows the fleet this browser saw last, greyed. After a second of waiting, it says "Connecting…" and when it was last seen.
- **Can't reach the daemon at** the address, with "Showing what this browser last saw" and a countdown. The daemon did not answer. States are frozen until it does.
- **Lost the daemon** and how long ago. The link dropped after it had been working. Everything greys and the working glyphs stop moving. Foundry retries by itself, with a countdown such as "Retrying in 5 s", and **Try now** retries at once.
- **Back after** a time. Once the link returns, this line shows briefly, and everything is live again.

If the page has never heard from the daemon and remembers nothing, it fills with **Can't reach the daemon**, the address it tried, how many times and when the next try is, and a **Try now** button. That is not an empty fleet. Terminals keep running on their machine. Check that the daemon is running there. See [Installing foundryd](../install.md).

## On a narrow screen

On a phone, the board is the home and the sidebar is a drawer.

![The board on a phone: the attention card, a one-line summary, the task box, and the bottom bar](images/board-narrow.png)

- A line above the task box sums the fleet, such as "2 need you · 2 working · 1 quiet".
- A bar at the bottom has **Board**, **Timeline**, **Machines** and **Workspaces**. The first three do what the switch does. **Board** carries a badge with how many workspaces need you. **Workspaces** opens the sidebar as a drawer, and the close button or `Esc` shuts it.
- What needs you and the tasks waiting for a machine sit at the top of the page, folded, so they never cover a terminal.
- Toasts appear at the top.
- Open a terminal and its header opens the drawer too. See [On a phone](sessions.md#on-a-phone).
