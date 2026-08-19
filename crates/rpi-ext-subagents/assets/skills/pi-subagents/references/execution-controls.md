# Pi Subagents: Execution Controls

This file is a detailed reference loaded from `skills/pi-subagents/SKILL.md`.

## Discovery and Scope Rules

Agent files can live in:
- `~/.rpi/agent/agents/**/*.md` — user scope
- `.rpi/agents/**/*.md` — canonical project scope
- legacy `.agents/**/*.md` — still read for compatibility, but `.rpi/agents/` wins on conflicts

Saved chain files are not a public execution surface. Author new orchestration with the structured `tasks`/`steps` parameters.

Precedence is by parsed runtime name:
1. project scope
2. user scope
3. builtin agents

Project settings resolve from the nearest parent directory containing `.rpi` or `.agents` by default. In monorepos or git worktrees where an incidental nested `.rpi` directory should not shadow the repository config, set `subagents.projectRootResolution: "git-root"` in the repository root `.rpi/settings.json`; a nested project can opt back with `"nearest"` in its own settings.

## Running Subagents

### Single agent

```jsonc
subagent({
  agent: "oracle",
  task: "Review my current direction and challenge assumptions."
})
```

### Forked context

```jsonc
subagent({
  agent: "oracle",
  task: "Review my current direction and challenge assumptions.",
  context: "fork"
})
```

`context: "fork"` creates a branched child session from the current persisted
parent session. It does **not** create a fresh minimal review context or filter
history down to only the relevant parts. Use it when you want a separate review
or execution thread that can still reference the parent session history.

Foreground results and async status label each child with
its resolved launch context as `[fresh]` or `[fork]`. Aggregate headers show
`[mixed]` when a run uses both modes.

### Composed workflows

The structured parameters are the sole public execution surface. Use `{ agent, task }` for one child, `tasks: [...]` (each item `{ key, agent, task, ... }`) for parallel children, and `steps: [...]` for a sequential chain whose task templates may interpolate `{ previous }` (prior step output), `{ outputs.<name> }` (a step bound with `as`), and `{ task }` (the original task). Prefer a single composed call whenever the parent is starting a coordinated wave, such as multiple reviews, cross-repo prep lanes, or a fanout that the parent will consume together.

```jsonc
// 1) Map the target first; read the scout output when it completes.
subagent({ agent: "scout", task: "Map the target", async: true })

// 2) Then fan out reviewers that build on the scan in the task text.
subagent({
  tasks: [
    { key: "correctness", agent: "reviewer", task: "Review correctness. Scan context: <paste or summarize the scout output>" },
    { key: "tests", agent: "reviewer", task: "Review tests. Scan context: <paste or summarize the scout output>" }
  ]
})
```

For a dependent sequence, bind each step's output with `as` and reference it in later task templates:

```jsonc
subagent({
  steps: [
    { agent: "scout", task: "Map the target", as: "scan" },
    { agent: "worker", task: "Implement using the scan: {outputs.scan}" }
  ]
})
```

Stable keys are required. Child launches follow ordinary single-agent execution controls. Give each child a distinct decision and output path when reports must outlive the workflow, then consume the aggregate workflow result before opening individual reports.

For one host-run verification command, pass `gate: "npm test"` on a `tasks` entry (or at the top level as a composition default). The runtime executes it on the host after the child finishes; a failing gate fails that child's run.

Interrupted or stopped background runs can be revived: `subagent({ action: "resume", id: "<run-id>", task: "Follow-up task text" })` starts a new async child from the persisted child session; `task` is optional and defaults to continuing the interrupted task. The revived child keeps its stored agent and session context.

### Async/background

Prefer async mode for every subagent launch. Set `async: true` no matter the task unless there is a specific reason to opt into a foreground/blocking run. This applies to scouts, researchers, workers, reviewers, validators, oracle checks, one-off delegates, and composed workflows. Keep the write path single-threaded even when the run is async.

