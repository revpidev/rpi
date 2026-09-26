# Changelog

## [0.1.5] - 2026-09-27

### Main line

- **Upstream parity to v0.86.1** (V15-01…15, ADR-0029 + ADR-0031 re-pinned in place): the behavioral gold standard moves from `9841914` (v0.85.0+) to `19451accd` (v0.86.1+1; 174 commits first pass + 11 incremental), fifteen host tasks restored behavioral-parity green, and the render plane ran the byte-equality gate for the first time (G14 — zero snapshot re-records). Per-domain summaries live in `changes/v0.1.5.md` (the release changelog single source of truth); key user-observable surfaces:
  - **Protocol & sessions**: **[BREAKING]** mid-conversation system messages — the prompt and tool declarations ride the transcript (`TranscriptContext` brand narrows the provider trait surface; the deferred-tools mechanism retires with upstream); per-model compaction budgets (`compaction.modelOverrides`); trailing oversized tool results no longer abort compaction (#9740); progressive `--resume`/`--continue`/`--session` discovery with exact resolution; prompt cache warming (`cacheWarming` setting + `usage` session entries + the `cache_warming_decision` extension event); the `ctx.sessionToolResults` additive ABI (ADR-0030); agent retry backoff capped by the new `retry.maxAgentDelayMs` setting (default 60 s) and compaction cancellation races closed (#8826/#9340/#9777, V15-10).
  - **Providers**: the Meta provider + Muse subscription OAuth (`/login meta`); built-in catalog regenerated @ v0.86.1 (41 catalogs / 1443 models; Radius public catalog + per-tier prompt-cache lifetimes); SSE idle timeout now defaults to unlimited (rpi#54, D-103 — set `httpIdleTimeoutMs: 300000` to restore the old default).
  - **Tools / CLI / extensions**: built-in tools default to strict-prefer JSON-schema sampling; bash durations render readably (`1h 3m 4s`) with signal exit codes (`128+N`); **[BREAKING]** a failing `user_bash` handler aborts the `!` command (no local-shell fallback), and RPC-mode `bash` now runs the same interception; `pi.on()` returns an unsubscribe handle; `ctx.modelRegistry.stream()/streamSimple()`; RPC steer/follow_up run extension input handlers; the extension render-hook theme-mutex deadlock fixed (found by the rpi#52 e2e).
  - **TUI**: verified clipboard writes (platform commands → WSL interop → #9618/#9688-gated OSC 52, 100 KB cap; headless/remote fallbacks) — D-104, with the WSL PowerShell chain actually reaching `powershell.exe` (pre-stable review fix); the LaTeX `cases`/nested-script, WezTerm Kitty image, CJK-punctuation completion, Alt-wheel acceleration fix family; **streaming renders O(lines²) → linear** (rpi#53: sourcepos lookup table + per-block component reuse + end-to-end `Arc` sharing; 200 KB folded delta ~227 ms → ~8 ms, byte-identical).
- **Plugin line (six-plugin lockstep)**: **the new first-party plugin `rpiv-todo`** (TE34–36: the `todo` tool / overlay / `/todos` command / session-branch replay — survives compaction and `/reload`; registry key `rpiv-todo`, debuting with this release); subagents rebased to v0.70.0 (**[BREAKING]** `fallbackModels` removed — retries never switch models; the worktree-admission / budget / allowlist governance family; budgets honored at the call level — top-level and per-task `toolBudget`); mcp-adapter rebased to v2.34.0+ (encrypted-file OAuth credential store, CIMD, `/mcp edit`, directTools search, top-level `addedToolNames`); ask-user-question transcript render summary line (rpi#52) + the theme-mutex fix; the rpiv-mono pin `0fdf4f8`.
- **rpi-native fixes**: #52 (ask_user_question dump rendering + host lock scoping), #53 (streaming render efficiency, above), #54 (SSE idle timeout default).

### RC verification fixes

- **rc.2**: the rpiv-todo overlay never appeared in a real interactive session (the tool worked; the UI didn't) — the interactive mode attached the extension UI bridge after `bind_extensions`, so `session_start` fired with no UI and the plugin's overlay foreground claim never ran. Both boot and every session-switch rebind now attach the bridge first (upstream binds the two atomically); a real-mode boot e2e pins the ordering.
- **rc.4**: Escape during streaming no longer crashes the `rpi-tui-driver` thread — the escape handler spawned the session abort with a bare `tokio::spawn` on the driver thread, which has no Tokio runtime (`there is no reactor running…`); both abort branches (and the same latent `/llama` search-debounce spawn) now route through the `spawn_async` fallback, pinned by runtime-less-thread regression tests.

### Pre-stable review fixes (shipped in v0.1.5-rc.3)

- The WSL clipboard write chain actually reaches `powershell.exe` now (the write-path runner dropped stdout; real WSL silently fell back to an unverified OSC 52 success) — query calls return real stdout bytes, writer calls get no output pipe at all (a daemonizing writer can no longer hang the drain); pinned by real-runner regressions including a fake-`wslpath` end-to-end chain.
- Zero-TTL MCP servers keep their direct tools while connected (#566): the live-overlay cache entry stripped of the declared `ttlMs` (a `ttlMs == 0` entry is invalid by design and used to drop the connected server's tools); regression-tested.
- RPC `bash` runs the `user_bash` extension interception with the same fail-closed contract as the interactive `!` path (all three arms pinned by an RPC-harness test).
- `mcp({ connect })` / `mcp({ search })` report `addedToolNames` at the tool-result top level (upstream shape; was nested under `details`; e2e-pinned).
- Assorted repairs, each with a regression test unless noted: the encrypted-file OAuth key decoder fails closed on multi-byte UTF-8 (no panic); macOS Local Network Privacy diagnostics match the real Rust io error texts; the #536 broker-before-grant order is pinned; dead DCR registrations are cleared from the store (#503 gate + URL scoping unit-tested); `oauth.clientMetadataUrl` interpolates env vars (pinned); ask_user_question multi-select answers strip control characters; post-login default models match the regenerated catalog (zai/zai-coding-cn/cerebras/xai) and a test now pins every default against the catalog; the todo reducer compares JSON numbers numerically (`1` vs `1.0` is a no-op); `agent_capabilities` reports real `executable`/`restrictedCount`/`restrictionSources` under a capability ceiling; the Anthropic beta set rides the header only; the wasm SDK `unsubscribe` releases the state lock before the `off` host call (review-verified; no re-entrant-host harness yet); the subagents skill docs no longer teach the removed `fallbackModels` (asset-scan-pinned).

### Pre-stable review fixes (round 2, shipped in v0.1.5-rc.4)

- Overlay composition matches `applyModelsJson`: base-model `baseUrl`/`compat` mapping (radius oauth keeps the model gateway URLs), `config.models` upserts into the base catalog with `findModelDefaults` inheritance (the T10 subset replaced the base list), `modelOverrides` last, extension lists with the same defaults, and both overlay wrappers recompute per call so dynamic catalogs stay live; a configured `apiKey` is no longer dropped, no longer erases the base OAuth method (stored OAuth credentials keep resolving), and the upstream structural validations apply; streaming follows `streamWith` (the base serves the apis it declares; every other model streams through that model's own api provider, unserved apis terminate with the upstream `No API provider registered` error).
- The post-retry abort gate (`agent-session.ts:1239-1241`) and the in-loop abort break close the abort-during-backoff path that could still start a post-abort compaction.
- MCP OAuth token exchange and refresh carry the origin-scoped service headers; `client_credentials` reads its stored registration URL-scoped, clears a token-less dead one, and forwards a live registration's DCR-issued secret; proxy argument validation runs before the approval gate; the CIMD + secret error uses the upstream literal.
- Extension/RPC/TUI repairs: the SDK `toolExecute` route releases the state lock before running a tool handler (`subscribe`/`unsubscribe` no longer deadlock); the RPC fail-closed response carries the handler's message; the interactive `!` interception gains its regression harness; `SessionCompactFailedEvent` joins the exported hook types; `complete_summarization` uses `biased`; the auto-compaction catch classifies cancel races as aborted; cache-warming timers exit on `clear_run`; `sync_compaction_model` no longer silently no-ops under contention; deferred post-login re-selection checks the session identity; the WSL clipboard temp file is 0600 atomically; the editor CJK separator class is composed from the shared table; and the clipboard/WezTerm/subagents tests are hermetic against hostile host environments.

### Internal

- workspace version bumped through 0.1.5-rc.1 → rc.2 → rc.3 → rc.4 with Cargo.lock synced each time; full gates zero failures (6863 cases at rc.3, after the pre-stable review batch; rc.4 re-ran the full gate after the round-2 batch and the driver-thread fix).
- deviations D-101 (closed) / D-103 / D-104 / TE-D43 (promoted) all resolved; the rpi-pages registry six-plugin matrix and RC endpoints refreshed with this RC.

## [0.1.4] - 2026-09-18

### Main line

- **Upstream parity to v0.85.0** (V14-01…18, ADR-0023): the behavioral gold standard moves from `4181f66` (v0.84.1+) to `9841914` (v0.85.0+, 698 commits / 866 files); eighteen parity tasks restored behavioral parity green item by item with an "upstream anchor → rpi landing → assertion evidence" triple each. Domain summaries in `changes/v0.1.4.md` (the release changelog's single source of truth); key user-observable surfaces:
  - **Protocol & sessions**: compaction trigger timing aligned (#6879 breaking — terminal turns no longer fire it); nine session-management repairs (fork compaction remap #8989 / import same-name #8985 / RPC abort cancelling manual compaction #8920, …); `message_update` restores cumulative usage (#7982), `toolcall_start` carries id/toolName (#7953), and the new `clear_queue` command (#8432).
  - **Providers**: Anthropic per-turn effort persistence and refusal fallback (managed-Claude request composition + providerThinkingLevel persisted); Bedrock raw response-header forwarding; the NO_PROXY semantics rewrite (#8737: root-domain/subdomain/IPv6/host:port/`*`); the default User-Agent across seven adapters; the model catalog regenerated against v0.85.0 data (GPT-6 Astra, compat fields).
  - **Tools / CLI / extensions**: the seven built-in tools resolve cwd at execution time against the session (#8627); three skills fixes (#8552/#7805/#8255); the `--` separator (#7269), `--use-theme` (#7722), session-scoped selector changes (#8356: Ctrl+S persists globally), Windows/WSL default bindings (#8372), config robustness (BOM #8337 / permissions #7779 / errors with paths #7829).
  - **TUI**: the full mouse-dispatch infrastructure + the selection-fix family (double-click word selection/right-click paste dedup/hover keeping list selection); fullscreen transcript search and jump indicators (#8800); the scrollbar redesign + verified copying (OSC 52 + readback + toasts); the LaTeX/table rendering fix family; the working indicator embedded in the editor border; in-place thinking toggles; three terminal-capability override envs (`RPI_HYPERLINKS`/`RPI_IMAGE_PROTOCOL`/`RPI_TRUE_COLOR` + `terminal.*` settings) and Alt+Enter's dual timeouts over SSH (#7899); the rpi-tui/config env divorce (`c505f4c19`, `RPI_TUI_DEBUG_REDRAW`/`rpi-tui-*.log` renamed, binary behavior unchanged).
- **M5 closeout**: the theme-validation split port (eb3e9feed — a lenient library cast + an app-installed validator); all fixtures re-recorded and pinned to 9841914; deviations D-092…D-100 all closed (3 registered + 5 retired + 2 retired-untriggered).
- **RC update channel** (V14-19, an rpi-native requirement with no upstream counterpart): `rpi update --rc` / `rpi update --extensions --rc` / `rpi install <name> --rc` pre-release channel flags (flagless always means stable; every update decision falls out of the semver total order); the new endpoint `api/latest-rc-version.json`; registry resolution gains channel filtering (stable excludes pre-releases); build.yml auto-marks `-rc` tags prerelease, with `releases/latest` and the install.sh fallback staying stable.
- **Sixteen rounds of RC pre-release verification** (`v0.1.4-rc.1`…`rc.16`): field-verification fixes (rc.1–rc.4, rc.8/rc.10 — see the sections below and the plugin section), three comprehensive review closures (rc.5 / rc.11 / rc.12), the de-branding pass (rc.9), the model-catalog refresh (rc.13), the highlight-engine eradication (rc.14), and two statusline enhancements (rc.15/rc.16) — each detailed below.

### Plugin rebases (pins switched atomically with TE27: subagents v0.66.0 / mcp-adapter v2.32.1, ADR-0025)

- **BREAKING: mcp direct tool naming** (TE23, R7.2.4 / #342/#346/#463/#455): server prefixes keep provider-valid `-`/`_`; direct/proxy tool names change for server names containing `-`/`_` (`my-server` no longer generates `my_2d_server_<tool>`); the candidate set becomes "original name first + legacy fallback", and server-level calls resolve the original upstream tool name first, failing closed on ambiguity. Migration: `settings.toolPrefix: "none"` or per-server `toolPrefix: "none"` for bare tool names, or update references to the new prefixed names; **no dual old/new registration**. Domain detail in `changes/v0.1.4.md`.

### Interactive custom-UI ABI (V14-20–V14-25, ADR-0024/ADR-0027)

- **A host ABI for extension-mounted interactive components (native + wasm dual carriers)** ([RPI-OWN] — upstream's `ctx.ui.custom()`/`Component` is an in-process object contract; rpi rebuilds the equivalent as a line-frame + polling JSON ABI): 7 additive `ui.*` host-calls, a component registry with overlay/editor-area mounting, line-frame composition with input dispatch, frame caps and wasm fuel budgets (traps surface as structured `fuelExhausted`/`handlerError` + forced unload; the host never crashes), a seven-exit-path cleanup matrix, and `ui.editExternal`; the dual-carrier consistency harness shows zero differences.
- **`ctx.sessionEntries` additive host-call** (ADR-0027): read-only active-branch entries (fail-closed); the mcp approval restore migrated to consume it (TE33, ABI-first + JSONL fallback for old hosts).

### New plugin: rpiv-ask-user-question (TE28–TE32)

- **The structured ask_user_question tool** (a first-party plugin; upstream rpiv-mono @ v2.9.0+): when requirements are unclear the model issues a 1–4 question questionnaire (options/previews/multi-select/notes); interactive terminals render a bottom tabbed overlay via the interactive UI ABI; RPC/ACP hosts degrade to a host-native per-question walker; non-interactive runs remove the tool. The contract surface matches upstream byte-for-byte (231 assertions all MATCH); two rounds of dialog-height stabilization (rc.8/rc.10 — padding always computed in non-input mode + padding inserted before the footer block, constant total height, hint hugging the bottom); ships lockstep with host Releases as `.rpix` and is indexed in the registry.

### statusline live_output frozen decode_ms (rc.16)

- **Decode-duration passthrough for stateless tok/s** (#50; `rpi-statusline`, PR #51): `rpi.live_output` gains `decode_ms` (first delta→now while streaming / frozen at first delta→end after `message_end`; always present, 0 before the first delta) — idle ticks recompute the same rate, and TTFT becomes the same-clock difference `elapsed_ms − decode_ms` (`Instant` twin anchors, monotonic across wall-clock jumps); precise tok/s scripts now render from a single snapshot as pure functions, with no per-session state file (all three persisted-state classes absorbed; the resume-orphan and stale-rate failure modes structurally eliminated). Zero changes to the host/ABI/subscription set; the previous 15 fields stay byte-identical.

### statusline live_output raw-material passthrough (rc.15)

- **`rpi.live_output` raw-material passthrough** (#45; `rpi-statusline`, PR #49): 6 additive fields — `output_tokens` (provider-cumulative output tokens during the stream, present only when >0, never rewritten on silence), `text`/`thinking`/`toolcall` (the current message's accumulated raw text, identity to `*_chars`), `decode_started_at_ms` (the first delta's wall-clock anchor, TTFT excluded), and `message_id` (changing per message) — letting scripts do their own language-aware token estimation instead of a single chars/token factor (biased both ways for Chinese vs English). The host had long delivered the accumulated partial (full text + usage) into extension events; the fix retains and passes it through: zero changes to the host/ABI/subscription set, the original 8 fields byte-identical, and the native-measures-but-never-converts red line preserved.

### Highlight-engine cross-syntax panic eradication (rc.14)

- **syntect (fancy-regex) cross-syntax panics eradicated** (#47; present since v0.1.3 T17, not an rc regression): 39 grammars (HTML / Markdown→HTML / PHP / Vue / Svelte / QML / JSP / Elixir, …) panicked on first hit when parsing `<script>` blocks, `~r` literals, and the like, pushing into the six incompatible grammars' contexts (catch_unwind contained it, but stderr polluted the TUI and whole blocks fell back to plain text). Build-time excision + a full rewrite of 21,692 `Direct` references (Named 19,858 / File by name 1,799 / same-scope rebound 34 / 1 case left degrading) + relinking (198→192); the vendored fork `vendor/syntect` introduced (5.3.0 + two visibility patches, behaviorally identical to the registry version). Embedded JS in HTML regains coloring; Elixir `~r` lines degrade to plain text; mod.rs files eliminated repo-wide (standard §3.1).

### Model catalog refresh (rc.13)

- **Built-in model catalog refreshed to the 2026-09-14 snapshot** (catalog-only; the behavioral pin stays `9841914`, deviation D-101): chat 1354 → 1397 models (Bedrock regional families +34, GPT-5.4 Codex retired, DeepSeek Flash merged, OpenRouter bulk/alias families expanded, …); images 50 → 54; zero behavioral change; the rpi-pages remote catalog synced. Converges after v0.1.5's upstream parity raise.

### Final pre-release re-review fixes (rc.12)

- **P1**: the three SSE `finish().unwrap()` sites now propagate errors (a malformed stream — a 1 MiB unwritten tail + incomplete UTF-8 + EOF — previously panicked and hung the turn with no terminal event, a gap in rc.11's fail-fast claim); the codex SSE body read now races the cancel signal (Esc/abort no longer waits out `httpTimeoutMs`, a same-class residue of rc.11 P1-5).
- **P2**: sequential tool-batch panic guards; the SSE line cap bounding only the unterminated tail; a 1 MiB cap on the pi-messages reader; `streamSimple toolChoice` forwarded by eight adapters; session newline-repair failure propagation; no silent `--rc` downgrade on range mismatch; the Editor stale-render click focus; the mcp `session_tree` approval restore across the gate window; subagents syntheticPaths fail-closed at collection + a tracked-path pre-check; openrouter-images cancellation.
- **Test gaps**: parent_id cycle guards, compaction session_id retention, the sequential-batch panic, the `--rc` downgrade — every zero-coverage area rc.11 named is now pinned.

### Pre-release review fixes (rc.11)

- **P0**: interactive-mode panic-hook wiring (both main-screen/alt-screen variants; the terminal always recovers after any panic); raw-mode leak fixes on the first-run setup flow's error paths.
- **P1**: `read` negative `limit` back to JS semantics without panicking; Windows bash tool kill/timeout convergence; subagents `syntheticPaths` escape validation (directories outside the worktree can no longer be deleted); parallel tool batches emitting the missing `tool_execution_end` on panics; the mcp adapter Ready/on_ready race window eliminated (the gate-flakiness root cause); all eight SSE adapters aborting body reads immediately.
- **P2 hardening**: wasm guest memory caps; SSE/Bedrock frame caps; empty-`data` event tolerance; atomic `auth.json` writes; session `parent_id` cycle guards; a 32 MiB smart-fetch response cap (deviation registered); compaction `session_id` upstream semantics; `--rc` without a pre-release index degrading to "already up to date"; extension-name component validation; unique `parity_runner` names, etc. (the full list in `changes/v0.1.4.md`).
- **Docs**: four inaccurate changelog claims corrected (the `immediate_retry` event name, the `x-api-source` header name, `imageGenerationModels`/mlx, the verified-copy readback gap registration); the CI entry rewritten to the local-gate end state.

### Residual pi-identifier de-branding (rc.9, ADR-0028)

- **de-pi pass** (a one-shot cleanup, after a repo-wide scan, of `pi` identifiers still appearing in rpi's own name): the default system prompt's `operating inside pi` → `rpi` (models stop introducing themselves as pi) and the "Pi documentation" section rewritten; CLI help changed from "Update pi" to "Update rpi" throughout and the positional alias `update pi` → `update rpi` (**`pi` no longer accepted**); Provider User-Agents and the Codex `originator` → `rpi`; temp-file prefixes `pi-*` → `rpi-*`; the orchestration skill renamed `pi-subagents` → `rpi-subagents` (skill layout bumped to v3, old installs migrating automatically on upgrade, user copies preserved); the ecosystem data plane (read-compatible) — the manifest key `#pi`→`#rpi`, mcp-adapter event names, and the model catalog api kind `"pi-messages"`→`"rpi-messages"` (old values auto-normalized). Real-defect fix: the skill doc's `PI_SUBAGENT_WAIT_TOOL_ENABLED` spelling unified with the code's `RPI_`. Deliberately kept: the `radius.pi.dev` gateway / OAuth `client_id` (external dependencies) and the "derived from Pi" acknowledgment.
- **Behavior changes (upgrade notes)**: `rpi update pi` is no longer accepted (use `rpi update rpi` / `rpi update self`); Provider-side UA/originator statistics change; the share page's local-preference keys change once; old `pi-subagents` skill installs migrate to `rpi-subagents`.

### RC-window comprehensive review closure (rc.5, 1 Blocker + 6 Majors)

- **Blocker rpi#33**: RPC abort hanging forever while manual compaction/branch summary was in flight (a V14-02 regression) — all four upstream `_resolveIdleWaitIfIdle` call sites restored (manual/auto compaction completion and the navigateTree finally).
- **Majors**: rpi#34 `update --extensions --rc` no longer downgrading installed stable extensions (unified `is_newer_package_version`, both directions); rpi#35 `pi.getFlag` seeing pending defaults during factory (upstream #8423); rpi#36 `/llama` explicit refresh forcing `allowNetwork:true` (the live catalog no longer overwritten by the stored snapshot under RPI_OFFLINE, plus the `providers` passthrough); rpi#37 `/model` search default-pinning semantics aligned upstream (`" default"` + prefix matching); rpi#38 `Box::handle_mouse` x-boundary guards (padding clicks no longer mis-trigger); rpi-pages#4 the site's stable-endpoint pre-release guard.
- **Backlogged fixes**: rpi#29 real-time step status for parallel batches (the subagent_wait list no longer stuck at "queued"); rpi#30 `globalConcurrencyLimit` wired as a run-scoped concurrency semaphore; rpi#27 in-place replacement of same-key belowEditor widgets (no more multi-widget flashing/sinking).

### RC-window verification fixes (rc.1–rc.4, field feedback)

- **rc.1**: a permanent one-row gap above the editor (the upstream ever-present `Spacer(1)` gap — output no longer touching the `⠋ Working` border); two DBG debug prints removed from the fullscreen mouse-dispatch path (+ a structural `deny(print_*)` guard in rpi-tui).
- **rc.3**: settings-list value-column alignment (the upstream label width `min(36)` mistyped as `min(30)`, misaligning once 32-column labels passed the cap); investigation closure: "borders wrap to two rows" on fullscreen↔normal switching is the terminal's Ambiguous-width rendering (upstream reproduces it too, an ADR-0020 residual risk; fullscreen being full-bleed matches upstream).
- **rc.4**: event-driven refresh dying after hot-switching TUI modes in `/settings` ("one keystroke, one frame") — `render_handle` now resolves the current renderer at call time (aligned with upstream's Proxy semantics); keyboard input dying after clicking the dock editor/selector in fullscreen — `SharedEntry::handle_mouse` returning gesture/focus targets to the inner shared component (aligned with upstream's `dispatchMouseEvent` forwarding).

### Internal

- workspace version bumped to 0.1.4 + Cargo.lock synced; full gates zero failures (case counts per the `changes/v0.1.4.md` end state).
- parity-checklist §3.8 v0.1.4 increment mapping recorded; rpi-pages changelog/latest-version synced.

## [0.1.3] - 2026-09-03

### Added

- **statusline live token counting** (V13-10, lead-in PR): the `statusLine.liveTokens` key enables ~1Hz script re-runs during streaming; stdin gains a pure-measurement `rpi.live_output` block; riders — eight-event payload forwarding parity completed + the `ctx.sessionFile` additive host-call (ADR-0022), fixing dual-instance same-cwd data mix-ups at the root (TE-D34 §1).
- **subagents authoritative parent-session location** (V13-02): `parent_session` prefers `ctx.sessionFile`, with the directory heuristic demoted to a hardened fallback (mtime floor + stem-shape filtering); four consumers switched to the authoritative session id (closing TE-D16).

### Fixed

- **statusline / smart-fetch, same family: events dead or host channel dangling after `/resume`** (v0.1.3 follow-up, same root cause): statusline's install early-return meant zero event subscriptions on the new host (frozen footer) + the old channel's poll timer burning a dangling cookie + the old loop exited with nothing restarting it; smart-fetch's `STATE.host` frozen on host 1 → ctx.cwd/toolUpdate going through the old cookie (dangling, or cwd falling back to the process directory). Fixed with the mcp-adapter discipline: unconditional event subscriptions + rebindable CHANNEL/host + refresh_loop restarted on rebind; each plugin gained a rebind_second_host regression (verified red with the fix reverted).
- **The "🔌 MCP" status line vanishing after `/resume` (and the MCP extension going missing entirely)** (v0.1.3 follow-up): session replacement reloads the same-path cdylib; dlopen's per-path memoization let the plugin's `STATE: OnceLock` survive across hosts, and the second `install` failed wholesale with `plugin already initialized` — the status line had been cleared with nothing to re-push it, and mcp tools/flags/events silently went missing in the new session. Fix: the host channel (fn pointer + cookie) became a rebindable `RwLock` — the rebind keeps the process-wide runtime + dispatcher, re-registers flags/events on the new host, resets the direct-tool surface, rediscovers config from the new cwd, re-pushes the tool surface and status line, and re-arms bridge-retry (the new host's UI bridge attaches only after switch_session returns; direct pushes land on a null bridge); every call site reads the current channel, and the old host receives zero pushes after the rebind. Dual-end regressions: rebind_second_host (the full /resume lifecycle) + native_same_path_reload (same-path second-host reload).
- **edit falsely reporting `Could not find edits[N]` while the edit succeeded** (V13-11 follow-up, rpi#18): a preview race (the UI draining slower than the agent writes) stacked with `update_display` building the call component before the backfill, freezing the red error on screen; the result component is now built first (aligned with upstream's renderResult in-place rebuild), reducing the race damage to upstream's flash-and-gone.
- **`rpi update --extensions` missing untracked plugins** (v0.1.3 follow-up): update/list only iterated the settings `packages` entries while the loader loads every manifest directory under the install roots — untracked installs (legacy versions / hand copies) were silently skipped, surfacing as "only one updates" in multi-plugin setups; update now matches the loader's discovery (identity-deduped into the batch, never written back to settings; `rpi update <name>` can target them), `rpi list` marks them `(untracked)`; a failing registry source no longer short-circuits the batch, and an untracked install absent from the registry (404) downgrades to a skip note.
- **Streaming request total timeouts killing active streams** (V13-08, lead-in): the total-deadline mis-mapping replaced with a three-stage timeout (connect/headers/inter-chunk body idle, reset per chunk); all 9 SSE adapters covered, with codex / openrouter_images as two intentional exceptions.
- **write large-file streaming rendering O(n²)** (V13-09, lead-in): layered caches (stable-prefix window skipping recomputation + visible-content fingerprint skipping rebuilds + lazy repair_json + copy dieting 3→2; erratum vs the first announcement — the state copy is gone, the context-tail update and event payloads each keep one); 400-line streaming 2250ms → 245ms (9.2×).
- **Extension UI swap tearing** (V13-05, concurrency correctness): widget swaps atomized under a single lock (cross-container add-then-remove) + selector's single-lock clear+add+set_focus — no more missing or bare-editor frames.
- **Streaming render hot path** (V13-06): MessageUpdate queue consecutive-segment folding keeping the tail (K deltas in one drain → exactly 1 update_content) + update_content taking references to eliminate caller-side deep copies.

### Internal

- **Low-tier miscellany cleanup** (V13-07): mcp-adapter `!command` secret parsing moved to spawn_blocking; status-bar pushes skipped during no-UI bridge retries; `getAllTools` lazy queries; statusline change-ticks reusing fetch_ctx (12→6 host calls); TUI per-frame size reads 4→1 ioctl.
- Deviation registry summary: TE-D16 closed, TE-D35/36/37, D-088/089/090/091.
- M0 closeout: two lead-in PRs (`fix/stream-idle-timeout-write-perf` / `feat/statusline-live-token-count`) merged to main + gate cleanup (clippy 1.97 lints).
- workspace version bumped to 0.1.3 + Cargo.lock synced; full gates 5367 cases zero failures.

## [0.1.2] - 2026-08-19

### Added

- **First-party plugin rpi-ext-statusline**: the CC-compatible scriptable custom statusline (an L0 native plugin). Writing a `statusLine` key in `settings.json` enables it (command + padding/cropping parameters), with two placements; the script is driven via stdin/stdout following the CC statusline JSON protocol, zero new ABI. Field follow-ups: instant refresh on model/thinking-level/branch switches, a new-session transcript latch race fix, and data-fingerprint polling self-healing.

### Fixed

- The extension host losing its UI bridge after `/new` and `/resume` session switches — the mcp status bar vanished and MCP tool approvals were silently rejected (#1).
- With `apiKey` configured in `models.json`, `auth.json` was still forcibly required — a literal key mistaken for an environment variable name; now parsed per the upstream config-value DSL (#3).
- `/changelog` always showing the empty-entries placeholder — the changelog asset never landed; now `CHANGELOG.md` is embedded in the binary + the `parseChangelog` port + half of the onboarding display chain (#5).
- The `model_select` event never emitted — a captured-previous identity check short-circuited.
- Onboarding startup header branding residue: the upstream "Pi" verbatim copy replaced with rpi's actual capabilities, dropping the docs-query promise rpi doesn't ship and pointing to the official site (#7).

### Internal

- subagents orchestration skill docs and prompt templates localized for the structured entry (ADR-0021): removed the `workflowScript` teaching and unimplemented-mechanism sections; the installer auto-upgrades old versions via the `.rpi-layout-version` marker; tool descriptions completed with the `tasks`/`steps` composition entries (ADR-0018 decision 5).
- registry / package_manager / package_command rustfmt cleanup (formatting only).

## [0.1.1] - 2026-08-16

### Added

- **Extension distribution and installation**: `rpi install <name>` (the revpi.dev registry channel, semver selection + sha256 verification), `rpi install github:<owner>/<repo>` (the Release-artifact channel), the `.rpix` package format with atomic installs; `remove` / `list` / `update` support it all end-to-end.
- **First-party plugins**: rpi-ext-mcp-adapter (the MCP client adapter), rpi-ext-subagents (structured subagent delegation), and rpi-ext-smart-fetch (the full web_fetch pipeline) ship with host releases, auto-indexed on the official site.
- **Upstream alignment with Pi v0.84.1**: rpi-ai message types and stream-termination semantics, the provider fix cluster, transactional models refresh; the rpi-tui renderer refactor / LaTeX and Mermaid rendering / the layout engine / the fullscreen renderer (alt screen / mouse / kitty); UI-mode wiring.
- The official site revpi.dev: the extension index API, the edge download proxy, and the plugin catalog page.

### Fixed

- The main-screen renderer's `panic!` on over-wide rows killing sessions, and full-width rows drifting into garbled wraps — truncation-with-continued-rendering + pessimistic-width conservative truncation (ADR-0020 / D-086).
- Lone `=`/`-` rows inside `$$`/`\[` formula blocks mis-parsed as setext headings, cutting formulas before math rendering — an equal-length shadow-source rewrite before parsing (D-078 backfill).
- LaTeX falling back to raw text for the whole block on unknown commands — all 78 gaps filled per the KaTeX list (`\blacksquare`/`\Box`/long-arrow tails/negation macros as single symbols/parameterized macros with degraded rendering, D-087).
- Word-level diff panicking with `not a char boundary` on multi-byte characters (Chinese/full-width), killing the render thread — the trim boundary now advances by the last character's length.
- Multiple native plugins failing to co-load (abi_stable memoizing by type; switched to per-path loading); the SSE line limit aligned to 10MiB; input dead after fullscreen hot-switches and `/settings` hangs.

## [0.1.0] - 2026-08-15

- Initial release: interactive TUI / JSON-RPC / print three modes, a multi-provider model runtime (rpi-ai), agent sessions with compaction, the skills / prompt templates / themes resource system, the bash / read / edit / write built-in tools, and the extension host (wasm sandbox + native L0).
