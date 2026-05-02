# Changelog

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
