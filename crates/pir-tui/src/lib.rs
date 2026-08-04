//! `pir-tui` — port of `@earendil-works/pi-tui` @ pi 0.82.1 (2efa728).
//!
//! Terminal UI: components, custom diff renderer, keybindings.
//!
//! Boundaries (coding-standards §8.1): ports pi-tui's algorithms (ANSI line
//! lists + custom diff + Overlay + Kitty input); does NOT use ratatui — its
//! widget/layout model is incompatible with Pi's interaction contract.
//! Terminal I/O uses `crossterm` only. Depends on no other internal crate
//! (coding-standards §2.2).
//!
//! Skeleton only (T01); the engine lands in T11.

pub mod error;
pub mod fuzzy;

pub use error::TuiError;
