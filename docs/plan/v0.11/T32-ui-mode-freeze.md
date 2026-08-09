# T32：UI 模式接线与 v0.11 Parity Freeze

- **状态**：未开始
- **里程碑**：M5
- **依赖**：T18、T23、T27、T29、T31
- **上游对照**：`f074efd92`（--ui-mode）、`c72728bc1`（--alt 映射）、`5446cd754`（更名 --tui-mode/tuiMode）、`b103937d3`（运行时切换 `switchTuiMode()`）、`ac4ac9eaf`（fullscreenExitOutput）、`6129a353b`+`8ac92f831`（fullscreenScrollbar）、`b3ed27b3f`（scrollbarThumb 主题色）、`b103937d3`（`TuiStopOptions.preserveScreen` + capture/restore）
- **需求章节**：v0.11 需求 R3.2、§7（验收标准）；设计 §5.2、§7
- **预估**：0.4 人月

---

## 目标

把 fullscreen 能力接进产品（CLI 参数、设置、运行时热切换、退出行为），
随后执行 v0.11 Parity Freeze：全量验收标准核对与发布准备。

## 范围

### In

**UI 模式接线：**

- CLI：`--tui-mode regular|fullscreen`；`--alt` 保留兼容映射（帮助文本移除）；settings 键 `tuiMode`（旧 `uiMode` **忽略回退默认**，不做迁移——与上游一致）
- `/settings` 运行时热切换：停旧渲染器（`preserve_screen`）→ `capture_render_state`/`restore_render_state` → 挂新渲染器，组件树重挂载
- 设置项：`fullscreenExitOutput`（`transcript`|`resume-hint`，默认退出打印完整 transcript；`resume-hint` 只打 resume 提示）、`fullscreenScrollbar`（`auto`|`always`|`hidden`）；主题色 `scrollbarThumb`
- sticky editor/footer dock 的 interactive 侧组装（组件树接 T30 布局根）

**Parity Freeze（v0.11 收口）：**

- `docs/parity-checklist.md` 更新：本版本全部变更项的「上游锚点 → rpi 测试」映射（扩展 API 88 条清单增量、JSON/RPC delta、流终止、settings 深合并、keybindings 新增 action 等）
- 需求 §7 验收标准八条逐条执行并记录
- fixtures 全量再生成 + 归一化 diff 全绿
- session 互通终验：v0.84 上游生成的 session JSONL（v3）由 rpi 加载续跑；rpi 生成的由上游加载续跑
- release 构建 smoke（gnu/musl，沿用 v0.1 口径）与二进制大小核对
- `README.md` 状态节、`docs/parity-checklist.md`、UPSTREAM.md 一致性核对

### Out

- [DEFER] 项的任何实现（G4 红线复核）
- 下一版本（v0.12）规划

## 开发要点

- 热切换的 capture/restore 状态清单（main-screen 7 字段）逐一核对，切换前后帧级一致
- `fullscreenExitOutput` 默认 `transcript`：退出路径（含异常退出/信号）都要覆盖
- Parity Freeze 发现的任何行为级缺口走 ADR 流程，不允许带病验收

## 进度跟踪

- [ ] 设计细化
- [ ] 实现（UI 模式接线）
- [ ] 自测
- [ ] Parity Freeze 执行
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] `--tui-mode` / `--alt` 映射 / `tuiMode` 设置三入口一致；旧 `uiMode` 键忽略回退
- [ ] `/settings` 热切换：regular→fullscreen→regular 往返帧级一致、组件状态保留
- [ ] `fullscreenExitOutput` 两态 × 正常/异常退出路径
- [ ] `fullscreenScrollbar` 三态 + `scrollbarThumb` 主题色生效
- [ ] Freeze：需求 §7 八条验收记录完整；fixtures 全绿；session 双向互通通过

## 门禁验收

通用门禁 G1–G7 全过（G4 全项复核——这是版本收口任务）。

任务特有标准：

- [ ] 需求 R3.2 五条逐条核对表
- [ ] `docs/parity-checklist.md` v0.11 增量映射表完整
- [ ] Freeze 报告（fixtures diff 摘要 + 互通验证 + release smoke）附验收记录
- [ ] v0.11 全部偏离闭环（deviations 登记表无「待回写」）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| （待登记） | | |

## 验收记录

（按 gates §3 模板填写）
