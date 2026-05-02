# MetaStack Architecture

This document describes the v0.3/v1.0 direction for MetaStack structured
routing and message sending. The v0.2 runtime remains a YAML-driven DAG runner
over `zellij-mcp`; the v0.3 work adds a routing layer for structured agent
communication.

## Current Runtime

MetaStack currently runs as a CLI process:

```text
metastack
  reads metastack.yaml
  starts zellij-mcp from mcp_binary
  calls spawn-pane, send-text, and read-pane
  schedules a task DAG
  writes task artifacts
```

This path is still useful and should remain portable. It is also the fallback
for agents that are only reachable as terminal UIs.

## Target Runtime

MetaStack should become the owner of agent routing:

```text
caller
  sends target + message to MetaStack
    MetaStack resolves the target
    MetaStack builds an internal RoutingEnvelope
    MetaStack selects backend
    backend sends message
    backend reports whether the message was accepted
    MetaStack routes replies by correlation id
```

The main distinction is between agents MetaStack owns and agents it merely
finds:

- `spawn(agent_spec)`: MetaStack owns lifecycle and structured sending from
  birth.
- `send_running_lossy(target, message)`: MetaStack falls back to zellij keystrokes
  for agents it did not spawn or cannot address structurally.

## Routing Model

The routing layer has four separate concepts:

- `SendRequest`: caller intent, target name, message, and correlation id.
- `ResolvedTargetHandle`: backend-specific address selected by discovery.
- `BackendCapabilities`: what the backend can preserve or report.
- `RouteEvent`: future backend-specific events routed by correlation id.

Keeping these concepts separate matters because the backends have different
lifecycle and delivery semantics. OpenCode can accept a prompt over HTTP.
Codex can acknowledge `turn/start` over JSON-RPC. Zellij can type text but
cannot provide a structured receipt or preserve message roles.

## Topology

Active control messages use a strict tree:

```text
parent
  -> child
      -> grandchild
```

Children roll results up to their parent. Siblings do not send direct control
messages to each other. Shared or lattice-like behavior belongs in the
substrate layer: vault notes, files, Hindsight, Stack Underflow, and trace
indexes.

The protocol can represent arbitrary depth with a route path such as
`[andy-oc, sutro, bytedmd]`, but operational trees should stay shallow unless a
parent has real integration responsibility. Do not create an agent layer just
to mirror a directory such as `~/dev`.

## Routing Envelope

Every structured send should pass through one common internal envelope.

```text
origin            who sent the message
target            logical agent name or address
backend           opencode | codex | claude | zellij
role              internal role; v0.3 sends user-message turns only
message           text payload for v0.3
cwd               project root used for target discovery
session_id        backend session id, when known
thread_id         backend thread id, when known
reply_to          parent/caller reply route
correlation_id    stable id for matching async replies
```

The envelope is not the external CLI API. External callers target a logical
name such as `nixos-cx`; MetaStack resolves that name and builds the envelope.
Backend-specific fields should stay inside backend config or backend state
unless they are needed for routing.

## Target Handles

Target discovery should resolve a logical agent name into a typed handle:

```text
OpenCode { cwd, session_id? }
Codex    { cwd, thread_id? }
Claude   { channel, member }
Zellij   { session, pane_id, submit_strategy }
```

`session_id` and `thread_id` should not remain loose untyped strings in the
router. The handle type defines which backend may use the identifier. When a
concrete id is not configured, the typed handle carries the `cwd` needed for
backend-specific discovery.

## Prototype Scope

The current v0.3 prototype is narrower than the full envelope:

- `metastack send` sends one-way `user` message turns only.
- `reply_to` is parsed from config and carried in the envelope, but no reply
  router exists yet. It means "return to caller/parent", not arbitrary peer
  addressing.
- OpenCode and Codex are the implemented adapters.
- Claude/Huddle and zellij fallback are design contracts/stubs until their
  addressing and reply semantics are precise.
- Codex opens a WebSocket per prototype send and waits for the `turn/start`
  JSON-RPC response. It does not wait for agent turn completion.
- v0.3 is intentionally fire-and-forget after backend acceptance. Durable
  delivery, acknowledgements, retries, and async agent results belong in a
  later service/protocol layer.

## Backend Semantics

| Backend | Delivery | Send receipt | Role semantics | Target discovery |
|---|---|---|---|---|
| OpenCode | HTTP `prompt_async` | HTTP status | user prompt turn in prototype | `cwd -> session_id` |
| Codex | JSON-RPC `turn/start` | JSON-RPC response | user prompt turn in prototype | `cwd -> active cli thread_id` |
| Claude | Huddle channel | none assumed in v0.3 | contract/stub | channel/member |
| Zellij | keystrokes | none | not preserved | session/pane id |

Backends should report their capabilities before dispatch:

```text
preserves_role
has_delivery_receipt
is_lossy
```

`role` in the envelope is internal metadata. The v0.3 public command does not
accept role selection; adapters reject anything other than a normal user-message
turn.

Fallback policy must be explicit:

