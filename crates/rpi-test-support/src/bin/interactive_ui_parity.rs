//! V14-22 C2 dual-carrier parity driver (R-U7.4 / G11 item 2).
//!
//! Drives the native fixture (`crates/rpi-test-native-plugin`) and the wasm
//! fixture (`examples/wasm-extension`) with the same JSONL corpus through the
//! real host-call channel and a deterministic [`ScriptedUiBridge`]. Each
//! scenario is judged here and written as `{scenario, carrier, mountOptions,
//! frames, terminal, scriptExhausted, toolResult}` per carrier plus a
//! `*.diff.json` verdict; `run.py` builds the fixtures, invokes this driver,
//! aggregates the verdict files and writes the report.
//!
//! Usage:
//! ```text
//! cargo run -p rpi-test-support --bin interactive-ui-parity -- \
//!     --fixture scripts/interactive-ui-parity/corpus \
//!     [--out fixtures/generated/interactive-ui-parity] \
//!     [--native target/debug/librpi_test_native_plugin.so] \
//!     [--wasm examples/wasm-extension/target/wasm32-unknown-unknown/release/rpi_wasm_extension_example.wasm] \
//!     [--fuzz 24 --seed 20260910]
//! ```
//!
//! Exit code 0 = every scenario matched; non-zero = any difference, load
//! failure or missing fixture.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rpi_ext_host::api::UiBridge;
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::interactive_ui::{ComponentEvent, DisposeReason};
use rpi_ext_host::types::{ExtensionMode, ToolExecuteRequest};
use rpi_test_support::ui_host::ScriptedUiBridge;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Carrier {
    Native,
    Wasm,
}

impl Carrier {
    fn slug(self) -> &'static str {
        match self {
            Carrier::Native => "native",
            Carrier::Wasm => "wasm",
        }
    }
}

#[derive(Clone, Debug)]
struct Scenario {
    name: String,
    events: Vec<ComponentEvent>,
}

#[derive(Default)]
struct Args {
    corpus: Option<PathBuf>,
    out: Option<PathBuf>,
    native: Option<PathBuf>,
    wasm: Option<PathBuf>,
    fuzz: Option<usize>,
    seed: u64,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        seed: 20_260_910,
        ..Args::default()
    };
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        let mut value = || argv.next().ok_or_else(|| format!("{arg} requires a value"));
        match arg.as_str() {
            "--fixture" => args.corpus = Some(PathBuf::from(value()?)),
            "--out" => args.out = Some(PathBuf::from(value()?)),
            "--native" => args.native = Some(PathBuf::from(value()?)),
            "--wasm" => args.wasm = Some(PathBuf::from(value()?)),
            "--fuzz" => {
                args.fuzz = Some(
                    value()?
                        .parse()
                        .map_err(|error| format!("--fuzz: {error}"))?,
                )
            }
            "--seed" => {
                args.seed = value()?
                    .parse()
                    .map_err(|error| format!("--seed: {error}"))?
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(args)
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn target_dir() -> PathBuf {
    std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root().join("target"))
}

fn native_plugin_file_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "librpi_test_native_plugin.dylib"
    } else if cfg!(target_os = "windows") {
        "rpi_test_native_plugin.dll"
    } else {
        "librpi_test_native_plugin.so"
    }
}

fn default_native() -> PathBuf {
    target_dir().join("debug").join(native_plugin_file_name())
}

fn default_wasm() -> PathBuf {
    let built = repo_root().join(
        "examples/wasm-extension/target/wasm32-unknown-unknown/release/rpi_wasm_extension_example.wasm",
    );
    if built.is_file() {
        return built;
    }
    repo_root().join("fixtures/generated/interactive-ui-parity/rpi_wasm_extension_example.wasm")
}

