//! Counts-only probe cost harness (V19-07 FR-D R7 / R5.2.8 / ADR-0049
//! decision 10).
//!
//! `cargo test -p rpi --lib v19_counts_probe_bench -- --ignored --nocapture`
//!
//! Measures `ctx.sessionStats({countsOnly:true})` — the host-side
//! `SessionContextActions::session_counts` the statusline runs once per
//! finished assistant message — against a SHORT and a LONG session
//! (≥ 1 MiB, thousands of entries) on the same build and machine:
//!
//! - per-probe wall time in batches (so one probe is far above the timer
//!   resolution), reporting p50/p95 per batch and over independent runs;
//! - temporary allocations per probe through a counting global allocator;
//! - the hard budget: ≤ 1 probe per terminal is a call-count invariant
//!   (asserted by the statusline fixtures), the probe must not clone or
//!   iterate entries (allocations must not grow with the entry count) and
//!   the long-history p95 must stay ≤ short-history p95 × 110 %. When the
//!   timing noise leaves the ratio undecided the batch size and the number of
//!   independent runs grow — the 110 % threshold is never relaxed.
//!
//! The whole module is `cfg(test)`: the counting allocator is only linked
//! into the lib test binary.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use rpi_agent::messages::AgentMessage;
use rpi_ext_host::api::ContextActions;

use crate::core::agent_session::AgentSession;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

struct CountingAllocator;

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
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !new_pointer.is_null() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        }
        new_pointer
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Per-batch statistics of one probe run.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Batch {
    probes: u64,
    /// Wall time of the whole batch, in nanoseconds.
    elapsed_ns: u128,
    /// Allocations/bytes during the batch.
    allocations: u64,
    allocated_bytes: u64,
    /// p95 of the per-probe samples (ns).
    p95_ns: u128,
    p50_ns: u128,
}

fn percentile(sorted: &[u128], percent: usize) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let index = (sorted.len() - 1) * percent / 100;
    sorted[index]
}

/// Run `probes` counts-only probes, timing each one.
fn run_batch(session: &AgentSession, probes: u64) -> Batch {
    let actions = crate::core::extension_context::SessionContextActions::new(session);
    let allocations_before = ALLOCATIONS.load(Ordering::Relaxed);
    let bytes_before = ALLOCATED_BYTES.load(Ordering::Relaxed);
    let started = std::time::Instant::now();
    let mut samples = Vec::with_capacity(probes as usize);
    let mut sink = 0u64;
    for _ in 0..probes {
        let before = std::time::Instant::now();
        let counts = actions.session_counts().expect("bound session");
        sink = sink.wrapping_add(counts.assistant_messages);
        samples.push(before.elapsed().as_nanos());
    }
    let elapsed_ns = started.elapsed().as_nanos();
    std::hint::black_box(sink);
    samples.sort_unstable();
    Batch {
        probes,
        elapsed_ns,
        allocations: ALLOCATIONS.load(Ordering::Relaxed) - allocations_before,
        allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed) - bytes_before,
        p50_ns: percentile(&samples, 50),
        p95_ns: percentile(&samples, 95),
    }
}

