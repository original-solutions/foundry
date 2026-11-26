# Installing foundryd

`foundryd` runs as a service on each machine. It starts at login on macOS and at boot on Linux, comes back if it stops, and updates itself from release tags.

## What you need

- Tailscale, signed in. The daemon listens on the Tailscale address and on localhost only.
- `gh`, logged in with read access to `original-solutions/foundry`. The daemon uses it to find and download new releases.
- Claude Code on the `PATH` of the user that runs the service.

## Get the binary

Download a release. The asset name is `foundryd-<arch>-<os>.tar.gz`, for example `foundryd-aarch64-macos.tar.gz` or `foundryd-x86_64-linux.tar.gz`.

```sh
gh release download <tag> --repo original-solutions/foundry --pattern 'foundryd-<arch>-<os>.tar.gz'
tar -xzf foundryd-<arch>-<os>.tar.gz
```

The archive holds `foundryd` and `foundry-holder`. Keep them in the same folder.

Or build from source:

```sh
npm --prefix web ci && npm --prefix web run build && cargo build --release
```

The two binaries are then in `target/release/`.

## Before you install

Set the gateway address in the shell you install from. `install` writes it into the service.

```sh
export FOUNDRY_GATEWAY_URL=https://your-gateway.example
```

Store the two gateway keys. Each command reads one line from stdin.

```sh
./foundryd secret set gateway
./foundryd secret set gateway-management
```

Do this before you start the service. Without the keys the daemon refuses to start sessions.

`install` also copies `PATH`, `FOUNDRY_PORT` and `CLAUDE_CONFIG_DIR` from your shell when they are set. The data directory is `FOUNDRY_HOME`, or `~/.local/share/foundry`.

## macOS

```sh
./foundryd install
```

This copies both binaries to `<home>/versions/<version>/`, points `<home>/current` at that folder, and writes the launchd agent to `~/Library/LaunchAgents/io.github.original-solutions.foundryd.plist`. It then loads the agent. The service runs `<home>/current/foundryd run`.

The log is `~/Library/Logs/foundry/foundryd.log`.

## Open the UI

The daemon serves the UI at its own address, `http://127.0.0.1:7433/` (or `FOUNDRY_PORT`). To open it in your browser:

```sh
foundryd dashboard
```

It prints the address, then opens it with `open` on macOS or `xdg-open` on Linux. It reads `FOUNDRY_PORT` from the shell you run it in, so give it the value you gave `install`. Run it from `<home>/current/`, or link `<home>/current/foundryd` into a folder on your `PATH`.

It exits 1 when no daemon is running there, and when it cannot open a browser. A Linux server with no desktop is the second case: `127.0.0.1` there is the server itself, so open the UI from your own machine at the server's Tailscale address instead: `http://<tailscale address>:7433/`, or the `FOUNDRY_PORT` you gave `install`.

## Linux

Run `install` as the user that will run the service. Never as root: Claude Code refuses its permission mode as root, and `install` refuses to run as root.

```sh
./foundryd install
```

This places the binaries as on macOS and writes the unit to `<home>/foundryd.service`. It then prints the lines that install it. Run them:

```sh
sudo install -m 644 <home>/foundryd.service /etc/systemd/system/foundryd.service
sudo systemctl daemon-reload
sudo systemctl enable foundryd
sudo systemctl restart foundryd
```

The unit runs as your user. Read the log with `journalctl -u foundryd`.

## Add a machine

