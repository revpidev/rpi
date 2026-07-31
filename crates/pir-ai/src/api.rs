//! API adapters, mirroring `packages/ai/src/api/` (design §3.3).

pub mod anthropic_messages;
pub mod constrained_sampling;
pub mod copilot_headers;
pub mod lazy;
pub mod openai_completions;
pub mod openai_prompt_cache;
pub mod openai_responses;
pub mod openai_responses_shared;
pub mod simple_options;
pub mod sse;
