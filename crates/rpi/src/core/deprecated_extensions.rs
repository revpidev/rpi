//! Extensions superseded by a host built-in (rpi-own; no upstream
//! counterpart).
//!
//! The mechanism was introduced in v0.1.6 for `rpi-mcp-adapter` after MCP
//! moved into the host (`builtin:mcp`, [`crate::extensions::mcp`]) and the
//! extension was removed from the workspace/registry in v0.1.7 (ADR-0037,
//! TE49). ADR-0038 removes the remaining startup/update removal guidance in
//! v0.1.8: the table below is empty, so an already-installed copy produces
//! no deprecation warning and no update skip, while the generic mechanism
//! stays available for a future host replacement. No user file is deleted
//! automatically and the retired extension is not restored.
//!
//! When the table has an entry, every consumer behaves as before:
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

/// Deprecated first-party extensions. ADR-0038 removed the only historical
/// entry (`rpi-mcp-adapter`) together with its startup/update guidance in
/// v0.1.8; the table and the generic consumers stay for a future host
/// replacement. Deprecated extensions are not packaged (see
/// `.github/workflows/build.yml` `EXT_CRATES`).
pub const DEPRECATED_EXTENSIONS: &[DeprecatedExtension] = &[];

/// The deprecated-extension record for `name` when present in `entries`.
fn find_entry<'a>(
    entries: &'a [DeprecatedExtension],
    name: &str,
) -> Option<&'a DeprecatedExtension> {
    entries.iter().find(|entry| entry.name == name)
}

/// The deprecated-extension record for `name`, when present.
pub fn deprecated_extension(name: &str) -> Option<&'static DeprecatedExtension> {
    find_entry(DEPRECATED_EXTENSIONS, name)
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
    installed_deprecated_extensions_in(DEPRECATED_EXTENSIONS, agent_dir, cwd, project_trusted)
}

/// [`installed_deprecated_extensions`] over an explicit table (unit tests
/// exercise the retained mechanism without adding a production entry).
fn installed_deprecated_extensions_in(
    entries: &'static [DeprecatedExtension],
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
) -> Vec<InstalledDeprecatedExtension> {
    let mut found = Vec::new();
    collect_installed(entries, &agent_dir.join("extensions"), false, &mut found);
    if project_trusted {
        collect_installed(
            entries,
            &crate::config::get_project_config_dir(cwd).join("extensions"),
            true,
            &mut found,
        );
    }
    found
}

fn collect_installed(
    table: &'static [DeprecatedExtension],
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
        let Some(extension) = find_entry(table, &name) else {
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

    const TEST_ENTRIES: &[DeprecatedExtension] = &[DeprecatedExtension {
        name: "test-replaced",
        replacement: "Test capability",
    }];

    #[test]
    fn retired_mcp_adapter_is_no_longer_deprecated() {
        assert!(DEPRECATED_EXTENSIONS.is_empty());
        assert!(deprecated_extension("rpi-mcp-adapter").is_none());
        assert!(deprecated_extension("rpi-subagents").is_none());
    }

    /// ADR-0038: an installed copy of the retired plugin no longer produces a
    /// startup removal notice, whether the user keeps it enabled, disables it,
    /// or simply leaves it unloaded. The scan is install-directory based, so
    /// the three local states differ only in loader state, which this path
    /// never consults.
    #[test]
    fn retired_mcp_adapter_produces_no_startup_guidance() {
        let dir = TestDir::new();
        let root = dir.path().join("extensions");
        write_manifest(&root.join("rpi-mcp-adapter"), "rpi-mcp-adapter");
        write_manifest(&root.join("renamed-mcp"), "rpi-mcp-adapter");
        let found = installed_deprecated_extensions(dir.path(), dir.path(), false);
        assert!(found.is_empty(), "no startup notice: {found:?}");
    }

    #[test]
    fn generic_scan_finds_manifest_name_and_dir_name_and_skips_staging_dirs() {
        let dir = TestDir::new();
        let root = dir.path().join("extensions");
        // A renamed directory still matches through the manifest name.
        write_manifest(&root.join("renamed"), "test-replaced");
        // Without a readable manifest the directory name is the identity.
        std::fs::create_dir_all(root.join("test-replaced")).unwrap();
        write_manifest(&root.join("rpi-todo"), "rpi-todo");
        // Staging directories are never surfaced.
        write_manifest(&root.join("test-replaced.tmp-1"), "test-replaced");
        let found = installed_deprecated_extensions_in(TEST_ENTRIES, dir.path(), dir.path(), false);
        let mut names: Vec<String> = found
            .iter()
            .map(|item| item.dir.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["renamed", "test-replaced"]);
        assert!(
            found
                .iter()
                .all(|item| item.extension.name == "test-replaced")
        );
        assert!(found.iter().all(|item| !item.project_scope));
    }

    #[test]
    fn generic_scan_gates_the_project_root_on_trust() {
        let dir = TestDir::new();
        let cwd = dir.path().join("project");
        write_manifest(
            &crate::config::get_project_config_dir(&cwd).join("extensions/test-replaced"),
            "test-replaced",
        );
        let untrusted = installed_deprecated_extensions_in(
            TEST_ENTRIES,
            &dir.path().join("agent"),
            &cwd,
            false,
        );
        assert!(untrusted.is_empty());
        let trusted =
            installed_deprecated_extensions_in(TEST_ENTRIES, &dir.path().join("agent"), &cwd, true);
        assert_eq!(trusted.len(), 1);
        assert!(trusted[0].project_scope);
        assert_eq!(trusted[0].removal_command(), "rpi remove test-replaced -l");
    }

    #[test]
    fn removal_source_prefers_the_github_install_marker() {
        let dir = TestDir::new();
        let install = dir.path().join("extensions/test-replaced");
        write_manifest(&install, "test-replaced");
        std::fs::write(
            install.join(extension_registry::GITHUB_INSTALL_MARKER_FILE),
            "github:revpidev/rpi\n",
        )
        .unwrap();
        let found = installed_deprecated_extensions_in(TEST_ENTRIES, dir.path(), dir.path(), false);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].removal_source(), "github:revpidev/rpi");
        assert_eq!(found[0].removal_command(), "rpi remove github:revpidev/rpi");
        assert_eq!(
            found[0].startup_message(),
            "The \"test-replaced\" extension is deprecated and will no longer be updated: Test \
             capability is now built into rpi. Uninstall it with \"rpi remove \
             github:revpidev/rpi\" to use the built-in extension."
        );
    }

    #[test]
    fn update_skip_message_names_source_and_removal_command() {
        let extension = &DeprecatedExtension {
            name: "test-replaced",
            replacement: "Test capability",
        };
        assert_eq!(
            update_skip_message(extension, "test-replaced", "test-replaced", false),
            "Skipping test-replaced: the \"test-replaced\" extension is deprecated and will \
             no longer be updated (Test capability is now built into rpi). Uninstall it with \"rpi \
             remove test-replaced\"."
        );
        assert_eq!(
            update_skip_message(extension, "test-replaced@^0.1", "test-replaced", true),
            "Skipping test-replaced@^0.1: the \"test-replaced\" extension is deprecated and \
             will no longer be updated (Test capability is now built into rpi). Uninstall it with \
             \"rpi remove test-replaced -l\"."
        );
    }
}
