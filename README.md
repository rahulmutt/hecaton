# hecaton

A control plane and orchestrator for fleets of coding agents (Claude Code
first), driven over a loopback HTTP API from a thin CLI. Agents run isolated — own
`$HOME`, own tools, own sandbox — as tmux windows grouped into crews that share
a repository.

## Quickstart
1. `mise trust && mise install` — pinned toolchain (Rust and every tool hecaton shells out to).
2. `git config core.hooksPath .githooks` — enables the pre-commit tier.
3. `mise run check` — lint + tests, e2e included; the same gate CI runs.
4. `mise x -- cargo run -q -p hecaton -- config resolve examples/payments.yaml --no-host-defaults`
   — resolves the example fleet and prints every agent's merged settings.
5. `mise x -- cargo run -q -p hecaton -- serve -d` — starts the daemon on `127.0.0.1:7643`
   (token, vault key and log under `$XDG_STATE_HOME/hecaton/server/`).
6. `mise x -- cargo run -q -p hecaton -- up my-fleet.yaml` — point `repo:` at a repository you
   can clone; waits until every agent's Claude has started. Then `status <fleet>`, `list`,
   and `down <fleet> --keep` (repos and homes survive; `--purge` removes everything).
7. `mise x -- cargo run -q -p hecaton -- dev materialize examples/payments.yaml backend/bob --no-host-defaults`
   — renders bob's generated files into a temp dir without launching anything.

## Where to look
- `ARCHITECTURE.md` — the map: pieces, flow, non-obvious decisions.
- `AGENTS.md` — conventions and gotchas for contributors (human or agent).
- `docs/superpowers/specs/` — the design; `docs/superpowers/plans/` — how it is being built.
- `docs/THREAT-MODEL.md` — what is protected, from whom, and what is out of scope.

## Status
Spec A is complete: configuration (Phase 1), runtime (Phase 2) and the control
plane (Phase 3: daemon, API, hook ingress, CLI). Next is Spec B, the `flow`
state machine over hook events.
