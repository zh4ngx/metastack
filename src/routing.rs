#![allow(dead_code)]

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt, future::BoxFuture};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashMap, fs, path::Path, time::Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    #[serde(rename = "opencode")]
    OpenCode,
    Codex,
    Claude,
    Zellij,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Agent,
    System,
}

impl Default for MessageRole {
    fn default() -> Self {
        Self::User
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RoutingEnvelope {
    pub origin: String,
    pub target: String,
    pub backend: BackendKind,
    #[serde(default)]
    pub role: MessageRole,
    pub message: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub reply_to: Option<String>,
    pub correlation_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InjectionStatus {
    Queued,
    Completed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InjectionReceipt {
    pub backend: BackendKind,
    pub target: String,
    pub correlation_id: String,
    pub status: InjectionStatus,
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TargetHandle {
    OpenCode {
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    Codex {
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread_id: Option<String>,
    },
    Claude {
        channel: String,
    },
    Zellij {
        #[serde(default)]
        session: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane_id: Option<String>,
        #[serde(default)]
        submit_strategy: ZellijSubmitStrategy,
    },
}

impl TargetHandle {
    fn apply_to_envelope(&self, envelope: &mut RoutingEnvelope) {
        match self {
            Self::OpenCode { cwd, session_id } => {
                envelope.cwd = Some(cwd.clone());
                envelope.session_id = session_id.clone();
            }
            Self::Codex { cwd, thread_id } => {
                envelope.cwd = Some(cwd.clone());
                envelope.thread_id = thread_id.clone();
            }
            Self::Claude { .. } | Self::Zellij { .. } => {}
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ZellijSubmitStrategy {
    #[default]
    Enter,
    TextThenEnter,
    ShiftEnter,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackPolicy {
    Never,
    ExplicitLossy,
    OnUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendCapabilities {
    pub backend: BackendKind,
    pub preserves_role: bool,
    pub has_completion_readback: bool,
    pub is_lossy: bool,
}

impl BackendCapabilities {
    pub fn for_kind(backend: BackendKind) -> Self {
        match backend {
            BackendKind::OpenCode => Self {
                backend,
                preserves_role: false,
                has_completion_readback: false,
                is_lossy: false,
            },
            BackendKind::Codex => Self {
                backend,
                preserves_role: false,
                has_completion_readback: true,
                is_lossy: false,
            },
            BackendKind::Claude => Self {
                backend,
                preserves_role: true,
                has_completion_readback: true,
                is_lossy: false,
            },
            BackendKind::Zellij => Self {
                backend,
                preserves_role: false,
                has_completion_readback: false,
                is_lossy: true,
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteEventKind {
    Accepted,
    Submitted,
    Delta,
    Completed,
    Failed,
    Degraded,
    NeedsApproval,
    Timeout,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RouteEvent {
    pub correlation_id: String,
    pub target: String,
    pub backend: BackendKind,
    pub kind: RouteEventKind,
    #[serde(default)]
    pub text: Option<String>,
}

pub trait InjectionBackend: Send + Sync {
    fn kind(&self) -> BackendKind;
    fn inject<'a>(&'a self, envelope: RoutingEnvelope) -> BoxFuture<'a, Result<InjectionReceipt>>;
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RoutingConfig {
    pub version: u8,
    #[serde(default)]
    pub backends: HashMap<String, BackendConfig>,
    #[serde(default)]
    pub agents: HashMap<String, AgentSpec>,
    #[serde(default)]
    pub routes: RouteConfig,
}

impl RoutingConfig {
    pub fn validate(&self) -> Result<()> {
        if self.version != 2 {
            bail!("routing config version {} is not supported", self.version);
        }
        if self.backends.is_empty() {
            bail!("routing config must define at least one backend");
        }
        if self.agents.is_empty() {
            bail!("routing config must define at least one agent");
        }

        for (name, agent) in &self.agents {
            if name.trim().is_empty() {
                bail!("routing agent name cannot be empty");
            }
            if agent.cwd.trim().is_empty() {
                bail!("routing target {name} must define cwd");
            }
            if !self.backends.contains_key(&agent.backend) {
                bail!("target {name} references unknown backend {}", agent.backend);
            }
        }

        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RouteConfig {
    #[serde(default)]
    pub default_reply_to: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentSpec {
    pub backend: String,
    pub cwd: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BackendConfig {
    #[serde(rename = "opencode")]
    OpenCode {
        base_url: String,
    },
    Codex {
        url: String,
        #[serde(default = "default_codex_model")]
        model: String,
        #[serde(default = "default_codex_effort")]
        effort: String,
        #[serde(default = "default_codex_approval_policy")]
        approval_policy: String,
        #[serde(default = "default_codex_sandbox_policy")]
        sandbox_policy: Value,
    },
    Claude {
        channel: String,
    },
    Zellij {
        mcp_binary: String,
        #[serde(default)]
        session: Option<String>,
    },
}

impl BackendConfig {
    pub fn kind(&self) -> BackendKind {
        match self {
            Self::OpenCode { .. } => BackendKind::OpenCode,
            Self::Codex { .. } => BackendKind::Codex,
            Self::Claude { .. } => BackendKind::Claude,
            Self::Zellij { .. } => BackendKind::Zellij,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedTarget {
    pub name: String,
    pub backend_name: String,
    pub backend: BackendConfig,
    pub handle: TargetHandle,
}

#[derive(Clone, Debug)]
pub struct TargetRegistry {
    config: RoutingConfig,
}

impl TargetRegistry {
    pub fn new(config: RoutingConfig) -> Self {
        Self { config }
    }

    pub fn resolve(&self, target: &str) -> Result<ResolvedTarget> {
        let agent = self
            .config
            .agents
            .get(target)
            .with_context(|| format!("unknown routing target {target}"))?;
        let backend = self.config.backends.get(&agent.backend).with_context(|| {
            format!(
                "target {target} references unknown backend {}",
                agent.backend
            )
        })?;

        let handle = match backend {
            BackendConfig::OpenCode { .. } => TargetHandle::OpenCode {
                cwd: agent.cwd.clone(),
                session_id: agent.session_id.clone(),
            },
            BackendConfig::Codex { .. } => TargetHandle::Codex {
                cwd: agent.cwd.clone(),
                thread_id: agent.thread_id.clone(),
            },
            BackendConfig::Claude { channel } => TargetHandle::Claude {
                channel: channel.clone(),
            },
            BackendConfig::Zellij { session, .. } => TargetHandle::Zellij {
                session: session.clone(),
                pane_id: None,
                submit_strategy: ZellijSubmitStrategy::default(),
            },
        };

        Ok(ResolvedTarget {
            name: target.to_string(),
            backend_name: agent.backend.clone(),
            backend: backend.clone(),
            handle,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Router {
    config: RoutingConfig,
}

impl Router {
    pub fn new(config: RoutingConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self { config })
    }

    pub fn from_yaml(text: &str) -> Result<Self> {
        let config: RoutingConfig =
            serde_yml::from_str(text).context("failed to parse routing YAML")?;
        Self::new(config)
    }

    pub fn envelope_for(
        &self,
        target: &str,
        message: String,
        origin: String,
    ) -> Result<(RoutingEnvelope, BackendConfig)> {
        let resolved = TargetRegistry::new(self.config.clone()).resolve(target)?;
        let mut envelope = RoutingEnvelope {
            origin,
            target: target.to_string(),
            backend: resolved.backend.kind(),
            role: MessageRole::User,
            message,
            cwd: None,
            session_id: None,
            thread_id: None,
            reply_to: self.config.routes.default_reply_to.clone(),
            correlation_id: Uuid::new_v4().simple().to_string(),
        };
        resolved.handle.apply_to_envelope(&mut envelope);

        Ok((envelope, resolved.backend))
    }

    pub async fn inject_text(
        &self,
        target: &str,
        message: String,
        origin: String,
    ) -> Result<InjectionReceipt> {
        let (envelope, backend) = self.envelope_for(target, message, origin)?;
        Self::dispatch(backend, envelope).await
    }

    async fn dispatch(
        backend: BackendConfig,
        envelope: RoutingEnvelope,
    ) -> Result<InjectionReceipt> {
        match &backend {
            BackendConfig::OpenCode { base_url } => {
                OpenCodeBackend::new(base_url).inject(envelope).await
            }
            BackendConfig::Codex { .. } => {
                CodexBackend::from_config(&backend)?.inject(envelope).await
            }
            BackendConfig::Claude { .. } => {
                bail!("Claude Huddle backend is documented but not implemented in this prototype")
            }
            BackendConfig::Zellij { .. } => {
                bail!("zellij fallback injection is not implemented in the structured prototype")
            }
        }
    }
}

pub async fn inject_from_config_path(
    path: &Path,
    target: &str,
    message: String,
    origin: String,
) -> Result<InjectionReceipt> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    Router::from_yaml(&text)?
        .inject_text(target, message, origin)
        .await
}

#[derive(Clone)]
pub struct OpenCodeBackend {
    base_url: String,
    client: reqwest::Client,
}

impl OpenCodeBackend {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(default_transport_timeout())
                .build()
                .expect("reqwest client builder should accept transport timeout"),
        }
    }

    fn session_url(&self) -> String {
        format!("{}/session", self.base_url)
    }

    fn prompt_async_url(&self, session_id: &str) -> String {
        format!("{}/session/{session_id}/prompt_async", self.base_url)
    }

    fn prompt_body(message: &str) -> Value {
        json!({"parts": [{"type": "text", "text": message}]})
    }

    pub fn select_session<'a>(
        sessions: &'a [OpenCodeSession],
        cwd: &str,
    ) -> Option<&'a OpenCodeSession> {
        sessions
            .iter()
            .filter(|session| session.directory.as_deref() == Some(cwd))
            .max_by_key(|session| session.updated_sort_key())
    }

    async fn discover_session(&self, cwd: &str) -> Result<String> {
        let sessions: Vec<OpenCodeSession> = self
            .client
            .get(self.session_url())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Self::select_session(&sessions, cwd)
            .map(|session| session.id.clone())
            .with_context(|| format!("no OpenCode session found for cwd {cwd}"))
    }
}

impl InjectionBackend for OpenCodeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::OpenCode
    }

    fn inject<'a>(&'a self, envelope: RoutingEnvelope) -> BoxFuture<'a, Result<InjectionReceipt>> {
        Box::pin(async move {
            ensure_user_message(&envelope)?;
            let session_id = match envelope.session_id.clone() {
                Some(session_id) => session_id,
                None => {
                    let cwd = envelope
                        .cwd
                        .as_deref()
                        .context("OpenCode injection requires cwd or session_id")?;
                    self.discover_session(cwd).await?
                }
            };

            self.client
                .post(self.prompt_async_url(&session_id))
                .json(&Self::prompt_body(&envelope.message))
                .send()
                .await?
                .error_for_status()?;

            Ok(InjectionReceipt {
                backend: BackendKind::OpenCode,
                target: envelope.target,
                correlation_id: envelope.correlation_id,
                status: InjectionStatus::Queued,
                session_id: Some(session_id),
                thread_id: envelope.thread_id,
            })
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct OpenCodeSession {
    pub id: String,
    #[serde(default)]
    pub directory: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    time: Option<OpenCodeSessionTime>,
}

impl OpenCodeSession {
    fn updated_sort_key(&self) -> String {
        if let Some(updated) = self.time.as_ref().and_then(|time| time.updated) {
            return format!("{updated:020}");
        }

        self.updated_at.clone().unwrap_or_default()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct OpenCodeSessionTime {
    #[serde(default)]
    updated: Option<u64>,
}

#[derive(Clone)]
pub struct CodexBackend {
    url: String,
    model: String,
    effort: String,
    approval_policy: String,
    sandbox_policy: Value,
    transport_timeout: Duration,
    completion_timeout: Duration,
}

impl CodexBackend {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            model: default_codex_model(),
            effort: default_codex_effort(),
            approval_policy: default_codex_approval_policy(),
            sandbox_policy: default_codex_sandbox_policy(),
            transport_timeout: default_transport_timeout(),
            completion_timeout: Duration::from_secs(300),
        }
    }

    pub fn from_config(config: &BackendConfig) -> Result<Self> {
        let BackendConfig::Codex {
            url,
            model,
            effort,
            approval_policy,
            sandbox_policy,
        } = config
        else {
            bail!("expected codex backend config");
        };

        Ok(Self {
            url: url.clone(),
            model: model.clone(),
            effort: effort.clone(),
            approval_policy: approval_policy.clone(),
            sandbox_policy: sandbox_policy.clone(),
            transport_timeout: default_transport_timeout(),
            completion_timeout: Duration::from_secs(300),
        })
    }

    fn initialize_request(id: u64) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "metastack-codex-adapter",
                    "title": "metastack Codex adapter",
                    "version": "0.3.0"
                },
                "capabilities": {
                    "experimentalApi": true
                }
            }
        })
    }

    fn thread_list_request(id: u64, cwd: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "thread/list",
            "params": {
                "cwd": cwd,
                "limit": 12,
                "sortKey": "updated_at",
                "sortDirection": "desc",
                "archived": false,
                "sourceKinds": ["cli"],
                "useStateDbOnly": false
            }
        })
    }

    fn thread_resume_request(&self, id: u64, thread_id: &str, cwd: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "thread/resume",
            "params": {
                "threadId": thread_id,
                "cwd": cwd,
                "model": self.model,
                "approvalPolicy": self.approval_policy,
                "excludeTurns": true
            }
        })
    }

    fn turn_start_request(&self, id: u64, thread_id: &str, envelope: &RoutingEnvelope) -> Value {
        let cwd = envelope.cwd.as_deref().unwrap_or(".");
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "turn/start",
            "params": {
                "threadId": thread_id,
                "input": [
                    {
                        "type": "text",
                        "text": envelope.message,
                        "text_elements": []
                    }
                ],
                "cwd": cwd,
                "model": self.model,
                "effort": self.effort,
                "approvalPolicy": self.approval_policy,
                "sandboxPolicy": self.sandbox_policy
            }
        })
    }

    fn initialized_notification() -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": "initialized"
        })
    }

    pub fn select_thread(value: &Value) -> Option<String> {
        Self::select_thread_ref(value).map(|thread| thread.id)
    }

    fn select_thread_ref(value: &Value) -> Option<CodexThreadRef> {
        let threads = value
            .get("data")
            .or_else(|| value.get("threads"))
            .or_else(|| value.get("result").and_then(|result| result.get("data")))
            .or_else(|| value.get("result").and_then(|result| result.get("threads")))
            .and_then(Value::as_array)
            .or_else(|| value.get("result").and_then(Value::as_array))?;

        let newest_cli = threads.iter().find(|thread| {
            Self::thread_source_is_cli(thread) && Self::thread_status(thread) == Some("active")
        });

        newest_cli
            .or_else(|| {
                threads
                    .iter()
                    .find(|thread| Self::thread_source_is_cli(thread))
            })
            .and_then(|thread| {
                let id = thread
                    .get("id")
                    .or_else(|| thread.get("threadId"))
                    .and_then(Value::as_str)?;
                Some(CodexThreadRef {
                    id: id.to_string(),
                    status: Self::thread_status(thread).map(str::to_string),
                })
            })
    }

    fn thread_source_is_cli(thread: &Value) -> bool {
        thread.get("source").and_then(Value::as_str) == Some("cli")
            || thread.get("sourceKind").and_then(Value::as_str) == Some("cli")
    }

    fn thread_status(thread: &Value) -> Option<&str> {
        thread
            .get("status")
            .and_then(|status| status.get("type").or(Some(status)))
            .and_then(Value::as_str)
    }

    fn turn_id(value: &Value) -> Option<String> {
        value
            .get("turn")
            .or_else(|| value.get("params").and_then(|params| params.get("turn")))
            .and_then(|turn| turn.get("id"))
            .or_else(|| value.get("turnId"))
            .or_else(|| value.get("params").and_then(|params| params.get("turnId")))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    fn turn_status(value: &Value) -> Option<&str> {
        value
            .get("turn")
            .or_else(|| value.get("params").and_then(|params| params.get("turn")))
            .and_then(|turn| turn.get("status"))
            .and_then(Value::as_str)
    }

    fn notification_thread_id(value: &Value) -> Option<&str> {
        value
            .get("params")
            .and_then(|params| params.get("threadId"))
            .and_then(Value::as_str)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CodexThreadRef {
    id: String,
    status: Option<String>,
}

impl InjectionBackend for CodexBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Codex
    }

    fn inject<'a>(&'a self, envelope: RoutingEnvelope) -> BoxFuture<'a, Result<InjectionReceipt>> {
        Box::pin(async move {
            ensure_user_message(&envelope)?;
            let cwd = envelope
                .cwd
                .as_deref()
                .context("Codex injection requires cwd")?;
            let (mut ws, _) =
                tokio::time::timeout(self.transport_timeout, connect_async(&self.url))
                    .await
                    .context("timed out connecting to Codex app-server")??;

            ws.send(Message::Text(
                Self::initialize_request(1).to_string().into(),
            ))
            .await?;
            wait_for_response(&mut ws, 1, self.transport_timeout).await?;
            ws.send(Message::Text(
                Self::initialized_notification().to_string().into(),
            ))
            .await?;

            let thread = match envelope.thread_id.clone() {
                Some(thread_id) => CodexThreadRef {
                    id: thread_id,
                    status: None,
                },
                None => {
                    ws.send(Message::Text(
                        Self::thread_list_request(2, cwd).to_string().into(),
                    ))
                    .await?;
                    let response = wait_for_response(&mut ws, 2, self.transport_timeout).await?;
                    Self::select_thread_ref(&response)
                        .with_context(|| format!("no Codex CLI thread found for cwd {cwd}"))?
                }
            };
            let thread_id = thread.id;

            let mut next_id = 3;
            ws.send(Message::Text(
                self.thread_resume_request(next_id, &thread_id, cwd)
                    .to_string()
                    .into(),
            ))
            .await?;
            wait_for_response(&mut ws, next_id, self.transport_timeout).await?;
            next_id += 1;

            ws.send(Message::Text(
                self.turn_start_request(next_id, &thread_id, &envelope)
                    .to_string()
                    .into(),
            ))
            .await?;
            let turn_response = wait_for_response(&mut ws, next_id, self.transport_timeout).await?;
            let turn_id =
                Self::turn_id(&turn_response).context("Codex turn/start did not return turn id")?;

            let completed = tokio::time::timeout(self.completion_timeout, async {
                loop {
                    let value = next_ws_json(&mut ws).await?;
                    if let Some(error) = value.get("error") {
                        bail!("Codex app-server error: {error}");
                    }
                    let method = value.get("method").and_then(Value::as_str);
                    if method == Some("error") {
                        bail!("Codex turn error: {}", value["params"]);
                    }
                    if method.is_some_and(is_codex_approval_or_input_request) {
                        bail!("Codex requested approval or user input: {}", value);
                    }
                    if method == Some("turn/completed") {
                        if Self::notification_thread_id(&value) != Some(thread_id.as_str())
                            || Self::turn_id(&value).as_deref() != Some(turn_id.as_str())
                        {
                            continue;
                        }

                        match Self::turn_status(&value) {
                            Some("completed") => return Ok::<(), anyhow::Error>(()),
                            Some(status) => bail!(
                                "Codex turn completed with status {status}: {}",
                                value["params"]["turn"]["error"]
                            ),
                            None => bail!("Codex turn/completed omitted turn status"),
                        }
                    }
                }
            })
            .await;

            match completed {
                Ok(Ok(())) => Ok(InjectionReceipt {
                    backend: BackendKind::Codex,
                    target: envelope.target,
                    correlation_id: envelope.correlation_id,
                    status: InjectionStatus::Completed,
                    session_id: envelope.session_id,
                    thread_id: Some(thread_id),
                }),
                Ok(Err(err)) => Err(err),
                Err(_) => Err(anyhow!("timed out waiting for Codex turn completion")),
            }
        })
    }
}

async fn wait_for_response<S>(ws: &mut S, id: u64, duration: Duration) -> Result<Value>
where
    S: futures_util::Stream<
            Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>,
        > + Unpin,
{
    tokio::time::timeout(duration, async {
        loop {
            let value = next_ws_json(ws).await?;
            if let Some(error) = value.get("error") {
                bail!("JSON-RPC error for id {id}: {error}");
            }
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    })
    .await
    .with_context(|| format!("timed out waiting for JSON-RPC response id {id}"))?
}

async fn next_ws_json<S>(ws: &mut S) -> Result<Value>
where
    S: futures_util::Stream<
            Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>,
        > + Unpin,
{
    while let Some(message) = ws.next().await {
        match message? {
            Message::Text(text) => {
                return serde_json::from_str(&text).context("invalid JSON-RPC message");
            }
            Message::Binary(bytes) => {
                return serde_json::from_slice(&bytes).context("invalid JSON-RPC bytes");
            }
            Message::Close(_) => bail!("WebSocket closed"),
            _ => {}
        }
    }
    bail!("WebSocket stream ended")
}

fn ensure_user_message(envelope: &RoutingEnvelope) -> Result<()> {
    if envelope.role == MessageRole::User {
        Ok(())
    } else {
        bail!(
            "{:?} backend accepts only user-message turns in this prototype",
            envelope.backend
        )
    }
}

fn is_codex_approval_or_input_request(method: &str) -> bool {
    method.contains("requestApproval")
        || method.contains("requestUserInput")
        || method.contains("elicitation/request")
        || method == "applyPatchApproval"
        || method == "execCommandApproval"
}

fn default_codex_model() -> String {
    "gpt-5.5".to_string()
}

fn default_codex_effort() -> String {
    "xhigh".to_string()
}

fn default_codex_approval_policy() -> String {
    "never".to_string()
}

fn default_codex_sandbox_policy() -> Value {
    json!({"type": "dangerFullAccess"})
}

fn default_transport_timeout() -> Duration {
    Duration::from_secs(30)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    struct FakeBackend;

    impl InjectionBackend for FakeBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Claude
        }

        fn inject<'a>(
            &'a self,
            envelope: RoutingEnvelope,
        ) -> BoxFuture<'a, Result<InjectionReceipt>> {
            Box::pin(async move {
                Ok(InjectionReceipt {
                    backend: BackendKind::Claude,
                    target: envelope.target,
                    correlation_id: envelope.correlation_id,
                    status: InjectionStatus::Completed,
                    session_id: envelope.session_id,
                    thread_id: envelope.thread_id,
                })
            })
        }
    }

    fn envelope() -> RoutingEnvelope {
        RoutingEnvelope {
            origin: "andy".to_string(),
            target: "nixos-cx".to_string(),
            backend: BackendKind::Codex,
            role: MessageRole::User,
            message: "status update".to_string(),
            cwd: Some("/home/andy/nixos".to_string()),
            session_id: None,
            thread_id: None,
            reply_to: Some("andy".to_string()),
            correlation_id: "corr-1".to_string(),
        }
    }

    #[tokio::test]
    async fn injection_backend_trait_dispatches_envelope() {
        let receipt = FakeBackend.inject(envelope()).await.unwrap();

        assert_eq!(receipt.backend, BackendKind::Claude);
        assert_eq!(receipt.target, "nixos-cx");
        assert_eq!(receipt.correlation_id, "corr-1");
        assert_eq!(receipt.status, InjectionStatus::Completed);
    }

    #[test]
    fn prompt_turn_backends_reject_non_user_roles() {
        let mut envelope = envelope();
        envelope.role = MessageRole::System;

        let error = ensure_user_message(&envelope).unwrap_err().to_string();
        assert!(error.contains("accepts only user-message turns"));
    }

    #[test]
    fn parses_config_v2_backends_and_targets() {
        let config: RoutingConfig = serde_yml::from_str(
            r#"
version: 2
backends:
  oc:
    type: opencode
    base_url: http://127.0.0.1:4096
  cx:
    type: codex
    url: ws://127.0.0.1:4107
agents:
  nixos-cx:
    backend: cx
    cwd: /home/andy/nixos
    thread_id: thread-1
"#,
        )
        .unwrap();

        let registry = TargetRegistry::new(config);
        let target = registry.resolve("nixos-cx").unwrap();
        assert_eq!(target.backend_name, "cx");
        assert_eq!(target.backend.kind(), BackendKind::Codex);
        assert_eq!(
            target.handle,
            TargetHandle::Codex {
                cwd: "/home/andy/nixos".to_string(),
                thread_id: Some("thread-1".to_string())
            }
        );
    }

    #[test]
    fn validates_config_v2_target_backends() {
        let config: RoutingConfig = serde_yml::from_str(
            r#"
version: 2
backends:
  oc:
    type: opencode
    base_url: http://127.0.0.1:4096
agents:
  bad:
    backend: missing
    cwd: /home/andy/nixos
"#,
        )
        .unwrap();

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("unknown backend missing"));
    }

    #[test]
    fn router_builds_envelope_with_reply_route() {
        let router = Router::from_yaml(
            r#"
version: 2
backends:
  cx:
    type: codex
    url: ws://127.0.0.1:4107
agents:
  nixos-cx:
    backend: cx
    cwd: /home/andy/nixos
    thread_id: thread-1
routes:
  default_reply_to: caller
"#,
        )
        .unwrap();

        let (envelope, backend) = router
            .envelope_for("nixos-cx", "status".to_string(), "andy".to_string())
            .unwrap();

        assert_eq!(backend.kind(), BackendKind::Codex);
        assert_eq!(envelope.target, "nixos-cx");
        assert_eq!(envelope.cwd.as_deref(), Some("/home/andy/nixos"));
        assert_eq!(envelope.thread_id.as_deref(), Some("thread-1"));
        assert_eq!(envelope.reply_to.as_deref(), Some("caller"));
        assert!(!envelope.correlation_id.is_empty());
    }

    #[test]
    fn routing_example_yaml_stays_parseable() {
        let config: RoutingConfig =
            serde_yml::from_str(include_str!("../routing.example.yaml")).unwrap();

        config.validate().unwrap();
        assert!(config.backends.contains_key("opencode"));
        assert!(config.backends.contains_key("codex"));
        assert!(config.agents.contains_key("nixos-cx"));
    }

    #[test]
    fn builds_opencode_prompt_body() {
        assert_eq!(
            OpenCodeBackend::prompt_body("hello"),
            json!({"parts": [{"type": "text", "text": "hello"}]})
        );
    }

    #[test]
    fn selects_newest_opencode_session_for_cwd() {
        let sessions = vec![
            OpenCodeSession {
                id: "old".to_string(),
                directory: Some("/home/andy/nixos".to_string()),
                updated_at: Some("2026-05-01T00:00:00Z".to_string()),
                time: None,
            },
            OpenCodeSession {
                id: "other".to_string(),
                directory: Some("/home/andy/vault".to_string()),
                updated_at: Some("2026-05-01T02:00:00Z".to_string()),
                time: None,
            },
            OpenCodeSession {
                id: "new".to_string(),
                directory: Some("/home/andy/nixos".to_string()),
                updated_at: Some("2026-05-01T03:00:00Z".to_string()),
                time: None,
            },
        ];

        assert_eq!(
            OpenCodeBackend::select_session(&sessions, "/home/andy/nixos")
                .map(|session| session.id.as_str()),
            Some("new")
        );
    }

    #[test]
    fn selects_newest_opencode_session_from_live_time_shape() {
        let sessions: Vec<OpenCodeSession> = serde_json::from_value(json!([
            {
                "id": "old",
                "directory": "/home/andy/nixos",
                "time": {"updated": 1777592772649u64}
            },
            {
                "id": "new",
                "directory": "/home/andy/nixos",
                "time": {"updated": 1777636176737u64}
            }
        ]))
        .unwrap();

        assert_eq!(
            OpenCodeBackend::select_session(&sessions, "/home/andy/nixos")
                .map(|session| session.id.as_str()),
            Some("new")
        );
    }

    #[tokio::test]
    async fn opencode_backend_posts_prompt_async_against_fake_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            assert!(request.starts_with("GET /session "));

            let body = serde_json::to_string(&json!([
                {
                    "id": "ses-old",
                    "directory": "/home/andy/nixos",
                    "time": {"updated": 1}
                },
                {
                    "id": "ses-new",
                    "directory": "/home/andy/nixos",
                    "time": {"updated": 2}
                }
            ]))
            .unwrap();
            write_http_response(&mut stream, 200, Some(&body)).await;

            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            assert!(request.starts_with("POST /session/ses-new/prompt_async "));
            assert!(request.contains(r#""text":"hello fake opencode""#));
            write_http_response(&mut stream, 204, None).await;
        });

        let backend = OpenCodeBackend::new(format!("http://{addr}"));
        let mut envelope = envelope();
        envelope.backend = BackendKind::OpenCode;
        envelope.target = "vault-oc".to_string();
        envelope.message = "hello fake opencode".to_string();
        envelope.cwd = Some("/home/andy/nixos".to_string());

        let receipt = backend.inject(envelope).await.unwrap();

        assert_eq!(receipt.backend, BackendKind::OpenCode);
        assert_eq!(receipt.status, InjectionStatus::Queued);
        assert_eq!(receipt.session_id.as_deref(), Some("ses-new"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn opencode_backend_times_out_when_session_endpoint_stalls() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut stream).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let mut backend = OpenCodeBackend::new(format!("http://{addr}"));
        backend.client = reqwest::Client::builder()
            .timeout(Duration::from_millis(20))
            .build()
            .unwrap();
        let mut envelope = envelope();
        envelope.backend = BackendKind::OpenCode;
        envelope.cwd = Some("/home/andy/nixos".to_string());

        let result = tokio::time::timeout(Duration::from_secs(1), backend.inject(envelope))
            .await
            .expect("OpenCode injection should return before outer test timeout");

        assert!(result.is_err());
        server.abort();
    }

    #[test]
    fn builds_codex_json_rpc_payloads() {
        let backend = CodexBackend::new("ws://127.0.0.1:4107");
        let request = backend.turn_start_request(3, "thread-1", &envelope());

        assert_eq!(request["method"], "turn/start");
        assert_eq!(request["params"]["threadId"], "thread-1");
        assert_eq!(request["params"]["cwd"], "/home/andy/nixos");
        assert_eq!(request["params"]["model"], "gpt-5.5");
        assert_eq!(request["params"]["effort"], "xhigh");
        assert_eq!(request["params"]["approvalPolicy"], "never");
        assert_eq!(
            request["params"]["sandboxPolicy"],
            json!({"type": "dangerFullAccess"})
        );
        assert_eq!(request["params"]["input"][0]["type"], "text");
        assert_eq!(request["params"]["input"][0]["text"], "status update");
        assert_eq!(request["params"]["input"][0]["text_elements"], json!([]));

        let list = CodexBackend::thread_list_request(2, "/home/andy/nixos");
        assert_eq!(list["params"]["sourceKinds"], json!(["cli"]));

        let resume = backend.thread_resume_request(3, "thread-1", "/home/andy/nixos");
        assert_eq!(resume["method"], "thread/resume");
        assert_eq!(resume["params"]["threadId"], "thread-1");
        assert_eq!(resume["params"]["cwd"], "/home/andy/nixos");
        assert_eq!(resume["params"]["excludeTurns"], true);

        let notification = CodexBackend::initialized_notification();
        assert_eq!(notification["method"], "initialized");
    }

    #[test]
    fn selects_active_cli_codex_thread() {
        let response = json!({
            "threads": [
                {"id": "background", "source": "api", "status": {"type": "active"}},
                {"id": "old-cli", "source": "cli", "status": {"type": "idle"}},
                {"id": "active-cli", "source": "cli", "status": {"type": "active"}}
            ]
        });

        assert_eq!(
            CodexBackend::select_thread(&response).as_deref(),
            Some("active-cli")
        );
    }

    #[test]
    fn selects_codex_thread_from_result_data_shape() {
        let response = json!({
            "data": [
                {
                    "id": "newest-cli",
                    "cwd": "/home/andy/nixos",
                    "source": "cli",
                    "status": {"type": "idle"},
                    "updatedAt": 1777636176
                }
            ],
            "nextCursor": null
        });

        assert_eq!(
            CodexBackend::select_thread(&response).as_deref(),
            Some("newest-cli")
        );
    }

    #[test]
    fn reads_codex_turn_ids_and_status() {
        let turn = json!({
            "turn": {
                "id": "turn-1",
                "status": "completed"
            }
        });
        let notification = json!({
            "method": "turn/completed",
            "params": {
                "threadId": "thread-1",
                "turn": {
                    "id": "turn-1",
                    "status": "completed"
                }
            }
        });

        assert_eq!(CodexBackend::turn_id(&turn).as_deref(), Some("turn-1"));
        assert_eq!(
            CodexBackend::turn_id(&notification).as_deref(),
            Some("turn-1")
        );
        assert_eq!(CodexBackend::turn_status(&notification), Some("completed"));
        assert_eq!(
            CodexBackend::notification_thread_id(&notification),
            Some("thread-1")
        );
    }

    #[tokio::test]
    async fn codex_backend_completes_turn_against_fake_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

            let request = next_ws_json(&mut ws).await.unwrap();
            assert_eq!(request["method"], "initialize");
            assert_eq!(request["id"], 1);
            send_ws_json(&mut ws, json!({"jsonrpc": "2.0", "id": 1, "result": {}})).await;

            let notification = next_ws_json(&mut ws).await.unwrap();
            assert_eq!(notification["method"], "initialized");

            let request = next_ws_json(&mut ws).await.unwrap();
            assert_eq!(request["method"], "thread/list");
            assert_eq!(request["id"], 2);
            assert_eq!(request["params"]["cwd"], "/home/andy/nixos");
            send_ws_json(
                &mut ws,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "result": {
                        "data": [
                            {
                                "id": "thread-1",
                                "cwd": "/home/andy/nixos",
                                "source": "cli",
                                "status": {"type": "active"}
                            }
                        ]
                    }
                }),
            )
            .await;

