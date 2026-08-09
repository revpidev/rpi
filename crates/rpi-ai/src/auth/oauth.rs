//! Port of `packages/ai/src/auth/oauth/` @ pi 0.82.1 (2efa728) — OAuth flow
//! building blocks: PKCE (`pkce`), the RFC 8628 device-code polling framework
//! (`device_code`), the localhost callback page/server (`callback_page`), the
//! flow registry (`load`) and the provider flows (`anthropic`,
//! `github_copilot`, `kimi_coding`, `openai_codex`, `openrouter`, `radius`,
//! `xai`).

pub mod anthropic;
pub mod callback_page;
pub mod device_code;
pub mod github_copilot;
pub mod kimi_coding;
pub mod load;
pub mod openai_codex;
pub mod openrouter;
pub mod pkce;
pub mod radius;
pub mod xai;

pub use anthropic::anthropic_oauth;
pub use callback_page::{
    oauth_error_html, oauth_success_html, CallbackCode, CallbackPageCopy, OAuthCallbackServer,
};
pub use device_code::{poll_oauth_device_code_flow, DeviceCodePollOptions, DeviceCodePollResult};
pub use github_copilot::github_copilot_oauth;
pub use kimi_coding::kimi_coding_oauth;
pub use load::{load_oauth_flow, load_xai_oauth, OAuthFlowLoader};
pub use openai_codex::openai_codex_oauth;
pub use openrouter::openrouter_oauth;
pub use pkce::{generate_pkce, Pkce};
pub use radius::{create_radius_oauth, RadiusOAuthOptions};
pub use xai::xai_oauth;
