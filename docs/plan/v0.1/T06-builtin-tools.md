# T06：内置四工具与 ToolContext

- **状态**：已完成
- **里程碑**：M2
- **依赖**：T05
- **上游对照**：`packages/coding-agent/src/core/tools/{read,write,edit,edit-diff,bash,truncate,file-mutation-queue,path-utils,output-accumulator,bash-executor}.ts`（**行为基准，ADR-0003 §2**）
- **需求章节**：§4.5
- **预估**：0.7–0.8 人月（M2 共 2–2.4，与 T05 合计）

---

## 目标

实现默认启用的四个内置工具（read / write / edit / bash）及其支撑设施，
行为锚点（含全部常数）与上游 coding-agent 实现对齐，可由 agent loop 驱动完成对拍场景。

## 范围

### In

- `rpi/src/tools/`：`read.rs`、`write.rs`、`edit.rs`、`edit_diff.rs`、`bash.rs`、`truncate.rs`、`file_mutation_queue.rs`、`path_utils.rs`、`output_accumulator.rs`、`bash_executor.rs`
- `ToolContext { cwd, signal, on_update, session_env }` 注入机制；可插拔 operations trait（ReadOperations/BashOperations 等，供扩展/沙箱改道）
- 公共截断（`truncate.rs`）：`DEFAULT_MAX_LINES=2000`、`DEFAULT_MAX_BYTES=50KB`、`GREP_MAX_LINE_LENGTH=500`；truncateHead 不截整行（首行超限 firstLineExceedsLimit）；truncateTail 末行可部分截断（UTF-8 边界感知）
- 行为锚点（需求 §4.5 表，逐项）：
  - read：文本/图像（jpg/png/gif/webp/bmp 魔数；**三条拒绝子规则**：JPEG SOF7 0xF7、PNG IDAT 前 acTL 即 APNG、BMP DIB 头校验（长度≥26、DIB size∈{12,40–124}、colorPlanes=1、bpp∈{1,4,8,16,24,32}）；识别失败按文本读取不报错，utils/mime.ts）；offset 1-indexed 越界报错；limit 先截取再 truncateHead；截断提示附 nextOffset；首行超 50KB 给 sed 回退提示；图像 autoResize 2000×2000；`@` 前缀剥离与**路径变体四类**尝试（① macOS 截图名空格→U+202F；② NFD；③ '→U+2019；④ NFD+弯引号组合，path-utils.ts:52-118）
  - write：utf-8；递归创建父目录；`Successfully wrote N bytes`
  - edit：`edits[]` 原始文件匹配、逆序应用（edits 为 JSON string 时 `JSON.parse` 还原数组——注释点名 Opus 4.6/GLM-5.1，edit.ts:101-107）；fuzzy 归一化全集（NFKC/行尾空白/智能引号/破折号/特殊空格）；唯一性在 fuzzy 空间校验；重叠/空 oldText/无变化错误文案；BOM/CRLF 保留；overlay 保留未改行原始字节；diff 上下文 4 行；legacy 参数 shim（`prepare_arguments` 路径）
  - bash：**无默认超时**（上限 2³¹−1 ms）；stdout+stderr 合流；tail 截断 2000 行/50KB、超量写 `tmpdir/pi-bash-<hex>.log`（滚动缓冲 2×50KB，`output_accumulator.rs`）；返回 LLM 的为原始解码文本（**控制字符清洗不在工具输出层**，只在 TUI 渲染层与用户 `!`/`!!` bash-executor：render-utils.ts:48、bash-executor.ts:82）；detached 进程组 + 杀进程树；onUpdate 100ms 节流；非零退出码抛错附输出；`shellPath`/`shellCommandPrefix`；`spawn_hook`；会话环境注入（仅 5 个 `RPI_*`、spawnHook 之前、未启用时删除继承 `RPI_*`）
  - `bash_executor.rs`：用户 `!`/`!!` 独立路径（非工具；滚动缓冲、超量临时文件、stripAnsi、无超时参数、**不注入会话变量**、`!!` → excludeFromContext）——RPC `bash` 命令（T10）与 interactive（T12）共用
  - file mutation queue：realpath 键（ENOENT 退化 resolve）；abort 不在事件回调里 reject
- 工具 schema（JSON Schema）与参数校验（复用 rpi-ai 宽松强转）
- 工具开关：`--tools`/`-t` allowlist、`--exclude-tools`/`-xt` denylist（deny 后于 allow）、`--no-tools`/`--no-builtin-tools` 的底层能力（CLI 接线在 T10）；默认激活集 `["read","bash","edit","write"]`

### Out

