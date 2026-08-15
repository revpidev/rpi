# smart-fetch parity harness (TE06)

Golden-fixture parity between the pinned upstream `agent-smart-fetch`
(v0.3.17 / `b0111612`, submodule under `external/agent-smart-fetch`) and
`crates/rpi-ext-smart-fetch` (design §5.1/§5.3).

- `gen-fixtures.mjs` — drives the pinned upstream TS sources over a fixed
  input matrix and writes input/output pairs to
  `fixtures/generated/smart-fetch-parity/` (committed as evidence).
  Upstream-private functions (`formatByteCount`, the error builders, the DOM
  fallback chain, download filename derivation) are exercised through their
  exported callers — `buildFetchErrorResponseText` and
  `createDefuddleFetch` with a mocked transport/defuddle, the same strategy
  the upstream unit tests use (`packages/core/test/unit/`).
- `run.sh` — staging + regeneration + `cargo test` replay in one step.
- `crates/rpi-ext-smart-fetch/tests/parity_fixtures.rs` — replays the
  fixtures through the port (pipeline scenarios go through the injected
  transport/extractor seams).

## One-time setup

Upstream dependencies are external to the repo (never inside `external/`,
which must stay pristine — gates.md G4):

```bash
mkdir -p /tmp/rpi-smart-fetch-parity-deps
cd /tmp/rpi-smart-fetch-parity-deps
cat > package.json <<'EOF'
{
  "name": "rpi-smart-fetch-parity-deps", "private": true, "type": "module",
  "dependencies": {
    "@earendil-works/pi-coding-agent": "0.84.2",
    "@sinclair/typebox": "0.34.52",
    "defuddle": "0.19.2",
    "linkedom": "0.18.13",
    "lodash": "4.18.1",
    "mime-types": "3.0.2",
    "tsx": "4.23.12",
    "wreq-js": "2.3.1"
  }
}
EOF
npm install --no-audit --no-fund
```

Known trap: if `npm install` prunes these packages later (e.g. a stray
`npm install --no-save <pkg>`), re-run the install above — `--no-save`
installs are removed from the tree on subsequent installs.

## Declared divergences recorded by the fixtures

- `truncateContent` astral cases (FR-P0-9 [VARIANT]): JS slices UTF-16 code
  units and can split a surrogate pair; the generator replaces the resulting
  lone surrogate with U+FFFD so the fixture stays parseable. The replay
  compares non-content fields only and asserts a common char prefix.
- `resolvePiSmartFetchSettings.tempDir` ([VARIANT], requirements §3):
  upstream `smart-fetch-pi` vs rpi `smart-fetch-rpi`; both sides normalize to
  `<TMPDIR>/smart-fetch-<NAME>`.
- pipeline `non-html-content-type`: upstream streams binaries to a temp file
  (FR-P1-4, TE07); the P0 pipeline terminates at "Not an HTML page". The
  replay skips this case until the download branch lands.
- Engine messages inside `Request failed while …: <message>` templates:
  mocked transports replay identical strings on both sides; real engines
  (wreq-js napi text vs wreq Display) diverge on the interpolated message
  only, never on the template.
