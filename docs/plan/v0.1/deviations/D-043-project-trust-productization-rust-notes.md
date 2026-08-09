# D-043：Project trust 产品化收尾（优先级链 / 启动弹窗）Rust 落地差异

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已关闭
- **关联任务**：T14（W4：Project trust 产品化收尾，需求 §7.8）
- **级别**：实现细节偏离
- **发现日期**：2026-08-07

## 原文档约定

- 文档与章节：`docs/01-requirements.md` §7.8；`docs/plan/v0.1/T14-packages-trust-export.md` W4 契约要点
- 原文约定：解析优先级链 CLI override > 扩展 `project_trust` 事件 > trust.json > defaultProjectTrust（ask→有 UI 弹窗、无 UI 返回 false）；启动两阶段加载；`/trust` 只写不重载

## 实际实现与偏离原因

T10 已移植 trust-manager + 非交互半边，W4 补齐优先级链与启动弹窗接线，落地差异：

1. **`ProjectTrustContext` 闭包化**（extensions/types.ts:525-530）：上游带 `ui.{select,confirm,input,notify}` 四面方法，信任流程只用 `select`，端口只保留 `has_ui: bool` + `select: Option<Box<dyn FnMut(&str, &[String]) -> Option<String> + Send>>`；`ProjectTrustContext::headless()` 为无 UI 形态。
2. **`resolve_project_trusted` 保持同步**：trust store 本身同步（上游 proper-lockfile 处 busy-sleep 也不改调用方为 async，trust-manager.ts:156-159）；异步的扩展事件发射由调用方先行完成，结果经 `extension_event: Option<ProjectTrustEventResult>` 参数传入（优先级位置在 override 之后、trust.json 之前，project-trust.ts:54-70）。`ExtensionRunner::emit_project_trust` 已加入 trait（默认 None：首个 yes/no 胜、undecided 落空语义注释在案），T15 宿主落地时接通。
3. **启动信任弹窗**（`showStartupSelector`，startup-ui.ts:134-164）落 `modes/interactive/startup_ui.rs::run_startup_selector`：复用 T12 的 `ExtensionSelectorComponent`，同步阻塞等结果通道 + 泵线程驱动（信任存储调用方同步，见上）；包来源启动主题（loadStartupThemes）不移植（首启 setup 同例）；`clearStartupTui` 的 25ms 重绘延迟省略；主题经 `resolve_theme_setting` + env 检测，无 OSC 二次探测。hasUI 判定对齐 main.ts:608/654：`isInitialRuntime && appMode == interactive && !help && list_models.is_none()`。
4. **弹窗阻塞 tokio worker**：同步 select 闭包在用户决策期间阻塞当前执行线程；启动流程顺序执行、无并发依赖，可接受。
5. **`getProjectTrustOptions` 落核心层**（trust-manager.ts:59-95 五选项+updates）：新增 `get_project_trust_options` / `get_project_trust_parent_path` / `ProjectTrustOption{,Update}`；T12 的 `commands_selectors.rs::build_trust_options` 自有选项组装保留不动（标签与上游逐字一致，value 编码差异属 T12 既有形态），启动弹窗走核心层端口。
6. **弹窗文案品牌替换**：`formatProjectTrustPrompt` 的 "This allows pi to load…" → "rpi"（与既有 warning 文案的 rpi 自称一致）。

**遗留（已析出为 D-044，行为级，ADR-0006）**：上游 `switchSession` 的 `projectTrustContextFactory`（interactive-mode.ts:4816/4830）未接线——交互模式内 resume 到**不同 cwd** 的会话时信任判定走 headless（ask→false，随后渲染既有的 untrusted warning）；同 cwd 重建命中 `trust_by_cwd` 缓存与上游一致。`CreateRuntimeOptions.project_trust_context` 字段已留口子，T15 接线后关闭 D-044。

## 影响面

TUI 行为：启动信任弹窗与上游一致（5 选项、ESC 取消=不信任、remember/父目录清除语义逐项测试锚定）。trust.json 线格式不变（camelCase 无关——纯路径→bool 映射）。

## 处置

- **回写位置**：`docs/plan/v0.1/T14-packages-trust-export.md` 偏离表
- **回写日期**：2026-08-07
- **ADR**：不需要

## 审查修复补记（2026-08-07 审查修复波次）

1. **trust.json 写原子化**：`write_trust_file` 改 `config::atomic_write`（同目录临时
   文件 + rename）；崩溃中截断的 trust.json 会使信任门禁硬失败（read 抛错），原子写
   消除该窗口。线格式（排序、pretty、尾换行）不变。
2. **lockfile 语义对齐 proper-lockfile**：重试仅限 `WouldBlock`（原任意错误都重试，
   等价 `ELOCKED` 判定）；释放时删除 `.lock` 文件（原遗留空锁文件）。
3. **key 排序改 UTF-16 code unit**：JS `sort()` 按 UTF-16 码元序（原码位序，仅
   emoji 等增补平面字符路径有差异）；新增 `utf16_code_unit_cmp`。
4. **`home_dir()` 回退 passwd 条目**：`HOME` 未设或为空时经 `getpwuid_r` 取
   `/etc/passwd`（上游 `process.env.HOME || homedir()`）；`~/.agents/skills` 豁免
   在无 HOME 环境下不再算出空路径。
