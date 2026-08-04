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
   `args.test.ts` 移植测试锚定行为（上游 72 个 `test(` + 3 个补充 = 75；本文档早前
   写的「84」系笔误，已更正）。
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

## 二次审查补登（2026-08-04）

T10 复审（5 路并行对拍）发现的偏离已修复；以下为修复后**仍然保留**的已知偏离，
补登备查：

8. **`--wasm-smoke`**（`pir/src/main.rs`）：Rust 独有的 wasmtime 冒烟钩子（T02 测量用，
   注释标明 T15 移除）；上游无此标志。参数解析前拦截，任意位置出现即生效。
9. **stdin 阻塞读取**（`app.rs::read_piped_stdin`）：在 async 上下文中同步
   `read_to_string`；上游为事件驱动。multi-thread runtime 下只阻塞当前 worker，
   headless 流程无可观察差异，保留。
10. **SIGHUP 直退**（`modes/print_mode.rs`）：信号处理器在独立线程 `exit(129)`，
    跳过上游的 `killTrackedDetachedChildren`/`disposeRuntime`；stdout 为行缓冲，
    实际丢失风险仅限进程内未写完的缓冲。
11. **RPC 字段形状边界从严**：已知命令的字段类型错误在 serde 边界拒绝
    （`Invalid command: …`），上游不做校验、在 handler 内以 TypeError 失败——
    `success:false` 形状一致，错误文案不同（实现内有注释，此处正式登记）。
12. **模型级 headers 组合期固化**：models.json 的模型 `headers` 与
    `modelOverrides.*.headers` 在组合期并入 `Model.headers`（上游
    `modelFromJson` 置 `headers: undefined`，请求期经 `resolveConfiguredModelHeaders`
    合并）。请求期结果等价，差异仅在于 `Model` 对象上可观察到 headers 字段。

### 二次审查已修复项（备查）

models.json provider `headers`/`authHeader` 全链路丢失（改为组合期包装 auth
resolve）；模型缺省 `contextWindow`/`maxTokens` 由 0 更正为 128000/16384 且缺
api/baseUrl 改为组合错误；`modelOverrides` 生效；provider 枚举序改插入序；
`get_error` 补 availability 错误；`setRuntimeApiKey`/`removeRuntimeApiKey` 末段改走
`refresh()`；`dispose()` 补 `_disconnectFromAgent`；`wait_for_idle` 修丢失唤醒竞态；
sdk 恢复路径 `expect` panic（无 model_change 条目的会话）；trust 锁错误吞掉与锁文件名
（`trust.json.lock`）；RPC 空行回 parse 错误、未知命令 `command` 字段形状、
`get_commands` 改用当前会话 cwd；print 错误文案剥 `session error: ` 前缀；fork 确认
提示与 `Aborted.` 改落 stderr；`is_local_path` 补 `github:` 且按 `http:`/`https:`/`ssh:`
前缀判定；`--session ""`/`--fork ""`/`--api-key ""` 空串按 falsy 处理；工具激活序不再
按字母排序（保序去重）；agent-loop 内建工具接入 settings
（`shellCommandPrefix`/`shellPath`/`imageAutoResize`）；compaction abort 经共享
CancellationToken 在 in-flight 时仍生效；`is_context_overflow` 对 0 窗口按 JS falsy
处理；`get_session_stats` 省略空 `sessionFile`；`cycle_thinking_level` 未命中语义；
`bind_extensions` on_error 退订句柄不再 `mem::forget`；help 动态段对齐；
`format_token_count` 半数舍入；`minimatch` 子集三项语义（`?`/`[...]` 不跨 `/`、
非独立段 `**` 退化为 `*`，并补 `a/**/b` 匹配 `a/b`）。

## 影响面

无（纯内部）。CLI 标志语义、diagnostics 分级、JSONL 会话格式、RPC 帧格式均以测试/对拍
锚定与上游一致；第 6 项仅影响枚举输出顺序的确定性（内容集合不变），第 7 项为内部 API
提前，不改变任何对外契约。

## 处置

- **回写位置**：`docs/02-design.md` §6.1（启动管线落地注记 + 手写解析器）、§6.3（list/listAll 提前 T10）、§6.6（Modes 落地注记）、§12（映射表 main.ts/sdk/model-runtime/modes/rpc-entry 行）
- **回写日期**：2026-08-04
- **ADR**：不需要
