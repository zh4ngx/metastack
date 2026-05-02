mod bucket;
mod mcp;
mod routing;

use anyhow::{Context, Result, bail};
use bucket::TokenBucket;
use mcp::McpClient;
use serde::Deserialize;
use serde::de;
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
#[serde(deny_unknown_fields)]
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
    startup_delay: StartupDelay,
    #[serde(default = "default_poll_interval")]
    poll_interval: PollInterval,
    #[serde(default = "default_timeout")]
    timeout: Timeout,
    #[allow(dead_code)]
    #[serde(default = "default_kill_on_done")]
    kill_on_done: bool,
    providers: HashMap<String, Provider>,
    tasks: Vec<Task>,
}

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
struct StartupDelay(f64);

impl StartupDelay {
    const DEFAULT: Self = Self(0.3);

    fn new(value: f64) -> std::result::Result<Self, &'static str> {
        if value.is_finite() && value >= 0.0 {
            Ok(Self(value))
        } else {
            Err("startup_delay must be finite and >= 0")
        }
    }

    fn as_duration(self) -> Duration {
        Duration::from_secs_f64(self.0)
    }
}

impl TryFrom<f64> for StartupDelay {
    type Error = &'static str;

    fn try_from(value: f64) -> std::result::Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for StartupDelay {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::try_from(f64::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
struct PollInterval(f64);

impl PollInterval {
    const DEFAULT: Self = Self(0.25);

    fn new(value: f64) -> std::result::Result<Self, &'static str> {
        if value.is_finite() && value > 0.0 {
            Ok(Self(value))
        } else {
            Err("poll_interval must be finite and > 0")
        }
    }

    fn as_duration(self) -> Duration {
        Duration::from_secs_f64(self.0)
    }
}

impl TryFrom<f64> for PollInterval {
    type Error = &'static str;

    fn try_from(value: f64) -> std::result::Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for PollInterval {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::try_from(f64::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
struct Timeout(f64);

impl Timeout {
    const DEFAULT: Self = Self(30.0);

    fn new(value: f64) -> std::result::Result<Self, &'static str> {
        if value.is_finite() && value > 0.0 {
            Ok(Self(value))
        } else {
            Err("timeout must be finite and > 0")
        }
    }

    fn as_duration(self) -> Duration {
        Duration::from_secs_f64(self.0)
    }
}

impl TryFrom<f64> for Timeout {
    type Error = &'static str;

    fn try_from(value: f64) -> std::result::Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for Timeout {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::try_from(f64::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

const SEND_USAGE: &str = "usage: metastack send [<routing-config.yaml>] <target> <message...>";
const HELP: &str = "metastack

Usage:
  metastack [config.yaml] [output-dir]
  metastack send [routing-config.yaml] <target> <message...>
  metastack --help
  metastack --version

Modes:
  DAG runner       Run a YAML task DAG through zellij-mcp panes.
  Structured send  Submit one message turn to a configured agent target.

Default files:
  DAG config:      ./metastack.yaml
  Routing config:  $XDG_CONFIG_HOME/metastack/routing.yaml or ~/.config/metastack/routing.yaml
";
const METASTACK_VERSION: &str = env!("CARGO_PKG_VERSION");

fn mcp_initialize_request() -> Value {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "metastack", "version": METASTACK_VERSION}
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args
        .first()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        println!("{HELP}");
        return Ok(());
    }
    if args
        .first()
        .is_some_and(|arg| arg == "--version" || arg == "-V")
    {
        println!("metastack {METASTACK_VERSION}");
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "send") {
        return send_command(&args[1..]).await;
    }
    if args.first().is_some_and(|arg| arg == "inject") {
        bail!("metastack inject has been renamed; use metastack send");
    }

    let path = args
        .first()
        .cloned()
        .unwrap_or_else(|| "./metastack.yaml".into());
    let text = fs::read_to_string(&path).with_context(|| format!("failed to read {path}"))?;
    let config: Config = serde_yml::from_str(&text).context("failed to parse YAML")?;
    validate(&config)?;
    let base = Path::new(&path)
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();

    let output_dir = args
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let session = config.session.clone().or_else(detect_zellij_session_name);
    let mut config = config;
    config.session = session;

    let (client, mut child) = McpClient::start(&config.mcp_binary).await?;
    client
        .request("initialize", mcp_initialize_request())
        .await?;
    client
        .notify("notifications/initialized", json!({}))
        .await?;

    let mut results = orchestrate(
        Arc::new(config.clone()),
        client.clone(),
        base,
        output_dir.clone(),
    )
    .await?;
    wait_for_spawned_panes(client.clone(), &config, &mut results, &output_dir).await?;
    client.close().await;
    if timeout(Duration::from_secs(2), child.wait()).await.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    print_table(&config.tasks, &results);
    Ok(())
}

async fn send_command(args: &[String]) -> Result<()> {
    let (config_path, target, message) = parse_send_args(args, |key| env::var(key).ok())?;
    let origin = env::var("USER").unwrap_or_else(|_| "metastack".to_string());
    let receipt =
        routing::send_from_config_path(config_path.as_path(), &target, message, origin).await?;

    println!("{}", format_send_receipt(&receipt));
    Ok(())
}

fn parse_send_args(
    args: &[String],
    get_var: impl FnMut(&str) -> Option<String>,
) -> Result<(PathBuf, String, String)> {
    let first = args.first().context(SEND_USAGE)?;
    let first_is_config = looks_like_routing_config_path(first);
    let (config_path, target, message_parts) = if first_is_config {
        (
            PathBuf::from(first),
            args.get(1).context(SEND_USAGE)?.clone(),
            args.get(2..).context(SEND_USAGE)?,
        )
    } else {
        (
            default_routing_config_path(get_var)?,
            first.clone(),
            args.get(1..).context(SEND_USAGE)?,
        )
    };

    let message = (!message_parts.is_empty())
        .then(|| message_parts.join(" "))
        .context(SEND_USAGE)?;
    if message.trim().is_empty() {
        bail!("metastack send message cannot be blank");
    }

    Ok((config_path, target, message))
}

fn looks_like_routing_config_path(value: &str) -> bool {
    value.contains('/') || value.ends_with(".yaml") || value.ends_with(".yml")
}

fn default_routing_config_path(mut get_var: impl FnMut(&str) -> Option<String>) -> Result<PathBuf> {
    if let Some(config_home) = get_var("XDG_CONFIG_HOME").filter(|value| !value.trim().is_empty()) {
        return Ok(PathBuf::from(config_home).join("metastack/routing.yaml"));
    }

    let home = get_var("HOME")
        .filter(|value| !value.trim().is_empty())
        .context("metastack send needs routing-config.yaml or XDG_CONFIG_HOME/HOME")?;
    Ok(PathBuf::from(home).join(".config/metastack/routing.yaml"))
}

fn format_send_receipt(receipt: &routing::SendReceipt) -> String {
    let mut fields = vec![
        "receipt".to_string(),
        format!("backend={:?}", receipt.backend),
        format!("target={}", receipt_value(&receipt.target)),
        format!("transport_status={:?}", receipt.status),
    ];
    fields.push(match receipt.status {
        routing::SendStatus::Accepted => "delivery=backend_accepted".to_string(),
        routing::SendStatus::Submitted => "delivery=local_submission_only".to_string(),
    });
    fields.push("completion=not_tracked".to_string());
    if let Some(session_id) = &receipt.session_id {
        fields.push(format!("session_id={}", receipt_value(session_id)));
    }
    if let Some(thread_id) = &receipt.thread_id {
        fields.push(format!("thread_id={}", receipt_value(thread_id)));
    }
    fields.push(format!(
        "correlation_id={}",
        receipt_value(&receipt.correlation_id)
    ));
    fields.join(" ")
}

fn receipt_value(value: &str) -> String {
    if !value.is_empty() && value.bytes().all(is_receipt_value_safe) {
        value.to_string()
    } else {
        serde_json::to_string(value).expect("string serialization cannot fail")
    }
}

fn is_receipt_value_safe(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/' | b':' | b'@')
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
            if has_failed_dependency(task, &results) {
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
    let path = artifact_path(name, output_dir);
    if let Err(e) = fs::write(&path, output) {
        eprintln!("warning: failed to write artifact {}: {e}", path.display());
    }
}

fn artifact_path(name: &str, output_dir: &Path) -> PathBuf {
    output_dir.join(format!("metastack-{}.txt", safe_name(name)))
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
        PromptMode::Instruction => instruction_prompt(&task.prompt, &sentinel),
        PromptMode::Shell => format!(
            "{}\n__metastack_code=$?\nprintf '\\n{}:%s\\n' \"$__metastack_code\"\nexit \"$__metastack_code\"",
            task.prompt, sentinel
        ),
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
    let call_timeout = config.timeout.as_duration();
    let spawn_result = timeout(call_timeout, client.call_tool("spawn-pane", args))
        .await
        .context("spawn-pane timed out")??;
    let pane_id = tool_data(&spawn_result)?
        .get("pane_id")
        .and_then(Value::as_str)
        .context("spawn-pane did not return pane_id")?
        .to_string();

    let mut output = String::new();
    let mut status = "timeout".to_string();
    let work = async {
        sleep(config.startup_delay.as_duration()).await;
        let mut send_args = json!({"pane_id": pane_id, "text": prompt, "submit": true});
        add_opt(&mut send_args, "session", config.session.clone());
        let send_result = timeout(call_timeout, client.call_tool("send-text", send_args))
            .await
            .context("send-text timed out")??;
        tool_data(&send_result)?;
        loop {
            let mut read_args = json!({"pane_id": pane_id, "full": true});
            add_opt(&mut read_args, "session", config.session.clone());
            let read_result = timeout(call_timeout, client.call_tool("read-pane", read_args))
                .await
                .context("read-pane timed out")??;
            output = extract_text(tool_data(&read_result)?).unwrap_or_default();
            if let Some(code) = exit_code(&output, &sentinel) {
                status = if code == "0" { "done" } else { "failed" }.into();
                break;
            }
            if started.elapsed() >= config.timeout.as_duration() {
                break;
            }
            sleep(config.poll_interval.as_duration()).await;
        }
        Ok::<(), anyhow::Error>(())
    };
    let error = work.await.err().map(|e| e.to_string());
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

async fn wait_for_spawned_panes(
    client: Arc<McpClient>,
    config: &Config,
    results: &mut HashMap<String, TaskResult>,
    output_dir: &Path,
) -> Result<()> {
    let mut running = drainable_panes(results);

    while !running.is_empty() {
        for name in running.keys().cloned().collect::<Vec<_>>() {
            let pane_id = running.get(&name).cloned().unwrap();
            let mut args = json!({"pane_id": pane_id, "full": true});
            add_opt(&mut args, "session", config.session.clone());
            let read = timeout(
                config.timeout.as_duration(),
                client.call_tool("read-pane", args),
            )
            .await;

            match read {
                Ok(Ok(result)) => {
                    if let Ok(data) = tool_data(&result) {
                        if let (Some(result), Some(output)) =
                            (results.get_mut(&name), extract_text(data))
                        {
                            result.output = output;
                            write_artifact(&name, &result.output, output_dir);
                        }
                        running.remove(&name);
                    } else {
                        running.remove(&name);
                    }
                }
                Ok(Err(_)) | Err(_) => {
                    running.remove(&name);
                }
            }
        }
        if !running.is_empty() {
            sleep(config.poll_interval.as_duration()).await;
        }
    }
    Ok(())
}

fn drainable_panes(results: &HashMap<String, TaskResult>) -> HashMap<String, String> {
    results
        .iter()
        .filter(|(_, result)| result.pane_id != "-" && result.status == "done")
        .map(|(name, result)| (name.clone(), result.pane_id.clone()))
        .collect()
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
    let mut artifact_names = HashMap::<String, String>::new();
    for task in &config.tasks {
        if indegree.insert(task.name.clone(), 0).is_some() {
            bail!("duplicate task name: {}", task.name);
        }
        let artifact_name = safe_name(&task.name);
        if artifact_name.is_empty() {
            bail!("task {} produces an empty artifact name", task.name);
        }
        if let Some(previous) = artifact_names.insert(artifact_name.clone(), task.name.clone()) {
            bail!(
                "task {} and task {} both produce artifact name {}",
                previous,
                task.name,
                artifact_name
            );
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
        if !p.capacity.is_finite() || !p.refill_per_sec.is_finite() {
            bail!("provider {name} requires finite capacity and refill_per_sec");
        }
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
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix(&prefix)
                .map(str::trim)
                .map(str::to_string)
        })
        .find(|s| !s.is_empty() && s.chars().all(|ch| ch.is_ascii_digit()))
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

fn has_failed_dependency(task: &Task, results: &HashMap<String, TaskResult>) -> bool {
    task.depends_on
        .iter()
        .any(|d| results.get(d).is_some_and(|r| r.status != "done"))
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

fn instruction_prompt(prompt: &str, sentinel: &str) -> String {
    format!(
        "{prompt}\n\nWhen complete, print the token {sentinel} followed immediately by a colon and exit code 0 on its own line."
    )
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
fn default_startup_delay() -> StartupDelay {
    StartupDelay::DEFAULT
}
fn default_poll_interval() -> PollInterval {
    PollInterval::DEFAULT
}
fn default_timeout() -> Timeout {
    Timeout::DEFAULT
}
fn default_kill_on_done() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> Config {
        Config {
            mcp_binary: "mcp".into(),
            session: None,
            direction: default_direction(),
            target_pane_id: None,
            floating: false,
            startup_delay: StartupDelay::new(0.3).unwrap(),
            poll_interval: PollInterval::new(0.25).unwrap(),
            timeout: Timeout::new(30.0).unwrap(),
            kill_on_done: default_kill_on_done(),
            providers: HashMap::from([(
                "default".into(),
                Provider {
                    command: vec!["sh".into()],
                    prompt_mode: PromptMode::Shell,
                    capacity: 1.0,
                    refill_per_sec: 1.0,
                },
            )]),
            tasks: vec![Task {
                name: "task".into(),
                provider: "default".into(),
                prompt: "true".into(),
                depends_on: Vec::new(),
                cwd: None,
                direction: None,
                target_pane_id: None,
            }],
        }
    }

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

    #[test]
    fn validates_timing_values() {
        let config = valid_config();
        assert!(validate(&config).is_ok());

        assert_eq!(
            StartupDelay::new(-0.1),
            Err("startup_delay must be finite and >= 0")
        );
        assert_eq!(
            PollInterval::new(0.0),
            Err("poll_interval must be finite and > 0")
        );
        assert_eq!(
            Timeout::new(f64::INFINITY),
            Err("timeout must be finite and > 0")
        );

        let invalid: std::result::Result<Config, _> = serde_yml::from_str(
            r#"
mcp_binary: mcp
startup_delay: -0.1
poll_interval: 0.25
timeout: 30
providers:
  default:
    command: [sh]
    prompt_mode: shell
    capacity: 1
    refill_per_sec: 1
tasks:
  - name: task
    provider: default
    prompt: "true"
"#,
        );
        assert!(
            invalid
                .err()
                .unwrap()
                .to_string()
                .contains("startup_delay must be finite and >= 0")
        );
    }

    #[test]
    fn rejects_unknown_dag_config_fields() {
        let top_level: std::result::Result<Config, _> = serde_yml::from_str(
            r#"
mcp_binary: mcp
unknown: true
providers:
  default:
    command: [sh]
    prompt_mode: shell
    capacity: 1
    refill_per_sec: 1
tasks:
  - name: task
    provider: default
    prompt: "true"
"#,
        );
        assert!(top_level.is_err());

        let provider: std::result::Result<Config, _> = serde_yml::from_str(
            r#"
mcp_binary: mcp
providers:
  default:
    command: [sh]
    prompt_mode: shell
    capacity: 1
    refill_per_sec: 1
    extra: true
tasks:
  - name: task
    provider: default
    prompt: "true"
"#,
        );
        assert!(provider.is_err());

        let task: std::result::Result<Config, _> = serde_yml::from_str(
            r#"
mcp_binary: mcp
providers:
  default:
    command: [sh]
    prompt_mode: shell
    capacity: 1
    refill_per_sec: 1
tasks:
  - name: task
    provider: default
    prompt: "true"
    depends_on: [other]
"#,
        );
        assert!(task.is_err());
    }

    #[test]
    fn smoke_test_example_yaml_stays_parseable() {
        let config: Config = serde_yml::from_str(include_str!("../smoke-test.example.yaml"))
            .expect("smoke-test.example.yaml should parse");

        assert_eq!(config.mcp_binary, "zellij-mcp");
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn default_metastack_yaml_stays_portable() {
        let config: Config =
            serde_yml::from_str(include_str!("../metastack.yaml")).expect("metastack.yaml parses");

        assert_eq!(config.mcp_binary, "zellij-mcp");
        assert!(config.session.is_none());
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn validates_artifact_names_are_non_empty_and_unique() {
        let mut config = valid_config();
        config.tasks[0].name = "api/v1".to_string();
        config.tasks.push(Task {
            name: "api:v1".into(),
            provider: "default".into(),
            prompt: "true".into(),
            depends_on: Vec::new(),
            cwd: None,
            direction: None,
            target_pane_id: None,
        });

        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("both produce artifact name api-v1"));

        let mut config = valid_config();
        config.tasks[0].name = String::new();
        let error = validate(&config).unwrap_err().to_string();

        assert!(error.contains("produces an empty artifact name"));
    }

    #[test]
    fn validates_provider_rates_are_finite() {
        let mut config = valid_config();
        config.providers.get_mut("default").unwrap().capacity = f64::INFINITY;

        let error = validate(&config).unwrap_err().to_string();

        assert!(error.contains("requires finite capacity and refill_per_sec"));
    }

    fn send_args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    #[test]
    fn parses_send_args_with_explicit_config_path() {
        let (config_path, target, message) = parse_send_args(
            &send_args(&["routing.example.yaml", "local-codex", "status", "update"]),
            |_| None,
        )
        .unwrap();

        assert_eq!(config_path, PathBuf::from("routing.example.yaml"));
        assert_eq!(target, "local-codex");
        assert_eq!(message, "status update");
    }

    #[test]
    fn parses_send_args_with_path_like_extensionless_config() {
        let (config_path, target, message) =
            parse_send_args(&send_args(&["./routing", "local-codex", "status"]), |_| {
                None
            })
            .unwrap();

        assert_eq!(config_path, PathBuf::from("./routing"));
        assert_eq!(target, "local-codex");
        assert_eq!(message, "status");
    }

    #[test]
    fn parses_send_args_with_yml_config_suffix() {
        let (config_path, target, message) = parse_send_args(
            &send_args(&["routing.yml", "local-codex", "status"]),
            |_| None,
        )
        .unwrap();

        assert_eq!(config_path, PathBuf::from("routing.yml"));
        assert_eq!(target, "local-codex");
        assert_eq!(message, "status");
    }

    #[test]
    fn parses_send_args_with_default_xdg_config_path() {
        let (config_path, target, message) = parse_send_args(
            &send_args(&["local-codex", "status", "update"]),
            |key| match key {
                "XDG_CONFIG_HOME" => Some("/tmp/xdg".to_string()),
                "HOME" => Some("/home/andy".to_string()),
                _ => None,
            },
        )
        .unwrap();

        assert_eq!(
            config_path,
            PathBuf::from("/tmp/xdg/metastack/routing.yaml")
        );
        assert_eq!(target, "local-codex");
        assert_eq!(message, "status update");
    }

    #[test]
    fn parses_send_args_with_home_config_fallback() {
        let (config_path, target, message) =
            parse_send_args(&send_args(&["local-codex", "status"]), |key| match key {
                "HOME" => Some("/home/andy".to_string()),
                _ => None,
            })
            .unwrap();

        assert_eq!(
            config_path,
            PathBuf::from("/home/andy/.config/metastack/routing.yaml")
        );
        assert_eq!(target, "local-codex");
        assert_eq!(message, "status");
    }

    #[test]
    fn parses_send_args_with_blank_xdg_home_fallback() {
        let (config_path, target, message) =
            parse_send_args(&send_args(&["local-codex", "status"]), |key| match key {
                "XDG_CONFIG_HOME" => Some("   ".to_string()),
                "HOME" => Some("/home/andy".to_string()),
                _ => None,
            })
            .unwrap();

        assert_eq!(
            config_path,
            PathBuf::from("/home/andy/.config/metastack/routing.yaml")
        );
        assert_eq!(target, "local-codex");
        assert_eq!(message, "status");
    }

    #[test]
    fn send_arg_parser_treats_bare_routing_as_target() {
        let (config_path, target, message) =
            parse_send_args(&send_args(&["routing", "status"]), |key| match key {
                "HOME" => Some("/home/andy".to_string()),
                _ => None,
            })
            .unwrap();

        assert_eq!(
            config_path,
            PathBuf::from("/home/andy/.config/metastack/routing.yaml")
        );
        assert_eq!(target, "routing");
        assert_eq!(message, "status");
    }

    #[test]
    fn send_arg_parser_does_not_probe_cwd_for_config_paths() {
        struct TempFile(PathBuf);

        impl Drop for TempFile {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.0);
            }
        }

        let file_name = format!("metastack-target-{}", std::process::id());
        let path = PathBuf::from(&file_name);
        fs::write(&path, "not a routing config").unwrap();
        let _cleanup = TempFile(path);

        let (config_path, target, message) =
            parse_send_args(&send_args(&[&file_name, "status"]), |key| match key {
                "HOME" => Some("/home/andy".to_string()),
                _ => None,
            })
            .unwrap();

        assert_eq!(
            config_path,
            PathBuf::from("/home/andy/.config/metastack/routing.yaml")
        );
        assert_eq!(target, file_name);
        assert_eq!(message, "status");
    }

    #[test]
    fn mcp_initialize_request_uses_package_version() {
        let request = mcp_initialize_request();

        assert_eq!(request["clientInfo"]["name"], "metastack");
        assert_eq!(request["clientInfo"]["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn send_arg_parser_requires_default_config_environment() {
        let err = parse_send_args(&send_args(&["local-codex", "status"]), |_| None).unwrap_err();

        assert!(
            err.to_string()
                .contains("routing-config.yaml or XDG_CONFIG_HOME/HOME")
        );
    }

    #[test]
    fn send_arg_parser_reports_usage_for_no_args() {
        let err = parse_send_args(&[], |_| None).unwrap_err();

        assert_eq!(err.to_string(), SEND_USAGE);
    }

    #[test]
    fn send_arg_parser_reports_usage_for_missing_message() {
        let err = parse_send_args(&send_args(&["local-codex"]), |key| match key {
            "HOME" => Some("/home/andy".to_string()),
            _ => None,
        })
        .unwrap_err();

        assert_eq!(err.to_string(), SEND_USAGE);
    }

    #[test]
    fn send_arg_parser_rejects_blank_messages() {
        let err = parse_send_args(&send_args(&["local-codex", "   "]), |key| match key {
            "HOME" => Some("/home/andy".to_string()),
            _ => None,
        })
        .unwrap_err();

        assert_eq!(err.to_string(), "metastack send message cannot be blank");
    }

    #[test]
    fn send_arg_parser_keeps_path_like_config_missing_message_as_usage_error() {
        let err =
            parse_send_args(&send_args(&["routing.yaml", "local-codex"]), |_| None).unwrap_err();

        assert_eq!(err.to_string(), SEND_USAGE);
    }

    #[test]
    fn send_receipt_output_distinguishes_completion() {
        let receipt = routing::SendReceipt {
            backend: routing::BackendKind::Claude,
            target: "andy-coh".to_string(),
            correlation_id: "corr-1".to_string(),
            status: routing::SendStatus::Submitted,
            session_id: None,
            thread_id: None,
        };

        assert_eq!(
            format_send_receipt(&receipt),
            "receipt backend=Claude target=andy-coh transport_status=Submitted delivery=local_submission_only completion=not_tracked correlation_id=corr-1"
        );

        let receipt = routing::SendReceipt {
            backend: routing::BackendKind::Codex,
            target: "local-codex".to_string(),
            correlation_id: "corr-2".to_string(),
            status: routing::SendStatus::Accepted,
            session_id: None,
            thread_id: Some("thread-1".to_string()),
        };

        assert_eq!(
            format_send_receipt(&receipt),
            "receipt backend=Codex target=local-codex transport_status=Accepted delivery=backend_accepted completion=not_tracked thread_id=thread-1 correlation_id=corr-2"
        );

        let receipt = routing::SendReceipt {
            backend: routing::BackendKind::OpenCode,
            target: "bad target".to_string(),
            correlation_id: "corr-3".to_string(),
            status: routing::SendStatus::Accepted,
            session_id: Some("ses-1\nspoof=1".to_string()),
            thread_id: None,
        };

        assert_eq!(
            format_send_receipt(&receipt),
            r#"receipt backend=OpenCode target="bad target" transport_status=Accepted delivery=backend_accepted completion=not_tracked session_id="ses-1\nspoof=1" correlation_id=corr-3"#
        );
    }

    #[test]
    fn post_dag_drain_only_tracks_completed_panes() {
        let results = HashMap::from([
            (
                "done".to_string(),
                TaskResult {
                    status: "done".to_string(),
                    provider: "default".to_string(),
                    pane_id: "terminal_1".to_string(),
                    output: String::new(),
                    error: None,
                    elapsed: 0.0,
                },
            ),
            (
                "timeout".to_string(),
                TaskResult {
                    status: "timeout".to_string(),
                    provider: "default".to_string(),
                    pane_id: "terminal_2".to_string(),
                    output: String::new(),
                    error: None,
                    elapsed: 0.0,
                },
            ),
            (
                "failed".to_string(),
                TaskResult {
                    status: "failed".to_string(),
                    provider: "default".to_string(),
                    pane_id: "terminal_3".to_string(),
                    output: String::new(),
                    error: Some("boom".to_string()),
                    elapsed: 0.0,
                },
            ),
        ]);

        assert_eq!(
            drainable_panes(&results),
            HashMap::from([("done".to_string(), "terminal_1".to_string())])
        );
    }

    #[test]
    fn instruction_prompt_does_not_embed_parseable_sentinel_completion() {
        let sentinel = "__METASTACK_DONE_task_abcd1234__";
        let prompt = instruction_prompt("review code", sentinel);

        assert!(prompt.contains(sentinel));
        assert_eq!(exit_code(&prompt, sentinel), None);
    }

    #[test]
    fn extracts_text_from_simple_and_mcp_tool_result_formats() {
        let simple = json!({"text": "plain output"});
        assert_eq!(extract_text(&simple).as_deref(), Some("plain output"));

        let tool_result = json!({
            "content": [
                {"type": "text", "text": "mcp output"}
            ]
        });
        assert_eq!(extract_text(&tool_result).as_deref(), Some("mcp output"));
    }

    #[test]
    fn parses_exit_code_sentinels() {
        let sentinel = "__METASTACK_DONE_task_abcd1234__";

        assert_eq!(
            exit_code(&format!("{sentinel}:0"), sentinel).as_deref(),
            Some("0")
        );
        assert_eq!(
            exit_code(&format!("{sentinel}:1"), sentinel).as_deref(),
            Some("1")
        );
        assert_eq!(exit_code("output without sentinel", sentinel), None);

        assert_eq!(
            exit_code(&format!("{sentinel}:0\ntrailing output"), sentinel).as_deref(),
            Some("0")
        );
        assert_eq!(
            exit_code(&format!("before\n{sentinel}:0\nafter"), sentinel).as_deref(),
            Some("0")
        );
        assert_eq!(
            exit_code(&format!("leading output\n{sentinel}:0"), sentinel).as_deref(),
            Some("0")
        );
        assert_eq!(
            exit_code(
                &format!("{sentinel}:1\nmore output\n{sentinel}:0"),
                sentinel
            )
            .as_deref(),
            Some("1")
        );
        assert_eq!(
            exit_code(
                &format!("print exactly {sentinel}:0 on its own line"),
                sentinel
            ),
            None
        );
        assert_eq!(exit_code(&format!("{sentinel}:0 trailing"), sentinel), None);
        assert_eq!(exit_code(&format!("{sentinel}:"), sentinel), None);
        assert_eq!(exit_code(&format!("{sentinel}:   "), sentinel), None);
    }

    #[test]
    fn constructs_artifact_paths_with_safe_task_names() {
        let output_dir = Path::new("/tmp/metastack-output");

        assert_eq!(
            artifact_path("build", output_dir),
            output_dir.join("metastack-build.txt")
        );
        assert_eq!(
            artifact_path("build: api/v1!", output_dir),
            output_dir.join("metastack-build--api-v1-.txt")
        );
    }

    #[test]
    fn skips_task_when_dependency_failed() {
        let mut task = valid_config().tasks.remove(0);
        task.name = "deploy".into();
        task.depends_on = vec!["build".into()];

        let results = HashMap::from([(
            "build".into(),
            TaskResult {
                status: "failed".into(),
                provider: "default".into(),
                pane_id: "pane-1".into(),
                output: String::new(),
                error: Some("command failed".into()),
                elapsed: 1.0,
            },
        )]);

        assert!(has_failed_dependency(&task, &results));

        let result = skipped(&task);
        assert_eq!(result.status, "skipped");
        assert_eq!(result.error.as_deref(), Some("dependency failed"));
    }
}
