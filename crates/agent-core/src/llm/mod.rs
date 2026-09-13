//! LLM access: OpenAI-compatible adapter (ADR-005), SSE parsing, and the
//! fake/recording providers for offline tests and keyless development
//! (ADR-020). All protocol knowledge is confined to this module.

pub mod client;
pub mod fake;
pub mod sse;
pub mod types;
pub(crate) mod wire;

pub use client::{ChatFuture, HttpClient, LlmClient, ModelsFuture};
pub use fake::{CASSETTE_VERSION, Cassette, FakeProvider, Interaction, RecordingClient};
pub use types::{ChatMessage, ChatRequest, ModelInfo, Role, ToolCall};

use std::path::PathBuf;
use std::sync::Arc;

use crate::error::Result;

/// How `AgentCore::connect` wires the provider client.
#[derive(Debug, Clone)]
pub enum ClientMode {
    /// Talk to the configured OpenAI-compatible endpoint.
    Live,
    /// Replay cassettes / built-in responses; keyless development and tests.
    Fake { cassette: PathBuf },
    /// Talk live and record every interaction into the cassette file.
    Record { cassette: PathBuf },
}

/// Default inter-event delay for the `--fake` UI so streaming is visible.
pub const FAKE_UI_DELAY: std::time::Duration = std::time::Duration::from_millis(20);

/// Builds the right `LlmClient` for a mode.
pub fn connect_client(
    mode: &ClientMode,
    base_url: &str,
    api_key: &str,
) -> Result<Arc<dyn LlmClient>> {
    match mode {
        ClientMode::Live => Ok(Arc::new(HttpClient::new(base_url, api_key)?)),
        ClientMode::Fake { cassette } => Ok(Arc::new(
            FakeProvider::load_or_builtin(cassette).with_delay(FAKE_UI_DELAY),
        )),
        ClientMode::Record { cassette } => Ok(Arc::new(RecordingClient::new(
            Arc::new(HttpClient::new(base_url, api_key)?),
            cassette.clone(),
        ))),
    }
}
