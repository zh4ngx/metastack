# MetaStack Architecture

This document describes the current MetaStack structured-routing prototype and
the direction toward service-mode routing. The original runtime remains a
YAML-driven DAG runner over `zellij-mcp`; structured send now includes
OpenCode, Codex, and narrow Claude/Huddle adapters.

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

This path is still useful and should remain portable. It is separate from
structured send; terminal fallback for `metastack send` is design-only and is
not implemented yet.

## Target Runtime

MetaStack should become the owner of agent routing:

```text
caller
  sends target + message to MetaStack
    MetaStack resolves the target
    MetaStack builds an internal RoutingEnvelope
    MetaStack selects backend
    backend sends message
    backend reports whether the message was submitted or accepted
    MetaStack routes replies by correlation id
```

The main distinction is between agents MetaStack owns and agents it merely
finds:

- `spawn(agent_spec)`: MetaStack owns lifecycle and structured sending from
  birth.
- `send_running_lossy(target, message)`: future design-only primitive where
  MetaStack could fall back to zellij keystrokes for agents it did not spawn or
  cannot address structurally.

## Routing Model

The routing layer has four separate concepts:

- `SendRequest`: caller intent, target name, message, and correlation id.
- `ResolvedTargetHandle`: backend-specific address selected by discovery.
- `BackendCapabilities`: what the backend can preserve or report.
- `RouteEvent`: future backend-specific events routed by correlation id.

Keeping these concepts separate matters because the backends have different
lifecycle and delivery semantics. OpenCode can accept a prompt over HTTP after
session discovery or validation. Codex can acknowledge `turn/start` over
JSON-RPC. Huddle can report local CLI submission. Zellij can type text but
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
role              internal role; prototype send uses user-message turns only
message           text payload
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
Claude   { channel?, member }
Zellij   { session, pane_id, submit_strategy }
```

`session_id` and `thread_id` should not remain loose untyped strings in the
router. The handle type defines which backend may use the identifier. When a
concrete id is not configured, the typed handle carries the `cwd` needed for
backend-specific discovery.

## Prototype Scope

The current prototype is narrower than the full envelope:

- `metastack send` sends one-way `user` message turns only.
- `metastack send` resolves the routing config before target resolution:
  explicit path-like config argument first. Path-like means the first argument
  contains `/` or ends in `.yaml`/`.yml`; bare target names do not depend on
  files in the caller's current directory.
  `$XDG_CONFIG_HOME/metastack/routing.yaml` when `XDG_CONFIG_HOME` is set, or
  `$HOME/.config/metastack/routing.yaml`.
- `routes.default_reply_to` is parsed from config and copied into the internal
  envelope `reply_to`, but no reply router exists yet. Config v2 has no
  per-agent `reply_to` key; unknown agent fields are rejected. `reply_to` means
  "return to caller/parent", not arbitrary peer addressing.
- OpenCode, Codex, and Claude/Huddle are the implemented prototype adapters.
- zellij fallback remains a design contract/stub until its addressing and reply
  semantics are precise.
- Codex opens a WebSocket per prototype send, validates configured thread ids
  against target `cwd` metadata and CLI source, and waits for the
  `turn/start` JSON-RPC response. It does not wait for agent turn completion.
- Claude/Huddle shells out to `huddle send` and reports local submission only.
- The prototype is intentionally fire-and-forget after backend submission or
  acceptance. Durable delivery, acknowledgements, retries, and async agent
  results belong in a later service/protocol layer.

## Backend Semantics

| Backend | Delivery | Send receipt | Role semantics | Target discovery |
|---|---|---|---|---|
| OpenCode | HTTP `prompt_async` | HTTP status | user prompt turn in prototype | `cwd -> session_id` |
| Codex | JSON-RPC `turn/start` | JSON-RPC response | user prompt turn in prototype | `cwd -> active cli thread_id` |
| Claude | `huddle sessions`, then `huddle send` CLI | process exit = submitted | user Huddle message in prototype | channel/member |
| Zellij | design-only keystrokes fallback | not implemented by `metastack send` | not preserved | session/pane id |

Backends should report their capabilities before dispatch. In the current
prototype, `has_delivery_receipt` means the backend reports more than local CLI
submission:

```text
preserves_role
has_delivery_receipt
is_lossy
```

`role` in the envelope is internal metadata. The public `send` command does not
accept role selection; adapters reject anything other than a normal user-message
turn.

Fallback policy must be explicit:

```text
never
explicit_lossy
on_unavailable
```

Codex `cx` sessions should not silently degrade to zellij. Zellij fallback is a
future design-only path for raw or user-launched TUIs where the caller
explicitly accepts lossy keystroke sending.

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

These are not the prototype `send` receipt. `send` only reports that the
selected backend submitted or accepted the message. OpenCode reports
`accepted`; Codex reports `accepted` after `turn/start`; Claude/Huddle reports
`submitted` after local `huddle send` success. Zellij structured-send fallback
is not implemented by `metastack send`; if added later, it would likely report
only `submitted` or `degraded`.

Do not rebuild reliable message delivery in this layer. If MetaStack needs
durable acks, retries, subscriptions, or long-running result streams, that
should happen in a later async-agent protocol layer rather than in the
prototype CLI.

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

Target discovery should pass `directory=<cwd>` to the session route, then
prefer the newest top-level session whose `directory` matches the target project
root. If only child sessions match, discovery can fall back to the newest child
session. Callers can pin `session_id` explicitly, but MetaStack still validates
that configured id against the cwd-filtered session list before posting.
Delivery returns before the agent finishes; OpenCode does not provide completion
readback through this primitive.

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
select active CLI thread, or validate configured thread_id
thread/resume to load the thread and attach this socket for events
turn/start
wait for the turn/start JSON-RPC response
```