Async does not mean parallel writes. Do not edit the same active worktree while an async worker is changing it. Parent-side overlap should be reading, validation prep, synthesis, command planning, or review of unaffected context unless the writer is isolated in a separate worktree.

Do not end your turn immediately after launching an async child if you promised to keep working. Continue the local inspection, synthesis, or validation prep, then check the async run when its result is needed.

In an interactive chat, normally return control when ready to yield and let Pi wake the session on completion; do not call `subagent_wait()` merely to wait. Override that default and call it when the current request is run-to-completion — for example, the user asked you to report results back before continuing or a skill cannot return before its background work finishes. Headless sessions auto-drain exact current-session work at `agent_end`; call `subagent_wait()` when this turn must receive results before it ends. Never substitute sleep or status-polling loops.

`subagent_wait()` returns when the next initially active async run or registered provider item finishes or a subagent needs attention. Use `subagent_wait({ all: true })` for all work active at call time, `subagent_wait({ id: "..." })` for one async or remembered detached foreground run, and `subagent_wait({ timeoutMs })` to cap the block. In a long-lived interactive parent session, use `subagent_wait({ id: "...", nonBlocking: true })` to resolve the prefix to one exact run, persist an armed subscription, return immediately, and wake later on completion, failure, attention, reconciliation failure, or timeout. Ordinary status lists armed subscriptions separately from active children. This differs from disabling `waitTool`, which returns immediately without arming a future wake. If a foreground child detaches for supervisor coordination, reply first, then wait on its id; do not resume or launch a replacement while it remains detached. Headless sessions also auto-drain exact current-session work at `agent_end` as a final safeguard.

```jsonc
subagent({
  agent: "worker",
  task: "Run the full test suite",
  async: true
})
```

File-only output mode works for composed child launches. Use distinct absolute or durable output paths when later steps need stable references. For cross-codebase waves, include the repo slug or lane key in each output path so reports from different repositories cannot collide.

For review fanout where the parent continues a local audit:

```jsonc
subagent({
  agent: "reviewer",
  task: "Review the current diff for correctness issues. Do not edit files.",
  async: true,
  context: "fresh"
})
// Continue local inspection, then later call status with the returned id.
```

Inspect async runs with `subagent({ action: "status", id: "..." })` or `subagent({ action: "status" })` for active runs. If a delegated fanout child launches nested runs, the parent status view shows them as a tree and you can target a nested run directly with its nested id.

Stop a current-session top-level async run with `stop`; it requests termination and reports the settled state. Interrupted or stopped runs can be revived later with `resume` when a persisted child session exists:

```jsonc
subagent({ action: "stop", id: "run-id" })
```

Use `steer` for top-level live async guidance and `resume` to revive an interrupted or stopped run:

```jsonc
subagent({ action: "steer", id: "run-id", message: "Focus on the failing test." })
subagent({ action: "resume", id: "run-id", task: "Follow up on this point." })
```

Resume behavior:
- `resume` revives interrupted or stopped background runs from the persisted child session file; running or queued runs refuse it.
- Use `steer` for acknowledged guidance to a live top-level async child.
- Revive starts a new async child process from the old session context; it does not restart the same OS process.
- If the chosen child has no persisted session file, resume fails and reports that directly.

Use diagnostics when setup or child startup looks wrong:

```typescript
subagent({ action: "doctor" })
```

Humans can use `/subagents-doctor` for the same read-only report. It checks runtime paths, discovery counts, async support, current session context, and intercom bridge state.

### Subagent control

Subagent control is the runtime visibility and intervention layer for delegated runs. It is separate from lifecycle status. Lifecycle status says whether a child is `queued`, `running`, `paused`, `complete`, `stopped`, `failed`, or `rejected`. Activity reporting is factual: it tracks the last observed activity time and the current tool when known. It does not pretend to know that a child is truly stuck. Manual top-level async cancellation uses `stop`.

