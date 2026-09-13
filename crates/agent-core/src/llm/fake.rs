//! Fake provider + record/replay cassettes (ADR-020): [`FakeProvider`]
//! replays [`Cassette`] events (falling back to a built-in canned response so
//! `--fake` boots keyless), [`RecordingClient`] decorates any [`LlmClient`]
//! and appends live interactions to a cassette (`--record`). Everything runs
//! offline, so the whole test suite needs no network.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::error::{ApiError, ApiErrorKind, Result};
use crate::events::CoreEvent;
use crate::llm::client::{CLIENT_EVENT_CAPACITY, ChatFuture, LlmClient, ModelsFuture};
use crate::llm::types::{ChatRequest, ModelInfo};

pub const CASSETTE_VERSION: u32 = 1;

/// A recorded provider request + the events the provider produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Interaction {
    pub request: ChatRequest,
    pub events: Vec<CoreEvent>,
}

/// Replay fixture: versioned, dated, and provider-attributed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cassette {
    pub cassette_version: u32,
    /// Unix timestamp of the recording (fixtures are dated, ADR-020).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at_unix: Option<u64>,
    /// The base URL the cassette was recorded against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelInfo>,
    #[serde(default)]
    pub interactions: Vec<Interaction>,
}

impl Cassette {
    pub fn new() -> Self {
        Self {
            cassette_version: CASSETTE_VERSION,
            recorded_at_unix: None,
            base_url: None,
            models: Vec::new(),
            interactions: Vec::new(),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            ApiError::new(
                ApiErrorKind::Config,
                format!("cannot read cassette {}: {e}", path.display()),
            )
        })?;
        let cassette: Cassette = serde_json::from_str(&text).map_err(|e| {
            ApiError::new(
                ApiErrorKind::Protocol,
                format!("cassette {} is not valid JSON: {e}", path.display()),
            )
        })?;
        if cassette.cassette_version != CASSETTE_VERSION {
            return Err(ApiError::new(
                ApiErrorKind::Protocol,
                format!(
                    "cassette {} has version {} but this build speaks version {CASSETTE_VERSION}; re-record it",
                    path.display(),
                    cassette.cassette_version
                ),
            ));
        }
        Ok(cassette)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ApiError::new(
                    ApiErrorKind::Internal,
                    format!("cannot create cassette dir: {e}"),
                )
            })?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| ApiError::internal(format!("cannot serialize cassette: {e}")))?;
        std::fs::write(path, text).map_err(|e| {
            ApiError::internal(format!("cannot write cassette {}: {e}", path.display()))
        })?;
        Ok(())
    }
}

impl Default for Cassette {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct Reusable {
    interaction: Interaction,
    played: bool,
}

/// Replay client. Matching: the first *unplayed* interaction whose model and
/// messages equal the request; when all matches have been played the first
/// match is reused (deterministic loops instead of stale errors).
pub struct FakeProvider {
    interactions: Mutex<Vec<Reusable>>,
    models: Vec<ModelInfo>,
    /// Canned response for requests with no cassette match; keeps `--fake`
    /// frictionless. `None` makes unmatched requests a loud error (tests).
    fallback: Option<Vec<CoreEvent>>,
    /// Delay between replayed events; 0 in tests, small for the fake UI.
    delay: std::time::Duration,
}

impl FakeProvider {
    pub fn from_cassette(cassette: Cassette) -> Self {
        Self {
            interactions: Mutex::new(
                cassette
                    .interactions
                    .into_iter()
                    .map(|i| Reusable {
                        interaction: i,
                        played: false,
                    })
                    .collect(),
            ),
            models: cassette.models,
            fallback: None,
            delay: std::time::Duration::ZERO,
        }
    }

    /// A keyless default: canned response for any request + a model list.
    pub fn builtin() -> Self {
        let events = vec![
            CoreEvent::Reasoning { text: "The user wants to see the fake provider speak. I will explain what it is and how to switch to a real model.".into() },
            CoreEvent::Delta { text: "This is Kaeru's fake provider, running keyless ".into() },
            CoreEvent::Delta { text: "for development. ".into() },
            CoreEvent::Delta { text: "Put a real [provider] api_key into data/config.toml, or run without --fake, to talk to an actual model.".into() },
            CoreEvent::TurnDone { usage: Some(crate::events::Usage {
                input_tokens: Some(21),
                output_tokens: Some(42),
                total_tokens: Some(63),
            }) },
        ];
        let models = [
            "openai/gpt-4o-mini",
            "openai/gpt-4o",
            "deepseek-v4.1-flash",
            "anthropic/claude-3.5-sonnet",
            "meta-llama/llama-3.3-70b-instruct",
            "mistralai/mistral-small",
        ]
        .into_iter()
        .map(|id| {
            let mut info = ModelInfo::new(id);
            if id.starts_with("deepseek") {
                info.reasoning_effort_levels = vec!["low".into(), "high".into(), "xhigh".into()];
                info.default_reasoning_effort = Some("high".into());
            }
            info
        })
        .collect();
        Self {
            interactions: Mutex::new(Vec::new()),
            models,
            fallback: Some(events),
            delay: std::time::Duration::ZERO,
        }
    }

    /// `--fake` bootstrap: use the cassette when present, else the built-in.
    pub fn load_or_builtin(path: &Path) -> Self {
        match Cassette::load(path) {
            Ok(cassette) => Self::from_cassette(cassette).with_fallback_builtin(),
            Err(err) => {
                tracing::warn!(target: "agent_core::llm", "no usable cassette at {}: {err}; using built-in fake responses", path.display());
                Self::builtin()
            }
        }
    }

