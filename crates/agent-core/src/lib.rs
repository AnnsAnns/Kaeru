//! Kaeru `agent-core` — the platform-agnostic agent library.
//!
//! Frontends (`agent-web`, later `agent-discord`) embed this crate in-process
//! and translate `CoreEvent`s into platform idioms. No frontend concept (HTTP,
//! HTML, Discord) is allowed in here (C2, ADR-010).
//!
//! Surface: config, the OpenAI-compatible client seam (real/fake/record), the
//! decoupled `ChatSession` turn executor, and (M3) the tool-calling agent loop
//! with web search, workers, the audit log and consent. Later milestones add
//! memory (M4) and the sandbox (M5) — see `docs/arc42-architecture.md`.

pub mod agent;
pub mod audit;
pub mod config;
pub mod context;
pub mod conversations;
pub mod error;
pub mod events;
pub mod llm;
pub mod memory;
pub mod search;
pub mod session;
pub mod tools;
mod util;

use std::path::PathBuf;
use std::sync::Arc;

pub use agent::{
    REFLECT_TAG, ReflectOutcome, ReflectStatus, Reflector, TurnOutcome, WorkerSpec, Workers,
};
pub use audit::{AuditEntry, AuditLog};
pub use config::{
    AgentConfig, Config, ContextConfig, DEFAULT_BASE_URL, DEFAULT_MAX_PROMPT_TOKENS, DEFAULT_MODEL,
    DEFAULT_PORT, Paths, ProviderConfig, ReflectConfig, ReflectorWorkerConfig, SearchConfig,
    SearchProviderKind, WorkerConfig, WorkersConfig,
};
pub use context::ContextPolicy;
pub use conversations::{
    CONVERSATION_SCHEMA_VERSION, Conversation, ConversationStore, StoredMessage,
};
pub use error::{ApiError, ApiErrorKind, Result};
pub use events::{
    ApprovalFuture, ApprovalKind, ApprovalSink, CoreEvent, Decision, DenyAll, EventStream, Risk,
    Usage,
};
pub use llm::{
    CASSETTE_VERSION, Cassette, ChatMessage, ChatRequest, ClientMode, FakeProvider, HttpClient,
    Interaction, LlmClient, ModelInfo, RecordingClient, Role, ToolCall,
};
pub use memory::{MemoryNote, MemoryStore, memory_block};
pub use search::{DisabledSearch, FakeSearch, SearchProvider, SearchResult};
pub use session::{ChatSession, ConversationRegistry, ThreadSummary, TurnHandle};
pub use tools::{Tool, ToolContext, ToolRegistry};

/// Shared core: configuration, the provider client, the tool registry, the
/// search provider, the worker registry, and the audit log.
///
/// Built through [`AgentCore::connect`] (wires workers + search from config)
/// and refined by the frontend with the `with_*` builders (tools, audit).
pub struct AgentCore {
    config: Config,
    client: Arc<dyn LlmClient>,
    mode: ClientMode,
    tools: ToolRegistry,
    search: Arc<dyn SearchProvider>,
    workers: Arc<Workers>,
    audit: AuditLog,
    memory: Option<MemoryStore>,
    /// Owner-written persona file, read fresh at each turn (M4.5, ADR-027).
    persona: Option<PathBuf>,
}

impl AgentCore {
    pub fn new(config: Config, client: Arc<dyn LlmClient>) -> Self {
        Self::with_mode(config, client, ClientMode::Live)
    }

    /// Like [`AgentCore::new`], but with an explicit mode (e.g. tests that
    /// inject a `FakeProvider` while reporting `is_fake` truthfully). No tools
    /// are registered; call [`AgentCore::with_tools`] for the M3 set.
    pub fn with_mode(config: Config, client: Arc<dyn LlmClient>, mode: ClientMode) -> Self {
        Self::assemble(config, client, mode, Arc::new(DisabledSearch))
    }

    /// Build the core with the client appropriate for `mode`. Wires the worker
    /// registry and the configured search provider; tools and the audit log are
    /// added by the frontend (they need runtime paths).
    pub fn connect(config: Config, mode: ClientMode) -> Result<Self> {
        let client =
            llm::connect_client(&mode, &config.provider.base_url, &config.provider.api_key)?;
        let search = search::from_config(&config.search);
        Ok(Self::assemble(config, client, mode, search))
    }

    /// The one struct literal every constructor goes through; `search` is the
    /// only field that differs between the live and test paths.
    fn assemble(
        config: Config,
        client: Arc<dyn LlmClient>,
        mode: ClientMode,
        search: Arc<dyn SearchProvider>,
    ) -> Self {
        let workers = Arc::new(Workers::from_config(
            Arc::clone(&client),
            &config.provider.model,
            &config.workers,
        ));
        Self {
            config,
            client,
            mode,
            tools: ToolRegistry::new(),
            search,
            workers,
            audit: AuditLog::disabled(),
            memory: None,
            persona: None,
        }
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

    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    pub fn search(&self) -> &dyn SearchProvider {
        self.search.as_ref()
    }

    pub fn workers(&self) -> &Workers {
        &self.workers
    }

    pub fn audit(&self) -> &AuditLog {
        &self.audit
    }

    /// The memory store, when one is configured (M4).
    pub fn memory(&self) -> Option<&MemoryStore> {
        self.memory.as_ref()
    }

    /// Attach the memory store (M4). Also add its tools via `with_tools`.
    pub fn with_memory(mut self, memory: MemoryStore) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Attach the owner-written persona file (M4.5, ADR-027). It is read fresh
    /// at each turn, so edits apply without a restart.
    pub fn with_persona(mut self, persona: impl Into<PathBuf>) -> Self {
        self.persona = Some(persona.into());
        self
    }

    /// The current persona text, read fresh from disk. An absent, unreadable or
    /// non-UTF-8 file means no system prompt (the M1-M4 behavior); the failure
    /// is logged, never fatal.
    pub fn persona(&self) -> Option<String> {
        let path = self.persona.as_ref()?;
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let text = text.trim();
                (!text.is_empty()).then(|| text.to_owned())
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                tracing::warn!(
                    target: "agent_core::persona",
                    "cannot read persona {}: {err}; continuing without a system prompt",
                    path.display()
                );
                None
            }
        }
    }

    /// The persona file path, when one is configured (the reflection job may
    /// revise it, M4.5).
    pub fn persona_path(&self) -> Option<&std::path::Path> {
        self.persona.as_deref()
    }

    /// Replace the tool registry (the M3 default set: web_search, memory_write).
    pub fn with_tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = tools;
        self
    }

    /// Replace the search provider.
    pub fn with_search(mut self, search: Arc<dyn SearchProvider>) -> Self {
        self.search = search;
        self
    }

    /// Replace the audit log.
    pub fn with_audit(mut self, audit: AuditLog) -> Self {
        self.audit = audit;
        self
    }

    /// True when the session runs on the fake provider (keyless dev / tests).
    pub fn is_fake(&self) -> bool {
        matches!(self.mode, ClientMode::Fake { .. })
    }

    /// The `ToolContext` a turn's tools run with.
    pub fn tool_context(&self, turn_id: u64) -> ToolContext {
        ToolContext {
            client: Arc::clone(&self.client),
            search: Arc::clone(&self.search),
            workers: Arc::clone(&self.workers),
            audit: self.audit.clone(),
            turn_id,
        }
    }
}
