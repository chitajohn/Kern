//! Agent configuration (SPEC.md §9, schema v1).
//!
//! `agent.yaml` is parsed with `deny_unknown_fields: true` everywhere; unknown
//! keys, bad types, unknown providers/tools, and invalid permission rules fail
//! at create time with a structured `CONFIG_INVALID` error that carries the
//! offending field (via `serde_path_to_error`). Line/column are included when
//! `serde_yaml` attaches them (syntax errors); semantic errors name the field
//! in the message — `serde_yaml` does not attach source positions to type
//! errors, so the field path is the locator (documented limitation).
//!
//! Secrets stay out of the config: a value written as `env:VAR` is kept as the
//! literal reference (never resolved at parse time, never persisted as its
//! value) and resolved in memory at use time via [`resolve_env_ref`]
//! (SPEC.md §14.3).

use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::error::{ErrorCode, KernError, Result};

/// Configuration duration, written as `<number><unit>` with unit `ms | s | m | h | d`
/// (e.g. `30s`, `12h`, `500ms`). Zero is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Duration {
    millis: u64,
}

impl Duration {
    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        let s = s.trim();
        let (digits, multiplier) = if let Some(rest) = s.strip_suffix("ms") {
            (rest, 1u64)
        } else if let Some(rest) = s.strip_suffix('s') {
            (rest, 1_000u64)
        } else if let Some(rest) = s.strip_suffix('m') {
            (rest, 60_000u64)
        } else if let Some(rest) = s.strip_suffix('h') {
            (rest, 3_600_000u64)
        } else if let Some(rest) = s.strip_suffix('d') {
            (rest, 86_400_000u64)
        } else {
            return Err(format!(
                "invalid duration {s:?}: expected <number><unit> with unit ms|s|m|h|d (e.g. \"30s\")"
            ));
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!(
                "invalid duration {s:?}: expected a positive integer before the unit"
            ));
        }
        let n: u64 = digits
            .parse()
            .map_err(|_| format!("invalid duration {s:?}: number out of range"))?;
        let millis = n
            .checked_mul(multiplier)
            .ok_or_else(|| format!("invalid duration {s:?}: overflows"))?;
        if millis == 0 {
            return Err(format!("invalid duration {s:?}: must be greater than zero"));
        }
        Ok(Self { millis })
    }

    pub fn from_millis(millis: u64) -> Self {
        Self { millis }
    }

    pub fn as_millis(self) -> u64 {
        self.millis
    }

    pub fn as_std(self) -> std::time::Duration {
        std::time::Duration::from_millis(self.millis)
    }

    /// Canonical string form, choosing the largest unit that divides evenly.
    pub fn as_str(self) -> String {
        const D: u64 = 86_400_000;
        const H: u64 = 3_600_000;
        const M: u64 = 60_000;
        const S: u64 = 1_000;
        if self.millis.is_multiple_of(D) {
            format!("{}d", self.millis / D)
        } else if self.millis.is_multiple_of(H) {
            format!("{}h", self.millis / H)
        } else if self.millis.is_multiple_of(M) {
            format!("{}m", self.millis / M)
        } else if self.millis.is_multiple_of(S) {
            format!("{}s", self.millis / S)
        } else {
            format!("{}ms", self.millis)
        }
    }
}

impl Serialize for Duration {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_str())
    }
}

impl<'de> Deserialize<'de> for Duration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct DurationVisitor;

        impl Visitor<'_> for DurationVisitor {
            type Value = Duration;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "a duration string like \"30s\" or \"12h\"")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Duration, E> {
                Duration::parse(v).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(DurationVisitor)
    }
}

/// Model provider (`SPEC.md §9`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    OpenAI,
    Anthropic,
    Ollama,
    Mock,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::OpenAI => "openai",
            Provider::Anthropic => "anthropic",
            Provider::Ollama => "ollama",
            Provider::Mock => "mock",
        }
    }
}

impl FromStr for Provider {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "openai" => Ok(Provider::OpenAI),
            "anthropic" => Ok(Provider::Anthropic),
            "ollama" => Ok(Provider::Ollama),
            "mock" => Ok(Provider::Mock),
            _ => Err(format!(
                "unknown provider {s:?} (expected openai|anthropic|ollama|mock)"
            )),
        }
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Agent spec (schema v1)
// ---------------------------------------------------------------------------

/// The fully validated agent specification (`SPEC.md §9`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpec {
    pub version: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub model: ModelConfig,
    pub tools: Vec<String>,
    /// External MCP tool servers (stdio transport). Their tools are named
    /// `<server>.<tool>` and must also be listed in `tools`.
    #[serde(default, skip_serializing_if = "McpConfig::is_empty")]
    pub mcp: McpConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub permissions: PermissionsConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<ScheduleConfig>,
    #[serde(default)]
    pub runtime: AgentRuntime,
    /// Agents this agent may call as tools (`agent.<name>`, ARCHITECTURE.md
    /// §35). A sub-agent is a real execution with its own id, workspace,
    /// checkpoint history, and budgets — not an in-process pretense — and its
    /// capability set is a subset of its parent's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagents: Vec<SubagentConfig>,
    /// Secrets injected into specific tool surfaces (ARCHITECTURE.md §33).
    /// Values are resolved from the daemon environment at agent start and are
    /// never persisted, logged, or shown to the model — only handed to the
    /// tool that was granted them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<CredentialConfig>,
}

/// Where a credential may be used (§33). A credential must name at least one
/// target; it is never injected anywhere implicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialTarget {
    /// As an environment variable for `shell` subprocesses.
    Shell,
    /// As an HTTP header value for the `http` tool.
    Http,
    /// As an environment variable for every MCP server the agent uses.
    Mcp,
}

impl CredentialTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            CredentialTarget::Shell => "shell",
            CredentialTarget::Http => "http",
            CredentialTarget::Mcp => "mcp",
        }
    }
}

impl std::str::FromStr for CredentialTarget {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "shell" => Ok(CredentialTarget::Shell),
            "http" => Ok(CredentialTarget::Http),
            "mcp" => Ok(CredentialTarget::Mcp),
            other => Err(format!(
                "unknown credential target {other:?} (expected shell|http|mcp)"
            )),
        }
    }
}

/// One sub-agent definition (`subagents[]`, ARCHITECTURE.md §35).
///
/// There is deliberately no `subagents` field here: nesting is not allowed, so
/// the call graph is exactly one level deep and a fan-out cannot recurse.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentConfig {
    /// Slug used in the tool name `agent.<name>`.
    pub name: String,
    /// What the parent's model reads to decide when to delegate.
    pub description: String,
    /// The child's tool list. Must be a subset of the parent's tools.
    pub tools: Vec<String>,
    /// Child step budget (default: the parent's `runtime.max_steps`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<u32>,
    /// Child wall-clock budget (default: the parent's `runtime.max_duration`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration: Option<Duration>,
    /// Optional model override; defaults to the parent's model block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelConfig>,
}

