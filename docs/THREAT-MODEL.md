# Threat model — hecaton

Tier-1 model per the security-practices skill; revisit when a trust boundary
changes (new runner, control/data-plane split, new credential type). Controls
marked *(planned §N)* are specified in the architecture spec and land in later
phases; the rest exist in code today.

## Assets
- **Claude credentials** (`~/.claude/.credentials.json`, account fields) — grant API access billed to the user.
- **gh OAuth token** — read/write access to the crew's repositories.
- **Agent isolation** — one agent must not read another agent's `$HOME`, session, or the host's `~/.claude`.
- **Fleet state integrity** — the daemon's record of what should be running.
- **Host integrity** — agents run arbitrary code; the sandbox is what stands between them and the host.

## Trust boundaries
- **CLI ↔ daemon** — resolved specs and credential bundles cross it over plain HTTP on `127.0.0.1`, authenticated by a 0600 bearer token; the user controls both ends today.
- **Agent ↔ daemon (hook ingress)** — event JSON produced by a process running LLM-driven code. **Untrusted input.**
- **Daemon ↔ disk** — credentials at rest, fleet records.
- **Agent ↔ host** — filesystem and network from inside the sandbox.
- **Daemon ↔ external tools** — argv/config handed to `git`, `gh`, `mise`, `nono`, `tmux`, `claude`.
- **Cloned repositories** — a repo's own `.claude/` settings, hooks and scripts are attacker-controlled content.
- **Supply chain** — crates and pinned tools.
- **Plugin ↔ daemon** — operator-installed packages running sandboxed; `hello` and (later) the host protocol cross it over loopback HTTP with a per-plugin token.

## Adversaries
- **A compromised or misbehaving agent** — can run any command the sandbox allows, emit arbitrary hook payloads, write anything into its worktree and the shared crew `.git`. Wants: credentials, other agents' data, host access.
- **A malicious repository** — controls files the agent reads and Claude's project-level settings. Wants: to steer the agent or exfiltrate credentials.
- **A local unprivileged process on the host** — can read world-readable files and connect to loopback ports. Wants: tokens, the admin bearer token.
- **A malicious dependency** — code executing at build or run time.
- **A compromised plugin** — runs inside its own nono profile with the daemon URL and its token. Wants: other plugins' or agents' state, the admin token.

## In scope
- Credential exposure on disk, in logs, in `Debug` output, in argv/env.
- Cross-agent access to `$HOME`, sessions, sandbox escape via mis-granted paths.
- Forged or replayed hook events; hook payloads used to inject into other agents.
- Repo-supplied config overriding hecaton-owned keys.
- Known CVEs in dependencies; secrets committed to the repo.

## Out of scope / accepted risks
- **Kernel-level sandbox escapes** — nono/Landlock is the control; hecaton does not add a second sandbox.
- **Git isolation between agents of the same crew** — they share `.git` by design (spec D3, §4); agents in a crew trust each other.
- **Daemon down ⇒ Claude's HTTP hooks fail open for most events** — recorded in spec §10; the fleet is degraded, not compromised.
- **A user who runs `hecaton` with real credentials on a host they do not trust** — same trust as running `claude` itself.
- **Multi-tenant use** — one user per daemon in this iteration.
- **The per-agent hook secret is readable by its own agent** — it sits in the agent's settings.json; it authenticates only that agent's events.
- **No TLS on loopback** — an unprivileged local process cannot read loopback traffic; TLS arrives with the remote control plane (Phase 3 spec P3-1).
- **The hecaton and mise binaries are readable inside the sandbox** (`mise exec` is the launcher; mise-action puts `mise` under `$HOME` on CI runners) (it is the `SessionStart` relay); the admin token and the state root are not granted, so an agent cannot drive the fleet API. `hook-relay` lets an agent post events as itself, which it could already do over HTTP.
- **A plugin can read its package and the shared mise install dir** — both are read-only grants; a package is operator-installed, digest-checked content.
- **A malicious package is trusted at install time** — `mise trust` + `mise install` run outside the sandbox as the daemon user (`crates/hecaton-runtime/src/plugin.rs::install_plugin_tools`), and the manifest's `sandbox` block may widen the plugin's own grants; the sandbox defends against a plugin compromised at runtime, not against installing a hostile package. Packages are operator-declared and digest-pinned.

