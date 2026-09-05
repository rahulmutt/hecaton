# Architecture

Full design: `docs/superpowers/specs/2026-09-05-hecaton-architecture-design.md`.
This page is the short map — decisions a newcomer would otherwise re-derive.

## Start here
`crates/hecaton/src/main.rs` is the only entry point. Every subcommand is a thin
wrapper: parse args → call a library crate → print. `config resolve` is the
first one; `serve`, `up`, `update`, `down` follow in later phases.

Mental model: **hecaton is a config generator and process launcher.** It turns a
fleet YAML file into one fully-resolved settings block per agent, then (later
phases) materializes each agent as a tmux window running
`nono run → mise exec → claude` in its own `$HOME`, and reconciles what exists
against what was declared.

## The pieces
- `hecaton-api` — serde wire types. A leaf; no logic. Anything that holds a secret
  hand-implements `Debug` and prints `<redacted>`.
- `hecaton-core` — the domain: validated names (`FleetName`, …), `RepoRef`, the
  `Fleet` that `FleetSpec` converts into with `TryFrom`. Later: the reconciler and
  the ports (`AgentRunner`, `FleetStore`, `EventHandler`) adapters implement.
  Never does I/O, so it tests with fakes.
- `hecaton-config` — YAML → resolved `FleetSpec`. Parses the three-level file,
  deep-merges settings layers as JSON values, types and validates each agent.
- `hecaton` — the binary, and the only crate allowed to see both ports and
  adapters; it does the wiring.

## How it flows (Phase 1)
`read` (file.rs) → `resolve` (resolve.rs): for each agent fold
`host claude.settings → defaults → crew.defaults → agent` with `merge_layers`
(merge.rs), deserialize into `AgentSettings`, `validate_agent` (validate.rs),
then `Fleet::try_from` for names and repos → print.

## Non-obvious decisions
- **Merge is a left fold, not associative.** `null` means "delete relative to the
  layers below me"; that only has meaning in order. Tested by property
  (idempotent, overlay-dominant, never emits null).
- **Resolution happens client-side.** The daemon only ever sees resolved specs, so
  merge semantics cannot drift between client and server.
- **Exact tool versions only.** `tools: { node: "22" }` is rejected; an unpinned
  entry is a reproducibility bug (developer-environment skill).
- **`claude.settings.hooks` is hecaton-owned.** Hook wiring is how the daemon
  hears from agents; users shape behaviour through `flow` instead.
- **Ports live in `hecaton-core`, adapters depend on it, never on each other.**
  The future Kubernetes split cuts between `hecaton-server` and
  `hecaton-runtime`; `core` is shared.
