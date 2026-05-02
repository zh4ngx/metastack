# Changelog

## v0.8.2 - 2026-05-02

- Add GitHub Actions CI for pull requests, `main`, and `v*` tags.
- Run host `nix flake check`, all-system evaluation, installed CLI smoke
  checks, and tag-to-package-version validation in CI.

## v0.8.1 - 2026-05-02

- Make MCP request bookkeeping cancellation-safe so timed-out tool calls do not
  leave stale pending response slots.
- Kill the MCP child process on drop, apply the configured timeout to MCP
  initialization, and close/wait/kill the child on early DAG errors.
- Exit nonzero when any DAG task is failed, timed out, skipped, or missing from
  the final result set.
- Split Nix build and development Rust toolchains so developer components such
  as `rust-src` and `rust-analyzer` do not leak into the installed package
  closure.
- Add Nix checks for installed CLI smoke coverage and accidental Rust toolchain
  references in the package output.

## v0.8.0 - 2026-05-02

- Reject unknown DAG config, provider, and task fields instead of silently
  ignoring typos such as `depends_on`.
- Prevent post-DAG output draining from polling timed-out or failed panes
  indefinitely.
- Require DAG completion sentinels to appear at the start of a trimmed output
  line with only the exit code after the colon, preventing echoed instruction
  prompts from completing tasks early.
- Reject non-finite provider `capacity` and `refill_per_sec` values.
- Extend the release version guard to check README install tags and changelog
  entries against Cargo package metadata.

## v0.7.1 - 2026-05-02

- Fail closed for implicit Codex thread discovery when `thread/list` lacks
  `cwd` or `directory` metadata, matching explicit `thread_id` validation.
- Percent-encode OpenCode `session_id` values when constructing
  `prompt_async` paths.
- Reject blank structured-send messages.
- Add `--help` and `--version` CLI output for install smoke tests.
- Add MIT license text, Cargo/Nix package metadata, and a portable default
  `metastack.yaml`.
- Document the known-good `zellij-mcp` source and commit.

## v0.7.0 - 2026-05-02

- Reject Claude/Huddle sends whose message text contains inline `@mention`
  tokens other than the configured target, preserving single-target routing.
- Reject unknown routing config fields at the top level, in backend configs,
  and in `routes`, instead of silently ignoring typos.
- Validate backend names, required backend fields, and OpenCode/Codex URL
  schemes before dispatch.
- Change structured-send receipt stdout from `sent ...` to `receipt ...`,
  include discovered `session_id` or `thread_id` values when available, and
  distinguish backend acceptance from local submission.
- Clarify release practice, NixOS pinning guidance, YAML-only config support,
  and `routes.default_reply_to` wording.

## v0.6.1 - 2026-05-02

- Apply the Codex transport timeout to WebSocket JSON writes, not only connect
  and response waits.

## v0.6.0 - 2026-05-02

- Reject DAG task names that would produce empty or duplicate artifact filenames
  after safe-name normalization.
- Reject backend-inapplicable and unknown routing agent fields instead of
  silently ignoring misconfigured target pins. This intentionally tightens
  routing config validation.
- Validate Codex `turn/start` acceptance payloads before reporting an accepted
  send receipt.
- Compatibility: existing routing configs that relied on ignored or
  backend-inapplicable agent fields now fail validation. Review routing config
  fields before upgrading from v0.5.x to v0.6.x.

## v0.5.2 - 2026-05-02

- Add a `nix flake check` release guard that builds the package and checks Cargo
  manifest, Cargo lockfile, and Nix package version metadata stay aligned.
- Add flake app metadata so release checks do not warn on the default app.
- Pin public GitHub install examples to release tags instead of floating `main`.
- Clarify that zellij structured-send fallback remains design-only and is not
  implemented by `metastack send`.
- Validate configured OpenCode `session_id` values against the target `cwd`
  before posting to `prompt_async`.
- Validate configured Codex `thread_id` values against target `cwd` metadata and
  CLI source before resuming and starting a turn.
- Correct Claude/Huddle backend capabilities to report local submission only,
  not delivery receipts.

## v0.5.1 - 2026-05-02

- Derive Nix package and runtime client versions from Cargo package metadata to
  prevent release-version drift.
- Add tests covering MCP and Codex `clientInfo` version metadata.

## v0.5.0 - 2026-05-02

- Add Claude/Huddle structured-send support through the `huddle send` CLI.
- Add default routing config discovery for `metastack send <target> <message...>`
  via `XDG_CONFIG_HOME`, falling back to `HOME`.
- Harden Codex send routing: filter discovered threads by `cwd`, reject malformed
  JSON-RPC responses, and ignore unrelated JSON-RPC response ids.
- Harden OpenCode send routing: query sessions with `directory=<cwd>` and prefer
  top-level sessions over child sessions.
- Clarify send receipts as transport submission/acceptance only, not task
  completion.
- Change send argument disambiguation so bare target names are deterministic:
  the optional routing config argument is recognized only when it contains `/`
  or ends in `.yaml`/`.yml`. Use `./routing` for extensionless config files.

## v0.2 - 2026-04-29

- Add typed config, output capture, Nix flake packaging, and runtime docs.