Default behavior is intentionally conservative. When no activity has been observed past the configured threshold, the run emits a `needs_attention` control event. Foreground runs can push this as a `subagent:control-event` event, and async runs persist it to `events.jsonl` so the parent tracker can surface it without constant manual polling. Notification-worthy control events are also inserted into the visible transcript so both the user and the parent agent can see them.

Use soft interrupt when a child is clearly blocked or drifting and the parent needs to regain control:

```typescript
subagent({ action: "interrupt" })
```

Pass `id` when targeting a specific controllable run, including a nested run shown in the parent status tree:

```typescript
subagent({ action: "interrupt", id: "abc123" })
subagent({ action: "interrupt", id: "nested-run-id" })
```

A soft interrupt cancels the current child turn and leaves the run paused. It does not mean the delegated task succeeded or failed. Bare `interrupt` does not target hidden nested descendants; use the explicit nested id. After an interrupt, decide the next explicit action: resume with clearer instructions, replace the task, ask the user, or stop the workflow.

If the run already has an active intercom bridge target, needs-attention notifications can also prepare a compact intercom ping for the orchestrator. When a child route is available, the ping tells the orchestrator which agent needs attention and includes the exact `intercom({ action: "send", to: "..." })` target for a nudge. Do not invent a target or ask the child to self-report when no bridge exists.

Steering is acknowledged delivery, not a send attempt or model-compliance signal:

```typescript
subagent({ action: "steer", id: "abc123", message: "Focus on the failing test." })
```

The action waits up to three seconds for the child Pi session to accept the correlated user input and returns a request id with `delivered`, `scheduled`, `pending`, `partial`, `recovered`, or `failed` plus per-child states. Indexed pending children return `scheduled` immediately. Only a top-level single-child run may automatically interrupt after a missed acknowledgment and recover after confirmed pause within a further 15 seconds. Recovery preserves the original child contract and only its remaining deadline, turn, and tool budgets. If the session is missing, a budget is exhausted, the pause cannot be confirmed, or replacement launch fails, the source remains paused when pausing succeeded and the action returns the exact failure. Chain, parallel, and nested runs never auto-interrupt; inspect their per-child outcomes and handle failures explicitly. A late acknowledgment is recorded and cannot cancel committed recovery.

## Worktree Isolation

When multiple agents might write concurrently, use worktrees instead of letting
them share one filesystem view.

```jsonc
subagent({
  tasks: [
    { key: "feature-a", agent: "worker", task: "Implement feature A", worktree: true },
    { key: "feature-b", agent: "worker", task: "Implement feature B", worktree: true }
  ]
})
```

`worktree: true` on a `tasks` entry gives that child its own git
worktree branched from HEAD. A top-level `worktree: true` makes this the
default for every child, and a child can opt out with `worktree: false`. This
requires a clean git state and is mainly for intentionally parallel write
workflows. On completion, use each child's handoff path from its
`artifactPaths` instead of scraping combined text. Each manifest records child status and output references, full
patch paths and stats, and whether each temporary worktree and branch was
removed. The manifest is journaled immediately after managed worktree setup, before children run, so abrupt exits retain owned paths and branches for recovery. Dirty or divergent work without a successfully captured patch is preserved with a partial-cleanup warning, and the retained manifest lists the preserved paths for manual Git recovery. If you want one writer thread and several advisory agents, prefer a
single-writer pattern instead.

Git worktrees start from tracked files, so ignored or untracked build state
such as `node_modules` may be absent. The clean-check ignores the extension's
own `.rpi/subagents*` runtime state but still
rejects ordinary source/config changes. The runtime attempts to symlink the
root checkout's `node_modules` into each managed worktree when it exists, but
agents should still treat dependency setup as an explicit bootstrap step before
running tests, typecheck, or builds. If module resolution fails in a fresh
worktree, first confirm dependencies were linked, installed, or provisioned by
`worktreeSetupHook` before treating it as a code failure.

