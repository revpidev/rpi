# subagents parity report (target track: pi-subagents v0.74.0 @ b6bda32f snapshot, ADR-0034)

generated: 2026-10-05T07:25:16.413Z

## args

- minimal-fresh: MATCH
- fork-session-file: MATCH
- tools-allowlist-with-read-autoprep: MATCH
- tools-empty-no-tools: MATCH
- extensions-declared-disables-ambient: MATCH
- subagentOnly-extensions-ambient: MATCH
- no-session: MATCH
- long-task-file-delivery: MATCH
- thinking-suffix-preserved: MATCH

## args (inline [RPI-OWN] golden)

- te18-exclude-deny-after-allow: MATCH (inline [RPI-OWN] golden)
- te18-exclude-without-allowlist: MATCH (inline [RPI-OWN] golden)

## frontmatter

- scout-upstream: MATCH
- block-list-tools: MATCH
- quoted-and-folded: MATCH
- no-frontmatter: MATCH
- unterminated: MATCH
- unknown-fields-ignored: MATCH
- tools-inherit: MATCH
- tools-false: MATCH
- exclude-tools: MATCH
- bad-frontmatter-invalid-line: MATCH
- thinking-field: MATCH

## final-output

- last-text-part: MATCH
- skip-errored: MATCH
- empty: MATCH
- acceptance-fence-whole-message: MATCH
- toolresult-and-user-ignored: MATCH
- multi-text-parts-join: MATCH
- blank-text-part-skipped: MATCH

## fallback

- overflow-context-length-exceeded: MATCH
- overflow-maximum-context-length: MATCH
- overflow-plain-error-control: MATCH
- overflow-tool-failure-not-overflow: MATCH

## model

- empty-registry-passthrough: MATCH
- no-registry-passthrough: MATCH
- explicit-hit-canonicalized: MATCH
- explicit-thinking-suffix-retry: MATCH
- explicit-miss-fails-closed: MATCH
- inherited-miss-passes-through: MATCH
- inherit-sentinel-resolves-parent: MATCH
- empty-model-inherits-parent: MATCH
- candidates-explicit-miss-throws: MATCH
- candidates-configured-miss-deferred-throw: MATCH
- candidates-inherited-passthrough: MATCH
- candidates-empty-chain-no-throw: MATCH
- scoped-token-expands-snapshot: MATCH
- scoped-token-in-snapshot-passes: MATCH
- scoped-token-empty-degrades-inherit: MATCH
- scoped-token-unresolved-fails-closed: MATCH
- scoped-token-agent-rule-candidates: MATCH
- provider-prefixed-catalog-id: MATCH
- huggingface-owner-name-whole-id: MATCH
- scope-renders-eight-patterns: MATCH

## discovery

- user-tree-robustness: MATCH

## notify

- single-basic: MATCH
- run-id-with-handoff: MATCH
- run-id-alone: MATCH
- task-info-with-run-id: MATCH
- parse-legacy-no-run-id: MATCH
- parse-roundtrip-run-id: MATCH
- parse-non-notify-text: MATCH

## Attribution summary

### upstream-semantics

- (none)

### rpi-deviation

- (none)

### unattributed

- (none)


## RESULT: MATCH
