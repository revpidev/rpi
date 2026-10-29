# rpi-durable

**Experimental.** The API changes without notice between releases.

A port of the upstream `@earendil-works/pi-durable` package
(`packages/durable` @ `4a080252f` / v1.4.0): the durable task runtime —
storage stack, session kernel, harness/scheduler — plus the tool chain and the
local execution environment.

## Isolation

The crate is a workspace member but is **not part of the default CLI path**:
`crates/rpi` does not depend on `rpi-durable`, no default command is
registered, and the durable storage stack never reads or writes the host's
JSONL session store. Consumers build against it directly (the
`persist/resume` acceptance tests are the integration anchors).

## Storage migration and namespacing

Opening a version 1 SQLite storage migrates it to schema version 2: the task
table is rebuilt in one transaction to accept the `sleeping` status. A migrated
database cannot be opened by rpi 0.1.7 or earlier, so keep a copy before
opening a valuable database. JSONL needs no migration and keeps reading
format 1 directories unchanged. Pass `SqliteStorageOptions::prefix` (or
`NodeSqliteStorageOptions::prefix`) to keep every durable table and index under
a validated namespace in a shared database; an invalid or reserved `sqlite_`
prefix fails before the database file is created.

## Layout

- `src/storage/` — Memory / JSONL / SQLite backends over the asynchronous
  SQLite facade, durable schema identifiers and the version 1 → 2 history.
- `src/chord/sqlite/` — portable SQLite driver interfaces, the generic
  migration runner and prefix validation (upstream v1.3+ location); the
  `storage::sqlite` entry points re-export the same names.
- `src/session/` — immutable entry stream, transactions, documents, forks.
- `src/harness/` — registry, scheduler, submissions/inbox, built-in
  generation/tool/compaction tasks, live task graph.
- `src/tools/` — `read`, `write`, `edit`, `bash` (and `powershell`) over the
  execution environment.
- `src/env/` — portable `ExecutionEnv` surface: file system, shell, streaming
  decoders, line scanner, snapshot watcher, and the local `NodeExecutionEnv`.

## Verification

- `cargo test -p rpi-durable` — storage conformance (three backends), session
  and scheduler recovery, tool semantics, and the execution-environment
  conformance suite.
- `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo fmt --all -- --check`.