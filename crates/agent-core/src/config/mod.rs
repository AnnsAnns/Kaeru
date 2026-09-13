//! Configuration (`data/config.toml`): one TOML file with provider
//! credentials and UI settings, created with documented defaults on first
//! start and forced to 0600 on unix (it holds the provider API key).

mod sections;
#[cfg(test)]
mod tests;

pub use sections::{
    AgentConfig, ContextConfig, FilesConfig, ProviderConfig, ReflectConfig, ReflectorWorkerConfig,
    SandboxConfig, SearchConfig, SearchProviderKind, WorkerConfig, WorkersConfig,
};

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ApiError, ApiErrorKind, Result};

use sections::{default_reflect_time, parse_hhmm};

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const DEFAULT_MODEL: &str = "openai/gpt-4o-mini";
pub const DEFAULT_PORT: u16 = 8080;
/// Deterministic context budget (ADR-018) in estimated tokens.
pub const DEFAULT_MAX_PROMPT_TOKENS: u64 = 16_000;
/// Agent-loop step ceiling: tool calls per turn before the loop stops (M3).
pub const DEFAULT_MAX_STEPS: u32 = 8;
/// Default number of web results a search returns (M3).
pub const DEFAULT_SEARCH_RESULTS: usize = 5;
/// Default output cap for a worker call (bounded distillation, M3/ADR-021).
pub const DEFAULT_WORKER_MAX_OUTPUT_TOKENS: u32 = 600;
/// Default output cap for the reflector, which emits several notes per run
/// (M4.5/ADR-028).
pub const DEFAULT_REFLECTOR_MAX_OUTPUT_TOKENS: u32 = 1200;
/// Default local time for the evening reflection (21:00, M4.5/ADR-028).
pub const DEFAULT_REFLECT_TIME: &str = "21:00";
/// Default writable workspace inside the Python sandbox (M5).
pub const DEFAULT_SANDBOX_WORKSPACE: &str = "data/sandbox/workspace";
/// Default wall-clock limit for one sandboxed script run (M5).
pub const DEFAULT_SANDBOX_TIMEOUT_SECS: u64 = 60;
/// Default address-space limit for a sandboxed script (M5).
pub const DEFAULT_SANDBOX_MEMORY_MB: u64 = 512;
/// Default upload size cap for the workspace (M5).
pub const DEFAULT_MAX_UPLOAD_MB: u64 = 50;
const DEFAULT_CONFIG_TOML: &str = r##"# Kaeru configuration. This file holds your provider API key:
# keep it private (it is written with 0600 permissions and git-ignored).

# Local port the web UI binds to. The server ALWAYS binds to 127.0.0.1 only.
port = 8080

# Shared secret required as the `X-Auth-Token` header on every /api/* request.
# Leave empty for plain localhost use. Set a long random value before exposing
# the server through a tunnel (see docs/arc42-architecture.md, M2).
auth_token = ""

[provider]
# Any OpenAI-compatible Chat Completions base URL:
#   OpenRouter: https://openrouter.ai/api/v1
#   Ollama:     http://127.0.0.1:11434/v1
#   LM Studio:  http://127.0.0.1:1234/v1
base_url = "https://openrouter.ai/api/v1"

# Provider API key. Leave empty for local servers that need no auth.
# Never paste this key anywhere else; the browser never sees it.
api_key = ""

# Default model for conversations (OpenRouter-style id, or your local model).
model = "openai/gpt-4o-mini"

# Reasoning effort for models that support it. Charm Hyper/DeepSeek accept
# "low"/"high"/"xhigh"; OpenAI-style providers use "low"/"medium"/"high".
# Leave empty to use the provider's own default effort.
reasoning_effort = ""

[context]
# Deterministic context budget (ADR-018), in estimated tokens: prompts are
# assembled as [rolling summary] + recent window; when a conversation grows
# past this budget, the oldest turns are summarized (never silently truncated).
max_prompt_tokens = 16000

[agent]
# Maximum tool-calling steps per turn before the loop stops and answers (M3).
max_steps = 8

[search]
# Web search provider for the `web_search` tool (M3). One of:
#   "off"      no web search (the tool reports it is not configured)
#   "brave"    Brave Search API      (api_key required)
#   "tavily"   Tavily Search API     (api_key required)
#   "searxng"  a self-hosted SearxNG (base_url required, e.g. http://127.0.0.1:8888)
provider = "off"
api_key = ""
base_url = ""
# How many results a search returns (pages fetched for summarization are capped).
max_results = 5

