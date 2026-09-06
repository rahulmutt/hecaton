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
- **CLI ↔ daemon** — resolved specs and credential bundles cross it; the user controls both ends today, but transport is treated as untrusted (future remote control plane).
- **Agent ↔ daemon (hook ingress)** — event JSON produced by a process running LLM-driven code. **Untrusted input.**
- **Daemon ↔ disk** — credentials at rest, fleet records.
- **Agent ↔ host** — filesystem and network from inside the sandbox.
- **Daemon ↔ external tools** — argv/config handed to `git`, `gh`, `mise`, `nono`, `tmux`, `claude`.
- **Cloned repositories** — a repo's own `.claude/` settings, hooks and scripts are attacker-controlled content.
- **Supply chain** — crates and pinned tools.

## Adversaries
- **A compromised or misbehaving agent** — can run any command the sandbox allows, emit arbitrary hook payloads, write anything into its worktree and the shared crew `.git`. Wants: credentials, other agents' data, host access.
- **A malicious repository** — controls files the agent reads and Claude's project-level settings. Wants: to steer the agent or exfiltrate credentials.
- **A local unprivileged process on the host** — can read world-readable files and connect to loopback ports. Wants: tokens, the admin bearer token.
- **A malicious dependency** — code executing at build or run time.

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

## Mitigations
| Threat | Control | Where |
|---|---|---|
| Secrets in debug output / logs | `CredentialBundle` hand-implements `Debug` → `<redacted>`; hook payloads logged at `debug` only | `crates/hecaton-api/src/credentials.rs`; *(planned §8)* |
| Secrets printed by `config resolve` | the credential bundle is loaded and discarded; the host settings.json is rendered verbatim as part of the resolved spec, so treat config resolve output as sensitive (use --no-host-defaults for shareable output) | `crates/hecaton/src/commands/config.rs` |
| Repo/user config overriding hook wiring | `claude.settings.hooks` rejected at validation; daemon re-owns the key when writing `settings.json` | `crates/hecaton-config/src/validate.rs`; *(planned §6)* |
| User `env` clobbering isolation variables | reserved `HOME`, `XDG_*`, `CLAUDE_CONFIG_DIR`, `GH_CONFIG_DIR`, `MISE_*`, `HECATON_*`, `PATH` rejected; the agent environment is set through the nono profile's set_vars (deny_vars ["*"]), so nothing but PATH crosses from the outer process | `crates/hecaton-config/src/validate.rs`; `crates/hecaton-runtime/src/env.rs`, `sandbox.rs` |
| Malformed names reaching tmux/branch/paths | DNS-label validation on fleet/crew/agent names before anything is created | `crates/hecaton-core/src/name.rs` |
| Credentials at rest | vault key 0600, XChaCha20-Poly1305, plaintext only while writing an agent's `.credentials.json` | *(planned §7)* |
| Forged hook events | per-agent secret, body size limit, timeout, per-agent rate limit, validation at the edge | *(planned §7–§8)* |
| Local process reaching the API | loopback bind, TLS, bearer token 0600 | *(planned §7)* |
| Sandbox mis-grants | generated nono profile with explicit read-only system paths and read-write `home/`+`workspace/`; `nono profile validate` before launch; user grants that conflict are rejected, not overridden | `crates/hecaton-runtime/src/sandbox.rs` (conflicts rejected with a path; `nono profile validate` before launch) |
| Secrets in argv / env / `launch.sh` | gh token only in `hosts.yml` 0600; Claude creds only in `.credentials.json`; argv arrays, shell-quoted `launch.sh` | `crates/hecaton-runtime/src/launch.rs` (no credential is an input), `home.rs`, `workspace.rs` (gh token only in two 0600 `hosts.yml` files) |
| Supply chain | exact-pinned `mise.toml`; committed `Cargo.lock`; `cargo audit` + `cargo deny` (`mise run audit`); `gitleaks` in `mise run precommit` | `mise.toml`, `deny.toml` |
