# Metastack Historical Bugs and Backlog

This file preserves the v0.1 production-test bug log and a small backlog. It is
not the source of truth for current protocol status; use `README.md`,
`ARCHITECTURE.md`, and `CHANGELOG.md` for release-facing behavior.

Test date: 2026-04-29
Test: 3 parallel codex review tasks via sh-wrapper pattern
Result: 2/3 tasks completed (done), 1/3 failed. Review outputs lost due to kill_on_done.

## Bug 1: Session hardcoding ignores current session — FIXED in src/main.rs

**What happened:** `session: main` was hardcoded in `metastack.yaml`. Panes spawned in the user's active zellij session (which happened to be named `main`), cluttering their workspace. If the user were in a different session, metastack would either fail or spawn panes in the wrong session.

**Root cause:** zellij sets `ZELLIJ=0` as an in-session marker, not as the session name. Auto-detection must read `ZELLIJ_SESSION_NAME` when the YAML config omits `session`.

**Fix (v0.1):** Keep an explicit YAML `session` as an override. Otherwise default `session` to `std::env::var("ZELLIJ_SESSION_NAME").ok()`, ignoring the `ZELLIJ` marker.

## Bug 2: Agent-as-direct-provider pattern is fragile — VALIDATOR WARNING added in src/main.rs

**What happened:** Initial attempt used `command: [codex, exec, ...]` with `prompt_mode: instruction`. Codex exec needs the prompt at invocation time (positional arg or piped stdin), not delivered via `send-text` after spawn. The panes appeared empty and codex exited immediately or hung without processing the sent text.

**Root cause:** The metastack architecture separates `command` (pane command) from `prompt` (sent text). This works for shell REPLs but breaks for tools that expect argv/stdin at startup.

**Fix (v0.1):** Document that `prompt_mode: shell` with `command: [sh]` is the robust pattern for agent CLI providers. Codex, Claude Code, opencode, etc. should all be invoked as shell commands, not as direct pane commands. Add a config validator that warns when `command` does not contain a shell interpreter and `prompt_mode` is not `shell`.

## Bug 3: default direction=right cascades and hogs screen

**What happened:** With 3+ panes and `direction: right`, each new pane splits the previous one horizontally, creating a cascading row of narrow panes that consume 90%+ of screen width.

**Root cause:** `direction` is applied relative to the *focused* pane. When metastack spawns pane A with direction:right, pane A gets focus. Spawning pane B with direction:right splits pane A, not the original main pane. This cascades.

**Fix (v0.1):** Support `direction: down` as a workaround (already tested). For the real fix, see Bug 4.

## Bug 4: Intended main/aux column layout only partially handled

**What happened:** The intended layout is main pane left 2/3, aux/worker panes tiled top-down in the right 1/3 column. Original metastack v0 had no mechanism to achieve this because `spawn-pane` targeted the focused pane and there was no `target_pane_id` routing.

**Current status:** `target_pane_id` is now wired from global config and per-task config into `spawn-pane`, so pane targeting is partially handled. The remaining gap is layout policy: resizing, tracking the aux column, and deciding whether later worker panes should split the previous worker pane or the original target.

**Remaining fix:** Add an explicit layout policy. After spawning the first aux pane with `direction: right`, capture its pane_id; for subsequent aux panes, split the previous aux pane with `direction: down`, then restore focus to the main pane. Add resize support through zellij-mcp or direct zellij action if fixed proportions matter.

## Bug 5: No task output persistence — FIXED in src/main.rs

**What happened:** `kill_on_done: true` closed all 3 review panes immediately after task completion. The actual codex review text (the valuable output) was lost. Metastack only prints a status table, not the full output.

**Root cause:** TaskResult contains `output: String` but `print_table` doesn't display it. There's no log file, no artifact directory, no persistence.

**Fix (v0.1):** Second positional CLI arg becomes output dir (default `/tmp`). Artifacts written to `{output_dir}/metastack-{safe_task_name}.txt`.

---

## Bug 6 (P0): read-pane → TaskResult.output pipeline empty even when sentinel detected — FIXED in src/main.rs

**What happened:** `write_artifact` fired but wrote 0 bytes. The `output` field in `TaskResult` was empty string even though the sentinel was detected and task status was "done".

**Root cause:** `read-pane` returns standard MCP tool result format `{"content": [{"type": "text", "text": "..."}]}` but metastack was reading `.get("text")` directly (expecting `{"text": "..."}` format). The `tool_data` helper returned the raw result for non-errors, so `.get("text")` returned `None` and `unwrap_or("")` produced empty string.

**Fix:** Added `extract_text()` helper that tries `.text` first, then falls back to `content[].text` array extraction.

---

## Review Task Results (outputs lost, status only)

| Task | Status | Elapsed | Notes |
|---|---|---|---|
| review-architecture | done | 113.75s | Codex completed successfully |
| review-sentinel | done | 129.88s | Codex completed successfully |
| review-wasi | failed | 36.59s | Error: "Separator is found, but chunk is longer than limit" |

The review-wasi failure is likely a codex exec error (possibly YAML parsing of a long line in the prompt, or serde_yml choking on output). Needs reproduction with output persistence to diagnose.

---

## Backlog / Known Issues

- **kill_on_done dead code**: `wait_for_spawned_panes()` replaced per-task kill; `kill_on_done` field is now dead code in Config. Either remove from struct (breaking YAML compat) or wire as global kill-after-wait flag.
- **orchestration test coverage**: Current tests cover focused regressions, not full runtime orchestration. Add fake MCP coverage for `spawn-pane`/`send-text`/`read-pane` sequencing, `wait_for_spawned_panes()`, provider rate limiting, validation edge cases, and shell-wrapper exit-code behavior.
- **Nix-declared topology**: Keep YAML as the portable runtime config, but add a Nix/Home Manager layer that declares providers, tasks, sessions, rate limits, secrets, and service lifecycle, then renders YAML/JSON for `metastack`. Do not embed Nix evaluation in the Rust binary.
- **structured-send hardening**: OpenCode HTTP, Codex app-server, and Claude/Huddle CLI submission are implemented as prototype send backends. Remaining work: live smoke coverage, response-tail verification, better ambiguity errors when multiple sessions match, target-scoped Codex connection managers, and DAG integration.
- **async caller notification**: A first-class completion notification path is still design-only. The current recommended practice is to report through the parent/child chain using structured send where available.
- **routing core follow-up**: Remaining work includes parent/caller reply routing via `reply_to`/`correlation_id`, lifecycle-owned `spawn(agent_spec)`, opt-in live Huddle smoke coverage with `huddle log --n N`, and explicit lossy terminal fallback implementation once addressing semantics are precise.
