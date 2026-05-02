# WASI Design Notes

Metastack currently builds as a native binary only. These notes describe the
boundary that would need to move before a future core crate could target WASI
Preview 1; they are not actionable build instructions for the current crate.

1. `tokio::process::Command` does not exist for `wasm32-wasip1`. Subprocess
   spawning for the MCP stdio client cannot run under pure WASI Preview 1.
2. `zellij-mcp` shells out through `zellij action` CLI wrappers, so the entire
   stdio transport layer requires a native host that can spawn subprocesses.
3. The core DAG logic, token bucket math, and YAML parsing should be able to
   compile to WASI once they are split into a crate that excludes the native
   `McpClient` module. For that future crate, the relevant smoke command would
   be:

   ```sh
   cargo build --target wasm32-wasip1
   ```

4. A practical split is a host-side adapter. The WASI guest exports something
   like `run_dag(config_yaml: &str) -> String`, while the native host provides a
   `spawn_mcp_and_call_tools(...)` import function that owns subprocess spawning,
   JSON-RPC stdio, and zellij tool calls.
