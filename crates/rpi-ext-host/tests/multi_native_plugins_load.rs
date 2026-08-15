//! Multi-native-plugin load regression: loading SEVERAL native plugins in
//! one process must give each extension its own module table. The original
//! bug: `RpiNativeModule_Ref::load_from_file` memoizes per TYPE
//! (`RootModule::load_from` → `root_module_statics().root_mod.try_init`),
//! so the second plugin path silently re-ran the FIRST plugin's
//! `rpi_extension_init` — every extension registered the first .so's tools
//! (found installing all three rpi plugins together). The per-path loader
//! in `native::load_native_plugin` fixed it; this test pins it.
//!
//! Packaging mirrors l0_load.rs: copies each cdylib from target/debug into
//! a throwaway package dir with its crate manifest. Skips when the cdylibs
//! are missing (build first: `cargo build -p rpi-ext-smart-fetch -p
//! rpi-ext-subagents -p rpi-ext-mcp-adapter`).

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
/// `librpi_ext_<name>.so` from target/debug.
fn package(tag: &str, spec: &str, plugin_root: &Path) -> Option<PathBuf> {
    let so = target_dir()
        .join("debug")
        .join(format!("librpi_ext_{}.so", spec.replace('-', "_")));
    if !so.is_file() {
        eprintln!(
            "skipping: cdylib missing at {} — build the plugin crates first",
            so.display()
        );
        return None;
    }
    let manifest = plugin_root.join(format!("crates/rpi-ext-{spec}/rpi-extension.json"));
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
    Some(dir)
}

fn cleanup(dirs: &[PathBuf]) {
    for dir in dirs {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_native_plugin_gets_its_own_module_table() {
    let plugin_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let Some(subagents) = package("a", "subagents", &plugin_root) else {
        return;
    };
    let Some(mcp) = package("b", "mcp-adapter", &plugin_root) else {
        cleanup(&[subagents]);
        return;
    };
    let Some(smart_fetch) = package("c", "smart-fetch", &plugin_root) else {
        cleanup(&[subagents, mcp]);
        return;
    };
    let paths = [subagents.clone(), mcp.clone(), smart_fetch.clone()];

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
        tools_of("librpi_ext_mcp_adapter.so"),
        vec!["mcp"],
        "mcp-adapter registers only the mcp proxy tool"
    );
    assert_eq!(
        tools_of("librpi_ext_smart_fetch.so"),
        vec!["web_fetch", "batch_web_fetch"],
        "smart-fetch registers its two tools"
    );
    // The commands face too (subagents is the only command registrar).
    let subagents_commands: Vec<String> = exts
        .iter()
        .find(|ext| ext.path.ends_with("librpi_ext_subagents.so"))
        .unwrap()
        .commands()
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    assert!(
        subagents_commands.iter().any(|c| c == "subagents"),
        "subagents commands registered on the right extension: {subagents_commands:?}"
    );

    cleanup(&paths);
}
