//! Human-readable memory (M4): a plain-markdown store plus budgeted context
//! injection (ADR-007/ADR-018). Memory persists across sessions, so writes go
//! through the consent-gated `memory_write` tool (ADR-016) and are auto-tagged
//! by the tool-free `distiller` worker (ADR-021).

pub mod inject;
pub mod store;

pub use inject::{MEMORY_BLOCK_BUDGET_TOKENS, memory_block};
pub use store::{MemoryNote, MemoryStore, local_date};

use store::MemoryNote as Note;

/// Lowercased, deduped query terms (order preserved). `min_chars` differs by
/// caller on purpose: `MemoryStore::search` accepts any non-empty term, while
/// context injection drops terms shorter than three characters so a stray
/// "a"/"is" cannot pull in unrelated notes.
pub(crate) fn query_terms(query: &str, min_chars: usize) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for term in query.split(|c: char| !c.is_alphanumeric()) {
        let term = term.to_lowercase();
        if term.chars().count() >= min_chars && !terms.contains(&term) {
            terms.push(term);
        }
    }
    terms
}

/// Term-overlap score; a tag hit outranks a body hit.
pub(crate) fn score_note(note: &Note, terms: &[String]) -> usize {
    let body = note.content.to_lowercase();
    let tags: Vec<String> = note.tags.iter().map(|tag| tag.to_lowercase()).collect();
    terms
        .iter()
        .map(|term| {
            if tags.iter().any(|tag| tag.contains(term.as_str())) {
                3
            } else if body.contains(term.as_str()) {
                1
            } else {
                0
            }
        })
        .sum()
}
