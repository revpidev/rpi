//! Client identity and provider attribution (port of
//! `packages/ai/src/utils/identity.ts` @ 4a080252f, adc5fa9f8 / 04a93bad8).

use serde::{Deserialize, Serialize};

use crate::types::{ClassifierModel, ImageModel, Model, ProviderHeaders};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserAgentMode {
    Short,
    Detailed,
    Custom(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientAttribution {
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub categories: Option<Vec<String>>,
}

pub fn rpi_attribution() -> ClientAttribution {
    ClientAttribution {
        name: "rpi".to_owned(),
        url: "https://revpi.dev".to_owned(),
        categories: Some(vec!["cli-agent".to_owned()]),
    }
}

/// Plain, non-secret identity for provider requests. `attribution: None` is a
/// deliberate off switch; an outer `None` means the field was not overridden.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<UserAgentMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<Option<ClientAttribution>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl ClientIdentity {
    pub fn new(version: &str, attribution_enabled: bool) -> Self {
        Self {
            user_agent: Some(UserAgentMode::Detailed),
            attribution: Some(attribution_enabled.then(rpi_attribution)),
            version: Some(version.to_owned()),
        }
    }
}

/// Later sources win per field; an explicit attribution null overrides an
/// inherited attribution instead of being skipped.
pub fn merge_client_identity(
    base: Option<ClientIdentity>,
    override_: Option<ClientIdentity>,
) -> Option<ClientIdentity> {
    let Some(override_) = override_ else {
        return base;
    };
    let mut merged = base.unwrap_or_default();
    if override_.user_agent.is_some() {
        merged.user_agent = override_.user_agent;
    }
    if override_.attribution.is_some() {
        merged.attribution = override_.attribution;
    }
    if override_.version.is_some() {
        merged.version = override_.version;
    }
    Some(merged)
}

/// Return a `Models` handle whose default identity overrides the previous
/// default field by field. Provider registries and all other state stay shared.
pub fn with_client_identity(
    models: &crate::models::Models,
    identity: ClientIdentity,
) -> crate::models::Models {
    let mut models = models.clone();
    models.set_identity(identity);
    models
}

pub fn format_user_agent(identity: Option<&ClientIdentity>) -> String {
    let Some(identity) = identity else {
        return detailed_user_agent(env!("CARGO_PKG_VERSION"));
    };
    match &identity.user_agent {
        Some(UserAgentMode::Short) => "rpi".to_owned(),
        Some(UserAgentMode::Custom(custom)) => custom.clone(),
        _ => detailed_user_agent(
            identity
                .version
                .as_deref()
                .unwrap_or(env!("CARGO_PKG_VERSION")),
        ),
    }
}

fn detailed_user_agent(version: &str) -> String {
    format!(
        "rpi/{version} ({}; rust/{}; {})",
        std::env::consts::OS,
        crate::utils::rpi_user_agent::os_release(),
        std::env::consts::ARCH
    )
}

pub struct ClientIdentityTarget<'a> {
    pub provider: &'a str,
    pub base_url: &'a str,
}

impl<'a> From<&'a Model> for ClientIdentityTarget<'a> {
    fn from(model: &'a Model) -> Self {
        Self {
            provider: &model.provider,
            base_url: &model.base_url,
        }
    }
}

impl<'a> From<&'a ClassifierModel> for ClientIdentityTarget<'a> {
    fn from(model: &'a ClassifierModel) -> Self {
        Self {
            provider: &model.provider,
            base_url: &model.base_url,
        }
    }
}

impl<'a> From<&'a ImageModel> for ClientIdentityTarget<'a> {
    fn from(model: &'a ImageModel) -> Self {
        Self {
            provider: &model.provider,
            base_url: &model.base_url,
        }
    }
}

impl<'a> From<&'a crate::types::ImagesModel> for ClientIdentityTarget<'a> {
    fn from(model: &'a crate::types::ImagesModel) -> Self {
        Self {
            provider: &model.provider,
            base_url: &model.base_url,
        }
    }
}

fn matches_host(base_url: &str, expected_host: &str) -> bool {
    url::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(|host| host == expected_host))
        .unwrap_or(false)
}

fn is_openrouter(target: &ClientIdentityTarget) -> bool {
    target.provider == "openrouter" || target.base_url.contains("openrouter.ai")
}

fn is_nvidia(target: &ClientIdentityTarget) -> bool {
    target.provider == "nvidia" || matches_host(target.base_url, "integrate.api.nvidia.com")
}

fn is_opencode(target: &ClientIdentityTarget) -> bool {
    matches!(target.provider, "opencode" | "opencode-go")
        || matches_host(target.base_url, "opencode.ai")
}

