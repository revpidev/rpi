//! Multi-native-plugin load regression: loading SEVERAL native plugins in
//! one process must give each extension its own module table. The original
//! bug: `RpiNativeModule_Ref::load_from_file` memoizes per TYPE
//! (`RootModule::load_from` → `root_module_statics().root_mod.try_init`),
//! so the second plugin path silently re-ran the FIRST plugin's
//! `rpi_extension_init` — every extension registered the first .so's tools
//! (found installing all three rpi plugins together). The per-path loader
//! in `native::load_native_plugin` fixed it; this test pins it.
//!
//! Packaging follows the `l0_load.rs` pattern used by the plugin crates:
//! copies each cdylib from target/debug into a throwaway package dir with its crate manifest. The cdylibs are built
//! by `cargo build` (G1) before the test gate (G2); a missing artifact is
//! a hard failure, never a silent skip (TE49 FR-A: no skip path).
//!
//! Carriers (TE49): rpi-ext-subagents / rpi-ext-todo / rpi-ext-smart-fetch
//! — three first-party cdylibs with disjoint tool sets. The previous third
//! carrier was removed with its crate (ADR-0037).

use std::path::{Path, PathBuf};

use rpi_ext_host::host::NativeExtensionHost;

fn target_dir() -> PathBuf {
    std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target")
        })
}

/// One packaged plugin: temp dir + crate manifest + the release of
/// `librpi_ext_<name>.so` from target/debug. Hard-fails when the cdylib is
/// missing (run `cargo build -p rpi-ext-<name>` / `cargo test --workspace`
/// after a workspace build first) — the pre-TE49 silent skip hid the test.
fn package(tag: &str, spec: &str, plugin_root: &Path) -> PathBuf {
    let so = target_dir()
        .join("debug")
        .join(format!("librpi_ext_{}.so", spec.replace('-', "_")));
    assert!(
        so.is_file(),
        "cdylib missing at {} — build it first: cargo build -p rpi-ext-{spec}",
        so.display()
    );
    let manifest = plugin_root.join(format!("crates/rpi-ext-{spec}/rpi-extension.json"));
    assert!(
        manifest.is_file(),
        "plugin manifest missing at {}",
        manifest.display()
    );
    let dir = std::env::temp_dir().join(format!(
        "rpi-multi-native-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0),
    ));
    std::fs::create_dir_all(&dir).expect("temp package dir");
    std::fs::copy(&so, dir.join(so.file_name().unwrap())).expect("copy cdylib");
    std::fs::copy(&manifest, dir.join("rpi-extension.json")).expect("copy manifest");
    dir
}

fn cleanup(dirs: &[PathBuf]) {
    for dir in dirs {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_native_plugin_gets_its_own_module_table() {
    // The subagents cdylib splits on `RPI_SUBAGENT_CHILD` at load time: a
    // gate run from inside a subagent session (ambient RPI_SUBAGENT_CHILD=1)
    // would load it in child mode and register only `contact_supervisor`.
    // Scrub the inherited family before loading (single-test binary, so the
    // process-wide env is contained).
    for name in [
        "RPI_SUBAGENT_CHILD",
        "RPI_SUBAGENT_FANOUT_CHILD",
        "RPI_SUBAGENT_DEPTH",
        "RPI_SUBAGENT_MAX_DEPTH",
        "RPI_SUBAGENT_CHILD_AGENT",
        "RPI_SUBAGENT_CHILD_INDEX",
        "RPI_SUBAGENT_WAIT_TOOL_ENABLED",
    ] {
        rpi_test_env::remove_var(name);
    }
    let plugin_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let subagents = package("a", "subagents", &plugin_root);
    let todo = package("b", "todo", &plugin_root);
    let smart_fetch = package("c", "smart-fetch", &plugin_root);
    let paths = [subagents.clone(), todo.clone(), smart_fetch.clone()];

    // subagents FIRST on purpose: the memoization bug returned the first
    // loaded module for every path, so with this order a regression makes
    // all three extensions register the subagents tool set.
    let host = NativeExtensionHost::new(std::env::temp_dir().to_string_lossy().as_ref());
    let errors = host.load_paths(&paths).await;
    assert!(errors.is_empty(), "load errors: {errors:?}");

    let exts = host.core().extensions().to_vec();
    assert_eq!(exts.len(), 3, "all three loaded: {}", exts.len());
    let tools_of = |so_suffix: &str| -> Vec<String> {
        exts.iter()
            .find(|ext| ext.path.ends_with(so_suffix))
            .unwrap_or_else(|| panic!("extension for {so_suffix} missing"))
            .tools()
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    };
    assert_eq!(
        tools_of("librpi_ext_subagents.so"),
        vec!["subagent", "subagent_supervisor", "subagent_wait"],
        "subagents tools belong to the subagents extension"
    );
    assert_eq!(
        tools_of("librpi_ext_todo.so"),
        vec!["todo"],
        "todo tool belongs to the todo extension"
    );
    assert_eq!(
        tools_of("librpi_ext_smart_fetch.so"),
        vec!["web_fetch", "batch_web_fetch"],
        "smart-fetch registers its two tools"
    );
    // The commands face too (subagents and todo are command registrars).
    let commands_of = |so_suffix: &str| -> Vec<String> {
        exts.iter()
            .find(|ext| ext.path.ends_with(so_suffix))
            .unwrap_or_else(|| panic!("extension for {so_suffix} missing"))
            .commands()
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    };
    let subagents_commands = commands_of("librpi_ext_subagents.so");
    assert!(
        subagents_commands.iter().any(|c| c == "subagents"),
        "subagents commands registered on the right extension: {subagents_commands:?}"
    );
    let todo_commands = commands_of("librpi_ext_todo.so");
    assert_eq!(
        todo_commands,
        vec!["todos"],
        "todo commands belong to the todo extension"
    );

    cleanup(&paths);
}
