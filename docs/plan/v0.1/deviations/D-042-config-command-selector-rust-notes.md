# D-042：config 子命令与 config-selector 真接线 Rust 落地差异

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已关闭
- **关联任务**：T14（W3：`pir config` 子命令）
- **级别**：实现细节偏离
- **发现日期**：2026-08-06

## 原文档约定

- 文档与章节：`docs/01-requirements.md` §3.2（子命令：config TUI Tab 切 scope；`-l` 要求 trust）；`docs/plan/v0.1/T14-packages-trust-export.md` W3 契约要点
- 原文约定：`pir config` 打开资源配置 TUI，Tab 切 User/Project scope，编辑 settings 项；`-l` 直接进 project 写 scope 且要求项目信任

## 实际实现与偏离原因

T12 交付的 `config_selector.rs` 以 `LoadedResources` 为输入、toggle 走内存态 + 写钩子（组件头注释自承「settings 写入是缺口，由 T14 接线」）。W3 将该组件重写为上游数据模型并补齐持久化，`pir config` 落 `crates/pir/src/cli/config_command.rs`：

1. **输入换成 `ScopedResolvedPaths`**（package manager 全量 `resolve_all` 输出：package + top-level settings 条目 + auto-discovered，含 `enabled` 与完整 `PathMetadata`），取代 T12 的 `LoadedResources` + 路径位置推断元数据。T12 的推断分支（`infer_metadata`）删除。
2. **持久化逐函数移植**（config-selector.ts:516-863）：global toggle 写 settings 数组的 `+/-pattern`（top-level）或包条目 object 化 + filter 数组（package）；project scope 三态循环（inherit/load/unload）按上游 `setProjectTopLevelOverride` / `setProjectPackageOverride` 写 project settings，含 `autoload:false` 占位条目的创建与回收。组件直接持有 `Arc<Mutex<SettingsManager>>`（上游同步读写同一实例）。
3. **项目视图解析用同文件新建的受信 SettingsManager**（上游复用命令的 manager 实例调 `resolve()`；`resolve` 只读，二者等价——Rust 所有权下 `DefaultPackageManager` 接管 manager）。
4. **上游 quirk 保留**：`tui.select.cancel` 默认绑定 `escape` + `ctrl+c`，使 config-selector.ts:491 的显式 ctrl+c → `onExit` 分支成为不可达死代码（ctrl+c 实际走 onCancel）；移植保留该行为并留测试锚点。
5. **`onToggle`/`onSwitchMode` 钩子删除**（上游仅为 requestRender；组件自渲染一致）。T12 引入的 `on_toggle(scope, name, enabled)` / `on_scope_change(scope)` 本地钩子随持久化落地一并移除。
6. **TUI 驱动复用 session-picker 模式**：`Tui::with_options` + 泵线程 + oneshot 关闭通道；主题从 settings 加载、dark 回退；不启动 theme watcher（session-picker 先例）。终端可注入（`TestTerminal` 驱动测试）。上游 `handleConfigCommand` 末尾 `process.exit(0)` → 关闭/退出路径均返回 exit code 0。
7. **排序用码位比较**（upstream `localeCompare`，D-039 先例）；`relative` 路径用 T09 `lexical_relative`（linux 下与上游一致）。

## 影响面

TUI 行为：与上游一致（含上述死代码 quirk）。settings 线格式（pattern 数组、包 object 形态、camelCase 字段）逐字段对齐上游。

## 处置

- **回写位置**：`docs/01-requirements.md` §3.2（追加实现注记）；`docs/plan/v0.1/T14-packages-trust-export.md` 偏离表
- **回写日期**：2026-08-06
- **ADR**：不需要

## 审查修复补记（2026-08-07 审查修复波次）

1. **项目 scope 写错误表面化**：`set_top_level_paths` / `write_packages` 原以
   `let _ =` 吞掉项目 setter 错误（trust 门禁内理论上不可达，但 `.pir/settings.json`
   已损坏或存储 IO 失败时 toggle 会静默不持久化）。现两函数返回 `Result`，项目写失败
   时 `eprintln!` 一行诊断并返回 false——toggle 不翻转、内存态与落盘一致（沿用既有
   `None` 语义，等价上游 throw）。用户 scope 写恒不可失败，语义不变。
2. **settings 写原子化**：`FileSettingsStorage::with_lock` 的 `std::fs::write` 改为
   `config::atomic_write`（同目录临时文件 + rename）；崩溃中截断的 settings.json
   会把后续所有读带进 Warning 静默路径（配合第 1 条的错误面）。线格式与锁语义不变。
