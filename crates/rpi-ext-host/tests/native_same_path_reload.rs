//! Same-path native plugin reload regression (the `/resume` status-line
//! loss bug). Session replacement (`/resume` `/new` `/fork` `/clone`
//! `/import`) builds a FRESH `NativeExtensionHost` which re-loads the same
//! cdylib path; dlopen memoizes per path, so the plugin root module is
//! initialized a second time on the SAME plugin statics. Before the rebind
//! fix the previous carrier's `install` failed on the second host (its own
//! "already initialized" guard) and the extension (tools, flags, event
//! handlers) silently vanished after `/resume`. This test pins the
//! host-visible half: a second host loading the same path must get a
//! successful init with its own registrations.
//!
//! Carriers (TE49): the previous carrier was removed with its crate
//! (ADR-0037). The replacement is the first-party
//! `rpi-ext-todo` cdylib: it registers a `session_start` event handler, the
//! `todo` tool and the `todos` command — the same three registration
//! surfaces the original asserted (the previous carrier's `mcp-config` flag
//! assertion is reshaped to the command surface; no other first-party
//! plugin registers a flag, see TE49 §7.3). Unlike the subagents carrier,
//! the todo install has no filesystem side effects, so this test stays
//! isolated when it runs in parallel with `multi_native_plugins_load`.
//!
//! Packaging mirrors multi_native_plugins_load.rs. The cdylib is built by
//! `cargo build` (G1) before the test gate (G2); a missing artifact is a
//! hard failure, never a silent skip (TE49 FR-A: no skip path).

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

/// One packaged plugin: temp dir + crate manifest + the cdylib from
/// target/debug. Hard-fails when the cdylib is missing (the pre-TE49
/// silent skip hid the test).
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
        "rpi-samepath-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp package dir");
    std::fs::copy(&so, dir.join(so.file_name().unwrap())).expect("copy cdylib");
    std::fs::copy(&manifest, dir.join("rpi-extension.json")).expect("copy manifest");
    dir
}

/// Two hosts, one plugin path: the second load (session replacement) must
/// succeed and re-register handlers + tools + commands on the new host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_host_reloading_same_path_reinstalls_the_plugin() {
    let plugin_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let dir = package("x", "todo", &plugin_root);

    // Host 1: like process start.
    let host1 = NativeExtensionHost::new("/tmp");
    let errors1 = host1.load_paths(std::slice::from_ref(&dir)).await;
    assert!(errors1.is_empty(), "{errors1:?}");
    assert!(
        host1.has_handlers("session_start"),
        "host1 must get the todo event handlers"
    );
    assert!(
        host1.get_tool_definition("todo").is_some(),
        "host1 must get the todo tool"
    );

    // Host 2: like /resume — fresh host, same plugin path. Before the
    // rebind fix this failed on the old carrier's second init.
    let host2 = NativeExtensionHost::new("/tmp");
    let errors2 = host2.load_paths(std::slice::from_ref(&dir)).await;
    assert!(errors2.is_empty(), "second-host load failed: {errors2:?}");
    assert!(
        host2.has_handlers("session_start"),
        "host2 must get the todo event handlers re-registered"
    );
    assert!(
        host2.get_tool_definition("todo").is_some(),
        "host2 must get the todo tool re-registered"
    );
    assert!(
        host2.get_command("todos").is_some(),
        "host2 must get the todos command re-registered"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
