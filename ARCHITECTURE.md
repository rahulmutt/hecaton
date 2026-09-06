# Architecture

Full design: `docs/superpowers/specs/2026-09-05-hecaton-architecture-design.md`.
This page is the short map — decisions a newcomer would otherwise re-derive.

## Start here
`crates/hecaton/src/main.rs` is the only entry point. Every subcommand is a thin
wrapper: parse args → call a library crate → print. `config resolve` and `dev
materialize` never talk to the daemon; `serve` runs it; `up`, `update`,
`down`, `status`, `list` are HTTP clients of it; `hook-relay` is what an
agent's `SessionStart` hook runs.

Mental model: **hecaton is a config generator and process launcher.** It turns a
fleet YAML file into one fully-resolved settings block per agent, then (later
phases) materializes each agent as a tmux window running
`nono run → mise exec → claude` in its own `$HOME`, and reconciles what exists
against what was declared.

## The pieces
- `hecaton-api` — serde wire types. A leaf; no logic. Anything that holds a secret
  hand-implements `Debug` and prints `<redacted>`.
- `hecaton-core` — the domain: validated names (`FleetName`, …), `RepoRef`, the
  `Fleet` that `FleetSpec` converts into with `TryFrom`, the pure reconciler
  (`reconcile::plan`/`execute`/`apply`), and the ports adapters implement —
  `Materializer`, `AgentRunner`, `Clock` today; `FleetStore` and
  `EventHandler` arrive with the server. Never does I/O, so it tests with
  fakes (`hecaton_core::fakes`).
- `hecaton-config` — YAML → resolved `FleetSpec`. Parses the three-level file,
  deep-merges settings layers as JSON values, types and validates each agent.
- `hecaton-server` — the daemon: one actor task per fleet over the reconciler,
  `FileFleetStore` with an encrypted `secrets.enc`, axum routes, hook ingress,
  `/metrics`. Depends on `core` + `api` only; the binary hands it the runtime.
- `hecaton` — the binary, and the only crate allowed to see both ports and
  adapters; it does the wiring.
- `hecaton-runtime` — driven adapters over git, gh, mise, nono, tmux. One
  module per materialization step; every path from StateLayout, every binary
  from ToolPaths; never reads the process environment.

## How it flows
**Config (Phase 1):** `read` (file.rs) → `resolve` (resolve.rs): for each agent fold
`host claude.settings → defaults → crew.defaults → agent` with `merge_layers`
(merge.rs), deserialize into `AgentSettings`, `validate_agent` (validate.rs),
then `Fleet::try_from` for names and repos.

**Runtime (Phase 2):** `reconcile::plan` (core) turns desired `Fleet` + last
`FleetStatus` + `ObservedState` into an ordered step list; `execute` walks it
through two ports. `Materializer` (`hecaton-runtime::Runtime`) makes files:
clone/worktree → `home/` (settings.json with hecaton's hooks, credentials,
hosts.yml) → `mise.toml` + `mise install` → `nono-profile.json` + validate →
`launch.sh`. `AgentRunner` (`TmuxRunner`) makes processes: session per crew,
window per agent, `remain-on-exit`, `respawn-window`. `hecaton dev
materialize` runs the file half alone.

**Control plane (Phase 3):** `up` resolves the file exactly like `config resolve`,
validates, and `POST`s `{spec, credentials}` to `http://127.0.0.1:7643`. The
`Daemon` registry spawns (or re-applies) the fleet's actor; the actor bumps the
generation, mints a hook secret per new agent, runs `reconcile_pass` in
`spawn_blocking`, persists `fleet.json` + `secrets.enc`, and publishes a snapshot
that `GET` reads. Agents post hook events to `/v1/agents/{f}/{c}/{a}/events` with
their secret; `SessionStart` arrives via `hecaton hook-relay` (a command hook) and
turns the agent `Ready`, which is what `up` waits for. `down` sets `desired: Down`;
a clean terminating pass settles in `Down`, `--purge` deletes the directory.

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
- **The agent environment lives in the nono profile, not in `env -i`.** nono
  refuses a read-write grant on any directory holding its own state root, and
  it derives that root from its own `$HOME`. So nono runs with `HOME=agents/<a>/nono`
  and sets the agent's `HOME`, `XDG_*`, `HECATON_*`, `TMPDIR`/`CLAUDE_CODE_TMPDIR`
  (the 0700 `home/tmp`; nothing under `/tmp` is granted and claude refuses an
  unreachable temp dir) through `environment.set_vars` with `deny_vars: ["*"]`.
  `PATH` is the one variable that crosses from outside.
- **The daemon port is an `open_port`, not a `connect_port`.** On Landlock a
  `connect_port` list is an outbound allowlist: the agent could reach the daemon
  and nothing else, not even DNS or the Anthropic API. `open_port` grants
  localhost TCP on that port only, leaves other egress at nono's default
  (allowed) for the fleet's `sandbox.network` to tighten, and still holds under
  a user `block: true`, so hooks keep flowing when egress is cut off.
- **Worktree branches are reused, never reset.** `-B … origin/<ref>` would drop
  unpushed agent commits on every re-`up` after `down --keep-repos`.
- **The planner is pure; the executor is dumb.** Every decision is in
  `reconcile::plan` (a total function) so the model-based test compares plans
  structurally and `cargo mutants` has something to bite.
- **Plain HTTP on loopback; TLS deferred.** An unprivileged local process cannot
  read loopback traffic, and TLS would have added cert generation, client pinning
  and a Claude CA-trust knob for nothing this iteration protects (Phase 3 spec P3-1).
- **`SessionStart` is a command hook, everything else HTTP.** Claude 2.1.263 refuses
  HTTP hooks for `SessionStart`/`Setup`; the relay is hecaton itself, so the nono
  profile grants the binary read-only and passes `HECATON_HOOK_SECRET` (P3-2, P3-3).
- **One actor per fleet.** Single writer, passes in `spawn_blocking`, snapshots on a
  `watch`; a Ready that lands mid-pass is applied when the pass ends (P3-4).
- **`Down` is a resting state.** The record and kept directories stay; `up` on a
  `Down` fleet re-applies in place; only `--purge` deletes (P3-5).
- **A failing pass retries at the resync cadence**, not at `next_restart_at`, so a
  flapping clone never spins the daemon.
- **The sandbox pins mise's config walk** (`MISE_CEILING_PATHS` = the agent
  workspace) because mise applies an untrusted repo `mise.toml`'s `[tools]`; a
  repo's own mise config is therefore invisible to agents, by design.
