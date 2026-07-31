//! Port of `packages/ai/src/auth/oauth/` @ pi 0.82.1 (2efa728) — OAuth flow
//! building blocks: PKCE (`pkce`), the RFC 8628 device-code polling framework
//! (`device_code`), the localhost callback page/server (`callback_page`) and
//! the Anthropic OAuth flow (`anthropic`).

pub mod anthropic;
pub mod callback_page;
pub mod device_code;
pub mod pkce;

pub use anthropic::anthropic_oauth;
pub use callback_page::{
    oauth_error_html, oauth_success_html, CallbackCode, CallbackPageCopy, OAuthCallbackServer,
};
pub use device_code::{poll_oauth_device_code_flow, DeviceCodePollOptions, DeviceCodePollResult};
pub use pkce::{generate_pkce, Pkce};
