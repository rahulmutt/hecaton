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
  HOME and `tmux -L hecaton-e2e-<pid> kill-server`.
