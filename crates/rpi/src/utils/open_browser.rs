use std::process::{Command, Stdio};

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrowserPlatform {
    MacOs,
    Windows,
    Other,
}

impl BrowserPlatform {
    #[cfg(target_os = "macos")]
    const CURRENT: Self = Self::MacOs;
    #[cfg(target_os = "windows")]
    const CURRENT: Self = Self::Windows;
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    const CURRENT: Self = Self::Other;
}

fn browser_command(platform: BrowserPlatform, target: &str) -> Command {
    let mut command = match platform {
        BrowserPlatform::MacOs => {
            let mut command = Command::new("open");
            command.arg(target);
            command
        }
        BrowserPlatform::Windows => {
            let mut command = Command::new("rundll32");
            command.args(["url.dll,FileProtocolHandler", target]);
            command
        }
        BrowserPlatform::Other => {
            let mut command = Command::new("xdg-open");
            command.arg(target);
            command
        }
    };
    command
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Opens a URL or file with the platform handler without invoking a shell.
pub fn open_browser(target: &str) {
    let mut command = browser_command(BrowserPlatform::CURRENT, target);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, 0x0000_0008);
    let _ = command.spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn windows_metacharacters_are_one_rundll32_argument() {
        let target = "https://example.com/callback?code=a&next=|calc.exe^";
        let command = browser_command(BrowserPlatform::Windows, target);
        assert_eq!(command.get_program(), "rundll32");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["url.dll,FileProtocolHandler", target]
        );
        assert_eq!(command.get_current_dir(), Some(Path::new("/")));
    }

    #[test]
    fn posix_openers_pass_the_target_without_a_shell() {
        for (platform, program) in [
            (BrowserPlatform::MacOs, "open"),
            (BrowserPlatform::Other, "xdg-open"),
        ] {
            let target = "https://example.com/a&b";
            let command = browser_command(platform, target);
            assert_eq!(command.get_program(), program);
            assert_eq!(command.get_args().collect::<Vec<_>>(), [target]);
            assert_eq!(command.get_current_dir(), Some(Path::new("/")));
        }
    }
}
