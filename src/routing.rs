#![allow(dead_code)]

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt, future::BoxFuture};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashMap, fs, path::Path, time::Duration};
use tokio::process::Command;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;

const METASTACK_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    #[serde(rename = "opencode")]
    OpenCode,
    Codex,
    Claude,
    Zellij,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    #[default]
    User,
    // Reserved for future backend-specific role-aware delivery. The prototype
    // public `send` command always creates user-message turns.
    Agent,
    System,
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
    pub channel: Option<String>,
    #[serde(default)]
    pub member: Option<String>,
    #[serde(default)]
    pub reply_to: Option<String>,
    pub correlation_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SendStatus {
    Submitted,
    Accepted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SendReceipt {
    pub backend: BackendKind,
    pub target: String,
    pub correlation_id: String,
    pub status: SendStatus,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
        member: String,
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
            Self::Claude { channel, member } => {
                envelope.channel = channel.clone();
                envelope.member = Some(member.clone());
            }
            Self::Zellij { .. } => {}
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
    pub has_delivery_receipt: bool,
    pub is_lossy: bool,
}

impl BackendCapabilities {
    pub fn for_kind(backend: BackendKind) -> Self {
        match backend {
            BackendKind::OpenCode => Self {
                backend,
                preserves_role: false,
                has_delivery_receipt: true,
                is_lossy: false,
            },
            BackendKind::Codex => Self {
                backend,
                preserves_role: false,
                has_delivery_receipt: true,
                is_lossy: false,
            },
            BackendKind::Claude => Self {
                backend,
                preserves_role: false,
                has_delivery_receipt: false,
                is_lossy: false,
            },
            BackendKind::Zellij => Self {
                backend,
                preserves_role: false,
                has_delivery_receipt: false,
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

pub trait SendBackend: Send + Sync {
    fn kind(&self) -> BackendKind;
    fn send<'a>(&'a self, envelope: RoutingEnvelope) -> BoxFuture<'a, Result<SendReceipt>>;
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingConfig {
    pub version: u8,
    #[serde(default)]
    pub aliases: HashMap<String, String>,
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

        for (name, backend) in &self.backends {
            if name.trim().is_empty() {
                bail!("routing backend name cannot be empty");
            }
            backend.validate(name)?;
        }

        for (role, target) in &self.aliases {
            if !is_supported_alias_role(role) {
                bail!("routing alias {role} is not supported; supported aliases: main, observer");
            }
            if target.trim().is_empty() {
                bail!("routing alias {role} cannot point to a blank target");
            }
            if self.aliases.contains_key(target) {
                bail!("routing alias {role} cannot point to alias {target}");
            }
            if !self.agents.contains_key(target) {
                bail!("routing alias {role} references unknown target {target}");
            }
        }

        for (name, agent) in &self.agents {
            if name.trim().is_empty() {
                bail!("routing agent name cannot be empty");
            }
            let backend = self.backends.get(&agent.backend).with_context(|| {
                format!("target {name} references unknown backend {}", agent.backend)
            })?;
            match backend.kind() {
                BackendKind::OpenCode => {
                    if agent.cwd.as_deref().is_none_or(|cwd| cwd.trim().is_empty()) {
                        bail!("routing target {name} must define cwd");
                    }
                    if agent.thread_id.is_some() {
                        bail!("routing target {name} cannot define thread_id for OpenCode backend");
                    }
                    if agent.member.is_some() {
                        bail!("routing target {name} cannot define member for OpenCode backend");
                    }
                }
                BackendKind::Codex => {
                    if agent.cwd.as_deref().is_none_or(|cwd| cwd.trim().is_empty()) {
                        bail!("routing target {name} must define cwd");
                    }
                    if agent.session_id.is_some() {
                        bail!("routing target {name} cannot define session_id for Codex backend");
                    }
                    if agent.member.is_some() {
                        bail!("routing target {name} cannot define member for Codex backend");
                    }
                }
                BackendKind::Claude => {
                    if agent
                        .member
                        .as_deref()
                        .is_none_or(|member| ClaudeBackend::normalize_member(member).is_empty())
                    {
                        bail!("routing target {name} must define Huddle member");
                    }
                    if agent.cwd.is_some() {
                        bail!("routing target {name} cannot define cwd for Claude backend");
                    }
                    if agent.session_id.is_some() {
                        bail!("routing target {name} cannot define session_id for Claude backend");
                    }
                    if agent.thread_id.is_some() {
                        bail!("routing target {name} cannot define thread_id for Claude backend");
                    }
                }
                BackendKind::Zellij => {
                    if agent.cwd.is_some() {
                        bail!("routing target {name} cannot define cwd for Zellij backend");
                    }
                    if agent.session_id.is_some() {
                        bail!("routing target {name} cannot define session_id for Zellij backend");
                    }
                    if agent.thread_id.is_some() {
                        bail!("routing target {name} cannot define thread_id for Zellij backend");
                    }
                    if agent.member.is_some() {
                        bail!("routing target {name} cannot define member for Zellij backend");
                    }
                }
            }
        }

        Ok(())
    }
}

fn is_supported_alias_role(role: &str) -> bool {
    matches!(role, "main" | "observer")
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    #[serde(default)]
    pub default_reply_to: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpec {
    pub backend: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub member: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum BackendConfig {
    #[serde(rename = "opencode")]
    OpenCode { base_url: String },
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
        #[serde(default)]
        channel: Option<String>,
        #[serde(default = "default_huddle_command")]
        command: String,
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

    fn validate(&self, name: &str) -> Result<()> {
        match self {
            Self::OpenCode { base_url } => {
                validate_nonblank(name, "base_url", base_url)?;
                let url = validate_url_scheme(name, "base_url", base_url, &["http", "https"])?;
                validate_no_query_or_fragment(name, "base_url", &url)?;
            }
            Self::Codex {
                url,
                model,
                effort,
                approval_policy,
                ..
            } => {
                validate_nonblank(name, "url", url)?;
                validate_url_scheme(name, "url", url, &["ws", "wss"])?;
                validate_nonblank(name, "model", model)?;
                validate_nonblank(name, "effort", effort)?;
                validate_nonblank(name, "approval_policy", approval_policy)?;
            }
            Self::Claude { command, .. } => {
                validate_nonblank(name, "command", command)?;
            }
            Self::Zellij { mcp_binary, .. } => {
                validate_nonblank(name, "mcp_binary", mcp_binary)?;
            }
        }
        Ok(())
    }
}

fn validate_nonblank(backend: &str, field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("routing backend {backend} field {field} cannot be blank");
    }
    Ok(())
}

fn validate_url_scheme(
    backend: &str,
    field: &str,
    value: &str,
    schemes: &[&str],
) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value)
        .with_context(|| format!("routing backend {backend} field {field} must be a URL"))?;
    if !schemes.contains(&url.scheme()) {
        bail!(
            "routing backend {backend} field {field} must use one of: {}",
            schemes.join(", ")
        );
    }
    Ok(url)
}

fn validate_no_query_or_fragment(backend: &str, field: &str, url: &reqwest::Url) -> Result<()> {
    if url.query().is_some() || url.fragment().is_some() {
        bail!("routing backend {backend} field {field} cannot include query or fragment");
    }
    Ok(())
}

fn is_mention_body(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
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
        let resolved_name = self
            .config
            .aliases
            .get(target)
            .map(String::as_str)
            .unwrap_or(target);
        let agent = self
            .config
            .agents
            .get(resolved_name)
            .with_context(|| format!("unknown routing target {target}"))?;
        let backend = self.config.backends.get(&agent.backend).with_context(|| {
            format!(
                "target {resolved_name} references unknown backend {}",
                agent.backend
            )
        })?;

        let handle =
            match backend {
                BackendConfig::OpenCode { .. } => TargetHandle::OpenCode {
                    cwd: agent.cwd.clone().with_context(|| {
                        format!("routing target {resolved_name} must define cwd")
                    })?,
                    session_id: agent.session_id.clone(),
                },
                BackendConfig::Codex { .. } => TargetHandle::Codex {
                    cwd: agent.cwd.clone().with_context(|| {
                        format!("routing target {resolved_name} must define cwd")
                    })?,
                    thread_id: agent.thread_id.clone(),
                },
                BackendConfig::Claude { channel, .. } => TargetHandle::Claude {
                    channel: channel.clone(),
                    member: agent
                        .member
                        .as_deref()
                        .map(ClaudeBackend::normalize_member)
                        .with_context(|| {
                            format!("routing target {resolved_name} must define Huddle member")
                        })?,
                },
                BackendConfig::Zellij { session, .. } => TargetHandle::Zellij {
                    session: session.clone(),
                    pane_id: None,
                    submit_strategy: ZellijSubmitStrategy::default(),
                },
            };

        Ok(ResolvedTarget {
            name: resolved_name.to_string(),
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
            target: resolved.name.clone(),
            backend: resolved.backend.kind(),
            role: MessageRole::User,
            message,
            cwd: None,
            session_id: None,
            thread_id: None,
            channel: None,
            member: None,
            reply_to: self.config.routes.default_reply_to.clone(),
            correlation_id: Uuid::new_v4().simple().to_string(),
        };
        resolved.handle.apply_to_envelope(&mut envelope);

        Ok((envelope, resolved.backend))
    }

    pub async fn send_text(
        &self,
        target: &str,
        message: String,
        origin: String,
    ) -> Result<SendReceipt> {
        let (envelope, backend) = self.envelope_for(target, message, origin)?;
        Self::dispatch(backend, envelope).await
    }

    async fn dispatch(backend: BackendConfig, envelope: RoutingEnvelope) -> Result<SendReceipt> {
        match &backend {
            BackendConfig::OpenCode { base_url } => {
                OpenCodeBackend::new(base_url).send(envelope).await
            }
            BackendConfig::Codex { .. } => {
                CodexBackend::from_config(&backend)?.send(envelope).await
            }
            BackendConfig::Claude { .. } => {
                ClaudeBackend::from_config(&backend)?.send(envelope).await
            }
            BackendConfig::Zellij { .. } => {
                bail!("zellij fallback send is not implemented in the structured prototype")
            }
        }
    }
}

pub async fn send_from_config_path(
    path: &Path,
    target: &str,
    message: String,
    origin: String,
) -> Result<SendReceipt> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    Router::from_yaml(&text)?
        .send_text(target, message, origin)
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
        format!(
            "{}/session/{}/prompt_async",
            self.base_url,
            percent_encode_path_segment(session_id)
        )
    }

    fn prompt_body(message: &str) -> Value {
        json!({"parts": [{"type": "text", "text": message}]})
    }

    pub fn select_session<'a>(
        sessions: &'a [OpenCodeSession],
        cwd: &str,
    ) -> Result<Option<&'a OpenCodeSession>> {
        let matching = sessions
            .iter()
            .filter(|session| session.directory.as_deref() == Some(cwd))
            .collect::<Vec<_>>();
        let top_level = matching
            .iter()
            .copied()
            .filter(|session| session.parent_id.is_none())
            .collect::<Vec<_>>();
        let candidates = if top_level.is_empty() {
            matching
        } else {
            top_level
        };

        match candidates.as_slice() {
            [] => Ok(None),
            [session] => Ok(Some(*session)),
            _ => bail!(
                "multiple OpenCode sessions found for cwd {cwd}; configure session_id explicitly"
            ),
        }
    }

    async fn list_sessions_for_cwd(&self, cwd: &str) -> Result<Vec<OpenCodeSession>> {
        self.client
            .get(self.session_url())
            .query(&[("directory", cwd)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("failed to parse OpenCode session list")
    }

    async fn discover_session(&self, cwd: &str) -> Result<String> {
        let sessions = self.list_sessions_for_cwd(cwd).await?;
        Self::select_session(&sessions, cwd)?
            .map(|session| session.id.clone())
            .with_context(|| format!("no OpenCode session found for cwd {cwd}"))
    }

    async fn validate_session_id(&self, cwd: &str, session_id: &str) -> Result<()> {
        let sessions = self.list_sessions_for_cwd(cwd).await?;
        sessions
            .iter()
            .any(|session| session.id == session_id && session.directory.as_deref() == Some(cwd))
            .then_some(())
            .with_context(|| {
                format!("configured OpenCode session_id {session_id} not found for cwd {cwd}")
            })
    }
}

impl SendBackend for OpenCodeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::OpenCode
    }

    fn send<'a>(&'a self, envelope: RoutingEnvelope) -> BoxFuture<'a, Result<SendReceipt>> {
        Box::pin(async move {
            ensure_user_message(&envelope)?;
            let cwd = envelope
                .cwd
                .as_deref()
                .context("OpenCode send requires cwd")?;
            let session_id = match envelope.session_id.clone() {
                Some(session_id) => {
                    self.validate_session_id(cwd, &session_id).await?;
                    session_id
                }
                None => self.discover_session(cwd).await?,
            };

            self.client
                .post(self.prompt_async_url(&session_id))
                .json(&Self::prompt_body(&envelope.message))
                .send()
                .await?
                .error_for_status()?;

            Ok(SendReceipt {
                backend: BackendKind::OpenCode,
                target: envelope.target,
                correlation_id: envelope.correlation_id,
                status: SendStatus::Accepted,
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
    #[serde(default, rename = "parentID")]
    parent_id: Option<String>,
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

#[derive(Clone, Debug)]
pub struct ClaudeBackend {
    command: String,
    transport_timeout: Duration,
}

impl ClaudeBackend {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            transport_timeout: default_transport_timeout(),
        }
    }

    pub fn from_config(config: &BackendConfig) -> Result<Self> {
        let BackendConfig::Claude { command, .. } = config else {
            bail!("expected Claude backend config");
        };

        Ok(Self {
            command: command.clone(),
            transport_timeout: default_transport_timeout(),
        })
    }

    fn command_args(member: &str, message: &str) -> Vec<String> {
        vec![
            "send".to_string(),
            "--to".to_string(),
            Self::normalize_member(member),
            Self::message_arg(message),
        ]
    }

    fn sessions_args() -> Vec<String> {
        vec!["sessions".to_string()]
    }

    fn normalize_member(member: &str) -> String {
        member.trim().trim_start_matches('@').to_ascii_lowercase()
    }

    fn message_arg(message: &str) -> String {
        if message.starts_with('-') {
            format!(" {message}")
        } else {
            message.to_string()
        }
    }

    fn member_is_connected(sessions: &str, member: &str) -> bool {
        let member = Self::normalize_member(member);
        sessions
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .map(Self::normalize_member)
            .any(|connected| connected == member)
    }

    fn ensure_single_target_message(message: &str, member: &str) -> Result<()> {
        let member = Self::normalize_member(member);
        let escaped = Self::inline_mentions(message)
            .into_iter()
            .find(|mention| mention != &member);

        if let Some(mention) = escaped {
            bail!(
                "Huddle message contains inline @{mention}; structured send only allows the configured target @{member}"
            );
        }
        Ok(())
    }

    fn inline_mentions(message: &str) -> Vec<String> {
        let bytes = message.as_bytes();
        let mut mentions = Vec::new();
        let mut i = 0;

        while i < bytes.len() {
            if bytes[i] != b'@' {
                i += 1;
                continue;
            }

            if i > 0 && is_mention_body(bytes[i - 1]) {
                i += 1;
                continue;
            }

            let mut end = i + 1;
            while end < bytes.len() && is_mention_body(bytes[end]) {
                end += 1;
            }

            if end > i + 1 {
                mentions.push(String::from_utf8_lossy(&bytes[i + 1..end]).to_ascii_lowercase());
            }
            i = end.max(i + 1);
        }

        mentions
    }

    async fn output(&self, args: Vec<String>, action: &str) -> Result<std::process::Output> {
        let mut command = Command::new(&self.command);
        command.args(args);
        command.kill_on_drop(true);
        let output = tokio::time::timeout(self.transport_timeout, command.output())
            .await
            .with_context(|| format!("timed out waiting for huddle {action}"))?
            .with_context(|| format!("failed to run {}", self.command))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let detail = if stderr.is_empty() { stdout } else { stderr };
            bail!(
                "huddle {action} failed with status {}: {}",
                output.status,
                detail
            );
        }

        Ok(output)
    }
}

impl SendBackend for ClaudeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Claude
    }

    fn send<'a>(&'a self, envelope: RoutingEnvelope) -> BoxFuture<'a, Result<SendReceipt>> {
        Box::pin(async move {
            ensure_user_message(&envelope)?;
            let member = envelope
                .member
                .as_deref()
                .context("Claude Huddle send requires member")?;

            let sessions = self.output(Self::sessions_args(), "sessions").await?;
            let sessions_text = String::from_utf8_lossy(&sessions.stdout);
            if !Self::member_is_connected(&sessions_text, member) {
                bail!("huddle target {member} unavailable (no_target)");
            }
            Self::ensure_single_target_message(&envelope.message, member)?;
            self.output(Self::command_args(member, &envelope.message), "send")
                .await?;

            Ok(SendReceipt {
                backend: BackendKind::Claude,
                target: envelope.target,
                correlation_id: envelope.correlation_id,
                status: SendStatus::Submitted,
                session_id: envelope.session_id,
                thread_id: envelope.thread_id,
            })
        })
    }
}

