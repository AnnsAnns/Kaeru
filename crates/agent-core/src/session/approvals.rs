//! Session-scoped consent plumbing: `SessionApprovals` emits `ApprovalRequest`
//! events and resolves them via `ChatSession::approve`, fail-closed on timeout.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;

use crate::agent::Emitter;
use crate::events::{ApprovalFuture, ApprovalKind, ApprovalSink, CoreEvent, Decision};

/// How long a consent card may wait for a decision before it fails closed.
pub(super) const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// Core-owned consent resolution for one frontend (ADR-014): emits
/// `ApprovalRequest` and waits for `ChatSession::approve`, failing closed on
/// timeout.
pub(super) struct SessionApprovals {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
    next_id: AtomicU64,
    emitter: Emitter,
    timeout: Duration,
}

impl SessionApprovals {
    pub(super) fn new(emitter: Emitter, timeout: Duration) -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            emitter,
            timeout,
        }
    }

    /// Deliver a decision to a waiting request; false when the id is unknown.
    pub(super) fn resolve(&self, request_id: &str, decision: Decision) -> bool {
        match self
            .pending
            .lock()
            .expect("approvals lock poisoned")
            .remove(request_id)
        {
            Some(sender) => sender.send(decision).is_ok(),
            None => false,
        }
    }
}

impl ApprovalSink for SessionApprovals {
    fn request(&self, kind: ApprovalKind, summary: String) -> ApprovalFuture {
        let id = format!("appr{:x}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("approvals lock poisoned")
            .insert(id.clone(), tx);
        self.emitter.emit(CoreEvent::ApprovalRequest {
            id: id.clone(),
            kind,
            summary,
        });
        let pending = Arc::clone(&self.pending);
        let timeout = self.timeout;
        Box::pin(async move {
            let decision = tokio::time::timeout(timeout, rx)
                .await
                .ok()
                .and_then(|result| result.ok())
                .unwrap_or(Decision::Deny);
            pending.lock().expect("approvals lock poisoned").remove(&id);
            decision
        })
    }
}
