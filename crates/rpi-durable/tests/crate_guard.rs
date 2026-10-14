//! FR-A guards: workspace placement, edition/MSRV inheritance, the experimental
//! doc wording, and the one-way dependency red line (`crates/rpi` must not
//! depend on `rpi-durable`).

use serde_json::Value;

fn cargo_metadata() -> Value {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    let output = std::process::Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--manifest-path",
        ])
        .arg(manifest)
        .output()
        .expect("cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("metadata json")
}

#[test]
fn crate_is_a_workspace_member_with_edition_2024_and_msrv_185() {
    let metadata = cargo_metadata();
    let packages = metadata["packages"].as_array().expect("packages");
    let durable = packages
        .iter()
        .find(|package| package["name"] == "rpi-durable")
        .expect("rpi-durable package");
    assert_eq!(durable["edition"], "2024");
    assert_eq!(durable["rust_version"], "1.85");
}

#[test]
fn default_host_crate_does_not_depend_on_rpi_durable() {
    let metadata = cargo_metadata();
    let packages = metadata["packages"].as_array().expect("packages");
    let rpi = packages
        .iter()
        .find(|package| package["name"] == "rpi")
        .expect("rpi package");
    let dependencies = rpi["dependencies"].as_array().expect("dependencies");
    let forbidden = dependencies
        .iter()
        .find(|dependency| dependency["name"] == "rpi-durable");
    assert!(
        forbidden.is_none(),
        "crates/rpi must not depend on rpi-durable"
    );
}

#[test]
fn crate_docs_carry_the_upstream_experimental_wording() {
    let source = include_str!("../src/lib.rs");
    assert!(
        source.contains("The API changes without notice between releases."),
        "crate rustdoc must carry the upstream README experimental sentence"
    );
}