/// OpenCode's per-conversation routing header, including custom providers
/// targeting its host. This is request auth context, not identity attribution.
pub fn get_opencode_session_headers(
    target: ClientIdentityTarget,
    session_id: Option<&str>,
) -> ProviderHeaders {
    match session_id.filter(|session_id| !session_id.is_empty()) {
        Some(session_id) if is_opencode(&target) => {
            [("x-opencode-session".to_owned(), Some(session_id.to_owned()))].into()
        }
        _ => ProviderHeaders::new(),
    }
}

/// Provider attribution headers for an identity. Empty when attribution is off.
pub fn get_attribution_headers(
    target: ClientIdentityTarget,
    identity: Option<&ClientIdentity>,
) -> ProviderHeaders {
    let Some(attribution) = identity
        .and_then(|identity| identity.attribution.as_ref())
        .and_then(|attribution| attribution.as_ref())
    else {
        return ProviderHeaders::new();
    };
    if is_openrouter(&target) {
        let mut headers: ProviderHeaders = [
            ("HTTP-Referer".to_owned(), Some(attribution.url.clone())),
            (
                "X-OpenRouter-Title".to_owned(),
                Some(attribution.name.clone()),
            ),
        ]
        .into_iter()
        .collect();
        if let Some(categories) = attribution
            .categories
            .as_ref()
            .filter(|categories| !categories.is_empty())
        {
            headers.insert(
                "X-OpenRouter-Categories".to_owned(),
                Some(categories.join(",")),
            );
        }
        return headers;
    }
    if is_nvidia(&target) {
        let mut name = attribution.name.clone();
        if let Some(first) = name.chars().next() {
            name.replace_range(..first.len_utf8(), &first.to_uppercase().to_string());
        }
        return [("X-BILLING-INVOKE-ORIGIN".to_owned(), Some(name))].into();
    }
    if is_opencode(&target) {
        return [(
            "x-opencode-client".to_owned(),
            Some(attribution.name.clone()),
        )]
        .into();
    }
    ProviderHeaders::new()
}

/// `User-Agent` and attribution, applied below model/request headers in each
/// adapter and after `before_provider_headers` (`transformHeaders`).
pub fn get_identity_headers(
    target: ClientIdentityTarget,
    identity: Option<&ClientIdentity>,
) -> ProviderHeaders {
    let mut headers = get_attribution_headers(target, identity);
    headers.insert("User-Agent".to_owned(), Some(format_user_agent(identity)));
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_merges_identity_without_turning_on_attribution() {
        assert!(format_user_agent(None).starts_with("rpi/"));
        assert_eq!(
            format_user_agent(Some(&ClientIdentity {
                user_agent: Some(UserAgentMode::Short),
                ..Default::default()
            })),
            "rpi"
        );
        let base = ClientIdentity::new("1.0.0", true);
        let merged = merge_client_identity(
            Some(base),
            Some(ClientIdentity {
                attribution: Some(None),
                ..Default::default()
            }),
        )
        .expect("merged");
        assert_eq!(merged.attribution, Some(None));
        assert_eq!(merged.version.as_deref(), Some("1.0.0"));
        assert!(
            get_attribution_headers(
                ClientIdentityTarget {
                    provider: "openrouter",
                    base_url: "https://openrouter.ai/api/v1",
                },
                Some(&merged),
            )
            .is_empty()
        );
    }

    #[test]
    fn maps_provider_attribution_and_keeps_gateways_separate() {
        let identity = ClientIdentity::new("1.0.0", true);
        let openrouter = get_attribution_headers(
            ClientIdentityTarget {
                provider: "openrouter",
                base_url: "https://openrouter.ai/api/v1",
            },
            Some(&identity),
        );
        assert_eq!(
            openrouter.get("HTTP-Referer"),
            Some(&Some("https://revpi.dev".to_owned()))
        );
        assert_eq!(
            openrouter.get("X-OpenRouter-Title"),
            Some(&Some("rpi".to_owned()))
        );
        assert_eq!(
            openrouter.get("X-OpenRouter-Categories"),
            Some(&Some("cli-agent".to_owned()))
        );
        assert!(!openrouter.contains_key("X-BILLING-INVOKE-ORIGIN"));

        let nvidia = get_attribution_headers(
            ClientIdentityTarget {
                provider: "custom",
                base_url: "https://integrate.api.nvidia.com/v1",
            },
            Some(&identity),
        );
        assert_eq!(
            nvidia.get("X-BILLING-INVOKE-ORIGIN"),
            Some(&Some("Rpi".to_owned()))
        );
        let opencode = get_attribution_headers(
            ClientIdentityTarget {
                provider: "opencode-go",
                base_url: "https://opencode.ai/inference/go/v1",
            },
            Some(&identity),
        );
        assert_eq!(
            opencode.get("x-opencode-client"),
            Some(&Some("rpi".to_owned()))
        );
    }
}
