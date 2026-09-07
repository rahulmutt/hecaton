# Hecaton plugin host protocol

The wire contract between the daemon and a plugin (plugins spec §4, §11):
what a plugin in any language must send and answer. The
`hecaton-plugin-sdk` crate implements both halves for Rust; this document
and the fixtures under `docs/plugin-protocol/` are the contract for every
other language, and are replayed through the SDK and the daemon's client by
the conformance tests so the three cannot drift apart (§6).

## 1. Scope and versioning

Host protocol major **1**. A plugin's `hello` (§3) sends `protocol`; the
daemon refuses any value it does not speak. Both directions are JSON over
plain HTTP/1.1 on loopback — no TLS, no proxies. Every request and response
body is a JSON object, except the routes stated to carry a raw byte body
(`GET`/`PUT /v1/plugin-host/kv/{key}`, `GET /v1/health`, `GET /v1/metrics`).
Every non-2xx response is `{ "error": "<message>" }`
(`hello-bad-token.json`, `fleet-missing.json`, `activate-rejected.json`).
Every body, either direction, is capped at 1 MiB.

## 2. Environment

The daemon delivers four variables to a plugin's process through the nono
profile environment (plugins spec §5.1):

| Variable | Meaning |
|---|---|
| `HECATON_API_URL` | The daemon's base URL, `http://127.0.0.1:<port>`, no trailing slash. Every plugin → daemon route in §3 is relative to it. |
| `HECATON_PLUGIN_NAME` | This plugin's name, as declared in `hecaton-plugin.yaml` and `plugins.yaml`. |
| `HECATON_PLUGIN_TOKEN` | The bearer for every plugin → daemon call: `Authorization: Bearer <token>`. A wrong or missing token is 401 `{ "error": "unknown plugin or bad token" }` on every route under `/v1/plugin-host/`, `hello` included (`hello-bad-token.json`). |
| `HECATON_PLUGIN_SCRATCH` | A read-write scratch directory; no API call needed. |

## 3. Plugin → daemon

Base `HECATON_API_URL`, path prefix `/v1/plugin-host/`, bearer
`HECATON_PLUGIN_TOKEN` on every call. Each route beyond `hello` is gated by
a capability the manifest's `needs` must declare (`fleets`, `actions`,
`attach`, `kv`); a call outside what is declared is rejected before the
route runs, with a client error naming the missing capability.

