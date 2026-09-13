//! The `[provider]`, `[context]`, `[agent]`, `[search]`, `[workers]`,
//! `[reflect]`, `[sandbox]` and `[files]` section structs. All fields have
//! serde defaults so a partial config file merges over [`Config::default`];
//! unknown keys are hard errors (`deny_unknown_fields`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{
    DEFAULT_BASE_URL, DEFAULT_MAX_PROMPT_TOKENS, DEFAULT_MAX_STEPS, DEFAULT_MAX_UPLOAD_MB,
    DEFAULT_MODEL, DEFAULT_REFLECT_TIME, DEFAULT_REFLECTOR_MAX_OUTPUT_TOKENS,
    DEFAULT_SANDBOX_MEMORY_MB, DEFAULT_SANDBOX_TIMEOUT_SECS, DEFAULT_SANDBOX_WORKSPACE,
    DEFAULT_SEARCH_RESULTS, DEFAULT_WORKER_MAX_OUTPUT_TOKENS,
};

fn default_max_prompt_tokens() -> u64 {
    DEFAULT_MAX_PROMPT_TOKENS
}

/// Deterministic context policy knobs (ADR-018, M2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextConfig {
    #[serde(default = "default_max_prompt_tokens")]
    pub max_prompt_tokens: u64,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_prompt_tokens: DEFAULT_MAX_PROMPT_TOKENS,
        }
    }
}

fn default_max_steps() -> u32 {
    DEFAULT_MAX_STEPS
}

/// Agent-loop knobs (M3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Tool-calling steps per turn before the loop stops (bounded loop, M3).
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps: DEFAULT_MAX_STEPS,
        }
    }
}

fn default_search_results() -> usize {
    DEFAULT_SEARCH_RESULTS
}

/// Web-search configuration (M3). `provider` selects the backend; an unknown
/// value is normalized to `off` by [`Config::validate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchConfig {
    /// `off` | `brave` | `tavily` | `searxng` (case-insensitive).
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub api_key: String,
    /// Self-hosted SearxNG endpoint (or a provider override base URL).
    #[serde(default)]
    pub base_url: String,
    #[serde(default = "default_search_results")]
    pub max_results: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            provider: "off".into(),
            api_key: String::new(),
            base_url: String::new(),
            max_results: DEFAULT_SEARCH_RESULTS,
        }
    }
}

impl SearchConfig {
    /// The normalized provider kind, or `Off` when unset/unknown.
    pub fn kind(&self) -> SearchProviderKind {
        SearchProviderKind::parse(&self.provider)
    }
}

/// Which web-search backend `[search] provider` selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchProviderKind {
    Off,
    Brave,
    Tavily,
    Searxng,
}

impl SearchProviderKind {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "brave" => Self::Brave,
            "tavily" => Self::Tavily,
            "searxng" | "searx" => Self::Searxng,
            _ => Self::Off,
        }
    }
}

fn default_max_output_tokens() -> u32 {
    DEFAULT_WORKER_MAX_OUTPUT_TOKENS
}

/// Evening-reflection configuration (M4.5, ADR-028). Enabling reflection is the
/// owner's standing consent for unattended, tool-free memory writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReflectConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Local time of the daily run, `HH:MM` (default 21:00).
    #[serde(default = "default_reflect_time")]
    pub time: String,
    /// Allow the reflector to revise `data/persona.md` a little when it judges
    /// the change helpful (M4.5). Every change is recorded as a memory note.
    #[serde(default = "default_persona_edits")]
    pub persona_edits: bool,
}

pub(super) fn default_reflect_time() -> String {
    DEFAULT_REFLECT_TIME.to_owned()
}

fn default_persona_edits() -> bool {
    true
}

impl Default for ReflectConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            time: default_reflect_time(),
            persona_edits: true,
        }
    }
}

impl ReflectConfig {
    /// The scheduled time as minutes since local midnight; an unparseable
    /// value falls back to the default (21:00).
    pub fn scheduled_minutes(&self) -> u32 {
        parse_hhmm(&self.time).unwrap_or_else(|| parse_hhmm(DEFAULT_REFLECT_TIME).unwrap_or(1260))
    }
}

