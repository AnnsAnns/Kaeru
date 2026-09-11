//! Configuration (`data/config.toml`).
//!
//! One TOML file next to the process working directory holds the provider
//! credentials and UI settings. The file is created with default values on
//! first start; on unix its permissions are forced to 0600 because it holds
//! the provider API key (never committed, see root `.gitignore`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ApiError, ApiErrorKind, Result};

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
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

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

fn default_reflect_time() -> String {
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
fn parse_hhmm(value: &str) -> Option<u32> {
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
                let config = Config::default();
                config.save(path)?;
                tracing::warn!(
                    config_path = %path.display(),
                    "no config found; wrote a default (edit [provider] or run --fake)"
                );
                Ok(config)
            }
            Err(e) => Err(ApiError::new(
                ApiErrorKind::Config,
                format!("cannot read config at {}: {e}", path.display()),
            )),
        }
    }

    /// Write the config file (creating parent directories), 0600 on unix.
    pub fn save(&self, path: &Path) -> Result<()> {
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kaeru-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parse_full_config() {
        let config = Config::parse(
            r##"
port = 9000
auth_token = "tok123"
[provider]
base_url = "http://127.0.0.1:11434/v1"
api_key = "ollama-needs-none"
model = "llama3"
"##,
        )
        .unwrap();
        assert_eq!(config.port, 9000);
        assert_eq!(config.auth_token(), Some("tok123"));
        assert_eq!(config.provider.model, "llama3");
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let config = Config::parse("[provider]\nmodel = \"m\"\n").unwrap();
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.provider.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.provider.api_key, "");
        assert_eq!(config.provider.model, "m");
        assert_eq!(config.auth_token(), None);
    }

    #[test]
    fn empty_auth_token_is_disabled() {
        let config = Config::parse("auth_token = \"   \"\n").unwrap();
        assert_eq!(config.auth_token(), None);
    }

    #[test]
    fn base_url_trailing_slash_is_normalized() {
        let config = Config::parse("[provider]\nbase_url = \"http://x/v1/\"\n").unwrap();
        assert_eq!(config.provider.base_url, "http://x/v1");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = Config::parse("port_typo = 1\n").unwrap_err();
        assert_eq!(err.kind, ApiErrorKind::Config);
        assert!(err.message.contains("invalid config"));
    }

    #[test]
    fn empty_model_is_rejected() {
        let err = Config::parse("[provider]\nmodel = \"\"\n").unwrap_err();
        assert!(err.message.contains("model"));
    }

    #[test]
    fn context_budget_is_configurable_with_a_default() {
        let config = Config::parse("[provider]\nmodel = \"m\"\n").unwrap();
        assert_eq!(config.context.max_prompt_tokens, DEFAULT_MAX_PROMPT_TOKENS);
        let config =
            Config::parse("[provider]\nmodel = \"m\"\n[context]\nmax_prompt_tokens = 100\n")
                .unwrap();
        assert_eq!(config.context.max_prompt_tokens, 100);
    }

    #[test]
    fn search_config_defaults_to_off_and_is_normalized() {
        let config = Config::parse("[provider]\nmodel = \"m\"\n").unwrap();
        assert_eq!(config.search.kind(), SearchProviderKind::Off);
        assert_eq!(config.search.max_results, DEFAULT_SEARCH_RESULTS);

        let config =
            Config::parse("[search]\nprovider = \"BRAVE\"\napi_key = \" k \"\nmax_results = 3\n")
                .unwrap();
        assert_eq!(config.search.kind(), SearchProviderKind::Brave);
        assert_eq!(config.search.api_key, "k");
        assert_eq!(config.search.max_results, 3);
    }

    #[test]
    fn agent_and_worker_config_have_defaults() {
        let config = Config::parse("[provider]\nmodel = \"m\"\n").unwrap();
        assert_eq!(config.agent.max_steps, DEFAULT_MAX_STEPS);
        assert_eq!(config.workers.summarizer.model, "");
        assert_eq!(
            config.workers.summarizer.max_output_tokens,
            DEFAULT_WORKER_MAX_OUTPUT_TOKENS
        );
        assert_eq!(config.workers.distiller.model, "");
        assert_eq!(
            config.workers.distiller.max_output_tokens,
            DEFAULT_WORKER_MAX_OUTPUT_TOKENS
        );

        let config = Config::parse(
            "[agent]\nmax_steps = 3\n[workers.summarizer]\nmodel = \"cheap/m\"\nmax_output_tokens = 128\n",
        )
        .unwrap();
        assert_eq!(config.agent.max_steps, 3);
        assert_eq!(config.workers.summarizer.model, "cheap/m");
        assert_eq!(config.workers.summarizer.max_output_tokens, 128);
    }

    #[test]
    fn reflect_config_defaults_off_and_normalizes_time() {
        let config = Config::parse("[provider]\nmodel = \"m\"\n").unwrap();
        assert!(!config.reflect.enabled);
        assert_eq!(config.reflect.time, DEFAULT_REFLECT_TIME);
        assert_eq!(config.reflect.scheduled_minutes(), 21 * 60);
        assert!(config.reflect.persona_edits);
        assert_eq!(
            config.workers.reflector.max_output_tokens,
            DEFAULT_REFLECTOR_MAX_OUTPUT_TOKENS
        );

        let config = Config::parse(
            "[reflect]\nenabled = true\ntime = \" 07:05 \"\npersona_edits = false\n[workers.reflector]\nmodel = \"r/m\"\n",
        )
        .unwrap();
        assert!(config.reflect.enabled);
        assert_eq!(config.reflect.time, "07:05");
        assert_eq!(config.reflect.scheduled_minutes(), 7 * 60 + 5);
        assert!(!config.reflect.persona_edits);
        assert_eq!(config.workers.reflector.model, "r/m");
        assert_eq!(
            config.workers.reflector.max_output_tokens,
            DEFAULT_REFLECTOR_MAX_OUTPUT_TOKENS
        );

        let config = Config::parse("[reflect]\ntime = \"bogus\"\n").unwrap();
        assert_eq!(config.reflect.time, DEFAULT_REFLECT_TIME);
    }

    #[test]
    fn load_creates_default_file_with_tight_permissions() {
        let dir = temp_dir("load-default");
        let path = dir.join("data/config.toml");
        let config = Config::load(&path).unwrap();
        assert!(path.is_file());
        assert_eq!(config.provider.base_url, DEFAULT_BASE_URL);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let reloaded = Config::load(&path).unwrap();
        assert_eq!(reloaded, config);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_reports_broken_config_with_path() {
        let dir = temp_dir("load-broken");
        let path = dir.join("config.toml");
        std::fs::write(&path, "port = \"not-a-number\"\n").unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(err.message.contains("invalid config"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
