//! Fake provider + record/replay cassettes (ADR-020).
//!
//! - [`Cassette`]: a versioned JSON file of recorded request/response pairs.
//!   Cassettes double as protocol conformance samples and make the whole
//!   `cargo test` suite run offline.
//! - [`FakeProvider`]: replays cassette events for matching requests; falls
//!   back to a built-in canned response so `--fake` boots the full UI keyless.
//! - [`RecordingClient`]: decorator over any [`LlmClient`] that appends live
//!   interactions to a cassette file (`--record`).

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
            "anthropic/claude-3.5-sonnet",
            "meta-llama/llama-3.3-70b-instruct",
            "mistralai/mistral-small",
        ]
        .into_iter()
        .map(|id| ModelInfo { id: id.to_owned() })
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
}

impl RecordingClient {
    pub fn new(inner: Arc<dyn LlmClient>, path: impl Into<PathBuf>) -> Self {
        Self {
            inner,
            path: path.into(),
        }
    }

    fn append_interaction(path: &Path, request: ChatRequest, events: Vec<CoreEvent>) {
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

    fn record_models(path: &Path, models: &[ModelInfo], base_url: Option<&str>) {
        let mut cassette = Cassette::load(path).unwrap_or_default();
        if cassette.models.is_empty() {
            cassette.models = models.to_vec();
            if let Some(base_url) = base_url {
                cassette.base_url = Some(base_url.to_owned());
            }
            if let Err(err) = cassette.save(path) {
                tracing::warn!(target: "agent_core::llm", "cannot update cassette {}: {err}", path.display());
            }
        }
    }
}

impl LlmClient for RecordingClient {
    fn chat(&self, request: ChatRequest) -> ChatFuture {
        let inner = Arc::clone(&self.inner);
        let path = self.path.clone();
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
                    RecordingClient::append_interaction(&path, request, recorded);
                }
            });
            Ok(out_rx)
        })
    }

    fn list_models(&self) -> ModelsFuture {
        let path = self.path.clone();
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let models = inner.list_models().await?;
            RecordingClient::record_models(&path, &models, None);
            Ok(models)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::ChatMessage;

    fn sample_cassette() -> Cassette {
        Cassette {
            cassette_version: 1,
            recorded_at_unix: Some(1_760_000_000),
            base_url: Some("https://example.test/v1".into()),
            models: vec![ModelInfo {
                id: "m/test".into(),
            }],
            interactions: vec![Interaction {
                request: ChatRequest::new("m/test", vec![ChatMessage::user("hello")]),
                events: vec![
                    CoreEvent::Delta { text: "Hel".into() },
                    CoreEvent::Delta { text: "lo".into() },
                    CoreEvent::TurnDone { usage: None },
                ],
            }],
        }
    }

    fn temp_cassette_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kaeru-fake-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("cassette.json")
    }

    async fn collect(rx: mpsc::Receiver<CoreEvent>) -> Vec<CoreEvent> {
        let mut events = Vec::new();
        let mut rx = rx;
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    }

    #[test]
    fn cassette_round_trips_through_json() {
        let path = temp_cassette_path("roundtrip");
        let cassette = sample_cassette();
        cassette.save(&path).unwrap();
        let loaded = Cassette::load(&path).unwrap();
        assert_eq!(loaded, cassette);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn cassette_version_mismatch_is_a_clear_error() {
        let path = temp_cassette_path("version");
        std::fs::write(&path, r#"{"cassette_version": 99, "interactions": []}"#).unwrap();
        let err = Cassette::load(&path).unwrap_err();
        assert!(err.message.contains("re-record"));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn replays_matching_request_and_loops_on_replay() {
        let fake = FakeProvider::from_cassette(sample_cassette());
        let request = ChatRequest::new("m/test", vec![ChatMessage::user("hello")]);
        for _ in 0..2 {
            let events = collect(fake.chat(request.clone()).await.unwrap()).await;
            assert_eq!(
                events,
                vec![
                    CoreEvent::Delta { text: "Hel".into() },
                    CoreEvent::Delta { text: "lo".into() },
                    CoreEvent::TurnDone { usage: None },
                ]
            );
        }
    }

    #[tokio::test]
    async fn unmatched_request_without_fallback_is_a_loud_error() {
        let fake = FakeProvider::from_cassette(sample_cassette());
        let request = ChatRequest::new("m/test", vec![ChatMessage::user("never recorded")]);
        let events = collect(fake.chat(request).await.unwrap()).await;
        let CoreEvent::Error { kind, message } = &events[0] else {
            panic!("expected error event, got {events:?}");
        };
        assert_eq!(*kind, ApiErrorKind::Internal);
        assert!(message.contains("re-record"));
    }

    #[tokio::test]
    async fn builtin_answers_any_request_keyless() {
        let fake = FakeProvider::builtin();
        let request = ChatRequest::new("whatever", vec![ChatMessage::user("anything")]);
        let events = collect(fake.chat(request).await.unwrap()).await;
        assert!(matches!(events.last(), Some(CoreEvent::TurnDone { .. })));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, CoreEvent::Delta { text } if text.contains("fake provider")))
        );
        let models = fake.list_models().await.unwrap();
        assert_eq!(models[0].id, "openai/gpt-4o-mini");
    }

    #[tokio::test]
    async fn recording_client_records_and_the_cassette_replays() {
        let path = temp_cassette_path("record");
        let inner: Arc<dyn LlmClient> = Arc::new(FakeProvider::builtin());
        let recorder = RecordingClient::new(Arc::clone(&inner), &path);

        let request = ChatRequest::new("openai/gpt-4o-mini", vec![ChatMessage::user("hi there")]);
        let events = collect(recorder.chat(request.clone()).await.unwrap()).await;
        assert!(!events.is_empty());

        let models = recorder.list_models().await.unwrap();
        assert!(!models.is_empty());

        let cassette = Cassette::load(&path).unwrap();
        assert_eq!(cassette.models.len(), models.len());
        assert_eq!(cassette.interactions.len(), 1);
        assert_eq!(cassette.interactions[0].request, request);
        assert_eq!(cassette.interactions[0].events, events);

        let replay = FakeProvider::from_cassette(cassette);
        let replayed = collect(replay.chat(request).await.unwrap()).await;
        assert_eq!(replayed, events);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
