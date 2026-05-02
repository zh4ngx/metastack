# metastack

`metastack` is a small CLI with two surfaces: a DAG runner over `zellij-mcp`
and a structured send prototype for routing one message turn to an existing
agent session.

It is not an MCP server. In DAG mode, a human or agent runs `metastack` from a
shell, and `metastack` starts the configured `zellij-mcp` binary as a child
process. It then acts as an MCP client, using a small subset of the
`zellij-mcp` tool surface, primarily `spawn-pane`, `send-text`, and
`read-pane`, to run DAG tasks in zellij panes. In structured-send mode,
`metastack` talks directly to configured backend services.

## Status

`metastack` currently has two CLI modes:

| Mode | Command | Status | Runtime requirements |
| --- | --- | --- | --- |
| DAG runner | `metastack [config] [output-dir]` | implemented | `zellij`, `zellij-mcp`, configured providers |
| Structured send | `metastack send <routing-config> <target> <message...>` | prototype | OpenCode serve, Codex app-server, or Huddle target |

Structured send support:

| Backend | Status | Notes |
| --- | --- | --- |
| OpenCode | implemented prototype | sends one user-message turn through `prompt_async`; no reply routing yet |
| Codex | implemented prototype | starts one user-message turn through the app-server WebSocket; no reply routing yet |
| Claude/Huddle | implemented prototype | checks `huddle sessions`, then shells out to `huddle send`; local submission only, no reply routing yet |
| zellij fallback | planned | documented lossy fallback only |

For routing topology, internal envelopes, and protocol semantics, see
[ARCHITECTURE.md](./ARCHITECTURE.md).

## Vocabulary

- `send`: public CLI action that submits one user-message turn to a concrete
  target.
- `route`: policy/resolution layer for choosing where a message should go.
  Today the CLI takes an already-resolved target.
- `inject`: retired public name. It is reserved for backend-specific transcript
  mutation if that ever becomes useful.

## Install

With Nix from this repository:

```bash
nix run . -- <args>
nix build .
./result/bin/metastack <args>
nix profile install .
```

With Nix from GitHub:

```bash
nix run github:zh4ngx/metastack -- <args>
nix profile install github:zh4ngx/metastack
```

Declarative NixOS/Home Manager users can add the flake package to
`environment.systemPackages` or `home.packages`. There is no
`programs.metastack` module yet.

With Cargo:

```bash
cargo install --git https://github.com/zh4ngx/metastack.git --locked
```

From a local checkout:

```bash
cargo run -- <args>
cargo install --path . --locked
```

There is not yet a crates.io release, Homebrew formula, or prebuilt binary
release.

The Nix package installs only the `metastack` binary. It does not install
`zellij`, `zellij-mcp`, OpenCode, Codex, Claude, or any provider commands used
by your YAML config.

## Prerequisites

For the DAG runner:

- `zellij`
- a separately installed or built `zellij-mcp` binary, referenced by
  `mcp_binary` in the config
- any provider commands you put in the config, such as `sh`, `codex`, `claude`,
  or another agent CLI

Run the DAG runner from inside a zellij session, or set `session` in the config
to the name of an existing zellij session. If `session` is omitted,
`metastack` uses `ZELLIJ_SESSION_NAME` when that environment variable is
available.

`zellij-mcp` is not vendored here. Build or install it from the `zellij-mcp`
project, then put the binary on `PATH` as `zellij-mcp` or set `mcp_binary` to
its absolute path. The DAG smoke test will fail until this binary is available.

For structured send:

- OpenCode targets need `opencode-serve` listening on `127.0.0.1:4096`
- Codex targets need `codex-app-server` listening on `127.0.0.1:4107`
- Claude/Huddle targets need the `huddle` CLI, the `huddled` daemon, and a
  channel-enabled Claude Code session launched through `coh` or equivalent
- OpenCode and Codex target `cwd` values in the routing config must match an
  active/newest backend session or thread

`metastack` does not start these backend services. In Andy's local agent setup,
running `oc` from a project root starts or attaches to `opencode-serve`, and
running `cx` from a project root starts or attaches to `codex-app-server`.
Outside that setup, start the equivalent services from those projects and
adjust the URLs in the routing config.

Claude/Huddle support assumes Claude Code v2.1.80 or newer, a working
`claude.ai` login, Huddle MCP configured in `~/.mcp.json`, and channels enabled
with `--dangerously-load-development-channels server:huddle`. The local `coh`
launcher wraps this setup; ordinary `co` sessions are not channel-enabled.
Huddle participant names are not necessarily zellij session names, so configure
the explicit `member` shown by `huddle sessions`.