1. Install `foundryd` on the new machine, as above. It must be on the same tailnet as the fleet.
2. Open the UI of any machine already in the fleet, go to the machines board and click **Add machine**.
3. Type the new machine's Tailscale name, for example `trilogy-mac`, and click **Add**.
4. On the new machine, run the command the dialog shows, for example `foundryd join 100.101.102.103 K7QX3M9PRT`. Run it in a shell with the same `FOUNDRY_HOME` and `FOUNDRY_PORT` you gave `install`, as `<home>/current/foundryd join …`, or as shown once `<home>/current/foundryd` is linked into a folder on your `PATH`. The code works once, within 10 minutes, and only on the machine whose name you typed. If the command says the machine could not be added to the machine list, run the same command again while the code is valid. When it names a machine that already has the name, rename that machine first. When it names one that already has the address, remove that machine first with **Remove from fleet** on its card on the machines board of another machine, and run the command again. A machine removed from the fleet has to be given a new identity before it can join again; see [Remove a machine](#remove-a-machine) for what that does. The new machine may show on the board all the same, when the fleet was only slow to answer, and running the command again is still what finishes the join.
5. Restart the service on the new machine: `launchctl kickstart -k gui/$(id -u)/io.github.original-solutions.foundryd` on macOS, `sudo systemctl restart foundryd` on Linux.

The dialog says when the machine has joined, and its card shows on the board under the machine's own Tailscale name: the short name, even when you typed its full name or its address. There are two exceptions, and in both the card keeps the name you typed until you rename it. One is when `foundryd join` warned that it could not ask Tailscale what the machine is called. The other is an older `foundryd join`, which sends no name of its own.

Run `foundryd join` from a release at least as new as the one on the machine you join through. An older `foundryd join` still joins. But when the machine cannot be listed it says to ask for a new code, where running the same command again is what works, and it gives up after 10 seconds while the fleet may take half a minute to answer.

## Remove a machine

Open the machines board of another machine in the fleet, click **Remove from fleet** on the card of the machine to remove, and confirm. A machine cannot be removed from its own UI. Every machine then refuses it: each revokes its key and closes its links, and the board stops listing its workspaces, terminals and sessions. Its workspaces stay on its disk.

The removed machine is sent its removal before its links close, and stops listing itself. A removal cannot be undone: a machine that missed it, or that took an entry the removed machine wrote for itself while cut off, drops that entry as soon as it hears from a machine that holds the removal.

A removed machine has to be given a new identity before it can join again. That is not built yet, so for now a removed machine cannot rejoin.

## Change a setting

`install` writes the service from the current shell alone. To change a variable, export every one the service needs again first: `FOUNDRY_HOME` if you use it, `FOUNDRY_GATEWAY_URL`, and any of `PATH`, `FOUNDRY_PORT` and `CLAUDE_CONFIG_DIR` you set. Then run:

```sh
<home>/current/foundryd install
```

On macOS this replaces the agent. On Linux it rewrites the unit; run the `sudo` lines it prints.

## Set up the factory

A project's factory claims the `factory:ready` issues of its repo and builds each one on a machine of the fleet. [P2-factory.md](specs/P2-factory.md) says how. A machine takes a run only when it holds a token for the project's GitHub login and has the `factory-build` skill. A machine without them still runs manual sessions.

Run the commands below with the same `FOUNDRY_HOME` you gave `install`, as `<home>/current/foundryd …`.

### GitHub tokens and the default login

Store a token for each GitHub login the machine may act as. The command reads one line from stdin and never prints it:

```sh
foundryd secret set github:<login>
```

A run pushes branches, which may change a workflow file, and writes issues, labels, comments and pull requests. A classic token with the `repo` and `workflow` scopes covers that, and so does the token of a `gh` login, which has both. Write the login in the case GitHub shows it, because the factory looks the token up by its exact name. No restart is needed.

Then name the machine's default login:

```sh
foundryd github default <login>
```

The default is read once, when a project's factory is turned on while this machine is the acting coordinator. It becomes that project's login. Set it on every machine that can be coordinator.

A token never leaves the machine that holds it. The factory hides every token and gateway key the machine holds from the comments it posts, the messages it sends a session and the events it records.

### The `factory-build` skill

Each run is a Claude Code session that starts with `/factory-build`. The daemon looks for `~/.claude/skills/factory-build/SKILL.md` under the service user's home every 5 seconds, and places no run on a machine where it is missing.

Install the version changed for Foundry, which reads its brief from the file named on its command line and leaves the GitHub login to Foundry. An older copy picks a `gh` login of its own. That change, and the move of the skill's source to `~/EA/skills/factory-build`, are done by hand before P2 starts ([Before P2 starts](specs/P2-factory.md#before-p2-starts)). The daemon checks only that `SKILL.md` is there, so it cannot tell an older copy from the Foundry one. Replace any older copy before the machine takes its first run.

Once the source is there, its installer links it into `~/.agents/skills/` on a machine with the skills hub, which `~/.claude/skills` links to:

```sh
~/EA/skills/factory-build/install.sh
```

On any other machine, copy the folder to `~/.claude/skills/factory-build/`. If the service sets `CLAUDE_CONFIG_DIR`, Claude Code reads skills from `$CLAUDE_CONFIG_DIR/skills/` but the daemon still checks `~/.claude/skills/`, so put the skill in both.

### Turn a project's factory on

The project's repo URL must be `https://github.com/<owner>/<repo>`, because the factory reaches GitHub only over HTTPS. The repo's default branch must hold a `.dw/config.yml` whose `factory` block has `instructors`, `base`, `provision`, `gate`, `limits` with at least `gate_retries`, and `autonomy: pr`. This repo's own [`.dw/config.yml`](../.dw/config.yml) is one. A file that does not load leaves the project idle with an attention item.

The UI has no factory switch yet, so use the daemon's API on any machine of the fleet. Find the project's id:

```sh
curl -s http://127.0.0.1:7433/api/factory
```

Each entry of `projects` has its `id`, `name` and `factory` setting. Turn it on:

```sh
curl -s -X POST http://127.0.0.1:7433/api/projects/<project id>/factory \
  -H 'Content-Type: application/json' -d '{"on": true, "paused": false}'
```

With no `login`, the project takes the acting coordinator's default login. Add `"login": "<login>"` to name another. The answer is `{"factory": {...}}` with the login it took. A `422` says why it refused, for example a repo URL that is not on `https://github.com/`, or no login named and no default on the coordinator.

A project that is on acts as its login, so every later edit that keeps it on names the login. To pause its claims:

```sh
curl -s -X POST http://127.0.0.1:7433/api/projects/<project id>/factory \
  -H 'Content-Type: application/json' -d '{"on": true, "paused": true, "login": "<login>"}'
```

To turn it off, send `{"on": false, "paused": false}`. Use the port you gave `install` if it is not 7433.

### Move a repo from the runner to Foundry

The Paradev runner (`original-solutions/factory-on-paradev`) builds every factory repo until it moves. The runner and Foundry never watch the same repo. Before the first repo moves, P1's acceptance run has passed and every machine runs a P2 release.

Foundry goes first, and builds itself until 10 runs have reached `pr-open` or `stuck` and no factory bug is open. Then factory-paradev-spike, spec-reader, clockwork, factory-on-paradev and people-hub move one at a time, each after the last moved repo has 5 clean runs.

To move one repo:

1. Stop the runner claiming: `factory-runner claims pause`.
2. Wait until no issue of the repo is in `factory:triage` or `factory:building`. `factory-runner status` lists the runner's runs, and `gh issue list --repo <owner>/<repo> --label factory:building` lists the issues in one state.
3. Take the repo out of `repos:` in the runner's config, `~/.config/factory-runner/config.yml`, and restart the runner with `launchctl kickstart -k gui/$(id -u)/io.github.original-solutions.factory-runner`.
4. Let the runner claim for its other repos again: `factory-runner claims resume`.
5. Turn the project's factory on in Foundry, as above.

Foundry leaves an issue in `factory:triage` or `factory:building` alone when its newest claim comment is the runner's, and lists it as a foreign claim on the attention list. An issue the runner left in `pr-open` gets no feedback round from Foundry. Label it `factory:ready` again and Foundry claims it. The run carries on from the issue's open pull request only when the project's login opened it, it is a branch of the same repo, and it targets the base. Otherwise the run starts again from the base and opens a second pull request. To keep the runner's branch, give the project the login the runner opened its pull requests as.

The way back is per repo, and mirrors the way forward:

1. Pause the project's claims in Foundry, as above.
2. Wait until no issue of the repo is in `factory:triage` or `factory:building`.
3. Turn the project's factory off in Foundry.
4. Put the repo back in the runner's `repos:` and restart the runner.

If Foundry is too broken to finish its runs, label each issue it left in `factory:triage` or `factory:building` `factory:ready` again and take its assignee off, so the runner claims it afresh. While Foundry cannot build foundry, the runner builds the fix.

When the last repo moves, skip step 4 of the way forward and stop the runner instead, which leaves it installed:

```sh
launchctl bootout gui/$(id -u)/io.github.original-solutions.factory-runner
```

A login loads it again, but with an empty `repos:` it claims nothing. It stays stopped for a week. Then `retro` compares the two, and the runner is uninstalled.

### Compare Foundry with the runner

The runner's `retro` reads Foundry's runs from an export. Write every run the machine holds into a folder:

```sh
foundryd factory export ~/foundry-runs
```

It writes `runs/<owner>__<repo>__<issue>__<attempt>.jsonl` there, in the runner's own format. Every machine is sent the fleet's factory records, so any machine that is linked to the others will do. Then run `retro` over the export for the week the runner was stopped, and over the runner's own data:

```sh
factory-runner retro --data-dir ~/foundry-runs --since <first day> --until <day after the last>
factory-runner retro --data-dir ~/.config/factory-runner --since <date>
```

For the runner, pick `--since` so its window holds its last 30 runs. `retro` prints the number of runs in the window on its first line. It reads merged pull requests' reviews with `gh`, so run it where `gh` can read the repos.

The exit criterion asks for a gate pass rate and a stuck rate no worse than the runner's. `retro` prints no gate pass rate. Its nearest line is `reached pr-open without a person`: the share of runs that passed the gate and reached `pr-open` before any question or stop. Foundry passes when that share is no lower than the runner's and its `stuck` share is no higher.

## Updates

A running service looks for a newer release tag every 10 minutes. On each pushed `vX.Y.Z` tag, the release workflow (`.github/workflows/release.yml`) publishes `foundryd-<arch>-<os>.tar.gz`, named from `std::env::consts::ARCH` and `OS` (`aarch64`, `x86_64`, `macos`, `linux`). Beside each archive it publishes `foundryd-<arch>-<os>.tar.gz.sig`, an Ed25519 signature made with the key in the repository secret `RELEASE_SIGNING_KEY`. It publishes nothing when the tag is not `v` followed by the version in the root `Cargo.toml`, or when that key does not match the public key built into `foundryd` (`RELEASE_KEY` in `crates/foundryd/src/update.rs`). It creates the release itself and fails when one already exists for the tag, so a release's assets are never replaced: new binaries ship under a new tag. Push the tag; do not make the release in the GitHub UI first.

The signature shows that an archive came from the release workflow, so an asset changed or uploaded any other way is never installed. It does not guard against push access: anyone who can push to the repository can push a tag, or a workflow that reads the key.

When a higher tag exists, the daemon downloads the asset and its `.sig`, and checks the signature against the key built into `foundryd` before it unpacks anything. A release whose signature does not verify is refused, as described below. A release with no signature is never installed: the daemon tries it again at the next look. Once the signature checks out, the daemon checks that `foundryd --version` says the tag's version, and installs it under `<home>/versions/`. It then points `<home>/current` at it and stops. The service starts the new version. Terminals keep running: each one lives in its own `foundry-holder` process.

The new version has to prove itself. It counts as healthy once it has run for 60 seconds. If it starts three times without getting there, the next start puts the previous version back and records the tag as refused in `<home>/update.json`. A start that hangs for 5 minutes exits and counts as a failed start. A refused tag is skipped while it is the highest. A higher tag installs and clears the refusal.

A release is refused the same way, so it is not downloaded again, when its signature does not verify, when its archive has no `foundryd` or no `foundry-holder`, when the new `foundryd --version` exits non-zero or prints anything but `foundryd <version>`, or when the system says the file cannot be executed (no such file or interpreter, not executable, not a program for this machine).

Every machine keeps its secrets in `<home>/secrets.json` (mode 0600), readable only by the service user, so a new binary reads them with no keychain prompt and release binaries need no stable code signature. Sessions run as the service user, so a session can read and change that file, the machine's identity and key table included. Isolation stronger than that is P4.

Earlier releases kept a Mac's secrets in the login keychain. On a Mac that ran one of them, only a version installed by hand copies them into `<home>/secrets.json`. No copy is made while an update is on trial, by the daemon or by `foundryd secret set` or `foundryd join`. So an update the daemon installs itself fails its start with an error saying so, and is rolled back and refused like any version that does not stay up, and so is every later release until a hand install has made the copy. Once it has rolled back, install the first release that uses `secrets.json` by hand on each such Mac, as in the sections above, with someone at the logged-in desktop. A hand install of the version still on trial keeps that trial, so it does not copy either. macOS asks once for each secret foundry stored (up to four: the machine's identity, its key table and the two gateway keys): choose "Always Allow" or enter the login password for each. If a keychain read fails, the new version writes nothing and exits, and the next start asks again. The copy leaves the keychain items where they were. Every later update reads the file and needs no prompt.

