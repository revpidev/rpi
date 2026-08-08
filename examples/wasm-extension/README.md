# pir wasm extension example: permission-gate

ABI v1 guest（`docs/extension-abi.md`）。行为：`tool_call` 里 block `read`
工具；注册自定义 `gate_tool`。

## 构建

```sh
rustup target add wasm32-unknown-unknown   # 一次性
cargo build --target wasm32-unknown-unknown --release
mkdir -p dist
cp ../../target/wasm32-unknown-unknown/release/pir_wasm_extension_example.wasm dist/permission_gate.wasm
```

## 安装

把整个 `examples/wasm-extension/` 目录拷贝（或链接）到
`~/.pir/agent/extensions/permission-gate/`（或项目的 `.pir/extensions/`），
pir 启动时按一层目录发现规则加载 `pir-extension.json`。

裸 `.wasm` 文件（无 manifest）也可直接放入扩展目录，此时
`capabilities = []`（仅 `on` 订阅），本例的 `registerTool` 会被
`capabilityDenied` 拒绝——用它可演示沙箱。
