# Pi Subagents: Management Authoring Rpc

This file is a detailed reference loaded from `skills/pi-subagents/SKILL.md`.

## Management Mode

The `subagent(...)` tool also supports management actions: `list`, `get`, `status`, `interrupt`, `stop`, `steer`, `resume`, `refine`, `refine.show`, `refine.rollback`, `grant-spawn-budget`, and `doctor`. Control actions for running subagents are covered in `references/execution-controls.md`.

### List available agents

```typescript
subagent({ action: "list" })
```

### Refinement overlays

```typescript
subagent({ action: "refine", agent: "reviewer" })
subagent({ action: "refine.show", agent: "reviewer" })
subagent({ action: "refine.rollback", agent: "reviewer" })
```

`refine` builds a bounded project-local guidance overlay for one agent from recent run evidence, using a fresh read-only proposal child; validated guidance is stored under `.rpi/subagents/refinements/<agent>.md` with revision snapshots and is injected into that agent's child system prompt for this project. `refine.show` prints the current overlay and history; `refine.rollback` restores the previous revision. Guidance that tries to override safety, policy, tool, output, acceptance, developer, or system instructions is rejected.

Authoring new agents or changing existing ones is file-based: create or edit an agent definition file (below). For small builtin changes such as a model swap, prefer `subagents.agentOverrides` in settings.

## Creating and Editing Agents by File

A minimal agent file looks like this:

```markdown
---
name: my-agent
package: code-analysis
description: What this agent does
aliases: developer, coder
model: openai-codex/gpt-5.4
thinking: high
tools: read, grep, find, ls, bash
systemPromptMode: replace
inheritProjectContext: true
inheritSkills: false
skills: safe-bash, review-checklist
skillPath: ./skills, ../shared-skills
---

Your system prompt here.
```

That is only a starting point. Omit `package` for the traditional unqualified runtime name. Common optional fields include:
- `defaultProgress`
- `defaultReads`
- `output`
- `aliases`
- `fallbackModels`
- `subagentOnlyExtensions`
- `skills`
- `skillPath`
- `memory`
- `maxSubagentDepth`
- `acceptance`
- `acceptanceRole`
- `async` — single-agent default for background launch (`true`/`false`); explicit tool-call `async` wins
- `timeoutMs` — single-agent default run-level max runtime in ms; foreground calls use a 30-minute package default only when neither the call nor agent provides one (tool alias `maxRuntimeMs` is also accepted)
- `turnBudget` — single-agent default `{ maxTurns, graceTurns? }` JSON object

`aliases` is an optional comma-separated or block-list set of alternate names for selecting an agent. Aliases resolve to the canonical `name` for execution, status, persistence, and config. Exact canonical names take precedence over aliases, and alias collisions between distinct canonical agents fail as ambiguous. Management create/update accepts a comma-separated string, string array, or `false`/empty string to clear aliases.

`acceptance` is a single-agent launch default. Use a scalar level such as `checked` or an inline/block YAML map such as `{ level: "none", reason: "lightweight lookup" }`. An explicit tool-call value wins; chain and parallel acceptance remains configured on the task or step. Management create/update accepts the same policy object, and `acceptance: ""` clears the frontmatter default (`false` remains the deprecated disabled-policy shorthand).

`acceptanceRole` is `read-only` or `writer` and controls automatic acceptance inference only. Explicit task mutation or no-edit intent wins; otherwise the role replaces agent-name guessing. Omission preserves the current name heuristics. The field does not grant or revoke tools. Management accepts `false` or an empty string to clear it.

`tools` is a strict child allowlist, not an extension loader. For a named extension tool, keep its registered name in `tools` and load its provider through normal Pi discovery, `extensions`, a path-like `tools` entry, or `subagentOnlyExtensions`. For example, pair `tools: read, fixture_search` with `subagentOnlyExtensions: ./tools/fixture-search.ts` when the provider should exist only in that agent's child sessions. The child now fails with the unavailable names and provider-loading guidance instead of silently continuing when a requested tool is absent; internal `structured_output` is allowed automatically when an output schema requires it.

`skillPath` adds invocation-private skill files or discovery directories relative to the agent file; it does not select them, so list the desired names under `skills`. Local matches win, unresolved or unreadable matches use normal discovery, and local candidates never enter the parent/global catalog. Use `memory: { scope: "project" | "user", path: "<name>" }` for opt-in role-specific durable memory under the dedicated `agent-memory/` namespace; it is separate from parent/session project memory.

For many customizations, builtin overrides in settings are lower-friction than
copying a full builtin file.

## Prompt Template Integration

The package includes prompt shortcuts for common workflows: `/parallel-review`,
`/review-loop`, `/parallel-research`, `/gather-context-and-clarify`, and
`/parallel-cleanup`. Use them when the user wants repeatable review,
review/fix loops, research, context handoff, implementation handoff,
clarification, or cleanup-review patterns. `/parallel-review autofix` and
`/parallel-cleanup autofix` synthesize reviewer feedback and then apply only the
fixes worth doing now. Parent agents can also apply the same recipes directly
with `subagent(...)` when the user describes the workflow in natural language
instead of invoking a slash command.

Additional user prompt templates can delegate into `pi-subagents` through the native `/prompt-workflow` command. This is useful when a slash command should always run through a particular agent or with forked context. Prompt frontmatter can set `subagent`, `model`, `skill`, `cwd`, `fresh`, `fork`, or `chain` for the native adapter; `chain:` templates run as structured `steps`.

## Extension RPC

There is no extension-RPC surface in rpi: other extensions cannot call `pi-subagents` through an in-process RPC bus. Cross-session coordination goes through the supervisor/intercom tools covered in `references/execution-controls.md` and lifecycle artifact files.
