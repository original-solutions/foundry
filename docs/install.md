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
4. On the new machine, run the command the dialog shows, for example `foundryd join 100.101.102.103 K7QX3M9PRT`. Run it in a shell with the same `FOUNDRY_HOME` and `FOUNDRY_PORT` you gave `install`, as `<home>/current/foundryd join …`, or as shown once `<home>/current/foundryd` is linked into a folder on your `PATH`. The code works once, within 10 minutes, and only on the machine whose name you typed. If the command says the machine could not be added to the machine list, run the same command again while the code is valid. When it names a machine that already has the name, rename that machine first. When it names one that already has the address, the join cannot finish yet, because a machine cannot be removed from the fleet yet. The new machine may show on the board all the same, when the fleet was only slow to answer, and running the command again is still what finishes the join.
5. Restart the service on the new machine: `launchctl kickstart -k gui/$(id -u)/io.github.original-solutions.foundryd` on macOS, `sudo systemctl restart foundryd` on Linux.

The dialog says when the machine has joined, and its card shows on the board under the machine's own Tailscale name: the short name, even when you typed its full name or its address. There are two exceptions, and in both the card keeps the name you typed until you rename it. One is when `foundryd join` warned that it could not ask Tailscale what the machine is called. The other is an older `foundryd join`, which sends no name of its own.

Run `foundryd join` from a release at least as new as the one on the machine you join through. An older `foundryd join` still joins. But when the machine cannot be listed it says to ask for a new code, where running the same command again is what works, and it gives up after 10 seconds while the fleet may take half a minute to answer.

## Change a setting

`install` writes the service from the current shell alone. To change a variable, export every one the service needs again first: `FOUNDRY_HOME` if you use it, `FOUNDRY_GATEWAY_URL`, and any of `PATH`, `FOUNDRY_PORT` and `CLAUDE_CONFIG_DIR` you set. Then run:

```sh
<home>/current/foundryd install
```

On macOS this replaces the agent. On Linux it rewrites the unit; run the `sudo` lines it prints.

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
