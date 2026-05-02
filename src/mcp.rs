use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, oneshot},
};

pub struct McpClient {
    stdin: Mutex<Option<ChildStdin>>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>,
}

impl McpClient {
    pub async fn start(binary: &str) -> Result<(Arc<Self>, Child)> {
        let mut child = Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn MCP binary {binary}"))?;
        let stdin = child.stdin.take().context("MCP stdin was not piped")?;
        let stdout = child.stdout.take().context("MCP stdout was not piped")?;
        let client = Arc::new(Self {
            stdin: Mutex::new(Some(stdin)),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        });
        tokio::spawn(read_loop(stdout, client.clone()));
        Ok((client, child))
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(err) = self.write_json(msg).await {
            self.pending.lock().await.remove(&id);
            return Err(err);
        }
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
        if let Some(tx) = client.pending.lock().await.remove(&id) {
            let _ = tx.send(result);
        }
    }
    for (_, tx) in client.pending.lock().await.drain() {
        let _ = tx.send(Err(anyhow!("MCP stdout closed")));
    }
}
