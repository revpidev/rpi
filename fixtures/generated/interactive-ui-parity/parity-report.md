# interactive-ui-parity 报告（V14-22 C2；R-U7.4 / G11 第 2 条）

- 语料：`scripts/interactive-ui-parity/corpus/`（5 个场景，JSONL 事件脚本）
- fuzz：24 场景（seed `20260910`，固定可重放；§4.5）
- 结论：通过（零差异）

## 构建

```
native fixture: exit 0
wasm fixture: exit 0
wasm fixture: examples/wasm-extension/target/wasm32-unknown-unknown/release/rpi_wasm_extension_example.wasm
```

## 逐场景（frames = `{lines,cursor?,done?}` 数）

| 场景 | native 帧 | wasm 帧 | 一致 | 文档化载体约束 |
|------|-----------|---------|------|----------------|
| `basic` | 5 | 5 | 是 | mountOptions.maxFrameBytes: wasm carrier caps the frame budget at 512 KiB (design §4.4) |
| `dispose` | 3 | 3 | 是 | mountOptions.maxFrameBytes: wasm carrier caps the frame budget at 512 KiB (design §4.4) |
| `styles_cursor` | 5 | 5 | 是 | mountOptions.maxFrameBytes: wasm carrier caps the frame budget at 512 KiB (design §4.4) |
| `visibility_focus_theme` | 9 | 9 | 是 | mountOptions.maxFrameBytes: wasm carrier caps the frame budget at 512 KiB (design §4.4) |
| `wake_and_tick` | 6 | 6 | 是 | mountOptions.maxFrameBytes: wasm carrier caps the frame budget at 512 KiB (design §4.4) |

> 文档化约束：wasm 载体的帧总预算固定上限 512 KiB（native 保持 guest 请求值），属设计 §4.4 已定执行约束，不占偏离编号（任务 §2.1）。

## 驱动输出

```
MATCH basic (native frames=5 wasm frames=5; documented constraint: wasm frame budget cap)
MATCH dispose (native frames=3 wasm frames=3; documented constraint: wasm frame budget cap)
MATCH styles_cursor (native frames=5 wasm frames=5; documented constraint: wasm frame budget cap)
MATCH visibility_focus_theme (native frames=9 wasm frames=9; documented constraint: wasm frame budget cap)
MATCH wake_and_tick (native frames=6 wasm frames=6; documented constraint: wasm frame budget cap)
fuzz: 24 scenarios (seed 20260910)
MATCH fuzz-000 (native frames=3 wasm frames=3; documented constraint: wasm frame budget cap)
MATCH fuzz-001 (native frames=4 wasm frames=4; documented constraint: wasm frame budget cap)
MATCH fuzz-002 (native frames=5 wasm frames=5; documented constraint: wasm frame budget cap)
MATCH fuzz-003 (native frames=11 wasm frames=11; documented constraint: wasm frame budget cap)
MATCH fuzz-004 (native frames=10 wasm frames=10; documented constraint: wasm frame budget cap)
MATCH fuzz-005 (native frames=5 wasm frames=5; documented constraint: wasm frame budget cap)
MATCH fuzz-006 (native frames=10 wasm frames=10; documented constraint: wasm frame budget cap)
MATCH fuzz-007 (native frames=13 wasm frames=13; documented constraint: wasm frame budget cap)
MATCH fuzz-008 (native frames=12 wasm frames=12; documented constraint: wasm frame budget cap)
MATCH fuzz-009 (native frames=6 wasm frames=6; documented constraint: wasm frame budget cap)
MATCH fuzz-010 (native frames=3 wasm frames=3; documented constraint: wasm frame budget cap)
MATCH fuzz-011 (native frames=8 wasm frames=8; documented constraint: wasm frame budget cap)
MATCH fuzz-012 (native frames=5 wasm frames=5; documented constraint: wasm frame budget cap)
MATCH fuzz-013 (native frames=10 wasm frames=10; documented constraint: wasm frame budget cap)
MATCH fuzz-014 (native frames=3 wasm frames=3; documented constraint: wasm frame budget cap)
MATCH fuzz-015 (native frames=9 wasm frames=9; documented constraint: wasm frame budget cap)
MATCH fuzz-016 (native frames=13 wasm frames=13; documented constraint: wasm frame budget cap)
MATCH fuzz-017 (native frames=10 wasm frames=10; documented constraint: wasm frame budget cap)
MATCH fuzz-018 (native frames=9 wasm frames=9; documented constraint: wasm frame budget cap)
MATCH fuzz-019 (native frames=6 wasm frames=6; documented constraint: wasm frame budget cap)
MATCH fuzz-020 (native frames=11 wasm frames=11; documented constraint: wasm frame budget cap)
MATCH fuzz-021 (native frames=9 wasm frames=9; documented constraint: wasm frame budget cap)
MATCH fuzz-022 (native frames=12 wasm frames=12; documented constraint: wasm frame budget cap)
MATCH fuzz-023 (native frames=7 wasm frames=7; documented constraint: wasm frame budget cap)
interactive-ui-parity: 29 scenarios, 0 difference(s) — OK
```

## 产物

- `*.native.json` / `*.wasm.json`：各载体完整转录（frames/terminal/toolResult/mountOptions）。
- `*.diff.json`：逐场景一致性判定（parity 投影 + 文档化约束标记）。
