# metastack

`metastack` is a small CLI DAG orchestrator over `zellij-mcp`.

It is not an MCP server. A human or agent runs `metastack` from a shell, and
`metastack` starts the configured `zellij-mcp` binary as a child process. It
then acts as an MCP client, using a small subset of the `zellij-mcp` tool
surface, primarily `spawn-pane`, `send-text`, and `read-pane`, to run tasks in
zellij panes.

## Runtime Model

```text
human or agent shell
  runs metastack
    reads metastack.yaml
    starts zellij-mcp from mcp_binary
    initializes as an MCP client
    spawns zellij panes for ready tasks
    sends each task prompt into its pane
    polls pane output for completion sentinels
    writes task artifacts
```

Tasks are scheduled as a dependency graph. `depends-on` edges are validated and
topo-sorted before execution, and each provider has a token bucket so concurrent
work can be rate-limited per provider.

After the DAG scheduler completes, `metastack` keeps polling spawned panes with
`wait_for_spawned_panes()` so the parent process does not exit before remaining
task output has been captured.

## When To Use It

Use `metastack` when a job is naturally a multi-task DAG with dependencies or
per-provider rate limits. For a bounded headless one-shot, a plain background
shell command is usually simpler. For an ongoing interactive exchange with a
single agent, a dedicated zellij pane is usually clearer.

`metastack` deliberately uses zellij panes instead of raw bash subprocesses or a
general process pool. Panes make long-running work visible, persistent across SSH
disconnects, and inspectable by a human or another agent while the DAG is still
running.

## Running

With Rust tooling available:

```bash
cargo run -- metastack.yaml /tmp/metastack-output
```

With the Nix flake:

```bash
nix develop -c cargo run -- metastack.yaml /tmp/metastack-output
nix build
```

Structured injection prototype:

```bash
cargo run -- inject routing.example.yaml nixos-cx "status update"
```

For this prototype, OpenCode targets require `opencode-serve` on
`127.0.0.1:4096` with a session whose `directory` matches the target `cwd`.
Codex targets require a `cx`/Codex app-server session on `127.0.0.1:4107` with
a matching active or newest CLI thread. Claude/Huddle and zellij fallback are
documented contracts, not active `metastack inject` adapters yet.

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

`kill_on_done` is accepted for config compatibility, but in the current working
tree it is not active: post-DAG output draining replaced the old immediate pane
kill path. See `BUGS.md` for the follow-up decision.

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

## Structured Injection Roadmap

The v0.3 branch introduces a routing prototype beside the existing DAG runner.
The routing layer uses one common envelope and backend-specific adapters:

```text
OpenCode -> HTTP prompt_async
Codex    -> JSON-RPC WebSocket app-server
Claude   -> Huddle channel bridge
Zellij   -> lossy keystroke fallback
```

`metastack inject <routing-config.yaml> <target> <message...>` is the prototype
entry point for sending one user-message turn through a configured target.
OpenCode and Codex are the first concrete adapters; Claude/Huddle and zellij
fallback are documented as backend contracts for follow-up work.
The CLI carries correlation metadata internally, but reply routing is still
roadmap work.

## Current Limitations

- Tasks run in zellij panes; this is not a general headless dispatch tool.
- `kill_on_done` is still accepted in config for compatibility, but is currently
  dead code after the post-DAG output drain change.
- Pane layout is constrained by what `zellij-mcp` exposes. `direction` and
  `target_pane_id` are available, but complex layout management still depends on
  zellij behavior.
