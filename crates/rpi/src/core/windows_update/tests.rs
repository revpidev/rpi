//! Module-level tests of the protocol constants and helpers.

use super::*;

#[test]
fn the_first_boot_protocol_starts_at_the_first_release_that_ships_it() {
    assert_eq!(FIRST_BOOT_PROTOCOL_VERSION, "0.1.9-rc.4");
    // The published rc.3 predates the protocol; rc.4 and later have it.
    assert!(!supports_first_boot_protocol("0.1.9-rc.3"));
    assert!(!supports_first_boot_protocol("0.1.9-rc.2"));
    assert!(!supports_first_boot_protocol("0.1.8"));
    assert!(supports_first_boot_protocol("0.1.9-rc.4"));
    assert!(supports_first_boot_protocol("0.1.9"));
    assert!(supports_first_boot_protocol("v0.1.9"));
    assert!(supports_first_boot_protocol("0.2.0"));
    // An unparseable target is not trusted with the protocol.
    assert!(!supports_first_boot_protocol("not-a-version"));
    assert!(!supports_first_boot_protocol(""));
    assert!(!supports_first_boot_protocol_since("0.1.9-rc.4", "garbage"));
    // Build metadata orders after a bare version in semver; the floor
    // constant never carries any.
    assert!(supports_first_boot_protocol_since("1.0.0+build", "1.0.0"));
}

#[test]
fn subagent_and_preflight_children_do_not_run_the_transaction() {
    assert!(startup_transaction_enabled(
        &transaction::StartupEnv::plain()
    ));
    assert!(!startup_transaction_enabled(&transaction::StartupEnv {
        subagent_child: true,
        ..transaction::StartupEnv::default()
    }));
    assert!(!startup_transaction_enabled(&transaction::StartupEnv {
        verify_child: true,
        ..transaction::StartupEnv::default()
    }));
    // The trampoline flag alone does not disable anything.
    assert!(startup_transaction_enabled(&transaction::StartupEnv {
        trampoline_pid: Some(7),
        ..transaction::StartupEnv::default()
    }));
    assert_eq!(VERIFY_EXTENSION_FLAG, "--verify-extension");
    assert_eq!(ENV_TRAMPOLINE_PID, "RPI_UPDATE_TRAMPOLINE_PID");
    assert_eq!(ENV_VERIFY_CHILD, "RPI_UPDATE_VERIFY_CHILD");
}

#[test]
fn container_state_distinguishes_retired_flat_and_generational() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "rpi-wu-root-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(dir.join("flat")).unwrap();
    std::fs::create_dir_all(dir.join("gen/0.2.0")).unwrap();
    std::fs::create_dir_all(dir.join("retired")).unwrap();
    std::fs::write(dir.join("gen/0.2.0/rpi-extension.json"), "{}").unwrap();
    std::fs::write(
        dir.join("gen/current.json"),
        r#"{"schemaVersion":1,"version":"0.2.0","path":"0.2.0"}"#,
    )
    .unwrap();
    std::fs::write(dir.join("retired/.retired"), "").unwrap();
    assert_eq!(
        container_state(&dir.join("flat")),
        Some(ContainerState::Installed {
            path: dir.join("flat")
        })
    );
    assert_eq!(
        container_state(&dir.join("gen")),
        Some(ContainerState::Installed {
            path: dir.join("gen/0.2.0")
        })
    );
    assert_eq!(
        container_state(&dir.join("retired")),
        Some(ContainerState::Retired)
    );
    assert_eq!(
        container_state(&dir.join("missing")),
        Some(ContainerState::Installed {
            path: dir.join("missing")
        }),
        "a missing container is simply not an install; callers check the manifest"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn budgets_are_the_designed_ones() {
    assert_eq!(LOCK_WAIT, std::time::Duration::from_secs(10));
    assert_eq!(RENAME_RETRIES, 5);
    assert_eq!(VERIFY_EXTENSION_TIMEOUT, std::time::Duration::from_secs(30));
    assert_eq!(fs::rename_backoff(1), std::time::Duration::from_millis(50));
    assert_eq!(fs::rename_backoff(4), std::time::Duration::from_millis(200));
    assert_eq!(fs::rename_backoff(5), std::time::Duration::from_millis(200));
    assert_eq!(fs::rename_backoff(9), std::time::Duration::from_millis(200));
    assert_eq!(fs::rename_backoff(0), std::time::Duration::from_millis(50));
}

#[test]
fn the_known_limits_are_documented_in_the_readme_and_the_release_notes() {
    // Design §11.7 "known limits" and the task's FR-W4 documentation
    // assertion: the user-facing documents must say, plainly, that a
    // shutdown before the first-start confirmation counts as a crash, that
    // a crash after the confirmation is not rolled back, and where the
    // log and the cleanup command are.
    // The crate lives in `crates/rpi`; the documents are at the workspace root.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let readme = std::fs::read_to_string(root.join("README.md")).unwrap();
    for phrase in [
        "Windows: an update takes effect at the next start",
        "It takes effect the next time rpi starts.",
        "shutting Windows down before the new version confirmed its first start counts as that crash",
        "a crash after the confirmation is not rolled back",
        "rpi update --cleanup",
        "logs\\update.log",
    ] {
        assert!(readme.contains(phrase), "README.md lacks {phrase:?}");
    }
    let readme_zh = std::fs::read_to_string(root.join("README.zh-CN.md")).unwrap();
    for phrase in [
        "Windows：更新在下次启动时生效",
        "新版本确认首启之前关机视同首启崩溃",
        "确认之后的崩溃不自动回滚",
        "rpi update --cleanup",
    ] {
        assert!(
            readme_zh.contains(phrase),
            "README.zh-CN.md lacks {phrase:?}"
        );
    }
    let notes = std::fs::read_to_string(
        root.join("changes")
            .join(format!("v{}.md", crate::config::VERSION)),
    )
    .unwrap_or_else(|_| {
        panic!(
            "changes/v{}.md must exist for the workspace version",
            crate::config::VERSION
        )
    });
    for phrase in [
        "Known limits",
        "shutting Windows down before the new version confirmed its first start",
        "a crash after the confirmation is not rolled back",
        "rpi update --cleanup",
    ] {
        assert!(
            notes.contains(phrase),
            "changes/v{}.md lacks {phrase:?}",
            crate::config::VERSION
        );
    }
    // The update command's help documents the retry command and never the
    // hidden preflight flag.
    let help = crate::cli::package_command::update_help();
    assert!(help.contains("--cleanup"));
    assert!(!help.contains(crate::core::windows_update::VERIFY_EXTENSION_FLAG));
}
