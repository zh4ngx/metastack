mod bucket;
mod mcp;

use anyhow::{Context, Result, bail};
use bucket::TokenBucket;
use mcp::McpClient;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
use tokio::{
    task::JoinSet,
    time::{Duration, sleep, timeout},
};
use uuid::Uuid;

#[derive(Clone, Deserialize)]
struct Config {
    mcp_binary: String,
    session: Option<String>,
    #[serde(default = "default_direction")]
    direction: String,
    #[serde(default)]
    target_pane_id: Option<String>,
    #[serde(default)]
    floating: bool,
    #[serde(default = "default_startup_delay")]
    startup_delay: f64,
    #[serde(default = "default_poll_interval")]
    poll_interval: f64,
    #[serde(default = "default_timeout")]
    timeout: f64,
    #[serde(default = "default_kill_on_done")]
    kill_on_done: bool,
    providers: HashMap<String, Provider>,
    tasks: Vec<Task>,
}

#[derive(Clone, Deserialize)]
struct Provider {
    command: Vec<String>,
    prompt_mode: PromptMode,
    capacity: f64,
    refill_per_sec: f64,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PromptMode {
    Shell,
    Instruction,
}

#[derive(Clone, Deserialize)]
struct Task {
    name: String,
    provider: String,
    prompt: String,
    #[serde(rename = "depends-on", default)]
    depends_on: Vec<String>,
    cwd: Option<String>,
    direction: Option<String>,
    target_pane_id: Option<String>,
}

#[allow(dead_code)]
#[derive(Clone)]
struct TaskResult {
    status: String,
    provider: String,
    pane_id: String,
    output: String,
    error: Option<String>,
    elapsed: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let path = env::args()
        .nth(1)
        .unwrap_or_else(|| "./metastack.yaml".into());
    let text = fs::read_to_string(&path).with_context(|| format!("failed to read {path}"))?;
    let config: Config = serde_yml::from_str(&text).context("failed to parse YAML")?;
    validate(&config)?;
    let base = Path::new(&path)
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();

    let output_dir = env::args()
        .nth(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let session = config
        .session
        .clone()
        .or_else(detect_zellij_session_name);
    let mut config = config;
    config.session = session;

    let (client, mut child) = McpClient::start(&config.mcp_binary).await?;
    client
        .request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05", "capabilities": {},
                "clientInfo": {"name": "metastack", "version": "0.1.0"}
            }),
        )
        .await?;
    client
        .notify("notifications/initialized", json!({}))
        .await?;

    let results = orchestrate(Arc::new(config.clone()), client.clone(), base, output_dir).await?;
    client.close().await;
    if timeout(Duration::from_secs(2), child.wait()).await.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    print_table(&config.tasks, &results);
    Ok(())
}

async fn orchestrate(
    config: Arc<Config>,
    client: Arc<McpClient>,
    base: PathBuf,
    output_dir: PathBuf,
) -> Result<HashMap<String, TaskResult>> {
    let tasks: HashMap<_, _> = config
        .tasks
        .iter()
        .cloned()
        .map(|t| (t.name.clone(), t))
        .collect();
    let buckets = Arc::new(
        config
            .providers
            .iter()
            .map(|(n, p)| {
                (
                    n.clone(),
                    Arc::new(TokenBucket::new(p.capacity, p.refill_per_sec)),
                )
            })
            .collect::<HashMap<_, _>>(),
    );
    let mut pending: HashSet<_> = tasks.keys().cloned().collect();
    let mut results = HashMap::new();
    let mut joins = JoinSet::new();

    while !pending.is_empty() || !joins.is_empty() {
        for name in pending.clone() {
            let task = tasks.get(&name).unwrap();
            let failed_dep = task.depends_on.iter().any(|d| {
                results
                    .get(d)
                    .is_some_and(|r: &TaskResult| r.status != "done")
            });
            if failed_dep {
                results.insert(name.clone(), skipped(task));
                pending.remove(&name);
            } else if task.depends_on.iter().all(|d| results.contains_key(d)) {
                let (task, cfg, cli, bs, dir) = (
                    task.clone(),
                    config.clone(),
                    client.clone(),
                    buckets.clone(),
                    base.clone(),
                );
                pending.remove(&name);
                joins.spawn(async move {
                    let result = run_task(task.clone(), cfg, cli, bs, dir).await;
                    (task.name, result)
                });
            }
        }
        if let Some(done) = joins.join_next().await {
            let (name, result) = done.context("task join failed")?;
            write_artifact(&name, &result.output, &output_dir);
            results.insert(name, result);
        }
    }
    Ok(results)
}

