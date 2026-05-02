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
| Structured send | `metastack send [<routing-config>] <target> <message...>` | prototype | OpenCode serve, Codex app-server, or Huddle target |

Structured send support:

| Backend | Status | Notes |
| --- | --- | --- |
| OpenCode | implemented prototype | sends one user-message turn through `prompt_async`; no reply routing yet |
| Codex | implemented prototype | starts one user-message turn through the app-server WebSocket; no reply routing yet |
| Claude/Huddle | implemented prototype | checks `huddle sessions`, then shells out to `huddle send`; local submission only, no reply routing yet |
| zellij fallback | not implemented | zellij is supported for DAG task execution only |

Structured send never implicitly falls back to zellij pane typing. If a
configured backend is unavailable or ambiguous, `metastack send` fails closed so
the caller can fix the backend service, target config, or explicit session pin.

For routing topology, internal envelopes, and protocol semantics, see
[ARCHITECTURE.md](./ARCHITECTURE.md).

## Release Practice

Agent work happens on named branches until it is ready to promote. Releases are
merged to `main` with an explicit version bump and tag. Downstream declarative
systems such as NixOS should consume tags or pinned revisions, not floating
`main`. While `metastack` is pre-1.0, patch releases are for compatible bug
fixes and minor releases are for new behavior or compatibility-affecting CLI or
config changes. For NixOS/Home Manager consumers, pin an exact tag or locked
revision in `flake.lock`. Patch bumps within the same minor line are intended to
be config-compatible. Minor bumps may add behavior or tighten CLI/config
compatibility and should be reviewed before updating declarative systems.

Before tagging on the release host, update `CHANGELOG.md`, bump
`Cargo.toml`/`Cargo.lock`, update public install examples, then run
`nix flake check`; it builds the package and checks that Cargo and Nix version
metadata stay aligned for the host system. Use
`nix flake check --all-systems --no-build` to evaluate all declared systems when
cross-system builders are not available.

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
nix run github:zh4ngx/metastack/v0.10.2 -- <args>
nix profile install github:zh4ngx/metastack/v0.10.2
```

Declarative NixOS/Home Manager users can enable the exported
`programs.metastack` modules. The NixOS module installs the package; the Home
Manager module can also render the canonical routing config.

For a flake-based NixOS or Home Manager config, add the input:

```nix
{
  inputs.metastack.url = "github:zh4ngx/metastack/v0.10.2";
}
```

The module snippets below assume the module can see `inputs`, for example via
`specialArgs = { inherit inputs; };` in `nixpkgs.lib.nixosSystem` or
`extraSpecialArgs = { inherit inputs; };` in Home Manager.

The flake exports a NixOS module that installs the package:

```nix
{
  imports = [ inputs.metastack.nixosModules.default ];

  programs.metastack.enable = true;
}
```

The Home Manager module installs the package and can render the canonical
`~/.config/metastack/routing.yaml`:

```nix
{
  imports = [ inputs.metastack.homeModules.default ];

  programs.metastack = {
    enable = true;
    routingConfig = {
      version = 2;
      backends.codex = {
        type = "codex";
        url = "ws://127.0.0.1:4107";
      };
      agents.local-codex = {
        backend = "codex";
        cwd = "/path/to/project";
      };
    };
  };
}
```

With Cargo:

```bash
cargo install --git https://github.com/zh4ngx/metastack.git --tag v0.10.2 --locked
```

From a local checkout:

```bash
cargo run -- <args>
cargo install --path . --locked
```

Dependency-light install smoke checks:

```bash
metastack --version
metastack --help
```

The package declares Rust 1.88 or newer as its minimum supported Rust version.
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

`zellij-mcp` is not vendored here. Build or install it from
`https://github.com/zh4ngx/zellij-mcp`; commit `75c94f2` is the known-good
local version for this release. Put the binary on `PATH` as `zellij-mcp` or set
`mcp_binary` to its absolute path. The DAG smoke test will fail until this
binary is available.

One direct checkout/build path is:

```bash
git clone https://github.com/zh4ngx/zellij-mcp.git
cd zellij-mcp
git checkout 75c94f2
cargo build --release
```

Then use the resulting `target/release/zellij-mcp` in `mcp_binary`, or place it
on `PATH`.

For structured send:

- OpenCode targets need `opencode-serve` listening on `127.0.0.1:4096`
- Codex targets need `codex-app-server` listening on `127.0.0.1:4107`
- Claude/Huddle targets need the `huddle` CLI, the `huddled` daemon, and a
  channel-enabled Claude Code session launched through `coh` or equivalent
- OpenCode and Codex target `cwd` values in the discovered or explicit routing
  config must match exactly one backend session or active/idle Codex CLI thread
  unless `session_id`/`thread_id` is pinned

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

For structured send, an explicit routing config path wins. If the path is
omitted and `XDG_CONFIG_HOME` is set, `metastack send` uses:

```text
$XDG_CONFIG_HOME/metastack/routing.yaml
```

If `XDG_CONFIG_HOME` is unset or blank, it uses:

```text
$HOME/.config/metastack/routing.yaml
```

On most shells, that HOME fallback is `~/.config/metastack/routing.yaml`.
The optional explicit config argument is recognized when the first send
argument contains `/` or ends in `.yaml` or `.yml`; use `./routing` for an
extensionless config file.

