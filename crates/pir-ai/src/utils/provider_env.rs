//! Port of `packages/ai/src/utils/provider-env.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional difference: the Bun `/proc/self/environ` sandbox fallback is
//! Bun-specific and is not ported (pir is a native binary; `std::env::var`
//! always sees the real environment).

use crate::types::ProviderEnv;

/// `getProviderEnvValue`: scoped overrides win over `std::env`.
pub fn get_provider_env_value(name: &str, env: Option<&ProviderEnv>) -> Option<String> {
    env.and_then(|env| env.get(name).cloned())
        .or_else(|| std::env::var(name).ok())
}