/// One corpus line → one host event (V14-22 §3.5 script shape).
fn event_from_json(value: &Value) -> Result<ComponentEvent, String> {
    if let Some(input) = value.get("input").and_then(Value::as_str) {
        return Ok(ComponentEvent::Input {
            data: input.to_owned(),
        });
    }
    if let Some(resize) = value.get("resize").and_then(Value::as_array) {
        if resize.len() != 2 {
            return Err(format!("resize needs [width,height]: {value}"));
        }
        return Ok(ComponentEvent::Resize {
            width: resize[0].as_u64().unwrap_or(80) as usize,
            height: resize[1].as_u64().unwrap_or(24) as usize,
        });
    }
    if value.get("tick").is_some() {
        return Ok(ComponentEvent::Tick);
    }
    if let Some(hidden) = value.get("hidden").and_then(Value::as_bool) {
        return Ok(ComponentEvent::Visibility { hidden });
    }
    if value.get("wake").is_some() {
        return Ok(ComponentEvent::Render);
    }
    if let Some(theme) = value.get("theme") {
        return Ok(ComponentEvent::Theme {
            theme: theme.clone(),
        });
    }
    if let Some(true) = value.get("focus").and_then(Value::as_bool) {
        return Ok(ComponentEvent::Focus);
    }
    if let Some(true) = value.get("blur").and_then(Value::as_bool) {
        return Ok(ComponentEvent::Blur);
    }
    if let Some(reason) = value.get("dispose").and_then(Value::as_str) {
        return Ok(ComponentEvent::Dispose {
            reason: DisposeReason::parse(reason).map_err(|error| error.message)?,
        });
    }
    Err(format!("unrecognized corpus event: {value}"))
}

fn load_corpus(dir: &Path) -> Result<Vec<Scenario>, String> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|error| format!("read corpus {}: {error}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(format!("no *.jsonl corpus files under {}", dir.display()));
    }
    let mut scenarios = Vec::new();
    for file in files {
        let name = file
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("scenario")
            .to_owned();
        let content = std::fs::read_to_string(&file)
            .map_err(|error| format!("read {}: {error}", file.display()))?;
        let mut events = Vec::new();
        for (index, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let value: Value = serde_json::from_str(line)
                .map_err(|error| format!("{}:{}: {error}", file.display(), index + 1))?;
            events.push(event_from_json(&value)?);
        }
        scenarios.push(Scenario { name, events });
    }
    Ok(scenarios)
}

/// Scratch dir with best-effort cleanup.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rpi-ui-parity-{tag}-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Copy the fixture into a manifest package the loader accepts (capabilities
/// `tools` + `ui`).
fn prepare_package(carrier: Carrier, fixture: &Path, dir: &Path) -> Result<PathBuf, String> {
    if !fixture.is_file() {
        return Err(format!(
            "{} fixture missing: {} (build it first)",
            carrier.slug(),
            fixture.display()
        ));
    }
    let file_name = fixture
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("bad fixture name: {}", fixture.display()))?;
    let dist = dir.join("dist");
    std::fs::create_dir_all(&dist).map_err(|error| error.to_string())?;
    std::fs::copy(fixture, dist.join(file_name)).map_err(|error| {
        format!(
            "copy {} -> {}: {error}",
            fixture.display(),
            dist.join(file_name).display()
        )
    })?;
    let carrier_field = match carrier {
        Carrier::Native => "native",
        Carrier::Wasm => "wasm",
    };
    let manifest = json!({
        "name": format!("ui-parity-{}", carrier.slug()),
        "version": "0.1.0",
        "description": "V14-22 dual-carrier parity fixture",
        carrier_field: format!("dist/{file_name}"),
        "capabilities": ["tools", "ui"],
        "rpiAbi": 1,
    });
    std::fs::write(
        dir.join("rpi-extension.json"),
        serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    Ok(dir.to_path_buf())
}

struct CarrierOutput {
    /// `{lines,cursor?,done?}` frames, in order.
    frames: Vec<Value>,
    terminal: Option<Value>,
    mount_options: Option<Value>,
    script_exhausted: bool,
    /// The `toolExecute` outcome (final tool result or error string).
    tool_result: Value,
}