The repository's `routing.example.yaml` is a shape example with Andy-local
targets: `vault-oc`, `nixos-cx`, and `andy-coh`. Copy its structure, but
replace target names, `cwd`, Huddle `member`, ports, model settings, approval
policy, and sandbox policy for your environment. The quick-start commands below
use the generic `local-codex` target from the minimal config in this README.

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

The checked-in `metastack.yaml` is the same portable shape as the smoke test and
contains no Andy-local paths.

## Structured Send Prototype

The structured send prototype sends one user-message turn through an already
running backend service. It returns after backend submission or acceptance, not
after the target agent completes work or replies:

```bash
metastack send local-codex "status update"
metastack send ~/.config/metastack/routing.yaml local-codex "status update"
metastack send ./routing local-codex "status update"
```

These commands assume the default routing config contains a `local-codex`
target like the minimal config below.

Successful sends print a transport receipt, not a task result:

```text
receipt backend=Codex target=local-codex transport_status=Accepted delivery=backend_accepted completion=not_tracked thread_id=... correlation_id=...
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

OpenCode validates any configured `session_id` against the target `cwd`. Without
a configured `session_id`, implicit discovery requires exactly one matching
candidate session and fails closed if multiple sessions share the same `cwd`.
Codex behaves the same way for `thread_id`: configured ids are validated against
target `cwd` metadata and CLI source. Implicit discovery prefers exactly one
active CLI thread, then exactly one idle CLI thread, and ignores stale
`notLoaded` records from older Codex SQLite state. Codex returns after the
app-server accepts `turn/start`. Production `cx` sessions are stateful WebSocket
conversations with item, delta, and completion events; this prototype only
submits one turn and waits for acceptance.

Claude/Huddle first checks `huddle sessions` for the configured `member`, then
returns after the local `huddle send` command exits successfully. The receipt
status is `Submitted`, not `Accepted`, because this only proves local Huddle
submission. It does not prove the Claude session read, started, completed, or
replied to the message. Use `huddle log --n N` for an opt-in live smoke-test
assertion that the coordinator appended the message, not as completion
verification. If `DISABLE_TELEMETRY=1` disables Claude channel feature-flag
evaluation, if the target was launched with `co` instead of `coh`, or if
`huddled` is down, Huddle delivery will fail outside of MetaStack.
Leading-dash messages are handled for the current Huddle CLI path, but edge
whitespace is not guaranteed to round-trip exactly. Because Huddle can parse
inline mentions from message text, Claude/Huddle sends reject `@mention` tokens
other than the configured target; this preserves single-target routing
semantics.

zellij fallback targets may parse as config concepts, but `metastack send`
currently returns an explicit "not implemented" error for them. Do not use
zellij as an implicit recovery path for failed structured send; fix the
backend/config issue or choose a future explicit lossy primitive.

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
cargo run -- send local-codex "status update"
nix run . -- send local-codex "status update"
cargo run -- send ~/.config/metastack/routing.yaml local-codex "status update"
```

OpenCode targets require `opencode-serve` on
`127.0.0.1:4096` with a session whose `directory` matches the target `cwd`.
Codex targets require a `cx`/Codex app-server session on `127.0.0.1:4107` with
a single matching active/idle CLI thread, or an explicit `thread_id`.
Claude/Huddle targets require `huddle send` and a channel-enabled `coh`
session.

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
be finite and `> 0`; invalid values fail during YAML parsing. The task timeout
starts after provider rate-limit acquisition. If a spawned task reaches the
timeout, `metastack` asks `zellij-mcp` to kill that pane before returning.

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
characters with `-`, capped at 40 characters. Config validation rejects task
names that normalize to an empty artifact name or collide with another task's
artifact name.

## Declarative Nix

`metastack` currently reads YAML at runtime. That should remain the portable
runtime interface.

For NixOS and Home Manager users, the flake exports modules that install the
binary and, for Home Manager, render structured-send routing config to
`~/.config/metastack/routing.yaml` before `metastack` starts:

```text
NixOS module
  installs metastack package

Home Manager module
  installs metastack package
  optionally renders routing.yaml from programs.metastack.routingConfig

External NixOS/Home Manager definitions
  manage backend services, secrets, and lifecycle

metastack binary
  reads rendered YAML
  routes structured sends to configured backends
  talks to zellij-mcp for DAG tasks
  orchestrates the DAG
```

The Rust binary does not evaluate Nix directly. Nix owns declaration,
materialization, rollback, and secret injection. External NixOS/Home Manager
service definitions should own service lifecycle for now. `metastack` stays a
small portable runtime that consumes a rendered config file.

YAML remains the fallback and lowest-level format. JSON is not currently a
documented runtime config format.

## Current Limitations

- DAG tasks run in zellij panes; the DAG runner is not a general headless
  dispatch tool.
- zellij is not a structured-send transport in this release. `metastack send`
  does not type into panes when OpenCode, Codex, or Huddle targets are
  unavailable.
- `kill_on_done` is still accepted in config for compatibility, but is currently
  ignored after the post-DAG output drain change.
- Pane layout is constrained by what `zellij-mcp` exposes. `direction` and
  `target_pane_id` are available, but complex layout management still depends on
  zellij behavior.
- Structured send reports transport submission or acceptance only and does not
  route replies yet.