# Worker models (ADR-021): concern-separated, tool-free LLM sub-calls with
# their own model. The summarizer distills fetched web pages before they reach
# the main model, so raw pages never enter the main context. An empty model
# uses the provider default.
[workers.summarizer]
model = ""
max_output_tokens = 600

# The distiller tidies memory candidates before they are saved: it produces a
# short note plus tags (M4). An empty model uses the provider default.
[workers.distiller]
model = ""
max_output_tokens = 600

# Evening reflection (M4.5, ADR-028): a daily job digests the conversations
# changed since the last run into `reflect`-tagged memory notes via the
# tool-free reflector worker. Enabling it is the standing consent; every run
# is audited. Leave disabled to avoid nightly worker token cost.
[reflect]
enabled = false
# Local time (HH:MM) for the daily run; a missed evening is caught up at boot.
time = "21:00"
# Let the reflector revise data/persona.md a little when it judges the change
# helpful. Every change is recorded as a `persona` memory note (why + how).
persona_edits = true

# The reflector writes several notes per run, so it gets more room than the
# other workers. An empty model uses the provider default.
[workers.reflector]
model = ""
max_output_tokens = 1200

[sandbox]
# The single folder the Python tool may write to ("the selected workspace").
# Uploads land here, and artifacts a script writes here are shown in the chat.
# Relative paths resolve against the process working directory.
workspace = "data/sandbox/workspace"

# Extra directories exposed to scripts read-only (e.g. a notes folder).
read_paths = []

# Wall-clock limit for one script run (also caps CPU time) and the
# address-space limit for the sandboxed process.
timeout_secs = 60
memory_mb = 512

[files]
# Size cap for uploads into the workspace (`POST /api/files`).
max_upload_mb = 50
"##;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    auth_token: Option<String>,
    #[serde(default)]
    pub provider: ProviderConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub search: SearchConfig,
    #[serde(default)]
    pub workers: WorkersConfig,
    #[serde(default)]
    pub reflect: ReflectConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub files: FilesConfig,
}
fn default_port() -> u16 {
    DEFAULT_PORT
}
impl Default for Config {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            auth_token: None,
            provider: ProviderConfig::default(),
            context: ContextConfig::default(),
            agent: AgentConfig::default(),
            search: SearchConfig::default(),
            workers: WorkersConfig::default(),
            reflect: ReflectConfig::default(),
            sandbox: SandboxConfig::default(),
            files: FilesConfig::default(),
        }
    }
}

impl Config {
    /// The auth token, normalized: `None` unless a non-empty value is set.
    pub fn auth_token(&self) -> Option<&str> {
        self.auth_token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
    }

    /// Builder: set the `X-Auth-Token` shared secret.
    pub fn with_auth_token(mut self, token: impl Into<String>) -> Self {
        self.auth_token = Some(token.into());
        self
    }

    /// Parse and validate config text (TOML).
    pub fn parse(text: &str) -> Result<Self> {
        let config: Config = toml::from_str(text)
            .map_err(|e| ApiError::new(ApiErrorKind::Config, format!("invalid config: {e}")))?;
        config.validate()
    }

