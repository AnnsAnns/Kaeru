//! Error taxonomy shared by the core and every frontend: one `ApiErrorKind`
//! vocabulary, mapped by frontends to platform idioms (HTTP status codes,
//! ...). `CoreEvent::Error` carries the same kind.

use serde::{Deserialize, Serialize};

/// Coarse failure categories, wire-serializable (`snake_case`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorKind {
    /// Invalid or missing configuration.
    Config,
    /// Another turn is already executing on this session.
    Busy,
    /// Provider rejected our credentials (HTTP 401/403).
    Unauthorized,
    /// Provider rate limited us (HTTP 429).
    RateLimited,
    /// Provider returned HTTP 404 (e.g. unknown model).
    NotFound,
    /// Access to a local resource was refused (path traversal, symlink
    /// escape in the file flow; M5).
    Forbidden,
    /// Connection / transport level failure.
    Network,
    /// Provider response violated the OpenAI-compatible protocol.
    Protocol,
    /// Any other provider-side failure (5xx, error bodies, ...).
    Provider,
    /// Turn was aborted (by the user or a dropped frontend connection).
    Aborted,
    /// Internal invariant violation; a bug, not a provider/config problem.
    Internal,
}

impl ApiErrorKind {
    /// Stable, human-friendly identifier (matches the serde wire form).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Busy => "busy",
            Self::Unauthorized => "unauthorized",
            Self::RateLimited => "rate_limited",
            Self::NotFound => "not_found",
            Self::Forbidden => "forbidden",
            Self::Network => "network",
            Self::Protocol => "protocol",
            Self::Provider => "provider",
            Self::Aborted => "aborted",
            Self::Internal => "internal",
        }
    }
}

/// A categorized error with a human-readable message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub kind: ApiErrorKind,
    pub message: String,
}

impl ApiError {
    pub fn new(kind: ApiErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::new(ApiErrorKind::Config, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ApiErrorKind::Internal, message)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.message)
    }
}

impl std::error::Error for ApiError {}

pub type Result<T> = std::result::Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&ApiErrorKind::RateLimited).unwrap(),
            "\"rate_limited\""
        );
        assert_eq!(ApiErrorKind::RateLimited.as_str(), "rate_limited");
    }

    #[test]
    fn display_includes_kind_and_message() {
        let err = ApiError::new(ApiErrorKind::Busy, "a turn is already in progress");
        assert_eq!(err.to_string(), "busy: a turn is already in progress");
    }
}
