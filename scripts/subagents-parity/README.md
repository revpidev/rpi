# subagents parity harness (TE04 G3; dual-track rebase TE13)

Drives the pinned upstream pi-subagents and this crate's `build_rpi_args` / frontmatter parser /
`get_finalOutput` / context-overflow classifier / model-resolution vectors / discovery entry
points with the same fixture set, then diffs the normalized outputs item by item.

## Dual tracks (v0.1.5 rotation, TE37 / ADR-0029)

| Track | Upstream | Purpose | Report directory |
|----|------|------|----------|
| `regression` (**default for the v0.1.5 window**) | the current pin v0.66.0 (`0fc0eebb`, an out-of-repo snapshot of the submodule HEAD) | zero-regression baseline of the window (full mode set) | `fixtures/generated/subagents-parity-v066/` (the TE27-era baseline directory, kept comparable) |
| `target` | the new pin v0.70.0 (`b72714de`, an out-of-repo snapshot until TE39 switches the pin) | new-semantics parity and golden re-records | `fixtures/generated/subagents-parity-v070/` |

Track names rotate each rebase cycle (TE13 convention): during v0.1.4 the current pin was named
`target`; for v0.1.5 it is named `regression` (the default), and `target` denotes the new pin.
TE39 flips the default back to `target` together with the pin switch. The retired v0.48
archaeology face (live `pi-args.ts`) was removed with the TE37 rotation — the args leg is the
frozen v0.48 golden on both tracks. The two fixture files (`fixtures.json`, `fixtures-target.json`)
are shared and concatenated on both tracks. Both upstream legs read **snapshots** extracted by
`setup-target-source.sh` (the live worktree cannot serve: v0.66's discovery chain imports `yaml`,
unresolvable from a pristine `external/`).

## Running

```bash
# One-time prep: tsx installed externally; never written into external/
mkdir -p /tmp/rpi-subagents-parity-deps && cd /tmp/rpi-subagents-parity-deps \
  && npm init -y && npm install tsx@4 --no-save

cd <repo-root>

# Regression track (v0.66.0 = the submodule pin; default for the window)
bash scripts/subagents-parity/setup-target-source.sh   # extracts BOTH snapshots (target v0.70 + regression v0.66) + prod deps
node scripts/subagents-parity/run-parity.mjs

# Target track (v0.70.0 snapshot)
node scripts/subagents-parity/run-parity.mjs --track=target

# Re-record the argv/env frozen baseline ([RPI-OWN], ADR-0025 §4; v0.48-era worktree + RPI_SUBAGENTS_PARITY_ARGS_LEGACY=1 required)
node scripts/subagents-parity/run-parity.mjs --record-args-golden
```

The Rust leg is built by `run-parity.mjs` itself (near-zero cost on cargo cache hits) and **executed from a private copy**:
each plugin crate's parity example is uniquely named (here `subagents_parity_runner`), so `target/debug/examples/`
no longer suffers name collisions (P2-9), where the example would belong to whichever crate built
last and the mcp harness would overwrite it (a harness defect found during TE13 testing).
The private copy keeps the two harnesses out of each other's way while keeping example names compatible with existing docs.

Exit code: non-zero = **unattributed** differences exist (both tracks); `ATTRIBUTED-OK` = every
difference is triaged in the track's manifest.

### Target-track discovery leg (TE15, R7.1.3)

`--track=target` runs an extra `discovery` mode: the same tree case is materialized **by both sides** —
the upstream leg places the fixture's `<CFGDIR>` as `.pi`, the Rust leg as `.rpi`, and the output normalizes that
segment back to `<CFGDIR>`; both sides call the **real discovery entry points** (upstream v0.66
`discoverAgents(cwd, "user")`, rpi `discover_agents_with_user_dirs_with_diagnostics`), comparing
`agents` (name/source/path) and `diagnostics` (path/source/error) filtered to that tree. The upstream leg points HOME /
USERPROFILE / `PI_CODING_AGENT_DIR` at an out-of-repo sandbox and sets `PI_OFFLINE=1` (skipping
`npm root -g`); scope `user` makes v0.66 take the `discoverAgentsUncached` path, so multiple cases
within one process don't pollute each other; built-in agents and `~/.agents` are filtered out by
path on both sides. Symlink cases are created on non-Windows platforms only (both sides skip
them consistently; see `fixtures/subagents-v066/discovery/materialize.json`).

## Target-track upstream source (out-of-repo; zero writes to external/)

