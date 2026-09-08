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
- `package-plugins` — builds the in-tree plugins and assembles each as a
  directory source under `target/plugins/<name>/` (under `CARGO_TARGET_DIR`
  when set); `test` and `e2e` depend on it, and the flow e2e skips (fails
  under `HECATON_REQUIRE_TOOLS`) without it.
- `serve` — a foreground daemon under `target/tmp/serve` for poking by hand
  (`HOME` is overridden, so it never touches your real state).
- `verify-claude` — the interactive spec §8.1 check with a real `claude`
  (`scripts/verify-claude.sh`); `HECATON_VERIFY_FAKE=1` self-tests it with
  `dev fake-claude`. Its data root (`target/tmp/verify-data`, the shared
  `MISE_DATA_DIR`) is kept across runs so claude downloads once per
  version; config and state under `target/tmp/verify-claude` are wiped.
- `lint`, `test`, `fmt`, `precommit`, `audit` — defined in `mise.toml`.
- `vendor-xterm` is a script, not a task: `scripts/vendor-xterm.sh` re-fetches
  and verifies the web plugin's assets against
  `crates/hecaton-plugin-web/assets/VENDOR.md`.

## Conventions
- Ports (`Materializer`, `AgentRunner`, `Clock`, `FleetStore`, `EventHandler`)
  live in `hecaton-core`; adapter crates implement them and never depend on
  each other. Only the `hecaton` binary wires adapters to ports;
  `hecaton-server` receives `Ports` and never imports `hecaton-runtime`.
  `hecaton-plugin-sdk` depends on `hecaton-api` only. `hecaton-server`'s
  *dev*-dependencies may include `hecaton-plugin-sdk` (in-process plugin
  tests). Plugin crates (`hecaton-plugin-flow`) depend on `hecaton-plugin-sdk`
  and `hecaton-api` only.
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
- tmux answers `has-session -t =<crew>` with "no current target" while its
  server is up with no session at all — the moment another crew's
  `new-session` on the same socket is starting it. `TmuxRunner` reads that
  as absent like "can't find session"; before it did, every daemon start
  with two crews (verify-claude's plugins fleet plus the fleet under test)
  failed its first pass and waited out the 30 s resync before `up` could
  proceed. The actor logs `agent ready (SessionStart received)`, the only
  line that dates readiness; the tick after it logs the phase.
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
  `cli_fleet`, `e2e`) bind port 0 and use private tmux sockets named after
  the test's pid, and every test root under `target/tmp` is
  `<prefix>-<pid>` (`hecaton_runtime::testing::TempRoot`). A root removes
  itself when the test passes and stays for reading when it fails; a run
  that nextest or Ctrl-C kills (the `.config/nextest.toml` slow-timeout
  terminates a hung test after three minutes) leaves its detached daemon,
  tmux server and socket alive, and the next e2e run reaps them by the dead
  pid in the socket name (`reap_earlier_runs`). To reap by hand:
  `tmux -L hecaton-e2e-<pid> kill-server` and `kill` the `hecaton serve`
  whose argv carries that `--tmux-socket` (the plugin e2e uses socket
  `hecaton-e2e-plugins-<pid>`, the flow e2e `hecaton-e2e-flow-<pid>`).
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
- Flow `match` regexes are full-match (`^(?:…)$`); `"rm -rf"` does not match
  `rm -rf /x`, `"rm -rf.*"` does. Compiled at `activate` with a 10 KiB size
  limit; errors carry the path
  `states.<s>.on[<i>].match.<pointer>: …`.
- Flow's state is KV `state/<agent>` (`plugins/flow/kv/state/<fleet>/<crew>/<agent>`
  on disk). A plugin or daemon restart resumes it; `down`, a config change
  or dropping the block resets it (`deactivate` deletes the key). To reset
  by hand: `down` and `up`. A changed config arrives as an `activate` in
  place, with no `deactivate` before it, and resets through the hash check;
  a rejected `update` therefore changes nothing (§17.9).
