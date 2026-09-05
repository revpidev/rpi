# Rpi Parity Fixtures

> Landing directory and runbook for the parity/contract test baseline data (golden fixtures).Upstream reference: the committed goldens are recorded against `external/pi` @ `4181f66e6b3ccbef760c2966ecd8b596b926fec6` (0.84.1+) — re-recorded in v0.11 T18 (previously `2efa728` / 0.82.1, recorded for v0.1). The current pin always lives in `UPSTREAM.md`; regenerating fixtures that match the committed data requires the submodule checked out at the recording pin.Note: `events.jsonl` holds the **internal** AgentSession event transcript (with cumulative `message`/`partial` on `message_update`); the delta-only wire shape (`toJsonEvent`, json-event.ts @ 4181f66) applies only at the print/json + RPC stdout boundary and is covered by the print/RPC tests, not by these fixtures.
>
> The **shared normalization and diff implementation** lives in `rpi-test-support` (`normalize.rs` / `diff.rs`). In addition, each parity test strips numeric keys (`usage`/`details` etc., see the `STRIPPED_KEYS` list at the top of each test file — currently a local implementation in three test files) before diffing.Fixtures store **raw bytes**; timestamp / uuid / session id / cwd stripping happens at diff time, not at generation time.

## 1. Directory layout

```
fixtures/
├── README.md                # This file: runbook + itemized parity baseline list
├── generate-fixtures.mjs    # Generation script (pinned commit + fixed prompt scripts)
└── generated/
    └── <scenario>/
        ├── session.jsonl    # Real on-disk session file (file-backed SessionManager)
        └── events.jsonl     # AgentSession event transcript (same event shape as json mode)
```

## 2. Runbook (repeatable generation)

Prerequisites (one-time; `node_modules/` and `dist/` are both in `.gitignore`, so red-line G4 is not touched):

```bash
cd external/pi
git rev-parse HEAD   # must match the recording pin (4181f66e6b3ccbef760c2966ecd8b596b926fec6,
                     # see the note at the top of this file)
npm ci --ignore-scripts
# Build order matters (workspace deps); pi-ai uses build:offline so the
# network-fetching generate-models step does not rewrite pinned sources.
npm run build --workspace @earendil-works/pi-telemetry
npm run build --workspace @earendil-works/pi-tui
npm run build:offline --workspace @earendil-works/pi-ai
npm run build --workspace @earendil-works/pi-protocol
npm run build --workspace @earendil-works/pi-client
npm run build --workspace @earendil-works/pi-agent-core
npm run build --workspace @earendil-works/pi-coding-agent
```

Generate (from this repository root):

```bash
node fixtures/generate-fixtures.mjs            # all scenarios
node fixtures/generate-fixtures.mjs single-turn # a single scenario
```

Generator behavior: each scenario creates an isolated cwd / agentDir in a temporary directory (**does not read or write `~/.pi`**), drives `createAgentSession` with `fauxProvider()` (fixed `api: "faux"`) plus a fixed prompt script, and exports the real session file of the file-backed `SessionManager` together with the event sequence captured via `session.subscribe`.

Verify repeatability (spot-check one scenario): regenerate the same scenario into a temporary copy; after normalization the diff against the in-repo fixtures must be empty:

```bash
cp -r fixtures/generated/single-turn /tmp/single-turn-before
node fixtures/generate-fixtures.mjs single-turn
cargo run -p rpi-test-support --example normalize-diff -- \
  /tmp/single-turn-before/session.jsonl fixtures/generated/single-turn/session.jsonl
```

(`normalize-diff` example — see §4; exit code 0 = identical after normalization.)

> **`session.jsonl` is a byte-level repeatable anchor; `events.jsonl` is not.**The upstream faux provider splits deltas with `Math.random` (`faux.ts` `splitStringByTokenSize`), so delta boundaries and counts differ between runs. The parity granularity for `events.jsonl` is the **event-type sequence plus terminal message content** (delta boundaries are not part of the contract and never land in the session JSONL); the rpi-side faux uses deterministic chunking (deviation note in the header of `rpi-test-support/src/faux.rs`).