fn write_artifact(name: &str, output: &str, output_dir: &Path) {
    if let Err(e) = fs::create_dir_all(output_dir) {
        eprintln!(
            "warning: failed to create artifact dir {}: {e}",
            output_dir.display()
        );
        return;
    }
    let path = output_dir.join(format!("{name}.txt"));
    if let Err(e) = fs::write(&path, output) {
        eprintln!("warning: failed to write artifact {}: {e}", path.display());
    }
}

async fn run_task(
    task: Task,
    config: Arc<Config>,
    client: Arc<McpClient>,
    buckets: Arc<HashMap<String, Arc<TokenBucket>>>,
    base: PathBuf,
) -> TaskResult {
    let started = Instant::now();
    match run_task_inner(&task, config, client, buckets, base, started).await {
        Ok(result) => result,
        Err(err) => TaskResult {
            status: "failed".into(),
            provider: task.provider,
            pane_id: "-".into(),
            output: String::new(),
            error: Some(err.to_string()),
            elapsed: started.elapsed().as_secs_f64(),
        },
    }
}

async fn run_task_inner(
    task: &Task,
    config: Arc<Config>,
    client: Arc<McpClient>,
    buckets: Arc<HashMap<String, Arc<TokenBucket>>>,
    base: PathBuf,
    started: Instant,
) -> Result<TaskResult> {
    let provider = config
        .providers
        .get(&task.provider)
        .context("unknown provider")?;
    buckets
        .get(&task.provider)
        .context("missing token bucket")?
        .acquire()
        .await;
    let safe = safe_name(&task.name);
    let uuid = Uuid::new_v4().simple().to_string();
    let sentinel = format!("__METASTACK_DONE_{}_{}__", safe, &uuid[..8]);
    let prompt = match provider.prompt_mode {
        PromptMode::Instruction => format!(
            "{}\n\nWhen complete, print exactly {}:0 on its own line.",
            task.prompt, sentinel
        ),
        PromptMode::Shell => format!("{}\nprintf '\\n{}:%s\\n' \"$?\"", task.prompt, sentinel),
    };

    let cwd = task
        .cwd
        .clone()
        .unwrap_or_else(|| base.to_string_lossy().into_owned());
    let direction = task
        .direction
        .clone()
        .unwrap_or_else(|| config.direction.clone());
    let target_pane_id = task
        .target_pane_id
        .clone()
        .or_else(|| config.target_pane_id.clone());
    let mut args = json!({"cwd": cwd, "command": provider.command, "name": format!("ms-{safe}"), "floating": config.floating, "direction": direction});
    add_opt(&mut args, "session", config.session.clone());
    add_opt(&mut args, "keep_focus_on", env::var("ZELLIJ_PANE_ID").ok());
    add_opt(&mut args, "target_pane_id", target_pane_id);
    let pane_id = tool_data(&client.call_tool("spawn-pane", args).await?)?
        .get("pane_id")
        .and_then(Value::as_str)
        .context("spawn-pane did not return pane_id")?
        .to_string();

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
            output = extract_text(tool_data(&client.call_tool("read-pane", read_args).await?)?)
                .unwrap_or_default();
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
        let _ = client
            .call_tool("kill-pane", args)
            .await
            .and_then(|r| tool_data(&r).map(|_| ()));
    }
    Ok(TaskResult {
        status: if error.is_some() {
            "failed".into()
        } else {
            status
        },
        provider: task.provider.clone(),
        pane_id,
        output,
        error,
        elapsed: started.elapsed().as_secs_f64(),
    })
}

fn tool_data(result: &Value) -> Result<&Value> {
    if result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let text: String = result
            .get("content")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        bail!(
            "tool error: {}",
            if text.is_empty() {
                "unknown error"
            } else {
                &text
            }
        );
    }
    Ok(result.get("structuredContent").unwrap_or(result))
}