Codex is not identical to fire-and-forget HTTP: MetaStack waits for the
`turn/start` JSON-RPC response so it knows the app-server accepted the message.
It intentionally does not keep the socket open for agent completion in the
prototype. A configured `thread_id` still goes through `thread/list` first so
MetaStack can reject ids that the app-server reports under a different cwd, with
no cwd metadata, or from a non-CLI source.

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

The local launch requirements are:

```text
claude-code v2.1.80+
claude.ai login
huddle-mcp in ~/.mcp.json
huddled daemon running
--dangerously-load-development-channels server:huddle
```

The Claude backend models Huddle as the transport, not as the routing
abstraction. MetaStack still owns the routing envelope and target discovery.
Huddle participant names are not necessarily zellij session names. The routing
config must carry the explicit Huddle `member` shown by `huddle sessions`; do
not infer it from a zellij session id such as `andy-coh`. `channel` is optional
metadata/reserved routing context in the current CLI adapter; `huddle send`
uses `member`.

The current prototype shells out to:

```text
huddle sessions
huddle send --to <member> "<message>"
```

If `huddle sessions` does not list the target member, dispatch fails with a
no-target/unavailable error. Successful `huddle send` command exit returns
`SendStatus::Submitted`. That means local Huddle submission only; it does not
imply the Claude session read, started, completed, or replied to the message.
Leading-dash messages are handled for this CLI path, but edge whitespace is not
guaranteed to round-trip exactly through `huddle send`.
Because Huddle can parse inline mentions from message text, the adapter rejects
`@mention` tokens other than the configured target. This fail-closed guard keeps
`metastack send <target> ...` single-target until Huddle exposes a structured
send mode that disables inline fanout.
Use `huddle log --n N` for opt-in smoke-test assertions that the coordinator
appended the message, not as completion verification. Bidirectional Channels MCP
integration, reply correlation, and completion tracking are intentionally out
of scope.

### Zellij Fallback (Design Only)

Zellij remains an explicit lossy fallback design option, but it is not
implemented by `metastack send`:

```text
zellij-mcp send-text
```

If implemented later, use this only for raw or user-launched TUIs where no
structured backend is available. It is lossy: no typed roles, no delivery
semantics, and no backend readback.

Codex interactive panes are especially fragile through keystrokes. Prefer the
Codex app-server backend for `cx` sessions.

This fallback remains a separate future `send_running_lossy()` primitive, not a
structured provider in the current prototype. Today, zellij send config parses as
a schema concept, but dispatch returns an explicit "not implemented" error.

## Routing Config V2

YAML remains the portable runtime format. `metastack send` normally loads this
shape from the discovered routing config path, but callers can still pass an
explicit path. A future Nix module should render this shape to YAML unless JSON
support is explicitly added to config discovery and documented as a supported
format.

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
  huddle:
    type: claude
    command: huddle

agents:
  nixos-cx:
    backend: codex
    cwd: /home/andy/nixos
  vault-oc:
    backend: opencode
    cwd: /home/andy/vault
  andy-coh:
    backend: huddle
    member: andy
```

The original task DAG config remains a separate YAML shape. Routing config v2 is
the current parallel schema for structured send; it does not replace DAG config.
Reply routing, route paths, and lossy terminal fallback are follow-up contracts,
not part of the minimal runnable example.

## Testing Strategy

The routing core should be tested without live agent processes:

- Envelope serialization and deserialization.
- Target resolution from static config.
- Routing config path resolution: explicit path wins, XDG default is preferred,
  HOME fallback works, and missing defaults produce a clear error.
- Backend request construction.
- OpenCode session selection and prompt body construction.
- Codex JSON-RPC request construction and active-thread selection.
- Claude/Huddle command construction, member preflight parsing, and config
  semantics.
- Fake backend implementing the common trait.

Live smoke tests should be opt-in because they depend on local services:

- OpenCode serve on `127.0.0.1:4096`.
- Codex app-server on `127.0.0.1:4107`.
- Huddle/coh channel availability and `huddle send` behavior.
- zellij session and pane ids.

## Migration Status

Implemented prototype steps:

1. Add the routing data model and backend trait.
2. Add OpenCode, Codex, and Claude/Huddle prototype backends.
3. Add static target discovery from config v2.
4. Add `metastack send` as a CLI path for one message.

Next design steps:

1. Add lifecycle-owned `spawn(agent_spec)` so MetaStack can create agents with
   structured sending enabled from birth.
2. Replace per-send Codex sockets with target-scoped connection managers
   and per-thread turn queues.
3. Integrate routing into DAG tasks once target discovery is stable.
4. Add Nix/Home Manager module support that renders config v2.
