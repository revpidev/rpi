//! `rpi-tui` — port of `@earendil-works/pi-tui` @ pi 0.82.1 (2efa728).
//!
//! Terminal UI: components, custom diff renderer, keybindings.
//!
//! Boundaries (coding-standards §8.1): ports pi-tui's algorithms (ANSI line
//! lists + custom diff + Overlay + Kitty input); does NOT use ratatui — its
//! widget/layout model is incompatible with Pi's interaction contract.
//! Terminal I/O uses `crossterm` only. Depends on no other internal crate
//! (coding-standards §2.2).
//!
//! Engine lands in T11 (in progress): utils / keys / stdin_buffer /
//! terminal_* modules first, then components, the TUI core and terminal
//! state recovery (`recovery`, coding-standards §8.5).

pub mod autocomplete;
pub mod components;
pub mod error;
pub mod fuzzy;
pub mod keybindings;
pub mod keys;
pub mod kill_ring;
pub mod native_modifiers;
pub mod recovery;
pub mod stdin_buffer;
pub mod terminal;
pub mod terminal_colors;
pub mod terminal_image;
pub mod tui;
pub(crate) mod tui_base;
pub mod tui_main_screen;
pub mod undo_stack;
pub mod utils;
pub mod word_navigation;

pub use error::TuiError;
