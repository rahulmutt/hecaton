# hecaton

A control plane and orchestrator for fleets of coding agents (Claude Code
first), driven over an HTTPS API from a thin CLI. Agents run isolated — own
`$HOME`, own tools, own sandbox — as tmux windows grouped into crews that share
a repository.

## Quickstart
1. `mise trust && mise install` — pinned toolchain (Rust and every tool hecaton shells out to).
2. `git config core.hooksPath .githooks` — enables the pre-commit tier.
3. `mise run check` — lint + tests; the same gate CI runs.
4. `mise x -- cargo run -q -p hecaton -- config resolve examples/payments.yaml --no-host-defaults`
   — resolves the example fleet and prints every agent's merged settings.
5. `mise x -- cargo run -q -p hecaton -- dev materialize examples/payments.yaml backend/bob --no-host-defaults`
   — renders bob's settings.json, mise.toml, nono-profile.json and launch.sh into a temp dir.

## Where to look
- `ARCHITECTURE.md` — the map: pieces, flow, non-obvious decisions.
- `AGENTS.md` — conventions and gotchas for contributors (human or agent).
- `docs/superpowers/specs/` — the design; `docs/superpowers/plans/` — how it is being built.
- `docs/THREAT-MODEL.md` — what is protected, from whom, and what is out of scope.

## Status
Phases 1 (configuration) and 2 (runtime: materialization, tmux runner,
reconciler) are complete. The daemon, hook ingress and `up`/`down` are Phase 3;
see the spec's §11.
