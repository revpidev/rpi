//! Terminal I/O for shell authentication commands (cli/auth-terminal.ts and
//! utils/web-url.ts @ b4afdddf).

use std::io::{BufRead, IsTerminal, Write};

use async_trait::async_trait;
use rpi_ai::auth::{ModelsError, ModelsErrorCode};

pub trait AuthOutput: Send + Sync {
    fn write(&self, line: &str);
    fn error(&self, line: &str);
}

#[async_trait]
pub trait LoginTerminal: AuthOutput {
    fn interactive(&self) -> bool;
    async fn question(&self, message: &str) -> Result<String, ModelsError>;
    async fn secret(&self, message: &str) -> Result<String, ModelsError>;
    fn open_url(&self, url: &str);
}

#[derive(Default)]
pub struct ProcessLoginTerminal;

impl ProcessLoginTerminal {
    fn read_answer(message: &str, secret: bool) -> Result<String, ModelsError> {
        print!("{message}");
        std::io::stdout()
            .flush()
            .map_err(|error| ModelsError::new(ModelsErrorCode::Auth, error.to_string()))?;
        let _echo = secret.then(DisableEcho::new);
        let mut answer = String::new();
        let read = std::io::stdin()
            .lock()
            .read_line(&mut answer)
            .map_err(|error| ModelsError::new(ModelsErrorCode::Auth, error.to_string()))?;
        if secret {
            println!();
        }
        if read == 0 {
            return Err(ModelsError::new(ModelsErrorCode::Auth, "Login cancelled"));
        }
        Ok(answer.trim_end_matches(['\r', '\n']).to_owned())
    }
}

impl AuthOutput for ProcessLoginTerminal {
    fn write(&self, line: &str) {
        println!("{line}");
    }

    fn error(&self, line: &str) {
        eprintln!("{line}");
    }
}

#[async_trait]
impl LoginTerminal for ProcessLoginTerminal {
    fn interactive(&self) -> bool {
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
    }

    async fn question(&self, message: &str) -> Result<String, ModelsError> {
        let message = message.to_owned();
        tokio::task::spawn_blocking(move || Self::read_answer(&message, false))
            .await
            .map_err(|error| ModelsError::new(ModelsErrorCode::Auth, error.to_string()))?
    }

    async fn secret(&self, message: &str) -> Result<String, ModelsError> {
        let message = message.to_owned();
        tokio::task::spawn_blocking(move || Self::read_answer(&message, true))
            .await
            .map_err(|error| ModelsError::new(ModelsErrorCode::Auth, error.to_string()))?
    }

    fn open_url(&self, url: &str) {
        crate::utils::open_browser::open_browser(url);
    }
}

#[cfg(unix)]
struct DisableEcho {
    original: libc::termios,
    active: bool,
}

#[cfg(unix)]
impl DisableEcho {
    fn new() -> Self {
        unsafe {
            let mut original = std::mem::zeroed::<libc::termios>();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut original) == 0 {
                let mut no_echo = original;
                no_echo.c_lflag &= !libc::ECHO;
                if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &no_echo) == 0 {
                    return Self {
                        original,
                        active: true,
                    };
                }
            }
            Self {
                original: std::mem::zeroed(),
                active: false,
            }
        }
    }
}

#[cfg(unix)]
impl Drop for DisableEcho {
    fn drop(&mut self) {
        if self.active {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original);
            }
        }
    }
}

#[cfg(not(unix))]
struct DisableEcho;

#[cfg(not(unix))]
impl DisableEcho {
    fn new() -> Self {
        Self
    }
}

pub fn web_url(value: &str) -> Option<String> {
    rpi_ai::auth::oauth::trusted_url::trusted_http_url(value)
}

pub fn replace_control_characters(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| {
            if character.is_control() {
                character.escape_default().collect::<Vec<_>>()
            } else {
                vec![character]
            }
        })
        .collect()
}

pub fn display_url(value: &str) -> String {
    web_url(value).unwrap_or_else(|| replace_control_characters(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_urls_are_normalized_and_untrusted_text_cannot_escape() {
        assert_eq!(
            web_url("https://example.com/a b"),
            Some("https://example.com/a%20b".to_owned())
        );
        assert_eq!(web_url("javascript:alert(1)"), None);
        assert!(!display_url("https://example.com/\u{1b}]0;title").contains('\u{1b}'));
    }
}
