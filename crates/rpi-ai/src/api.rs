//! API adapters, mirroring `packages/ai/src/api/` (design §3.3).

pub mod anthropic_messages;
pub mod azure_openai_responses;
pub mod bedrock;
pub mod bedrock_converse_stream;
pub mod codex_ws;
pub mod constrained_sampling;
pub mod copilot_headers;
pub mod google_adc;
pub mod google_generative_ai;
pub mod google_shared;
pub mod google_vertex;
pub mod lazy;
pub mod mistral_conversations;
pub mod openai_codex_responses;
pub mod openai_completions;
pub mod openai_prompt_cache;
pub mod openai_responses;
pub mod openai_responses_shared;
pub mod openrouter_images;
pub mod pi_messages;
pub mod simple_options;
pub mod sse;
pub mod stream_timeouts;
