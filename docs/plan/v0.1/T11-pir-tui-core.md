# T11：pir-tui 核心引擎

- **状态**：未开始
- **里程碑**：M5
- **依赖**：T01
- **上游对照**：`packages/tui/src/{tui,terminal,keys,stdin-buffer}.ts`、`src/components/{text,container,spacer}.ts`、`docs/tui.md`
- **需求章节**：§8.6（引擎部分）
- **预估**：3–4 人月（M5 共 8–11，与 T12 合计）

---

## 目标

移植 pi-tui 的渲染与输入引擎：ANSI 行列表 + 三策略差分 + CSI 2026 + Kitty/legacy
键位解析，为 Interactive 模式提供与 Pi 行为一致的底座。

## 范围

### In

- crossterm 后端：raw mode、读写、尺寸（**不引入 ratatui**，设计文档 §5.1）
- `Component` / `Focusable` trait；`Tui` 容器（children / overlays / focus / previous_lines）
- 渲染管线（编码规范 §8.3，步骤不得重排）：CSI 2026 包裹 → 全量/差分策略 → 16ms 节流 → 行尾 SGR + OSC 8 reset → Kitty 图像行 expand/delete
- `StdinBuffer` → 键位解析（Kitty flags=7 + legacy CSI）→ 全局 listener → focused 组件
- `KeybindingsManager`：读 JSON 映射到 editor/action 枚举，token 名与 Pi 一致；**禁止硬编码键位**
- 基础组件：`Text` / `Container` / `Spacer`
- 宽度工具：grapheme 宽度（`unicode-width` + ANSI 感知包装）
- 终端状态恢复：进入保存、退出/panic/信号恢复（panic hook 先恢复终端再输出，编码规范 §8.5）
- 终端特例处理框架（Windows Terminal / tmux / Apple，按上游逻辑移植）

### Out

- 业务组件（Editor / SelectList / Markdown / SettingsList / Image，T12）
- Interactive 模式绑定（T12）

## 开发要点

- `VirtualTerminal`（T02）驱动帧级测试：断言 ANSI 序列子集（去 CSI 2026 抖动）
- 渲染差分的三策略分支逐一构造触发条件测试
- 终端恢复是硬性正确性要求：所有退出路径逐条核对（正常、abort、错误、panic）

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] VirtualTerminal 帧对比：首次全量 / 尺寸变化全量 / 行差分三路径
- [ ] 16ms 节流行为测试
- [ ] 行尾 SGR + OSC 8 reset 断言
- [ ] 键位解析：Kitty flags=7 各修饰键组合 + legacy CSI 回退
- [ ] 键位全部来自默认表/JSON 配置，无硬编码（grep 检查 + 测试覆盖）
- [ ] panic hook：人为 panic 后终端状态恢复（VT 断言）
- [ ] 宽度工具：CJK / emoji / 组合字符 / ANSI 包裹文本宽度正确

## 门禁验收

通用门禁 G1–G7 全过。

任务特有标准：

- [ ] 渲染管线五步有测试锚点且顺序锁定
- [ ] 组件渲染快照黄金文件（Text/Container/Spacer）建立
- [ ] 真机 smoke：至少本机一种终端人工验证无闪烁、键位可用（记录终端与结果）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
