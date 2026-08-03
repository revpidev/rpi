# D-014：settings 与资源加载移植 Rust 落地差异（锁 / 引擎级解析 / TUI 件下沉 / 占位边界）

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已回写
- **关联任务**：T09
- **级别**：实现细节偏离
- **发现日期**：2026-08-03

## 原文档约定

- 文档与章节：`docs/02-design.md` §6.7（ResourceLoader）、§12（模块映射表）、`docs/01-requirements.md` §3.3、§7.1–§7.5、§7.7、`docs/coding-standards.md` §9.2（fs2 锁）、附录 A（依赖基线）
- 原文约定：SettingsManager/ResourceLoader/skills/prompt-templates/themes/keybindings 按上游逐文件映射移植；锁用 fs2 对齐 proper-lockfile；serde_yaml/ignore 列入依赖基线。

## 实际实现与偏离原因

1. **SettingsManager 同步 API**：上游 Promise 写队列串行化 → setter 内联同步写盘，
   `flush()` 为 no-op；异步调用方须 `spawn_blocking` 包装（同 session_manager 先例）。
2. **锁实现差异**：fs2 flock 直接锁目标文件（替代 proper-lockfile 的 `.lock` sidecar）；
   文件不存在时的写路径先 `O_CREAT` 建空文件再上锁（内容不变）；竞争重试仅
   `WouldBlock`（对应 ELOCKED），10×20ms `thread::sleep`。keybindings 旧键迁移写回
   （上游 `migrations.ts:157-172` 无锁）按 G4「锁仅限 auth/settings/trust」加了同款
   fs2 锁；其锁/写 I/O 失败传播为 `PirError::Resource`（上游全部吞掉）。
3. **Settings 内部表示**：insertion-ordered `serde_json::Map`（JS 对象插入序/spread
   语义），未知键完整往返。文件中类型错误的值（如 `steeringMode: 5`）在 getter 回落
   默认/跳过，而非 JS 的原样透传/TypeError——有效配置行为一致。
4. **`trackingId` 的 `randomUUID()`**：`/dev/urandom`（unix）+ 时间/pid/计数兜底
   （依赖基线无 rand/uuid crate）；兜底非加密安全。
5. **引擎级解析差异（对拍已标注排除项）**：serde_yaml 与 JS yaml 的块标量 chomping
   （`|` 切片 EOF 尾换行）与语法错误文案不同；手写校验器与 TypeBox 的 "Other errors"
   措辞不同；serde_json 与 JS SyntaxError 的 JSON 解析错误文案不同。均只影响错误/
   边界文案，有效输入行为一致（fixtures/README.md §3.1 已登记排除口径）。
6. **prompt template description 截断按 Unicode scalar 计**（JS 按 UTF-16 code
   unit）：BMP 文本完全一致，astral 字符可能早截一个 char。分词/expand 正则显式采用
   JS `\s` 字符集（含 U+FEFF）——这是对齐而非偏离，备查。
7. **`PromptTemplate`/`Theme` 无 `sourceInfo` 字段**：上游 `isUnderPath` 作用域分类
   回调的唯一用途是喂 `createSyntheticSourceInfo`；provenance 归 resource_loader
   （skills 侧按 `findSourceInfoForPath` 前缀匹配实现，prompts/themes 侧留 getter
   给 T15）。
8. **system prompt 文档段落**：上游 `getReadmePath/getDocsPath/getExamplesPath` 锚在
   pi 的 package dir；pir 无捆绑 package docs 目录，改为
   `BuildSystemPromptOptions::doc_paths` 参数，`None` 时整段省略（T10 决定来源）。
9. **themes/keybindings 的 TUI 件下沉 T11/T12**：`detectCapabilities`/`matchesKey`
   （Kitty 协议解析）/全局 theme、keybindings 单例/`getMarkdownTheme` 等 helper、热
   重载 watcher 本体均未移植（依赖 TUI 运行时）；本任务交付纯数据/解析/检测逻辑
   （OSC 11 / CSI ?996n / CSI 16t / OSC 9;4 字节常量与响应解析、`get_theme_watch_path`）。
   内置 dark/light 主题为 const 字符串懒解析（值逐字一致）；`chalk` 样式用原生 ANSI
   转义；平台差异用 `cfg!(target_os)` 编译时判定。
10. **resource_loader 占位边界**：extensions 仅占位（路径解析+存在性检查，T14/T15
    替换）；packages 为输入端口（`PackageResourcePaths`，安装加载在 T14）；SDK
    override 钩子与 inline factories 未移植（T15）；上游 CLI 缺失路径诊断的不一致
    （skills/prompts 门控 `is_local_path`、themes 不门控）照原样移植。
11. **环境变量模块拆分**：`PIR_CODING_AGENT_DIR`/`PIR_CODING_AGENT_SESSION_DIR` 常量
    保留在 `config.rs`（§10.1 单点路径解析），`core/environment.rs` 持有其余进程级
    `PIR_*` 常量与读取辅助；bash 5 变量注入沿用 T06 既有接线。

## 影响面

无（纯内部与错误文案级）。不改变 settings/themes/keybindings JSON 形状、发现顺序、
rank、注入格式字节或任何对拍契约；资源对拍（`parity_resources_test.rs` 7 组黄金）
全过，引擎级文案差异已在 fixtures 口径中登记排除。

## 处置

- **回写位置**：`docs/02-design.md` §6.7（Rust 落地注记）、§12（映射表
  resource-loader/skills/prompt-templates/system-prompt/themes 行）
- **回写日期**：2026-08-03
- **ADR**：不需要
