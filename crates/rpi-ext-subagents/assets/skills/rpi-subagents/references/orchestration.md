# Rpi Subagents: Orchestration Reference

On-demand reference loaded from `skills/rpi-subagents/SKILL.md`.

## Launch surfaces

Use the structured `subagent` parameters only; saved chain files and scripted
workflow definitions are unsupported surfaces.

- Single child: `subagent({ agent, task })`.
- `context: "fork"` branches the persisted parent session and keeps its history
  (not a filtered review context); `"fresh"` is isolated; `"profile"` uses the
  agent's `defaultContext`.
- Parallel wave: `tasks: [{ key, agent, task, ... }]`, bounded by `concurrency`
  (default 4), failures isolated, results in key order.
- Chain: `steps: [{ agent, task, as?, ... }]` interpolating `{ task }`,
  `{ previous }`, and `{outputs.<name>}` (a step bound with `as`); a failed step
  stops the chain.
- Compatibility workflow: `workflow: "parallel"` with
  `args.tasks: [{ agent, task, cwd?, model? }]`. It normalizes to native `tasks`
  and cannot combine with `agent`, `task`, `tasks`, `steps`, or `action`.
- `gate: "<command>"` runs one host-side verification after the child; failure
  fails the run. Child-reported success is evidence, not verification.

Items also accept `async`, `model`, `thinking`, `context`, `cwd`, `output`,
`worktree`, `timeoutMs`, `skill`; top-level fields are child defaults and
compositions default to async. Keys are optional (the scheduler generates
`task-N` when omitted), but a stable key helps aggregation; each child still
needs a lane-distinct task, output path, and decision.

## Async defaults and waiting

Prefer async for every launch — scouts, researchers, workers, reviewers,
validators, oracle checks, compositions — unless blocking is required. Async is not
parallel writing: one writer per worktree, with parent-side overlap for reading,
validation prep, and synthesis.

Do not end a turn right after launching work you promised to finish: in
interactive chat, normally return control and let rpi wake the session rather than
calling `subagent_wait()` just to wait; override for run-to-completion requests.
Headless sessions auto-drain at `agent_end`.

- `subagent_wait()` — next active async run finishes, or a subagent needs
  attention.
- `{ all: true }` — everything active at call time.
- `{ timeoutMs }` — cap the block; work keeps running.
- `{ id }` — one async or remembered detached foreground run (id or prefix).
- `{ id, nonBlocking: true }` — resolve one run, arm a subscription, return, then
  wake on completion/failure/attention/timeout; disabling `waitTool` returns
  without arming a wake.

Children do not get `subagent_wait` automatically; keep it in a child's `tools`
allowlist when that child must wait on its own background work. Inspect with
`action: "status"`, or `{ action: "status", id, view: "transcript" }` for a child
transcript; nested runs are targetable by nested id.

## Control: steer, interrupt, stop, resume

Lifecycle (`queued`, `running`, `paused`, `complete`, `stopped`, `failed`,
`rejected`) differs from attention: `needs_attention` means no activity past the
threshold, `paused` means interrupted or awaiting direction, not failed. Silence
during long tools is normal; intervene on clear signals.

- `stop` — cancel a top-level async run, or one child via `childId`
  (`step:<index>` or child run id).
- `interrupt` (requires `id`) — request a soft interrupt of an active run: the
  current child turn is cancelled and the run stops at that point, so follow with
  `resume` with clearer instructions, or `stop`.
- `steer` — deliver a message to a live run's steer inbox, optionally scoped with
  `targetIndex`/`childId`; terminal runs and single-child runs with a pending
  supervisor ask refuse it. Returns the request id and target.
- `resume` — revive an interrupted run as a new async child from its persisted
  session, with optional `task` (defaults to continuing). Running/queued runs and
  stopped children refuse.
- If a foreground child detaches for supervisor coordination, reply first, then
  wait on its id rather than replacing it.

## Worktree isolation

Use worktrees whenever multiple children might write: `worktree: true` per task,
or top-level as the default (a child opts out with `false`). Each child branches
from `baseRef` (default `HEAD`, named refs only) and needs a clean git state. Take
handoffs from `artifactPaths`, not scraped text: the manifest records status,
output references, patch paths, stats, and worktree/branch removal. For one writer
plus advisors, prefer the single-writer pattern. Worktrees start from tracked
files, so run `worktreeSetupHook` (or an explicit bootstrap) before tests.

## The oracle workflow

Parent forks to `oracle` → oracle reviews direction, drift, assumptions, and risks
→ it may coordinate back through `contact_supervisor` → the parent decides → only
then does `worker` implement.

`oracle` is a forked advisory thread inheriting parent history as its baseline
contract, not a fresh-context reviewer. Use it for trajectory problems:
architecture boundaries, capability routing, merge conflicts, reviewer
disagreement, context drift, or fixes needing product/scope tradeoffs. Ask broad
questions when the concern is unclear. Keep it advisory unless given the writer
role.

## Supervisor and intercom coordination

Native coordination needs no external transport: a child calls
`contact_supervisor` to reach exactly the parent session that spawned it (scoped by
parent session id); the parent inspects or replies with `subagent_supervisor`.

- `need_decision` — blocked, needs clarification instead of guessing, or approval
  of a product, API, scope, or authority choice. Waits; stay alive until reply.
- `interview_request` — structured supervisor input; also waits.
- `progress_update` — non-blocking: a plan-changing discovery or a checkpoint.

Routine completion handoffs are not expected; return findings via the normal
response or output artifact. A review-only child must not edit project files but
may still report findings.

