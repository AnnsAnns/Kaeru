//! Human-readable memory (M4): a plain-markdown store plus budgeted context
//! injection (ADR-007/ADR-018). Memory persists across sessions, so writes go
//! through the consent-gated `memory_write` tool (ADR-016) and are auto-tagged
//! by the tool-free `distiller` worker (ADR-021).

pub mod inject;
pub mod store;

pub use inject::{MEMORY_BLOCK_BUDGET_TOKENS, memory_block};
pub use store::{MemoryNote, MemoryStore, local_date};