**Discipline**: fixture changes must be committed in the same commit as the behavior change, with the change described in the commit message.

## 3. Initial scenarios (delivered by T02)

| Scenario | Script highlights | Contract covered |
|----------|-------------------|------------------|
| `single-turn` | Single prompt → single text response | header/model_change/thinking_level_change/message entries; agent_start→turn_start→message_*→turn_end→agent_end→agent_settled event order |
| `tool-calls` | Two toolCalls (read + bash) → real tool execution → closing text | toolcall_* event order, toolResult entries, tool_execution_* events, source order of both tools |
| `steering-followup` | steer mid-stream, followUp mid-stream afterwards | Queuing semantics: without tool calls both steer and followUp are delivered as subsequent turns (queue_update events, turn sequence) |
| `abort` | `session.abort()` mid-stream | Aborted assistant message persisted (stopReason=aborted), abort event order |
| `length-truncation` | stopReason=length closing | Persisted shape of length-truncated messages |
| `compaction-threshold` | 8192 window / 4096 reserve / 512 keep: three Q&A rounds, threshold triggers two compaction rounds (split-turn prefix + UPDATE iteration), third round's prepare is empty and silent | CompactionEntry (firstKeptEntryId/usage/details/fromHook=false), compaction_start/end event order, tokensBefore recomputation, estimatedTokensAfter |
| `compaction-overflow` | 16384 window: overflow error ("prompt is too long") → recovery compaction → retry succeeds | Overflow recovery path, willRetry=true event order, budget reset after one recovery |

Completion plan (task index): the compaction scenarios shipped with **T08**; RPC coverage ships with **T10** — via in-process per-command contract tests for all 32 commands (`crates/rpi/tests/rpc_mode_test.rs`, anchored to the upstream RPC protocol doc) plus three-mode parity of the scenarios above (`crates/rpi/tests/parity_headless_test.rs`). No separate RPC transcript fixtures are recorded (the 32-command wire protocol is fully enumerated by the contract tests; transcripts would add no coverage).

### 3.1 resources case group (delivered by T09)

`fixtures/generated/resources/`: golden JSON produced by the real upstream modules (skills/prompt-templates/theme/keybindings/settings-manager/resource-loader, dist builds). The Rust-side parity test is `crates/rpi/tests/parity_resources_test.rs` (normalized diff reuses rpi-test-support; absolute paths in the golden data were replaced with `<path>` at generation time, and the Rust side applies the same replacement via `Normalizer::with_path`).

Generate (from this repository root):

```bash
node fixtures/generate-resources-golden.mjs                  # all 6 groups
node fixtures/generate-resources-golden.mjs themes settings  # single group
```

| Case group | Input | Contract covered |
|------------|-------|------------------|
| `skills-battery` | Upstream `test/fixtures/skills/` 13-case directory + read-only copy of `skills-collision/` (`input/`, never modifies external/) | `loadSkills()` name/description/filePath/baseDir/sourceInfo/disableModelInvocation + warning/collision diagnostic shapes; first-come-first-served conflicts |
| `prompt-dsl` | 21 embedded (template body, args string) cases in the script | `parseCommandArgs` quote awareness + all `substituteArgs` forms (`$1..$N`/`$@`/`$ARGUMENTS`/`${N:-d}`/`${@:-d}`/`${ARGUMENTS:-d}`/`${@:N}`/`${@:N:L}`, missing slots → empty string, no recursion) |
| `themes` | 11 embedded custom theme JSONs + built-in dark/light | ANSI color tables after `loadThemeFromPath` parses both color modes (truecolor/256color): vars references, 256-color integers, `""` defaults, thinkingMax fallback, invalid-value diagnostics; color table snapshot of parsed built-in themes |
| `keybindings` | 5 embedded legacy key-name configs | `migrateKeybindingsConfig`: legacy name migration, new name wins on new/old conflict, definition order + extras alphabetical order, raw values pass through |
| `settings` | 5 embedded deepMerge cases + 8 migration cases | `deepMergeSettings` (single-level shallow merge for nesting / replacement at depth ≥ 2 / arrays and scalars replaced, observed through the `SettingsManager.fromStorage` getter surface) + 4 legacy-format migrations (queueMode/websockets/skills object/retry.maxDelayMs) |
| `resource-loader-e2e` | `input/` multi-level tree (home `.agents/skills`, global agentDir, `.agents/skills` inside a git repo, cwd `.rpi`, settings-declared paths, CLI paths, invalid theme JSON, outside-repo isolation case) | Full `DefaultResourceLoader` pipeline: rank order (project settings > project auto > user settings > user auto > CLI extras), first-come-first-served name conflicts, git repo root ancestor scan upper bound, context files global→root→leaf order, theme/prompt conflicts and full invalid-theme warning diagnostics |