```text
never
explicit_lossy
on_unavailable
```

Codex `cx` sessions should not silently degrade to zellij. Zellij fallback is
for raw or user-launched TUIs where the caller explicitly accepts lossy
keystroke sending.

## Route Events

Future service-mode adapters may report normalized events:

```text
accepted
submitted
delta
completed
failed
degraded
needs_approval
timeout
```

These are not the v0.3 `send` receipt. `send` only reports that the selected
backend accepted the message. OpenCode can usually report `accepted`; Codex can
report `accepted` after `turn/start`; zellij can usually report only
`submitted` or `degraded`.

Do not rebuild reliable message delivery in this layer. If MetaStack needs
durable acks, retries, subscriptions, or long-running result streams, that
should happen in a later async-agent protocol layer rather than in the v0.3 CLI.

MetaStack should use `correlation_id` and `reply_to` to route responses rather
than forcing agents to guess transport-specific return paths.

## Backends

### OpenCode

OpenCode serve is fire-and-forget HTTP:

```text
GET  http://127.0.0.1:4096/session
POST http://127.0.0.1:4096/session/<id>/prompt_async
```

The request body is:

```json
{
  "parts": [
    {
      "type": "text",
      "text": "message"
    }
  ]
}
```

Target discovery should select the newest session whose `directory` matches the
target project root. Delivery returns before the agent finishes; OpenCode does
not provide completion readback through this primitive.

### Codex

Codex `cx` sessions use the app-server WebSocket:

```text
ws://127.0.0.1:4107
```

The canonical flow is:

```text
connect WebSocket
initialize with experimentalApi
send initialized notification
thread/list filtered by cwd
select active CLI thread
thread/resume to load the thread and attach this socket for events
turn/start
wait for the turn/start JSON-RPC response
```

Codex is not identical to fire-and-forget HTTP: MetaStack waits for the
`turn/start` JSON-RPC response so it knows the app-server accepted the message.
It intentionally does not keep the socket open for agent completion in v0.3.

Codex input is an array of user input objects:

```json
{
  "input": [
    {
      "type": "text",
      "text": "message",
      "text_elements": []
    }
  ]
}
```

Do not send OpenCode-style `{ "parts": [...] }` payloads to Codex.

### Claude

Claude structured sending is through Huddle channels launched by `coh`.
Default `co` sessions are not assumed to be channel-enabled.

The Claude backend should model Huddle as the transport, not as the routing
abstraction. MetaStack still owns the routing envelope and target discovery.
This is not implemented in the current prototype. Treat Huddle like transport
submission in v0.3 unless the channel API exposes a concrete acknowledgement;
do not imply remote turn completion.

### Zellij Fallback

Zellij remains the universal fallback:

```text
zellij-mcp send-text
```

Use this for raw or user-launched TUIs where no structured backend is available.
It is lossy: no typed roles, no delivery semantics, and no backend readback.

Codex interactive panes are especially fragile through keystrokes. Prefer the
Codex app-server backend for `cx` sessions.

This fallback remains a separate `send_running_lossy()` primitive, not a structured
provider in the current prototype.

## Config V2 Sketch

YAML remains the portable runtime format. A future Nix module can render this
shape to YAML or JSON.

```yaml
version: 2

backends:
  opencode:
    type: opencode
    base_url: http://127.0.0.1:4096
  codex:
    type: codex
    url: ws://127.0.0.1:4107
    model: gpt-5.5
    effort: xhigh
    approval_policy: never
    sandbox_policy:
      type: dangerFullAccess

agents:
  nixos-cx:
    backend: codex
    cwd: /home/andy/nixos
  vault-oc:
    backend: opencode
    cwd: /home/andy/vault
```

Existing v0.2 task DAG config should continue to parse. Config v2 can be added
as a parallel schema before replacing the existing YAML shape. Reply routing,
route paths, Claude/Huddle, and lossy terminal fallback are follow-up contracts,
not part of the minimal runnable example.

## Testing Strategy

The routing core should be tested without live agent processes:

- Envelope serialization and deserialization.
- Target resolution from static config.
- Backend request construction.
- OpenCode session selection and prompt body construction.
- Codex JSON-RPC request construction and active-thread selection.
- Fake backend implementing the common trait.

Live smoke tests should be opt-in because they depend on local services:

- OpenCode serve on `127.0.0.1:4096`.
- Codex app-server on `127.0.0.1:4107`.
- Huddle/coh channel availability.
- zellij session and pane ids.

## Migration Plan

1. Add the routing data model and backend trait.
2. Add OpenCode and Codex prototype backends.
3. Add static target discovery from config v2.
4. Add `metastack send` as a CLI path for one message.
5. Add lifecycle-owned `spawn(agent_spec)` so MetaStack can create agents with
   structured sending enabled from birth.
6. Replace per-send Codex sockets with target-scoped connection managers
   and per-thread turn queues.
7. Integrate routing into DAG tasks once target discovery is stable.
8. Add Nix/Home Manager module support that renders config v2.
