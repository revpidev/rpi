//! V19-01 schema-validation measurement binary; not a distributed CLI surface.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rpi::core::config_schemas::{validate_runtime_settings_document, validate_settings_document};
use rpi_ai::models_json::ModelConfig;
use serde_json::{Value, json};

struct CountingAllocator;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static DEALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static DEALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        DEALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        DEALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AllocationSnapshot {
    allocations: u64,
    deallocations: u64,
    allocated_bytes: u64,
    deallocated_bytes: u64,
    retained_bytes: u64,
}

impl AllocationSnapshot {
    fn capture() -> Self {
        let allocations = ALLOCATIONS.load(Ordering::Relaxed);
        let deallocations = DEALLOCATIONS.load(Ordering::Relaxed);
        let allocated_bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
        let deallocated_bytes = DEALLOCATED_BYTES.load(Ordering::Relaxed);
        Self {
            allocations,
            deallocations,
            allocated_bytes,
            deallocated_bytes,
            retained_bytes: allocated_bytes.saturating_sub(deallocated_bytes),
        }
    }
}

fn checkpoint(directory: &Path, name: &str) -> Result<(), String> {
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("create sample directory: {error}"))?;
    std::fs::write(directory.join(format!("{name}.marker")), name)
        .map_err(|error| format!("write {name} marker: {error}"))?;
    let acknowledgement = directory.join(format!("{name}.ack"));
    let deadline = Instant::now() + Duration::from_secs(30);
    while !acknowledgement.is_file() {
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {name} acknowledgement"));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

fn parse_args() -> Result<(String, PathBuf, PathBuf, PathBuf), String> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.len() != 4 {
        return Err(
            "usage: rpi-v19-schema-probe <none|models|settings|both> <sample-dir> <summary> <models-json>"
                .to_owned(),
        );
    }
    if !matches!(
        arguments[0].as_str(),
        "none" | "models" | "settings" | "both"
    ) {
        return Err(format!("unknown schema probe mode {}", arguments[0]));
    }
    Ok((
        arguments[0].clone(),
        PathBuf::from(&arguments[1]),
        PathBuf::from(&arguments[2]),
        PathBuf::from(&arguments[3]),
    ))
}

async fn initialize(mode: &str, models_json: &Path) -> Result<Value, String> {
    let mut models_validated = false;
    let mut settings_validated = false;
    let mut runtime_settings_validated = false;
    if matches!(mode, "models" | "both") {
        let config = ModelConfig::load(Some(models_json)).await;
        if let Some(error) = config.error() {
            return Err(format!("models validation failed: {error}"));
        }
        models_validated = true;
    }
    if matches!(mode, "settings" | "both") {
        validate_settings_document(&json!({})).map_err(|error| format!("settings: {error}"))?;
        settings_validated = true;
        validate_runtime_settings_document(&json!({}))
            .map_err(|error| format!("runtime settings: {error}"))?;
        runtime_settings_validated = true;
    }
    Ok(json!({
        "modelsValidated": models_validated,
        "settingsValidated": settings_validated,
        "runtimeSettingsValidated": runtime_settings_validated,
    }))
}

fn run() -> Result<(), String> {
    let (mode, sample_dir, summary_path, models_json) = parse_args()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(|error| format!("create probe runtime: {error}"))?;
    checkpoint(&sample_dir, "before")?;
    let before = AllocationSnapshot::capture();
    let started = Instant::now();
    let validation = runtime.block_on(initialize(&mode, &models_json))?;
    let initialization_nanos = started.elapsed().as_nanos();
    let after = AllocationSnapshot::capture();
    checkpoint(&sample_dir, "after")?;
    let summary = json!({
        "schema": "rpi-v19-schema-probe-v1",
        "mode": mode,
        "initializationNanos": initialization_nanos,
        "beforeAllocations": before,
        "afterAllocations": after,
        "validation": validation,
    });
    let bytes = serde_json::to_vec_pretty(&summary).map_err(|error| error.to_string())?;
    std::fs::write(&summary_path, bytes)
        .map_err(|error| format!("write summary {}: {error}", summary_path.display()))
}

fn main() {
    if let Err(error) = run() {
        eprintln!("schema probe failed: {error}");
        std::process::exit(1);
    }
}