/// Parse `HH:MM` into minutes since midnight; `None` when malformed or out of
/// range.
pub(super) fn parse_hhmm(value: &str) -> Option<u32> {
    let (hours, minutes) = value.trim().split_once(':')?;
    let hours: u32 = hours.trim().parse().ok()?;
    let minutes: u32 = minutes.trim().parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(hours * 60 + minutes)
}

/// Per-worker model + bound (ADR-021). An empty `model` means the provider
/// default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerConfig {
    #[serde(default)]
    pub model: String,
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u32,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            max_output_tokens: DEFAULT_WORKER_MAX_OUTPUT_TOKENS,
        }
    }
}

fn default_reflector_max_output_tokens() -> u32 {
    DEFAULT_REFLECTOR_MAX_OUTPUT_TOKENS
}

/// The reflector's per-worker config (M4.5): same shape as [`WorkerConfig`] but
/// with a larger default output cap, since it emits several notes per run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReflectorWorkerConfig {
    #[serde(default)]
    pub model: String,
    #[serde(default = "default_reflector_max_output_tokens")]
    pub max_output_tokens: u32,
}

impl Default for ReflectorWorkerConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            max_output_tokens: DEFAULT_REFLECTOR_MAX_OUTPUT_TOKENS,
        }
    }
}

fn default_sandbox_workspace() -> PathBuf {
    PathBuf::from(DEFAULT_SANDBOX_WORKSPACE)
}

fn default_sandbox_timeout_secs() -> u64 {
    DEFAULT_SANDBOX_TIMEOUT_SECS
}

fn default_sandbox_memory_mb() -> u64 {
    DEFAULT_SANDBOX_MEMORY_MB
}

/// Python-sandbox configuration (M5): the selected workspace (the only
/// writable path), extra read-only dirs, and the supervisor limits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// The one writable folder inside the sandbox; uploads land here.
    #[serde(default = "default_sandbox_workspace")]
    pub workspace: PathBuf,
    /// Additional directories exposed to scripts read-only.
    #[serde(default)]
    pub read_paths: Vec<PathBuf>,
    /// Wall-clock limit per script run (also bounds CPU time).
    #[serde(default = "default_sandbox_timeout_secs")]
    pub timeout_secs: u64,
    /// Address-space cap for the sandboxed process (RLIMIT_AS).
    #[serde(default = "default_sandbox_memory_mb")]
    pub memory_mb: u64,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            workspace: default_sandbox_workspace(),
            read_paths: Vec::new(),
            timeout_secs: DEFAULT_SANDBOX_TIMEOUT_SECS,
            memory_mb: DEFAULT_SANDBOX_MEMORY_MB,
        }
    }
}

fn default_max_upload_mb() -> u64 {
    DEFAULT_MAX_UPLOAD_MB
}

/// File-flow configuration (M5): the authenticated upload cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesConfig {
    #[serde(default = "default_max_upload_mb")]
    pub max_upload_mb: u64,
}

impl Default for FilesConfig {
    fn default() -> Self {
        Self {
            max_upload_mb: DEFAULT_MAX_UPLOAD_MB,
        }
    }
}

/// Worker registry configuration (the summarizer, M3; the distiller, M4; the
/// reflector, M4.5).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkersConfig {
    #[serde(default)]
    pub summarizer: WorkerConfig,
    #[serde(default)]
    pub distiller: WorkerConfig,
    #[serde(default)]
    pub reflector: ReflectorWorkerConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_model")]
    pub model: String,
    /// Optional reasoning effort for models that support it. Empty means the
    /// provider's own default; normalized to `None` by [`Config::parse`].
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

impl ProviderConfig {
    /// The configured reasoning effort, or `None` when unset/blank.
    pub fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning_effort
            .as_deref()
            .map(str::trim)
            .filter(|effort| !effort.is_empty())
    }
}

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_owned()
}

fn default_model() -> String {
    DEFAULT_MODEL.to_owned()
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            base_url: default_base_url(),
            api_key: String::new(),
            model: default_model(),
            reasoning_effort: None,
        }
    }
}
