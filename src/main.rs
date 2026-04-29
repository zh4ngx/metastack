mod bucket;
mod mcp;

use anyhow::{bail, Context, Result};
use bucket::TokenBucket;
use mcp::McpClient;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
use tokio::{task::JoinSet, time::{sleep, timeout, Duration}};
use uuid::Uuid;

#[derive(Clone, Deserialize)]
struct Config {
    mcp_binary: String, session: Option<String>,
    #[serde(default = "default_direction")] direction: String,
    #[serde(default)] floating: bool,
    #[serde(default = "default_startup_delay")] startup_delay: f64,
    #[serde(default = "default_poll_interval")] poll_interval: f64,
    #[serde(default = "default_timeout")] timeout: f64,
    #[serde(default = "default_kill_on_done")] kill_on_done: bool,
    providers: HashMap<String, Provider>, tasks: Vec<Task>,
}

#[derive(Clone, Deserialize)]
struct Provider { command: Vec<String>, prompt_mode: PromptMode, capacity: f64, refill_per_sec: f64 }

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PromptMode { Shell, Instruction }

#[derive(Clone, Deserialize)]
struct Task {
    name: String, provider: String, prompt: String,
    #[serde(rename = "depends-on", default)] depends_on: Vec<String>,
    cwd: Option<String>,
}

#[allow(dead_code)]
#[derive(Clone)]
struct TaskResult {
    status: String, provider: String, pane_id: String,
    output: String, error: Option<String>, elapsed: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let path = env::args().nth(1).unwrap_or_else(|| "./metastack.yaml".into());
    let text = fs::read_to_string(&path).with_context(|| format!("failed to read {path}"))?;
    let config: Config = serde_yml::from_str(&text).context("failed to parse YAML")?;
    validate(&config)?;
    let base = Path::new(&path).parent().unwrap_or(Path::new(".")).to_path_buf();

    let (client, mut child) = McpClient::start(&config.mcp_binary).await?;
    client.request("initialize", json!({
        "protocolVersion": "2024-11-05", "capabilities": {},
        "clientInfo": {"name": "metastack", "version": "0.1.0"}
    })).await?;
    client.notify("notifications/initialized", json!({})).await?;

    let results = orchestrate(Arc::new(config.clone()), client.clone(), base).await?;
    client.close().await;
    if timeout(Duration::from_secs(2), child.wait()).await.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    print_table(&config.tasks, &results);
    Ok(())
}

async fn orchestrate(config: Arc<Config>, client: Arc<McpClient>, base: PathBuf) -> Result<HashMap<String, TaskResult>> {
    let tasks: HashMap<_, _> = config.tasks.iter().cloned().map(|t| (t.name.clone(), t)).collect();
    let buckets = Arc::new(config.providers.iter().map(|(n, p)| {
        (n.clone(), Arc::new(TokenBucket::new(p.capacity, p.refill_per_sec)))
    }).collect::<HashMap<_, _>>());
    let mut pending: HashSet<_> = tasks.keys().cloned().collect();
    let mut results = HashMap::new();
    let mut joins = JoinSet::new();

    while !pending.is_empty() || !joins.is_empty() {
        for name in pending.clone() {
            let task = tasks.get(&name).unwrap();
            let failed_dep = task.depends_on.iter().any(|d| results.get(d).is_some_and(|r: &TaskResult| r.status != "done"));
            if failed_dep {
                results.insert(name.clone(), skipped(task));
                pending.remove(&name);
            } else if task.depends_on.iter().all(|d| results.contains_key(d)) {
                let (task, cfg, cli, bs, dir) = (task.clone(), config.clone(), client.clone(), buckets.clone(), base.clone());
                pending.remove(&name);
                joins.spawn(async move { let result = run_task(task.clone(), cfg, cli, bs, dir).await; (task.name, result) });
            }
        }
        if let Some(done) = joins.join_next().await {
            let (name, result) = done.context("task join failed")?;
            write_artifact(&name, &result.output);
            results.insert(name, result);
        }
    }
    Ok(results)
}

fn write_artifact(name: &str, output: &str) {
    let path = format!("/tmp/metastack-review-{name}.txt");
    if let Err(e) = fs::write(&path, output) {
        eprintln!("warning: failed to write artifact {path}: {e}");
    }
}

async fn run_task(
    task: Task, config: Arc<Config>, client: Arc<McpClient>,
    buckets: Arc<HashMap<String, Arc<TokenBucket>>>, base: PathBuf,
) -> TaskResult {
    let started = Instant::now();
    match run_task_inner(&task, config, client, buckets, base, started).await {
        Ok(result) => result,
        Err(err) => TaskResult {
            status: "failed".into(), provider: task.provider, pane_id: "-".into(),
            output: String::new(), error: Some(err.to_string()),
            elapsed: started.elapsed().as_secs_f64(),
        },
    }
}

