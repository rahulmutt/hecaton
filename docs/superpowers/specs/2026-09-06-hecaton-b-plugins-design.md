# Hecaton — Spec B: Plugins

**Date:** 2026-09-06
**Status:** Approved in brainstorm 2026-09-06. Supersedes the "Spec B — state
machine" entry of `2026-09-05-hecaton-architecture-design.md` §11 (the
*architecture spec*); builds on `2026-09-06-hecaton-a3-control-plane-design.md`
(the *Phase 3 spec*).
**Scope:** a general, stateful plugin mechanism for the daemon — the plugin
package format and manifest, the declarative daemon plugin config, the host
protocol (observers, interceptors, actions, fleet reads, KV, attach), the
reverse-proxied HTTP mount, materialization and sandboxing of plugins as
workloads of a reserved fleet, the `hecaton plugin` CLI, the
`hecaton-plugin-sdk` crate, and two in-tree plugins: `flow` (the state machine
the architecture spec called Spec B) and `web` (terminals in a browser).

Where this document and earlier specs disagree, this document wins; §12 lists
the corrections and the reasons.

---

## 1. Decisions log

| # | Decision | Rationale |
|---|----------|-----------|
| PB-1 | **Plugins are out-of-process and daemon-launched.** A plugin is a separate program the daemon materializes, launches and talks to over loopback HTTP. | Any language; a plugin crash cannot take the daemon down; plugins that own long-lived processes (`ttyd`) fit. In-process traits would need a rebuild per plugin; WASM is heavy and awkward for process-owning plugins. |
| PB-2 | **Observers and interceptors.** A plugin declares, per hook event, whether it observes (asynchronous stream, never delays Claude) or intercepts (the daemon waits for a verdict inside a shared budget, fail-open). | `flow` must block a tool call; `web` must never slow one down. One mode would either forbid the first or risk the second. |
| PB-3 | **Custom HTTP is a reverse-proxied mount**, `/v1/plugins/<name>/*` on the daemon's listener, admin-token authenticated, WebSocket passthrough included. | One port, one token, one place to add TLS later. Plugin-owned ports would each reinvent auth and the loopback story. |
| PB-4 | **A plugin is a package**: a tarball (or directory) with `mise.toml` at the root and a `hecaton-plugin.yaml` manifest naming the mise task that starts it, its hook subscriptions, capabilities and extra sandbox settings. | Plugins can be scripts, not only binaries; their tools are pinned exactly through mise like everything else; `ttyd` is a dependency of `web`, not of hecaton. |
| PB-5 | **Plugins are workloads of the reserved fleet `hecaton`**, crew `plugins`, driven through the existing `Materializer` and `AgentRunner` ports by the existing reconciler. | Sandboxing, tool installs, restart backoff, status, metrics and the model-based tests come for free; each plugin has a tmux window for its logs. The runner is untouched. |
| PB-6 | **The daemon config declares plugins**: `plugins.yaml` names each plugin's source, digest and daemon-level config; `serve` reconciles the installed set to it. `hecaton plugin install` is sugar that edits the file. | Envoy/Vault posture: the file is the source of truth, hecaton owns fetch and digest verification. mise installs tools, not directory trees. |
| PB-7 | **Fleet-level activation.** The fleet YAML's `plugins:` block (replacing the reserved `flow: {}`) carries per-agent plugin config, merged like every other map, still resolved client-side. | Per-agent behaviour (`flow` states) needs the three-level merge; the daemon must never merge settings layers itself (architecture spec §5). |
| PB-8 | **State is daemon-provided**: a namespaced KV with an opt-in vault path for secrets, plus a scratch directory for bulk data. | Plugins stay restartable processes; secrets have a vault path; recordings and the like do not go through an API. |
| PB-9 | **Attach is runner-neutral**: the daemon exposes a PTY stream for an agent over the protocol; tmux implements it today. | `web` never learns the tmux socket; Docker/Pod runners implement the same stream later. |
| PB-10 | **Transport is JSON over HTTP/1.1 on loopback, WebSockets for streams**; no gRPC. | Reuses `axum`, `ureq`, the proven `open_port` grant and the per-launch secret pattern. `tonic`/`prost`/`protoc` would be added for typed bidi streams WebSockets already give. |
| PB-11 | **Per-plugin capabilities (`needs`)** gate every host route; a call outside the manifest's list is 403. | Least privilege for a trusted-but-sandboxed process; makes a plugin's reach reviewable from its manifest. |
| PB-12 | **Interceptors fail open**, individually, inside a chain budget of 1.5 s within the existing 2 s hook timeout. | Continues the recorded fail-open posture: a plugin outage degrades a fleet, it never wedges an agent. |
| PB-13 | The **DSL "plugin for plugins" is named, not designed**. | No use case yet (YAGNI, as the architecture spec's "later" list). |

## 2. Package and manifest

A plugin package is a directory whose root holds `mise.toml`, the manifest
`hecaton-plugin.yaml`, and the plugin's files. It is distributed as
`<name>-<version>.tar.gz`. Nothing in a package is executed at install time.

```yaml
apiVersion: hecaton/v1
kind: Plugin
name: web                    # [a-z0-9-]+; unique per daemon
version: 0.1.0               # semver; informational and shown by `plugin list`
protocol: 1                  # host protocol major (§4); the daemon refuses others
start: serve                 # task in this package's mise.toml
hooks:
  observe: [SessionStart, SessionEnd, Stop]   # streamed, never delays Claude
  intercept: []                               # the daemon waits for a verdict
needs: [fleets, attach]      # host capabilities (§4.1); anything else is 403
routes: true                 # mounts /v1/plugins/web/* (§6)
sandbox: {}                  # nono-mirroring YAML merged over hecaton's base (§5)
```

Rules, validated at install:

- `name`, `version`, `protocol`, `start` are required; `hooks`, `needs`,
  `routes`, `sandbox` default to empty, empty, `false`, `{}`.
- Every event under `hooks` is one of the nine hook events hecaton wires
  (`HOOK_EVENTS`); an event may appear in both lists.
- `needs` values are exactly the capability names of §4.1.
- `mise.toml` exists, every `[tools]` entry is an exact version (developer
  environment rule; same check as fleet `tools:`), and `[tasks.<start>]`
  exists.
- The manifest is `deny_unknown_fields`; errors are
  `hecaton-plugin.yaml: hooks.intercept: unknown event "Foo"`.

Installed packages live under `$XDG_DATA_HOME/hecaton/plugins/<name>/<digest12>/`
(the first twelve hex digits of the tarball's sha256), files 0444 and
directories 0555, so a package is immutable and two versions coexist.

### 2.1 Daemon config

`$XDG_CONFIG_HOME/hecaton/plugins.yaml` is the declarative load list. Its order
is the interceptor order (§4.3).

```yaml
plugins:
  - name: flow
    source: https://github.com/acme/hecaton-plugin-flow/releases/download/v0.1.0/flow-0.1.0.tar.gz
    sha256: 3f2a…                    # required for URL and tarball sources
  - name: web
    source: ./plugins/web-0.1.0.tar.gz   # relative to this file
    sha256: 9c41…
    config: { title: "payments fleet" }  # daemon-level config, passed at hello
  - name: dev-thing
    source: ../src/hecaton/target/plugins/dev-thing   # a directory: used in place, no digest
```

`source` is a `https://` URL, a tarball path, or a directory path. A directory
is used in place, never copied, and carries no digest; that is how the in-tree
plugins run in development and the e2e. `name` must equal the manifest's
`name`, and names are unique in the file.

**URL sources are declared but not yet installable.** This build's `ureq`
carries no TLS provider (Phase 3 spec P3-1 forbids adding one), so
`load_plugins_file` and `hecaton plugin install` reject an `https://` source
with "URL sources need a TLS-enabled build; use `plugin package` and a tarball
path". `fetch`, `Source::Url` and the digest plumbing stay in place and are
enabled when a TLS provider arrives; until then an operator packages the plugin
and declares the tarball.

**Sync** (`serve` at start, and `hecaton plugin sync` against a live daemon)
reconciles the installed set to the file: fetch and verify anything missing
(URL fetch through `ureq`, 60 s timeout, 64 MiB cap, digest checked before
unpacking, unpack rejecting absolute paths, `..`, symlinks and hard links),
validate every manifest, then hand the resulting `Vec<ResolvedPlugin>` to the
plugin fleet (§5.2). A plugin removed from the file is stopped and its record
kept until `plugin remove --purge`. `serve` refuses to start on a digest
mismatch, an unknown protocol, a manifest error or a duplicate name; `sync`
reports the same errors and changes nothing.

### 2.2 Fleet YAML

The settings block's reserved `flow: {}` becomes `plugins: {}`: a map from
plugin name to a passthrough JSON object, merged fleet → crew → agent like every
other map (`null` deletes). Resolution stays client-side. `hecaton-config`
validates only that keys are valid names and values are objects.

```yaml
defaults:
  plugins:
    web: { enabled: true }
crews:
  backend:
    agents:
      alice:
        plugins:
          flow: { initial: working, states: { … } }   # see §8.1
```

The daemon rejects an `up` or `update` that names an unloaded plugin with 400
`plugins.web: plugin not loaded`. For every loaded plugin and every agent whose
resolved block mentions it, the daemon calls `activate` (§4.2); a rejection
fails the request with 400 `plugins.flow: <plugin's message>`. Agents whose
block does not mention a plugin are never activated for it, and interceptors
are only consulted for agents they are active for.

## 3. `hecaton-core` and `hecaton-api` additions

```rust
// hecaton-api
pub struct PluginManifest { name, version, protocol: u32, start, hooks: HookSubscriptions,
                            needs: BTreeSet<Capability>, routes: bool, sandbox: Value }
pub struct HookSubscriptions { observe: BTreeSet<String>, intercept: BTreeSet<String> }
pub enum Capability { Fleets, Actions, Attach, Kv }
pub struct PluginStatus { name, version, phase: AgentPhase, listen: Option<String>,
                          routes: bool, active_agents: u32 }
pub struct HelloRequest { name, version, protocol, listen }   // listen: "127.0.0.1:<port>" or a unix path
pub enum PluginAction { SendText { text, submit: bool }, Restart, Stop }
// AgentSettings: `flow: Value` → `plugins: BTreeMap<String, Value>`

// hecaton-core
pub struct ResolvedPlugin { pub id: AgentId /* hecaton/plugins/<name> */, pub package: PathBuf,
                            pub manifest: PluginManifest, pub config: Value, pub digest: Option<String> }
impl ResolvedPlugin { pub fn hash(&self) -> String }   // package, manifest, config: a change restarts it
pub trait Materializer { …; fn materialize_plugin(&self, plugin: &ResolvedPlugin, secret: &str)
                              -> Result<LaunchPlan, MaterializeError>; }
pub trait AgentRunner { …; fn attach(&self, agent: &AgentId) -> Result<Box<dyn PtyStream>, RunnerError>; }
pub trait PtyStream: Read + Write + Send { fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()>; }
pub struct Outcome { pub response: Value, pub actions: Vec<PluginAction> }   // P3-6 reversed
pub const RESERVED_FLEET: &str = "hecaton";   // FleetName::from_str rejects it for user fleets
```

`FleetName` parsing rejects `hecaton` with `name "hecaton" is reserved for the
daemon's plugins`; the daemon constructs it through a private constructor. The
reconciler is unchanged: it plans over ids, hashes and phases, and
`ensure_agent` already takes a `LaunchPlan`. `PassThrough` stays as the handler
used when no plugin intercepts.

## 4. Host protocol

Version 1. JSON over HTTP/1.1, loopback, WebSockets for streams. Every body is
a JSON object; every non-2xx is `{ "error": … }`. The `hecaton-plugin-sdk`
crate (§7) wraps both directions for Rust; `docs/plugin-protocol.md` is the
contract for every other language and is kept in step with the SDK by the
protocol conformance test (§10).

### 4.1 Plugin → daemon

Base `HECATON_API_URL` (the daemon's `http://127.0.0.1:<port>`), bearer
`HECATON_PLUGIN_TOKEN`: 32 random bytes hex, minted per launch, delivered
through the nono profile environment exactly like an agent's hook secret,
constant-time compared, 401 for unknown token or unknown plugin alike. Routes
under `/v1/plugin-host/`, each gated by the manifest's `needs`; a call outside
it is 403 `capability "actions" not declared in hecaton-plugin.yaml`.

| Route | Capability | Semantics |
|---|---|---|
| `POST hello` `{ name, version, protocol, listen }` | always | must match the record; sets the plugin `Ready`, stores `listen`, replies `{ config }` with the daemon-level config. Idempotent; a second hello replaces `listen`. |
| `GET fleets` · `GET fleets/{f}` | `fleets` | the same `FleetRecord` snapshots the CLI reads, secrets excluded |
| `GET fleets/watch` (WS) | `fleets` | one text frame per published snapshot change, any fleet |
| `POST agents/{id}/actions` `{ action: send_text, text, submit }` · `{ action: restart }` · `{ action: stop }` | `actions` | executed through the runner on a spawned task; 404 for an agent the plugin is not active for |
| `GET agents/{id}/attach` (WS) | `attach` | binary frames are PTY bytes both ways; text frames are `{ "resize": { "cols", "rows" } }`; closes when the agent's window dies |
| `GET kv/{key}` · `PUT kv/{key}` · `DELETE kv/{key}` · `GET kv?prefix=` | `kv` | body is raw bytes ≤ 1 MiB; `PUT …?secret=true` stores through the vault; keys `[A-Za-z0-9._/-]{1,200}` |

KV lives under `$XDG_STATE_HOME/hecaton/plugins/<name>/kv/`, one 0600 file per
key (secret ones sealed like `secrets.enc`), atomic writes. The scratch
directory `$XDG_STATE_HOME/hecaton/plugins/<name>/scratch/` needs no call: its
path arrives as `HECATON_PLUGIN_SCRATCH` and is read-write in the profile. Both
survive restarts and `plugin remove`; only `--purge` deletes them.

### 4.2 Daemon → plugin

At the `listen` address from `hello`. 5 s timeout unless stated.

| Route | Semantics |
|---|---|
| `POST /v1/activate` `{ agent, config }` | the agent's resolved config for this plugin; non-2xx fails the `up`/`update` with the body's `error` |
| `POST /v1/deactivate` `{ agent }` | on `down`, on removal from the spec, and on a config change (followed by a new `activate`) |
| `POST /v1/events` `{ events: [HookEvent] }` | observer batch, at most 64 events, oldest first; 2xx acknowledges, anything else is logged and counted |
| `POST /v1/intercept` `{ event, response_so_far, deadline_ms }` → `{ response }` | synchronous verdict (§4.3); timeout is `deadline_ms` |
| `GET /v1/health` | 2xx or the plugin is marked `Degraded` in its status message (never restarted for it) |
| `GET /v1/metrics` | Prometheus text, re-exposed by the daemon (§9) |
| `/v1/routes/*` | what `/v1/plugins/<name>/*` proxies to (§6) |

The daemon sends `activate` for every active agent again after each `hello`,
so a restarted plugin recovers its activations without persisting them.

### 4.3 Interceptor chain and observer delivery

For a hook event on agent `a`, the daemon walks the load-list order over the
plugins that intercept that event and are active for `a`. Each receives the
event and `response_so_far` (initially `{}`) and returns the new response; the
last one is written to Claude verbatim. The chain shares a budget of 1500 ms
inside the 2 s hook timeout: each call gets `deadline_ms` = what remains. A
plugin that times out, refuses the connection, or returns a non-2xx or a
non-object is skipped, `response_so_far` is unchanged, the failure is counted
with a `reason` label, and the chain continues. `Outcome.actions` collects the
`send`/`restart`/`stop` a verdict may carry (§8.1) and the daemon executes them
after writing the response, as the architecture spec §8 intended.

Observers get a per-plugin delivery task with a bounded queue of 1024 events.
Events for one agent keep their order; batches are cut at 64 events or 100 ms.
On overflow the oldest event is dropped and `hecaton_plugin_events_dropped_total`
is incremented. A plugin that is not `Ready` receives nothing and its queue is
cleared at the next `hello`; catching up on missed events is not offered.

## 5. Plugins as workloads

### 5.1 Materialization and sandbox

A plugin is materialized like an agent under
`$XDG_STATE_HOME/hecaton/plugins/<name>/`:

```
plugins/<name>/
  home/            HOME, XDG_CONFIG_HOME…, TMPDIR=home/tmp (0700)
  nono/            nono's own $HOME (its state root), as for agents
  kv/  scratch/    §4.1
  (no mise.toml copy — the package's own is used in place, see §15)
  nono-profile.json  0600; carries HECATON_PLUGIN_TOKEN
  launch.sh        cd <package>; exec nono run --profile … -- mise run <start>
  logs/
```

`mise install` runs with the shared `MISE_DATA_DIR` so tools are shared with
agents when versions match; `MISE_CEILING_PATHS` is the package's parent
directory plus the plugin's home, so mise's upward config walk sees neither
above the package nor above the plugin's own `$HOME` (see §15). The base
profile grants: read on `/usr`, `/lib`, `/lib64`, `/bin`, `/etc`, the mise
data dir, the package directory (which holds its own `mise.toml`), and the
hecaton and mise binaries; read-write on `home/` and `scratch/`, and **no**
grant on `kv/` (KV goes through the API); `network.open_port: [<daemon port>]`;
`environment.set_vars` = `HOME`, `XDG_*`, `TMPDIR`, `PATH`,
`HECATON_API_URL`, `HECATON_PLUGIN_NAME`, `HECATON_PLUGIN_TOKEN`,
`HECATON_PLUGIN_SCRATCH`, with `deny_vars: ["*"]`. The manifest's `sandbox`
merges over the base with the agent rules: hecaton's required grants win,
conflicting user values are rejected at sync with
`hecaton-plugin.yaml: sandbox.filesystem.write: "/etc" conflicts with a required grant`.
The profile is checked with `nono profile validate` before launch.

### 5.2 The plugin fleet

The daemon owns one extra actor for the fleet `hecaton`, crew `plugins`, whose
"spec" is the synced `Vec<ResolvedPlugin>`. It reuses the actor and reconciler
of the Phase 3 spec verbatim: `ResolvedPlugin::hash` drives restarts on package,
manifest or config change; `observe` gives process liveness; backoff and the
resync cadence apply; the tmux session is `hecaton-plugins`, one window per
plugin. Differences from a user fleet: it is never `down`ed by the API, it is
not stored in `fleets/` (the config file is its record; `Ready`/`listen` are
in-memory), its secrets index holds plugin tokens instead of hook secrets, and
`hello` is its readiness event as `SessionStart` is an agent's. `hecaton status
hecaton` renders it; `hecaton plugin list` is that view with `listen` and
`routes` columns.

`PluginHost` (`hecaton-server/src/plugins/`) owns: sync (§2.1), the plugin
actor handle, the token index, the `listen` map, the per-plugin observer
queues, the interceptor chain (which is the daemon's `EventHandler`), the
activation map (agent → plugins), the KV store, and the proxy (§6). Fleet
actors call `PluginHost::activate_all(fleet)` from the `Apply` path before
their pass and `deactivate_all` on `Down`; a rejected activation fails the
request before anything is materialized.

## 6. HTTP mount

`ANY /v1/plugins/{name}/{*rest}` on the daemon's router, authenticated by the
admin bearer token like every other API route. The daemon forwards to
`<listen>/v1/routes/{rest}` with the query string, method, body (1 MiB limit)
and headers minus hop-by-hop ones and `Authorization`, adds
`X-Hecaton-Forwarded-Prefix: /v1/plugins/<name>` so plugins can build links,
and streams the response back. A request with `Upgrade: websocket` is upgraded
on both sides and the two byte streams are copied until either closes. 404
`plugin "x" has no routes` when `routes: false` or the plugin is unknown, 503
`plugin "x" is not ready` before `hello`. The proxy is `hyper` + `hyper-util`
client-side, which `axum` already depends on; no new HTTP client crate.

## 7. `hecaton-plugin-sdk`

A library crate with two halves: `Host` (typed client for §4.1, including
`attach` returning a `PtyStream` over `tungstenite`) and `Plugin` (an `axum`
router for §4.2 that a plugin fills by implementing a trait with default
no-op methods: `activate`, `deactivate`, `observe`, `intercept`, `metrics`,
`routes`). `Plugin::serve()` binds `127.0.0.1:0`, sends `hello`, and runs. The
SDK reads only the `HECATON_*` variables of §5.1, in one `Env::from_env`
constructor, so plugins stay testable with an injected environment. It also
provides `hecaton_plugin_sdk::testing::FakeHost`, an in-process host used by
plugin unit tests and by the server's own tests through the same wire format.

## 8. In-tree plugins

Both are workspace crates on the SDK, packaged by `mise run package-plugins`
into `target/plugins/<name>/` (a directory source, §2.1) and used in place by
the e2e. They are the only plugins this spec designs.

### 8.1 `hecaton-plugin-flow`

The state machine of the original brief. Intercepts every hook event; needs
`actions`, `kv`. Per-agent config:

```yaml
plugins:
  flow:
    initial: working
    states:
      working:
        on:
          - event: Stop
            goto: review
            send: { text: "Run the tests and fix any failures.", submit: true }
          - event: PreToolUse
            match: { /tool_input/command: "^rm -rf" }
            respond: { decision: block, reason: "no recursive deletes" }
      review:
        on:
          - event: Stop
            goto: done
      done: {}
```

Semantics: a rule matches when the event name equals `event` and every `match`
entry — a JSON pointer into the payload and a regex anchored at both ends —
matches a string value at that pointer (non-strings are stringified with
`to_string`; a missing pointer never matches). The first matching rule of the
current state wins; no match means `{}` and no transition. `respond` is
returned as the verdict merged over `response_so_far` (top-level keys
replaced); `send` becomes a `send_text` action; `goto` moves the state, must
name a declared state, and `initial` must too. Regexes compile at `activate`
with a 10 KiB size limit; any error rejects the activation with the config
path (`states.working.on[1].match./tool_input/command: …`). The current state
per agent is stored in KV under `state/<agent>` so a plugin restart resumes;
`activate` with a changed config resets to `initial`. Metrics:
`hecaton_plugin_flow_state{fleet,crew,agent,state}` (1 for the current state)
and `hecaton_plugin_flow_transitions_total{fleet,crew,agent,from,to}`.

### 8.2 `hecaton-plugin-web`

Observes `SessionStart` and `SessionEnd`; needs `fleets`, `attach`; `routes:
true`; its `mise.toml` pins `ttyd`. `GET /v1/routes/` is an index page of the
agents whose per-agent config has `enabled: true`, with their phase from the
fleets snapshot. Opening an agent starts, on demand, `ttyd --port 0 --writable
--base-path <prefix>/agents/<id> -- hecaton-plugin-web attach <id>`; the
`attach` subcommand bridges the daemon's attach WebSocket to its own stdio and
forwards `SIGWINCH` as resizes. The plugin reverse-proxies `agents/<id>/*` to
that `ttyd` (WebSocket included), stops it after ten minutes without a client,
and the daemon proxies the whole plugin under `/v1/plugins/web/`. The plugin
never learns about tmux.

## 9. Metrics

Daemon-side additions to the Phase 3 registry:

| Metric | Type | Labels |
|---|---|---|
| `hecaton_plugin_events_total` | counter | `plugin, event, mode` (`observe`/`intercept`) |
| `hecaton_plugin_intercept_duration_seconds` | histogram | `plugin, event` |
| `hecaton_plugin_intercept_failures_total` | counter | `plugin, reason` (`timeout`/`connect`/`status`/`body`) |
| `hecaton_plugin_events_dropped_total` | counter | `plugin` |
| `hecaton_plugin_actions_total` | counter | `plugin, action` |
| `hecaton_plugin_proxy_requests_total` | counter | `plugin, status` |

Plugin phases are already `hecaton_agents{fleet="hecaton",crew="plugins"}`, and
`hecaton_hook_actions_total` gains its producer. On every `/metrics` request the
daemon fetches each `Ready` plugin's `GET /v1/metrics` with a 500 ms timeout in
parallel and appends the bodies whose every family name starts with
`hecaton_plugin_<name>_`; a body with any other family, or a timeout, is
dropped whole and counted in `hecaton_plugin_metrics_scrape_failures_total{plugin}`
(a seventh daemon-side counter). The
`hecaton_flow_*` names the architecture spec reserved are therefore
`hecaton_plugin_flow_*` (§12).

## 10. CLI

- `hecaton plugin install <path|url> [--sha256 <hex>]` — computes or checks the
  digest, appends an entry to `plugins.yaml` (fails if the name exists), runs
  `sync`. `--sha256` is required for URLs.
- `hecaton plugin sync` — `POST /v1/plugins/sync` against the live daemon;
  prints what was installed, stopped and left alone.
- `hecaton plugin list` — name, version, phase, listen, routes, active agents.
- `hecaton plugin remove <name> [--purge]` — removes the entry, syncs; `--purge`
  also deletes `plugins/<name>/` (kv, scratch, home) and the installed packages.
- `hecaton plugin package <dir> [--out <file>]` — validates the manifest and
  `mise.toml`, writes the tarball, prints `sha256`.

`serve` fails fast with the §2.1 errors. `up`/`update` print
`plugins.<name>: <message>` for activation rejections in the existing
config-path style.

## 11. Security, errors, docs, testing

**Threat model** (`docs/THREAT-MODEL.md`) gains a row: *plugin ↔ daemon*,
trust "operator-installed, sandboxed"; controls: nono profile from the base
plus the manifest's `sandbox`, `needs` enforced per route, per-launch token
carried only in the 0600 profile and the `Authorization` header, admin token
required on the proxy, 1 MiB bodies, KV secrets through the vault, no grant on
`kv/`, `server/token` or the state root. Accepted risks recorded: a plugin with
`actions` can drive any agent it is active for; a plugin with `fleets` sees
every fleet's spec (secrets excluded); interceptor fail-open means a dead
`flow` plugin stops blocking. Package unpacking is treated as untrusted input
(§2.1 rules) and fuzzed alongside the YAML parser.

**Errors.** `PluginError` (`thiserror`) in `hecaton-server/src/plugins/` with
config-path messages (`plugins.yaml: plugins[1].sha256: mismatch (expected …, got …)`,
`hecaton-plugin.yaml: needs: unknown capability "root"`); `ApiError` mappings for
400/403/404/503 above. The SDK's errors are `thiserror` too; only the plugin
binaries use `anyhow`.

**Docs.** `docs/plugin-protocol.md` (the wire contract, §4 and §6, versioned
with `protocol`). `ARCHITECTURE.md`: the plugin piece, the reserved fleet,
PB-5, PB-6, PB-12. `AGENTS.md`: `package-plugins` task; gotchas as found
(reserved name, directory sources, metrics prefix rule). `README.md`: a
`plugins.yaml` example and status "Spec B (plugins) in progress". Architecture
spec: dated addendum pointing at §12 here. The Phase 3 spec's "Next: Spec B
brainstorm" line is left as history.

**Testing.**

| Layer | What | Where |
|---|---|---|
| unit | manifest parse/validate; `plugins.yaml` load and sync planning; digest check and unpack rejections; `FleetName` reserved; `needs` gating; chain merge and budget; observer batching, ordering and drop; KV round trip and secret path; proxy header filtering; flow rule matching, regex limit, state persistence; attach frame encoding; SDK `Env::from_env` | in-module |
| property (`proptest`) | the chain's final response equals the fold over the non-failing plugins regardless of which fail; every KV `put` then `get` round-trips arbitrary bytes and keys; unpack never writes outside the target for arbitrary tar entries | in-module |
| protocol conformance | the SDK `Plugin` router and the server's client agree with `docs/plugin-protocol.md` fixtures (recorded request/response JSON under `docs/plugin-protocol/`); the SDK `Host` against the real router | `hecaton-plugin-sdk/tests`, `hecaton-server/tests` |
| server integration | a fake plugin built on the SDK runs in-process against `PluginHost` with `FakeMaterializer`/`FakeRunner`/`FakeClock`: hello readiness and re-activation, activation rejection failing `up`, intercept timeout fail-open, WebSocket echo through the proxy, sync adding and removing | `hecaton-server/tests/plugins_it.rs` |
| runtime integration (`test-it`) | materialize and launch a real package under nono: it reaches `hello`, cannot read outside its grants, its mise tools resolve, a manifest `sandbox` conflict is rejected | `hecaton-runtime/tests` |
| e2e | `plugins.yaml` with directory sources for `flow` and `web`; `up` with both enabled; `fake-claude` gains a stdin reader that appends what tmux sends to `$HOME/fake-claude.stdin`, so the test asserts flow's `send_text` arrived and a `PreToolUse` block verdict came back through the HTTP hook; `GET /v1/plugins/web/` lists the agent; the attach WebSocket streams bytes | `hecaton/tests/e2e.rs` |
| fuzz | package unpacking; manifest YAML | nightly |

### 11.1 Verify at implementation time

| Assumption | Fallback |
|---|---|
| a nono profile can let the plugin bind a loopback listener (`network.open_port` or a listen-side equivalent) | **Verified 2026-09-06** (nono 0.75.0, e2e plugin_hello_journey): dev fake-plugin binds 127.0.0.1:0 inside the profile and reports it in hello; the Unix-socket fallback is not needed. |
| `ttyd --base-path` works behind two WebSocket proxies | `web` embeds xterm.js and a WS bridge and drops `ttyd` |
| tmux `attach-session` on the private socket gives a clean PTY for `attach` (via `portable-pty` or `openpty`) | `pipe-pane` for output and `send-keys` for input, no resize |
| hecaton's `hyper`/`hyper-util` WebSocket upgrade passthrough works for `axum`'s upgrade path | `tokio-tungstenite` on both sides with frame-level copying |

## 12. Corrections to earlier specs

| Earlier spec said | This spec says | Why |
|---|---|---|
| Architecture §11: Spec B is `hecaton-events` and the `flow` block | Spec B is the plugin mechanism; `flow` is the first in-tree plugin | generality: metrics, web terminals and future DSLs share one extension point |
| Architecture §3: `hecaton-events` crate | no such crate; `hecaton-plugin-sdk`, `hecaton-plugin-flow`, `hecaton-plugin-web` | the state machine is out of process |
| Architecture §5, `hecaton-api`: settings block field `flow: {}` | `plugins: { <name>: {…} }` | one block for every plugin |
| Architecture §8: `enum Action`, `Outcome { response, actions }`; Phase 3 P3-6 dropped `actions` | `PluginAction`, `Outcome { response, actions }` restored | a producer exists |
| Architecture §8: `hecaton_flow_state`, `hecaton_flow_transitions_total` | `hecaton_plugin_flow_state`, `hecaton_plugin_flow_transitions_total` | the metrics prefix rule keeps plugin families attributable |
| Phase 3 §2: `Materializer` and `AgentRunner` complete | plus `materialize_plugin` and `attach` | plugins as workloads; runner-neutral attach |
| Phase 3 §3.4: fleet names are any `[a-z0-9-]+` | `hecaton` is reserved | the plugin fleet |

## 13. Build order

Three mergeable phases, each fully tested before the next:

1. **Plugin workloads** — package format and manifest, `plugins.yaml` and sync,
   `plugin install|list|remove|package|sync`, `ResolvedPlugin`,
   `materialize_plugin`, the plugin fleet and `PluginHost` skeleton, `hello`
   readiness, the SDK's `Env`/`hello` half. Ends with a trivial SDK plugin
   reaching `Ready` in the e2e.
2. **Event protocol** — activate/deactivate, observers, the interceptor chain
   replacing `PassThrough`, `Outcome.actions`, actions, fleets, KV, metrics
   re-export, the full SDK, `hecaton-plugin-flow`. Ends with the flow e2e
   assertions.
3. **Proxy and attach** — the `/v1/plugins/*` mount with WebSocket passthrough,
   `AgentRunner::attach` on tmux, `hecaton-plugin-web`. Ends with the web e2e
   assertions.

## 14. Done when

- `mise run check` passes here and in CI with `HECATON_REQUIRE_TOOLS=1`, e2e
  included, within the five-minute budget.
- By hand: `plugins.yaml` with the two directory sources, `serve -d`, `up
  examples/payments.yaml` with `flow` and `web` enabled, a browser on
  `/v1/plugins/web/` shows a live Claude terminal, and a blocked `rm -rf` shows
  up in Claude as the flow plugin's reason.
- Every row of §11.1 has a recorded verdict.
- `cargo mutants -p hecaton-core` still reports no surviving mutants in
  `reconcile`.

## 15. Refinements from the phase 1 plan (2026-09-06)

- Plugins are driven through a synthetic `Fleet` (`hecaton_core::plugin_fleet`); the reconciler is unchanged and `PluginMaterializer` maps the ids back.
- The plugin token is the actor's per-agent hook secret, delivered through the `HookTarget` the executor already passes; `hello` authenticates with `Daemon::verify_secret`.
- The package's `mise.toml` is used in place, never copied: the sandboxed `mise run` finds it as the local config from the package cwd (no `MISE_GLOBAL_CONFIG_FILE` inside the sandbox; `MISE_CEILING_PATHS` = package parent + plugin home), while the daemon-side `mise trust`/`mise install` name it through `MISE_GLOBAL_CONFIG_FILE`.
- The tmux session is `hecaton/plugins`.
- The reserved name is enforced at the API and in `hecaton_config::resolve`, not in `FleetName`.
- `Materializer` gains `materialize_plugin` and `purge_plugin`.
- `PluginStatus` has no `active_agents` until Phase 2.
- `plugin install` syncs only when a daemon is running; `plugin remove --purge` requires one.
- Unpacked package directories are 0755 (files 0444/0555) so `--purge` is a plain `remove_dir_all`.
- `plugins.yaml` is written atomically by `plugin install|remove` (temp file + rename).
- `PluginError::Fetch` carries `url`, not `source`.
- URL sources are rejected at load and at `plugin install` until a TLS-enabled build (§2.1); a pinned URL whose digest is already unpacked is answered from `install_root` without fetching.
