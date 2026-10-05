//! Kaeru `agent-server`: the server library (M6, ADR-011/030). It owns the
//! `/api/*` HTTP surface — the wire-serialized `CoreEvent`/consent API every
//! frontend consumes — the embedded web UI assets, and the boot wiring of the
//! one server binary:
//!
//! - `agent-daemon` — owns `agent-core` + `data/`, binding localhost by
//!   default and a tailnet address on explicit configuration (ADR-030).
//!
//! This is frontend-side infrastructure (it speaks HTTP/HTML), so it stays
//! out of `agent-core` (C2).

pub mod assets;
pub mod bind;
pub mod boot;
pub mod bridge;
pub mod error;
pub mod files;
pub mod markdown;
pub mod routes;

pub use boot::{Boot, Cli, ParseError, bootstrap, init_tracing, parse_cli, run, usage};
pub use routes::{AppState, router};
