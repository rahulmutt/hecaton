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
7. `mise x -- cargo run -q -p hecaton -- plugin install ./my-plugin` — declares a
   plugin package (a directory with `mise.toml` and `hecaton-plugin.yaml`) in
   `$XDG_CONFIG_HOME/hecaton/plugins.yaml` and syncs the daemon; `plugin list`
   shows its phase, `plugin remove <name> [--purge]` takes it out. `plugin
   package <dir>` builds the tarball and prints the `sha256` a `plugins.yaml`
   entry needs. `https://` sources are part of the file format but rejected
   until a TLS-enabled build — package the plugin and point at the tarball.
8. `mise x -- cargo run -q -p hecaton -- dev materialize examples/payments.yaml backend/bob --no-host-defaults`
   — renders bob's generated files into a temp dir without launching anything.

## Where to look
- `ARCHITECTURE.md` — the map: pieces, flow, non-obvious decisions.
- `AGENTS.md` — conventions and gotchas for contributors (human or agent).
- `docs/superpowers/specs/` — the design; `docs/superpowers/plans/` — how it is being built.
- `docs/THREAT-MODEL.md` — what is protected, from whom, and what is out of scope.

## Status
Spec A is complete. Spec B (plugins) is in progress: phase 1, plugin
workloads, is done — packages, `plugins.yaml`, the sandboxed plugin fleet and
`hello`; the event protocol and the `flow` plugin are next.

### Upgrading to Spec B phase 1
- Fleet files rename the reserved `flow: {}` settings block to `plugins: {}`
  (a map of plugin name → that plugin's config). Stored `fleet.json` records
  still load with the old name.
- On the first start after the upgrade every running agent restarts once: its
  spec hash changed with the block.
- A stored fleet named `hecaton` is ignored, with an error in the daemon log —
  the name is now reserved for the plugin fleet.
