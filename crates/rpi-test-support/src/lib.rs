//! `rpi-test-support` — test infrastructure crate (coding-standards §12).
//!
//! Parity harness for the whole project (design §10.2, delivered in T02):
//! - [`faux`]: deterministic faux provider (scripted `StreamEvent` sequences)
//! - [`normalize`]: the **single** parity normalizer (strip timestamps / uuids
//!   / session ids / cwd); every task reuses this, reimplementation is
//!   forbidden (coding-standards §12.3)
//! - [`diff`]: normalized comparison of event sequences, session JSONL (line
//!   order enforced) and transcripts
//! - [`vt`]: `VirtualTerminal` frame recorder for TUI tests
//!
//! Referenced only as a **dev-dependency** — it must never enter the release
//! dependency chain (coding-standards §2.2).

pub mod diff;
pub mod error;
pub mod faux;
pub mod normalize;
pub mod vt;

pub use diff::{diff_event_sequence, diff_events_normalized, diff_jsonl, diff_text, DiffFailure};
pub use error::TestSupportError;
pub use faux::{
    faux_assistant_message, faux_text, faux_thinking, faux_tool_call, FauxAiProvider,
    FauxAssistantOptions, FauxContent, FauxModelDefinition, FauxProvider, FauxProviderOptions,
    FauxResponseStep, FauxState,
};
pub use normalize::Normalizer;
pub use vt::VirtualTerminal;
