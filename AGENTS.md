# Hecaton — agent instructions

Read `ARCHITECTURE.md` first. The design is in
`docs/superpowers/specs/2026-09-05-hecaton-architecture-design.md`; the threat
model in `docs/THREAT-MODEL.md` — load it before touching anything that handles
credentials, hook input, or sandbox rules.

## Tasks (`mise run <task>`)
- `check` — lint + test; run before every commit.
- `test-it` — the `hecaton-runtime` integration tests against real
  git/mise/nono/tmux, with `HECATON_REQUIRE_TOOLS=1` so a missing tool fails
  instead of skipping.
- `mutants` — nightly tier: mutation-tests `hecaton-core` (the reconciler).
- `e2e` — the Phase 3 journey against a real daemon; needs the same tools as `test-it`.
- `serve` — a foreground daemon under `target/tmp/serve` for poking by hand
  (`HOME` is overridden, so it never touches your real state).
- `verify-claude` — the interactive spec §8.1 check with a real `claude`
  (`scripts/verify-claude.sh`); `HECATON_VERIFY_FAKE=1` self-tests it with
  `dev fake-claude`.
- `lint`, `test`, `fmt`, `precommit`, `audit` — defined in `mise.toml`.

## Conventions
- Ports (`Materializer`, `AgentRunner`, `Clock`, `FleetStore`, `EventHandler`)
  live in `hecaton-core`; adapter crates implement them and never depend on
  each other. Only the `hecaton` binary wires adapters to ports;
  `hecaton-server` receives `Ports` and never imports `hecaton-runtime`.
  `hecaton-plugin-sdk` depends on `hecaton-api` only. `hecaton-server`'s
  *dev*-dependencies may include `hecaton-plugin-sdk` (in-process plugin
  tests).
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
- `hecaton-runtime` integration tests skip with a printed reason when a tool or
  Landlock is missing; `mise run test-it` (and CI) sets `HECATON_REQUIRE_TOOLS=1`
  so they fail instead. Their temp roots live under `target/tmp`, never `/tmp`
  (nono grants `/tmp` by default, which would make escape assertions vacuous).
- The embedded default tool table is `include_str!("../../../mise.toml")` in
  `hecaton-runtime/src/toolchain.rs`; bumping `claude` or `gh` in `mise.toml`
  changes what agents get.
- nono's state root follows nono's own `$HOME`; never point that at the agent's
  `home/` (see ARCHITECTURE.md).
- The sandbox grants read on exactly two binaries outside `/usr`, `/bin`,
  `/lib`: this `hecaton` (the relay) and the discovered `mise` (`launch.sh`
  execs it). GitHub's mise-action installs `mise` under `$HOME`, so without
  that grant every agent in CI died with exit 127 while local runs, where
  `mise` sits in `/usr/local/bin`, were fine.
- Never put the daemon port in the profile's `network.connect_port`: on
  Landlock that list is an outbound allowlist and the agent loses DNS and the
  API. The daemon port is an `open_port` (localhost only), and claude's temp
  dir is `home/tmp` via `TMPDIR`/`CLAUDE_CODE_TMPDIR` — nothing under `/tmp`
  is granted. Both were found by `mise run verify-claude` against the real
  `claude`, not by the e2e (the fake needs neither).
- `Workspace::git` (`crates/hecaton-runtime/src/workspace.rs`) scrubs
  `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE`/`GIT_PREFIX`/`GIT_COMMON_DIR` from
  every git call via `Cmd::env_remove`, and the integration-test `git`
  fixtures do the same — the pre-commit hook exports them, and a git
  subprocess that inherits them operates on this repository instead of the
  test's.
- The e2e overrides the system tool table with an empty `[tools]` in its scratch
  `$XDG_CONFIG_HOME/hecaton/mise.toml` so nothing downloads; the real embedded
  table pins `claude`, and a fresh `up` on a real host installs it.
- `hecaton dev fake-claude` is what the e2e runs as `claude.binary`; it reads
  `$CLAUDE_CONFIG_DIR/settings.json` and fires the hooks itself. Change the hooks
  block in `home.rs` and the fake together.
- `serve` writes `server/endpoint` after binding; clients resolve `--api-url`,
  then `HECATON_API_URL`, then that file. Tests bind port 0 and read it.
- `DELETE …?keep_repos=true`: axum's `Query` rejects bare flags, so every flag is
  `key=true|false` (`DownQuery::to_query_string`).
