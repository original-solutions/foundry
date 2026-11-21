---
name: release
description: Ship a foundryd release by hand (version bump, gate, v* tag, signed GitHub Release that every machine's updater installs). Arguments - patch (default), minor, major, X.Y.Z, --dry-run.
disable-model-invocation: true
---

# foundryd release

A release is a pushed `vX.Y.Z` tag. `.github/workflows/release.yml` builds four archives (`foundryd-<arch>-<os>.tar.gz`), signs each with `RELEASE_SIGNING_KEY`, and creates the GitHub Release with generated notes. Every installed daemon checks for a higher tag every 10 minutes and installs it itself (`docs/install.md`, "Updates"). So the tag is the deploy: it reaches every machine within minutes, and a bad one is fixed only by a higher one.

The workflow publishes nothing unless the tag is exactly `v` + the `version` in the root `Cargo.toml` `[workspace.package]`. It refuses a tag that already has a release, so each tag publishes once. A run that fails ships under the next patch, and tags on origin stay as they are.

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
sed -i '' '/^\[workspace.package\]/,/^\[/ s/^version = ".*"/version = "X.Y.Z"/' Cargo.toml
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
git push origin main vX.Y.Z
```

The commit carries the change only, with no trailers. Push the tag yourself. The workflow creates the release, and one made first in the GitHub UI makes the run fail.

## 6. Watch it publish

```sh
gh run list -R original-solutions/foundry --workflow release.yml --limit 1
gh run watch <id> -R original-solutions/foundry --exit-status
gh release view vX.Y.Z -R original-solutions/foundry --json assets --jq '[.assets[].name]'
```

Done when the run is green and the release lists 8 assets: an archive and its `.sig` for each of `aarch64-macos`, `x86_64-macos`, `aarch64-linux`, `x86_64-linux`. A red run: show the failing step's log (`gh run view <id> --log-failed`) and stop; the fix ships under the next patch.

## 7. See this machine take it

Within about 10 minutes of the release:

```sh
readlink ~/.local/share/foundry/current          # versions/X.Y.Z
grep -E "installed vX.Y.Z|update check failed" ~/Library/Logs/foundry/foundryd.log | tail -3
cat ~/.local/share/foundry/update.json            # "refused" stays null
```

Done when `current` points at `versions/X.Y.Z` and the new daemon answers `curl -s 127.0.0.1:7433/api/machine`. A `refused` tag means the new version failed to start three times and was rolled back: read the log lines before the rollback and report them.

## Report

```
Released vX.Y.Z
  commit:  <short sha> on main
  release: https://github.com/original-solutions/foundry/releases/tag/vX.Y.Z (8 assets)
  this machine: running X.Y.Z   (or: still X.Y.W, why)
```