/// One credential grant (§33).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialConfig {
    /// The variable/header name. For `shell`/`mcp` this is the env var name
    /// (uppercase); for `http` it may be a header name (`Authorization`).
    pub name: String,
    /// Source value: `env:VAR` (resolved at agent start) or a literal.
    pub from: String,
    /// Targets this credential is injected into. Defaults to `[shell]` — the
    /// narrowest useful grant.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// `http` only: the header to set (default `Authorization`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// `http` only: prefix for the header value (e.g. `Bearer `).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
}

impl CredentialConfig {
    /// The resolved targets (default `[shell]`).
    pub fn targets(&self) -> Result<Vec<CredentialTarget>> {
        if self.tools.is_empty() {
            return Ok(vec![CredentialTarget::Shell]);
        }
        let mut out = Vec::with_capacity(self.tools.len());
        for tool in &self.tools {
            out.push(
                tool.parse::<CredentialTarget>()
                    .map_err(|e| invalid("credentials", format!("{}: {e}", self.name)))?,
            );
        }
        Ok(out)
    }
}

/// A credential resolved for use at agent start. The value is `exclude`d from
/// Debug so it cannot leak through a log line or a panic message.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedCredential {
    pub name: String,
    pub value: String,
    pub targets: Vec<CredentialTarget>,
    pub header: Option<String>,
    pub prefix: Option<String>,
}

impl std::fmt::Debug for ResolvedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedCredential")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .field("targets", &self.targets)
            .field("header", &self.header)
            .field("prefix", &self.prefix)
            .finish()
    }
}

impl ResolvedCredential {
    /// `VAR=value` pairs for the shell tool.
    pub fn shell_env(&self) -> Option<(String, String)> {
        self.targets
            .contains(&CredentialTarget::Shell)
            .then(|| (self.name.clone(), self.value.clone()))
    }

    /// `(header, value)` for the http tool, when granted.
    pub fn http_header(&self) -> Option<(String, String)> {
        if !self.targets.contains(&CredentialTarget::Http) {
            return None;
        }
        let header = self
            .header
            .clone()
            .unwrap_or_else(|| "Authorization".into());
        let value = format!("{}{}", self.prefix.clone().unwrap_or_default(), self.value);
        Some((header, value))
    }

    /// `VAR=value` pairs for MCP servers, when granted.
    pub fn mcp_env(&self) -> Option<(String, String)> {
        self.targets
            .contains(&CredentialTarget::Mcp)
            .then(|| (self.name.clone(), self.value.clone()))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    pub provider: Provider,
    pub model: String,
    #[serde(default = "default_temperature")]
    pub temperature: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
    /// Provider base URL; may be an `env:VAR` reference (resolved at use time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Ordered routing rules (ARCHITECTURE.md §32). The first rule whose
    /// conditions all hold wins for that call; otherwise `provider`/`model`
    /// above are used. Routing is evaluated per model call, so a long run can
    /// escalate without restarting.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routing: Vec<RoutingRule>,
}

/// One model-routing rule (§32). Every present condition must hold (`AND`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingRule {
    /// Match once the run has completed more than this many steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_step_above: Option<u32>,
    /// Match once the run has consumed more than this many tokens (reported or
    /// estimated, cumulative for the execution).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_tokens_above: Option<u64>,
    pub provider: Provider,
    pub model: String,
    /// Override `model.max_tokens` for calls matching this rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Override `model.temperature` for calls matching this rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
}

impl RoutingRule {
    /// Whether this rule matches the run's current position.
    pub fn matches(&self, steps: u64, tokens_used: u64) -> bool {
        if self.if_step_above.is_none() && self.if_tokens_above.is_none() {
            // An unconditional rule: always matches (validation allows it so a
            // trailing fallback can pin a different provider).
            return true;
        }
        if let Some(step) = self.if_step_above {
            if steps <= u64::from(step) {
                return false;
            }
        }
        if let Some(tokens) = self.if_tokens_above {
            if tokens_used <= tokens {
                return false;
            }
        }
        true
    }
}

fn default_temperature() -> f64 {
    0.0
}

/// Where an agent's memory lives (`memory.backend`, ARCHITECTURE.md §34).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryBackend {
    /// Agent-scoped rows in the runtime's SQLite store (default).
    #[default]
    Store,
    /// A JSON file inside the agent workspace.
    File,
}

impl MemoryBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryBackend::Store => "store",
            MemoryBackend::File => "file",
        }
    }
}

/// `memory:` block (`SPEC.md §9`). Exposes `memory.read/write/list` tools.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryConfig {
    pub enabled: bool,
    pub inject_digest: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_keys: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_value_bytes: Option<u64>,
    /// Storage backend (default `store`).
    pub backend: MemoryBackend,
    /// `file` backend only: directory inside the agent workspace
    /// (default `.kern/memory`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

/// `permissions:` block. Absence of a rule class means no access (default deny).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PermissionsConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filesystem: Option<FilesystemRules>,
    /// MCP tool rules: `<server>.<tool>` globs, default deny.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp: Option<RuleList>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkRules>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryRules>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<ShellRules>,
}

/// One allow/ask/deny rule set (SPEC.md §10).
///
/// `allow` grants, `ask` raises a human approval request, `deny` overrides
/// both (precedence within a match: `deny` > `ask` > `allow`). Absence of a
/// rule list means nothing is allowed for that class (default deny).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuleList {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deny: Option<Vec<String>>,
}

impl RuleList {
    /// All rule entries (allow, ask, deny) for validation/serialization.
    pub fn entries(&self) -> impl Iterator<Item = &String> {
        self.allow
            .iter()
            .flatten()
            .chain(self.ask.iter().flatten())
            .chain(self.deny.iter().flatten())
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilesystemRules {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read: Option<RuleList>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write: Option<RuleList>,
}

/// Network rules are class-level (a host is matched regardless of the
/// HTTP action), so the allow/ask/deny lists sit directly on the class.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkRules {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deny: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryRules {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read: Option<RuleList>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write: Option<RuleList>,
}

/// `permissions.shell:` — the only way to expose the `shell` tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellRules {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Present and valid only when `enabled` is true (`required | best-effort | off`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxMode>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    Required,
    BestEffort,
    Off,
}

impl SandboxMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SandboxMode::Required => "required",
            SandboxMode::BestEffort => "best-effort",
            SandboxMode::Off => "off",
        }
    }
}

/// `mcp:` block — external MCP tool servers over stdio (SPEC.md §9).
///
/// Servers are spawned as child processes with a scrubbed environment; every
/// tool they expose is a normal permission-gated Kern tool named
/// `<server>.<tool>`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Server name; its tools are `<name>.<tool>`.
    pub name: String,
    /// Executable on `PATH` (or an absolute path).
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// The only environment the server sees beyond a non-secret allowlist.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    /// Per-request JSON-RPC deadline (default 30s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
}