- 可选工具 grep / find / ls（T14，Rust 原生实现）
- 扩展工具注册与同名覆盖（T15）
- `!` / `!!` 的 interactive 交互接线（T12）

## 开发要点

- `edit-diff` 算法逐语义移植，边界用例（无匹配、多匹配、模糊匹配规则）逐项对照上游测试
- bash 子进程以进程组管理，取消时整组终止（编码规范 §11.3）
- 截断/超时/节流常数与上游逐值核对（对拍可见）
- 工具输出截断、错误返回形状与上游对齐

## 进度跟踪

- [x] 设计细化
- [x] 实现
- [x] 自测
- [x] 门禁验收
- [x] 文档回写

## 自测清单

- [x] 上游 tools 相关测试意图移植通过（tools.test.ts 的 read/write/edit/bash 用例意图、path-utils.test.ts、edit-tool-legacy-input.test.ts、file-mutation-queue.test.ts、harness/truncate.test.ts 全部移植，snake_case 同名锚定）
- [x] edit-diff 边界用例集（无匹配/多匹配/部分匹配/fuzzy 命中）与上游语义一致（`tests/edit_diff.rs` 53 例 + `tests/edit.rs` fuzzy/CRLF 18 例）
- [x] bash：流式 update 100ms 节流（`test_coalesce_chatty_output`：5000 行更新数 < 25）、tail 截断 + 临时文件（`test_line_truncation_writes_temp_file`）、取消后进程组无残留（`test_no_zombie_after_cancel`）、环境注入 5 变量且用户 bash 不注入（`test_rpi_session_env_injected` / `test_rpi_env_stripped_when_no_session` / `test_expose_session_env_false`；bash_executor 路径不经注入逻辑）
- [x] bash_executor：`!`/`!!` 路径输出清洗（`test_ansi_stripped` / `test_carriage_return_removed`）、超量临时文件、abort → `cancelled: true`；`excludeFromContext` 为 `bashExecution` 消息组装层语义（T10/T12 接线），本任务交付独立执行路径与清洗
- [x] file mutation queue：并发 edit/write 串行化语义正确（`test_serializes_same_file` / `test_parallel_different_files` / `test_symlink_alias_same_queue` + `tests/write.rs` 共享队列用例）；abort 不撕裂队列（write/edit abort 用例）
- [x] 截断/超时/节流常量与上游逐值核对表（见验收记录 §G3 附表）

## 门禁验收

通用门禁 G1–G7 全过。

任务特有标准：

- [x] faux provider + 工具脚本场景（read 文件、bash 命令）事件序列与 fixtures 归一化 diff 一致（`crates/rpi/tests/parity_tools_test.rs`，真实 read/bash 工具；连续 5 次运行稳定）
- [x] `spawn_hook` 可替换 spawn 行为（`test_spawn_hook_called` / `test_spawn_hook_can_modify_env` 断言 hook 被调用并可改写 command/env）
- [x] 需求 §4.5 表各锚点逐条核对有测试锚点（见验收记录锚点映射表）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| D-011 | 内置工具层 Rust 落地差异（ToolContext 形状、image/kamadak-exif 替代 Photon、自实现 Myers diff、OutputAccumulator 同步 API、on_data Vec<u8>、trackDetachedChildPid 未移植等 12 项） | 已回写 |

## 验收记录

- 验收日期：2026-08-01
- 验收人：kimi-code（单人开发，按清单逐项自证）
- G1 构建/静态检查：通过（`cargo build --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check` 全绿，无警告）
- G2 测试：通过（workspace 合计 679 passed, 0 failed；其中 rpi crate 232 例：lib 99 + edit_diff 53 + edit 41 + bash 27 + write 8 + parity_tools 1 + writeup 等；无 live 测试）
- G3 对拍：通过。`cargo test -p rpi --test parity_tools_test`：真实 read（临时目录 note.txt）+ 真实 bash（spawn echo）对 `fixtures/generated/tool-calls/events.jsonl`，归一化粒度与 T05 一致（`message_update` 整类排除、`usage`/`willRetry`/`details` 键排除、timestamp/id 归一化），行序敏感 diff 一致，连续 5 次运行稳定。read 侧包 30ms 延迟 operations 以钉死并行完成序 [bash, read]（fixture 实录时序，测试内注释说明）
- G4 红线：通过（`external/pi` 无改动、HEAD=2efa728；无 JS/TS 执行能力；未读写 `~/.pi`/`.pi`（getShellEnv 前置 `~/.rpi/bin` 为 ADR-0001 配置根）；Session 仍仅 JSONL；token 估算未触碰；非测试代码无 unwrap/expect（3 处带不变式注释的 expect：Myers 解存在性、正则编译、newline 扫描不变式）；日志无凭据；无范围排除项引入；无 rg/fd 下载机制；session 写入无文件锁）
- G5 线格式：通过（`TruncationResult` serde camelCase 有 `test_truncation_result_serde_camel_case` 锚定；工具 details 键 `truncation`/`fullOutputPath`/`diff`/`patch`/`firstChangedLine` camelCase，`firstChangedLine: None` 省略对齐 JS undefined）
- G6 文档同步：通过（全部移植文件带英文溯源注释；回写 `02-design.md` §6.5 Rust 落地注记、`coding-standards.md` 附录 A 依赖基线新增 4 行）
- G7 偏离闭环：通过（D-011 已登记并回写，状态「已回写」）
- 结论：通过

