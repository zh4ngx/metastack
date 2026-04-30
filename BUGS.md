# Metastack v0 Production Test — Bugs Surfaced

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

## Bug 4: No support for intended main/aux column layout

**What happened:** The intended layout is main pane left 2/3, aux/worker panes tiled top-down in the right 1/3 column. Metastack v0 has no mechanism to achieve this because:
- `spawn-pane` targets the focused pane for splitting
- There's no `target_pane_id` argument to split a specific pane
- No pane resize capability after spawn

**Root cause:** zellij-mcp's `spawn-pane` only supports `direction`, not `target_pane_id`. zellij CLI itself does support `--pane-id` on some commands, but `new-pane` splits relative to focus.

**Fix (v0.1):** Two options:
1. **Short-term:** Add `spawn-pane` support for `target_pane_id` in zellij-mcp (if zellij CLI supports it, or use `focus-pane` before `new-pane` then restore focus).
2. **Medium-term:** After spawning the first aux pane with `direction: right`, capture its pane_id, then for subsequent aux panes: `focus-pane` the previous aux pane, `spawn-pane` with `direction: down`, then `focus-pane` back to main. This requires metastack to track the "last aux pane" state.
3. **Resize:** Add a `resize-pane` tool to zellij-mcp or call `zellij action resize` directly.

## Bug 5: No task output persistence — FIXED in src/main.rs

**What happened:** `kill_on_done: true` closed all 3 review panes immediately after task completion. The actual codex review text (the valuable output) was lost. Metastack only prints a status table, not the full output.

**Root cause:** TaskResult contains `output: String` but `print_table` doesn't display it. There's no log file, no artifact directory, no persistence.

**Fix (v0.1):** Second positional CLI arg becomes output dir (default `/tmp`). Artifacts written to `{output_dir}/metastack-{task_name}.txt`.

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
