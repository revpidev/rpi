# jev-router (example rpi extension)

Virtual-model example ported from upstream's
`packages/coding-agent/examples/extensions/jev-router.ts` (pi v1.0.0 /
`a13d35a74`, V16-12).

`jev/auto` plans on a strong OpenAI Codex model and implements on a cheap
one:

- Planning: `gpt-5.6-sol` for complex work, `gpt-5.6-terra` otherwise. The
  TypeSafe Jev classifier rates the first user message; the planning model
  stays for the rest of the phase.
- Implementation: `gpt-5.6-luna` after the first successful `edit`/`write`
  tool result; the phase is router state stored on the session branch.

Requests outside the agent loop (compaction summaries,
`ctx.modelRegistry.*`) route to Luna. The selected thinking level passes
through as the routed model's reasoning effort.

Build for `wasm32-unknown-unknown` (add the target first):

```sh
rustup target add wasm32-unknown-unknown
cargo build --target wasm32-unknown-unknown --release
```

Requires TypeSafe credentials (`TYPESAFE_API_KEY`) and an OpenAI Codex login:

```sh
rpi -e ./target/wasm32-unknown-unknown/release/rpi_wasm_extension_jev_router.wasm --model jev/auto
```