`setup-target-source.sh` uses `git -C external/pi-subagents archive <pin>` to extract BOTH snapshots —
the v0.70 target into `/tmp/rpi-subagents-parity-target-v070` and the current-pin (v0.66)
regression source into `/tmp/rpi-subagents-parity-regression-v066` (each overridable via
`RPI_SUBAGENTS_TARGET_SRC` / `RPI_SUBAGENTS_REGRESSION_SRC`; `--skip-regression` skips the latter) —
no checkout,
no `git worktree add`, no submodule HEAD changes — so
`git -C external/pi-subagents status --porcelain`
stays empty. Inside each snapshot, `npm install --omit=dev` installs the prod dependencies of the snapshot's own
`package.json` (the frontmatter/agents chain imports `yaml` at runtime). The fetch range needs only one
`git -C external/pi-subagents fetch --deepen=350 origin` (read-only).

## The [RPI-OWN] argv/env baseline

Upstream v0.65+ deleted `src/runs/shared/pi-args.ts` / `buildPiArgs` (sub agents moved to in-process
AgentSession), so the rpi subprocess model's argv/env assembly has no upstream counterpart (R7.1.0.4, ADR-0025 §4):

- both tracks compare against the **frozen golden file** `args-golden-v048.json` — recorded from
  the v0.48 upstream leg by `--record-args-golden` (session-base placeholders normalized to `<SESSION_BASE>`;
  only the non-inline cases of fixtures.json are re-recorded; re-recording requires a v0.48-era
  worktree and `RPI_SUBAGENTS_PARITY_ARGS_LEGACY=1` since every reachable pin deleted pi-args.ts);
- when M2/M3 change argv/env via the R7.1.4 series, the corresponding task updates the golden file and registers
  "old expectation → new expectation + evidence" per G2;
- **TE18 addendum**: new semantics with no upstream recorder (`--exclude-tools` and other behaviors upstream never
  had on the argv surface) land as **inline [RPI-OWN] goldens** in the `expected` field of
  `fixtures-target.json` cases, compared by the orchestrator's `compareArgsTarget` directly against the Rust leg
  (bypassing the upstream leg);
  semantic correctness is pinned by the task's §3.3 rules + crate unit tests, with the inline goldens guarding
  against future regressions.

### TE18 addition: the model-resolution leg (target track)

`--track=target` runs an extra `model` mode: shared fixtures (registry/parentModel/origin) drive both the v0.66
snapshot's `resolveSubagentModelOverride` / `buildModelCandidates` and the corresponding rpi implementations
(`parity::resolve_subagent_model_override_public` /
`build_model_candidates_public`), both sides emitting `{resolved|candidates}` or `{error}`
(fail-closed throws diff identically on both sides), covering empty-registry passthrough / hit normalization /
thinking-suffix retry / miss fail-closed (#1093) and the origin-aware candidate chain.

## Attribution rules (target track)

Every target-track difference must hit `expected-target-diffs-v070.json` (the v0.1.5 manifest; the regression
track keeps the historical, emptied `expected-target-diffs.json`), or the report lands in `### unattributed` with a non-zero exit code:

- `upstream-semantics`: new-tag behavior rpi hasn't adopted yet (attached R entry + owning task);
- `rpi-deviation`: deviations where rpi's existing implementation disagrees with both pins;
- each entry carries `mode/case`, `section`, `r`, `owner`; the report summarizes in two sections.
- A difference field of `null` means the Rust-side function isn't implemented yet (e.g. M0's `isContextOverflow` /
  `isRetryableModelFailureAttempt`), attributed through the same two sections — never silently skipped.

> **TE14 landing note (2026-09-09)**: `expected-target-diffs.json` has been emptied —
> after R7.1.2.1's five mode-table entries landed (`REQUEST_LIMIT_EXCEEDED`/`usage limit`/
> `connection (error|reset|closed|aborted)`/`\b500\b`/`internal server error`) together with
> `isContextOverflow` (R7.1.2.2) and `isRetryableModelFailureAttempt` (R7.1.2.3),
> all 16 fallback cases `MATCH` (target track 43/43), with the report at
> `fixtures/generated/subagents-parity-v066/parity-report.md` (`RESULT: MATCH`).
> If TE15–TE18 produce new differences later, attribute them by appending to the list under the original rules.

## Composition