/// Populate the session with `messages` assistant messages of about
/// `bytes_each` bytes of text (plus a user turn every 8 messages).
fn populate(session: &AgentSession, messages: usize, bytes_each: usize) -> usize {
    let paragraph = "The quick brown fox jumps over the lazy dog. ".repeat(bytes_each / 45 + 1);
    let text: String = paragraph.chars().take(bytes_each).collect();
    let mut appended = 0usize;
    let manager = session.session_manager();
    let mut manager = manager.lock().unwrap_or_else(|error| error.into_inner());
    for index in 0..messages {
        if index % 8 == 0 {
            manager
                .append_message(AgentMessage::User(rpi_ai::types::UserMessage {
                    role: rpi_ai::types::UserRole::User,
                    content: rpi_ai::types::UserContent::Text("question".into()),
                    timestamp: index as i64,
                }))
                .expect("append user");
        }
        manager
            .append_message(AgentMessage::Assistant(rpi_ai::types::AssistantMessage {
                role: rpi_ai::types::AssistantRole::Assistant,
                content: vec![rpi_ai::types::AssistantContent::Text(
                    rpi_ai::types::TextContent {
                        text: text.clone(),
                        text_signature: None,
                    },
                )],
                api: rpi_ai::types::ApiKind("openai-completions".into()),
                provider: "custom".into(),
                model: "m1".into(),
                response_model: None,
                response_id: None,
                provider_thinking_level: None,
                thinking_level: None,
                diagnostics: None,
                usage: rpi_ai::types::Usage::default(),
                stop_reason: rpi_ai::types::StopReason::Stop,
                error_message: None,
                timestamp: index as i64,
                deferred: None,
                end_turn: None,
                raw_stop_reason: None,
                duration_ms: None,
            }))
            .expect("append assistant");
        appended += 1;
    }
    appended
}

fn session_bytes(session: &AgentSession) -> usize {
    let manager = session.session_manager();
    let manager = manager.lock().unwrap_or_else(|error| error.into_inner());
    manager
        .get_entries()
        .iter()
        .map(|entry| {
            serde_json::to_vec(&entry.raw_value())
                .map(|bytes| bytes.len())
                .unwrap_or(0)
        })
        .sum()
}

/// `part` as a percent of `whole` (0 when `whole` is 0).
fn percent_of(part: u128, whole: u128) -> u128 {
    part.saturating_mul(100).checked_div(whole).unwrap_or(0)
}

/// Median of values.
fn median(values: &mut [u128]) -> u128 {
    values.sort_unstable();
    if values.is_empty() {
        return 0;
    }
    if values.len() % 2 == 1 {
        values[values.len() / 2]
    } else {
        (values[values.len() / 2 - 1] + values[values.len() / 2]) / 2
    }
}

