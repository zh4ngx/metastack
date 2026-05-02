# Changelog

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
