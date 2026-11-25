# V19-00 replay and Linux memory harness

This directory provides the shared measurement harness for v0.1.9. It drives the real interactive `UiCommand` tree and regular/fullscreen renderers with recorded inputs; it does not call a provider, read real credentials, or require a physical terminal. It is a development/test tool, not a distributed CLI command or a packaging path.

## Build

The replay target requires the release profile and the `v19-benchmark` feature. The feature exposes the existing `VirtualTerminal` to the replay binary; normal `rpi` builds do not include the target.

```bash
cargo build --release -p rpi-ext-statusline -p rpi-test-native-plugin
cargo build --release -p rpi --bin rpi-v19-replay --features v19-benchmark
```

The same workspace `Cargo.lock`, compiler, target and release profile must be used for an A/B comparison. Record the source commit and the hash of the reviewed measurement overlay separately.

## Inputs and scenarios

Generate or verify the immutable replay inputs:

```bash
python3 scripts/v019-benchmark/generate_inputs.py
python3 scripts/v019-benchmark/generate_inputs.py --check
```

`fixtures/v019-benchmark/bundles/manifest.json` records each bundle's byte length, SHA-256 and suite. There are 17 formal CPU scenarios, one smoke scenario and seven memory scenarios. They cover:

- new, approximately 1 MiB Markdown, and code-heavy retained histories;
- plain text, code, CJK, Mermaid and long tool-call arguments;
- regular/fullscreen, follow/scroll-back, 80×24, 200×60 and mid-stream resize;
- Mermaid streaming/off, existing statusline auto behavior and `liveTokens` off;
- no extensions, the real default statusline extension, and a real cumulative `message_update` native test extension;
- custom, Kimi/Tiktoken and GLM/HF model families.

The v0.1.8 baseline has no `estimate` mode; no such baseline is synthesized. V19-07 adds candidate-only estimate measurements using these same inputs.

## Run

```bash
python3 scripts/v019-benchmark/run_replay.py \
  --binary target/release/rpi-v19-replay \
  --bundle fixtures/v019-benchmark/bundles/code-fullscreen-1mib-main.json \
  --output-dir /tmp/rpi-v19-main-baseline \
  --repeat 3
```

Every repeat is a fresh process. The logical cwd is `<sandbox-root>/<scenario>` (default `/tmp/rpi-v19-replay-sandbox/<scenario>`), not a child of the evidence output directory. The runner resets that dedicated sandbox before each process, so separate baseline/candidate output roots still render identical paths. Use `--sandbox-root` only for another dedicated, non-shared location; do not run the same scenario concurrently. The runner:

- verifies the input against the manifest and passes the expected SHA-256 to the child;
- sets `RPI_V19_DISABLE_ADAPTIVE_BACKPRESSURE=1` for the candidate-side internal switch (the stable build simply ignores it);
- rejects non-release builds, missing/malformed logs, broken frame sequences and incorrect hashes;
- requires every cumulative-extension delta to be observed exactly once;
- requires identical final VT screen hashes and frame-output hash sequences across repeats;
- saves raw frames, terminal bytes, stdout/stderr, per-run summaries and an aggregate `report.json`.

`RPI_V19_BENCHMARK_LOG` is set by the runner. The renderer's opt-in probe writes JSONL with layout, paint, output-buffer construction, pure-compute, terminal-I/O, frame-interval and output-hash fields. Pure compute excludes terminal write/VirtualTerminal work; `terminal_io_ns` is reported separately.

Reports include process CPU/wall medians, `CPU/rendered_frame`, `CPU/input_delta`, pooled pure-compute p95, frame-interval p95, input-to-visible p95, statusline worker CPU when the named thread exists, and VirtualTerminal cost. `/proc/<pid>/task` samples preserve tid/comm/jiffies for attribution instead of guessing at worker cost. The VirtualTerminal is in-process, so its cost is measured by `terminal_io_ns`, not assigned to a fictitious external process.

For an arena attribution run, pass `--arena 1`, `--arena 2` or `--arena 4`; this sets the standard `MALLOC_ARENA_MAX` value for that child only. The default run does not set it. V19-02's replay binary applies the same early `mallopt(M_ARENA_MAX, 1)` default as the production entries, so an unset variable now measures the explicit default-1 behavior. Set `RPI_V19_ARENA_PROBE=1` to run the development-only early-entry heap/child-environment probe instead of a replay; `cargo test -p rpi --features v19-benchmark --test arena_early_init_test` exercises its default/override/invalid matrix.

Run the complete CPU or memory matrix sequentially:

```bash
python3 scripts/v019-benchmark/run_suite.py --suite cpu \
  --binary target/release/rpi-v19-replay --output-dir /tmp/rpi-v19-cpu \
  --repeat 3 --source-commit <source-commit> --overlay-sha256 <harness-patch-sha256>
python3 scripts/v019-benchmark/run_suite.py --suite memory \
  --binary target/release/rpi-v19-replay --output-dir /tmp/rpi-v19-memory \
  --repeat 1 --source-commit <source-commit> --overlay-sha256 <harness-patch-sha256>
```

