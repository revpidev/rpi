---
name: pi-subagents
description: |
  Delegate work to builtin or custom subagents with single-agent, parallel,
  chained, async, forked-context, and coordinated workflows. Use
  for advisory review, implementation handoffs, and multi-step tasks where a
  single agent should stay in control while other agents contribute context,
  planning, or execution.
---

# Pi Subagents

This skill is for the main parent orchestrator only. Do not inject or follow it inside spawned child subagents. The parent session owns delegation, orchestration, review fanout, and final fix-worker launches. Ordinary children should not run their own subagent workflows; the explicit exception is a delegated fanout child whose resolved builtin `tools` includes `subagent`, and that child may use `subagent` only for the fanout work the parent assigned.

Use this skill when the parent orchestrator needs one specialized child or composed orchestration. Use the structured parameters of the `subagent` tool for all execution, including one isolated child: `{ agent, task }` for one child, `tasks: [{ key, agent, task, ... }]` for a parallel wave (bounded by `concurrency`, failures isolated, results aggregated in key order), and `steps: [...]` for a sequential chain whose task templates may interpolate `{ previous }` (prior step output), `{ outputs.<name> }` (a bound step output), and `{ task }` (the original task). Compositions start asynchronously by default; pass `async: false` only for a small foreground run. Adaptive control flow — branching on results, retries, building the next wave from findings — stays in the parent session: read the returned results, then issue the next call.

## How to use this router

Read the matching reference file before acting. Paths are relative to this `SKILL.md`; resolve them against `skills/pi-subagents/` and load them with the read tool.

| Task | Read |
| --- | --- |
| Decide whether to delegate, choose agents, compare tool versus slash commands, apply prompt techniques, or understand builtin roles | `references/prompting-and-roles.md` |
| Run one-child, parallel, chained, async, forked, gate-checked, or supervisor-coordinated workflows | `references/execution-controls.md` |
| List agents, manage refinement overlays, edit agent definition files, or use prompt-template integration | `references/management-authoring-rpc.md` |
| Check safety constraints, best practices, standard workflows, or error handling | `references/constraints-and-recipes.md` |

For broad or uncertain requests, read more than one reference. For complex work, start with `references/prompting-and-roles.md` and `references/execution-controls.md`, then consult `references/constraints-and-recipes.md` before launching or reviewing child work.

## Always-on constraints

- Keep the parent as orchestrator and final decision-maker.
- Use one writer per cwd/worktree unless isolated worktrees are intentional.
- For cross-codebase work, record the target repo, explicit `cwd`, authority boundary, and expected output before launch. Do not assume the parent session cwd is the child repo.
- For parallel fanout, compare child prompts before launch. Do not send clone prompts with only issue numbers, titles, or broad file globs swapped; each child needs a lane-specific task, source seam, prior evidence, and decision that remains distinct without the item number. Launch that fanout as one async `tasks` composition with stable keys and aggregate output unless there is truly only one child.
- Prefer fresh-context review/validation fanout, then synthesize and apply fixes in the parent.
- Use async/background by default when work can proceed independently; do not poll just to wait. Adaptive gates branch in the parent session: read each wave's results, then launch the next call only when the gate passes.
- Preserve capability ceilings, including child tool restrictions and session-scoped allowed-agent restrictions.
- Escalate unresolved product, architecture, authority, release, merge, or safety decisions upward instead of letting a child decide silently.
- Treat receipts, CI, review bots, and external-run records as evidence, not authority to merge, close, comment, publish, or release.
- As a conservative orchestration policy, do not pass `turnBudget`, a hard `toolBudget`, or a tight `usageBudget` to mutation-capable workers. The default tool budget blocks read/search tools rather than mutation tools, and reported usage has no reservation model. If a worker is interrupted after a tool call starts, checkpoint after the current tool returns with changed files, build/test state, and commit or PR state.