    /// Replay matching cassette interactions but answer anything else with the
    /// built-in canned response.
    pub fn with_fallback_builtin(mut self) -> Self {
        self.fallback = Self::builtin().fallback;
        self
    }

    /// Replay timing: 0 for tests, a few milliseconds for the fake UI.
    pub fn with_delay(mut self, delay: std::time::Duration) -> Self {
        self.delay = delay;
        self
    }

    fn pick_events(&self, request: &ChatRequest) -> Option<Vec<CoreEvent>> {
        let mut interactions = self.interactions.lock().expect("cassette lock poisoned");
        let matches: Vec<usize> = interactions
            .iter()
            .enumerate()
            .filter(|(_, r)| r.interaction.request == *request)
            .map(|(i, _)| i)
            .collect();
        let pick = matches
            .iter()
            .find(|&&i| !interactions[i].played)
            .or(matches.first())?;
        interactions[*pick].played = true;
        Some(interactions[*pick].interaction.events.clone())
    }
}

impl LlmClient for FakeProvider {
    fn chat(&self, request: ChatRequest) -> ChatFuture {
        let (tx, rx) = mpsc::channel(CLIENT_EVENT_CAPACITY);
        let events = match self.pick_events(&request) {
            Some(events) => events,
            None => match &self.fallback {
                Some(events) => events.clone(),
                None => {
                    let _ = tx.try_send(CoreEvent::error(
                        ApiErrorKind::Internal,
                        format!(
                            "fake provider: no cassette interaction matches this request \
                             (model {}, {} messages); re-record with --record",
                            request.model,
                            request.messages.len()
                        ),
                    ));
                    return Box::pin(async move { Ok(rx) });
                }
            },
        };
        let delay = self.delay;
        Box::pin(async move {
            tokio::spawn(async move {
                for event in events {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    if tx.send(event).await.is_err() {
                        return; // consumer gone: stop replaying
                    }
                }
            });
            Ok(rx)
        })
    }

    fn list_models(&self) -> ModelsFuture {
        let models = self.models.clone();
        Box::pin(async move { Ok(models) })
    }
}

/// Decorator that tees live interactions into a cassette file.
pub struct RecordingClient {
    inner: Arc<dyn LlmClient>,
    path: PathBuf,
    /// Serializes cassette load-modify-save cycles: overlapping turns used to
    /// race, and only the last writer's interaction survived.
    cassette: Arc<Mutex<()>>,
}

impl RecordingClient {
    pub fn new(inner: Arc<dyn LlmClient>, path: impl Into<PathBuf>) -> Self {
        Self {
            inner,
            path: path.into(),
            cassette: Arc::new(Mutex::new(())),
        }
    }

    fn append_interaction(
        path: &Path,
        lock: &Mutex<()>,
        request: ChatRequest,
        events: Vec<CoreEvent>,
    ) {
        let _guard = lock.lock().expect("cassette lock poisoned");
        let mut cassette = Cassette::load(path).unwrap_or_else(|_| {
            let mut c = Cassette::new();
            c.recorded_at_unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs());
            c
        });
        cassette.interactions.push(Interaction { request, events });
        if let Err(err) = cassette.save(path) {
            tracing::warn!(target: "agent_core::llm", "cannot update cassette {}: {err}", path.display());
        }
    }

    /// Refresh the cassette's model list and base URL. A later `list_models`
    /// call replaces a stale list, so provider model changes are picked up
    /// instead of being frozen at the first recording.
    fn record_models(path: &Path, lock: &Mutex<()>, models: &[ModelInfo], base_url: Option<&str>) {
        let _guard = lock.lock().expect("cassette lock poisoned");
        let mut cassette = Cassette::load(path).unwrap_or_default();
        let base_url_changed = base_url.is_some() && cassette.base_url.as_deref() != base_url;
        if cassette.models == models && !base_url_changed {
            return;
        }
        cassette.models = models.to_vec();
        if let Some(base_url) = base_url {
            cassette.base_url = Some(base_url.to_owned());
        }
        if let Err(err) = cassette.save(path) {
            tracing::warn!(target: "agent_core::llm", "cannot update cassette {}: {err}", path.display());
        }
    }
}

impl LlmClient for RecordingClient {
    fn chat(&self, request: ChatRequest) -> ChatFuture {
        let inner = Arc::clone(&self.inner);
        let path = self.path.clone();
        let lock = Arc::clone(&self.cassette);
        Box::pin(async move {
            let mut inner_rx = inner.chat(request.clone()).await?;
            let (tx, out_rx) = mpsc::channel(CLIENT_EVENT_CAPACITY);
            tokio::spawn(async move {
                let mut recorded = Vec::new();
                while let Some(event) = inner_rx.recv().await {
                    recorded.push(event.clone());
                    if tx.send(event).await.is_err() {
                        break; // consumer gone; the partial interaction is still recorded
                    }
                }
                if !recorded.is_empty() {
                    RecordingClient::append_interaction(&path, &lock, request, recorded);
                }
            });
            Ok(out_rx)
        })
    }

    fn list_models(&self) -> ModelsFuture {
        let path = self.path.clone();
        let inner = Arc::clone(&self.inner);
        let lock = Arc::clone(&self.cassette);
        Box::pin(async move {
            let models = inner.list_models().await?;
            RecordingClient::record_models(&path, &lock, &models, None);
            Ok(models)
        })
    }
}

#[cfg(test)]
mod tests;
