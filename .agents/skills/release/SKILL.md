---
name: release
description: Ship a foundryd release by hand (version bump, gate, v* tag, signed GitHub Release that every machine's updater installs). Arguments - patch (default), minor, major, X.Y.Z, --dry-run.
disable-model-invocation: true
---

# foundryd release

A release is a pushed `vX.Y.Z` tag. `.github/workflows/release.yml` builds four archives (`foundryd-<arch>-<os>.tar.gz`), signs each with `RELEASE_SIGNING_KEY`, and creates the GitHub Release with generated notes. Every installed daemon checks for a higher tag every 10 minutes and installs it itself (`docs/install.md`, "Updates"). So the tag is the deploy: it reaches every machine within minutes, and a bad one is fixed only by a higher one.

The workflow publishes nothing unless the tag is exactly `v` + the `version` in the root `Cargo.toml` `[workspace.package]`. It refuses a tag that already has a release, so each tag publishes once. A pushed tag is never moved or deleted: a failed run is either run again on the same tag (step 6) or, when the fix is a code change, replaced by the next patch.

## Arguments

| Token | Meaning |
|---|---|
| `patch` / `minor` / `major` | The bump. `patch` when none is given; take minor or major only when the user names it |
| `X.Y.Z` or `vX.Y.Z` | That exact version, higher than the latest tag |
| `--dry-run` | Steps 1–4 only: nothing is committed, tagged or pushed |

## 1. Preflight

Run from the primary checkout (`git worktree list`, first line):

```sh
git switch main && git pull --ff-only && git fetch origin --tags
git status --porcelain --untracked-files=no      # must print nothing
sed -n '/^\[workspace.package\]/,/^\[/p' Cargo.toml | grep '^version'
git tag -l 'v*' --sort=-v:refname | head -3
git log --oneline "$(git describe --tags --abbrev=0)"..HEAD
gh pr list -R original-solutions/foundry --state open --json number,title
```

Done when you are on `main`, level with `origin/main`, the tracked tree is clean, and you hold the current version, the latest tag, the commits since it, and the open PRs. A tracked change in the tree stops the release: name the files to the user and wait.

## 2. Plan and confirm

Tell the user, in one short block: current version → new version (say whether the bump was the default or theirs), the tag, the commits it ships, any open PR they may want in first, and that the tag installs itself on every machine within about 10 minutes. Wait for their yes. "Release now", "ship it" or `--yes` given with the request counts as a yes.

## 3. Bump

```sh
perl -pi -e 's/^version = ".*"/version = "X.Y.Z"/ if /^\[workspace\.package\]/ ... /^\[/' Cargo.toml
cargo update --workspace --offline
git diff --stat
cargo metadata --no-deps --format-version 1 --locked | jq -r '.packages[] | select(.name=="foundryd") | .version'
```

Done when the diff touches only `Cargo.toml` (one line) and `Cargo.lock` (the four workspace crates), and `cargo metadata` prints `X.Y.Z`.

## 4. Gate

The gate in `AGENTS.md`, every command, on the bumped tree:

```sh
npm --prefix web ci && npm --prefix web run build
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
npm --prefix web run check
```

Done when all pass. On a failure, show its output, put the tree back (`git checkout -- Cargo.toml Cargo.lock`) and stop. On `--dry-run`, put the tree back here and report.

## 5. Commit, tag, push

```sh
git commit -am "chore(release): vX.Y.Z"
git tag vX.Y.Z
git push --atomic origin main vX.Y.Z
```

`--atomic` pushes both refs or neither, so a refused `main` (origin moved since step 1) never leaves a tag that ships a commit `main` lacks. On that refusal, delete the local tag (`git tag -d vX.Y.Z`), drop the release commit (`git reset --hard HEAD~1`; step 1 left the tree clean, so it holds only the bump), and start again at step 1. The commit carries the change only, with no trailers. Push the tag yourself. The workflow creates the release, and one made first in the GitHub UI makes the run fail.

## 6. Watch it publish

The run for this tag, not the newest release run (a tag push takes a few seconds to register a run):

```sh
for i in $(seq 24); do
  id=$(gh run list -R original-solutions/foundry --workflow release.yml --branch vX.Y.Z --limit 1 --json databaseId --jq '.[0].databaseId // empty')
  [ -n "$id" ] && break; sleep 5
done; echo "run: ${id:-none after 2 minutes}"
gh run watch "$id" -R original-solutions/foundry --exit-status
gh release view vX.Y.Z -R original-solutions/foundry --json assets --jq '[.assets[].name]'
```

Done when the run is green and the release lists 8 assets: an archive and its `.sig` for each of `aarch64-macos`, `x86_64-macos`, `aarch64-linux`, `x86_64-linux`.

A red run: show the failing step's log (`gh run view "$id" -R original-solutions/foundry --log-failed`) and tell the user which case it is. Rerun only on their yes.

- **Failed before `Publish`** with a cause outside the code (a runner, a network fetch): nothing was released. Rerun the same tag: `gh run rerun "$id" -R original-solutions/foundry --failed`.
- **Failed in `Publish`:** it can leave a draft release for the tag, and the rerun then fails on it. Delete the draft (`gh release delete vX.Y.Z -R original-solutions/foundry --yes`, never `--cleanup-tag`), then rerun as above. This is what the comment above `Publish` in `release.yml` says.
- **Needs a code change** (a build or a check that fails on the tagged commit): the tag stays. Fix it on `main` and release the next patch.

## 7. See this machine take it

Within about 10 minutes of the release. The data directory is `FOUNDRY_HOME` as the service was installed with it, else `~/.local/share/foundry`; the port is `FOUNDRY_PORT`, else 7433:

```sh
home="${FOUNDRY_HOME:-$HOME/.local/share/foundry}"
readlink "$home/current"                          # versions/X.Y.Z
log() { if [ "$(uname)" = Darwin ]; then cat ~/Library/Logs/foundry/foundryd.log; else journalctl -u foundryd --no-pager; fi; }
log | grep -E "installed vX.Y.Z|is refused|did not become healthy|update check failed" | tail -5
cat "$home/update.json"                           # "refused" stays null
curl -s "127.0.0.1:${FOUNDRY_PORT:-7433}/api/machine"
```

Done when `current` points at `versions/X.Y.Z` and the new daemon answers on `/api/machine`.

A `refused` tag in `update.json` has one of two causes, and the log line says which:

- **Refused before install:** the signature did not verify, the archive has no `foundryd` or `foundry-holder`, or the new `foundryd --version` failed or printed another version. The line reads `vX.Y.Z is refused: <reason>`, and `current` never moved.
- **Rolled back:** the new version installed but failed to start three times. The line reads `did not become healthy`, and `current` points at the previous version again.

Report the line and its cause. `docs/install.md`, "Updates", has the full rules.

## Report

```
Released vX.Y.Z
  commit:  <short sha> on main
  release: https://github.com/original-solutions/foundry/releases/tag/vX.Y.Z (8 assets)
  this machine: running X.Y.Z   (or: still X.Y.W, why)
```