### G3 附表 1：常数逐值核对表

| 常数 | 上游值（出处） | rpi 值（出处） | 一致 |
|------|----------------|----------------|------|
| DEFAULT_MAX_LINES | 2000（truncate.ts:11） | 2000（truncate.rs:14） | ✓ |
| DEFAULT_MAX_BYTES | 50*1024=51200（truncate.ts:12） | 51200（truncate.rs:17） | ✓ |
| GREP_MAX_LINE_LENGTH | 500（truncate.ts:13） | 500（truncate.rs:20） | ✓ |
| MAX_TIMEOUT_MS | 2³¹−1=2147483647（bash.ts:24） | 2147483647（bash.rs:25） | ✓ |
| MAX_TIMEOUT_SECONDS | 2147483.647（bash.ts:25） | 2147483.647（bash.rs:26） | ✓ |
| BASH_UPDATE_THROTTLE_MS | 100（bash.ts:200） | 100（bash.rs:27） | ✓ |
| EXIT_STDIO_GRACE_MS | 100（child-process.ts:16） | 100（bash.rs:28） | ✓ |
| bash-executor maxOutputBytes | DEFAULT_MAX_BYTES*2=102400（bash-executor.ts:58） | 102400（bash_executor.rs:53） | ✓ |
| IMAGE_TYPE_SNIFF_BYTES | 4100（mime.ts:3） | 4100（mime.rs:8） | ✓ |
| 滚动缓冲 maxRollingBytes | max(maxBytes*2,1)=102400（output-accumulator.ts:60） | 102400（output_accumulator.rs:43） | ✓ |
| 滚动 trim 阈值 | maxRollingBytes*2（output-accumulator.ts:157） | 同（output_accumulator.rs） | ✓ |
| 图像 autoResize | 2000×2000（image-resize-core.ts:25-26） | 2000×2000（image_process.rs:24-25） | ✓ |
| 图像 maxBytes | 4.5MB=4718592（image-resize-core.ts:7） | 4718592（image_process.rs:29） | ✓ |
| jpegQuality / 质量梯度 | 80 / [80,85,70,55,40]（image-resize-core.ts:28,122） | 80 / [80,85,70,55,40]（image_process.rs:32,319） | ✓ |
| 尺寸回退步进 | ×0.75（image-resize-core.ts:146-154） | ×0.75（image_process.rs:369-378） | ✓ |
| diff 上下文行 | 4（edit-diff.ts:369,383） | 4（edit.rs 调用点 / edit_diff.rs） | ✓ |
| 临时文件名 | `tmpdir/pi-{bash,output}-<8字节hex>.log` | 同（output_accumulator.rs / bash_executor.rs，16 位 hex） | ✓ |
| 会话环境变量 | PI_* 5 个（bash.ts:166-181） | RPI_* 5 个（ADR-0001 有意重命名；删除→条件注入→spawnHook 顺序一致） | ✓（有意差异） |

### G3 附表 2：需求 §4.5 锚点 → 测试锚点映射

