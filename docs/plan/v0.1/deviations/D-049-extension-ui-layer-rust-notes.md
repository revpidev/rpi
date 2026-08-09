# D-049：T15 扩展 UI/渲染层落地差异

- **状态**：已回写
- **关联任务**：T15（W4 为主）
- **级别**：第 1 条行为级（功能缺口，ADR-0007）；第 2 条实现细节偏离
- **发现日期**：2026-08-08（W4 汇报候选的复核登记）

## 原文档约定

- 上游基准：`packages/coding-agent/src/core/extensions/types.ts`
  （`ExtensionUIContext.custom`，types.ts:195-209）@ 0.82.1（2efa728）。
- 设计 §13 剩余开放项 3（ComponentTree wire schema 由 T15 冻结）——设计草图
  含 `row` 横向容器。

## 实际实现与偏离原因

1. **（行为级，ADR-0007）`ctx.ui.custom()` 声明式 v1 无交互回传**：上游
   `custom()` 挂载获得键盘焦点的交互组件，resolve 值为组件的
   `done(result)`；rpi 的 ComponentTree 是纯 JSON 描述符、无交互通道，
   W4 实现为「树映射成 TUI 组件挂到 editor region 展示，随后立即 resolve
   `undefined`」（`crates/rpi/src/modes/interactive/interactive_mode/
   ui_bridge.rs:413-425` 注释）。需要交互回传的扩展（如 llama.cpp 的
   LlamaView）走原生 `UiBridge::as_any` downcast 口子（W7），不经过声明式
   协议。
2. **ComponentTree schema v1 无 `row`**：rpi-tui 无横向容器组件，v1 冻结为
   `text / spacer / box / column` 四种节点（垂直堆叠）；横向组合出 v1 范围。
   未知 `type` 渲染为含 JSON 原文的 text 节点（fail-visible 不静默）
   （`crates/rpi-ext-host/src/types.rs:786-801`
   `COMPONENT_TREE_SCHEMA_V1` 注释；映射器
   `rpi::modes::interactive::component_tree`）。

## 影响面

扩展 API / TUI 行为（仅扩展渲染路径）：第 1 条为 v0.1 功能缺口
（ADR-0007）；第 2 条收窄了扩展可用组件集，内置 UI 不受影响。

## 处置

- **回写位置**：`02-design.md` §13 剩余开放项 3（schema v1 冻结结论）；
  `docs/extension-abi.md` §7；本表 D-049 行；T15 任务文件偏离记录表
- **回写日期**：2026-08-08
- **ADR**：ADR-0007（第 1 条）；第 2 条不需要