/// The hard budget: long-history p95 ≤ short-history p95 × 110 %.
const BUDGET_PERCENT: u128 = 110;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "V19-07 counts-only probe cost report; run explicitly with --ignored --nocapture"]
async fn v19_counts_probe_bench_short_vs_long_history() {
    let short = crate::modes::interactive::test_support::build_test_session().await;
    let base_entries = {
        let manager = short.session.session_manager();
        let manager = manager.lock().unwrap_or_else(|error| error.into_inner());
        manager.get_entries().len()
    };
    let long = crate::modes::interactive::test_support::build_test_session().await;
    let long_messages = populate(&long.session, 4_000, 320);
    let short_bytes = session_bytes(&short.session);
    let long_bytes = session_bytes(&long.session);
    let long_entries = {
        let manager = long.session.session_manager();
        let manager = manager.lock().unwrap_or_else(|error| error.into_inner());
        (
            manager.get_entries().len(),
            manager.entry_total(),
            manager.message_counts().assistant,
        )
    };
    assert!(
        long_bytes >= 1024 * 1024,
        "long history must be ≥ 1 MiB: {long_bytes}"
    );
    assert!(
        long_entries.0 >= 3_000,
        "thousands of entries: {}",
        long_entries.0
    );
    assert_eq!(long_entries.2 as usize, long_messages);

    // Warm-up (page faults, lazy init) before any measurement.
    for _ in 0..3 {
        let _ = run_batch(&short.session, 256);
        let _ = run_batch(&long.session, 256);
    }

    // Batches: 20 × 2,000 probes per history, interleaved, so both see the
    // same machine state; then independent runs decide the ratio.
    let mut probes_per_batch = 2_000u64;
    let mut runs = 0usize;
    let mut short_p95s = Vec::new();
    let mut long_p95s = Vec::new();
    let mut short_allocs = u64::MAX;
    let mut long_allocs = u64::MAX;
    let mut short_bytes_per_probe = u64::MAX;
    let mut long_bytes_per_probe = u64::MAX;
    let mut report = String::new();
    let decided = loop {
        runs += 1;
        for _ in 0..20 {
            let short_batch = run_batch(&short.session, probes_per_batch);
            let long_batch = run_batch(&long.session, probes_per_batch);
            short_p95s.push(short_batch.p95_ns);
            long_p95s.push(long_batch.p95_ns);
            short_allocs = short_allocs.min(short_batch.allocations);
            long_allocs = long_allocs.min(long_batch.allocations);
            short_bytes_per_probe = short_bytes_per_probe.min(short_batch.allocated_bytes);
            long_bytes_per_probe = long_bytes_per_probe.min(long_batch.allocated_bytes);
        }
        let short_median = median(&mut short_p95s.clone());
        let long_median = median(&mut long_p95s.clone());
        // Decidable when the medians are above the timer's resolution and the
        // ratio is clearly inside or outside the budget band (a 5 % margin on
        // either side); otherwise grow the batch and run again.
        let resolution = 50u128;
        let ratio_percent = percent_of(long_median, short_median);
        let decided = short_median >= resolution
            && long_median >= resolution
            && (ratio_percent + 5 <= BUDGET_PERCENT
                || ratio_percent.saturating_sub(5) > BUDGET_PERCENT);
        report.push_str(&format!(
            "run {runs}: probes/batch {probes_per_batch}  short p95 {short_median} ns  long p95 {long_median} ns  ratio {ratio_percent}%  \
             allocs/probe short {} long {}  bytes/probe short {short_bytes_per_probe} long {long_bytes_per_probe}\n",
            short_allocs / probes_per_batch,
            long_allocs / probes_per_batch,
        ));
        if decided || runs >= 6 {
            break decided;
        }
        probes_per_batch *= 2;
    };
    let short_median = median(&mut short_p95s);
    let long_median = median(&mut long_p95s);
    // Percent of the short-history p95 (0 when the short p95 is below the
    // timer's resolution — then the run is not decidable and grows).
    let ratio_percent = percent_of(long_median, short_median);
    println!(
        "V19-07 counts-only probe bench\nshort history: {short_bytes} bytes, {base_entries} entries\n\
         long history: {long_bytes} bytes, {} entries ({} assistant messages)\n{report}\n\
         result: short p95 {short_median} ns, long p95 {long_median} ns, ratio {ratio_percent}% (budget ≤ {BUDGET_PERCENT}%), \
         allocations/probe short {} long {}, bytes/probe short {short_bytes_per_probe} long {long_bytes_per_probe}, decided={decided}, runs={runs}",
        long_entries.0,
        long_entries.2,
        short_allocs / probes_per_batch,
        long_allocs / probes_per_batch,
    );
    // Temporary allocations must not grow with the entry count (per probe:
    // the two identity strings of the counts shape).
    let short_bytes_per_probe = short_bytes_per_probe / probes_per_batch;
    let long_bytes_per_probe = long_bytes_per_probe / probes_per_batch;
    println!(
        "bytes per probe: short {short_bytes_per_probe}, long {long_bytes_per_probe};          short p95 {short_median} ns, long p95 {long_median} ns, ratio {ratio_percent}%"
    );
    assert!(
        long_bytes_per_probe <= short_bytes_per_probe + 64,
        "the probe allocates more on a long history: {long_bytes_per_probe} vs {short_bytes_per_probe} bytes/probe"
    );
    assert!(
        short_bytes_per_probe <= 512,
        "the probe allocates {} bytes",
        short_bytes_per_probe
    );
    // The timing budget is the hard gate.
    assert!(
        ratio_percent <= BUDGET_PERCENT,
        "long-history p95 {long_median} ns exceeds short-history p95 {short_median} ns × 110% ({ratio_percent}%)"
    );
}
