//! Extensions superseded by a host built-in (rpi-own; no upstream
//! counterpart).
//!
//! v0.1.6 moved MCP into the host (`builtin:mcp`,
//! [`crate::extensions::mcp`]), which retires the first-party
//! `rpi-mcp-adapter` extension's reason to exist. The extension stays
//! loadable — it is still the optional-replacement escape hatch for users
//! who intentionally keep it — but it is deprecated:
//!
//! - every `rpi update` path (extension update included) skips it, so it is
//!   never upgraded again ([`crate::core::package_manager`] consumes
//!   [`DeprecatedExtension`] records);
//! - the startup pipeline reports a warning recommending `rpi remove`
//!   whenever the extension is found in an extension install root, even
//!   when it is disabled or not loaded
//!   ([`installed_deprecated_extensions`]).
//!
//! Detection is install-directory based: the manifest `name` when readable,
//! else the directory name (mirroring the untracked-install discovery in the
//! package manager). `github:` installs recover the original source from
//! [`extension_registry::GITHUB_INSTALL_MARKER_FILE`], so the recommendation
//! names the exact `rpi remove` argument.

use std::path::{Path, PathBuf};

use crate::config::APP_NAME;
use crate::core::extension_registry;

/// One extension whose capability moved into the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeprecatedExtension {
    /// Manifest / registry / install-directory name.
    pub name: &'static str,
    /// The host capability that replaced it (message copy).
    pub replacement: &'static str,
}

/// Deprecated first-party extensions. Keep in sync with the official-site
/// registry entries (`rpi-pages/registry/<name>.json`, `"deprecated": true`)
/// and the release packaging list (`.github/workflows/build.yml`
/// `EXT_CRATES` — deprecated extensions are no longer packaged).
pub const DEPRECATED_EXTENSIONS: &[DeprecatedExtension] = &[DeprecatedExtension {
    name: "rpi-mcp-adapter",
    replacement: "MCP",
}];

/// The deprecated-extension record for `name`, when present.
pub fn deprecated_extension(name: &str) -> Option<&'static DeprecatedExtension> {
    DEPRECATED_EXTENSIONS
        .iter()
        .find(|entry| entry.name == name)
}

/// Name recorded by an installed extension directory: the manifest `name`
/// when readable, else the directory name.
pub fn installed_extension_dir_name(dir: &Path) -> Option<String> {
    extension_registry::installed_extension_name(dir).or_else(|| {
        dir.file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
    })
}

/// A deprecated extension found in an extension install root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledDeprecatedExtension {
    pub extension: &'static DeprecatedExtension,
    /// The install directory (`<root>/<name>/`).
    pub dir: PathBuf,
    /// The project install root (`<cwd>/.rpi/extensions`); the `rpi remove`
    /// recommendation then carries `-l`.
    pub project_scope: bool,
}

impl InstalledDeprecatedExtension {
    /// The `<source>` argument for `rpi remove`: the
    /// [`extension_registry::GITHUB_INSTALL_MARKER_FILE`] content for
    /// `github:` installs, else the installed extension name.
    pub fn removal_source(&self) -> String {
        removal_source_for_dir(&self.dir, self.extension.name)
    }

    /// `rpi remove <source> [-l]`.
    pub fn removal_command(&self) -> String {
        removal_command(&self.removal_source(), self.project_scope)
    }

    /// Startup warning recommending removal.
    pub fn startup_message(&self) -> String {
        format!(
            "The \"{}\" extension is deprecated and will no longer be updated: {} is now built \
             into rpi. Uninstall it with \"{}\" to use the built-in extension.",
            self.extension.name,
            self.extension.replacement,
            self.removal_command()
        )
    }
}

/// `rpi remove <removal_source> [-l]`.
pub fn removal_command(removal_source: &str, project_scope: bool) -> String {
    if project_scope {
        format!("{APP_NAME} remove {removal_source} -l")
    } else {
        format!("{APP_NAME} remove {removal_source}")
    }
}

/// Update-flow skip note (`DefaultPackageManager::update_configured_sources`):
/// the source being skipped and the exact removal command.
pub fn update_skip_message(
    extension: &DeprecatedExtension,
    source: &str,
    removal_source: &str,
    project_scope: bool,
) -> String {
    format!(
        "Skipping {source}: the \"{}\" extension is deprecated and will no longer be updated ({} \
         is now built into rpi). Uninstall it with \"{}\".",
        extension.name,
        extension.replacement,
        removal_command(removal_source, project_scope)
    )
}

/// Installed deprecated extensions under the extension install roots: the
/// user root (`<agent_dir>/extensions`) always, the project root
/// (`<cwd>/.rpi/extensions`) only for a trusted project. Staging directories
/// (`.tmp-*` / `.old-*`) are skipped, mirroring the package manager's
/// untracked-install discovery.
pub fn installed_deprecated_extensions(
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
) -> Vec<InstalledDeprecatedExtension> {
    let mut found = Vec::new();
    collect_installed(&agent_dir.join("extensions"), false, &mut found);
    if project_trusted {
        collect_installed(
            &crate::config::get_project_config_dir(cwd).join("extensions"),
            true,
            &mut found,
        );
    }
    found
}