impl McpConfig {
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// The configured server that owns `tool` (`<server>.<tool>`), if any.
    pub fn owner_of(&self, tool: &str) -> Option<&McpServerConfig> {
        let (server, _) = tool.split_once('.')?;
        self.servers.iter().find(|s| s.name == server)
    }

    /// Whether `tool` names a tool of a configured server.
    pub fn is_mcp_tool(&self, tool: &str) -> bool {
        self.owner_of(tool).is_some()
    }

    fn validate(&self) -> Result<()> {
        let mut seen: Vec<&str> = Vec::new();
        for server in &self.servers {
            if !valid_slug(&server.name) {
                return Err(invalid(
                    "mcp.servers.name",
                    format!(
                        "invalid server name {:?}: must match [a-z0-9][a-z0-9-_]*",
                        server.name
                    ),
                ));
            }
            if seen.contains(&server.name.as_str()) {
                return Err(invalid(
                    "mcp.servers.name",
                    format!("duplicate MCP server {:?}", server.name),
                ));
            }
            seen.push(&server.name);
            if server.command.trim().is_empty() {
                return Err(invalid(
                    "mcp.servers.command",
                    format!("server {:?}: command must not be empty", server.name),
                ));
            }
            if let Some(timeout) = server.timeout {
                if timeout.as_millis() == 0 {
                    return Err(invalid(
                        "mcp.servers.timeout",
                        format!("server {:?}: timeout must be positive", server.name),
                    ));
                }
            }
            for key in server.env.keys() {
                if key.trim().is_empty() {
                    return Err(invalid(
                        "mcp.servers.env",
                        format!("server {:?}: env keys must not be empty", server.name),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// `schedule:` block — exactly one of `every | cron | at`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScheduleConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub every: Option<Duration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_if_running: Option<bool>,
    /// Consecutive failed runs before the schedule backs off (exponential).
    /// `0` disables backoff; unset defaults to 3 (SPEC.md §13).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backoff_after_failures: Option<u32>,
}

impl ScheduleConfig {
    /// Consecutive failed runs before exponential backoff (default 3,
    /// `0` disables).
    pub fn backoff_after_failures(&self) -> u32 {
        self.backoff_after_failures.unwrap_or(3)
    }
}

/// `runtime:` block (agent-level knobs, `SPEC.md §9`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentRuntime {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint_interval: Option<Duration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_history_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent_tools: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint_retention: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_recover: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_retries: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_timeout: Option<Duration>,
    /// Address-space cap for every tool process, in MiB. Enforced
    /// as `RLIMIT_AS` by whichever sandbox backend is active (bwrap, landlock,
    /// or the rlimit fallback). Absent = no memory cap (the default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_memory_limit_mb: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_tool_args: Option<bool>,
    /// Wall-clock cap for one execution (0 = unbounded, the default).
    /// Anchored to the execution's `started_at` and enforced between steps
    /// and while parked for approval — a run can never exceed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_duration: Option<Duration>,
    /// Per-execution tool-call budget (0 = unbounded, the default). Counts
    /// every fresh call the model issues across turns AND restarts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    /// How long an `ask` permission request stays decidable before the
    /// engine expires it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_timeout: Option<Duration>,
    /// Durable-sleep threshold: a `sleep` call at or above this
    /// duration parks the agent (`sleeping`, runner unloaded, wake_at
    /// persisted) instead of blocking the runner in-process. Shorter sleeps
    /// stay in-runner. Default 10s.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub durable_sleep_min: Option<Duration>,
    /// Stream model output: text fragments are broadcast live as transient
    /// `model.delta` events (SSE) while the turn itself is recorded normally.
    /// Default false — v0.1 turn semantics, no provider stream dependency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Compact the transcript once the estimated prompt exceeds this many
    /// tokens (ARCHITECTURE.md §7.5). Unset/`0` disables compaction, leaving
    /// the trim-only behavior.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact_above_tokens: Option<u64>,
    /// Messages kept verbatim at the tail when compacting (default 6).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact_keep_recent: Option<u32>,
    /// Budget for one `agent.<name>` delegation (default 300s). A delegation
    /// runs a whole child execution, so it is NOT bounded by `tool_timeout`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_timeout: Option<Duration>,
}

impl AgentRuntime {
    /// Defaults per `SPEC.md §9` (used when the field is absent).
    pub fn checkpoint_interval(&self) -> Duration {
        self.checkpoint_interval
            .unwrap_or_else(|| Duration::parse("30s").expect("const"))
    }

    pub fn max_steps(&self) -> u32 {
        self.max_steps.unwrap_or(100)
    }

    pub fn max_history_tokens(&self) -> u64 {
        self.max_history_tokens.unwrap_or(16_384)
    }

    pub fn max_concurrent_tools(&self) -> u32 {
        self.max_concurrent_tools.unwrap_or(4)
    }

    pub fn checkpoint_retention(&self) -> u32 {
        self.checkpoint_retention.unwrap_or(50)
    }

    pub fn auto_recover(&self) -> bool {
        self.auto_recover.unwrap_or(true)
    }

    pub fn model_retries(&self) -> u32 {
        self.model_retries.unwrap_or(2)
    }

    /// Approval window for `ask` requests (default 300 s per SPEC §10).
    /// After it, the engine seals the request `expired` and the agent is
    /// told the permission was denied — no waiting agent parks forever.
    pub fn ask_timeout(&self) -> Duration {
        self.ask_timeout
            .unwrap_or_else(|| Duration::parse("300s").expect("const"))
    }

    pub fn tool_timeout(&self) -> Duration {
        self.tool_timeout
            .unwrap_or_else(|| Duration::parse("30s").expect("const"))
    }

    /// The tool address-space cap in bytes (`None` = unlimited, the default).
    pub fn tool_memory_limit_bytes(&self) -> Option<u64> {
        self.tool_memory_limit_mb
            .map(|mb| u64::from(mb) * 1024 * 1024)
    }

    /// Durable-sleep threshold (default 10s). Sleeps at or above this park
    /// the agent; shorter sleeps run inside the runner.
    pub fn durable_sleep_min(&self) -> Duration {
        self.durable_sleep_min
            .unwrap_or_else(|| Duration::parse("10s").expect("const"))
    }

    pub fn log_tool_args(&self) -> bool {
        self.log_tool_args.unwrap_or(false)
    }

    /// Whether to stream model output (default false).
    pub fn stream(&self) -> bool {
        self.stream.unwrap_or(false)
    }

    /// Wall-clock cap for one execution (`0`/unset = unbounded).
    pub fn max_duration(&self) -> Option<Duration> {
        self.max_duration.filter(|d| d.as_millis() > 0)
    }

    /// Per-execution tool-call budget (`0`/unset = unbounded).
    pub fn max_tool_calls(&self) -> Option<u32> {
        self.max_tool_calls.filter(|n| *n > 0)
    }

    /// Compact the transcript above this estimated prompt size (`0`/unset =
    /// disabled).
    pub fn compact_above_tokens(&self) -> Option<u64> {
        self.compact_above_tokens.filter(|n| *n > 0)
    }

    /// Messages kept verbatim at the tail when compacting (default 6).
    pub fn compact_keep_recent(&self) -> u32 {
        self.compact_keep_recent.unwrap_or(6).max(2)
    }

    /// Budget for one sub-agent delegation (default 300s).
    pub fn subagent_timeout(&self) -> Duration {
        self.subagent_timeout
            .unwrap_or_else(|| Duration::parse("300s").expect("const"))
    }
}

/// Builtin tool names in v0.1 (custom registered tools
/// will be validated against the registry once it exists).
pub const KNOWN_TOOLS: &[&str] = &[
    "filesystem",
    "http",
    "shell",
    "noop",
    "sleep",
    "memory.read",
    "memory.write",
    "memory.list",
];

// ---------------------------------------------------------------------------
// Parsing and validation
// ---------------------------------------------------------------------------

/// Parse and fully validate an `agent.yaml` (schema v1).
///
/// Any failure returns `CONFIG_INVALID` with a message naming the offending
/// field and, where `serde_yaml` provides it, a line/column in `detail`.
pub fn parse_agent_spec(yaml: &str) -> Result<AgentSpec> {
    let spec =
        serde_path_to_error::deserialize::<_, AgentSpec>(serde_yaml::Deserializer::from_str(yaml))
            .map_err(|err| {
                let field = err.path().to_string();
                let mut detail = serde_json::Map::new();
                detail.insert("field".to_string(), Value::String(field));
                if let Some(loc) = err.inner().location() {
                    detail.insert("line".to_string(), Value::from(loc.line()));
                    detail.insert("column".to_string(), Value::from(loc.column()));
                }
                KernError::new(ErrorCode::ConfigInvalid, err.inner().to_string())
                    .with_detail(Value::Object(detail))
            })?;
    spec.validate()?;
    Ok(spec)
}

impl AgentSpec {
    /// Semantic validation of a structurally valid spec (rules serde cannot
    /// express). Errors name the field in the message.
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            return Err(invalid(
                "version",
                format!("unsupported spec version {}", self.version),
            ));
        }
        if !valid_slug(&self.name) {
            return Err(invalid(
                "name",
                format!(
                    "invalid agent name {:?}: must match [a-z0-9][a-z0-9-_]*",
                    self.name
                ),
            ));
        }
        if self.tools.is_empty() {
            return Err(invalid("tools", "at least one tool is required"));
        }
        if !(0.0..=2.0).contains(&self.model.temperature) {
            return Err(invalid("model.temperature", "must be between 0 and 2"));
        }
        if let Some(m) = self.model.max_tokens {
            if m == 0 {
                return Err(invalid("model.max_tokens", "must be positive"));
            }
        }
        if let Some(url) = &self.model.base_url {
            if let Some(rest) = url.strip_prefix("env:") {
                validate_env_ref_name("model.base_url", rest)?;
            } else if url.trim().is_empty() {
                return Err(invalid("model.base_url", "must not be empty"));
            }
        }
        if self.model.model.trim().is_empty() {
            return Err(invalid("model.model", "must not be empty"));
        }
        for (index, rule) in self.model.routing.iter().enumerate() {
            let field = format!("model.routing[{index}]");
            if rule.model.trim().is_empty() {
                return Err(invalid(&field, "model must not be empty"));
            }
            if let Some(max) = rule.max_tokens {
                if max == 0 {
                    return Err(invalid(&field, "max_tokens must be positive"));
                }
            }
            if let Some(t) = rule.temperature {
                if !(0.0..=2.0).contains(&t) {
                    return Err(invalid(&field, "temperature must be between 0 and 2"));
                }
            }
            if rule.if_step_above.is_none() && rule.if_tokens_above.is_none() {
                // Allowed, but only as the LAST rule: an unconditional rule
                // anywhere else would make every later rule unreachable.
                if index + 1 != self.model.routing.len() {
                    return Err(invalid(
                        &field,
                        "a rule with no conditions must be the last rule",
                    ));
                }
            }
        }

        // Tools: known builtin names, or `<server>.<tool>` for a configured
        // MCP server; tool class gates from §9.
        self.validate_subagents()?;

        for tool in &self.tools {
            // Sub-agent tools were validated (and their targets exist) above.
            if tool.starts_with("agent.") {
                continue;
            }
            if KNOWN_TOOLS.contains(&tool.as_str()) {
                if tool.starts_with("memory.") && !self.memory.enabled {
                    return Err(invalid(
                        "tools",
                        format!("tool {tool:?} requires memory.enabled: true"),
                    ));
                }
                if tool == "shell" && !self.permissions.shell_enabled() {
                    return Err(invalid(
                        "tools",
                        "the shell tool must not be exposed unless permissions.shell.enabled: true",
                    ));
                }
                continue;
            }
            if !self.mcp.is_mcp_tool(tool) {
                return Err(invalid(
                    "tools",
                    format!(
                        "unknown tool {tool:?} (known: {}; MCP tools are \
                         \"<server>.<tool>\" for a server in `mcp.servers`)",
                        KNOWN_TOOLS.join(", ")
                    ),
                ));
            }
            if !self.permissions.mcp_permits(tool) {
                return Err(invalid(
                    "tools",
                    format!("MCP tool {tool:?} must be allowed or asked in permissions.mcp"),
                ));
            }
        }
        self.validate_credentials()?;

        self.mcp.validate()?;

        self.memory.validate()?;
        self.permissions.validate()?;
        if let Some(schedule) = &self.schedule {
            schedule.validate()?;
        }
        self.runtime.validate()
    }
}

impl AgentSpec {
    /// Sub-agent definitions (§35): unique slugs, subset capability sets, and
    /// no nesting.
    fn validate_subagents(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for tool in &self.tools {
            if let Some(name) = tool.strip_prefix("agent.") {
                if !self.subagents.iter().any(|s| s.name == name) {
                    return Err(invalid(
                        "tools",
                        format!(
                            "tool {tool:?} names a sub-agent that is not declared in `subagents`"
                        ),
                    ));
                }
            }
        }
        for (index, sub) in self.subagents.iter().enumerate() {
            let field = format!("subagents[{index}]");
            if !valid_slug(&sub.name) {
                return Err(invalid(
                    &field,
                    format!(
                        "invalid sub-agent name {:?}: must match [a-z0-9][a-z0-9-_]*",
                        sub.name
                    ),
                ));
            }
            if !seen.insert(sub.name.as_str()) {
                return Err(invalid(
                    &field,
                    format!("duplicate sub-agent name {:?}", sub.name),
                ));
            }
            if sub.description.trim().is_empty() {
                return Err(invalid(&field, "description must not be empty"));
            }
            if sub.tools.is_empty() {
                return Err(invalid(&field, "at least one tool is required"));
            }
            if sub.max_steps == Some(0) {
                return Err(invalid(&field, "max_steps must be positive"));
            }
            if let Some(d) = sub.max_duration {
                if d.as_millis() == 0 {
                    return Err(invalid(&field, "max_duration must be positive"));
                }
            }
            if let Some(model) = &sub.model {
                if model.model.trim().is_empty() {
                    return Err(invalid(&field, "model.model must not be empty"));
                }
                if !(0.0..=2.0).contains(&model.temperature) {
                    return Err(invalid(&field, "model.temperature must be between 0 and 2"));
                }
                if model.max_tokens == Some(0) {
                    return Err(invalid(&field, "model.max_tokens must be positive"));
                }
            }
            for tool in &sub.tools {
                if tool.starts_with("agent.") {
                    return Err(invalid(
                        &field,
                        "sub-agents cannot declare sub-agents (nesting is not supported)",
                    ));
                }
                if !self.tools.contains(tool) {
                    return Err(invalid(
                        &field,
                        format!(
                            "tool {tool:?} must also be in the parent's `tools` \
                             (a sub-agent cannot exceed its parent's capabilities)"
                        ),
                    ));
                }
                if tool.starts_with("memory.") && !self.memory.enabled {
                    return Err(invalid(
                        &field,
                        format!("tool {tool:?} requires memory.enabled: true"),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Credential grants: names, sources, and targets (§33).
    fn validate_credentials(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for (index, cred) in self.credentials.iter().enumerate() {
            let field = format!("credentials[{index}]");
            if cred.name.trim().is_empty() {
                return Err(invalid(&field, "name must not be empty"));
            }
            if !seen.insert(cred.name.as_str()) {
                return Err(invalid(
                    &field,
                    format!("duplicate credential name {:?}", cred.name),
                ));
            }
            let targets = cred.targets()?;
            if let Some(rest) = cred.from.strip_prefix("env:") {
                validate_env_ref_name(&field, rest)?;
            } else if cred.from.is_empty() {
                return Err(invalid(&field, "from must not be empty"));
            }
            let has_http = targets.contains(&CredentialTarget::Http);
            if cred.header.is_some() && !has_http {
                return Err(invalid(&field, "header requires \"http\" in tools"));
            }
            if cred.prefix.is_some() && !has_http {
                return Err(invalid(&field, "prefix requires \"http\" in tools"));
            }
            // Env-style targets must be valid variable names, so a grant cannot
            // smuggle an assignment or a newline into a child environment.
            for target in &targets {
                if matches!(target, CredentialTarget::Shell | CredentialTarget::Mcp) {
                    let valid = !cred.name.is_empty()
                        && cred
                            .name
                            .chars()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                        && !cred.name.starts_with(|c: char| c.is_ascii_digit());
                    if !valid {
                        return Err(invalid(
                            &field,
                            format!(
                                "name {:?} must be an uppercase environment variable name",
                                cred.name
                            ),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Resolve every credential's value from the daemon environment (§33).
    ///
    /// Fails closed: an unset `env:` source fails the agent's start rather than
    /// injecting an empty secret or silently skipping the grant.
    pub fn resolve_credentials(&self) -> Result<Vec<ResolvedCredential>> {
        let mut out = Vec::with_capacity(self.credentials.len());
        for cred in &self.credentials {
            let value = resolve_env_ref(&cred.from)?;
            out.push(ResolvedCredential {
                name: cred.name.clone(),
                value,
                targets: cred.targets()?,
                header: cred.header.clone(),
                prefix: cred.prefix.clone(),
            });
        }
        Ok(out)
    }
}

impl MemoryConfig {
    fn validate(&self) -> Result<()> {
        if let Some(dir) = &self.dir {
            if dir.trim().is_empty() {
                return Err(invalid("memory.dir", "must not be empty"));
            }
            let path = std::path::Path::new(dir);
            if path.is_absolute() {
                return Err(invalid(
                    "memory.dir",
                    "must be relative to the agent workspace",
                ));
            }
            if path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(invalid(
                    "memory.dir",
                    "must not contain '..' (it stays inside the agent workspace)",
                ));
            }
        }
        for (field, value) in [
            ("memory.max_keys", self.max_keys),
            ("memory.max_value_bytes", self.max_value_bytes),
        ] {
            if let Some(v) = value {
                if v == 0 {
                    return Err(invalid(field, "must be positive"));
                }
            }
        }
        Ok(())
    }
}

impl PermissionsConfig {
    /// The shell tool is exposed only when `permissions.shell.enabled` is true.
    pub fn shell_enabled(&self) -> bool {
        self.shell.as_ref().map(|s| s.enabled).unwrap_or(false)
    }

    /// Whether an MCP tool name is allowed or asks under `permissions.mcp`.
    /// `deny` (and the absence of a matching rule) default-denies.
    pub fn mcp_permits(&self, tool: &str) -> bool {
        let Some(rules) = &self.mcp else {
            return false;
        };
        let matches = |patterns: &Option<Vec<String>>| {
            patterns.iter().flatten().any(|pattern| {
                glob::Pattern::new(pattern)
                    .map(|p| p.matches(tool))
                    .unwrap_or(false)
            })
        };
        if matches(&rules.deny) {
            return false;
        }
        matches(&rules.allow) || matches(&rules.ask)
    }

    fn validate(&self) -> Result<()> {
        if let Some(fs) = &self.filesystem {
            for (field, rules) in [
                ("permissions.filesystem.read", fs.read.as_ref()),
                ("permissions.filesystem.write", fs.write.as_ref()),
            ] {
                if let Some(rules) = rules {
                    for p in rules.entries() {
                        if p.trim().is_empty() {
                            return Err(invalid(field, "paths must not be empty"));
                        }
                    }
                }
            }
        }
        if let Some(net) = &self.network {
            for (field, hosts) in [
                ("permissions.network.allow", net.allow.as_ref()),
                ("permissions.network.ask", net.ask.as_ref()),
                ("permissions.network.deny", net.deny.as_ref()),
            ] {
                if let Some(hosts) = hosts {
                    for h in hosts {
                        if h.trim().is_empty() || h.contains('/') || h.contains(char::is_whitespace)
                        {
                            return Err(invalid(
                                field,
                                format!("invalid host entry {h:?} (expected host or host:port)"),
                            ));
                        }
                    }
                }
            }
        }
        if let Some(mem) = &self.memory {
            for (field, rules) in [
                ("permissions.memory.read", mem.read.as_ref()),
                ("permissions.memory.write", mem.write.as_ref()),
            ] {
                if let Some(rules) = rules {
                    for g in rules.entries() {
                        if g.is_empty() {
                            return Err(invalid(field, "key globs must not be empty"));
                        }
                    }
                }
            }
        }
        if let Some(shell) = &self.shell {
            if shell.enabled && shell.sandbox.is_none() {
                return Err(invalid(
                    "permissions.shell.sandbox",
                    "required when permissions.shell.enabled is true (required | best-effort | off)",
                ));
            }
        }
        if let Some(mcp) = &self.mcp {
            for pattern in mcp.entries() {
                if pattern.trim().is_empty() {
                    return Err(invalid(
                        "permissions.mcp",
                        "tool patterns must not be empty",
                    ));
                }
                if glob::Pattern::new(pattern).is_err() {
                    return Err(invalid(
                        "permissions.mcp",
                        format!("invalid tool pattern {pattern:?}"),
                    ));
                }
            }
        }
        Ok(())
    }
}

impl ScheduleConfig {
    fn validate(&self) -> Result<()> {
        let present = [self.every.is_some(), self.cron.is_some(), self.at.is_some()]
            .into_iter()
            .filter(|p| *p)
            .count();
        match present {
            0 => {
                return Err(invalid(
                    "schedule",
                    "exactly one of every|cron|at is required",
                ))
            }
            1 => {}
            _ => {
                return Err(invalid(
                    "schedule",
                    "exactly one of every|cron|at is required (found multiple)",
                ))
            }
        }
        if let Some(every) = self.every {
            if every.as_millis() == 0 {
                return Err(invalid("schedule.every", "must be greater than zero"));
            }
        }
        if let Some(cron) = &self.cron {
            validate_cron(cron).map_err(|e| invalid("schedule.cron", e))?;
        }
        if let Some(at) = &self.at {
            if *at <= Utc::now() {
                return Err(invalid("schedule.at", "must be in the future"));
            }
        }
        if let Some(tz) = &self.timezone {
            if tz.trim().is_empty() {
                return Err(invalid("schedule.timezone", "must not be empty"));
            }
        }
        Ok(())
    }
}

impl AgentRuntime {
    fn validate(&self) -> Result<()> {
        for (field, value) in [
            ("runtime.max_steps", self.max_steps.map(u64::from)),
            ("runtime.max_history_tokens", self.max_history_tokens),
            (
                "runtime.max_concurrent_tools",
                self.max_concurrent_tools.map(u64::from),
            ),
            (
                "runtime.checkpoint_retention",
                self.checkpoint_retention.map(u64::from),
            ),
            ("runtime.max_tool_calls", self.max_tool_calls.map(u64::from)),
            (
                "runtime.tool_memory_limit_mb",
                self.tool_memory_limit_mb.map(u64::from),
            ),
        ] {
            if let Some(v) = value {
                if v == 0 {
                    return Err(invalid(field, "must be positive"));
                }
            }
        }
        Ok(())
    }
}

fn valid_slug(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn invalid(field: &str, message: impl Into<String>) -> KernError {
    let mut detail = serde_json::Map::new();
    detail.insert("field".to_string(), Value::String(field.to_string()));
    // The message is prefixed with the field so it stands alone in logs/CLI
    // output; `detail.field` carries it machine-readably.
    let full = format!("{field}: {}", message.into());
    KernError::new(ErrorCode::ConfigInvalid, full).with_detail(Value::Object(detail))
}

fn validate_env_ref_name(field: &str, var: &str) -> Result<()> {
    if var.is_empty() || !var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(invalid(field, format!("invalid env reference env:{var}")));
    }
    Ok(())
}

/// Validate a cron expression (`SPEC.md §9`).
///
/// The spec's canonical form is standard 5-field cron (`minute hour day-of-month
/// month day-of-week`, e.g. `"0 3 * * *"`); 6/7-field forms (seconds-first,
/// optional year) and the `@daily`-style shorthands are also accepted. The
/// `cron` crate requires the seconds field, so 5-field expressions are
/// normalized to seconds=0 before validation — standard cron semantics.
fn validate_cron(expr: &str) -> std::result::Result<(), String> {
    let normalized = if expr.trim_start().starts_with('@') {
        expr.to_string()
    } else {
        match expr.split_whitespace().count() {
            5 => format!("0 {expr}"),
            _ => expr.to_string(),
        }
    };
    cron::Schedule::from_str(&normalized)
        .map(|_| ())
        .map_err(|e| format!("invalid cron expression {expr:?}: {e}"))
}

/// Resolve a config value that may be an `env:VAR` reference.
///
/// Called at *use* time (e.g. when the model gateway builds a request), never
/// at parse time, so resolved secret values are never persisted or logged.
/// A missing variable is a `CONFIG_INVALID` error naming the variable.
pub fn resolve_env_ref(value: &str) -> Result<String> {
    match value.strip_prefix("env:") {
        Some(var) => {
            validate_env_ref_name("", var)?;
            std::env::var(var).map_err(|_| {
                invalid(
                    "env",
                    format!("environment variable {var} (referenced by env:{var}) is not set"),
                )
            })
        }
        None => Ok(value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;
    use std::sync::Mutex;

    // Env mutation is not thread-safe; serialize env-touching tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const VALID_SPEC: &str = r#"
version: 1
name: researcher
description: "Research assistant"

model:
  provider: openai
  model: gpt-4o-mini
  temperature: 0.2
  max_tokens: 2048
  timeout: 60s

tools:
  - filesystem
  - http

memory:
  enabled: false

permissions:
  filesystem:
    read:
      allow: [./workspace]
      ask: [./shared/**]
      deny: [./workspace/secret/**]
    write:
      allow: [./workspace]
  network:
    allow: [api.github.com]
    deny: [api.github.com]

schedule:
  every: 12h
  skip_if_running: true

runtime:
  checkpoint_interval: 30s
  max_steps: 100
"#;

    #[test]
    fn golden_spec_parses_and_round_trips() {
        let spec = parse_agent_spec(VALID_SPEC).expect("valid spec must parse");
        assert_eq!(spec.version, 1);
        assert_eq!(spec.name, "researcher");
        assert_eq!(spec.model.provider, Provider::OpenAI);
        assert_eq!(spec.model.model, "gpt-4o-mini");
        assert_eq!(spec.model.timeout.unwrap().as_millis(), 60_000);
        assert_eq!(spec.tools, vec!["filesystem", "http"]);
        let fs = spec.permissions.filesystem.as_ref().unwrap();
        assert_eq!(
            fs.read.as_ref().unwrap().allow.as_deref().unwrap(),
            ["./workspace"]
        );
        assert_eq!(
            fs.read.as_ref().unwrap().ask.as_deref().unwrap(),
            ["./shared/**"]
        );
        assert_eq!(
            fs.read.as_ref().unwrap().deny.as_deref().unwrap(),
            ["./workspace/secret/**"]
        );
        assert_eq!(
            fs.write.as_ref().unwrap().allow.as_deref().unwrap(),
            ["./workspace"]
        );
        let net = spec.permissions.network.as_ref().unwrap();
        assert_eq!(net.allow.as_deref().unwrap(), ["api.github.com"]);
        assert_eq!(net.deny.as_deref().unwrap(), ["api.github.com"]);
        assert_eq!(
            spec.schedule.as_ref().unwrap().every.unwrap().as_millis(),
            12 * 3_600_000
        );

        // YAML re-serialization round-trips to the same typed value.
        let out = serde_yaml::to_string(&spec).unwrap();
        let again = parse_agent_spec(&out).expect("serialized spec must re-parse");
        assert_eq!(again, spec);
    }

    #[test]
    fn defaults_apply_when_blocks_absent() {
        let yaml = r#"
version: 1
name: minimal
model:
  provider: mock
  model: test
tools:
  - noop
"#;
        let spec = parse_agent_spec(yaml).unwrap();
        assert_eq!(spec.memory, MemoryConfig::default());
        assert_eq!(spec.permissions, PermissionsConfig::default());
        assert_eq!(spec.schedule, None);
        assert_eq!(spec.runtime.checkpoint_interval().as_str(), "30s");
        assert_eq!(spec.runtime.max_steps(), 100);
        assert_eq!(spec.runtime.tool_timeout().as_str(), "30s");
        assert!(!spec.runtime.log_tool_args());
    }

    #[test]
    fn duration_parsing_and_canonical_form() {
        assert_eq!(Duration::parse("30s").unwrap().as_str(), "30s");
        assert_eq!(Duration::parse("500ms").unwrap().as_str(), "500ms");
        assert_eq!(Duration::parse("12h").unwrap().as_str(), "12h");
        assert_eq!(Duration::parse("2d").unwrap().as_str(), "2d");
        assert_eq!(Duration::parse("90s").unwrap().as_millis(), 90_000);
        assert_eq!(
            Duration::parse("1.5s").unwrap_err(),
            "invalid duration \"1.5s\": expected a positive integer before the unit"
        );
        for bad in ["30", "30x", "-5s", "s", "", "1h30m"] {
            assert!(Duration::parse(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(Duration::parse("0s").is_err());
    }

    #[test]
    fn unknown_key_fails_with_field_path() {
        let yaml = format!("{VALID_SPEC}bogus_key: 1\n");
        let err = parse_agent_spec(&yaml).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(
            err.message.contains("bogus_key"),
            "message must name the field: {}",
            err.message
        );
    }

    #[test]
    fn unknown_key_in_nested_block_fails() {
        let yaml = VALID_SPEC.replace("  timeout: 60s", "  timeout: 60s\n  bogus: true");
        let err = parse_agent_spec(&yaml).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("bogus"), "{}", err.message);
    }

    #[test]
    fn unknown_key_in_rule_list_fails() {
        let yaml = VALID_SPEC.replace(
            "      allow: [./workspace]\n      ask: [./shared/**]",
            "      allow: [./workspace]\n      ask: [./shared/**]\n      maybe: [./x]",
        );
        let err = parse_agent_spec(&yaml).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("maybe"), "{}", err.message);
    }

    #[test]
    fn empty_rule_entries_fail() {
        let yaml = VALID_SPEC.replace(
            "      deny: [./workspace/secret/**]",
            "      deny: [\"  \"]",
        );
        let err = parse_agent_spec(&yaml).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("filesystem.read"), "{}", err.message);
    }

    #[test]
    fn bad_provider_fails() {
        let yaml = VALID_SPEC.replace("provider: openai", "provider: watson");
        let err = parse_agent_spec(&yaml).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("watson"), "{}", err.message);
    }

    #[test]
    fn missing_name_fails() {
        let yaml = VALID_SPEC.replace("name: researcher\n", "");
        let err = parse_agent_spec(&yaml).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
    }

    #[test]
    fn bad_slug_fails() {
        // `9start` is valid: the spec slug allows a leading digit.
        for bad in ["My Agent", "has space", "UPPER", "", "-dash", "_underscore"] {
            let yaml = VALID_SPEC.replace("name: researcher", &format!("name: {bad}"));
            let err = parse_agent_spec(&yaml).unwrap_err();
            assert_eq!(
                err.code(),
                ErrorCode::ConfigInvalid,
                "{bad:?} must be rejected"
            );
            assert!(err.message.contains("name"), "{}", err.message);
        }
    }

    #[test]
    fn bad_duration_in_config_fails() {
        let yaml = VALID_SPEC.replace("timeout: 60s", "timeout: 60");
        let err = parse_agent_spec(&yaml).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("duration"), "{}", err.message);
    }

    #[test]
    fn unsupported_version_fails() {
        let yaml = VALID_SPEC.replace("version: 1", "version: 2");
        let err = parse_agent_spec(&yaml).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("version"), "{}", err.message);
    }

    #[test]
    fn empty_tools_fails() {
        let yaml = VALID_SPEC.replace("  - filesystem\n  - http\n", "");
        let err = parse_agent_spec(&yaml).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("tools"), "{}", err.message);
    }

    #[test]
    fn unknown_tool_fails() {
        let yaml = VALID_SPEC.replace("  - http\n", "  - rocket\n");
        let err = parse_agent_spec(&yaml).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("rocket"), "{}", err.message);
    }

    #[test]
    fn shell_tool_requires_shell_permission() {
        let yaml = VALID_SPEC.replace("  - http\n", "  - shell\n");
        let err = parse_agent_spec(&yaml).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("shell"), "{}", err.message);
    }

    // ------------------------------------------------------------------
    // MCP servers and tools
    // ------------------------------------------------------------------

    fn mcp_yaml(tools: &str, permissions: &str, extra_servers: &str) -> String {
        format!(
            "version: 1\nname: mcp-agent\nmodel:\n  provider: mock\n  model: test\n\
             mcp:\n  servers:\n    - name: fs\n      command: npx\n      args: [\"-y\", \"srv\"]\n\
             {extra_servers}tools: [{tools}]\npermissions:\n{permissions}\n"
        )
    }

    #[test]
    fn mcp_block_parses_and_names_tools() {
        let spec = parse_agent_spec(&mcp_yaml(
            "fs.read_file",
            "  mcp:\n    allow: [\"fs.*\"]\n    ask: [\"fs.write_file\"]\n",
            "",
        ))
        .expect("valid MCP config must parse");

        assert_eq!(spec.mcp.servers.len(), 1);
        let server = &spec.mcp.servers[0];
        assert_eq!(server.name, "fs");
        assert_eq!(server.command, "npx");
        assert_eq!(server.args, vec!["-y".to_string(), "srv".to_string()]);
        assert!(server.timeout.is_none(), "timeout defaults when absent");
        assert!(spec.mcp.is_mcp_tool("fs.read_file"));
        assert!(!spec.mcp.is_mcp_tool("memory.read"), "builtins are not MCP");
        assert!(spec.permissions.mcp_permits("fs.write_file"), "ask permits");

        // Round-trips through serde (the store keeps configs as JSON).
        let json = serde_json::to_string(&spec).unwrap();
        let back: AgentSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn mcp_tool_requires_a_permission_rule() {
        let err = parse_agent_spec(&mcp_yaml(
            "fs.read_file",
            "  mcp:\n    deny: [\"fs.*\"]\n",
            "",
        ))
        .unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("permissions.mcp"), "{}", err.message);
    }

    #[test]
    fn mcp_tool_requires_a_configured_server() {
        let err = parse_agent_spec(&mcp_yaml(
            "ghost.read_file",
            "  mcp:\n    allow: [\"*\"]\n",
            "",
        ))
        .unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("unknown tool"), "{}", err.message);
    }

    #[test]
    fn duplicate_mcp_servers_are_rejected() {
        let duplicate = "    - name: fs\n      command: other\n";
        let err = parse_agent_spec(&mcp_yaml(
            "fs.read_file",
            "  mcp:\n    allow: [\"fs.*\"]\n",
            duplicate,
        ))
        .unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("duplicate"), "{}", err.message);
    }

    #[test]
    fn invalid_mcp_glob_is_rejected() {
        // The first pattern covers the tool; the second is a malformed glob
        // that must still fail validation rather than silently never match.
        let err = parse_agent_spec(&mcp_yaml(
            "fs.read_file",
            "  mcp:\n    allow: [\"fs.*\", \"fs.[\"]\n",
            "",
        ))
        .unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(
            err.message.contains("invalid tool pattern"),
            "{}",
            err.message
        );
    }

    #[test]
    fn shell_enabled_requires_sandbox_field() {
        let yaml = r#"
version: 1
name: risky
model:
  provider: mock
  model: test
tools:
  - shell
permissions:
  shell:
    enabled: true
"#;
        let err = parse_agent_spec(yaml).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("sandbox"), "{}", err.message);
    }

    #[test]
    fn shell_enabled_with_sandbox_ok() {
        let yaml = r#"
version: 1
name: shellbox
model:
  provider: mock
  model: test
tools:
  - shell
permissions:
  shell:
    enabled: true
    sandbox: required
"#;
        let spec = parse_agent_spec(yaml).unwrap();
        assert!(spec.permissions.shell_enabled());
        assert_eq!(
            spec.permissions.shell.as_ref().unwrap().sandbox,
            Some(SandboxMode::Required)
        );
    }

    #[test]
    fn memory_tool_requires_memory_enabled() {
        let yaml = VALID_SPEC.replace("  - http\n", "  - memory.read\n");
        let err = parse_agent_spec(&yaml).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("memory.enabled"), "{}", err.message);
    }

    #[test]
    fn invalid_cron_fails() {
        let yaml = VALID_SPEC.replace("every: 12h", "cron: \"0 99 * * *\"");
        let err = parse_agent_spec(&yaml).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("cron"), "{}", err.message);
    }

    #[test]
    fn valid_cron_parses() {
        // SPEC canonical 5-field form (minute hour dom month dow).
        let yaml = VALID_SPEC.replace("every: 12h", "cron: \"0 3 * * *\"");
        assert!(parse_agent_spec(&yaml).is_ok(), "5-field cron must parse");
        // Seconds-prefixed and shorthand forms are accepted too.
        let six = VALID_SPEC.replace("every: 12h", "cron: \"0 0 3 * * *\"");
        assert!(parse_agent_spec(&six).is_ok(), "6-field cron must parse");
        let shorthand = VALID_SPEC.replace("every: 12h", "cron: \"@daily\"");
        assert!(
            parse_agent_spec(&shorthand).is_ok(),
            "@daily shorthand must parse"
        );
    }

    #[test]
    fn cron_wrong_field_count_fails() {
        let yaml = VALID_SPEC.replace("every: 12h", "cron: \"* *\"");
        let err = parse_agent_spec(&yaml).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("cron"), "{}", err.message);
    }

    #[test]
    fn multiple_schedule_kinds_fail() {
        let yaml = VALID_SPEC.replace("every: 12h", "every: 12h\n  cron: \"0 3 * * *\"");
        let err = parse_agent_spec(&yaml).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("schedule"), "{}", err.message);
    }

    #[test]
    fn schedule_without_kind_fails() {
        let yaml = r#"
version: 1
name: nosched
model:
  provider: mock
  model: test
tools:
  - noop
schedule:
  timezone: UTC
"#;
        let err = parse_agent_spec(yaml).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("schedule"), "{}", err.message);
    }

    #[test]
    fn schedule_at_in_past_fails() {
        let past = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let yaml = VALID_SPEC.replace("every: 12h", &format!("at: \"{past}\""));
        let err = parse_agent_spec(&yaml).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("future"), "{}", err.message);
    }

    #[test]
    fn zero_runtime_knobs_fail() {
        let yaml = VALID_SPEC.replace("  max_steps: 100", "  max_steps: 0");
        let err = parse_agent_spec(&yaml).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
    }

    #[test]
    fn env_references_are_not_resolved_or_persisted() {
        let _guard = ENV_LOCK.lock().unwrap();
        let yaml = r#"
version: 1
name: envref
model:
  provider: openai
  model: gpt-4o-mini
  base_url: env:OPENAI_BASE_URL
tools:
  - noop
"#;
        std::env::set_var("OPENAI_BASE_URL", "https://secret-internal.example.com");
        let spec = parse_agent_spec(yaml).unwrap();
        // The literal reference is kept; the resolved value never appears.
        assert_eq!(spec.model.base_url.as_deref(), Some("env:OPENAI_BASE_URL"));
        let serialized = serde_yaml::to_string(&spec).unwrap();
        assert!(
            !serialized.contains("secret-internal.example.com"),
            "resolved secret leaked into serialized config: {serialized}"
        );
        assert!(serialized.contains("env:OPENAI_BASE_URL"));

        // Use-time resolution works.
        assert_eq!(
            resolve_env_ref("env:OPENAI_BASE_URL").unwrap(),
            "https://secret-internal.example.com"
        );
        assert_eq!(resolve_env_ref("plain-value").unwrap(), "plain-value");
        std::env::remove_var("OPENAI_BASE_URL");

        // Missing var at use time is a structured error.
        let err = resolve_env_ref("env:OPENAI_BASE_URL").unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
    }

    #[test]
    fn temperature_range_is_validated() {
        let yaml = VALID_SPEC.replace("temperature: 0.2", "temperature: 3.0");
        let err = parse_agent_spec(&yaml).unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(err.message.contains("temperature"), "{}", err.message);
    }
}