For development without Nix, use a recent stable Rust toolchain that supports
the Rust 2024 edition.

## Configuration Files

For installed structured-send use, the intended routing config path is:

```text
~/.config/metastack/routing.yaml
```

The repository's `routing.example.yaml` is a shape example with Andy-local
targets. Copy its structure, but replace `cwd`, Huddle `member`, ports, model
settings, approval policy, and sandbox policy for your environment.

The DAG runner reads the first positional argument as its config path. If that
argument is omitted, it defaults to `./metastack.yaml`.

## DAG Smoke Test

The repository includes `smoke-test.example.yaml`, a minimal config with no
local machine paths:

```yaml
mcp_binary: zellij-mcp
startup_delay: 0.2
poll_interval: 0.2
timeout: 10
providers:
  shell:
    command: [sh]
    prompt_mode: shell
    capacity: 1
    refill_per_sec: 1
tasks:
  - name: hello
    provider: shell
    prompt: echo hello from metastack
```

Run it from inside zellij:

```bash
metastack smoke-test.example.yaml /tmp/metastack-output
```

Expected success shape:

- the command exits successfully
- `/tmp/metastack-output/metastack-hello.txt` is written
- the artifact contains `hello from metastack` and a completion sentinel

Common setup failures:

- `failed to spawn MCP binary zellij-mcp`: install/build `zellij-mcp` or edit
  `mcp_binary`
- zellij session errors: run from inside zellij or set `session`
- provider command errors: make sure `sh` or your chosen provider command is on
  `PATH`

If `zellij-mcp` is not on `PATH`, edit `mcp_binary` to point at your local
binary. Provider commands are resolved from the `metastack` process environment,
so `command: [sh]` requires `sh` to be available there.

The checked-in `metastack.yaml` is a development example and still contains
Andy-local paths. Use it as a configuration reference, not as the cold-start
smoke test.

## Structured Send Prototype

The structured send prototype sends one user-message turn through an already
running backend service. It returns after backend submission or acceptance, not
after the target agent completes work or replies:

```bash
metastack send ~/.config/metastack/routing.yaml local-codex "status update"
```

Successful sends print a transport receipt, not a task result:

```text
sent backend=Codex target=local-codex transport_status=Accepted completion=not_tracked correlation_id=...
```

Minimal generic routing config:

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
  local-opencode:
    backend: opencode
    cwd: /path/to/project

  local-codex:
    backend: codex
    cwd: /path/to/project

  local-claude:
    backend: huddle
    member: claude-member-name
```

OpenCode returns after HTTP `prompt_async` acceptance. Codex returns after the
app-server accepts `turn/start`. Production `cx` sessions are stateful
WebSocket conversations with item, delta, and completion events; this prototype
only submits one turn and waits for acceptance.

Claude/Huddle first checks `huddle sessions` for the configured `member`, then
returns after the local `huddle send` command exits successfully. The receipt
status is `Submitted`, not `Accepted`, because this only proves local Huddle
submission. It does not prove the Claude session read, started, completed, or
replied to the message. Use `huddle log --n N` for an opt-in live smoke-test
assertion that the coordinator appended the message, not as completion
verification. If `DISABLE_TELEMETRY=1` disables Claude channel feature-flag
evaluation, if the target was launched with `co` instead of `coh`, or if
`huddled` is down, Huddle delivery will fail outside of MetaStack.

zellij fallback targets may parse as config concepts, but `metastack send`
currently returns an explicit "not implemented" error for them.

## Safety

Do not run untrusted configs. DAG configs can spawn arbitrary provider commands
in zellij panes. Structured send configs can target existing agent sessions and
set Codex approval/sandbox policy. The generic Codex example above mirrors this
project's current local default of `approval_policy: never` with
`dangerFullAccess`; change those settings before using it in a more restrictive
environment.

## When To Use It

Use `metastack` when a job is naturally a multi-task DAG with dependencies or
per-provider rate limits. For a bounded headless one-shot, a plain background
shell command is usually simpler. For an ongoing interactive exchange with a
single agent, a dedicated zellij pane is usually clearer.

`metastack` deliberately uses zellij panes instead of raw bash subprocesses or a
general process pool. Panes make long-running work visible, persistent across SSH
disconnects, and inspectable by a human or another agent while the DAG is still
running.

## Running From Source

With Rust tooling available:

```bash
cargo run -- smoke-test.example.yaml /tmp/metastack-output
```

With the Nix flake:

```bash
nix run . -- smoke-test.example.yaml /tmp/metastack-output
nix develop -c cargo run -- smoke-test.example.yaml /tmp/metastack-output
nix build .
```

Structured send prototype:

```bash
cargo run -- send ~/.config/metastack/routing.yaml local-codex "status update"
nix run . -- send ~/.config/metastack/routing.yaml local-codex "status update"
```

OpenCode targets require `opencode-serve` on
`127.0.0.1:4096` with a session whose `directory` matches the target `cwd`.
Codex targets require a `cx`/Codex app-server session on `127.0.0.1:4107` with
a matching active or newest CLI thread. Claude/Huddle targets require
`huddle send` and a channel-enabled `coh` session.

For the DAG runner, the first positional argument is the config path. If
omitted, it defaults to `./metastack.yaml`.

For the DAG runner, the second positional argument is the artifact output
directory. If omitted, it defaults to `/tmp`.

## Configuration Shape

```yaml
mcp_binary: /path/to/zellij-mcp
session: main
direction: down
target_pane_id: terminal_0
startup_delay: 0.2
poll_interval: 0.2
timeout: 30
kill_on_done: true
providers:
  codex:
    command: [sh]
    prompt_mode: shell
    capacity: 1
    refill_per_sec: 1
