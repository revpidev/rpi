# rpi wasm extension example: permission-gate

An ABI v1 guest (`docs/extension-abi.md`). Behavior: blocks the `read` tool in
`tool_call`; registers a custom `gate_tool`.

## Build

```sh
rustup target add wasm32-unknown-unknown   # one-time
cargo build --target wasm32-unknown-unknown --release
mkdir -p dist
cp ../../target/wasm32-unknown-unknown/release/rpi_wasm_extension_example.wasm dist/permission_gate.wasm
```

## Install

Copy (or symlink) the whole `examples/wasm-extension/` directory to
`~/.rpi/agent/extensions/permission-gate/` (or a project's `.rpi/extensions/`).
rpi loads `rpi-extension.json` on startup using the one-directory-deep
discovery rule.

A bare `.wasm` file (without a manifest) can also be dropped directly into the
extensions directory; in that case `capabilities = []` (only `on` subscriptions
are allowed), and this example's `registerTool` would be rejected with
`capabilityDenied` — useful for demonstrating the sandbox.