## Mitigations
| Threat | Control | Where |
|---|---|---|
| Secrets in debug output / logs | `CredentialBundle` hand-implements `Debug` → `<redacted>`; hook payloads logged at `debug` only | `crates/hecaton-api/src/credentials.rs`; hook payloads logged at debug only (`daemon.rs`); the e2e asserts no hook secret or token appears in `server.log` or any `launch.sh` |
| Secrets printed by `config resolve` | the credential bundle is loaded and discarded; the host settings.json is rendered verbatim as part of the resolved spec, so treat config resolve output as sensitive (use --no-host-defaults for shareable output) | `crates/hecaton/src/commands/config.rs` |
| Repo/user config overriding hook wiring | `claude.settings.hooks` rejected at validation; daemon re-owns the key when writing `settings.json` | `crates/hecaton-config/src/validate.rs`; `crates/hecaton-runtime/src/home.rs::render_settings` |
| User `env` clobbering isolation variables | reserved `HOME`, `XDG_*`, `CLAUDE_CONFIG_DIR`, `GH_CONFIG_DIR`, `MISE_*`, `HECATON_*`, `PATH`, `TMPDIR`, `CLAUDE_CODE_TMPDIR` rejected; the agent environment is set through the nono profile's set_vars (deny_vars ["*"]), so nothing but PATH crosses from the outer process | `crates/hecaton-config/src/validate.rs`; `crates/hecaton-runtime/src/env.rs`, `sandbox.rs` |
| Malformed names reaching tmux/branch/paths | DNS-label validation on fleet/crew/agent names before anything is created | `crates/hecaton-core/src/name.rs` |
| Credentials at rest | vault key 0600 created at first serve; secrets.enc holds XChaCha20-Poly1305 ciphertext with the fleet name as AAD; plaintext only while writing an agent's files | `crates/hecaton-server/src/vault.rs`, `store.rs` |
| Forged hook events | per-agent 32-byte secret compared in constant time (unknown agent and bad secret answer alike), checked before the per-agent rate limiter and before body validation; 20/s burst 50 per agent, 1 MiB body, JSON-object-with-string-name validation at the edge, 2 s handler timeout | `crates/hecaton-server/src/hooks.rs`, `auth.rs`, `daemon.rs` |
| Local process reaching the API | loopback bind, 0600 admin token compared in constant time on every /v1/fleets route | `crates/hecaton-server/src/api.rs`, `lifecycle.rs` |
| Sandbox mis-grants | generated nono profile with explicit read-only system paths and read-write `home/`+`workspace/` (the agent's temp dir is the 0700 `home/tmp`; nothing under `/tmp` is granted); loopback to the daemon port via `network.open_port`, other egress at nono's default (allowed) until the fleet's `sandbox.network` tightens it; `nono profile validate` before launch; user grants that conflict are rejected, not overridden | `crates/hecaton-runtime/src/sandbox.rs` (conflicts rejected with a path; `nono profile validate` before launch) |
| Secrets in argv / env / `launch.sh` | gh token only in `hosts.yml` 0600; Claude creds only in `.credentials.json`; `settings.json` holds the per-agent hook secret and is 0600 inside a 0700 `home/`; argv arrays, shell-quoted `launch.sh` | `crates/hecaton-runtime/src/launch.rs` (no credential is an input), `home.rs`, `workspace.rs` (gh token only in two 0600 `hosts.yml` files) |
| Hook secret at rest on the agent side | `settings.json` (HTTP header) and `nono-profile.json` (`HECATON_HOOK_SECRET`) are 0600 inside a 0700 agent dir; the secret authenticates only that agent; `nono-profile.json` also carries `MISE_CEILING_PATHS`/`MISE_GLOBAL_CONFIG_FILE` (not secrets, just noting the file is the sandbox boundary) | `crates/hecaton-runtime/src/home.rs`, `sandbox.rs` |
| Supply chain | exact-pinned `mise.toml`; committed `Cargo.lock`; `cargo audit` + `cargo deny` (`mise run audit`); `gitleaks` in `mise run precommit` | `mise.toml`, `deny.toml` |
| Package unpacking | digest verified before unpacking; absolute paths, `..`, symlinks, hard links and special entries rejected; files land 0444/0555 | `crates/hecaton-server/src/plugins/package.rs` |
| Plugin token | 32 random bytes, minted when the plugin is first declared and rotated on remove + re-add (the actor reuses an existing secret on every later `Apply`, so a restart keeps it), only in the 0600 `nono-profile.json` (`HECATON_PLUGIN_TOKEN`) and the `Authorization` header; constant-time compare, unknown plugin and bad token answer alike; the SDK redacts it in `Debug` | `daemon.rs::plugin_hello`, `hecaton-plugin-sdk/src/lib.rs` |
| Plugin sandbox | read: system dirs, shared mise dir, the package, the hecaton and mise binaries; read-write: `home/` and `scratch/` only; `kv/` is never granted; `nono profile validate` before launch; manifest `sandbox` conflicts rejected | `crates/hecaton-runtime/src/plugin.rs` |
| Reserved fleet | `hecaton` refused for user fleets at the API and client-side, so no user spec can shadow the plugin fleet's ids or secrets | `daemon.rs::reject_reserved`, `hecaton-config/src/resolve.rs` |
