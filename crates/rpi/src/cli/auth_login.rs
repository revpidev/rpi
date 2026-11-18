//! Shell `auth login` (cli/auth-login.ts @ b4afdddf, commit 9b9cb30ae).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rpi_ai::auth::{
    AuthEvent, AuthInteraction, AuthPrompt, AuthType, LoginOptions, ModelsError, ModelsErrorCode,
    SelectOption,
};
use rpi_ai::models::{ModelsRefreshOptions, Provider};
use tokio_util::sync::CancellationToken;

use crate::cli::auth_terminal::{LoginTerminal, display_url, replace_control_characters, web_url};
use crate::core::model_runtime::ModelRuntime;
use crate::core::preferred_providers::compare_providers;
use crate::core::radius::{RADIUS_PROVIDER_ID, plan_radius_mcp_server};

pub const AUTH_LOGIN_USAGE: &str = "rpi auth login <provider> [--api-key] [--no-browser]";

pub struct AuthLoginOptions {
    pub model_runtime: Arc<ModelRuntime>,
    pub terminal: Arc<dyn LoginTerminal>,
    pub agent_dir: PathBuf,
    pub cwd: PathBuf,
    pub get_device_id: Arc<dyn Fn() -> String + Send + Sync>,
    pub signal: CancellationToken,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LoginRequest {
    provider_ref: String,
    api_key: bool,
    browser: bool,
}

fn parse_login_request(args: &[String]) -> Result<LoginRequest, String> {
    let usage = || format!("Usage: {AUTH_LOGIN_USAGE}");
    let mut request = LoginRequest {
        provider_ref: String::new(),
        api_key: false,
        browser: true,
    };
    for arg in args {
        match arg.as_str() {
            "--api-key" => request.api_key = true,
            "--no-browser" => request.browser = false,
            _ if arg.starts_with('-') || !request.provider_ref.is_empty() => {
                return Err(usage());
            }
            _ => request.provider_ref = arg.clone(),
        }
    }
    if request.provider_ref.is_empty() {
        return Err(usage());
    }
    if request.api_key && !request.browser {
        return Err(
            "--no-browser only applies to account sign-in; omit it with --api-key.".to_owned(),
        );
    }
    Ok(request)
}

pub(crate) fn matching_providers(
    runtime: &ModelRuntime,
    provider_ref: &str,
) -> Vec<Arc<dyn Provider>> {
    let normalized = provider_ref.trim().to_lowercase();
    if normalized.is_empty() {
        return Vec::new();
    }
    let mut providers = runtime.get_providers();
    providers
        .sort_by(|left, right| compare_providers(left.id(), left.name(), right.id(), right.name()));
    providers
        .into_iter()
        .filter(|provider| {
            provider.id().to_lowercase() == normalized
                || provider.name().to_lowercase() == normalized
        })
        .collect()
}

pub fn select_auth_type(provider: &dyn Provider, api_key: bool) -> Result<AuthType, String> {
    let has_api_key_login = provider
        .auth()
        .api_key
        .as_ref()
        .is_some_and(|auth| auth.supports_login());
    if !api_key && provider.auth().oauth.is_some() {
        return Ok(AuthType::Oauth);
    }
    if has_api_key_login {
        return Ok(AuthType::ApiKey);
    }
    if api_key && provider.auth().oauth.is_some() {
        return Err(format!(
            "{} has no API key login. Omit --api-key to sign in.",
            provider.name()
        ));
    }
    let method = provider
        .auth()
        .api_key
        .as_ref()
        .map(|auth| auth.name())
        .unwrap_or("ambient credentials");
    Err(format!(
        "{} is configured outside {} ({method}).",
        provider.name(),
        crate::config::APP_NAME
    ))
}

pub async fn run_auth_login(args: &[String], options: AuthLoginOptions) -> i32 {
    let request = match parse_login_request(args) {
        Ok(request) => request,
        Err(message) => {
            options.terminal.error(&message);
            return 1;
        }
    };
    let provider = matching_providers(&options.model_runtime, &request.provider_ref)
        .into_iter()
        .next();
    let Some(provider) = provider else {
        options.terminal.error(&format!(
            "Unknown provider \"{}\".",
            replace_control_characters(&request.provider_ref)
        ));
        let mut login_providers = options
            .model_runtime
            .get_providers()
            .into_iter()
            .filter(|provider| {
                provider.auth().oauth.is_some()
                    || provider
                        .auth()
                        .api_key
                        .as_ref()
                        .is_some_and(|auth| auth.supports_login())
            })
            .map(|provider| provider.id().to_owned())
            .collect::<Vec<_>>();
        login_providers.sort();
        options
            .terminal
            .error(&format!("Providers: {}", login_providers.join(", ")));
        return 1;
    };
    if !options.terminal.interactive() {
        options.terminal.error(&format!(
            "\"{AUTH_LOGIN_USAGE}\" requires an interactive terminal"
        ));
        return 1;
    }
    let auth_type = match select_auth_type(provider.as_ref(), request.api_key) {
        Ok(auth_type) => auth_type,
        Err(message) => {
            options.terminal.error(&message);
            return 1;
        }
    };

    let interaction = CliAuthInteraction {
        provider_id: provider.id().to_owned(),
        auth_type,
        browser: request.browser,
        terminal: Arc::clone(&options.terminal),
        signal: options.signal.clone(),
        skipped_methods: Mutex::new(Vec::new()),
    };
    let login_options = LoginOptions {
        get_device_id: Some(options.get_device_id.clone()),
        agent_name: None,
    };
    if let Err(error) = options
        .model_runtime
        .login(provider.id(), auth_type, &interaction, Some(&login_options))
        .await
    {
        if error.message.starts_with("Credential login committed") {
            options.terminal.error(&format!(
                "Logged in to {}, but local model state could not be synchronized: {}",
                provider.name(),
                error.message
            ));
        } else if options.signal.is_cancelled() || error.message == "Login cancelled" {
            options.terminal.error("Login cancelled.");
        } else {
            options.terminal.error(&format!(
                "Failed to log in to {}: {}",
                provider.name(),
                error.message
            ));
        }
        return 1;
    }

    options.terminal.write(&format!(
        "{}. Credentials saved to {}",
        if auth_type == AuthType::Oauth {
            format!("Logged in to {}", provider.name())
        } else {
            format!("Saved API key for {}", provider.name())
        },
        options.agent_dir.join("auth.json").display()
    ));
    refresh_provider_catalog(&provider, &options).await;
    if provider.id() == RADIUS_PROVIDER_ID {
        configure_radius_mcp_server(&provider, &options);
    }
    0
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MethodChoice {
    Ask,
    Browser,
    Other,
}

struct CliAuthInteraction {
    provider_id: String,
    auth_type: AuthType,
    browser: bool,
    terminal: Arc<dyn LoginTerminal>,
    signal: CancellationToken,
    skipped_methods: Mutex<Vec<String>>,
}

impl CliAuthInteraction {
    fn method_choice(&self) -> MethodChoice {
        if self.auth_type == AuthType::ApiKey {
            MethodChoice::Ask
        } else if self.browser {
            MethodChoice::Browser
        } else {
            MethodChoice::Other
        }
    }

    fn cancelled() -> ModelsError {
        ModelsError::new(ModelsErrorCode::Auth, "Login cancelled")
    }

    async fn select_in_terminal(
        &self,
        message: &str,
        options: &[SelectOption],
        prompt_signal: Option<CancellationToken>,
    ) -> Result<String, ModelsError> {
        self.terminal.write(message);
        for (index, option) in options.iter().enumerate() {
            self.terminal.write(&format!(
                "  {}. {}{}",
                index + 1,
                option.label,
                option
                    .description
                    .as_ref()
                    .map(|description| format!(" - {description}"))
                    .unwrap_or_default()
            ));
        }
        loop {
            let prompt = format!("Select 1-{}: ", options.len());
            let question = self.terminal.question(&prompt);
            tokio::pin!(question);
            let answer = tokio::select! {
                answer = &mut question => answer?,
                _ = self.signal.cancelled() => return Err(Self::cancelled()),
                _ = async {
                    if let Some(signal) = prompt_signal.clone() {
                        signal.cancelled().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => return Err(Self::cancelled()),
            };
            let answer = answer.trim();
            if !answer.is_empty()
                && answer.bytes().all(|byte| byte.is_ascii_digit())
                && let Ok(index) = answer.parse::<usize>()
                && let Some(option) = index.checked_sub(1).and_then(|index| options.get(index))
            {
                return Ok(option.id.clone());
            }
            self.terminal
                .write(&format!("Enter a number from 1 to {}.", options.len()));
        }
    }

    async fn select_method(
        &self,
        message: &str,
        options: &[SelectOption],
        signal: Option<CancellationToken>,
    ) -> Result<String, ModelsError> {
        let Some((first, others)) = options.split_first() else {
            return self.select_in_terminal(message, options, signal).await;
        };
        match self.method_choice() {
            MethodChoice::Ask => self.select_in_terminal(message, options, signal).await,
            MethodChoice::Browser => {
                *self
                    .skipped_methods
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) =
                    others.iter().map(|option| option.label.clone()).collect();
                Ok(first.id.clone())
            }
            MethodChoice::Other => match others {
                [] => Ok(first.id.clone()),
                [only_other] => {
                    self.terminal
                        .write(&format!("{message} {}", only_other.label));
                    Ok(only_other.id.clone())
                }
                _ => self.select_in_terminal(message, others, signal).await,
            },
        }
    }

    async fn answer_prompt(
        &self,
        message: String,
        secret: bool,
        signal: Option<CancellationToken>,
    ) -> Result<String, ModelsError> {
        let answer = self.terminal.question(&message);
        let secret_answer = self.terminal.secret(&message);
        tokio::pin!(answer);
        tokio::pin!(secret_answer);
        tokio::select! {
            answer = &mut answer, if !secret => answer,
            answer = &mut secret_answer, if secret => answer,
            _ = self.signal.cancelled() => Err(Self::cancelled()),
            _ = async {
                if let Some(signal) = signal {
                    signal.cancelled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => Err(Self::cancelled()),
        }
    }
}

impl AuthInteraction for CliAuthInteraction {
    fn signal(&self) -> Option<CancellationToken> {
        Some(self.signal.clone())
    }

    fn prompt<'a>(
        &'a self,
        prompt: AuthPrompt,
    ) -> rpi_ai::auth::BoxFutureSend<'a, Result<String, ModelsError>> {
        Box::pin(async move {
            match prompt {
                AuthPrompt::Select {
                    message,
                    options,
                    signal,
                } => self.select_method(&message, &options, signal).await,
                AuthPrompt::Secret {
                    message,
                    placeholder,
                    signal,
                } => {
                    let placeholder = placeholder
                        .map(|placeholder| format!(" ({placeholder})"))
                        .unwrap_or_default();
                    self.answer_prompt(format!("{message}{placeholder}: "), true, signal)
                        .await
                }
                AuthPrompt::Text {
                    message,
                    placeholder,
                    signal,
                }
                | AuthPrompt::ManualCode {
                    message,
                    placeholder,
                    signal,
                } => {
                    let placeholder = placeholder
                        .map(|placeholder| format!(" ({placeholder})"))
                        .unwrap_or_default();
                    self.answer_prompt(format!("{message}{placeholder}: "), false, signal)
                        .await
                }
            }
        })
    }

    fn notify(&self, event: AuthEvent) {
        match event {
            AuthEvent::AuthUrl { url, instructions } => {
                self.terminal.write(&format!(
                    "Open this URL to sign in:\n  {}",
                    display_url(&url)
                ));
                if let Some(instructions) = instructions {
                    self.terminal.write(&instructions);
                }
                if self.browser
                    && let Some(target) = web_url(&url)
                {
                    self.terminal.open_url(&target);
                }
                let skipped = self
                    .skipped_methods
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                let rerun = format!(
                    "\"{} auth login {} --no-browser\"",
                    crate::config::APP_NAME,
                    self.provider_id
                );
                match skipped.as_slice() {
                    [] => {}
                    [only] => self
                        .terminal
                        .write(&format!("To use \"{only}\" instead, run {rerun}")),
                    methods => {
                        let labels = methods
                            .iter()
                            .map(|method| format!("\"{method}\""))
                            .collect::<Vec<_>>()
                            .join(", ");
                        self.terminal.write(&format!(
                            "Other sign-in methods: {labels}. Run {rerun} to choose one."
                        ));
                    }
                }
            }
            AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                open_browser_on_confirm,
                ..
            } => {
                self.terminal.write(&format!(
                    "Open {} and enter code: {user_code}",
                    display_url(&verification_uri)
                ));
                if self.browser
                    && open_browser_on_confirm.unwrap_or(true)
                    && let Some(target) = web_url(&verification_uri)
                {
                    self.terminal.open_url(&target);
                }
                self.terminal.write("Waiting for authentication...");
            }
            AuthEvent::Info { message, links } => {
                self.terminal.write(&message);
                for link in links.unwrap_or_default() {
                    self.terminal.write(&format!(
                        "  {}{}",
                        link.label
                            .map(|label| format!("{label}: "))
                            .unwrap_or_default(),
                        display_url(&link.url)
                    ));
                }
            }
            AuthEvent::Progress { message } => self.terminal.write(&message),
        }
    }
}

async fn refresh_provider_catalog(provider: &Arc<dyn Provider>, options: &AuthLoginOptions) {
    let cancelled = || {
        options.terminal.write(&format!(
            "Skipped the {} model catalog refresh; {} refreshes it on next start.",
            provider.name(),
            crate::config::APP_NAME
        ));
    };
    let signal = CancellationToken::new();
    let cancel = signal.clone();
    let global = options.signal.clone();
    let cancel_task = tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(15)) => cancel.cancel(),
            _ = global.cancelled() => cancel.cancel(),
        }
    });
    let result = options
        .model_runtime
        .refresh(Some(ModelsRefreshOptions {
            providers: Some(vec![provider.id().to_owned()]),
            signal: Some(signal),
            ..Default::default()
        }))
        .await;
    cancel_task.abort();
    if options.signal.is_cancelled() {
        cancelled();
    } else if result.aborted {
        options.terminal.write(&format!(
            "The {} model catalog refresh timed out; {} refreshes it on next start.",
            provider.name(),
            crate::config::APP_NAME
        ));
    } else if !result.errors.is_empty() {
        options.terminal.write(&format!(
            "The {} model catalog could not be refreshed; {} retries on next start.",
            provider.name(),
            crate::config::APP_NAME
        ));
    }
}