async fn run_task_inner(
    task: &Task, config: Arc<Config>, client: Arc<McpClient>,
    buckets: Arc<HashMap<String, Arc<TokenBucket>>>, base: PathBuf, started: Instant,
) -> Result<TaskResult> {
    let provider = config.providers.get(&task.provider).context("unknown provider")?;
    buckets.get(&task.provider).context("missing token bucket")?.acquire().await;
    let safe = safe_name(&task.name);
    let uuid = Uuid::new_v4().simple().to_string();
    let sentinel = format!("__METASTACK_DONE_{}_{}__", safe, &uuid[..8]);
    let prompt = match provider.prompt_mode {
        PromptMode::Instruction => format!("{}\n\nWhen complete, print exactly {}:0 on its own line.", task.prompt, sentinel),
        PromptMode::Shell => format!("{}\nprintf '\\n{}:%s\\n' \"$?\"", task.prompt, sentinel),
    };

    let cwd = task.cwd.clone().unwrap_or_else(|| base.to_string_lossy().into_owned());
    let mut args = json!({"cwd": cwd, "command": provider.command, "name": format!("ms-{safe}"), "floating": config.floating, "direction": config.direction});
    add_opt(&mut args, "session", config.session.clone());
    add_opt(&mut args, "keep_focus_on", env::var("ZELLIJ_PANE_ID").ok());
    let pane_id = tool_data(&client.call_tool("spawn-pane", args).await?)?
        .get("pane_id").and_then(Value::as_str).context("spawn-pane did not return pane_id")?.to_string();

    let mut output = String::new();
    let mut status = "timeout".to_string();
    let work = async {
        sleep(Duration::from_secs_f64(config.startup_delay)).await;
        let mut send_args = json!({"pane_id": pane_id, "text": prompt, "submit": true});
        add_opt(&mut send_args, "session", config.session.clone());
        tool_data(&client.call_tool("send-text", send_args).await?)?;
        loop {
            let mut read_args = json!({"pane_id": pane_id, "full": true});
            add_opt(&mut read_args, "session", config.session.clone());
            output = tool_data(&client.call_tool("read-pane", read_args).await?)?
                .get("text").and_then(Value::as_str).unwrap_or("").to_string();
            if let Some(code) = exit_code(&output, &sentinel) {
                status = if code == "0" { "done" } else { "failed" }.into();
                break;
            }
            if started.elapsed() >= Duration::from_secs_f64(config.timeout) {
                break;
            }
            sleep(Duration::from_secs_f64(config.poll_interval)).await;
        }
        Ok::<(), anyhow::Error>(())
    };
    let error = work.await.err().map(|e| e.to_string());
    if config.kill_on_done {
        let mut args = json!({"pane_id": pane_id});
        add_opt(&mut args, "session", config.session.clone());
        let _ = client.call_tool("kill-pane", args).await.and_then(|r| tool_data(&r).map(|_| ()));
    }
    Ok(TaskResult {
        status: if error.is_some() { "failed".into() } else { status },
        provider: task.provider.clone(), pane_id, output, error,
        elapsed: started.elapsed().as_secs_f64(),
    })
}

fn tool_data(result: &Value) -> Result<&Value> {
    if result.get("isError").and_then(Value::as_bool).unwrap_or(false) {
        let text: String = result.get("content")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(|v| v.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        bail!("tool error: {}", if text.is_empty() { "unknown error" } else { &text });
    }
    Ok(result.get("structuredContent").unwrap_or(result))
}

fn validate(config: &Config) -> Result<()> {
    let mut indegree = HashMap::<String, usize>::new();
    let mut children = HashMap::<String, Vec<String>>::new();
    for task in &config.tasks {
        if indegree.insert(task.name.clone(), 0).is_some() { bail!("duplicate task name: {}", task.name); }
        if !config.providers.contains_key(&task.provider) { bail!("task {} references unknown provider {}", task.name, task.provider); }
    }
    for (name, p) in &config.providers {
        if p.capacity < 1.0 || p.refill_per_sec <= 0.0 { bail!("provider {name} requires capacity >= 1 and refill_per_sec > 0"); }
    }
    for task in &config.tasks {
        for dep in &task.depends_on {
            if !indegree.contains_key(dep) { bail!("task {} depends on unknown task {}", task.name, dep); }
            *indegree.get_mut(&task.name).unwrap() += 1;
            children.entry(dep.clone()).or_default().push(task.name.clone());
        }
    }
    let mut q: VecDeque<_> = indegree.iter().filter(|(_, n)| **n == 0).map(|(n, _)| n.clone()).collect();
    let mut seen = 0;
    while let Some(name) = q.pop_front() {
        seen += 1;
        for child in children.get(&name).into_iter().flatten() {
            let n = indegree.get_mut(child).unwrap();
            *n -= 1;
            if *n == 0 { q.push_back(child.clone()); }
        }
    }
    if seen != config.tasks.len() { bail!("dependency cycle detected"); }
    Ok(())
}

fn exit_code(output: &str, sentinel: &str) -> Option<String> {
    let prefix = format!("{sentinel}:");
    output.match_indices(&prefix)
        .filter_map(|(idx, _)| output[idx + prefix.len()..].split_whitespace().next().map(str::to_string))
        .find(|s| s.chars().all(|ch| ch.is_ascii_digit()))
}

fn add_opt(args: &mut Value, key: &str, value: Option<String>) {
    if let (Some(map), Some(value)) = (args.as_object_mut(), value) {
        map.insert(key.to_string(), Value::String(value));
    }
}

fn safe_name(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).take(40).collect()
}

fn skipped(task: &Task) -> TaskResult {
    TaskResult { status: "skipped".into(), provider: task.provider.clone(), pane_id: "-".into(),
        output: String::new(), error: Some("dependency failed".into()), elapsed: 0.0 }
}

fn print_table(tasks: &[Task], results: &HashMap<String, TaskResult>) {
    println!("task provider status pane elapsed");
    println!("---- -------- ------ ---- -------");
    for task in tasks {
        if let Some(r) = results.get(&task.name) {
            println!("{} {} {} {} {:.2}s", task.name, r.provider, r.status, r.pane_id, r.elapsed);
        }
    }
}

fn default_direction() -> String { "right".into() }
fn default_startup_delay() -> f64 { 0.3 }
fn default_poll_interval() -> f64 { 0.25 }
fn default_timeout() -> f64 { 30.0 }
fn default_kill_on_done() -> bool { true }
