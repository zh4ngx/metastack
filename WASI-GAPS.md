# WASI Gaps

Metastack v0 uses a native stdio MCP client. That boundary is the part that
does not fit pure WASI Preview 1.

1. `tokio::process::Command` does not exist for `wasm32-wasip1`. Subprocess
   spawning for the MCP stdio client cannot run under pure WASI Preview 1.
2. `zellij-mcp` shells out through `zellij action` CLI wrappers, so the entire
   stdio transport layer requires a native host that can spawn subprocesses.
3. The core DAG logic, token bucket math, and YAML parsing can compile to WASI
   when the native `McpClient` module is excluded. For such a crate, run:

   ```sh
   cargo build --target wasm32-wasip1
   ```

4. A practical split is a host-side adapter. The WASI guest exports something
   like `run_dag(config_yaml: &str) -> String`, while the native host provides a
   `spawn_mcp_and_call_tools(...)` import function that owns subprocess spawning,
   JSON-RPC stdio, and zellij tool calls.