| 需求 §4.5 锚点 | 测试锚点 |
|----------------|----------|
| read：魔数识别 + 三条拒绝子规则（JPEG SOF7 / PNG acTL / BMP DIB） | `tools::mime::tests`（14 例：各格式正例 + SOF7/APNG/DIB 拒绝） |
| read：识别失败按文本读取 | `tools::read::tests`（.png 后缀文本按文本处理） |
| read：offset 1-indexed 越界报错文案 | `tools::read::tests`（offset 越界 `Offset 100 is beyond end of file (3 lines total)`） |
| read：limit 先截取再 truncateHead；截断提示附 nextOffset | `tools::read::tests`（`[Showing lines 1-2000 of 2500. Use offset=2001 to continue.]`、`[90 more lines in file. Use offset=11 to continue.]`、offset+limit） |
| read：首行超 50KB sed 回退提示 | `tools::read::tests`（首行超限 sed 提示） |
| read：图像 autoResize 2000×2000 / BMP→PNG 转换 hint | `tools::image_process::tests`（缩放维度 hint、BMP→PNG） |
| read：`@` 前缀剥离 + 路径变体四类 | `tools::path_utils::tests`（macOS AM/PM 大小写、NFD、弯引号、NFD+弯引号组合） |
| write：utf-8、递归父目录、`Successfully wrote N bytes`（UTF-16 计数） | `tests/write.rs`（写入成功文案、UTF-16 字节数、父目录递归、覆盖） |
| edit：edits 对原始文件匹配、非增量 | `tests/edit.rs`（`test_edits_match_original_not_incremental` 等） |
| edit：edits 为 JSON string 时解析（Opus 4.6/GLM-5.1 shim） | `tests/edit.rs` legacy shim 8 例（JSON string 解析、无效 JSON 保留、顶层 oldText/newText 折叠、schema 不含 legacy 字段） |
| edit：fuzzy 归一化全集（NFKC/行尾空白/智能引号/破折号/特殊空格） | `tests/edit_diff.rs` + `tests/edit.rs` fuzzy 12 例（全角标点、兼容性等价、智能单双引号、破折号、NBSP、行尾空白） |
| edit：唯一性 fuzzy 空间校验；重叠/空 oldText/无变化文案 | `tests/edit_diff.rs`（duplicate 单/多、overlap、empty oldText 单/多、no change 单/多） |
| edit：BOM/CRLF 保留；overlay 保留未改行原始字节 | `tests/edit.rs` CRLF/BOM 6 例；`tests/edit_diff.rs`（preserve untouched lines、preserve correct occurrence） |
| edit：diff 上下文 4 行、折叠；patch 形状 | `tests/edit_diff.rs`（600 行折叠、first_changed_line、unified patch、`\ No newline at end of file`） |
| edit：access 错误文案（ENOENT/EACCES/通用） | `tests/edit.rs`（ENOENT、EACCES、`Error: disk offline.`） |
| bash：无默认超时、上限 2³¹−1、校验文案 | `test_timeout`（sleep 5 + timeout 1）、`test_invalid_timeout_validation`（两条文案逐字） |
| bash：stdout+stderr 合流 | `test_stdout_stderr_merged` |
| bash：tail 截断 2000 行/50KB + 超量临时文件；尾换行不计额外行 | `test_line_truncation_writes_temp_file`、`test_trailing_newline_not_extra_line` |
| bash：返回 LLM 原始文本不清洗（与 executor 对比） | `test_ansi_stripped`（executor 清洗）+ bash 路径 OutputAccumulator 原始累积（无清洗调用，源码注释锚定 bash.ts:659） |
| bash：detached 进程组 + 杀进程树；取消无残留 | `test_no_zombie_after_cancel`、`test_aborted_command` |
| bash：onUpdate 100ms 节流合并 | `test_coalesce_chatty_output`（5000 行 < 25 次更新） |
| bash：非零退出码抛错附输出 | `test_exit_1_error`（`Command exited with code 1`） |
| bash：shellPath / shellCommandPrefix / spawn_hook | `test_spawn_error_bad_shell`、`test_command_prefix`/`test_no_prefix`、`test_spawn_hook_called`/`test_spawn_hook_can_modify_env` |
| bash：会话环境注入 5 RPI_*（先删后注、未启用清除继承） | `test_rpi_session_env_injected`、`test_rpi_env_stripped_when_no_session`、`test_expose_session_env_false` |
| bash-executor：滚动缓冲 2×50KB、超 50KB 临时文件、stripAnsi+清洗+去 \r、无超时、不注入 RPI_* | `test_large_output_writes_temp_file`、`test_ansi_stripped`、`test_carriage_return_removed`、`test_abort_returns_cancelled`（无 timeout 参数、不经注入路径——源码形状锚定） |
| file mutation queue：realpath 键（symlink alias 同队列）、串行/并行、abort 不撕裂 | `tools::file_mutation_queue::tests`（serializes/parallel/symlink/gc）+ `tests/write.rs`（共享队列、abort） |
| 截断公共：truncateHead 不截整行/首行超限；truncateTail 末行部分截断 UTF-8 边界感知 | `tools::truncate::tests`（17 例，移植 harness/truncate.test.ts 意图） |
| 工具开关：allowlist/denylist（deny 后于 allow）/no-tools/默认激活集 | `tools::wiring_tests`（5 例） |
