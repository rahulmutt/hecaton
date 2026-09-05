# Hecaton — agent instructions

Read `ARCHITECTURE.md` first. The design is in
`docs/superpowers/specs/2026-09-05-hecaton-architecture-design.md`; the threat
model in `docs/THREAT-MODEL.md` — load it before touching anything that handles
credentials, hook input, or sandbox rules.

## Tasks (`mise run <task>`)
- `check` — lint + test; run before every commit.
- `lint`, `test`, `fmt`, `precommit`, `audit` — defined in `mise.toml`.

## Conventions
- Ports (`AgentRunner`, `FleetStore`, `EventHandler`) live in `hecaton-core`;
  adapter crates implement them and never depend on each other. Only the
  `hecaton` binary wires adapters to ports.
- Library crates return `thiserror` errors whose messages start with the config
  path (`crews.backend.agents.bob.tools.node: …`); only the binary uses `anyhow`.
- Every tool version — `mise.toml` and fleet `tools:` — is exact.
- Types holding secrets hand-implement `Debug` and print `<redacted>`. Secrets
  never go in argv, env, or logs; `config resolve` withholds the credential
  bundle but prints the host `settings.json` verbatim unless
  `--no-host-defaults` is given.
- New Cargo dependencies are a deliberate decision: add to
  `[workspace.dependencies]` with an exact version and say why in the commit.

## Gotchas
- Run cargo through mise (`mise x -- cargo …`) or via a `mise run` task.
- insta snapshots: read the `.snap.new`, compare against the plan's expected
  values, then `mise x -- cargo insta accept`. Never blind-accept.
- Edition 2024 makes `std::env::set_var` unsafe and the workspace forbids
  `unsafe`; inject environment through parameters (see `HostPaths::from_env`).
- The merge is a left fold — don't "fix" its non-associativity.