The suite runner writes a host/build/binary/Cargo.lock identity index and links every raw report. Memory bundles use a file-backed SessionManager and the production session factory, so `/new` and `/resume` are real lifecycle transitions. Named sample events synchronize with the runner through marker/ack files; the runner captures the live process before acknowledging, and missing captures fail the run. Memory timing includes those sampling pauses and is never used for the CPU gate.

## Backpressure report (V19-06)

The fixed-cadence CPU gate renders exactly once per replay event through `render_now`, so it never sees the driver's tick-driven schedule. The adaptive 60/30/20 fps backpressure (ADR-0049 decision 5) is measured separately, and its numbers never count towards the gate:

```bash
python3 scripts/v019-benchmark/run_suite.py --suite backpressure \
  --binary target/release/rpi-v19-replay --output-dir /tmp/rpi-v19-backpressure \
  --repeat 3 --source-commit <source-commit> --overlay-sha256 <harness-patch-sha256>
```

The backpressure suite runs the CPU scenarios with `--render-mode throttled`: the replay leaves `RPI_V19_DISABLE_ADAPTIVE_BACKPRESSURE` unset, drives `tick` after every event instead of `render_now`, waits for the frame a `keyboard_input` event owes (measuring input-to-visible latency on the real schedule) and drains the final frame before reading the terminal. Frame boundaries then depend on measured frame costs, so the runner only requires identical final screens across repeats and reports the per-run frame sequences.

Each frame event carries the schedule state at frame start: `backpressureTier` (60/30/20), `requestedGenerations`/`renderedGenerations` (latest-wins), `supersededRequests` (requests folded into a pending frame), `latencyPromotions` (input frames that bypassed the backoff), `tierChanges` and `lastFrameCostNs`. The run summary exposes them as `backpressure` (`tierFrames` counts frames per tier); the suite report lists them per run under `backpressurePerRun`. Compare the report with the fixed-cadence report of the same binary: the throttled run keeps the same final screens while trading intermediate frames for lower CPU.

## Stable v0.1.8 source identity and instrumentation

Keep an independent detached worktree at stable commit `16062830e9aa4ee305c41dbc811d91995885b5b1` for read-only source verification. To obtain the same internal frame metrics from stable, use a separate disposable measurement worktree at that commit and apply the reviewed V19-00 measurement overlay. The overlay explicitly contains replay/probe/test-fixture instrumentation, mechanical pin-provenance updates and the user-approved export `template.css/template.js` synchronization; it contains no later functional or optimization commits. Do not describe it as instrumentation-only.

For each stable measurement, record:

- stable source commit and worktree diff;
- harness commit/patch SHA-256;
- an itemized diff proving the overlay is limited to the V19-00 instrumentation, pin provenance and approved mechanical export assets;
- the build commands above and identical input manifest.

The unmodified stable checkout remains the source-of-truth; the instrumented measurement build is explicitly described as stable plus observability, never as an unrelated release or an rc.5/debug baseline.

## Linux `/proc` capture and mapping budgets

Capture one process and its direct children at a named sampling point:

```bash
python3 scripts/v019-benchmark/proc_sample.py capture \
  --pid <pid> --label streaming --output /tmp/rpi-v19-memory/streaming
```

The capture preserves raw `status`, `smaps_rollup`, `smaps` and `top` output for every PID, plus host/OS/libc/terminal metadata. Summaries include VmSize/VmRSS/VmHWM, RssAnon/RssFile/RssShmem, Threads, PSS, private/shared fields and path+permissions mapping groups with dev/inode identity, including `---p` guard mappings. Parent and child reports remain separate; SHR is never added to RES.

Compare two one-process mapping summaries and execute the shared new-read-only-code budget:

```bash
python3 scripts/v019-benchmark/proc_sample.py compare \
  --baseline /tmp/.../baseline/summary.json \
  --candidate /tmp/.../candidate/summary.json \
  --output /tmp/.../mapping-delta.json
python3 scripts/v019-benchmark/proc_sample.py check-budget \
  --report /tmp/.../mapping-delta.json
```

All positive read-only executable mapping deltas share one budget: SHR and file-backed RSS (RssFile) must each be ≤2048 KiB. Binary size is not added to either value. Deltas remain net changes per path+permissions; a same-path replacement binary is identified by dev/inode and excluded from `existingSharedKibGrowth`, rather than counting the whole candidate as new code or hiding its delta under existing growth. The report has explicit attribution slots for jsonschema codegen, other v0.1.9 code, existing mappings and new non-code mappings; V19-01 and later tasks must fill in source attribution rather than hiding growth in an unassigned bucket. New non-code mappings are reported, not automatically exempted. PSS deltas remain available to detect shared-page privatization.

