# MetaStack Architecture

This document describes the v0.3/v1.0 direction for MetaStack structured
injection. The v0.2 runtime remains a YAML-driven DAG runner over `zellij-mcp`;
the v0.3 work adds a routing layer for structured agent communication.

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
  sends RoutingEnvelope to MetaStack
    MetaStack resolves target
    MetaStack selects backend
    backend injects message
    backend reports receipt, stream events, or completion
    MetaStack routes replies by correlation id
```

The main distinction is between agents MetaStack owns and agents it merely
finds:

- `spawn(agent_spec)`: MetaStack owns lifecycle and structured injection from
  birth.
- `inject_running(target, message)`: MetaStack falls back to zellij keystrokes
  for agents it did not spawn or cannot address structurally.

## Routing Model

The routing layer has four separate concepts:

- `RoutingRequest`: caller intent, target name, role, message, and correlation
  id.
- `ResolvedTargetHandle`: backend-specific address selected by discovery.
- `BackendCapabilities`: what the backend can preserve or report.
- `RouteEvent`: accepted, submitted, delta, completion, failure, degradation,
  approval, and timeout events routed by correlation id.

Keeping these concepts separate matters because the backends have different
lifecycle and delivery semantics. OpenCode can accept a prompt without
completion readback. Codex must stream a turn until completion. Zellij can type
text but cannot preserve message roles.

## Routing Envelope

Every structured injection should pass through one common envelope.

```text
origin            who sent the message
target            logical agent name or address
backend           opencode | codex | claude | zellij
role              user | agent | system
message           text payload for v0.3
cwd               project root used for target discovery
session_id        backend session id, when known
thread_id         backend thread id, when known
reply_to          where replies should be routed
correlation_id    stable id for matching async replies
```

The envelope is the stable MetaStack API. Backend-specific fields should stay
inside backend config or backend state unless they are needed for routing.

## Target Handles

Target discovery should resolve a logical agent name into a typed handle:

```text
OpenCode { session_id }
Codex    { thread_id }
Claude   { channel, member }
Zellij   { session, pane_id, submit_strategy }
```

`session_id` and `thread_id` should not remain loose untyped strings once target
discovery is implemented. The handle type defines which backend may use the
identifier.

## Prototype Scope

The current v0.3 prototype is narrower than the full envelope:

- `metastack inject` sends one-way `user` message turns only.
- `reply_to` is parsed from config and carried in the envelope, but no reply
  router exists yet.
- OpenCode and Codex are the implemented adapters.
- Claude/Huddle and zellij fallback are design contracts/stubs until their
  addressing and reply semantics are precise.
- Codex opens a WebSocket per prototype injection and keeps it open through turn
  completion. A persistent connection manager keyed by target is the next step.

## Backend Semantics

| Backend | Delivery | Completion readback | Role semantics | Target discovery |
|---|---|---|---|---|
| OpenCode | HTTP accepted | no | user prompt turn in prototype | `cwd -> session_id` |
| Codex | WebSocket turn | yes | user prompt turn in prototype | `cwd -> active cli thread_id` |
| Claude | Huddle channel | expected, protocol-defined | contract/stub | channel/member |
| Zellij | keystrokes | no reliable readback | not preserved | session/pane id |

Backends should report their capabilities before dispatch:

```text
preserves_role
has_completion_readback
is_lossy
```

`role` in the envelope is caller intent. A backend that cannot preserve that
role must either reject the route or report a degraded route event.

Fallback policy must be explicit:

```text
never
explicit_lossy
on_unavailable
```

Codex `cx` sessions should not silently degrade to zellij. Zellij fallback is
for raw or user-launched TUIs where the caller explicitly accepts lossy
keystroke injection.

## Route Events

Backends should report normalized events:

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

OpenCode usually returns `accepted`. Codex should produce `submitted`, optional
`delta` events, then `completed` or `failed`. Zellij can usually report only
`submitted` or `degraded`.

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
keep socket open
consume item deltas, turn/completed, thread/status/changed, errors
```

Codex is not fire-and-forget. Closing the socket after `turn/start` can deliver
the prompt but loses completion and readback.

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

Claude structured injection is through Huddle channels launched by `coh`.
Default `co` sessions are not assumed to be channel-enabled.

The Claude backend should model Huddle as the transport, not as the routing
abstraction. MetaStack still owns the routing envelope and target discovery.
This is not implemented in the current prototype.

### Zellij Fallback

Zellij remains the universal fallback:

```text
zellij-mcp send-text
```

Use this for raw or user-launched TUIs where no structured backend is available.
It is lossy: no typed roles, no delivery semantics, and no backend readback.

Codex interactive panes are especially fragile through keystrokes. Prefer the
Codex app-server backend for `cx` sessions.

This fallback remains a separate `inject_running()` primitive, not a structured
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
  zellij:
    type: zellij
    mcp_binary: /path/to/zellij-mcp
    session: main

agents:
  nixos-cx:
    backend: codex
    cwd: /home/andy/nixos
  vault-oc:
    backend: opencode
    cwd: /home/andy/vault

routes:
  default_reply_to: caller
```

Existing v0.2 task DAG config should continue to parse. Config v2 can be added
as a parallel schema before replacing the existing YAML shape.

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
4. Add `metastack inject` as a CLI path for one message.
5. Add lifecycle-owned `spawn(agent_spec)` so MetaStack can create agents with
   structured injection enabled from birth.
6. Replace per-injection Codex sockets with target-scoped connection managers
   and per-thread turn queues.
7. Integrate routing into DAG tasks once target discovery is stable.
8. Add Nix/Home Manager module support that renders config v2.