            let request = next_ws_json(&mut ws).await.unwrap();
            assert_eq!(request["method"], "thread/resume");
            assert_eq!(request["id"], 3);
            assert_eq!(request["params"]["threadId"], "thread-1");
            send_ws_json(&mut ws, json!({"jsonrpc": "2.0", "id": 3, "result": {}})).await;

            let request = next_ws_json(&mut ws).await.unwrap();
            assert_eq!(request["method"], "turn/start");
            assert_eq!(request["id"], 4);
            assert_eq!(request["params"]["threadId"], "thread-1");
            assert_eq!(request["params"]["input"][0]["text"], "status update");
            send_ws_json(
                &mut ws,
                json!({
                    "jsonrpc": "2.0",
                    "id": 4,
                    "result": {
                        "turn": {
                            "id": "turn-1",
                            "status": "inProgress"
                        }
                    }
                }),
            )
            .await;
            send_ws_json(
                &mut ws,
                json!({
                    "jsonrpc": "2.0",
                    "method": "turn/completed",
                    "params": {
                        "threadId": "thread-1",
                        "turn": {
                            "id": "turn-1",
                            "status": "completed"
                        }
                    }
                }),
            )
            .await;
        });

        let mut backend = CodexBackend::new(format!("ws://{addr}"));
        backend.completion_timeout = Duration::from_secs(5);
        let receipt = backend.inject(envelope()).await.unwrap();

        assert_eq!(receipt.backend, BackendKind::Codex);
        assert_eq!(receipt.status, InjectionStatus::Completed);
        assert_eq!(receipt.thread_id.as_deref(), Some("thread-1"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn codex_backend_times_out_waiting_for_rpc_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let request = next_ws_json(&mut ws).await.unwrap();
            assert_eq!(request["method"], "initialize");
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let mut backend = CodexBackend::new(format!("ws://{addr}"));
        backend.transport_timeout = Duration::from_millis(20);
        let error = backend.inject(envelope()).await.unwrap_err().to_string();

        assert!(error.contains("timed out waiting for JSON-RPC response id 1"));
        server.abort();
    }

    #[test]
    fn falls_back_to_newest_cli_codex_thread() {
        let response = json!({
            "result": {
                "threads": [
                    {"id": "newest-cli", "source": "cli", "status": {"type": "idle"}},
                    {"id": "other", "source": "api", "status": {"type": "active"}}
                ]
            }
        });

        assert_eq!(
            CodexBackend::select_thread(&response).as_deref(),
            Some("newest-cli")
        );
    }

    #[test]
    fn models_capabilities_and_lossy_zellij_handles() {
        let opencode = BackendCapabilities::for_kind(BackendKind::OpenCode);
        assert!(!opencode.preserves_role);
        assert!(!opencode.has_completion_readback);
        assert!(!opencode.is_lossy);

        let codex = BackendCapabilities::for_kind(BackendKind::Codex);
        assert!(!codex.preserves_role);
        assert!(codex.has_completion_readback);
        assert!(!codex.is_lossy);

        let zellij = BackendCapabilities::for_kind(BackendKind::Zellij);
        assert!(!zellij.preserves_role);
        assert!(!zellij.has_completion_readback);
        assert!(zellij.is_lossy);

        let handle = TargetHandle::Zellij {
            session: Some("main".to_string()),
            pane_id: Some("terminal_0".to_string()),
            submit_strategy: ZellijSubmitStrategy::TextThenEnter,
        };
        assert_eq!(
            serde_json::to_value(handle).unwrap(),
            json!({
                "type": "zellij",
                "session": "main",
                "pane_id": "terminal_0",
                "submit_strategy": "text_then_enter"
            })
        );
    }

    async fn read_http_request(stream: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0; 1024];
        loop {
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "HTTP request ended before headers were complete");
            buf.extend_from_slice(&chunk[..n]);

            let Some(header_end) = buf.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&buf[..header_end]);
            let content_len = headers
                .lines()
                .find_map(|line| {
                    let lower = line.to_ascii_lowercase();
                    lower
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if buf.len() >= header_end + 4 + content_len {
                return String::from_utf8(buf).unwrap();
            }
        }
    }

    async fn write_http_response(stream: &mut TcpStream, status: u16, body: Option<&str>) {
        let reason = match status {
            200 => "OK",
            204 => "No Content",
            _ => "OK",
        };
        let body = body.unwrap_or("");
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }

    async fn send_ws_json<S>(ws: &mut S, value: Value)
    where
        S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    {
        ws.send(Message::Text(value.to_string().into()))
            .await
            .unwrap();
    }
}