| Route | Capability | Request | Response | Status | Fixture |
|---|---|---|---|---|---|
| `POST hello` | always | `{ name, version, protocol, listen }` | `{ config }` | 200 | `hello.json` |
| `POST hello`, bad/missing token | always | same | `{ error }` | 401 | `hello-bad-token.json` |
| `GET fleets` | `fleets` | — | `[FleetRecord]` | 200 | `fleets.json` |
| `GET fleets/{name}` | `fleets` | — | `FleetRecord` | 200 | (shape as in `fleets.json`'s `response[0]`) |
| `GET fleets/{name}`, unknown name | `fleets` | — | `{ error }` | 404 | `fleet-missing.json` |
| `POST agents/{fleet}/{crew}/{agent}/actions` | `actions` | one of the three action shapes below | `{}` | 200 | `action.json` |
| `POST agents/…/actions`, agent not active for this plugin | `actions` | — | `{ error }` | 404 | (same status as `fleet-missing.json`) |
| `GET kv?prefix=` | `kv` | — | `{ keys }` | 200 | `kv-list.json` |
| `GET kv/{key}` | `kv` | — | raw bytes | 200 | `kv-get.json` |
| `GET kv/{key}`, unknown key | `kv` | — | `{ error }` | 404 | (same status as `fleet-missing.json`) |
| `PUT kv/{key}?secret=<bool>` | `kv` | raw bytes | `{}` | 200 | `kv-put.json` |
| `DELETE kv/{key}` | `kv` | — | `{}` | 200 | (same success shape as `PUT`) |

A `FleetRecord` is `{ spec: { name, crews }, generation, desired: { state },
stopped, status: { generation, observed_generation, phase, agents } }`
(`fleets.json`); secrets are excluded. `fleets/{name}` answers the same
shape for one fleet.

**Actions** (`POST agents/{fleet}/{crew}/{agent}/actions`) are one of:

- `{ "action": "send_text", "text": <string>, "submit": <bool> }`
  (`action.json`)
- `{ "action": "restart" }`
- `{ "action": "stop" }`

**KV**: keys match `[A-Za-z0-9._/-]{1,200}` with no `..` segment. `PUT` and
`GET` bodies are raw bytes, content-type `application/octet-stream`;
`?secret=true` on `PUT` stores the value through the daemon's vault.
`?prefix=` on the list route filters returned `keys` by prefix
(`kv-list.json`).

## 4. Daemon → plugin

At the `listen` address the plugin's `hello` gave (§3), plain HTTP, 5 s
timeout unless stated otherwise.

| Route | Request | Response | Status | Fixture |
|---|---|---|---|---|
| `POST /v1/activate` | `{ agent, config }` | `{}` | 200 | `activate.json` |
| `POST /v1/activate`, rejected | `{ agent, config }` | `{ error }` | 400 | `activate-rejected.json` |
| `POST /v1/deactivate` | `{ agent }` | `{}` | 200 | (same success shape as `activate.json`) |
| `POST /v1/events` | `{ events: [HookEvent] }` | `{}` | 200 | `events.json` |
| `POST /v1/intercept` | `{ event, response_so_far, deadline_ms }` | `{ response, actions }` | 200 | `intercept.json` |
| `GET /v1/health` | — | raw bytes | 200 | `health.json` |
| `GET /v1/metrics` | — | raw bytes (Prometheus text) | 200 | `metrics.json` |

**`activate`**: `config` is the agent's resolved settings for this plugin.
A non-2xx rejects the agent's activation; the operator sees it as
`crews.<c>.agents.<a>.plugins.<name>: <message>`, `<message>` being the
body's `error` (`activate-rejected.json`).

**`deactivate`**: sent on `down`, when the agent's spec drops the plugin,
and on a config change (immediately followed by a new `activate`).

**`events`**: an observer batch, at most 64 events, oldest first; a 2xx
acknowledges, anything else is logged and counted, and there is no
catch-up for what was missed while the plugin was not `Ready`. A
`HookEvent` is `{ agent, name, session_id?, received_at, payload }`
(`events.json`; `session_id` is omitted when absent, present in
`intercept.json`'s event).

**`intercept`**: `response_so_far` is the chain's response before this
plugin (`{}` for the first); `deadline_ms` is what remains of the chain's
1500 ms shared budget. The reply's `response` must be a JSON object
(`intercept.json`); `actions` is the list of §3's action shapes to run
after the response is written, and is omitted when empty. A plugin that
times out, refuses the connection, answers a non-2xx, or answers a
non-object `response` is skipped for this event: `response_so_far` passes
through unchanged and the chain continues (fail-open).

**`health`**: a 2xx keeps the plugin's status clear; anything else marks it
degraded. Never causes a restart.

**`metrics`**: Prometheus text. Every family name — `# TYPE`/`# HELP` lines
and samples alike — must start with `hecaton_plugin_<name>_`
(`metrics.json`'s `hecaton_plugin_flow_state`); the daemon drops a body
that breaks this rule instead of re-exposing it.

## 5. Activation lifecycle

An agent's `(agent, plugin)` pair is one of three states, visible in
`hecaton status` and the CLI's wait: **pending** (recorded, `activate` not
yet sent because the plugin was not `Ready`), **active** (the plugin
answered 2xx), or **rejected** (the plugin's non-2xx, with its message).

Every `hello` re-sends `activate` for every pair the daemon still
considers active or pending, pending or active alike, so a restarted
plugin recovers its activations without the daemon persisting anything
about them. A pair is deactivated when its fleet goes `down`, when an `up`
or `update` drops the plugin from the agent's spec, or on a config change
for that pair (immediately followed by the new `activate`). A `rejected`
pair does not stop the agent — the fleet keeps running, and a plugin that
never activates simply never runs for that agent.

## 6. Conformance

`docs/plugin-protocol/*.json` holds fourteen fixtures, one JSON object
each: `{ route, direction, request, status, response }` for
`daemon-to-plugin` and most `plugin-to-daemon` routes; `raw` (base64)
replaces `request`/`response` for the kv byte bodies, `health.json` and
`metrics.json`; `hello-bad-token.json` additionally carries a top-level
`"token"` to send instead of the real one.

Two tests replay every fixture:

- `crates/hecaton-plugin-sdk/tests/conformance.rs` — every
  `daemon-to-plugin` fixture through the SDK's `router` (a real HTTP round
  trip to a `Reference` plugin), and every `plugin-to-daemon` fixture
  through the SDK's `Host` against `hecaton_plugin_sdk::testing::FakeHost`.
- `crates/hecaton-server/tests/protocol_it.rs` — the daemon's own
  `PluginClient` sends the `activate`, `activate-rejected`, `events` and
  `intercept` fixtures' `request` bodies to
  `hecaton_server::testing::stub_plugin` and checks the bytes it recorded,
  and that the parsed verdict matches the fixture's `response`.