`proc_sample.py compare --attribution <file>` accepts one attribution object. The schema-probe commands accept `--attribution-dir <dir>`: `compare` reads `<mode>-run-<N>.json`, while `compare-replay` reads `run-<N>.json`. Each object supplies non-empty `jsonschemaCodegen.source` and `otherV019ReadOnlyCode.source`, plus `existingMappings` and `newNonCodeMappings` lists. Every positive existing or new non-code Shared_Clean delta needs a matching `path` + `permissions` entry with a non-empty `reason`; comparison fails closed when coverage is missing. Completed reports carry `budgetAttribution` and `attributionComplete: true`. This completeness check is separate from `budgetError`: both must pass, and explained resident-page growth stays visible in `budgetCheck` rather than being zeroed or hidden.

Required memory sampling points are startup-stable, first tokenizer, streaming, message_end, 50-turn idle, `/new`, `/resume`, model-family switch and PNG-over-budget. The memory matrix includes empty/long history, default/no extensions, bidirectional Kimi/GLM switches and 40 real PNG images. Run default and no-extension variants for attribution, but only default-extension results can satisfy the candidate gate. For arena overrides, append `--arena 1`, `--arena 2` or `--arena 4` to the memory suite command; the default run does not set `MALLOC_ARENA_MAX`.

## V19-01 schema-validation probe

Build the `v19-benchmark`-only probe in each A/B source tree with the same workspace release profile. Record the build log and process resource usage separately. The probe accepts `none`, `models`, `settings` and `both`; models calls `ModelConfig::load` and therefore the real `ModelsJson::validator`, while settings executes both public settings validation entry points and their scoped runtime validators.

```bash
cargo build --release -p rpi --bin rpi-v19-schema-probe --features v19-benchmark
python3 scripts/v019-benchmark/schema_probe.py run \
  --binary /tmp/rpi-v19-01-ab/rpi-v19-schema-probe \
  --output-dir /tmp/rpi-v19-01-schema-candidate \
  --repeat 3 --source-commit <source-commit> --overlay-sha256 <patch-sha256> \
  --cargo-lock <path-to-the-actual-source-tree-Cargo.lock>
python3 scripts/v019-benchmark/schema_probe.py compare \
  --baseline /tmp/rpi-v19-01-schema-baseline/suite-report.json \
  --candidate /tmp/rpi-v19-01-schema-candidate/suite-report.json \
  --output /tmp/rpi-v19-01-schema-comparison
python3 scripts/v019-benchmark/schema_probe.py compare-replay \
  --baseline /tmp/rpi-v19-01-replay-baseline/suite-report.json \
  --candidate /tmp/rpi-v19-01-replay-candidate/suite-report.json \
  --scenario memory-empty-default --label startup \
  --output /tmp/rpi-v19-01-replay-comparison
```

Run baseline and candidate binaries from the same measurement path and replace the measurement binary by rename so dev/inode records the generation change without charging the entire candidate as new code. Preserve both binaries and their hashes outside that path. `--cargo-lock` is supported by both `schema_probe.py run` and `run_suite.py`; it is optional for a current-tree run and required when replaying a binary from another source tree, and defaults to the checkout's `Cargo.lock`. Use the same filesystem as the stable baseline for file-backed code-page evidence; a tmpfs executable moves those pages to `RssShmem` and is not valid for the RssFile budget. Each fresh process pauses before and after initialization for full `/proc` capture. Reports include top/status/smaps metrics, initialization CPU/wall time, allocation counts and bytes, binary size and mapping-level code-page deltas. The four modes are independent measurements; never add their memory deltas together. Source inspection of `jsonschema 0.61.2` `meta_codegen`/`validator_for_draft` must accompany the memory evidence for the native generated-meta-validator claim; `evaluate`, `AsRef` and wasm fallbacks are outside that claim.

## Self-test

After the release build:

```bash
bash scripts/v019-benchmark/test.sh
```

The self-test runs input verification, `/proc` parser/budget unit tests, two deterministic smoke replays, incorrect-hash and missing-log negative tests, a debug-profile rejection, a real `/proc` capture and a zero-delta budget check. Logs default to `/tmp/rpi-v19-harness-self-test`.

## Full-workspace gate environment

Run the workspace gate from a clean shell. Inherited agent/subagent markers change resource-loader and extension behavior and make unrelated targets fail (for example `ask_user_question_e2e`, `parity_resources_test`, `resource_loader_test`, `context_with_system` and `l0_load`):

```bash
env -u RPI_SUBAGENT_CHILD -u RPI_NO_GLOBAL_CONTEXT -u AI_AGENT \
  TMPDIR=/tmp cargo test --workspace --no-fail-fast
```

Use the standard `/tmp` filesystem (not a long ancestor path) so repo-ancestor discovery and fixed-width path assertions match the recorded gates. Do not set `MALLOC_ARENA_MAX` for the default gate: the production default is applied in-process, and forcing `MALLOC_ARENA_MAX=1` changes test-thread scheduling; the Kitty cache tests are serialized through the shared test caps lock, but the recorded default gate remains the no-override run.