| File | Responsibility |
|------|------|
| `fixtures.json` | Shared baseline cases: 9 groups of argv/env inputs, 6 groups of frontmatter content, 5 groups of message arrays |
| `fixtures-target.json` | Target-track additions: frontmatter (inherit/false, excludeTools, broken frontmatter, thinking), final-output, fallback vectors (post-#2270/TE39: context-overflow only), discovery tree (TE15), notify (TE17), **inline argv [RPI-OWN] goldens (TE18: the excludeTools surface, no upstream recorder; expectations inline in the cases) and model-resolution vectors (TE18 R7.1.4.4/.5, re-anchored by TE39 at v0.70 `model-resolution.ts`)** |
| `args-golden-v048.json` | The frozen argv/env golden file ([RPI-OWN]; covers only the 9 cases of fixtures.json; `--record-args-golden` re-records only the non-inline cases) |
| `expected-target-diffs.json` | The regression-track difference attribution list (historical, emptied by TE14) |
| `expected-target-diffs-v070.json` | The v0.1.5 target-track attribution manifest (TE37 seed, empty by design; TE38/TE39 append) |
| `upstream-runner.mjs` | Runs upstream modules directly via tsx from the track root (live submodule = regression; v0.70 snapshot = target); args via the golden file on both tracks; fallback/model re-anchored at v0.70 `model-resolution.ts` (TE39 followed the #2270 removal) |
| `setup-target-source.sh` | Extracts the v0.70 snapshot out-of-repo + installs its prod dependencies (zero writes to external/) |
| `examples/subagents_parity_runner.rs` | Drives this crate with the same fixtures (parity facade, `lib.rs::parity`); built by the orchestrator and executed from a private copy |
| `run-parity.mjs` | Orchestration + normalized diff + attribution + report writing; fixture materialization and the Rust binary copy land in out-of-repo temp directories |

`PI_CODING_AGENT_PACKAGE_ROOT=/tmp` short-circuits upstream `resolvePiPackageRoot`'s
`import.meta.resolve` (the function throws when the package isn't installed; upstream prioritizes the env).

## Normalization allowlist (exemptions and rationale)

1. **Session-path concretization**: the fixture's `/sess/root` is rewritten by the orchestrator to a shared
   temp directory (compared as-is with the same value on both sides; `--session-dir`/`--session` values byte-identical);
   at comparison time `${SESSION_BASE}/sess/root` → `<SESSION_BASE>`, letting the
   frozen golden files recorded across runs compare directly.
2. **Temp directory names**: mkdtemp prefixes `pi-subagent-*` / `rpi-subagent-*`
   (the ADR-0001 rename) → `<TMPDIR>`.
3. **`--extension` values**: upstream injects its own source files (prompt-runtime.ts /
   fanout-child.ts / the permission system), rpi injects this plugin's cdylib (one library serving
   both prompt-runtime and fanout-child roles, TE-D17) → all normalized to `<EXT>`;
   consecutive `<EXT> --extension <EXT>` runs collapse into one entry (a known difference:
   upstream's two source files vs rpi's single cdylib).
4. **env key order**: JS insertion order vs Rust BTreeMap order → both sides compared sorted by key.
5. **Upstream-exclusive env keys dropped**: `PI_SUBAGENT_RUNTIME_ACKNOWLEDGED_EXTENSIONS`
   (the runtime-ack extension receipt, P1), `PI_CODING_AGENT_PACKAGE_ROOT` /
   `PI_SUBAGENTS_PI_CODING_AGENT_PACKAGE_ROOT` (node package-root propagation; rpi has no
   counterpart — both historical names dropped). All other keys align through the
   `PI_SUBAGENT_*` → `RPI_SUBAGENT_*` rename.
6. **rpi-exclusive env keys dropped (added in TE05; TE18 addendum)**: `RPI_SUBAGENT_STEER_INBOX` and
   `RPI_SUBAGENT_SUPERVISOR_CHANNEL_DIR` — rpi-native slots for the steer inbox and the
   supervisor channel directory (FR-P1-04/10); `RPI_NO_GLOBAL_CONTEXT` (TE18 /
   ADR-0026 — the env-switch form on the rpi side of upstream #1560's in-process
   `inheritGlobalContext:false` default; neither pin has an argv/env counterpart; its presence is
   pinned by crate unit tests + an e2e env dump, not by this diff). The above keys are excluded from the diff.
7. **Prompt temp-file contents not compared**: rpi additionally prepends a boundary-instruction block at the
   file head (after `<active_agent>`, before the body — the TE-D17 mechanism-equivalent replacement);
   path-and-flag equality at the argv/env layer suffices.
8. **Brand-mapped strings (ADR-0028)**: the debrand pass renamed model-visible strings on the rpi side
   (e.g. `"the active Pi model registry"` → `"the active rpi model registry"` in launch/model.rs); the
   upstream leg emits the upstream spelling, so `normalizeOutput` maps the pair onto each other before
   diffing (error fields; extend `BRAND_MAP` as more debranded surfaces enter the fixtures). Surfaced by
   the TE37 rotation re-run — the drift existed since the debrand commit but the pre-rotation baseline
   report predated it.

## v0.70 skeleton facts (TE37 close reads, ADR-0029)

- `src/runs/shared/model-fallback.ts` **deleted** (#2270 / f58dfcb5, "remove automatic model
  fallback") — no replacement module; `isRetryableModelFailure` has no src occurrence at v0.70.
  **TE39 ruling (2026-09-27, user-approved): follow the removal** — the rpi fallback surface
  was deleted with it, the retryable/attempt fixtures retired, and the fallback/model legs
  now drive v0.70 `model-resolution.ts` (`isContextOverflow`, `resolveSubagentModelOverride`,
  and `resolveModelSelection` for the surviving single-candidate vectors).
- `src/runs/shared/pi-args.ts` stays absent (deleted v0.65) — the argv/env face remains the frozen
  v0.48 golden ([RPI-OWN], ADR-0025 §4).
- Unchanged faces at v0.70 (verified): `src/agents/frontmatter.ts` (`parseFrontmatter`),
  `src/shared/utils.ts` (`getFinalOutput`), `src/agents/agents.ts` (`discoverAgents`),
  `src/runs/background/notify.ts` (`formatSingleCompletion` / `parseSubagentNotifyContent`).

## v0.66 shared-surface changes (target-track close reads, ADR-0025 appendix D)

- `src/runs/shared/pi-args.ts` **deleted** → argv/env moved to [RPI-OWN] (previous section);
- `src/agents/frontmatter.ts` is **byte-identical** v0.48→v0.66 (frontmatter cases share one shape across tracks);
- `src/shared/utils.ts`: `getFinalOutput` gained `stripPiTurnTimingFooter` (#1792; rpi has no such output,
  [N/A], hence no footer cases); `hasEmptyTerminalAssistantResponse` extended with the "empty-text terminal state"
  semantics (R7.1.1.2, TE14);
- `src/runs/shared/model-fallback.ts`: new `REQUEST_LIMIT_EXCEEDED`/`usage limit`/
  `connection (error|reset|closed|aborted)`/`500`/`internal server error` patterns plus
  `isRetryableModelFailureAttempt`/`isContextOverflow`/`recordRetryableModelFailure`
  (R7.1.2.1–.3, TE14); `isRetryableModelFailure` and `formatModelAttemptNote` semantics unchanged.
- `src/agents/agents.ts` discovery surface: `DISCOVERY_PRUNED_DIR_NAMES` gained `.pi`/`sync-backups`
  (#1596/671bc27c); symlinked directories followed as directories + a `visitedDirectories` realpath set
  (#1505/#1510/9433419a); single-file try/catch → `AgentDiscoveryDiagnostic`
  (#1200/e973fa3c). rpi substitutes `.rpi` for `.pi` (ADR-0001); everything else is parity-checked with
  the same semantics (TE15).

## Environment isolation (read before running)

`run-parity.mjs` starts both legs' subprocesses with a **cleaned environment**: every environment key
prefixed `PI_SUBAGENT*` / `RPI_SUBAGENT*` is deleted (`cleanSessionEnv`; the harness's own
`RPI_SUBAGENTS_PARITY_TRACK` is added after cleaning). Reasons:

- the upstream leg's `pi-args.ts` falls back to reading `PI_*` values from the shell, while the Rust leg reads
  `RPI_*` (the bridge renames `PI_SUBAGENT_*` to `RPI_SUBAGENT_*`) — if any of those keys were exported
  in the outer shell, its value would reach only one leg and the args mode would show false MISMATCHes
  (every case except `fork-session-file` goes through the environment fallback). Harnesses before
  2026-08-15 lacked this isolation; to reproduce the old behavior, export a key manually and watch
  the false positives.
- **pi subagent sessions** forward the parent environment to sub agents with a `PI_SUBAGENTS_` prefix
  (seen in the wild: `PI_SUBAGENTS_PI_CODING_AGENT_PACKAGE_ROOT`, the package-root key copied into the
  child's env by `pi-args.ts:641`), so cleaning only `PI_SUBAGENT_PARENT_SESSION` is not enough — running
  the parity inside a pi subagent session used to produce 8 args false MISMATCHes. After the
  2026-09-09 fix all prefix keys are cleaned and both environments (plain shell / pi subagent)
  report `MATCH`.

## Known non-applicable surfaces

- Full tool descriptions: the entry point changed from workflowScript to structured parameters (ADR-0016),
  so the copy necessarily differs; the custom-template mechanism and SAFETY-section structure are covered by
  crate unit tests.
- Session entry filtering: upstream filters inside the subprocess context event, rpi filters on the fork branch's
  file (design §3.4) — equivalent results at different layers (e2e scenario 3 covers it).
- The turn-timing footer (#1792): rpi has no such output; no parity cases per [N/A]
  (03 appendix C.3).
