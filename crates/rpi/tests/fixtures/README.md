# codemode description parity fixture

`codemode-description.json` is generated from the pinned upstream
(`external/pi` @ `a13d35a74`, v1.0.0) with
`packages/coding-agent/src/extensions/codemode/tool.ts`
(`createCodemodeDescription` / `describeScriptCall` / `describeOutput`).

Regenerate with Node's built-in type stripping (Node 22.18+/24):

```bash
node --experimental-strip-types crates/rpi/tests/fixtures/gen-codemode-description.mjs
```

The generator is self-contained. It never modifies an upstream file: it
copies the needed sources to a temp directory, rewrites the
`@earendil-works/pi-codemode` imports to the copied
`packages/codemode/src` files, stubs `getDocsPath()` to read
`PI_PACKAGE_DIR` (`/tmp/rpi-codemode-docs`), runs the upstream functions and
writes the fixture. The Rust test
(`crates/rpi/tests/codemode_parity_test.rs`) sets the same
`RPI_PACKAGE_DIR`, then asserts the ported renderer produces byte-identical
text (V16-07 FR-H R1 hard-parity surface).