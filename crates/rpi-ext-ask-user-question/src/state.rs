//! Row/item data surface + canonical state machine (`state/`).
//!
//! Q0 landed the pure data modules ([`row_intent`], [`build`]); Q2 adds the
//! dialog state machine ([`key_router`], [`reducer`], [`selectors`]) and the
//! ABI session driver ([`session`], landing later in this task).

pub mod build;
pub mod key_router;
pub mod reducer;
pub mod row_intent;
pub mod selectors;
pub mod session;
