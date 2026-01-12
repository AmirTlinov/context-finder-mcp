//! Meaning engine (lenses-first) for Context Finder.
//!
//! This crate is intentionally **agent-first**:
//! - deterministic, bounded outputs
//! - evidence-backed claims (no implicit guesses)
//! - designed to minimize LLM token usage (CP encodings + optional diagrams)
//!
//! It is consumed by both:
//! - `context-cli` (Command API)
//! - `context-mcp` (MCP tools)

pub mod model;

mod common;
mod focus;
mod pack;
mod paths;
mod secrets;

pub use focus::meaning_focus;
pub use model::{
    EvidencePointer, MeaningFocusBudget, MeaningFocusRequest, MeaningFocusResult,
    MeaningPackBudget, MeaningPackRequest, MeaningPackResult,
};
pub use pack::meaning_pack;
