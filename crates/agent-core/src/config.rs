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
}

fn default_port() -> u16 {
    DEFAULT_PORT
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
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            auth_token: None,
            provider: ProviderConfig::default(),
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
        config.auth_token = config
            .auth_token
            .take()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty());
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
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            config: PathBuf::from("data/config.toml"),
            cassette: PathBuf::from("data/cassette.json"),
            conversations: PathBuf::from("data/conversations"),
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