Generic `intercom` is a fallback only when bridge instructions provide a target and
`contact_supervisor` is unavailable; never invent a target. `intercomBridge` mode
(`always`, `fork-only`, `off`) decides whether the bridge injects the child-facing
tool and detaches the child for coordination.

## Agent management

`list` (with `capabilities: true` for prompt-free selection), `get` with `agent`
(full definition), `status` (runs), `doctor` (paths, discovery, async, bridge), and
`grant-spawn-budget` with `additional`. Discovery: `~/.rpi/agent/agents/**/*.md`
(user), `.rpi/agents/**/*.md` (project), legacy `.agents/**/*.md` (compat;
`.rpi/agents/` wins). Precedence by resolved runtime name: project, user, builtin.
Set `subagents.projectRootResolution: "git-root"` when a nested `.rpi` must not
shadow repo config (the nested project can opt back with `"nearest"`).

`grant-spawn-budget` is root-interactive-parent only: it needs an interactive
session, asks for confirmation, and is bounded by the original cap. Settings such
as `authorityPolicy` and `permissions` are only shape-validated, not a source of
tool authority.

## Authoring agents by file

```markdown
---
name: my-agent
description: What this agent does
model: provider/model-id
tools: read, grep, find, ls, bash
inheritProjectContext: true
---
Your system prompt here.
```

Optional frontmatter includes `package`, `aliases`, `thinking`, `output`,
`defaultReads`, `defaultProgress`, `subagentOnlyExtensions`, `skills`/`skill`,
`inheritSkills`, `memory` (opt-in `agent-memory/`), `maxSubagentDepth`,
`acceptance`, `acceptanceRole` (`read-only`/`writer`; inference only), `async`,
and `timeoutMs` (alias `maxRuntimeMs`; foreground default 30 minutes).

- `tools` is a strict allowlist, not an extension loader: name the extension tool
  in `tools` and load its provider through discovery, `extensions`, a path-like
  entry, or `subagentOnlyExtensions`; a missing tool fails the child.
- `skills`/`skill` selects packaged skills and `inheritSkills` controls whether
  the parent's skills carry over.
- Prefer `subagents.agentOverrides` for small builtin changes. Packaged `worker`,
  `oracle`, and `advisor` default to forked context.

## Refinement overlays

`refine` (with required `guidance`) records a bounded project-local overlay for one
agent at `.rpi/subagents/refinements/<agent>.md` and injects it into that agent's
child prompt for this project; the base definition is unchanged, and a trusted
project is required. Guidance containing code fences, global rewrites, or
safety/tool/acceptance/settings overrides is rejected. `refine.show` prints the
overlay and revision history; `refine.rollback` restores the previous revision.

## Prompt templates

`/parallel-review`, `/review-loop`, `/parallel-research`,
`/gather-context-and-clarify`, `/parallel-cleanup`; `autofix` variants synthesize
feedback and apply only fixes worth doing now. Parents can run the same recipes
directly with `subagent(...)`. Custom templates delegate via `/prompt-workflow`
(frontmatter: `subagent`, `model`, `skill`, `cwd`, `fresh`, `fork`, `chain`).
`gate` covers one host-run acceptance command; review/read-only calls omit it.

## Recipes

Recon → implement, binding the scout output:

```jsonc
subagent({ steps: [
  { agent: "scout", task: "Map the target and name the implementation seam", as: "recon" },
  { agent: "worker", task: "Implement from: {outputs.recon}" }
] })
```

Implementation → review → fixes: launch one async `worker`, treat its handoff as
intermediate, fan out fresh-context reviewers on distinct angles (correctness,
tests, simplicity; add security, performance, docs, or user-flow for risky work),
synthesize against approved scope, then send exactly one fix `worker`. Never apply
unapproved scope, product, or architecture choices — escalate. Repeat until no
fixes worth doing now remain, an unapproved decision appears, or the round cap
(3 default) is reached, then validate and inspect the final diff yourself.
Parallel analysis uses `scout` lanes with distinct keys and output paths; never
parallel writers in one checkout. For large work use serial milestones, each with
one writer, a validation contract, review, a fix pass, and parent acceptance.

## Constraints and configuration

- Forking needs a persisted parent session; otherwise use `context: "fresh"`.
  Forked children inherit history, so adversarial reviewers should be fresh unless
  forked context was requested.
- Default nesting depth is 2; deeper delegation is blocked unless configured.
  Children get neither the `subagent` tool nor this skill unless a fanout agent's
  `tools` includes it. Implementation children must call real edit tools, not
  print pseudo tool calls.
- Keep authority clear: advisory children are not decision-makers, and one session
  holds at most one pending outbound ask.
- Knobs: `asyncByDefault`, `forceTopLevelAsync`, `waitTool`,
  `maxSubagentSpawnsPerSession` (`0`/unset unlimited; compaction does not reset
  usage), `worktreeSetupHook`, `artifactDir` (`project`/`session`/`temp`),
  `parallel.concurrency`.
- Status and result payloads expose per-step state, usage (`totalCost`, token
  counts), and a child tree; `protocol_output_limit` means a child JSONL line
  exceeded the 16 MiB parser cap.

## Error handling

| Symptom | Response |
| --- | --- |
| Unknown agent | `action: "list"`, check scope/precedence |
| Setup or bridge confusion | `action: "doctor"` or `/subagents-doctor` |
| Max depth exceeded | Flatten the workflow or raise `maxSubagentDepth` |
| No session file for fork | Persist the current session first |

## Extension RPC

There is no extension-RPC surface: other extensions cannot call `rpi-subagents`
through an in-process bus. Cross-session coordination uses the supervisor tools
above plus lifecycle artifacts.
