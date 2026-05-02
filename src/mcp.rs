use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex as AsyncMutex, oneshot},
};

pub struct McpClient {
    stdin: AsyncMutex<Option<ChildStdin>>,
    next_id: AtomicU64,
    pending: StdMutex<HashMap<u64, oneshot::Sender<Result<Value>>>>,
}

impl McpClient {
    pub async fn start(binary: &str) -> Result<(Arc<Self>, Child)> {
        let mut command = Command::new(binary);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn MCP binary {binary}"))?;
        let stdin = child.stdin.take().context("MCP stdin was not piped")?;
        let stdout = child.stdout.take().context("MCP stdout was not piped")?;
        let client = Arc::new(Self {
            stdin: AsyncMutex::new(Some(stdin)),
            next_id: AtomicU64::new(1),
            pending: StdMutex::new(HashMap::new()),
        });
        tokio::spawn(read_loop(stdout, client.clone()));
        Ok((client, child))
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("MCP pending mutex poisoned")
            .insert(id, tx);
        let _pending = PendingRequest::new(&self.pending, id);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.write_json(msg).await?;
        rx.await.context("MCP response channel closed")?
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write_json(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
            .await
    }

    pub async fn close(&self) {
        self.stdin.lock().await.take();
    }

    async fn write_json(&self, msg: Value) -> Result<()> {
        let mut guard = self.stdin.lock().await;
        let stdin = guard.as_mut().context("MCP stdin is closed")?;
        stdin.write_all(msg.to_string().as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }
}

struct PendingRequest<'a> {
    pending: &'a StdMutex<HashMap<u64, oneshot::Sender<Result<Value>>>>,
    id: u64,
}

impl<'a> PendingRequest<'a> {
    fn new(pending: &'a StdMutex<HashMap<u64, oneshot::Sender<Result<Value>>>>, id: u64) -> Self {
        Self { pending, id }
    }
}

impl Drop for PendingRequest<'_> {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&self.id);
        }
    }
}

async fn read_loop(stdout: ChildStdout, client: Arc<McpClient>) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = value.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let result = if let Some(err) = value.get("error") {
            Err(anyhow!("MCP error for id {id}: {err}"))
        } else {
            Ok(value.get("result").cloned().unwrap_or(Value::Null))
        };
        if let Some(tx) = client
            .pending
            .lock()
            .expect("MCP pending mutex poisoned")
            .remove(&id)
        {
            let _ = tx.send(result);
        }
    }
    let drained = client
        .pending
        .lock()
        .expect("MCP pending mutex poisoned")
        .drain()
        .collect::<Vec<_>>();
    for (_, tx) in drained {
        let _ = tx.send(Err(anyhow!("MCP stdout closed")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_request_removes_entry_on_drop() {
        let pending = StdMutex::new(HashMap::new());
        let (tx, _rx) = oneshot::channel();
        pending.lock().expect("pending lock").insert(7, tx);

        {
            let _guard = PendingRequest::new(&pending, 7);
        }

        assert!(pending.lock().expect("pending lock").is_empty());
    }
}
