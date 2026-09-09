# subagents parity report (target track: pi-subagents v0.66.0 @ 0fc0eebb)

generated: 2026-09-09T04:15:30.800Z

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

- retry-request-limit-exceeded: ATTRIBUTED [upstream-semantics] R7.1.2.1 → TE14 fields: retryable
- retry-usage-limit: ATTRIBUTED [upstream-semantics] R7.1.2.1 → TE14 fields: retryable
- retry-connection-reset: ATTRIBUTED [upstream-semantics] R7.1.2.1 → TE14 fields: retryable
- retry-http-500: ATTRIBUTED [upstream-semantics] R7.1.2.1 → TE14 fields: retryable
- retry-internal-server-error: ATTRIBUTED [upstream-semantics] R7.1.2.1 → TE14 fields: retryable
- retry-rate-limit-control: MATCH
- retry-quota-control: MATCH
- retry-tool-failure-not-retryable: MATCH
- overflow-context-length-exceeded: ATTRIBUTED [upstream-semantics] R7.1.2.2 → TE14 fields: contextOverflow
- overflow-maximum-context-length: ATTRIBUTED [upstream-semantics] R7.1.2.2 → TE14 fields: contextOverflow
- overflow-plain-error-control: ATTRIBUTED [upstream-semantics] R7.1.2.2 → TE14 fields: contextOverflow
- overflow-tool-failure-not-overflow: ATTRIBUTED [upstream-semantics] R7.1.2.2 → TE14 fields: contextOverflow
- attempt-tool-count-blocks-replay: ATTRIBUTED [upstream-semantics] R7.1.2.3 → TE14 fields: attempt
- attempt-no-tools-empty-messages: ATTRIBUTED [upstream-semantics] R7.1.2.3 → TE14 fields: attempt
- attempt-empty-output-cold-start: ATTRIBUTED [upstream-semantics] R7.1.2.3 → TE14 fields: attempt
- attempt-matching-message-error: ATTRIBUTED [upstream-semantics] R7.1.2.3 → TE14 fields: attempt

## Attribution summary

### upstream-semantics

- fallback/retry-request-limit-exceeded (R7.1.2.1 → TE14)
- fallback/retry-usage-limit (R7.1.2.1 → TE14)
- fallback/retry-connection-reset (R7.1.2.1 → TE14)
- fallback/retry-http-500 (R7.1.2.1 → TE14)
- fallback/retry-internal-server-error (R7.1.2.1 → TE14)
- fallback/overflow-context-length-exceeded (R7.1.2.2 → TE14)
- fallback/overflow-maximum-context-length (R7.1.2.2 → TE14)
- fallback/overflow-plain-error-control (R7.1.2.2 → TE14)
- fallback/overflow-tool-failure-not-overflow (R7.1.2.2 → TE14)
- fallback/attempt-tool-count-blocks-replay (R7.1.2.3 → TE14)
- fallback/attempt-no-tools-empty-messages (R7.1.2.3 → TE14)
- fallback/attempt-empty-output-cold-start (R7.1.2.3 → TE14)
- fallback/attempt-matching-message-error (R7.1.2.3 → TE14)

### rpi-deviation

- (none)

### unattributed

- (none)


## RESULT: ATTRIBUTED-OK