## The Oracle Workflow

The intended oracle loop is:
1. the main agent forks to `oracle`
2. `oracle` reviews direction, drift, assumptions, and risks
3. `oracle` can coordinate back through `contact_supervisor` when the bridge injects it
4. the main agent decides what direction to approve
5. only then should `worker` implement

```jsonc
// Advisory review in a branched thread. Oracle defaults to forked context.
subagent({
  agent: "oracle",
  task: "Review my current direction, challenge assumptions, and propose the best next move."
})

// Implementation only after explicit approval. Worker defaults to forked context.
subagent({
  agent: "worker",
  task: "Implement the approved approach: ..."
})
```

`oracle` is not a fresh-context reviewer in the Cognition article sense. It is
a forked advisory thread that inherits the parent session history and uses that
history as a baseline contract.

Use `oracle` as a smart-friend escalation when the parent needs help with trajectory rather than diff inspection: architectural boundaries, model capability routing, merge conflicts, reviewer disagreement, context drift after long work, a worker about to invent a pattern, or fixes that require product/scope tradeoffs. Ask broad questions when the right concern is unclear, and let `oracle` point out missing context or files the parent should inspect before asking again. Keep `oracle` advisory unless it has been explicitly assigned the single writer role.

## Subagent + Intercom Coordination

`pi-subagents` includes native supervisor coordination. Child agents can use `contact_supervisor` to ask the exact parent session that spawned them; messages are scoped by parent session id and should not appear in other Pi sessions. Parents inspect or reply with `subagent_supervisor`. This path does not require `pi-intercom`.

This is separate from optional external completion delivery. Set `intercomBridge.resultDelivery: true` only when an external listener consumes and acknowledges `subagent:result-intercom` grouped results. It does not deliver results by itself, and it does not change native supervisor asks or progress updates.

Most agents should not call generic `intercom` directly unless bridge instructions provide a target and `contact_supervisor` is unavailable. Do not invent a target. Prefer the tool from the injected bridge instructions.

Use `contact_supervisor` with `reason: "need_decision"` when:
- a subagent is blocked on a decision
- a child needs clarification instead of guessing
- an approval, product, API, or scope choice is required before continuing safely

Use `contact_supervisor` with `reason: "interview_request"` when the child needs structured supervisor input rather than a freeform answer. The request waits for a parent reply, so the child should stay alive and continue only after the reply arrives.

Do not use `contact_supervisor` just to resolve review-only/no-project-edit versus progress-writing or output-artifact instructions. The child must not modify project/source files, but returning findings through its normal response or configured output artifact is allowed unless the parent explicitly set `output: false`.

Use `contact_supervisor` with `reason: "progress_update"` when:
- a child is explicitly asked for progress
- a meaningful discovery changes the plan
- a long-running child needs to report a blocked/progress checkpoint without waiting for normal tool return flow

Message conventions:
- `reason: "need_decision"` and `reason: "interview_request"` wait for the parent reply and return it to the child.
- `reason: "progress_update"` is non-blocking and should stay concise.
- Child-side routine completion handoffs are not expected. Native supervisor messages are for decisions, structured input, and meaningful progress updates while a child is still running.

If bridge instructions provide the child-facing tool, a child can ask:

```typescript
contact_supervisor({
  reason: "need_decision",
  message: "Should I optimize for readability or performance here?"
})
```

The parent replies with the native supervisor tool:

```typescript
subagent_supervisor({ action: "reply", message: "Optimize for readability." })
```

Or inspects unresolved asks first:

```typescript
subagent_supervisor({ action: "pending" })
```

If no external `pi-intercom` tool owns the `intercom` name, native supervisor coordination may also expose `intercom` as a compatibility fallback. Prefer `subagent_supervisor` for parent replies because it never overrides installed `pi-intercom`.

If intercom messages do not show up, run `subagent({ action: "doctor" })` or `/subagents-doctor`.