impl CarrierOutput {
    fn to_json(&self, scenario: &str, carrier: Carrier) -> Value {
        json!({
            "scenario": scenario,
            "carrier": carrier.slug(),
            "mountOptions": self.mount_options,
            "frames": self.frames,
            "terminal": self.terminal,
            "scriptExhausted": self.script_exhausted,
            "toolResult": self.tool_result,
        })
    }

    /// The R-U7.4 comparison surface: frame sequence + terminal result
    /// (+ script completeness). `mountOptions` is evidence, not behaviour —
    /// the carrier frame-budget clamp is a documented execution constraint
    /// (design §4.4) checked separately by [`documented_constraint_only`].
    fn parity_projection(&self) -> Value {
        json!({
            "frames": self.frames,
            "terminal": self.terminal,
            "scriptExhausted": self.script_exhausted,
        })
    }
}

/// Whether two full carrier records differ only by the documented wasm frame
/// budget clamp (R-U7.2 / design §4.4: native keeps the guest's budget, wasm
/// caps it at 512 KiB). Any other difference is a parity failure.
///
/// The wasm value must be the **exact** documented clamp
/// (`min(native, 512 KiB)`): a regression that quietly lowers the wasm budget
/// further is not an allowed constraint difference.
fn documented_constraint_only(native: &Value, wasm: &Value) -> bool {
    if native == wasm {
        return true;
    }
    let native_bytes = native
        .pointer("/mountOptions/maxFrameBytes")
        .and_then(Value::as_u64);
    let wasm_bytes = wasm
        .pointer("/mountOptions/maxFrameBytes")
        .and_then(Value::as_u64);
    let (Some(native_bytes), Some(wasm_bytes)) = (native_bytes, wasm_bytes) else {
        return false;
    };
    let expected =
        native_bytes.min(rpi_ext_host::interactive_ui::WASM_DEFAULT_MAX_FRAME_BYTES as u64);
    if wasm_bytes != expected {
        return false;
    }
    let mut native = native.clone();
    let mut wasm = wasm.clone();
    *native
        .pointer_mut("/mountOptions/maxFrameBytes")
        .expect("checked above") = json!(0);
    *wasm
        .pointer_mut("/mountOptions/maxFrameBytes")
        .expect("checked above") = json!(0);
    // The carrier name itself is provenance, not behaviour.
    native["carrier"] = json!("carrier");
    wasm["carrier"] = json!("carrier");
    native == wasm
}

async fn run_carrier(
    carrier: Carrier,
    fixture: &Path,
    scenario: &Scenario,
    work: &Path,
) -> Result<CarrierOutput, String> {
    let package = prepare_package(carrier, fixture, work)?;
    let bridge = Arc::new(ScriptedUiBridge::new(scenario.events.clone()));
    let cwd = work.join("cwd");
    std::fs::create_dir_all(&cwd).map_err(|error| error.to_string())?;
    let host = NativeExtensionHost::new(&cwd.to_string_lossy());
    host.runtime().set_ui_bridge(
        Some(Arc::clone(&bridge) as Arc<dyn UiBridge>),
        ExtensionMode::Tui,
    );
    let errors = host.load_paths(&[package]).await;
    if !errors.is_empty() {
        return Err(format!("{}: load failed: {errors:?}", carrier.slug()));
    }
    let definition = host
        .get_tool_definition("interactive_ui_fixture")
        .ok_or_else(|| {
            format!(
                "{}: fixture tool `interactive_ui_fixture` was not registered",
                carrier.slug()
            )
        })?;
    let request = ToolExecuteRequest {
        tool_call_id: format!("parity-{}-{}", carrier.slug(), scenario.name),
        params: json!({}),
        signal: CancellationToken::new(),
        on_update: None,
    };
    let outcome = match (definition.execute)(request, host.core().create_context()).await {
        Ok(result) => serde_json::to_value(&result)
            .map_err(|error| format!("{}: tool result json: {error}", carrier.slug()))?,
        Err(error) => json!({ "error": error.to_string() }),
    };
    let transcript = bridge.transcript();
    Ok(CarrierOutput {
        frames: transcript
            .frames
            .iter()
            .map(|frame| frame.to_json())
            .collect(),
        terminal: transcript.terminal,
        mount_options: transcript.mount_options,
        script_exhausted: transcript.script_exhausted,
        tool_result: outcome,
    })
}