- Hook secrets live in `secrets.enc` (vault), the agent's `settings.json` (HTTP
  header) and `nono-profile.json` (`HECATON_HOOK_SECRET` for the relay), all 0600.
- `hecaton-server` and `hecaton` integration tests (`api_it`, `cli_serve`,
  `cli_fleet`, `e2e`) bind port 0 and use private tmux sockets; if a run is
  interrupted, kill leftovers via `server/hecaton.pid` under the test's temp
  HOME and `tmux -L hecaton-e2e-<pid> kill-server` (the plugin e2e uses
  socket `hecaton-e2e-plugins-<pid>`).
- `hecaton` is a reserved fleet name (the plugin fleet). `FleetName` still
  parses it — the reservation lives in `Daemon::apply`/`down` and
  `hecaton_config::resolve`.
- A plugin's token is the hook secret the fleet actor mints for
  `hecaton/plugins/<name>`; it lives in `nono-profile.json` as
  `HECATON_PLUGIN_TOKEN` and nowhere else. It is minted when the plugin is
  added and rotates on remove + re-add, not on every restart: `Actor::apply`
  reuses an existing secret and only drops the ones the spec no longer wants.
- Plugin packages: `plugins.yaml` directory sources are used in place with no
  digest (development and the e2e); tarballs and URLs need `sha256` and
  unpack read-only under `$XDG_DATA_HOME/hecaton/plugins/<name>/<digest12>/`.
- The daemon runs `mise trust` + `mise install` on the package's own
  `mise.toml` with `MISE_STATE_DIR` under the plugin's home; the sandbox uses
  the same dir, so if `mise run` says the config is untrusted, the two
  `MISE_STATE_DIR`s diverged (`plugin.rs`: `install_plugin_tools` vs
  `plugin_env`). Do not set `MISE_GLOBAL_CONFIG_FILE` inside the sandbox —
  mise would run the start task in `$HOME` and walk `$HOME`'s ancestors out
  of the sandbox (`plugin.rs::plugin_env` explains).
- `hecaton dev fake-plugin` is what the plugin e2e runs; it binds a loopback
  listener under nono and says hello through the SDK.
- `plugin remove --purge` (and `down --purge`) used to answer 500 `Directory
  not empty` about one run in twenty: `tmux kill-window` returns before nono
  finishes writing its ledger under `plugins/<name>/nono/`. Fixed on both
  sides — `PluginHost::purge` waits (30 s) for the actor to take the plugin
  out of the record before deleting anything, and `Runtime::rm_rf` retries
  `remove_dir_all` for 5 s while the error is `DirectoryNotEmpty`.
- `up` waits for plugin activations as well as `Ready`; a `fake=pending` in
  the timeout table means the plugin never said `hello` (look at
  `plugins/<name>/logs/`), a `rejected` row fails `up` at once with the
  plugin's message. Re-running `update` after fixing the plugin does
  re-attempt it: `Daemon::apply` diffs against the fleet's *active* rows
  only, so a pending or rejected pair is offered again even though its
  config did not change (R24).
- The activation table is not persisted: after a daemon restart every pair
  is `pending` until the plugin's next `hello`, which re-activates all of
  them.
- `plugin remove` prunes the activation rows of the removed plugin; `plugin
  remove --purge` also deletes `plugins/<name>/kv/` — a plugin's KV state
  survives a plain remove.
- A plugin's `stop` action leaves the agent `Stopped` until a `restart`
  action or the next `up`/`update`; the reconciler will not restart it and
  `status` shows `stopped`.
- The `PluginClient` is built with `.no_proxy()`; do not remove it — a
  `HTTP_PROXY` in the daemon's environment would otherwise capture loopback
  calls.
- Plugin `/v1/metrics` bodies must carry only `hecaton_plugin_<name>_`
  families or the whole body is dropped (counted in
  `hecaton_plugin_metrics_scrape_failures_total`).
- The SDK's `Plugin` trait uses return-position `impl Future + Send`;
  implement methods as `async fn` in the impl block (the compiler accepts
  that), and keep `Send` state (`Mutex`, not `RefCell`).
- The e2e waits for `plugin list … ready` before `up` when a fleet names a
  plugin: `Daemon::apply` only activates a pair inline against a plugin that
  is already listening, and the agent's first `PreToolUse` can fire before a
  pending pair's next `hello`.
