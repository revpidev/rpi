# Vendored `quickjs.wasm` (quickjs-wasi 3.6.2)

Source artifact for the single JS engine of the codemode sandbox
(ADR-0034 decision 3, ADR-0035; replaces the ADR-0001 "no JS" red line only
for this QuickJS-via-WASM form).

- npm package: `quickjs-wasi@3.6.2`
- Upstream repository: `git://github.com/vercel-labs/quickjs-wasi.git`
- Tarball: `https://registry.npmjs.org/quickjs-wasi/-/quickjs-wasi-3.6.2.tgz`
- Tarball sha256: `f1f4349f19a2d849e33ea0ae9bec2e7062b8839f4eceb17c9051ddbaa2720982`
- npm dist.integrity: `sha512-FCqGtGOrMgzUiIrMNMA2YnsOxCNwo31dzqXvclXUC6xeT35NJLKXQJsvbeCTjvoFAwZgEAPg8U6+KAPDGXn8Mg==`
- npm dist.shasum: `6ab7ed689997a9013b49ea7bbce3e88896bd80d9`
- `quickjs.wasm` sha256: `d4c9375f2b1ca4dc95f72c8aa2982a7a9951ac8011490d79c6582df732b4bbd9`
- `quickjs.wasm` size: 637405 bytes
- Retrieved: 2026-10-02 (files copied out of the official tarball; the file
  hash was verified against the installed package copy as a second source)

No patches are applied: the binary is embedded byte-for-byte by
`crates/rpi-codemode/src/wasm.rs` (`include_bytes!`), and the sha256 above is
asserted by `wasm::verify_embedded_wasm()` in tests (G1/vendor gate). The
sandbox drives the module through the `qjs_*` C ABI exported by the binary
(the same ABI `quickjs-wasi`'s own JS wrapper consumes), not through any
Node/Bun glue.

`LICENSE` next to this file is the package's MIT license text.

## rpi host shims (`random_get`)

The rpi host (`crates/rpi-codemode/src/runtime/worker.rs`) implements the
module's WASI `random_get` import with `RandomState` (`std`'s SipHash-1-3 over
OS-seeded, per-thread keys). That is **not a CSPRNG**; the upstream
quickjs-wasi wrapper uses `crypto.getRandomValues` instead. In 3.6.2 the
import is consumed once per VM by WASI libc init, while `Math.random()` seeds
from `clock_time_get`, so no script-visible capability depends on these bytes.
If a future extension path consumes `random_get`, replace the body with an OS
entropy source (for example the `getrandom` crate) before shipping it.