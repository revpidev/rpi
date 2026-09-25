# subagents recorded fixtures (pi-subagents v0.66.0, TE27-era recording)

> **Superseded as the active parity anchor by TE39 (2026-09-27)**: the `external/pi-subagents` submodule HEAD is
> now `b72714de95e612406b3461e63dfc182856333a7e` (v0.70.0; the active track reports live in `fixtures/generated/subagents-parity-v070/`).
> These v0.66.0 recordings remain the historical baseline (recorded by TE13 as a read-only snapshot outside the repository; zero writes to `external/`); the v0.70 track reuses the fixtures.json/fixtures-target.json corpus with the fallback/model legs re-anchored at v0.70 `model-resolution.ts` (TE39).

## Purpose and consumers

| File | Shape/content | Requirement | Owning task |
|------|----------|------|----------|
| `events/*.jsonl` + `events/expected.json` | Three groups of subprocess stdout event streams (willRetry failure-then-success / empty terminal text / tool error + empty reply) with expected terminal states | R7.1.1.3 | TE14 |
| `discovery/agents-tree/` + `discovery/materialize.json` | A discovery directory tree: broken frontmatter, nesting, `sync-backups` pruning, `.rpi` pruning, symlinked directories (including cycles) | R7.1.3.1–.4 | TE15 |
| `terminal-classification.json` | Sub-step terminal-state classification vectors + async-step effective thinking | R7.1.6.1/.2 | TE16 |
| `notify-fields.json` | Completion-notification field/line-format goldens | R7.1.7.2 | TE17 |

## Event streams (`events/`)

- Each JSONL line = one JSON event from the subprocess stdout (same shape as
  `crates/rpi-ext-subagents/tests/fixtures/child_stream.jsonl`:
  the `session` header + `agent_start`/`turn_start`/`message_start`/`message_update`
  (`assistantMessageEvent`)/`message_end`/`turn_end`/`tool_execution_*`/
  `tool_result_end`/`agent_end`/`agent_settled`).
- `expected.json`'s `expected` field was derived by reading the v0.66 sources (each entry's `anchors`
  gives file:line); `currentRpiAtM0` records rpi's state at TE13 (the differences attributed
  in the target-track report). TE14 replays the JSONL with `ChildRunState` and asserts
  `finalOutput`/`error`/`exitCode` match `expected`.
- **Semantics of the three groups**:
  1. `will-retry-then-success.jsonl`: the first provider attempt ends with `errorMessage` +
     `stopReason:"error"` + `agent_end willRetry:true`, then succeeds — after recovery the
     `errorMessage` must not linger (R7.1.1.1 #1919);
  2. `empty-terminal-text.jsonl`: `stopReason:"stop"` with empty-text content and
     `usage.output==0` — an empty terminal state diagnosed as empty-output
     (R7.1.1.2 #1921);
  3. `tool-error-empty-reply.jsonl`: an exploratory tool errors and the model replies empty —
     the empty-output diagnosis takes precedence over the stale tool error
     (R7.1.1.2 #1921).

> **TE14 landing note (2026-09-09)**: the three JSONL groups + `expected.json` are consumed directly by
> `terminal_classification_tests::recorded_event_stream_groups_match_upstream_expected` in
> `crates/rpi-ext-subagents/src/runner/foreground.rs` (`ChildRunState` replay +
> `synthesize_exit_from_parts`), asserting per group that `exitCode`/`error`/`finalOutput`
> match `expected`, all green; the M0 behaviors recorded in `currentRpiAtM0`
> have been superseded by the R7.1.1.1/.2 fixes.

## Discovery directory (`discovery/`)

- Files committed under `agents-tree/` cover: a valid agent, unclosed frontmatter,
  a line without a colon, **two fatal-frontmatter cases** (`async: maybe` / `timeoutMs: not-a-number`),
  a nested agent, and `sync-backups/` (to be pruned).
- `.rpi/` and symlinks are **not committed**; `materialize.json` rebuilds them in the test's
  temp directory: `.rpi/` is globally ignored by the repository `.gitignore` (`.rpi/`), and
  symlinks are unreliable on Windows checkouts. `materialize.json` pins the paths, targets,
  cycles, and expectations (the visible-agent set, pruned paths, silently-skipped set, and
  diagnostics set).
- **TE15 review correction (2026-09-09)**: TE13's initial version listed `broken-frontmatter.md` /
  `broken-no-colon.md` as diagnostics, which contradicts a close read of upstream v0.66.0 — both files
  hit `continue` in `loadAgentsFromDefinitionFiles` for missing name/description
  (silently skipped, no diagnostics); TE15 added the two fatal-frontmatter cases as the real
  diagnostics source and moved the original two to `expected_silent_skips`.
- Consumption: crate unit tests materialize per `materialize.json` and assert; the target-track
  harness's `discovery` mode uses the same tree cases for a field-by-field comparison against
  upstream `discoverAgents` (the "target-track discovery leg" in
  `scripts/subagents-parity/README.md`).

## Terminal classification and notifications

- `terminal-classification.json`: `child_status_union`/`projection_status_union` taken from
  v0.66 `src/shared/types.ts:398/537`; aggregation-semantics anchors at
  `src/runs/foreground/subagent-executor.ts:4228-4237` (stopped/timedOut/
  interrupted). After TE16 completes the full vector set this file can be extended
  (never silently changing semantics).
- **TE16 extension (2026-09-09)**: added `terminal_vectors.cases` (7 cases, including precedence
  and `stop-wins-over-timeout`), consumed by unit tests in
  `crates/rpi-ext-subagents/src/p1/parallel.rs`; the original `cases`/`thinking`
  shapes unchanged (append-only sections).
- `notify-fields.json`: line-format anchor at `src/runs/background/notify.ts:213-232`.
  After TE17 lands, `cases.expected_lines` serve as renderer round-trip assertion input.

## Shape pinning and change policy

The shapes in this directory (field names, directory layout, status sets) were pinned by TE13
and consumed directly by TE14–TE17. If implementation work finds a shape needs adjusting
(e.g. an upstream field name contradicting the close read), register it per the G2 policy as
"old shape → new shape + upstream evidence" and sync this README with the corresponding task
document; **never rewrite silently**.