The e2e tree is prepared by the script and the Rust test repeating the same flow (`prepareE2eTree`): copy `input/` to a temp dir, create a `.pi/` twin for every `.rpi/` (upstream reads `.pi`, rpi reads `.rpi` — intentional naming difference; the golden data uniformly records the `.rpi` spelling), and create a `repo/.git` marker directory that git cannot track.

**Engine-related exclusions** (golden data pins only the stable parts; see generation script comments): `invalid-yaml` diagnostic message text (JS yaml vs serde_yaml), trailing newline of block scalars in `multiline-description` (serde_yaml does not keep `|` trailing newlines at EOF), `invalid-color-value-type` (typebox vs handwritten validator wording), `invalid-json-document` (JS SyntaxError vs serde_json error text).

## 4. Normalization / diff usage

```rust
use rpi_test_support::{diff_jsonl, diff_event_sequence, Normalizer};

// Session JSONL parity (line order included):
diff_jsonl(expected_fixture, actual_output)?;

// Event sequence parity (event-type order):
diff_event_sequence(expected_events, actual_events)?;
```

CLI form (spot checks, manual parity):`cargo run -p rpi-test-support --example normalize-diff -- <expected> <actual>` — normalizes each side, diffs, and prints the first difference (line number + context).

Normalization rules (`rpi-test-support/src/normalize.rs`):

- `timestamp` keys → type-preserving constants (number → `0`, string → `"<ts>"`)
- id keys (`id`/`parentId`/`fromId`/`firstKeptEntryId`/`toolCallId`/`sessionId`/`responseId`/`parentSession`) and uuids anywhere → the uniform placeholder `<id:N>` (first-appearance order)
- ISO-8601 timestamps inside strings → `<ts>`
- configured cwd / agentDir path prefixes → `<path>`
- everything else is kept byte-for-byte

In addition, each parity test strips numeric keys per scenario before `diff_jsonl` (`STRIPPED_KEYS`: `parity_headless_test.rs` strips `usage`/`details`, `parity_compaction_test.rs` strips `usage`/`tokensBefore`/`estimatedTokensAfter`, `parity_tools_test.rs` strips `usage`/`willRetry`/`details`) — token accounting numbers do not participate in parity; this stripping currently lives in three test files (not yet moved down into rpi-test-support).

## 5. Itemized parity-level baseline list

Six upstream documents (`external/pi/packages/coding-agent/docs/`) are the byte/behavior-level parity baselines. The table below registers"document item → parity anchor"; anchor status fills in as tasks progress (✅ = anchored, ⏳ = planned task).

### 5.1 `session-format.md` (T07 home)