/// Deterministic xorshift PRNG (no dependency; fixed seed = replayable).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

/// §4.5 fuzz: a random-but-valid event script, seed-fixed and replayable.
fn fuzz_scenario(seed: u64, index: usize) -> Scenario {
    let mut rng = Rng(seed ^ ((index as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)));
    let mut events = vec![ComponentEvent::Resize {
        width: 60 + rng.below(60),
        height: 20 + rng.below(20),
    }];
    let steps = rng.below(12);
    for _ in 0..steps {
        events.push(match rng.below(9) {
            0 => ComponentEvent::Input {
                data: ["a", "b", "c", "h", "g", "↑", "\u{1b}[B"][rng.below(7)].to_owned(),
            },
            1 => ComponentEvent::Tick,
            2 => ComponentEvent::Focus,
            3 => ComponentEvent::Blur,
            4 => ComponentEvent::Theme {
                theme: json!({"name": if rng.below(2) == 0 { "dark" } else { "light" }}),
            },
            5 => ComponentEvent::Visibility {
                hidden: rng.below(2) == 0,
            },
            6 => ComponentEvent::Render,
            7 => ComponentEvent::Input {
                data: format!("burst{}", rng.below(100)),
            },
            _ => ComponentEvent::Input {
                data: "c".to_owned(),
            },
        });
    }
    events.push(ComponentEvent::Input {
        data: "q".to_owned(),
    });
    Scenario {
        name: format!("fuzz-{index:03}"),
        events,
    }
}

async fn run_once(
    args: &Args,
    scenario: &Scenario,
    native: &Path,
    wasm: &Path,
    scratch: &Path,
    write_out: bool,
) -> Result<bool, String> {
    let native_work = scratch.join("native");
    let wasm_work = scratch.join("wasm");
    let native_out = run_carrier(Carrier::Native, native, scenario, &native_work).await?;
    let wasm_out = run_carrier(Carrier::Wasm, wasm, scenario, &wasm_work).await?;

    let native_json = native_out.to_json(&scenario.name, Carrier::Native);
    let wasm_json = wasm_out.to_json(&scenario.name, Carrier::Wasm);
    let projections_match = native_out.parity_projection() == wasm_out.parity_projection();
    let constraint_only = projections_match && documented_constraint_only(&native_json, &wasm_json);
    let matched = projections_match && constraint_only;
    let documented = !native_json.eq(&wasm_json) && matched;
    // The `documented` flag must only fire for the byte-budget clamp; make
    // the evidence self-checking (the two records are otherwise equal).
    debug_assert!(
        !documented
            || (native_json["mountOptions"]["maxFrameBytes"]
                != wasm_json["mountOptions"]["maxFrameBytes"])
    );
    if write_out {
        let out_dir = args
            .out
            .clone()
            .unwrap_or_else(|| repo_root().join("fixtures/generated/interactive-ui-parity"));
        std::fs::create_dir_all(&out_dir).map_err(|error| error.to_string())?;
        for (carrier, value) in [(Carrier::Native, &native_json), (Carrier::Wasm, &wasm_json)] {
            let path = out_dir.join(format!("{}.{}.json", scenario.name, carrier.slug()));
            std::fs::write(
                &path,
                serde_json::to_string_pretty(value).map_err(|error| error.to_string())?,
            )
            .map_err(|error| format!("write {}: {error}", path.display()))?;
        }
        let result = json!({
            "scenario": scenario.name,
            "matched": matched,
            "documentedConstraint": documented.then_some(
                "mountOptions.maxFrameBytes: wasm carrier caps the frame budget at 512 KiB (design §4.4)"
            ),
            "native": native_out.parity_projection(),
            "wasm": wasm_out.parity_projection(),
        });
        let path = out_dir.join(format!("{}.diff.json", scenario.name));
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&result).map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    }
    let status = if matched { "MATCH" } else { "DIFF" };
    println!(
        "{status} {} (native frames={} wasm frames={}{})",
        scenario.name,
        native_out.frames.len(),
        wasm_out.frames.len(),
        if documented {
            "; documented constraint: wasm frame budget cap"
        } else {
            ""
        }
    );
    if !matched {
        println!("--- native ---\n{native_json}\n--- wasm ---\n{wasm_json}");
    }
    Ok(matched)
}

