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

Every machine keeps its secrets in `<home>/secrets.json`, readable only by the service user. Earlier releases kept a Mac's secrets in the login keychain. On a Mac that ran one of them, the first update to a release that uses `secrets.json` copies the keys into that file. It can bring up a keychain prompt on the logged-in desktop for each secret foundry stored, up to four: the machine's identity, its key table and the two gateway keys. Choose "Always Allow" (or enter the login password) for each. Answer them within about four minutes: the start limit is 5 minutes, and the new version has to stay up for one more. A later answer still copies the keys, but that start counts as failed and the next start uses the file. If a keychain read fails, the new version writes nothing and exits, and the next start tries again. The keychain items stay until the new version has passed its trial, so a rollback still finds them. After that the daemon deletes them. Every later update reads the file and needs no prompt.

Only a daemon started from `<home>/current` takes part in updates. One run any other way (`cargo run`, `./foundryd run`) never updates itself and is never counted as a trial.

If `gh` is missing or a download fails, the daemon logs a warning and tries again 10 minutes later. So does a version check that times out (10 seconds) or hits a passing system error, such as being out of processes or memory. Each `gh` and `tar` call has a time limit too.

To go back to an older version by hand, run `install` from that version's folder:

```sh
<home>/versions/<version>/foundryd install
```

`install` says it went back and names the refused tag. The newer version is then refused, as after a rollback. Export the same variables first, as in the section above, because `install` rewrites the service from your shell. The refusal holds until a higher tag ships. On Linux, run the `sudo systemctl restart foundryd` line it prints so the service starts the older version.

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
