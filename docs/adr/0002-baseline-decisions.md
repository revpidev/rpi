# ADR-0002：版本钉死、交付形态与其余基线决策

- **状态**：已采纳  
- **日期**：2026-07-27  
- **关联**：[ADR-0001](./0001-extension-and-config-dir.md)、[`02-design.md`](../02-design.md)

## 决策

### 1. 上游对照版本固定

| 项 | 值 |
|----|-----|
| 仓库 | `https://github.com/earendil-works/pi.git` |
| 包版本 | **0.82.1**（`packages/coding-agent/package.json`） |
| Git commit | **`2efa728d2ee90ef597626e96b1e28ef2b279f07c`** |
| 本地路径 | `external/pi/` |

行为金标准、对拍测试、token 估算算法均以此 commit 为准。升级上游须新开 ADR 并重新对拍。

钉死记录文件：仓库根 [`UPSTREAM.md`](../../UPSTREAM.md)。

### 2. 扩展安装列入支持计划

在 Rust/Wasm 扩展 ABI（ADR-0001）之上，**产品计划包含**扩展/包的安装与管理（对齐 Pi 的 `install` / `remove` / `list` / `update` / `config` 意图），不是「仅本地路径手工拷贝」的长期方案。

第一版可分阶段落地，但需求与路线图中 **M8 必须覆盖**：

- 本地目录 / 路径安装  
- 可分发的包格式（Wasm 包为主；Rust 动态库为可选高级路径）  
- 全局 `~/.pir/agent/` 与项目 `.pir/` 安装位置  
- 启用/禁用与发现规则  

**不做**：安装并执行现有 npm TS `pi-package` 扩展代码。声明式资源（skills/prompts/themes）的包分发仍应对齐文件布局。

### 3. TUI 为交付硬性要求

Interactive TUI 是产品必达能力，不是可选后期。实施上仍可 **并行** 先打通 agent/session/RPC，但 **parity / 首个完整版本** 必须包含与 Pi 同构的交互模式（`pir-tui` + interactive mode）。

### 4. Token 估算与 Pi 完全一致

compaction、context usage、overflow 判断等使用的 token 估算，须与钉死版本 Pi **同一算法与常量**（移植其实现或共享等价逻辑），不允许「文档化偏差」。用黄金用例对拍。

### 5. 二进制：单文件部署，Wasm 打进主包

- 发布物为 **单一可执行文件**（优先静态/自包含，如 musl + rustls）  
- **Wasm 扩展运行时嵌入主二进制**（例如 wasmtime），用户无需另装 runtime  
- 扩展 Wasm 模块本身仍可按需从 `~/.pir` 加载；runtime 不外置  

### 6. Session：不做 Pi 路径迁移

- 仅使用 `~/.pir/agent/sessions/`（及配置的 sessionDir）  
- **不提供** `~/.pi` → `~/.pir` 自动/半自动迁移工具  
- JSONL **格式**仍与 Pi 对齐（便于有需要时手工拷贝）  

### 7. 第一版存储：仅 JSONL

- Session 后端第一版 **只做 JSONL**  
- SQLite / 其他后端 **不做**（直至另有 ADR）  

### 8. 可配置自有 Endpoint

版本检查、安装/更新 telemetry（及同类产品 HTTP 回调）须支持在 settings / 环境变量中配置 **自有 endpoint**，不硬编码仅官方 URL。未配置时可用合理默认或关闭。

LLM 的自定义 base URL / 兼容端点继续通过 `models.json` / provider 配置（与 Pi 同构），与本条产品 endpoint 分开。

### 9. 许可证

与 Pi 相同：**MIT**。

## 后果

- `external/pi` 应保持在钉死 commit；文档与 CI 校验 commit 哈希  
- 路线图中 TUI（原 M5）与扩展安装（M8）均为正式范围  
- 二进制体积会因嵌入 Wasm runtime 增大，属可接受权衡  
- Telemetry/更新默认策略需在 settings 中可关、可改 URL  
