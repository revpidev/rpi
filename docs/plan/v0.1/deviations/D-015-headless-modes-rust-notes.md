# D-015：headless 模式移植 Rust 落地差异（手写 CLI 解析器 / T13 子集 / 占位边界 / 路径与排序确定性）

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已回写
- **关联任务**：T10
- **级别**：实现细节偏离
- **发现日期**：2026-08-04

## 原文档约定

- 文档与章节：`docs/02-design.md` §6.1（启动管线）、§6.6（AgentSessionRuntime）、§12（模块映射表）、`docs/01-requirements.md` §2.2–§2.5、§3.1–§3.3、`docs/plan/v0.1/T10-headless-modes.md`（设计细化）
- 原文约定：CLI 解析（clap）；provider 清单 38 工厂 + 远程 catalog；启动管线按 main.ts 逐段移植；SessionManager::list 属 T12；session 环境变量注入沿用 T06。

## 实际实现与偏离原因

1. **clap → 手写解析器**（`pir/src/cli/args.rs`）：clap 无法表达上游 `args.ts` 的三类语义——
   `-p` 值吞噬规则、未知 `--flag` 收集为扩展标志透传（`extensionFlagValues` + help 动态段）、
   互斥诊断矩阵（`--fork`/`--session-id` 等）。改为与上游同构的手写扫描器，
   `args.test.ts` 84 测试全量移植锚定行为。
2. **provider 生态为 T13 子集**：provider-composer、远程 models catalog、38 内置 provider
   工厂整体在 T13；T10 的 ModelRuntime 仅提供组合点（`register_provider`）与
   auth.json/models.json/RuntimeCredentials 覆盖层。
3. **交互件与 T14 子命令占位**：`--resume` 交互 picker（T12）、install/remove/list/update/config
   子命令分流（T14）、`--export` HTML 实现（T14）均留占位——app 入口可识别并给出
   「未实现」诊断，参数形状已锁定。
4. **docs 路径 = 可执行文件目录**：上游 `getDocsPath` 锚在 pi 的 package dir（npm 包随捆
   用户文档）；pir 无捆绑 package docs，system prompt 的 docs 段取 exe dir 探测，
   缺失时整段省略（延续 D-014 §8 的 `doc_paths` 参数口径）。
5. **`ToolContext.session_env` 动态 cell**：改为 `Option<Arc<RwLock<SessionEnv>>>`，
   bash 工具的 `PIR_*` 环境变量在每次 spawn 时动态解析（需求 §3.3 要求 session 切换后
   新值生效），而非 T06 最初的快照注入。
6. **资源枚举确定性排序**：skills/prompts 的资源枚举按路径排序——JS `fs.readdir` 序
   在 Linux 上不稳定，导致 `parity_resources_test` e2e 偶发失败；排序后输出确定性
   （内容集合不变）。
7. **SessionManager::list 提前到 T10**：`list/list_all/build_session_info/SessionInfo`
   原规划在 T12；`--session` 三级解析（本项目 id → 全局跨项目）需要会话发现能力，
   提前实现。

## 影响面

无（纯内部）。CLI 标志语义、diagnostics 分级、JSONL 会话格式、RPC 帧格式均以测试/对拍
锚定与上游一致；第 6 项仅影响枚举输出顺序的确定性（内容集合不变），第 7 项为内部 API
提前，不改变任何对外契约。

## 处置

- **回写位置**：`docs/02-design.md` §6.1（启动管线落地注记 + 手写解析器）、§6.3（list/listAll 提前 T10）、§6.6（Modes 落地注记）、§12（映射表 main.ts/sdk/model-runtime/modes/rpc-entry 行）
- **回写日期**：2026-08-04
- **ADR**：不需要