pub fn configure_radius_mcp_server(provider: &Arc<dyn Provider>, options: &AuthLoginOptions) {
    let Some(plan) = plan_radius_mcp_server(provider.id(), &options.agent_dir, &options.cwd) else {
        return;
    };
    match crate::extensions::mcp::config::add_mcp_server_config(
        &plan.mcp_path,
        &plan.name,
        &plan.config,
    ) {
        Ok(replaced) => options.terminal.write(&format!(
            "{} MCP server \"{}\" in {} to use the {} login.",
            if replaced { "Updated" } else { "Added" },
            plan.name,
            plan.mcp_path.display(),
            provider.name()
        )),
        Err(error) => options.terminal.error(&format!(
            "Could not update {}: {error}",
            plan.mcp_path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_request_validates_flag_combinations() {
        assert_eq!(
            parse_login_request(&["OpenAI".to_owned(), "--api-key".to_owned()]),
            Ok(LoginRequest {
                provider_ref: "OpenAI".to_owned(),
                api_key: true,
                browser: true,
            })
        );
        assert!(parse_login_request(&["openai".to_owned(), "--no-browser".to_owned()]).is_ok());
        assert!(
            parse_login_request(&[
                "openai".to_owned(),
                "--api-key".to_owned(),
                "--no-browser".to_owned()
            ])
            .is_err()
        );
        assert!(parse_login_request(&[]).is_err());
        assert!(parse_login_request(&["openai".to_owned(), "extra".to_owned()]).is_err());
    }
}