#[derive(Clone)]
pub struct CodexBackend {
    url: String,
    model: String,
    effort: String,
    approval_policy: String,
    sandbox_policy: Value,
    transport_timeout: Duration,
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
                    "version": METASTACK_VERSION
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
                "limit": 100,
                "sortKey": "updated_at",
                "sortDirection": "desc",
                "archived": false,
                "sourceKinds": ["cli", "vscode"],
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
        Self::select_thread_ref(value, None)
            .ok()
            .flatten()
            .map(|thread| thread.id)
    }

    fn select_thread_for_cwd(value: &Value, cwd: &str) -> Option<String> {
        Self::select_thread_ref(value, Some(cwd))
            .ok()
            .flatten()
            .map(|thread| thread.id)
    }

    fn select_thread_ref(value: &Value, cwd: Option<&str>) -> Result<Option<CodexThreadRef>> {
        let Some(threads) = Self::thread_items(value) else {
            return Ok(None);
        };

        let matching_threads = Self::threads_matching_cwd(threads.as_slice(), cwd);
        let active_routable = matching_threads
            .iter()
            .copied()
            .filter(|thread| {
                Self::thread_source_is_routable(thread)
                    && Self::thread_status(thread) == Some("active")
            })
            .collect::<Vec<_>>();
        if !active_routable.is_empty() {
            return Self::unique_implicit_thread(active_routable, cwd);
        }

        let idle_routable = matching_threads
            .iter()
            .copied()
            .filter(|thread| {
                Self::thread_source_is_routable(thread)
                    && Self::thread_status(thread) == Some("idle")
            })
            .collect::<Vec<_>>();
        Self::unique_implicit_thread(idle_routable, cwd)
    }

    fn unique_implicit_thread(
        candidates: Vec<&Value>,
        cwd: Option<&str>,
    ) -> Result<Option<CodexThreadRef>> {
        match candidates.as_slice() {
            [] => Ok(None),
            [thread] => {
                let id = Self::thread_id(thread).context("Codex routable thread missing id")?;
                Ok(Some(Self::thread_ref(thread, id)))
            }
            _ => match cwd {
                Some(cwd) => bail!(
                    "multiple live Codex routable threads found for cwd {cwd}; configure thread_id explicitly"
                ),
                None => {
                    bail!(
                        "multiple live Codex routable threads found; configure thread_id explicitly"
                    )
                }
            },
        }
    }

    fn select_thread_ref_by_id(
        value: &Value,
        cwd: Option<&str>,
        thread_id: &str,
    ) -> Option<CodexThreadRef> {
        let cwd = cwd?;
        let threads = Self::thread_items(value)?;
        threads
            .iter()
            .find(|thread| {
                Self::thread_id(thread) == Some(thread_id)
                    && Self::thread_cwd(thread) == Some(cwd)
                    && Self::thread_source_is_routable(thread)
            })
            .map(|thread| Self::thread_ref(thread, thread_id))
    }

    fn thread_items(value: &Value) -> Option<&Vec<Value>> {
        value
            .get("data")
            .or_else(|| value.get("threads"))
            .or_else(|| value.get("result").and_then(|result| result.get("data")))
            .or_else(|| value.get("result").and_then(|result| result.get("threads")))
            .and_then(Value::as_array)
            .or_else(|| value.get("result").and_then(Value::as_array))
    }

    fn threads_matching_cwd<'a>(threads: &'a [Value], cwd: Option<&str>) -> Vec<&'a Value> {
        let Some(cwd) = cwd else {
            return threads.iter().collect();
        };
        threads
            .iter()
            .filter(|thread| Self::thread_cwd(thread) == Some(cwd))
            .collect::<Vec<_>>()
    }

    fn thread_cwd(thread: &Value) -> Option<&str> {
        thread
            .get("cwd")
            .or_else(|| thread.get("directory"))
            .and_then(Value::as_str)
    }

    fn thread_id(thread: &Value) -> Option<&str> {
        thread
            .get("id")
            .or_else(|| thread.get("threadId"))
            .and_then(Value::as_str)
    }

    fn thread_ref(thread: &Value, id: &str) -> CodexThreadRef {
        CodexThreadRef {
            id: id.to_string(),
            status: Self::thread_status(thread).map(str::to_string),
        }
    }

    fn validate_turn_start_response(value: &Value) -> Result<()> {
        let turn = value
            .get("turn")
            .or_else(|| value.get("data").and_then(|data| data.get("turn")))
            .context("Codex turn/start response missing turn")?;
        let turn_id = turn
            .get("id")
            .and_then(Value::as_str)
            .context("Codex turn/start response missing turn id")?;
        let status = turn
            .get("status")
            .and_then(Value::as_str)
            .context("Codex turn/start response missing turn status")?;

        match status {
            "completed" | "inProgress" | "queued" | "pending" => Ok(()),
            other => bail!("Codex turn {turn_id} was not accepted: status {other}"),
        }
    }

    fn thread_source_is_routable(thread: &Value) -> bool {
        matches!(
            thread.get("source").and_then(Value::as_str),
            Some("cli" | "vscode")
        ) || matches!(
            thread.get("sourceKind").and_then(Value::as_str),
            Some("cli" | "vscode")
        )
    }

    fn thread_status(thread: &Value) -> Option<&str> {
        thread
            .get("status")
            .and_then(|status| status.get("type").or(Some(status)))
            .and_then(Value::as_str)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CodexThreadRef {
    id: String,
    status: Option<String>,
}

impl SendBackend for CodexBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Codex
    }

    fn send<'a>(&'a self, envelope: RoutingEnvelope) -> BoxFuture<'a, Result<SendReceipt>> {
        Box::pin(async move {
            ensure_user_message(&envelope)?;
            let cwd = envelope.cwd.as_deref().context("Codex send requires cwd")?;
            let (mut ws, _) =
                tokio::time::timeout(self.transport_timeout, connect_async(&self.url))
                    .await
                    .context("timed out connecting to Codex app-server")??;

            send_ws_json(&mut ws, Self::initialize_request(1), self.transport_timeout).await?;
            wait_for_response(&mut ws, 1, self.transport_timeout).await?;
            send_ws_json(
                &mut ws,
                Self::initialized_notification(),
                self.transport_timeout,
            )
            .await?;

            send_ws_json(
                &mut ws,
                Self::thread_list_request(2, cwd),
                self.transport_timeout,
            )
            .await?;
            let response = wait_for_response(&mut ws, 2, self.transport_timeout).await?;
            let thread = if let Some(thread_id) = envelope.thread_id.clone() {
                Self::select_thread_ref_by_id(&response, Some(cwd), &thread_id).with_context(
                    || format!("configured Codex thread_id {thread_id} not found for cwd {cwd}"),
                )?
            } else {
                Self::select_thread_ref(&response, Some(cwd))?
                    .with_context(|| format!("no Codex routable thread found for cwd {cwd}"))?
            };
            let thread_id = thread.id;

            let mut next_id = 3;
            send_ws_json(
                &mut ws,
                self.thread_resume_request(next_id, &thread_id, cwd),
                self.transport_timeout,
            )
            .await?;
            wait_for_response(&mut ws, next_id, self.transport_timeout).await?;
            next_id += 1;

            send_ws_json(
                &mut ws,
                self.turn_start_request(next_id, &thread_id, &envelope),
                self.transport_timeout,
            )
            .await?;
            let response = wait_for_response(&mut ws, next_id, self.transport_timeout).await?;
            Self::validate_turn_start_response(&response)?;

            Ok(SendReceipt {
                backend: BackendKind::Codex,
                target: envelope.target,
                correlation_id: envelope.correlation_id,
                status: SendStatus::Accepted,
                session_id: envelope.session_id,
                thread_id: Some(thread_id),
            })
        })
    }
}

