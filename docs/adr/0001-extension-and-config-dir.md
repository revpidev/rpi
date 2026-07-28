# ADR-0001：扩展宿主与配置目录

- **状态**：已采纳  
- **日期**：2026-07-27  
- **关联**：[`00-feasibility.md`](./00-feasibility.md)、[`01-requirements.md`](./01-requirements.md)、[`02-design.md`](./02-design.md)

## 背景

Pi 的扩展生态基于 TypeScript + jiti 动态加载。纯 Rust 无法直接执行现有 `.ts` / npm pi-package。配置路径在 Pi 中为 `~/.pi/agent` 与项目 `.pi`。

## 决策

### 1. 扩展：仅 Rust / Wasm API 同构

- **做**：`ExtensionAPI` **形状与事件语义**与 Pi 对齐（registerTool/Command、生命周期钩子、UI bridge 等）。
- **实现**：
  - **Rust**：crate 内置扩展 + `cdylib` / 稳定 ABI 动态插件
  - **Wasm**：组件或 wasm32 插件 + host ABI（与 Rust API 同一套能力面）
- **不做（明确非目标）**：嵌入 Deno/Node/QuickJS 跑现有 TS 扩展；Node sidecar；兼容现有 npm/git **pi-package 中的 TS 扩展**。
- **允许的差异**：扩展需用 Rust/Wasm 重写；**不要求** `jiti` / TS `package.json#pi.extensions` 入口兼容。分发与安装见 [ADR-0002](./0002-baseline-decisions.md)（列入正式计划）。
- **仍应对齐**：Skills / Prompt Templates / Themes 等**声明式资源**的文件格式与发现规则（与扩展代码执行无关）。

### 2. 配置目录：默认 `~/.pir`

| 用途 | 路径 |
|------|------|
| 全局 agent 目录 | `~/.pir/agent/` |
| 项目本地 | `<cwd>/.pir/` |
| 环境变量前缀 | `PIR_*` |

- Session、settings、auth、skills、extensions、themes、packages 等均落在上述树下（布局镜像 Pi 的 `agent/` 子结构，仅根名改为 `pir`）。
- **不**默认读写 `~/.pi` / `.pi`。若未来需要迁移工具，另开 ADR，不作为运行时默认行为。

## 后果

- 工作量估算（核心工程 **~23–33 人月**，另计 20–30% 持续开销，全量约 28–43 人月；仍含完整 TUI/Provider）以「砍掉 TS 扩展兼容」为前提。
- 生态从零建设；需提供扩展脚手架与 ABI 文档。
- 与 Pi 用户并存时配置互不干扰；session JSONL **格式**仍应力求互通，便于手动拷贝迁移。

## 否决的备选

- L2 嵌入 JS 跑 TS 扩展（复杂度高，非当前目标）
- 默认兼容 `~/.pi`（易与官方 Pi 互相踩配置；用户已选定 `~/.pir`）
