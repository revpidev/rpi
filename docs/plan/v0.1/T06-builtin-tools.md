# T06：内置四工具与 ToolContext

- **状态**：未开始
- **里程碑**：M2
- **依赖**：T05
- **上游对照**：`packages/coding-agent/src/core/tools/{read,write,edit,edit-diff,bash,truncate,file-mutation-queue,path-utils}.ts`
- **需求章节**：§4.4
- **预估**：0.5–0.7 人月（M2 共 1.5–2，与 T05 合计）

---

## 目标

实现默认启用的四个内置工具（read / write / edit / bash）及其支撑设施，
行为锚点与上游对齐，可由 agent loop 驱动完成对拍场景。

## 范围

### In

- `pir/src/tools/`：`read.rs`、`write.rs`、`edit.rs`、`edit_diff.rs`、`bash.rs`、`truncate.rs`、`file_mutation_queue.rs`、`path_utils.rs`
- `ToolContext { cwd, signal, on_update, session_env }` 注入机制
- 行为锚点（需求 §4.4 表）：
  - read：文本/图像、行范围、截断策略
  - write：创建/覆盖、目录创建
  - edit：精确替换 / `edit-diff` 算法语义移植、文件 mutation queue
  - bash：流式输出、截断、取消（进程组终止）、session 环境变量注入、`spawn_hook` 改道入口
- 工具 schema（JSON Schema）与参数校验
- 工具开关：allowlist / denylist / `--no-tools` / `--no-builtin-tools` 的底层能力（CLI 接线在 T10）

### Out

- 可选工具 grep / find / ls（T14 或按需另立任务）
- 扩展工具注册与同名覆盖（T15）
- `!` / `!!` 用户 bash 交互路径（T12）

## 开发要点

- `edit-diff` 算法逐语义移植，边界用例（无匹配、多匹配、模糊匹配规则）逐项对照上游测试
- bash 子进程以进程组管理，取消时整组终止（编码规范 §11.3）
- 截断策略（行数/字节）常量与上游一致
- 工具输出截断、错误返回形状与上游对齐（对拍可见）

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] 上游 tools 相关测试意图移植通过
- [ ] edit-diff 边界用例集（无匹配/多匹配/部分匹配）与上游语义一致
- [ ] bash：流式 update 回调、截断、取消后进程组无残留（测试断言无僵尸进程）
- [ ] file mutation queue：并发 edit/write 串行化语义正确
- [ ] 截断常量与上游逐值核对

## 门禁验收

通用门禁 G1–G7 全过。

任务特有标准：

- [ ] faux provider + 工具脚本场景（read 文件、bash 命令）事件序列与 fixtures 归一化 diff 一致
- [ ] `spawn_hook` 可替换 spawn 行为（测试用 hook 断言被调用）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