| Item | Anchor | Status |
|------|--------|--------|
| File location `sessions/--<path>--/<ts>_<uuid>.jsonl` | T07 unit tests + fixtures header | ⏳ T07 |
| Session version (v1→v2→v3 migration, current v3) | T07 migration cases | ⏳ T07 |
| Entry base (`id` 8-hex / `parentId` / ISO `timestamp`) | all `generated/*/session.jsonl` | ✅ T02 |
| SessionHeader (incl. `parentSession` variant) | first line of `generated/*/session.jsonl` | ✅ T02 (parentSession variant ⏳ T07) |
| SessionMessageEntry (user/assistant/toolResult) | `single-turn` / `tool-calls` / `abort` | ✅ T02 |
| ModelChangeEntry / ThinkingLevelChangeEntry | lines 2/3 of each fixture | ✅ T02 |
| CompactionEntry (firstKeptEntryId / retainedTail / usage / details / fromHook) | `compaction-threshold` / `compaction-overflow` fixtures | ✅ T08 (firstKeptEntryId form; retainedTail read compatibility see D-012) |
| BranchSummaryEntry / CustomEntry / CustomMessageEntry / LabelEntry / SessionInfoEntry | — | ⏳ T07/T08 |
| Extended messages (bashExecution / custom / branchSummary / compactionSummary) | — | ⏳ T07/T08 |
| Tree Structure / Context Building algorithm | T07 unit tests | ⏳ T07 |
| Persisted shape of stopReason=length / aborted | `length-truncation` / `abort` | ✅ T02 |

### 5.2 `rpc.md` (T10 home)

| Item | Anchor | Status |
|------|--------|--------|
| Protocol framing (JSONL request/response/event) | RPC transcript fixtures | ⏳ T10 |
| All 32 commands (prompt/steer/follow_up/abort/new_session/get_state/get_messages/set_model/cycle_model/get_available_models/set_thinking_level/cycle_thinking_level/get_available_thinking_levels/set_steering_mode/set_follow_up_mode/compact/set_auto_compaction/set_auto_retry/abort_retry/bash/abort_bash/get_session_stats/export_html/switch_session/fork/clone etc.) | RPC transcript fixtures + contract tests | ⏳ T10 |
| steer/followUp/abort event semantics | `steering-followup` / `abort` event transcripts | ✅ T02 (SDK layer; RPC layer ⏳ T10) |

### 5.3 `compaction.md` (T08 home)

| Item | Anchor | Status |
|------|--------|--------|
| Trigger conditions / cut-point rules / split turns | T08 golden cases (`compaction/golden.json`) + `compaction_runner_test` + compaction fixtures | ✅ T08 |
| CompactionEntry / BranchSummaryEntry structure | `compaction-threshold` / `compaction-overflow` fixtures (CompactionEntry); BranchSummaryEntry preparation/filling in T08 golden unit tests | ✅ T08 (BranchSummaryEntry persistence ⏳ T12/T16) |
| Summary Format section template (Goal/Constraints/Progress/…/Critical Context) | T08 `compaction/prompts/*.txt` byte-for-byte comparison | ✅ T08 |
| Message Serialization | T08 golden cases (serializeConversation) | ✅ T08 |
| session_before_compact / session_before_tree extended semantics | T15 extension event parity | ⏳ T15 |
| Settings (threshold fields) | T08 cases (reserveTokens/keepRecentTokens); settings file wiring `parity_resources_test::parity_settings_*` | ✅ T08 + ✅ T09 |

### 5.4 `keybindings.md` (T11/T12 home)

| Item | Anchor | Status |
|------|--------|--------|
| Key Format parsing | T11 unit tests | ⏳ T11 |
| Default binding tables for all actions (per-table across 12 sections) | T12 binding-table snapshot golden files | ⏳ T12 |
| Custom config merge semantics | T09/T12 cases | ⏳ T12 |

### 5.5 `tmux.md` / 5.6 `terminal-setup.md` (T11/T12 home, byte-sequence level)

| Item | Anchor | Status |
|------|--------|--------|
| tmux recommended config and `csi-u` behavior | T12 terminal capability detection cases | ⏳ T12 |
| Per-terminal (Kitty/iTerm2/Apple/Ghostty/WezTerm/Alacritty/VS Code/Windows Terminal/xfce4/IntelliJ) settings and escape sequences | T11/T12 VirtualTerminal frame parity (CSI 2026 jitter removed) | ⏳ T11/T12 |
