//! V19-07 FR-F memory report: RES/RssAnon of every bundled tokenizer family
//! after load, and the release of the previous family on a model switch.
//!
//! Run in release mode on Linux (`cargo run -p rpi-ext-statusline --release
//! --example tokenizer_memory`); the same source is applied to the baseline
//! commit for the same-machine A/B.

use rpi_ext_statusline::tokens::{
    load_family_for_memory_report, process_rss_anon_kib, release_tokenizer_cache,
};

const FAMILIES: [(&str, &str); 6] = [
    ("deepseek", "deepseek-chat"),
    ("kimi", "kimi-k3.1-pro"),
    ("glm", "glm-5.3"),
    ("qwen", "qwen3.5"),
    ("hunyuan", "hy4-preview"),
    ("minimax", "minimax-m3"),
];

const SAMPLE: &str = "The quick brown fox jumps over the lazy dog. 中文测试 with numbers 0123456789 and <|im_start|> special tokens.\n";

fn measure(label: &str, baseline: (u64, u64), out: &mut String) {
    // Return every freed table to the OS first (glibc keeps large freed
    // chunks in its arenas): the deltas then show what the CURRENT tables
    // retain, which is what the resident memory of the statusline tracks.
    unsafe {
        libc::malloc_trim(0);
    }
    // Settle the allocator before reading the counters.
    let (anon, rss) = process_rss_anon_kib().expect("/proc/self/status");
    out.push_str(&format!(
        "{label:<28} RssAnon {anon:>8} KiB (+{:>6})  VmRSS {rss:>8} KiB (+{:>6})\n",
        anon.saturating_sub(baseline.0),
        rss.saturating_sub(baseline.1),
    ));
}

fn main() {
    let mut report = String::new();
    let (anon, rss) = process_rss_anon_kib().expect("Linux /proc");
    report.push_str(&format!(
        "process baseline           RssAnon {anon:>8} KiB           VmRSS {rss:>8} KiB\n"
    ));
    let baseline = (anon, rss);
    // Warm the sample string's pages so the first family does not pay for it.
    let black_hole = SAMPLE.len();
    measure("warm", baseline, &mut report);
    let _ = black_hole;
    for (family, model) in FAMILIES {
        let (count, retained) =
            load_family_for_memory_report(model, SAMPLE).expect("bundled family");
        measure(
            &format!("{family} loaded (count {count})"),
            baseline,
            &mut report,
        );
        report.push_str(&format!("{:<28} tables retain {retained:>8} bytes\n", ""));
    }
    // The last load evicted every earlier family: release and return to the
    // baseline.
    release_tokenizer_cache();
    measure("released", baseline, &mut report);
    // Model switch: load Kimi, then GLM — Kimi's tables must be gone.
    load_family_for_memory_report("kimi-k3.1-pro", SAMPLE).expect("kimi");
    measure("switch: kimi", baseline, &mut report);
    load_family_for_memory_report("glm-5.3", SAMPLE).expect("glm");
    measure("switch: kimi → glm", baseline, &mut report);
    release_tokenizer_cache();
    measure("switch: released", baseline, &mut report);
    print!("{report}");
}
