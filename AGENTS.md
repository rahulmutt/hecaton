# Hecaton — agent instructions

Read `ARCHITECTURE.md` for where code lives and why. The design is in
`docs/superpowers/specs/2026-09-05-hecaton-architecture-design.md`.

## Tasks (run with `mise run <task>`)
- `check` — everything the PR tier runs (lint + test). Run before every commit.
- `lint`, `test`, `fmt`, `precommit`, `audit` — see `mise.toml` for what each does.

## Conventions
- Ports (`AgentRunner`, `FleetStore`, `EventHandler`) live in `hecaton-core`; adapters implement them and never depend on each other.
- Library crates return `thiserror` errors; only the `hecaton` binary uses `anyhow`.
- Every tool version — in `mise.toml` and in fleet config `tools:` — is exact.
- Types that hold secrets implement `Debug` by hand and print `<redacted>`.

## Gotchas
- Run cargo through mise: `mise x -- cargo …` (or a `mise run` task).
- `git config core.hooksPath .githooks` once after cloning to get the pre-commit tier.
- insta snapshots: review the `.snap.new` file, then `mise x -- cargo insta accept`. Never blind-accept.
