# Metastack v0 Production Test — Bugs Surfaced

Test date: 2026-04-29
Test: 3 parallel codex review tasks via sh-wrapper pattern
Result: 2/3 tasks completed (done), 1/3 failed. Review outputs lost due to kill_on_done.

## Bug 1: Session hardcoding ignores current session

**What happened:** `session: main` was hardcoded in `metastack.yaml`. Panes spawned in the user's active zellij session (which happened to be named `main`), cluttering their workspace. If the user were in a different session, metastack would either fail or spawn panes in the wrong session.

**Root cause:** The YAML config requires an explicit `session` field. There's no auto-detection of `ZELLIJ` env var.

**Fix (v0.1):** Default `session` to `std::env::var("ZELLIJ").ok()` (the session name inherited from the parent zellij process). Only fall back to YAML-configured session if not running inside zellij.

## Bug 2: Agent-as-direct-provider pattern is fragile

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

## Bug 5: No task output persistence

**What happened:** `kill_on_done: true` closed all 3 review panes immediately after task completion. The actual codex review text (the valuable output) was lost. Metastack only prints a status table, not the full output.

**Root cause:** TaskResult contains `output: String` but `print_table` doesn't display it. There's no log file, no artifact directory, no persistence.

**Fix (v0.1):** Add `--output-dir` CLI arg. Write each task's full output to `{output_dir}/{task_name}.txt` on completion. Include a `--show-output` flag for the status table to optionally print truncated output.

---

## Review Task Results (outputs lost, status only)

| Task | Status | Elapsed | Notes |
|---|---|---|---|
| review-architecture | done | 113.75s | Codex completed successfully |
| review-sentinel | done | 129.88s | Codex completed successfully |
| review-wasi | failed | 36.59s | Error: "Separator is found, but chunk is longer than limit" |

The review-wasi failure is likely a codex exec error (possibly YAML parsing of a long line in the prompt, or serde_yml choking on output). Needs reproduction with output persistence to diagnose.