async fn send_ws_json<S>(ws: &mut S, value: Value, duration: Duration) -> Result<()>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    tokio::time::timeout(duration, ws.send(Message::Text(value.to_string().into())))
        .await
        .context("timed out writing JSON-RPC message")??;
    Ok(())
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
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = value.get("error") {
                    bail!("JSON-RPC error for id {id}: {error}");
                }
                return value
                    .get("result")
                    .cloned()
                    .with_context(|| format!("malformed JSON-RPC response for id {id}"));
            }
            if value.get("id").is_none_or(Value::is_null)
                && let Some(error) = value.get("error")
            {
                bail!("JSON-RPC error while waiting for id {id}: {error}");
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

fn default_codex_model() -> String {
    "gpt-5.5".to_string()
}

fn default_codex_effort() -> String {
    "xhigh".to_string()
}

fn default_huddle_command() -> String {
    "huddle".to_string()
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

fn percent_encode_path_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        task::{Context as TaskContext, Poll},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    struct FakeBackend;

    impl SendBackend for FakeBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Claude
        }

        fn send<'a>(&'a self, envelope: RoutingEnvelope) -> BoxFuture<'a, Result<SendReceipt>> {
            Box::pin(async move {
                Ok(SendReceipt {
                    backend: BackendKind::Claude,
                    target: envelope.target,
                    correlation_id: envelope.correlation_id,
                    status: SendStatus::Accepted,
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
            channel: None,
            member: None,
            reply_to: Some("andy".to_string()),
            correlation_id: "corr-1".to_string(),
        }
    }

    struct PendingFlushSink;

    impl futures_util::Sink<Message> for PendingFlushSink {
        type Error = tokio_tungstenite::tungstenite::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, _item: Message) -> Result<(), Self::Error> {
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn send_backend_trait_dispatches_envelope() {
        let receipt = FakeBackend.send(envelope()).await.unwrap();

        assert_eq!(receipt.backend, BackendKind::Claude);
        assert_eq!(receipt.target, "nixos-cx");
        assert_eq!(receipt.correlation_id, "corr-1");
        assert_eq!(receipt.status, SendStatus::Accepted);
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
    fn target_registry_resolves_supported_aliases_before_literal_targets() {
        let router = Router::from_yaml(
            r#"
version: 2
aliases:
  main: nixos-cx
  observer: andy-coh
backends:
  cx:
    type: codex
    url: ws://127.0.0.1:4107
  huddle:
    type: claude
agents:
  main:
    backend: huddle
    member: wrong-literal-target
  nixos-cx:
    backend: cx
    cwd: /home/andy/nixos
  andy-coh:
    backend: huddle
    member: andy
"#,
        )
        .unwrap();

        let (envelope, backend) = router
            .envelope_for("main", "status".to_string(), "andy".to_string())
            .unwrap();

        assert_eq!(backend.kind(), BackendKind::Codex);
        assert_eq!(envelope.target, "nixos-cx");
        assert_eq!(envelope.cwd.as_deref(), Some("/home/andy/nixos"));

        let (observer_envelope, observer_backend) = router
            .envelope_for("observer", "status".to_string(), "andy".to_string())
            .unwrap();

        assert_eq!(observer_backend.kind(), BackendKind::Claude);
        assert_eq!(observer_envelope.target, "andy-coh");
        assert_eq!(observer_envelope.member.as_deref(), Some("andy"));
    }

    #[test]
    fn literal_targets_still_resolve_when_no_alias_matches() {
        let router = Router::from_yaml(
            r#"
version: 2
aliases:
  main: andy-cx
backends:
  cx:
    type: codex
    url: ws://127.0.0.1:4107
agents:
  andy-cx:
    backend: cx
    cwd: /home/andy
  nixos-cx:
    backend: cx
    cwd: /home/andy/nixos
"#,
        )
        .unwrap();

        let (envelope, backend) = router
            .envelope_for("nixos-cx", "status".to_string(), "andy".to_string())
            .unwrap();

        assert_eq!(backend.kind(), BackendKind::Codex);
        assert_eq!(envelope.target, "nixos-cx");
        assert_eq!(envelope.cwd.as_deref(), Some("/home/andy/nixos"));
    }

    #[test]
    fn validates_alias_roles_and_targets() {
        let unsupported_role = r#"
version: 2
aliases:
  reviewer: andy-coh
backends:
  huddle:
    type: claude
agents:
  andy-coh:
    backend: huddle
    member: andy
"#;
        let error = Router::from_yaml(unsupported_role).unwrap_err().to_string();
        assert!(error.contains("supported aliases: main, observer"));

        let unknown_target = r#"
version: 2
aliases:
  observer: missing-target
backends:
  huddle:
    type: claude
agents:
  andy-coh:
    backend: huddle
    member: andy
"#;
        let error = Router::from_yaml(unknown_target).unwrap_err().to_string();
        assert!(error.contains("references unknown target missing-target"));

        let alias_to_alias = r#"
version: 2
aliases:
  main: andy-cx
  observer: main
backends:
  cx:
    type: codex
    url: ws://127.0.0.1:4107
agents:
  andy-cx:
    backend: cx
    cwd: /home/andy
"#;
        let error = Router::from_yaml(alias_to_alias).unwrap_err().to_string();
        assert!(error.contains("cannot point to alias main"));
    }

    #[test]
    fn parses_claude_huddle_targets_without_cwd() {
        let config: RoutingConfig = serde_yml::from_str(
            r#"
version: 2
backends:
  huddle:
    type: claude
    channel: huddle
    command: huddle
agents:
  andy-coh:
    backend: huddle
    member: andy-coh
"#,
        )
        .unwrap();

        config.validate().unwrap();
        let registry = TargetRegistry::new(config);
        let target = registry.resolve("andy-coh").unwrap();
        assert_eq!(target.backend.kind(), BackendKind::Claude);
        assert_eq!(
            target.handle,
            TargetHandle::Claude {
                channel: Some("huddle".to_string()),
                member: "andy-coh".to_string()
            }
        );
    }

    #[test]
    fn validates_backend_specific_agent_fields() {
        let opencode_with_thread_id = r#"
version: 2
backends:
  oc:
    type: opencode
    base_url: http://127.0.0.1:4096
agents:
  local-oc:
    backend: oc
    cwd: /home/andy/vault
    thread_id: thread-1
"#;
        let error = Router::from_yaml(opencode_with_thread_id)
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot define thread_id for OpenCode backend"));

        let codex_with_session_id = r#"
version: 2
backends:
  cx:
    type: codex
    url: ws://127.0.0.1:4107
agents:
  local-cx:
    backend: cx
    cwd: /home/andy/nixos
    session_id: ses-1
"#;
        let error = Router::from_yaml(codex_with_session_id)
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot define session_id for Codex backend"));

        let claude_with_cwd = r#"
version: 2
backends:
  huddle:
    type: claude
agents:
  local-claude:
    backend: huddle
    member: andy
    cwd: /home/andy
"#;
        let error = Router::from_yaml(claude_with_cwd).unwrap_err().to_string();
        assert!(error.contains("cannot define cwd for Claude backend"));
    }

    #[test]
    fn rejects_unknown_routing_config_fields() {
        let unknown_top_level = r#"
version: 2
unexpected: true
backends:
  cx:
    type: codex
    url: ws://127.0.0.1:4107
agents:
  local-cx:
    backend: cx
    cwd: /home/andy/nixos
"#;
        Router::from_yaml(unknown_top_level).unwrap_err();

        let unknown_route_field = r#"
version: 2
backends:
  cx:
    type: codex
    url: ws://127.0.0.1:4107
agents:
  local-cx:
    backend: cx
    cwd: /home/andy/nixos
routes:
  reply_to: caller
"#;
        Router::from_yaml(unknown_route_field).unwrap_err();

        let unknown_backend_field = r#"
version: 2
backends:
  huddle:
    type: claude
    commmand: huddle
agents:
  local-claude:
    backend: huddle
    member: andy
"#;
        Router::from_yaml(unknown_backend_field).unwrap_err();
    }

    #[test]
    fn rejects_unknown_agent_fields() {
        let text = r#"
version: 2
backends:
  cx:
    type: codex
    url: ws://127.0.0.1:4107
agents:
  local-cx:
    backend: cx
    cwd: /home/andy/nixos
    thread-id: thread-1
"#;
        Router::from_yaml(text).unwrap_err();
    }

    #[test]
    fn validates_backend_config_values() {
        let blank_backend_name = r#"
version: 2
backends:
  "":
    type: claude
agents:
  local-claude:
    backend: ""
    member: andy
"#;
        let error = Router::from_yaml(blank_backend_name)
            .unwrap_err()
            .to_string();
        assert!(error.contains("routing backend name cannot be empty"));

        let blank_huddle_command = r#"
version: 2
backends:
  huddle:
    type: claude
    command: " "
agents:
  local-claude:
    backend: huddle
    member: andy
"#;
        let error = Router::from_yaml(blank_huddle_command)
            .unwrap_err()
            .to_string();
        assert!(error.contains("field command cannot be blank"));

        let wrong_opencode_scheme = r#"
version: 2
backends:
  oc:
    type: opencode
    base_url: ws://127.0.0.1:4096
agents:
  local-oc:
    backend: oc
    cwd: /home/andy/vault
"#;
        let error = Router::from_yaml(wrong_opencode_scheme)
            .unwrap_err()
            .to_string();
        assert!(error.contains("must use one of: http, https"));

        let opencode_query_base_url = r#"
version: 2
backends:
  oc:
    type: opencode
    base_url: http://127.0.0.1:4096?x=1
agents:
  local-oc:
    backend: oc
    cwd: /home/andy/vault
"#;
        let error = Router::from_yaml(opencode_query_base_url)
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot include query or fragment"));

        let wrong_codex_scheme = r#"
version: 2
backends:
  cx:
    type: codex
    url: http://127.0.0.1:4107
agents:
  local-cx:
    backend: cx
    cwd: /home/andy/nixos
"#;
        let error = Router::from_yaml(wrong_codex_scheme)
            .unwrap_err()
            .to_string();
        assert!(error.contains("must use one of: ws, wss"));
    }

    #[test]
    fn validates_claude_huddle_member_is_explicit() {
        let config: RoutingConfig = serde_yml::from_str(
            r#"
version: 2
backends:
  huddle:
    type: claude
agents:
  andy-coh:
    backend: huddle
"#,
        )
        .unwrap();

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("must define Huddle member"));
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
    fn router_builds_claude_huddle_envelope() {
        let router = Router::from_yaml(
            r#"
version: 2
backends:
  huddle:
    type: claude
    channel: huddle
agents:
  andy-coh:
    backend: huddle
    member: andy-coh
routes:
  default_reply_to: caller
"#,
        )
        .unwrap();

        let (envelope, backend) = router
            .envelope_for("andy-coh", "status".to_string(), "andy".to_string())
            .unwrap();

        assert_eq!(backend.kind(), BackendKind::Claude);
        assert_eq!(envelope.target, "andy-coh");
        assert_eq!(envelope.channel.as_deref(), Some("huddle"));
        assert_eq!(envelope.member.as_deref(), Some("andy-coh"));
        assert_eq!(envelope.reply_to.as_deref(), Some("caller"));
        assert!(envelope.cwd.is_none());
        assert!(!envelope.correlation_id.is_empty());
    }

    #[test]
    fn routing_example_yaml_stays_parseable() {
        let config: RoutingConfig =
            serde_yml::from_str(include_str!("../routing.example.yaml")).unwrap();

        config.validate().unwrap();
        assert!(config.backends.contains_key("opencode"));
        assert!(config.backends.contains_key("codex"));
        assert!(config.backends.contains_key("huddle"));
        assert!(config.agents.contains_key("nixos-cx"));
        assert!(config.agents.contains_key("andy-coh"));
    }

    #[test]
    fn builds_opencode_prompt_body() {
        assert_eq!(
            OpenCodeBackend::prompt_body("hello"),
            json!({"parts": [{"type": "text", "text": "hello"}]})
        );

        let backend = OpenCodeBackend::new("http://127.0.0.1:4096");
        assert_eq!(
            backend.prompt_async_url("ses/one?x=1#frag"),
            "http://127.0.0.1:4096/session/ses%2Fone%3Fx%3D1%23frag/prompt_async"
        );
    }

    #[test]
    fn builds_claude_huddle_command_args() {
        assert_eq!(
            ClaudeBackend::command_args("andy-coh", "hello huddle"),
            vec!["send", "--to", "andy-coh", "hello huddle"]
        );
        assert_eq!(
            ClaudeBackend::command_args("@Andy", "--help"),
            vec!["send", "--to", "andy", " --help"]
        );
        assert_eq!(ClaudeBackend::sessions_args(), vec!["sessions"]);
    }

    #[test]
    fn detects_connected_huddle_members() {
        let sessions = "andy                 pid=1464138 since 2026-05-02T06:10:53.353Z\n\
                        vault                pid=1465000 since 2026-05-02T06:11:00.000Z";

        assert!(ClaudeBackend::member_is_connected(sessions, "andy"));
        assert!(ClaudeBackend::member_is_connected(sessions, "@Andy"));
        assert!(ClaudeBackend::member_is_connected(sessions, " andy "));
        assert!(ClaudeBackend::member_is_connected(sessions, "vault"));
        assert!(!ClaudeBackend::member_is_connected(sessions, "andy-coh"));
    }

    #[test]
    fn rejects_huddle_inline_mentions_that_escape_target() {
        ClaudeBackend::ensure_single_target_message("status for @andy", "andy").unwrap();
        ClaudeBackend::ensure_single_target_message("email user@example.com", "andy").unwrap();

        let error = ClaudeBackend::ensure_single_target_message("status for @vault", "andy")
            .unwrap_err()
            .to_string();
        assert!(error.contains("inline @vault"));

        let error = ClaudeBackend::ensure_single_target_message("status for @all", "andy")
            .unwrap_err()
            .to_string();
        assert!(error.contains("inline @all"));
    }

    #[tokio::test]
    async fn claude_backend_reports_missing_huddle_command() {
        let mut envelope = envelope();
        envelope.backend = BackendKind::Claude;
        envelope.target = "andy-coh".to_string();
        envelope.member = Some("andy-coh".to_string());

        let error = ClaudeBackend::new("metastack-missing-huddle-command-for-test")
            .send(envelope)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("failed to run metastack-missing-huddle-command-for-test"));
    }

    #[test]
    fn selects_unique_opencode_session_for_cwd() {
        let sessions = vec![
            OpenCodeSession {
                id: "other".to_string(),
                directory: Some("/home/andy/vault".to_string()),
                updated_at: Some("2026-05-01T02:00:00Z".to_string()),
                parent_id: None,
                time: None,
            },
            OpenCodeSession {
                id: "new".to_string(),
                directory: Some("/home/andy/nixos".to_string()),
                updated_at: Some("2026-05-01T03:00:00Z".to_string()),
                parent_id: None,
                time: None,
            },
        ];

        assert_eq!(
            OpenCodeBackend::select_session(&sessions, "/home/andy/nixos")
                .unwrap()
                .map(|session| session.id.as_str()),
            Some("new")
        );
    }

    #[test]
    fn opencode_session_discovery_rejects_ambiguous_top_level_sessions() {
        let sessions = vec![
            OpenCodeSession {
                id: "old".to_string(),
                directory: Some("/home/andy/nixos".to_string()),
                updated_at: Some("2026-05-01T00:00:00Z".to_string()),
                parent_id: None,
                time: None,
            },
            OpenCodeSession {
                id: "new".to_string(),
                directory: Some("/home/andy/nixos".to_string()),
                updated_at: Some("2026-05-01T03:00:00Z".to_string()),
                parent_id: None,
                time: None,
            },
        ];

        let error = OpenCodeBackend::select_session(&sessions, "/home/andy/nixos")
            .unwrap_err()
            .to_string();

        assert!(error.contains("multiple OpenCode sessions"));
        assert!(error.contains("session_id"));
    }

    #[test]
    fn opencode_session_discovery_prefers_top_level_sessions() {
        let sessions: Vec<OpenCodeSession> = serde_json::from_value(json!([
            {
                "id": "parent",
                "directory": "/home/andy",
                "time": {"updated": 10}
            },
            {
                "id": "newer-child",
                "directory": "/home/andy",
                "parentID": "parent",
                "time": {"updated": 20}
            }
        ]))
        .unwrap();

        assert_eq!(
            OpenCodeBackend::select_session(&sessions, "/home/andy")
                .unwrap()
                .map(|session| session.id.as_str()),
            Some("parent")
        );
    }

    #[test]
    fn opencode_session_discovery_rejects_ambiguous_child_sessions() {
        let sessions: Vec<OpenCodeSession> = serde_json::from_value(json!([
            {
                "id": "old-child",
                "directory": "/home/andy",
                "parentID": "parent",
                "time": {"updated": 10}
            },
            {
                "id": "new-child",
                "directory": "/home/andy",
                "parentID": "parent",
                "time": {"updated": 20}
            }
        ]))
        .unwrap();

        let error = OpenCodeBackend::select_session(&sessions, "/home/andy")
            .unwrap_err()
            .to_string();

        assert!(error.contains("multiple OpenCode sessions"));
        assert!(error.contains("session_id"));
    }

    #[test]
    fn opencode_session_discovery_rejects_ambiguous_live_time_shape() {
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

        let error = OpenCodeBackend::select_session(&sessions, "/home/andy/nixos")
            .unwrap_err()
            .to_string();

        assert!(error.contains("multiple OpenCode sessions"));
        assert!(error.contains("session_id"));
    }

    #[tokio::test]
    async fn opencode_backend_posts_prompt_async_against_fake_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            assert!(request.starts_with("GET /session?directory=%2Fhome%2Fandy%2Fnixos "));

            let body = serde_json::to_string(&json!([
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

        let receipt = backend.send(envelope).await.unwrap();

        assert_eq!(receipt.backend, BackendKind::OpenCode);
        assert_eq!(receipt.status, SendStatus::Accepted);
        assert_eq!(receipt.session_id.as_deref(), Some("ses-new"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn opencode_backend_rejects_ambiguous_session_discovery() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            assert!(request.starts_with("GET /session?directory=%2Fhome%2Fandy%2Fnixos "));

            let body = serde_json::to_string(&json!([
                {
                    "id": "ses-a",
                    "directory": "/home/andy/nixos",
                    "time": {"updated": 1}
                },
                {
                    "id": "ses-b",
                    "directory": "/home/andy/nixos",
                    "time": {"updated": 2}
                }
            ]))
            .unwrap();
            write_http_response(&mut stream, 200, Some(&body)).await;
        });

        let backend = OpenCodeBackend::new(format!("http://{addr}"));
        let mut envelope = envelope();
        envelope.backend = BackendKind::OpenCode;
        envelope.cwd = Some("/home/andy/nixos".to_string());

        let error = backend.send(envelope).await.unwrap_err().to_string();

        assert!(error.contains("multiple OpenCode sessions"));
        assert!(error.contains("session_id"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn opencode_backend_percent_encodes_session_id_path_segment() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut stream).await;

            let body = serde_json::to_string(&json!([
                {
                    "id": "ses/one?x=1#frag",
                    "directory": "/home/andy/nixos",
                    "time": {"updated": 1}
                }
            ]))
            .unwrap();
            write_http_response(&mut stream, 200, Some(&body)).await;

            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            assert!(request.starts_with("POST /session/ses%2Fone%3Fx%3D1%23frag/prompt_async "));
            write_http_response(&mut stream, 204, None).await;
        });

        let backend = OpenCodeBackend::new(format!("http://{addr}"));
        let mut envelope = envelope();
        envelope.backend = BackendKind::OpenCode;
        envelope.cwd = Some("/home/andy/nixos".to_string());

        let receipt = backend.send(envelope).await.unwrap();

        assert_eq!(receipt.session_id.as_deref(), Some("ses/one?x=1#frag"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn opencode_backend_rejects_stale_explicit_session_id() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            assert!(request.starts_with("GET /session?directory=%2Fhome%2Fandy%2Fnixos "));

            let body = serde_json::to_string(&json!([
                {
                    "id": "ses-live",
                    "directory": "/home/andy/nixos",
                    "time": {"updated": 2}
                }
            ]))
            .unwrap();
            write_http_response(&mut stream, 200, Some(&body)).await;
        });

        let backend = OpenCodeBackend::new(format!("http://{addr}"));
        let mut envelope = envelope();
        envelope.backend = BackendKind::OpenCode;
        envelope.target = "vault-oc".to_string();
        envelope.cwd = Some("/home/andy/nixos".to_string());
        envelope.session_id = Some("ses-stale".to_string());

        let error = backend.send(envelope).await.unwrap_err().to_string();

        assert!(error.contains("configured OpenCode session_id ses-stale not found"));
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

        let result = tokio::time::timeout(Duration::from_secs(1), backend.send(envelope))
            .await
            .expect("OpenCode send should return before outer test timeout");

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
        assert_eq!(list["params"]["sourceKinds"], json!(["cli", "vscode"]));
        assert_eq!(list["params"]["limit"], 100);

        let resume = backend.thread_resume_request(3, "thread-1", "/home/andy/nixos");
        assert_eq!(resume["method"], "thread/resume");
        assert_eq!(resume["params"]["threadId"], "thread-1");
        assert_eq!(resume["params"]["cwd"], "/home/andy/nixos");
        assert_eq!(resume["params"]["excludeTurns"], true);

        let notification = CodexBackend::initialized_notification();
        assert_eq!(notification["method"], "initialized");

        let initialize = CodexBackend::initialize_request(1);
        assert_eq!(
            initialize["params"]["clientInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
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
    fn selects_codex_thread_matching_cwd() {
        let response = json!({
            "data": [
                {
                    "id": "wrong-active-cli",
                    "cwd": "/home/andy/other",
                    "source": "cli",
                    "status": {"type": "active"}
                },
                {
                    "id": "matching-idle-cli",
                    "cwd": "/home/andy/nixos",
                    "source": "cli",
                    "status": {"type": "idle"}
                }
            ]
        });

        assert_eq!(
            CodexBackend::select_thread_for_cwd(&response, "/home/andy/nixos").as_deref(),
            Some("matching-idle-cli")
        );
    }

    #[test]
    fn codex_thread_selection_rejects_ambiguous_active_cli_threads() {
        let response = json!({
            "data": [
                {
                    "id": "active-a",
                    "cwd": "/home/andy/nixos",
                    "source": "cli",
                    "status": {"type": "active"}
                },
                {
                    "id": "active-b",
                    "cwd": "/home/andy/nixos",
                    "source": "cli",
                    "status": {"type": "active"}
                }
            ]
        });

        let error = CodexBackend::select_thread_ref(&response, Some("/home/andy/nixos"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("multiple live Codex routable threads"));
        assert!(error.contains("thread_id"));
    }

    #[test]
    fn codex_thread_selection_rejects_ambiguous_idle_cli_threads() {
        let response = json!({
            "data": [
                {
                    "id": "idle-a",
                    "cwd": "/home/andy/nixos",
                    "source": "cli",
                    "status": {"type": "idle"}
                },
                {
                    "id": "idle-b",
                    "cwd": "/home/andy/nixos",
                    "source": "cli",
                    "status": {"type": "idle"}
                }
            ]
        });

        let error = CodexBackend::select_thread_ref(&response, Some("/home/andy/nixos"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("multiple live Codex routable threads"));
        assert!(error.contains("thread_id"));
    }

    #[test]
    fn codex_thread_selection_prefers_single_active_cli_thread() {
        let response = json!({
            "data": [
                {
                    "id": "idle-cli",
                    "cwd": "/home/andy/nixos",
                    "source": "cli",
                    "status": {"type": "idle"}
                },
                {
                    "id": "active-cli",
                    "cwd": "/home/andy/nixos",
                    "source": "cli",
                    "status": {"type": "active"}
                }
            ]
        });

        let selected = CodexBackend::select_thread_ref(&response, Some("/home/andy/nixos"))
            .unwrap()
            .unwrap();

        assert_eq!(selected.id, "active-cli");
    }

    #[test]
    fn codex_thread_selection_accepts_vscode_remote_thread() {
        let response = json!({
            "data": [
                {
                    "id": "sutro-thread",
                    "cwd": "/home/andy/sutro",
                    "source": "vscode",
                    "status": {"type": "idle"}
                }
            ]
        });

        let selected = CodexBackend::select_thread_ref(&response, Some("/home/andy/sutro"))
            .unwrap()
            .unwrap();

        assert_eq!(selected.id, "sutro-thread");
        assert_eq!(selected.status.as_deref(), Some("idle"));
    }

    #[test]
    fn codex_thread_selection_ignores_not_loaded_threads() {
        let response = json!({
            "data": [
                {
                    "id": "stale-a",
                    "cwd": "/home/andy/nixos",
                    "source": "cli",
                    "status": {"type": "notLoaded"}
                },
                {
                    "id": "idle-cli",
                    "cwd": "/home/andy/nixos",
                    "source": "cli",
                    "status": {"type": "idle"}
                },
                {
                    "id": "stale-b",
                    "cwd": "/home/andy/nixos",
                    "source": "cli",
                    "status": {"type": "notLoaded"}
                }
            ]
        });

        let selected = CodexBackend::select_thread_ref(&response, Some("/home/andy/nixos"))
            .unwrap()
            .unwrap();

        assert_eq!(selected.id, "idle-cli");
    }

    #[test]
    fn codex_thread_selection_returns_none_for_only_not_loaded_threads() {
        let response = json!({
            "data": [
                {
                    "id": "stale-a",
                    "cwd": "/home/andy/nixos",
                    "source": "cli",
                    "status": {"type": "notLoaded"}
                },
                {
                    "id": "stale-b",
                    "cwd": "/home/andy/nixos",
                    "source": "cli",
                    "status": {"type": "notLoaded"}
                }
            ]
        });

        assert_eq!(
            CodexBackend::select_thread_ref(&response, Some("/home/andy/nixos"))
                .unwrap()
                .map(|thread| thread.id),
            None
        );
    }

    #[test]
    fn codex_thread_selection_rejects_mismatched_cwd_when_response_has_cwd() {
        let response = json!({
            "data": [
                {
                    "id": "wrong-active-cli",
                    "cwd": "/home/andy/other",
                    "source": "cli",
                    "status": {"type": "active"}
                }
            ]
        });

        assert_eq!(
            CodexBackend::select_thread_for_cwd(&response, "/home/andy/nixos"),
            None
        );
    }

    #[test]
    fn codex_thread_selection_fails_closed_without_cwd_metadata() {
        let response = json!({
            "data": [
                {
                    "id": "active-cli",
                    "source": "cli",
                    "status": {"type": "active"}
                }
            ]
        });

        assert_eq!(
            CodexBackend::select_thread_for_cwd(&response, "/home/andy/nixos"),
            None
        );
    }

    #[test]
    fn codex_explicit_thread_validation_requires_matching_cwd_and_routable_source() {
        let response = json!({
            "data": [
                {
                    "id": "wrong-cwd",
                    "cwd": "/home/andy/other",
                    "source": "cli",
                    "status": {"type": "active"}
                },
                {
                    "id": "right-id-api-source",
                    "cwd": "/home/andy/nixos",
                    "source": "api",
                    "status": {"type": "active"}
                },
                {
                    "id": "vscode-thread",
                    "cwd": "/home/andy/nixos",
                    "source": "vscode",
                    "status": {"type": "idle"}
                },
                {
                    "id": "thread-1",
                    "cwd": "/home/andy/nixos",
                    "source": "cli",
                    "status": {"type": "idle"}
                }
            ]
        });

        let selected =
            CodexBackend::select_thread_ref_by_id(&response, Some("/home/andy/nixos"), "thread-1")
                .unwrap();

        assert_eq!(selected.id, "thread-1");
        assert_eq!(selected.status.as_deref(), Some("idle"));
        assert_eq!(
            CodexBackend::select_thread_ref_by_id(
                &response,
                Some("/home/andy/nixos"),
                "vscode-thread"
            )
            .unwrap()
            .id,
            "vscode-thread"
        );
        assert!(
            CodexBackend::select_thread_ref_by_id(&response, Some("/home/andy/nixos"), "wrong-cwd")
                .is_none()
        );
        assert!(
            CodexBackend::select_thread_ref_by_id(
                &response,
                Some("/home/andy/nixos"),
                "right-id-api-source"
            )
            .is_none()
        );
    }

    #[test]
    fn codex_explicit_thread_validation_allows_not_loaded_pin() {
        let response = json!({
            "data": [
                {
                    "id": "stale-not-loaded",
                    "cwd": "/home/andy/nixos",
                    "source": "cli",
                    "status": {"type": "notLoaded"}
                }
            ]
        });

        let selected = CodexBackend::select_thread_ref_by_id(
            &response,
            Some("/home/andy/nixos"),
            "stale-not-loaded",
        )
        .unwrap();

        assert_eq!(selected.id, "stale-not-loaded");
        assert_eq!(selected.status.as_deref(), Some("notLoaded"));
    }

    #[test]
    fn codex_explicit_thread_validation_fails_closed_without_cwd_metadata() {
        let response = json!({
            "data": [
                {
                    "id": "thread-1",
                    "source": "cli",
                    "status": {"type": "active"}
                }
            ]
        });

        assert!(
            CodexBackend::select_thread_ref_by_id(&response, Some("/home/andy/nixos"), "thread-1")
                .is_none()
        );
    }

    #[test]
    fn validates_codex_turn_start_acceptance_payload() {
        assert!(
            CodexBackend::validate_turn_start_response(&json!({
                "turn": {"id": "turn-1", "status": "inProgress"}
            }))
            .is_ok()
        );
        assert!(
            CodexBackend::validate_turn_start_response(&json!({
                "turn": {"id": "turn-1", "status": "completed"}
            }))
            .is_ok()
        );

        let error = CodexBackend::validate_turn_start_response(&json!({}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing turn"));

        let error = CodexBackend::validate_turn_start_response(&json!({
            "turn": {"id": "turn-1", "status": "failed"}
        }))
        .unwrap_err()
        .to_string();
        assert!(error.contains("was not accepted"));
    }

    #[tokio::test]
    async fn codex_backend_submits_turn_against_fake_server() {
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
                                "id": "stale-a",
                                "cwd": "/home/andy/nixos",
                                "source": "cli",
                                "status": {"type": "notLoaded"}
                            },
                            {
                                "id": "thread-1",
                                "cwd": "/home/andy/nixos",
                                "source": "cli",
                                "status": {"type": "active"}
                            },
                            {
                                "id": "stale-b",
                                "cwd": "/home/andy/nixos",
                                "source": "cli",
                                "status": {"type": "notLoaded"}
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
        });

        let backend = CodexBackend::new(format!("ws://{addr}"));
        let receipt = backend.send(envelope()).await.unwrap();

        assert_eq!(receipt.backend, BackendKind::Codex);
        assert_eq!(receipt.status, SendStatus::Accepted);
        assert_eq!(receipt.thread_id.as_deref(), Some("thread-1"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn codex_backend_validates_explicit_thread_id_before_resume() {
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
                                "status": {"type": "idle"}
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
        });

        let backend = CodexBackend::new(format!("ws://{addr}"));
        let mut envelope = envelope();
        envelope.thread_id = Some("thread-1".to_string());
        let receipt = backend.send(envelope).await.unwrap();

        assert_eq!(receipt.status, SendStatus::Accepted);
        assert_eq!(receipt.thread_id.as_deref(), Some("thread-1"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn codex_backend_rejects_ambiguous_thread_discovery_before_resume() {
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
                                "id": "thread-a",
                                "cwd": "/home/andy/nixos",
                                "source": "cli",
                                "status": {"type": "active"}
                            },
                            {
                                "id": "thread-b",
                                "cwd": "/home/andy/nixos",
                                "source": "cli",
                                "status": {"type": "active"}
                            }
                        ]
                    }
                }),
            )
            .await;

            let next = tokio::time::timeout(Duration::from_millis(50), next_ws_json(&mut ws)).await;
            if let Ok(Ok(value)) = next {
                panic!(
                    "unexpected Codex request after ambiguous discovery: {}",
                    value
                );
            }
        });

        let backend = CodexBackend::new(format!("ws://{addr}"));
        let error = backend.send(envelope()).await.unwrap_err().to_string();

        assert!(error.contains("multiple live Codex routable threads"));
        assert!(error.contains("thread_id"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn codex_backend_rejects_explicit_thread_id_for_wrong_cwd() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

            let request = next_ws_json(&mut ws).await.unwrap();
            assert_eq!(request["method"], "initialize");
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
                                "id": "thread-stale",
                                "cwd": "/home/andy/other",
                                "source": "cli",
                                "status": {"type": "active"}
                            }
                        ]
                    }
                }),
            )
            .await;
        });

        let backend = CodexBackend::new(format!("ws://{addr}"));
        let mut envelope = envelope();
        envelope.thread_id = Some("thread-stale".to_string());
        let error = backend.send(envelope).await.unwrap_err().to_string();

        assert!(error.contains("configured Codex thread_id thread-stale not found"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn codex_backend_rejects_failed_turn_start_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

            let request = next_ws_json(&mut ws).await.unwrap();
            assert_eq!(request["method"], "initialize");
            send_ws_json(&mut ws, json!({"jsonrpc": "2.0", "id": 1, "result": {}})).await;

            let notification = next_ws_json(&mut ws).await.unwrap();
            assert_eq!(notification["method"], "initialized");

            let request = next_ws_json(&mut ws).await.unwrap();
            assert_eq!(request["method"], "thread/list");
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
            send_ws_json(&mut ws, json!({"jsonrpc": "2.0", "id": 3, "result": {}})).await;

            let request = next_ws_json(&mut ws).await.unwrap();
            assert_eq!(request["method"], "turn/start");
            send_ws_json(
                &mut ws,
                json!({
                    "jsonrpc": "2.0",
                    "id": 4,
                    "result": {
                        "turn": {
                            "id": "turn-1",
                            "status": "failed"
                        }
                    }
                }),
            )
            .await;
        });

        let backend = CodexBackend::new(format!("ws://{addr}"));
        let error = backend.send(envelope()).await.unwrap_err().to_string();

        assert!(error.contains("Codex turn turn-1 was not accepted"));
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
            std::future::pending::<()>().await;
        });

        let mut backend = CodexBackend::new(format!("ws://{addr}"));
        backend.transport_timeout = Duration::from_millis(200);
        let error = backend.send(envelope()).await.unwrap_err().to_string();

        assert!(
            error.contains("timed out waiting for JSON-RPC response id 1"),
            "{error}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn ws_json_write_uses_transport_timeout() {
        let mut sink = PendingFlushSink;
        let error = super::send_ws_json(
            &mut sink,
            json!({"jsonrpc": "2.0", "method": "initialized"}),
            Duration::from_millis(20),
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(error.contains("timed out writing JSON-RPC message"));
    }

    #[tokio::test]
    async fn json_rpc_wait_ignores_unrelated_error_ids() {
        let mut stream = futures_util::stream::iter(vec![
            Ok(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": 99,
                    "error": {"code": -32000, "message": "unrelated"}
                })
                .to_string()
                .into(),
            )),
            Ok(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "result": {"ok": true}
                })
                .to_string()
                .into(),
            )),
        ]);

        let response = wait_for_response(&mut stream, 3, Duration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(response, json!({"ok": true}));
    }

    #[tokio::test]
    async fn json_rpc_wait_reports_matching_error_id() {
        let mut stream = futures_util::stream::iter(vec![Ok(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "error": {"code": -32000, "message": "boom"}
            })
            .to_string()
            .into(),
        ))]);

        let error = wait_for_response(&mut stream, 3, Duration::from_secs(1))
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("JSON-RPC error for id 3"));
        assert!(error.contains("boom"));
    }

    #[tokio::test]
    async fn json_rpc_wait_rejects_matching_response_without_result_or_error() {
        let mut stream = futures_util::stream::iter(vec![Ok(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 3
            })
            .to_string()
            .into(),
        ))]);

        let error = wait_for_response(&mut stream, 3, Duration::from_secs(1))
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("malformed JSON-RPC response for id 3"));
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
        assert!(opencode.has_delivery_receipt);
        assert!(!opencode.is_lossy);

        let codex = BackendCapabilities::for_kind(BackendKind::Codex);
        assert!(!codex.preserves_role);
        assert!(codex.has_delivery_receipt);
        assert!(!codex.is_lossy);

        let claude = BackendCapabilities::for_kind(BackendKind::Claude);
        assert!(!claude.preserves_role);
        assert!(!claude.has_delivery_receipt);
        assert!(!claude.is_lossy);

        let zellij = BackendCapabilities::for_kind(BackendKind::Zellij);
        assert!(!zellij.preserves_role);
        assert!(!zellij.has_delivery_receipt);
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
