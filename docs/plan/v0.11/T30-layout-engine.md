# T30：布局引擎

- **状态**：未开始
- **里程碑**：M5
- **依赖**：T28
- **上游对照**：`ea1e77e2d`（视口布局系统）、`f24ab6e14`（嵌套最小尺寸修复）、`6129a353b`+`8ac92f831`（滚动条）；`packages/tui/src/layout.ts`（410 行）、`layout-node.ts`（51 行）、`components/{stack,v-stack,h-stack,scroll-view}.ts`、`test/layout.test.ts`（306 行）
- **需求章节**：v0.11 需求 R5.2.2；设计 §4.2
- **预估**：0.4 人月

---

## 目标

落地全屏渲染依赖的布局引擎：LayoutNode 协议、Stack 约束求解、ScrollView、
clip 传播与按宽度 render cache。这是 T31 全屏渲染器的直接前置。

## 范围

### In

- `LayoutNode` trait（替代上游 `LAYOUT_NODE` symbol 协议）：任意组件实现即参与布局；`StackLayoutNode`/`ScrollLayoutNode`
- `allocate_stack_sizes()`：basis/grow/shrink/min/max/gap 约束求解（含嵌套最小尺寸修复 `f24ab6e14`）
- `VStack`/`HStack`（含常规渲染模式下的非布局回退实现）
- `ScrollView`：follow-end、`primary`、overscroll 链式（`chain|contain`，`scroll_by` 返回未消费增量）、滚动条三态（`hidden|auto|always`，`always` 预留最右列）、transient 滚动条 1s 隐藏 + thumb 拖拽几何、运行时 `set_scrollbar()`
- `render_layout_frame`：render cache 按宽度键控、clip 传播、滚动条绘制、图片行裁剪钩子、OSC133 前缀剥离
- `get_scroll_views_at`（指针命中滚动视图，层级排序）、`get_scrollbar_geometry`
- 组件可见性回调（`visible`）

### Out

- 全屏渲染器本体（终端控制/鼠标/选择/Kitty 缓存 → T31）
- 布局引擎在 main-screen 模式的应用（上游亦不启用，仅全屏用）

## 开发要点

- 约束求解算法直接移植（`allocateStackSizes`），用上游 `layout.test.ts` 306 行锁定
- render cache 键控宽度：缓存失效边界用例（同宽不同内容不得命中）
- clip 传播与滚动条几何是全屏鼠标命中（T31）的基础，几何计算单独黄金化

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] `layout.test.ts` 306 行测试意图全移植通过
- [ ] 约束求解：grow/shrink/basis/min/max/gap 组合矩阵 + 嵌套最小尺寸回归
- [ ] ScrollView：follow 语义、overscroll 链式未消费增量、滚动条三态几何、transient 隐藏计时
- [ ] render cache 宽度键控边界；clip 传播嵌套场景

## 门禁验收

通用门禁 G1–G7 全过（G3：布局几何黄金）。

任务特有标准：

- [ ] 需求 R5.2.2 逐条核对表

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| （待登记） | | |

## 验收记录

（按 gates §3 模板填写）
