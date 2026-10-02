# codemode description parity fixture

`codemode-description.json` is generated from the pinned upstream
(`external/pi` @ `a13d35a74`, v1.0.0) with
`packages/coding-agent/src/extensions/codemode/tool.ts`
(`createCodemodeDescription` / `describeScriptCall` / `describeOutput`).

Generation method (no upstream file is modified): the needed sources are
copied to a temp directory, the `@earendil-works/pi-codemode` imports are
rewritten to the copied `packages/codemode/src` files, a `getDocsPath()` stub
reads `PI_PACKAGE_DIR`, and Node's built-in type stripping runs the driver:

```bash
PI_PACKAGE_DIR=/tmp/rpi-codemode-docs node --experimental-strip-types gen.mjs
```

The test sets `RPI_PACKAGE_DIR=/tmp/rpi-codemode-docs` so both sides render
the same `codemode.md` path, then asserts the ported Rust renderer produces
byte-identical description text (V16-07 FR-H R1 hard-parity surface).