tasks:
  - name: review-main
    provider: codex
    cwd: .
    prompt: codex exec "Review src/main.rs briefly."
  - name: review-mcp
    provider: codex
    depends-on: [review-main]
    prompt: codex exec "Review src/mcp.rs briefly."
```

`session` is optional. If omitted, `metastack` uses `ZELLIJ_SESSION_NAME` when
available.

`startup_delay` must be finite and `>= 0`. `poll_interval` and `timeout` must
be finite and `> 0`; invalid values fail during YAML parsing.

`kill_on_done` is accepted for config compatibility, but it is currently
ignored: post-DAG output draining replaced the old immediate pane kill path.
See `BUGS.md` for the follow-up decision.

## Provider Pattern

The robust provider pattern is:

```yaml
command: [sh]
prompt_mode: shell
```

In this mode, `metastack` spawns a shell pane and sends the task prompt as shell
text. This works well for agent CLIs because the task prompt can contain the
full command invocation, arguments, and quoted prompt text.

Avoid treating an agent CLI as the pane command when that CLI expects its prompt
at startup through argv or stdin. For example, `command: [codex, exec, ...]`
combined with later `send-text` delivery is fragile because `metastack`
separates pane startup from prompt delivery.

Tasks are scheduled as a dependency graph. `depends-on` edges are validated and
topo-sorted before execution, and each provider has a token bucket so concurrent
work can be rate-limited per provider.

## Completion

For shell providers, `metastack` wraps each prompt so the task's exit code is
saved before printing a completion sentinel:

```sh
__metastack_code=$?
printf '\n__METASTACK_DONE_<task>_<id>__:%s\n' "$__metastack_code"
exit "$__metastack_code"
```

`metastack` polls pane output until it sees that sentinel. Exit code `0` marks
the task `done`; any other code marks it `failed`. Tasks depending on failed
tasks are marked `skipped`.

## Artifacts

Task output is written to:

```text
{output_dir}/metastack-{safe_task_name}.txt
```

`safe_task_name` keeps ASCII alphanumeric characters and replaces other
characters with `-`, capped at 40 characters.

## Declarative Nix Roadmap

`metastack` currently reads YAML at runtime. That should remain the portable
runtime interface.

For NixOS and Home Manager users, the intended direction is to declare the same
agent topology in Nix and materialize it to YAML or JSON before `metastack`
starts:

```text
Nix module or flake output
  declares providers, tasks, sessions, rate limits, secrets, and service policy
  renders metastack config
  starts metastack as a systemd user service

metastack binary
  reads rendered YAML or JSON
  talks to zellij-mcp
  orchestrates the DAG
```

The Rust binary should not evaluate Nix directly. Nix should own declaration,
materialization, service lifecycle, rollback, and secret injection. `metastack`
should stay a small portable runtime that consumes a rendered config file.

YAML remains the fallback and lowest-level format.

## Current Limitations

- DAG tasks run in zellij panes; the DAG runner is not a general headless
  dispatch tool.
- `kill_on_done` is still accepted in config for compatibility, but is currently
  ignored after the post-DAG output drain change.
- Pane layout is constrained by what `zellij-mcp` exposes. `direction` and
  `target_pane_id` are available, but complex layout management still depends on
  zellij behavior.
- Structured send reports transport submission or acceptance only and does not
  route replies yet.