async fn run() -> Result<std::process::ExitCode, String> {
    let args = parse_args()?;
    let corpus = args
        .corpus
        .clone()
        .unwrap_or_else(|| repo_root().join("scripts/interactive-ui-parity/corpus"));
    let native = args.native.clone().unwrap_or_else(default_native);
    let wasm = args.wasm.clone().unwrap_or_else(default_wasm);

    let scenarios = load_corpus(&corpus)?;
    let scratch = TempDir::new("scenarios");
    let mut failures = 0usize;
    let mut total = 0usize;
    for scenario in &scenarios {
        total += 1;
        let work = TempDir::new(&scenario.name);
        if !run_once(
            &args,
            scenario,
            &native,
            &wasm,
            work.path(),
            args.fuzz.is_none(),
        )
        .await?
        {
            failures += 1;
        }
    }

    if let Some(count) = args.fuzz {
        println!("fuzz: {count} scenarios (seed {})", args.seed);
        for index in 0..count {
            total += 1;
            let scenario = fuzz_scenario(args.seed, index);
            let work = TempDir::new(&format!("fuzz-{index}"));
            if !run_once(&args, &scenario, &native, &wasm, work.path(), false).await? {
                failures += 1;
            }
        }
    }

    println!(
        "interactive-ui-parity: {total} scenarios, {failures} difference(s) — {}",
        if failures == 0 { "OK" } else { "FAILED" }
    );
    let _ = scratch;
    Ok(if failures == 0 {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    })
}

fn main() -> std::process::ExitCode {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .worker_threads(2)
        .build()
        .expect("tokio runtime");
    match runtime.block_on(run()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("interactive-ui-parity: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record with the compared fields (only `maxFrameBytes` varies in the
    /// constraint tests).
    fn record(max_frame_bytes: u64) -> Value {
        json!({
            "scenario": "unit",
            "carrier": "native",
            "mountOptions": { "maxFrameBytes": max_frame_bytes },
            "frames": [],
            "terminal": null,
            "scriptExhausted": false,
            "toolResult": {},
        })
    }

    /// V14-22 §4.1: the documented wasm clamp is accepted only at its exact
    /// value; identical records trivially pass.
    #[test]
    fn documented_constraint_only_accepts_exact_clamp() {
        assert!(documented_constraint_only(
            &record(1_048_576),
            &record(524_288)
        ));
        assert!(documented_constraint_only(
            &record(300_000),
            &record(300_000)
        ));
    }

    /// Negative controls (review blind-spot 1/3): a further-lowered wasm
    /// budget or any other field difference is a parity failure.
    #[test]
    fn documented_constraint_only_rejects_other_differences() {
        assert!(!documented_constraint_only(
            &record(1_048_576),
            &record(262_144)
        ));
        let mut wasm = record(524_288);
        wasm["frames"] = json!([{"lines": ["x"]}]);
        assert!(!documented_constraint_only(&record(1_048_576), &wasm));
        let mut wasm = record(524_288);
        wasm["terminal"] = json!({"done": true});
        assert!(!documented_constraint_only(&record(1_048_576), &wasm));
    }
}
