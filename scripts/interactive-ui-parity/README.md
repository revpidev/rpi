# interactive-ui-parity（V14-22 C2 双载体一致性 harness）

R-U7.4 / G11 第 2 条的验收装置：同一 JSONL 输入脚本驱动 **native**
（`crates/rpi-test-native-plugin` cdylib）与 **wasm**
（`examples/wasm-extension`）两个 fixture guest，在真实 host-call JSON 通道
（`rpi-ext-host` 的 `rpi_host_call` 分发 + `ScriptedUiBridge`）上跑同一个
scripted 组件，逐帧 diff `{lines,cursor?,done?}` 与终止结果。

## 运行

```bash
# 从仓库根执行（自动构建两个 fixture；wasm32 目标缺失时回退到用户级 rustup）
python3 scripts/interactive-ui-parity/run.py

# 只跑二进制（fixture 已构建时）
cargo run -p rpi-test-support --bin interactive-ui-parity -- \
  --fixture scripts/interactive-ui-parity/corpus \
  --out fixtures/generated/interactive-ui-parity
```

产物：

- `fixtures/generated/interactive-ui-parity/parity-report.md` —— 报告（构建/逐场景/fuzz/结论）；
- `<scenario>.native.json` / `<scenario>.wasm.json` —— 各载体完整转录（frames / terminal /
  toolResult / mountOptions）；
- `<scenario>.diff.json` —— 一致性判定（parity 投影 + 文档化约束标记）。

退出码非 0 = 任何帧序列/终止结果差异、fixture 缺失或加载失败。

## 语料格式（`corpus/*.jsonl`）

一行一个宿主事件（顺序即投递顺序），文件名为场景名：

| 行 | 事件 |
|---|---|
| `{"input":"a"}` | `input`（原始按键字节，不归一化） |
| `{"resize":[80,24]}` | `resize` |
| `{"tick":1}` | `tick` |
| `{"hidden":true}` / `{"hidden":false}` | `visibility` |
| `{"wake":1}` | `render` |
| `{"theme":{"name":"light"}}` | `theme` |
| `{"focus":true}` / `{"blur":true}` | `focus` / `blur` |
| `{"dispose":"sessionReload"}` | `dispose`（终止路径） |

每个场景必须以能触发 `done` 的输入（`q`）或 `dispose` 结束；脚本提前耗尽会被
harness 判为失败（`scriptExhausted`）。

> 驱动在系统临时目录下拷贝 fixture 包（逐场景清理）；若 `/tmp` 空间不足，
> 先设置 `TMPDIR=<大容量目录>` 再运行（例如 `TMPDIR=$HOME/.cache/rpi-ui-parity-tmp`）。

## fuzz（§4.5）

`run.py` 默认追加 24 个 seed 固定（`20260910`）的随机事件序列（长度/交错/
隐藏态/主题/焦点/tick/wake/宽字符），同样双载体对拍；差异即非 0 退出，可用
`--fuzz N --seed S` 复现。

## 允许的差异（文档化执行约束）

只允许设计 §4.4 已定约束出现在对拍中，且 harness 会显式标注：

- **wasm 帧预算**：`mountOptions.maxFrameBytes` 由 wasm 载体钳到 512 KiB
  （native 保持 guest 请求值）。这不是行为差异，`documented_constraint_only`
  会验证「除该字段外两条转录完全相等」后才判 MATCH。

其余任何帧序列/终止结果差异都判失败（R-U7.4）。宿主侧注册表行为（tick
暂停/恢复、wake 线程安全、限额、fuel/trap 卸载）由
`crates/rpi` / `crates/rpi-ext-host` 的单元测试覆盖，不在此 harness 重复。