- `Plugin::metrics` returns `Option<&Metrics>`; register families through
  `Metrics` (short names, the SDK adds `hecaton_plugin_<name>_`). A plugin
  in another language must apply the prefix itself or its whole scrape is
  dropped.
- Test a plugin through `hecaton_plugin_sdk::testing::Harness`: it serves
  the real router and speaks to it over HTTP; `restart` swaps the instance
  against the same `FakeHost` (KV kept).
- `target/plugins/<name>/` is the *development* package layout (binary in
  `bin/`). A release package pins the binary as a mise tool and ships no
  binary (`docs/plugin-protocol.md` §7).
- `TmuxRunner::stop_crew` lists sessions with `#{session_group}` and kills
  every session of the crew's group: an attach (`hecaton-attach-<hex>`)
  is a session grouped with the crew's, and `kill-session` on the crew
  alone would leave its windows — and the agents — alive in the group.
- Never set `destroy-unattached` on an attach session before its client
  is attached: tmux 3.7c destroys a detached session the moment the option
  lands. `TmuxRunner::attach` runs create, select-window and both
  set-options as one command sequence inside the PTY.
- Every daemon → plugin call carries the plugin's own token; the SDK
  router 401s without it. A test plugin outside the SDK (a raw axum
  router) must be given the token or check nothing; `StubScript
  { expect_token: Some(..) }` makes the server's stub demand it.
- The root of a plugin mount forwards to `/v1/routes` (no slash); axum's
  `nest` answers the nested `/` there and 404s `/v1/routes/`. A plugin's
  `routes()` router registers `/`, not `/index`.
- Cookie-authenticated proxy requests need `Sec-Fetch-Site: same-origin`,
  or an `Origin` equal to the exact `http://127.0.0.1:<port>` of the login
  URL (`localhost` is another origin) or whose authority is the request's
  `Host` (a WebSocket handshake through a reverse proxy: no
  `Sec-Fetch-Site`, `Origin` the proxy's hostname — found by
  `verify-claude` behind one, as `[disconnected]` on the terminal page). A
  test client that sends none of them passes (a navigation sends none).
- `/v1/plugins/<name>` without the trailing slash is the admin purge
  route, so a browser there got 401 `missing or invalid admin token`
  even with a good cookie; a GET there is now a 308 to the mount, and a
  login `to` of a bare mount root gains its slash. Found by
  `verify-claude` through a reverse proxy, where the URL's final `/`
  went missing in the copy.
- `hecaton plugin open <name>` prints a URL valid for 60 s, once. Opening
  it twice is a 404 by design; the body says `already used Ns ago` or
  `expired Ns ago` (remembered 10 min), and server.log records every
  attempt with its `Host`, `Sec-Fetch-Site` and `User-Agent` — read those
  before blaming a reverse proxy or a prefetching browser.
- The web plugin's assets are `include_bytes!` of `assets/`; the crate
  does not build without them. Run `scripts/vendor-xterm.sh` after a
  fresh clone only if the files are missing — they are committed.
- `FleetWatch::next` never returns: a plugin that stops wanting frames
  drops the watch (the web plugin aborts its task at exit).
- A tmux command sequence inherits the previous command's target, so the
  attach sequence must never name the crew session; a `select-window -t
  =f/c:…` in it would make `set-option destroy-unattached on` land on
  the crew session and destroy it.
- Enter on the attach PTY is `\r` (what a terminal and xterm.js send);
  tmux treats `\n` as `C-j`. The `tmux_it` attach test writes `\r`.
- The vendored minified `xterm.js` trips gitleaks' `generic-api-key` rule
  on `…Key=void 0`; `.gitleaks.toml` keeps the default ruleset and
  allowlists exactly the three vendored files by anchored path. Anything
  else under `assets/` is still scanned.
