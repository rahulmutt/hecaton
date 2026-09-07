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
  `/metrics`, `plugins/`: `plugins.yaml` sync, package install, `PluginHost`,
  `PluginRegistry` (activations, interceptor order), `PluginEventHandler` (the
  chain), `PluginKv`. Depends on `core` + `api` only; the binary hands it the
  runtime.
- `hecaton` — the binary, and the only crate allowed to see both ports and
  adapters; it does the wiring.
- `hecaton-runtime` — driven adapters over git, gh, mise, nono, tmux. One
  module per materialization step; every path from StateLayout, every binary
  from ToolPaths; never reads the process environment.
- `hecaton-plugin-sdk` — the plugin side of the host protocol; depends on
  `api` only. `Host` (async, one method per route), the `Plugin` trait and
  `serve`, `testing::FakeHost`, `Metrics` (the prefixing registry) and
  `testing::Harness`.
- `hecaton-plugin-flow` — the first in-tree plugin, on the SDK: a per-agent
  state machine over hook events (`config.rs` parses and compiles the
  `plugins.flow` block, `machine.rs` is the pure step, `plugin.rs` owns the
  agents, the KV-mirrored state and the metrics). Plugin crates depend on
  the SDK and `api` only.

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

**Plugins (Spec B, phase 1):** `serve` syncs `$XDG_CONFIG_HOME/hecaton/plugins.yaml`
— installs tarballs under `$XDG_DATA_HOME/hecaton/plugins/<name>/<digest12>/`,
validates manifests — and renders the list as a synthetic fleet
`hecaton`/`plugins`/`<name>` that an ordinary fleet actor reconciles.
`PluginMaterializer` maps each synthetic agent back to its plugin and the
runtime materializes it like an agent under `plugins/<name>/` (`home/`,
profile, `launch.sh` running `nono run → mise run <start>` from the package
root). The actor's per-agent hook secret is the plugin's token;
`POST /v1/plugin-host/hello` verifies it and is the plugin's `SessionStart`.

**Event protocol (Spec B, phase 2a):** `up` resolves the spec and the daemon,
before its actor sees it, activates every `(agent, plugin)` pair on a `Ready`
plugin (`POST /v1/activate` at the address the plugin gave in `hello`); a
rejection is a 400 `crews.<c>.agents.<a>.plugins.<p>: <message>` and nothing
lands, a plugin that is not ready leaves the pair `pending` until its next
`hello`. Activation rows are the `PluginRegistry`'s and are overlaid on the
record at read time, so `up` waits on them and `status` shows them. Every
hook event runs `PluginEventHandler`: the interceptors that subscribe to it
and are active for the agent, in `plugins.yaml` order, each given what
remains of a 1500 ms budget, failures skipped and counted; the last response
goes to Claude; the verdict's actions run afterwards (`send_text` through the
runner, `stop`/`restart` through the actor's `SetStopped`). Observers get
batches from a per-plugin queue. Plugins call back through
`/v1/plugin-host/{fleets,agents/*/actions,kv}` with their token, gated by the
manifest's `needs`. `docs/plugin-protocol.md` is the contract.

**Flow (Spec B, phase 2b):** an agent whose settings carry `plugins.flow`
is activated on the flow plugin with that block; the plugin compiles it
(full-match regexes, config-path errors the daemon prefixes into
`crews.<c>.agents.<a>.plugins.flow: …`) and resumes or resets the agent's
state from KV `state/<agent>`. Every hook event of that agent runs one
step: the first rule of the current state whose event and `match` entries
hold sets the verdict's keys, carries `send`/`action` as verdict actions,
and moves the state — written back to KV before the verdict returns.
`mise run package-plugins` assembles `target/plugins/flow/`, the directory
source the e2e loads.

## Non-obvious decisions
- **Merge is a left fold, not associative.** `null` means "delete relative to the
  layers below me"; that only has meaning in order. Tested by property
  (idempotent, overlay-dominant, never emits null).
- **Resolution happens client-side.** The daemon only ever sees resolved specs, so
  merge semantics cannot drift between client and server.
- **Exact tool versions only.** `tools: { node: "22" }` is rejected; an unpinned
  entry is a reproducibility bug (developer-environment skill).
- **`claude.settings.hooks` is hecaton-owned.** Hook wiring is how the daemon
  hears from agents; users shape behaviour through the settings block's
  `plugins:` map instead (it was called `flow:` before Spec B, and still
  loads under that name).
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
- **Plugins ride the reconciler as a synthetic fleet.** `plugin_fleet()`
  renders plugins as agents whose only setting is the plugin hash;
  `plan`/`execute`/`apply` were not touched, and `cargo mutants` still covers
  them for plugins too (PB-5).
- **The plugin token is the hook secret.** One minting path, one index, one
  constant-time check; `hello` is authenticated exactly like a hook event.
- **`hecaton` is a reserved fleet name.** Rejected client-side (`config
  resolve`) and by `POST`/`DELETE /v1/fleets`; readable through `GET
  /v1/fleets/hecaton` and `hecaton status hecaton`; never a fleet-list row.
- **`plugins.yaml` is the record.** The plugin actor persists nothing
  (`NullStore`); state survives removal under `plugins/<name>/` until
  `--purge`.
- **The package's `mise.toml` is used in place.** Never copied: the sandboxed
  `mise run` finds it as the local config from the package cwd (no
  `MISE_GLOBAL_CONFIG_FILE` inside the sandbox; `MISE_CEILING_PATHS` =
  package parent + plugin home), while the daemon-side `mise trust`/`mise
  install` name it through `MISE_GLOBAL_CONFIG_FILE`.
- **Activation runs before the actor, and its state is a read-time overlay.**
  `Daemon::apply` activates first so a rejection can be a 400 with nothing
  landed; the registry owns the rows and `Daemon::get`/`snapshots` copy them
  into `AgentStatus.plugins`. Two writers, two records: the actor's is
  persisted, the registry's is rebuilt from the fleet records at start
  (§16.2, §16.3).
- **`stop` and `restart` are per-agent desired state.** `FleetRecord.stopped`
  is honoured by the planner: stopped if observed, never restarted, counter
  untouched. A `restart` is stop then resume, two passes; an `Apply` clears
  the set for every declared agent (§16.4).
- **The chain fails open.** A dead or slow interceptor is skipped and
  counted; a dead `flow` plugin stops blocking, which the threat model
  accepts.
- **The daemon speaks `reqwest` to plugins, the CLI still speaks `ureq`.**
  The chain runs on every hook event under a deadline; a blocking client
  would cost a thread per event (§16.1). No TLS feature on either.
- **Flow's regexes are full-match.** `match: { /tool_input/command: "rm" }`
  matches only the command `rm`; write `rm.*`. A pattern silently matching
  every command containing it is the worse surprise (§17.2).
- **Flow's state survives restarts and resets on `deactivate`.** Plugin and
  daemon restarts re-`activate` without a `deactivate`, so the KV state is
  resumed; `down`, a config change and dropping the block go through
  `deactivate`, which deletes it (§17.3). A changed config arrives as an
  `activate` in place, with no `deactivate` before it, and resets through
  the hash check; a rejected `update` therefore changes nothing (§17.9).
- **Plugin metrics are registered through the SDK.** `Metrics` prefixes
  every family, so an SDK plugin cannot trip the daemon's prefix rule
  (§17.4).
- **Plugin metrics are re-exported only under `hecaton_plugin_<name>_`.** A
  body with any other family is dropped whole, so a plugin cannot spoof the
  daemon's own series.
