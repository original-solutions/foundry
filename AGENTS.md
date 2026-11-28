# Foundry

Read `docs/specs/P1-core.md` (what to build) and `docs/specs/P1-core-plan.md` (how it is cut) before changing code. For the factory, read `docs/specs/P2-factory.md` (what to build) and `docs/specs/P2-factory-plan.md` (how it is cut) beside them. For Settings, read `docs/specs/P3-settings.md` (what to build) and `docs/specs/P3-settings-plan.md` (how it is cut) beside those. Each plan's "Global constraints" apply to every change it covers.

## Rules for a build

- Tests first. No production change without a failing test before it.
- The gate, run from the repo root after `npm --prefix web ci && npm --prefix web run build` and `cargo install cargo-nextest --version 0.9.151 --locked`:
  `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo nextest run --workspace --locked`, `cargo test --workspace --doc --locked`, `npm --prefix web run check`.
- Edit only the files your issue names. Other modules are built by other issues at the same time. If you need something from a module that is still a stub, stop and ask on the issue.
- The scaffold declares every dependency. Add one only when nothing declared does the job, and say so in the pull request.
- No test spends model calls. The one contract test against Claude Code is `#[ignore]` and runs only with `FOUNDRY_CONTRACT=1`.
- Nothing orders records by wall-clock time. Use the record's version.
- Secrets never reach a log line, an error message or an API response.
- No `unsafe` block without a `// SAFETY:` comment.
- Commits and pull requests contain the change only: no co-author trailers and no tool bylines.
