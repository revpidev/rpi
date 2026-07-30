# Wasm ABI Spike（T02）

验证设计文档 §7 / 需求 §9.2 的 Wasm 扩展协议形状（M0 技术不确定性消除）。
正式 Wasm 宿主在 T15 实现；本目录的 spike 代码保留为 ABI 参考。

## 验证的三个往返

1. **registerTool**：guest（Wasm）注册工具描述符 → host 记录 → host 回调
   guest 的 `pir_tool_execute` 执行工具；
2. **dialog 往返**：工具执行中 guest 调 `pir_host_dialog_select`，host
   （脚本化 UiBridge）返回 `option-b`；
3. **声明式 UI 组件描述渲染往返**：guest 发送组件树 JSON → host 渲染成
   文本帧并返回用户事件（click）→ guest 更新组件树二次渲染。

## ABI 形状（spike 约定）

- 载荷均为 guest 线性内存中的 UTF-8 JSON；
- guest → host：`(ptr, len)` 两个参数；
- host → guest：单个 `u64`，`(ptr << 32) | len`，内存由 guest 的
  `pir_alloc` 分配；
- host import 模块名 `pir`：`pir_host_register_tool` /
  `pir_host_dialog_select` / `pir_host_render_component`。

## 运行

```bash
# 1. 构建 guest（需要 wasm32-unknown-unknown target）
cd crates/pir-ext-host/examples/wasm-spike/guest
cargo build --release --target wasm32-unknown-unknown

# 2. 运行宿主 spike（workspace 根目录）
cd ../../../../..
cargo run -p pir-ext-host --example wasm_spike
# 输出 WASM SPIKE OK 且退出码 0 即通过
```

> 工具链说明：本机系统 rustup home（`/usr/local/rust/rustup`）为 root 所有、
> 无法加装 target；仓库内 `.tooling/`（已 gitignore）提供带
> `wasm32-unknown-unknown` / `x86_64-unknown-linux-musl` target 的 1.97.1
> 工具链，使用时设置
> `RUSTUP_HOME=<repo>/.tooling/rustup CARGO_HOME=<repo>/.tooling/cargo` 即可。

## musl release 体积实测（T02，需求 §11.2）

```bash
CC_x86_64_unknown_linux_musl=gcc RUSTFLAGS="-C linker=rust-lld" \
  cargo build --release -p pir --target x86_64-unknown-linux-musl
ls -la target/x86_64-unknown-linux-musl/release/pir
./target/x86_64-unknown-linux-musl/release/pir --wasm-smoke
```

说明：musl target 用 rust-lld 自包含链接（无 musl-gcc 环境）；
`CC_x86_64_unknown_linux_musl=gcc` 覆盖 cc-rs 的默认 musl-gcc 查找，
用于编译 wasmtime 自带的小段 C（zstd / runtime helpers）。
`--wasm-smoke` 钩子（T15 前临时存在）强制 wasmtime 链入二进制，
使体积测量反映真实嵌入。