fn validate(config: &Config) -> Result<()> {
    let mut indegree = HashMap::<String, usize>::new();
    let mut children = HashMap::<String, Vec<String>>::new();
    for task in &config.tasks {
        if indegree.insert(task.name.clone(), 0).is_some() {
            bail!("duplicate task name: {}", task.name);
        }
        if !config.providers.contains_key(&task.provider) {
            bail!(
                "task {} references unknown provider {}",
                task.name,
                task.provider
            );
        }
    }
    for (name, p) in &config.providers {
        if p.capacity < 1.0 || p.refill_per_sec <= 0.0 {
            bail!("provider {name} requires capacity >= 1 and refill_per_sec > 0");
        }
        let is_shell = p.command.iter().any(|c| {
            c.contains("sh") || c.contains("bash") || c.contains("fish") || c.contains("zsh")
        });
        if !is_shell && matches!(p.prompt_mode, PromptMode::Instruction) {
            eprintln!(
                "warning: provider {name} uses non-shell command with instruction prompt_mode; consider shell mode or wrapping with sh"
            );
        }
    }
    for task in &config.tasks {
        for dep in &task.depends_on {
            if !indegree.contains_key(dep) {
                bail!("task {} depends on unknown task {}", task.name, dep);
            }
            *indegree.get_mut(&task.name).unwrap() += 1;
            children
                .entry(dep.clone())
                .or_default()
                .push(task.name.clone());
        }
    }
    let mut q: VecDeque<_> = indegree
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(n, _)| n.clone())
        .collect();
    let mut seen = 0;
    while let Some(name) = q.pop_front() {
        seen += 1;
        for child in children.get(&name).into_iter().flatten() {
            let n = indegree.get_mut(child).unwrap();
            *n -= 1;
            if *n == 0 {
                q.push_back(child.clone());
            }
        }
    }
    if seen != config.tasks.len() {
        bail!("dependency cycle detected");
    }
    Ok(())
}

fn exit_code(output: &str, sentinel: &str) -> Option<String> {
    let prefix = format!("{sentinel}:");
    output
        .match_indices(&prefix)
        .filter_map(|(idx, _)| {
            output[idx + prefix.len()..]
                .split_whitespace()
                .next()
                .map(str::to_string)
        })
        .find(|s| s.chars().all(|ch| ch.is_ascii_digit()))
}

fn extract_text(value: &Value) -> Option<String> {
    if let Some(text) = value.get("text").and_then(Value::as_str) {
        return Some(text.to_string());
    }
    value
        .get("content")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .filter_map(|v| v.get("text").and_then(Value::as_str))
                .next()
        })
        .map(|s| s.to_string())
}

fn detect_zellij_session_name() -> Option<String> {
    detect_zellij_session_name_from(|key| env::var(key).ok())
}

fn detect_zellij_session_name_from(
    mut get_var: impl FnMut(&str) -> Option<String>,
) -> Option<String> {
    // Zellij documents ZELLIJ as a presence marker set to "0"; the session
    // name lives in ZELLIJ_SESSION_NAME.
    get_var("ZELLIJ_SESSION_NAME").and_then(|name| {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn add_opt(args: &mut Value, key: &str, value: Option<String>) {
    if let (Some(map), Some(value)) = (args.as_object_mut(), value) {
        map.insert(key.to_string(), Value::String(value));
    }
}

fn safe_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(40)
        .collect()
}

fn skipped(task: &Task) -> TaskResult {
    TaskResult {
        status: "skipped".into(),
        provider: task.provider.clone(),
        pane_id: "-".into(),
        output: String::new(),
        error: Some("dependency failed".into()),
        elapsed: 0.0,
    }
}

fn print_table(tasks: &[Task], results: &HashMap<String, TaskResult>) {
    println!("task provider status pane elapsed");
    println!("---- -------- ------ ---- -------");
    for task in tasks {
        if let Some(r) = results.get(&task.name) {
            println!(
                "{} {} {} {} {:.2}s",
                task.name, r.provider, r.status, r.pane_id, r.elapsed
            );
        }
    }
}

fn default_direction() -> String {
    "right".into()
}
fn default_startup_delay() -> f64 {
    0.3
}
fn default_poll_interval() -> f64 {
    0.25
}
fn default_timeout() -> f64 {
    30.0
}
fn default_kill_on_done() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_zellij_session_name_from_environment() {
        let session = detect_zellij_session_name_from(|key| match key {
            "ZELLIJ_SESSION_NAME" => Some("mz".to_string()),
            _ => None,
        });

        assert_eq!(session.as_deref(), Some("mz"));
    }

    #[test]
    fn does_not_use_zellij_marker_as_session_name() {
        let mut requested = Vec::new();
        let session = detect_zellij_session_name_from(|key| {
            requested.push(key.to_string());
            match key {
                "ZELLIJ" => Some("0".to_string()),
                _ => None,
            }
        });

        assert_eq!(session, None);
        assert_eq!(requested, vec!["ZELLIJ_SESSION_NAME"]);
    }

    #[test]
    fn ignores_blank_zellij_session_name() {
        let session = detect_zellij_session_name_from(|key| match key {
            "ZELLIJ_SESSION_NAME" => Some("   ".to_string()),
            _ => None,
        });

        assert_eq!(session, None);
    }
}
