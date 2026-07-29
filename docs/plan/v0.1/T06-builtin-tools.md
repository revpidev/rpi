# T06：内置四工具与 ToolContext

- **状态**：未开始
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

- `pir/src/tools/`：`read.rs`、`write.rs`、`edit.rs`、`edit_diff.rs`、`bash.rs`、`truncate.rs`、`file_mutation_queue.rs`、`path_utils.rs`、`output_accumulator.rs`、`bash_executor.rs`
- `ToolContext { cwd, signal, on_update, session_env }` 注入机制；可插拔 operations trait（ReadOperations/BashOperations 等，供扩展/沙箱改道）
- 公共截断（`truncate.rs`）：`DEFAULT_MAX_LINES=2000`、`DEFAULT_MAX_BYTES=50KB`、`GREP_MAX_LINE_LENGTH=500`；truncateHead 不截整行（首行超限 firstLineExceedsLimit）；truncateTail 末行可部分截断（UTF-8 边界感知）
- 行为锚点（需求 §4.5 表，逐项）：
  - read：文本/图像（jpg/png/gif/webp/bmp 魔数；**三条拒绝子规则**：JPEG SOF7 0xF7、PNG IDAT 前 acTL 即 APNG、BMP DIB 头校验（长度≥26、DIB size∈{12,40–124}、colorPlanes=1、bpp∈{1,4,8,16,24,32}）；识别失败按文本读取不报错，utils/mime.ts）；offset 1-indexed 越界报错；limit 先截取再 truncateHead；截断提示附 nextOffset；首行超 50KB 给 sed 回退提示；图像 autoResize 2000×2000；`@` 前缀剥离与**路径变体四类**尝试（① macOS 截图名空格→U+202F；② NFD；③ '→U+2019；④ NFD+弯引号组合，path-utils.ts:52-118）
  - write：utf-8；递归创建父目录；`Successfully wrote N bytes`
  - edit：`edits[]` 原始文件匹配、逆序应用（edits 为 JSON string 时 `JSON.parse` 还原数组——注释点名 Opus 4.6/GLM-5.1，edit.ts:101-107）；fuzzy 归一化全集（NFKC/行尾空白/智能引号/破折号/特殊空格）；唯一性在 fuzzy 空间校验；重叠/空 oldText/无变化错误文案；BOM/CRLF 保留；overlay 保留未改行原始字节；diff 上下文 4 行；legacy 参数 shim（`prepare_arguments` 路径）
  - bash：**无默认超时**（上限 2³¹−1 ms）；stdout+stderr 合流；tail 截断 2000 行/50KB、超量写 `tmpdir/pi-bash-<hex>.log`（滚动缓冲 2×50KB，`output_accumulator.rs`）；返回 LLM 的为原始解码文本（**控制字符清洗不在工具输出层**，只在 TUI 渲染层与用户 `!`/`!!` bash-executor：render-utils.ts:48、bash-executor.ts:82）；detached 进程组 + 杀进程树；onUpdate 100ms 节流；非零退出码抛错附输出；`shellPath`/`shellCommandPrefix`；`spawn_hook`；会话环境注入（仅 5 个 `PIR_*`、spawnHook 之前、未启用时删除继承 `PIR_*`）
  - `bash_executor.rs`：用户 `!`/`!!` 独立路径（非工具；滚动缓冲、超量临时文件、stripAnsi、无超时参数、**不注入会话变量**、`!!` → excludeFromContext）——RPC `bash` 命令（T10）与 interactive（T12）共用
  - file mutation queue：realpath 键（ENOENT 退化 resolve）；abort 不在事件回调里 reject
- 工具 schema（JSON Schema）与参数校验（复用 pir-ai 宽松强转）
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

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] 上游 tools 相关测试意图移植通过
- [ ] edit-diff 边界用例集（无匹配/多匹配/部分匹配/fuzzy 命中）与上游语义一致
- [ ] bash：流式 update 100ms 节流、tail 截断 + 临时文件、取消后进程组无残留（测试断言无僵尸进程）、环境注入 5 变量且用户 bash 不注入
- [ ] bash_executor：`!`/`!!` 路径 excludeFromContext 语义、输出清洗
- [ ] file mutation queue：并发 edit/write 串行化语义正确；abort 不撕裂队列
- [ ] 截断/超时/节流常量与上游逐值核对表（附验收记录）

## 门禁验收

通用门禁 G1–G7 全过。

任务特有标准：

- [ ] faux provider + 工具脚本场景（read 文件、bash 命令）事件序列与 fixtures 归一化 diff 一致
- [ ] `spawn_hook` 可替换 spawn 行为（测试用 hook 断言被调用）
- [ ] 需求 §4.5 表各锚点逐条核对有测试锚点

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
