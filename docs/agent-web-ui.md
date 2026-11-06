# `kctl agent`: a local chat UI that drives kcore

Status: **built**, in the `kctl` binary. There is nothing else to install: no Node, no npm package, no second executable. Two things are not tested against live services: the ChatGPT sign-in (it follows OpenAI's published flow and is covered with a fake token endpoint) and a real model driving a full VM create. See [Status](#status).

## What it is

An operator runs `kctl agent`. A browser tab opens on `http://127.0.0.1:<port>/#token=…`. The page is a chat in the shape of the OpenCode web UI: sessions on the left, the conversation in the middle, tool calls shown as cards. The operator picks a model (ChatGPT plan, OpenAI key, Claude key, DeepSeek key, OpenCode) and types "create a VM for the build runner". The model calls the kcore tools: advise, plan, apply, read, delete, operation, catalog and a generic rpc. When a tool needs an answer it opens a form with real inputs (pick lists filled from the cluster, not free text). Nothing is applied until the operator clicks a button the model cannot click. Every call reaches the controller over mTLS with the certificate `kctl` chose, so the controller audit log shows who did it.

The tools are the same ones [`kcore-mcp`](../kcore-mcp/README.md) offers to editors, written again in Rust inside `kctl` (same names and questions; the model-facing schemas drop `confirm`). `kcore-mcp` stays for Cursor, VS Code and Claude Desktop; the agent does not talk to it.

```bash
kctl agent
kctl agent --role vm-admin
kctl agent --port 8123 --no-browser
```

| Flag | Meaning |
|---|---|
| `--port N` | Port for the UI. 0 picks a free one. It also fixes the ChatGPT sign-in callback port. |
| `--no-browser` | Print the link, do not open it. |
| `--provisioning HOST:PORT` | kcore-provisioning address for provisioning RPCs. `KCORE_PROVISIONING` works too. |
| `--role read-only\|vm-admin` | Run as a dedicated `agent-<login>` operator. Created, granted the role and given a certificate on the first run (needs a cluster-admin connection and mTLS). Refuses an operator that already holds a stronger role. |

The global flags apply as for every other command: `-c`, `-s`, `-k`, `--tls-server-name`, `--as`. State lives in `~/.kcore/agent` (`KCORE_AGENT_HOME` overrides it): `auth.json` (0600), `chatgpt.json`, `host.json`, and `sessions/<id>.jsonl`.

## How it is built

All of it is `crates/kctl/src/agent/`, one process.

| Module | Job |
|---|---|
| `http.rs` | axum server on `127.0.0.1` only. Static UI embedded with `include_str!`, bearer-token API, one SSE stream per session, the sandboxed app frames, the ChatGPT callback. |
| `session.rs` | One chat. The model loop (25 steps at most), pending forms, cancel, app calls, transcript. |
| `gate.rs`, `forms.rs` | What the model may ask for, which calls need an approval card, and the checks on form answers. |
| `transcript.rs` | Append-only JSON lines. Session reload repairs a crash: unanswered forms are cancelled, an open turn is closed, every tool call gets a result. |
| `tools/` | The tools. `advise`, `plan`, `requests` (resource kinds to controller requests), `options` (cluster pick lists), `rpc` (generic calls), `apps`. |
| `tools/rpc.rs` | Generic gRPC. A descriptor set is built from `proto/*.proto` at compile time, so `kcore_rpc` can call any unary controller, node or provisioning method without hand-written stubs. Requests are checked against the descriptors before they leave. |
| `providers/` | OpenAI-compatible (OpenAI, DeepSeek, OpenCode), Anthropic Messages, and the ChatGPT Responses API. |
| `auth.rs`, `chatgpt.rs` | API keys, OpenCode key import, and the ChatGPT sign-in (PKCE, JWT checks, refresh, revoke). |

The private key stays in the `kctl` process. The browser holds a 256-bit session token, nothing else.

### Routes

Every `/api` request carries `Authorization: Bearer <token>`. The UI reads the token once from the URL fragment and keeps it in `sessionStorage`. `Host` and `Origin` must be `127.0.0.1:<port>` or `localhost:<port>`. No CORS, no cookies.

| Route | Role |
|---|---|
| `GET /`, `/app.js`, `/style.css` | The UI |
| `GET /api/cluster/summary?cluster=NAME` | Controller and operator CN for a cluster (the active one if no name) |
| `GET /api/clusters`, `PUT /api/clusters/active` | The kctl contexts, and remember which one new sessions use |
| `GET /api/providers`, `GET /api/providers/:id/models`, `PUT/DELETE /api/providers/:id/key` | Provider setup |
| `POST/DELETE /api/providers/chatgpt/signin` | Start or end the ChatGPT sign-in. `DELETE` revokes the refresh token |
| `GET /auth/callback` | The loopback return from OpenAI. No bearer token, because a browser redirect cannot carry one; the one-time `state` is the check |
| `GET/POST /api/import/opencode` | Look at, then copy, OpenCode API keys. Account logins are never copied |
| `GET/POST /api/sessions`, `GET /api/sessions/:id` | Session list and transcript |
| `POST /api/sessions/:id/messages` | Send a user turn |
| `GET /api/sessions/:id/events?after=N` | SSE: text deltas, tool calls and results, forms |
| `POST /api/sessions/:id/forms/:formId` | Answer, decline or cancel a form |
| `POST /api/sessions/:id/cancel` | Stop the turn |
| `POST /api/sessions/:id/apps`, `GET /apps/:key` | Fetch an app view a tool of this session offers, and serve it sandboxed |
| `POST /api/sessions/:id/call` | A tool call made by an app view. Read-only tools only |

### Approval

- The model never sees a `confirm` argument. The session sends `confirm: false` to `kcore_apply`, `kcore_delete` and `kcore_operation` whatever the model wrote, so the tool asks the operator itself and the UI shows an approval card.
- `kcore_rpc` with a method that is not a get, list, classify, check or plan, and `kcore_operation` with `install-node`, get an approval card from the session with the exact request. `confirm: true` is set only after the operator says yes. The `acknowledge` phrase for a node install is hidden from that card.
- Decline, cancel, Stop and closing the tab all mean "not approved". The tool result tells the model nothing changed.
- The model cannot choose where the call goes. `connection` is reduced to `node`; the controller, TLS settings and operator come from the context `kctl` resolved.
- Answers from a form come back to the model in the tool result as `Operator form answers`.

## Clusters

The contexts in `~/.kcore/config` are the clusters the agent can drive. The page has a cluster picker above "New session". A session is bound to one cluster when it starts, and that cluster is saved in its transcript, so a restored session goes back to the same cluster and a chat never mixes two. Changing the picker starts a new session on the chosen cluster.

The choice is remembered in `~/.kcore/agent/settings.json` (mode 0600). At startup the active cluster is that choice if the context still exists, otherwise the config's `current-context`, otherwise the first context by name. `kctl agent` prints it. With `--controller` or `-k` the agent uses exactly that connection and the picker is hidden.

Each new session reads the config and certificates again, so a re-issued certificate or CA is picked up by the next session. A running session keeps what it started with. With `--role`, the dedicated `agent-<user>` operator is created on each cluster the first time it is used. A cluster that cannot be connected to still shows up in the picker. Its tools answer with the reason.

## Providers

| Provider | Auth | Endpoint | Notes |
|---|---|---|---|
| OpenAI, ChatGPT plan | Sign in with ChatGPT, open-source flow | Responses API, `https://api.openai.com/v1` | PKCE S256. First sign-in uses `client_id=dynamic_agent_client`, `agent_name_hint=kcore`, a persistent `ext_agent_host_id` per machine. The callback must be a loopback URL on `127.0.0.1`, same path every time, port may vary: `http://127.0.0.1:<port>/auth/callback`. The issued `client_id` is saved per ChatGPT account and reused. Scopes: `openid profile email offline_access resource.invoke chatgpt.tokens.use.direct`; refuse to use the token for inference unless `chatgpt.tokens.use.direct` was granted. Requests set `store: false`, `stream: true`, send the whole history in `input`, put the system prompt in `instructions`, and group the tools in one `kcore` namespace; replayed `function_call` items carry the namespace. Fields the plan route rejects (`temperature`, `max_output_tokens`, `previous_response_id` and the rest) are never sent. A 401 triggers one refresh and retry. `subscription_sharing_usage_limit_exceeded` is shown as a usage-limit message. Plan usage needs a Go/Plus/Pro plan; the help page lists OpenCode as an approved integration, and the open-source flow is self-service. |
| OpenAI, API key | Key | Chat Completions | Fallback for Team/Enterprise accounts and CI. |
| Anthropic | API key only | Messages API | Anthropic's Claude Code legal page says third-party apps may not offer Claude.ai login or route requests through Free/Pro/Max credentials, and enforces it server-side (OpenCode removed its Anthropic OAuth plugin in 1.3.0, March 2026). Offer a Console API key. Do not ship a Claude subscription login. |
| DeepSeek | API key only | `https://api.deepseek.com`, OpenAI-compatible | `deepseek-chat` supports function calling. Check `deepseek-reasoner` tool support before listing it. |
| OpenCode Console | API key (`https://opencode.ai/auth`) | `https://opencode.ai/inference/openai/v1/chat/completions`, models at `/inference/v1/models` | One key reaches Claude, GPT, DeepSeek, Gemini, Kimi and others; Console translates between APIs. This is the supported way to use Claude without a Claude Console account. Free-tier models are restricted to OpenCode itself. Built from OpenCode's published docs; not run against a live key. |
| OpenCode Go | API key from a workspace with Go | `https://opencode.ai/inference/go/openai/v1/chat/completions` | Subscription plan with open models. The model-list URL (`/inference/go/v1/models`) is inferred; the default model is the fallback if it does not answer. |

Credential storage: `~/.kcore/agent/auth.json`, mode 0600, same shape as OpenCode's `auth.json` (`{ provider: { type: "oauth" | "api", ... } }`). Refresh tokens stay in that file; the browser never sees them. The agent sends only `{ provider, account, expiresAt }` to the UI.

The OpenAI sign-in runs on the agent's own HTTP server, not a separate listener, because the callback must be a loopback address and the agent already owns one. `kctl agent --port` therefore also fixes the callback port; the first sign-in and later sign-ins may use different ports, which the open-source flow allows as long as scheme, host and path stay the same.

## Forms

Forms stop the model from guessing image URLs, checksums, network names and SSH keys out of free text, and stop the operator from typing them wrong.

- Each question can name a source: `networks`, `ssh-keys`, `nodes`, `vms` or `node-images`. A question marked `restrict` becomes a pick list: the VM `network`, the container `network`, the PostgreSQL `targetNode`, the volume `vm`, the disk-layout `nodeId`. Options carry labels, for example a node shows as `node-103 (host-a, ready)`.
- A source without `restrict` keeps a free-text field and lists what exists in the explanation: the VM `auth` field lists stored SSH key names (a pasted public key still works), the VM `image` field lists images on the nodes.
- Numbers keep their minimum (`cpu`, `keep`).
- If the controller cannot be read, or the list is empty, the question stays as it was and says so. Each source is read once per ask, with an 8 second limit.
- Answers are checked again by the session before the tool sees them: required fields, choices, numbers, no unknown fields.

Not built: forms for `ceph-cluster` members and node disks, and `cluster-update` windows. They use text fields.

## MCP Apps

MCP Apps is the `io.modelcontextprotocol/ui` extension: a tool points at a `ui://` resource, the host renders that HTML in a sandboxed iframe, and the iframe talks to the host over JSON-RPC `postMessage`.

- `kcore_plan` points at `ui://kcore/plan`. The view shows the action (create, update, replace, unchanged), field changes with immutable ones marked, the spec for a create, warnings, the controller calls and the cluster state. Fields that look like secrets are masked.
- The agent is the host. It answers `ui/initialize`, sends `tool-input` and `tool-result`, resizes the frame on `size-changed`, and handles `tools/call`, `ui/open-link` (https only, with a confirm) and `ping`.
- The frame is `sandbox="allow-scripts"` without `allow-same-origin`, so it has an opaque origin and cannot read the page or its token. Its response carries `default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; sandbox allow-scripts`: no network, no frames, no storage. The URL is a random 48-hex key that expires after an hour.
- A view's `tools/call` is limited to `kcore_advise`, `kcore_plan`, `kcore_read` and `kcore_catalog`, with `connection` removed. A view cannot approve or change anything.

Not built: form views (`ui://kcore/<kind>-form`). It has been tried in a browser against the plan view only, not in ChatGPT, Claude or Cursor.

## Security

- Loopback only. The server binds `127.0.0.1`. There is no flag to listen elsewhere; use an SSH tunnel for remote access.
- A 256-bit session token in every API request, new on each run. The token is in the URL fragment, so it is not sent to the server in the request line or logged.
- The browser never holds cluster certificates, provider refresh tokens or `auth.json`.
- Human approval for every change, enforced in the session with the exact arguments displayed.
- Tool results are data. The system prompt says so, and a node install keeps its typed acknowledgement phrase in a form field, never prefilled.
- Tool output is redacted (`key-data`, cloud-init secrets, tokens) before it reaches the transcript or the UI.
- Provider calls leave the machine. The header shows which provider a session uses.
- With `--as` or `--role`, controller RBAC limits what the agent can do regardless of the model. A read-only operator gets a read-only agent.

## Distribution

`kctl` is the only artifact. The UI is compiled in, the descriptor set is compiled in, and TLS is rustls, so the release archive and `nix build .#kctl` already contain the agent.

## Status

- Built: the server, sessions and reload, the model loop, all eight tools, forms and cluster pick lists, the plan view and app host, ChatGPT sign-in, OpenAI, Anthropic, DeepSeek and OpenCode keys, OpenCode key import, `--role`.
- Tested with mock servers: every provider's streaming and error paths, the ChatGPT flow with a fake issuer and real JWT signatures, the HTTP API, the event stream, session reload after a crash.
- Not tested against live services: the ChatGPT sign-in against OpenAI, OpenCode Console and Go, and a real model creating a VM end to end.
- Not built: Ceph member and disk pickers, a usage display, an audit view in the UI.

## Open questions

- Should `kctl agent` refuse to run as `kctl` (cluster admin) without `--as` or `--role`? Refusing is safer; defaulting is friendlier for a single-operator lab. Today it uses the context identity.
- Does the ChatGPT open-source flow accept `agent_name_hint=kcore` without prior registration? The docs say registration is dynamic; the code follows them, but nobody has signed in with it yet. The credential file is not locked across processes, so two `kctl agent` runs on one machine can race a rotating refresh token.
- One agent per `kctl agent` run, or a long-lived agent the dashboard can embed? Embedding needs a different credential story (dashboard identity, not operator identity).