Only a daemon started from `<home>/current` takes part in updates. One run any other way (`cargo run`, `./foundryd run`) never updates itself and is never counted as a trial.

If `gh` is missing or a download fails, the daemon logs a warning and tries again 10 minutes later. So does a version check that times out (10 seconds) or hits a passing system error, such as being out of processes or memory. Each `gh` and `tar` call has a time limit too.

To go back to an older version by hand, run `install` from that version's folder:

```sh
<home>/versions/<version>/foundryd install
```

`install` says it went back and names the refused tag. The newer version is then refused, as after a rollback. Export the same variables first, as in the section above, because `install` rewrites the service from your shell. The refusal holds until a higher tag ships. On Linux, run the `sudo systemctl restart foundryd` line it prints so the service starts the older version.

On a Mac, a release from before `secrets.json` reads only the keychain, which still holds the secrets as they were when foundry copied them. Before you go back to one by hand, remove `<home>/secrets.json`. The keychain is then the one store again, and the next hand install copies it afresh. Redo any change made since the copy, such as a machine joining, a revoked key or `foundryd secret set`, because the keychain never saw it. An automatic rollback never goes back past the copy, because an update the daemon installs itself never makes one.

A rollback, automatic or by hand, runs the previous version on the same data, `<home>/store.sqlite` included, so every schema change keeps that file usable by the version before it, and by its own version again once the older one has written to it (the rule on `MIGRATIONS` in `crates/foundryd/src/store/mod.rs`).

## Uninstall

```sh
./foundryd uninstall
```

On macOS this unloads and removes the agent. On Linux it removes `<home>/foundryd.service` and prints the lines that finish the job:

```sh
sudo systemctl disable --now foundryd
sudo rm /etc/systemd/system/foundryd.service
sudo systemctl daemon-reload
```

The data directory and its versions are kept.

## Check that a restart keeps sessions

Do this once per machine after installing.

1. Open the UI and start a session.
2. Note the holder's pid: `pgrep -fl foundry-holder`.
3. Restart the service.
   - macOS: `launchctl kickstart -k gui/$(id -u)/io.github.original-solutions.foundryd`
   - Linux: `sudo systemctl restart foundryd`
4. Run `pgrep -fl foundry-holder` again. The pid is the same.
5. Reload the UI. The session is still there, shows the screen as it was, and carries on.

Plan task 19 runs this check by hand.
