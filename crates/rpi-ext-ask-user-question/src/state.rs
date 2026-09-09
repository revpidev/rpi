//! Row/item data surface (`state/`).
//!
//! Q0 lands the pure data modules only — [`row_intent`] (kind metadata +
//! sentinel derivation) and [`build`] (per-question item list). The state
//! machine, key router, selectors and session driver land with Q2/Q3
//! (`state/{key_router,reducer,selectors,session}.rs`, design §2).

pub mod build;
pub mod row_intent;
