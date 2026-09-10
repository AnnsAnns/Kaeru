//! Kaeru `agent-core` — the platform-agnostic agent library.
//!
//! Frontends (`agent-web`, later `agent-discord`) embed this crate in-process
//! and translate `CoreEvent`s into platform idioms. No frontend concept (HTTP,
//! HTML, Discord) is allowed in here (C2, ADR-010).
//!
//! M1 surface: config, the OpenAI-compatible client seam (real/fake/record),
//! and the decoupled `ChatSession` turn executor. Later milestones add tools
//! (M3), memory (M4), and the sandbox (M5) — see `docs/arc42-architecture.md`.

pub mod config;
pub mod context;
pub mod conversations;
pub mod error;
pub mod events;
pub mod llm;
pub mod session;

use std::sync::Arc;

pub use config::{
    Config, ContextConfig, DEFAULT_BASE_URL, DEFAULT_MAX_PROMPT_TOKENS, DEFAULT_MODEL,
    DEFAULT_PORT, Paths, ProviderConfig,
};
pub use context::ContextPolicy;
pub use conversations::{
    CONVERSATION_SCHEMA_VERSION, Conversation, ConversationStore, StoredMessage,
};
pub use error::{ApiError, ApiErrorKind, Result};
pub use events::{ApprovalKind, CoreEvent, Decision, EventStream, Risk, Usage};
pub use llm::{
    CASSETTE_VERSION, Cassette, ChatMessage, ChatRequest, ClientMode, FakeProvider, HttpClient,
    Interaction, LlmClient, ModelInfo, RecordingClient, Role,
};
pub use session::{ChatSession, TurnHandle};

/// Shared core: configuration plus the provider client.
///
/// Built through [`AgentCore::connect`] which wires the client for the
/// requested [`ClientMode`] (live / fake / record).
pub struct AgentCore {
    config: Config,
    client: Arc<dyn LlmClient>,
    mode: ClientMode,
}

impl AgentCore {
    pub fn new(config: Config, client: Arc<dyn LlmClient>) -> Self {
        Self::with_mode(config, client, ClientMode::Live)
    }

    /// Like [`AgentCore::new`], but with an explicit mode (e.g. tests that
    /// inject a `FakeProvider` while reporting `is_fake` truthfully).
    pub fn with_mode(config: Config, client: Arc<dyn LlmClient>, mode: ClientMode) -> Self {
        Self {
            config,
            client,
            mode,
        }
    }

    /// Build the core with the client appropriate for `mode`.
    pub fn connect(config: Config, mode: ClientMode) -> Result<Self> {
        let client =
            llm::connect_client(&mode, &config.provider.base_url, &config.provider.api_key)?;
        Ok(Self {
            config,
            client,
            mode,
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn client(&self) -> &dyn LlmClient {
        self.client.as_ref()
    }

    pub fn mode(&self) -> &ClientMode {
        &self.mode
    }

    /// True when the session runs on the fake provider (keyless dev / tests).
    pub fn is_fake(&self) -> bool {
        matches!(self.mode, ClientMode::Fake { .. })
    }
}
