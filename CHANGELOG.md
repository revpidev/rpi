# Changelog

## [0.1.5] - 2026-09-27

### 主线

- **上游追平 v0.86.1**（V15-01…15，ADR-0029 + ADR-0031 就地重钉）：行为金标准从 `9841914`（v0.85.0+）升级到 `19451accd`（v0.86.1+1；首轮 174 commits + 增量 11 commits），15 个宿主任务恢复行为对拍绿，渲染面首次执行**字节等价门**（G14——快照零重录）。分域摘要见 `changes/v0.1.5.md`（发布 changelog 单一事实源），关键用户可观测面：
  - **协议与会话**：**[BREAKING]** mid-conversation system messages——prompt/工具声明改由 transcript 承载（`TranscriptContext` 品牌收窄；deferred-tools 机制随上游退场）；per-model compaction 预算（`compaction.modelOverrides`）；尾部工具结果超限不再放弃压缩（#9740）；`--resume`/`--continue`/`--session` 渐进发现与精确解析；prompt cache warming（`cacheWarming` 设置 + `usage` 会话条目 + `cache_warming_decision` 扩展事件）；`ctx.sessionToolResults` additive ABI（ADR-0030）。
  - **Providers**：Meta provider + Muse 订阅 OAuth（`/login meta`）；内建目录重生成 @ v0.86.1（41 目录 / 1443 模型，Radius 公共目录 + per-tier prompt-cache 生命期）；SSE 空闲超时默认**不限**（rpi#54，D-103——`httpIdleTimeoutMs: 300000` 可恢复旧默认）。
  - **工具 / CLI / 扩展**：内建工具默认 strict-prefer JSON-schema 采样；bash 时长可读渲染（`1h 3m 4s`）与信号退出码（`128+N`）；**[BREAKING]** `user_bash` handler 失败即中止 `!` 命令（不再回落本地 shell）；`pi.on()` 返回退订句柄；`ctx.modelRegistry.stream()/streamSimple()`；RPC steer/follow_up 走扩展 input handler；修复扩展 render hook 在主题互斥锁上的死锁（rpi#52 e2e 发现）。
  - **TUI**：剪贴板验证写入链（平台命令 → WSL interop → #9618/#9688 门控 OSC 52，100 KB 上限；无头/远程会话回退）——D-104；LaTeX `cases`/嵌套 script、WezTerm Kitty 图像、CJK 标点补全边界、Alt 滚轮加速等修复族；**流式渲染 O(lines²) → 线性**（rpi#53：sourcepos 查表化 + 块级组件复用 + Arc 端到端共享，200 KB folded delta ~227ms → ~8ms，字节等价）。
- **插件线（六插件 lockstep）**：**新第一方插件 `rpiv-todo`**（TE34–36：`todo` 工具/overlay/`/todos` 命令/会话分支重放——跨 compaction 与 `/reload` 存活；registry 键 `rpiv-todo`，首发随本 RC）；subagents 重定基 v0.70.0（**[BREAKING]** `fallbackModels` 移除——重试不再自动换模型；worktree 准入/预算/allowlist 治理族）；mcp-adapter 重定基 v2.34.0+（加密文件 OAuth 凭据仓、CIMD、`/mcp edit`、directTools search 等）；ask-user-question 转录渲染摘要行（rpi#52）+ 主题死锁修复；rpiv-mono pin `0fdf4f8`。
- **rpi 自有修复**：#52（ask-user_question 转储渲染 + 宿主锁作用域）、#53（流式渲染效率，上述）、#54（SSE 空闲超时默认）。

### Internal

- workspace version bumped to 0.1.5-rc.1 + Cargo.lock synced; full gates zero failures (workspace 6843 cases at TE36 closeout).
- deviations D-101（核销）/D-103/D-104/TE-D43（转正）全闭环；rpi-pages registry 六插件矩阵与 RC 端点随本 RC 发布刷新。

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