fn collect_installed(
    root: &Path,
    project_scope: bool,
    found: &mut Vec<InstalledDeprecatedExtension>,
) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(dir_name) = dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if dir_name.contains(".tmp-") || dir_name.contains(".old-") {
            continue;
        }
        let Some(name) = installed_extension_dir_name(&dir) else {
            continue;
        };
        let Some(extension) = deprecated_extension(&name) else {
            continue;
        };
        found.push(InstalledDeprecatedExtension {
            extension,
            dir,
            project_scope,
        });
    }
}

/// The `<source>` argument for `rpi remove` for an installed extension
/// directory: the [`extension_registry::GITHUB_INSTALL_MARKER_FILE`] content
/// for `github:` installs, else the installed manifest name (falling back to
/// `fallback`).
pub fn removal_source_for_dir(dir: &Path, fallback: &str) -> String {
    match std::fs::read_to_string(dir.join(extension_registry::GITHUB_INSTALL_MARKER_FILE)) {
        Ok(marker) if !marker.trim().is_empty() => marker.trim().to_owned(),
        _ => {
            extension_registry::installed_extension_name(dir).unwrap_or_else(|| fallback.to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let unique = format!(
                "rpi-deprecated-ext-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            );
            let root = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&root).unwrap();
            TestDir(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_manifest(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("rpi-extension.json"),
            format!("{{\"name\": \"{name}\"}}"),
        )
        .unwrap();
    }

    #[test]
    fn deprecated_extension_matches_only_the_table() {
        let entry = deprecated_extension("rpi-mcp-adapter").expect("table entry");
        assert_eq!(entry.replacement, "MCP");
        assert!(deprecated_extension("rpi-subagents").is_none());
        assert!(deprecated_extension("rpi-mcp-adapter-extra").is_none());
    }

    #[test]
    fn scan_finds_manifest_name_and_dir_name_and_skips_staging_dirs() {
        let dir = TestDir::new();
        let root = dir.path().join("extensions");
        // A renamed directory still matches through the manifest name.
        write_manifest(&root.join("my-mcp"), "rpi-mcp-adapter");
        // Without a readable manifest the directory name is the identity.
        std::fs::create_dir_all(root.join("rpi-mcp-adapter")).unwrap();
        write_manifest(&root.join("rpi-todo"), "rpi-todo");
        // Staging directories are never surfaced.
        write_manifest(&root.join("rpi-mcp-adapter.tmp-1"), "rpi-mcp-adapter");
        let found = installed_deprecated_extensions(dir.path(), dir.path(), false);
        let mut names: Vec<String> = found
            .iter()
            .map(|item| item.dir.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["my-mcp", "rpi-mcp-adapter"]);
        assert!(
            found
                .iter()
                .all(|item| item.extension.name == "rpi-mcp-adapter")
        );
        assert!(found.iter().all(|item| !item.project_scope));
    }

    #[test]
    fn scan_gates_the_project_root_on_trust() {
        let dir = TestDir::new();
        let cwd = dir.path().join("project");
        write_manifest(
            &crate::config::get_project_config_dir(&cwd).join("extensions/rpi-mcp-adapter"),
            "rpi-mcp-adapter",
        );
        let untrusted = installed_deprecated_extensions(&dir.path().join("agent"), &cwd, false);
        assert!(untrusted.is_empty());
        let trusted = installed_deprecated_extensions(&dir.path().join("agent"), &cwd, true);
        assert_eq!(trusted.len(), 1);
        assert!(trusted[0].project_scope);
        assert_eq!(
            trusted[0].removal_command(),
            "rpi remove rpi-mcp-adapter -l"
        );
    }

    #[test]
    fn removal_source_prefers_the_github_install_marker() {
        let dir = TestDir::new();
        let install = dir.path().join("extensions/rpi-mcp-adapter");
        write_manifest(&install, "rpi-mcp-adapter");
        std::fs::write(
            install.join(extension_registry::GITHUB_INSTALL_MARKER_FILE),
            "github:revpidev/rpi\n",
        )
        .unwrap();
        let found = installed_deprecated_extensions(dir.path(), dir.path(), false);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].removal_source(), "github:revpidev/rpi");
        assert_eq!(found[0].removal_command(), "rpi remove github:revpidev/rpi");
        assert_eq!(
            found[0].startup_message(),
            "The \"rpi-mcp-adapter\" extension is deprecated and will no longer be updated: MCP \
             is now built into rpi. Uninstall it with \"rpi remove github:revpidev/rpi\" to use \
             the built-in extension."
        );
    }

    #[test]
    fn update_skip_message_names_source_and_removal_command() {
        let extension = deprecated_extension("rpi-mcp-adapter").unwrap();
        assert_eq!(
            update_skip_message(extension, "rpi-mcp-adapter", "rpi-mcp-adapter", false),
            "Skipping rpi-mcp-adapter: the \"rpi-mcp-adapter\" extension is deprecated and will \
             no longer be updated (MCP is now built into rpi). Uninstall it with \"rpi remove \
             rpi-mcp-adapter\"."
        );
        assert_eq!(
            update_skip_message(extension, "rpi-mcp-adapter@^0.1", "rpi-mcp-adapter", true),
            "Skipping rpi-mcp-adapter@^0.1: the \"rpi-mcp-adapter\" extension is deprecated and \
             will no longer be updated (MCP is now built into rpi). Uninstall it with \"rpi \
             remove rpi-mcp-adapter -l\"."
        );
    }
}