    /// Load the config file, writing a documented default file when missing.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Config::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Config::write_default(path)?;
                tracing::warn!(
                    config_path = %path.display(),
                    "no config found; wrote a default (edit [provider] or run --fake)"
                );
                Ok(Config::default())
            }
            Err(e) => Err(ApiError::new(
                ApiErrorKind::Config,
                format!("cannot read config at {}: {e}", path.display()),
            )),
        }
    }

    /// Write the documented default config file (creating parent
    /// directories), 0600 on unix.
    ///
    /// This writes [`DEFAULT_CONFIG_TOML`], the commented first-boot template,
    /// not the receiver: it is the missing-file branch of [`Config::load`]. Use
    /// `toml::to_string` at the call site if an edited config must be
    /// persisted instead.
    pub fn write_default(path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ApiError::new(
                    ApiErrorKind::Config,
                    format!("cannot create config dir {}: {e}", parent.display()),
                )
            })?;
        }
        std::fs::write(path, DEFAULT_CONFIG_TOML).map_err(|e| {
            ApiError::new(
                ApiErrorKind::Config,
                format!("cannot write config at {}: {e}", path.display()),
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
                |e| {
                    ApiError::new(
                        ApiErrorKind::Config,
                        format!("cannot chmod 0600 config at {}: {e}", path.display()),
                    )
                },
            )?;
        }
        Ok(())
    }

    fn validate(self) -> Result<Self> {
        let mut config = self;
        config.provider.base_url = config
            .provider
            .base_url
            .trim()
            .trim_end_matches('/')
            .to_owned();
        config.provider.model = config.provider.model.trim().to_owned();
        config.provider.reasoning_effort = config
            .provider
            .reasoning_effort
            .take()
            .map(|e| e.trim().to_owned())
            .filter(|e| !e.is_empty());
        config.auth_token = config
            .auth_token
            .take()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty());
        config.search.provider = config.search.provider.trim().to_ascii_lowercase();
        config.search.api_key = config.search.api_key.trim().to_owned();
        config.search.base_url = config
            .search
            .base_url
            .trim()
            .trim_end_matches('/')
            .to_owned();
        if config.search.max_results == 0 {
            config.search.max_results = DEFAULT_SEARCH_RESULTS;
        }
        config.workers.summarizer.model = config.workers.summarizer.model.trim().to_owned();
        if config.workers.summarizer.max_output_tokens == 0 {
            config.workers.summarizer.max_output_tokens = DEFAULT_WORKER_MAX_OUTPUT_TOKENS;
        }
        config.workers.distiller.model = config.workers.distiller.model.trim().to_owned();
        if config.workers.distiller.max_output_tokens == 0 {
            config.workers.distiller.max_output_tokens = DEFAULT_WORKER_MAX_OUTPUT_TOKENS;
        }
        config.workers.reflector.model = config.workers.reflector.model.trim().to_owned();
        if config.workers.reflector.max_output_tokens == 0 {
            config.workers.reflector.max_output_tokens = DEFAULT_REFLECTOR_MAX_OUTPUT_TOKENS;
        }
        config.reflect.time = config.reflect.time.trim().to_owned();
        if parse_hhmm(&config.reflect.time).is_none() {
            config.reflect.time = default_reflect_time();
        }
        if config.agent.max_steps == 0 {
            config.agent.max_steps = DEFAULT_MAX_STEPS;
        }
        if config.sandbox.timeout_secs == 0 {
            config.sandbox.timeout_secs = DEFAULT_SANDBOX_TIMEOUT_SECS;
        }
        if config.sandbox.memory_mb == 0 {
            config.sandbox.memory_mb = DEFAULT_SANDBOX_MEMORY_MB;
        }
        if config.files.max_upload_mb == 0 {
            config.files.max_upload_mb = DEFAULT_MAX_UPLOAD_MB;
        }
        if config.provider.base_url.is_empty() {
            return Err(ApiError::config("[provider] base_url must not be empty"));
        }
        if config.provider.model.is_empty() {
            return Err(ApiError::config("[provider] model must not be empty"));
        }
        Ok(config)
    }
}

/// Default locations used by the frontend binaries: everything mutable lives
/// under a single `data/` directory so backup = copy one folder.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config: PathBuf,
    pub cassette: PathBuf,
    pub conversations: PathBuf,
    /// Append-only audit log (M3).
    pub audit: PathBuf,
    /// Markdown memory store (M3 write side; M4 enriches).
    pub memory: PathBuf,
    /// Owner-written persona/character file (M4.5, ADR-027).
    pub persona: PathBuf,
    /// Last successful evening-reflection run (M4.5, ADR-028).
    pub reflect_state: PathBuf,
    /// uv-prepared ephemeral Python environments (M5); disposable.
    pub sandbox_envs: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            config: PathBuf::from("data/config.toml"),
            cassette: PathBuf::from("data/cassette.json"),
            conversations: PathBuf::from("data/conversations"),
            audit: PathBuf::from("data/audit.jsonl"),
            memory: PathBuf::from("data/memory"),
            persona: PathBuf::from("data/persona.md"),
            reflect_state: PathBuf::from("data/reflect-state.json"),
            sandbox_envs: PathBuf::from("data/sandbox/envs"),
        }
    }
}
