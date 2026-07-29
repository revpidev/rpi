# T14：可选工具 / Packages / Trust / Export / llama / 更新

- **状态**：未开始
- **里程碑**：M7
- **依赖**：T09（资源管线与 settings）、T10（CLI 与子命令骨架）
- **上游对照**：`docs/packages.md`、`docs/settings.md`（trust/telemetry 段）、`docs/security.md`、`docs/llama-cpp.md`、`packages/coding-agent/src/core/{tools/{grep,find,ls},package-manager,trust-manager,project-trust}.ts`、`src/package-manager-cli.ts`、`src/modes/interactive`（share/export 段）
- **需求章节**：§3.2、§4.5（可选工具）、§7.6、§7.8、§10（endpoint/单文件）、llama.cpp 与 export/share（§6.4、§8.4 相关项）
- **预估**：1.5–2.5 人月

---

## 目标

交付可选三工具、包管理、项目信任、导出分享、llama.cpp 集成与产品 endpoint 配置化，
达到 M7「Provider/OAuth 之外的产品能力」验收口径。

## 范围

### In

- **可选工具 grep / find / ls**（`pir/src/tools/`，**Rust 原生实现**，`ignore`/`globset` crate，**不引入外部 rg/fd 下载**，ADR-0003 §2）：
  - grep：rg 等价行为（`--json --line-number --color=never --hidden` 语义）；输出格式：匹配行 `path:lineno: text`、上下文行 `path-lineno- text`（grep.ts:264-265）；默认 limit=100 匹配（达标即停）；单行截 500 字符；context 回读补行；50KB 截断；`limit=N*2` 提示
  - find：fd 等价行为（`--glob --color=never --hidden` 语义）；repo 外 `--no-require-git` 等价；含 `/` pattern → full-path + `**/` 前缀规则；默认 limit=1000；相对化输出、目录尾斜杠；固定忽略 node_modules/.git
  - ls：默认 limit=500；大小写不敏感排序；目录加 `/`；含 dotfiles；stat 失败跳过；50KB 截断
- 子命令（需求 §3.2）：`pir install` / `remove`(`uninstall`) / `list`（User/Project 分组、`(filtered)` 标记）/ `update`（裸=self+提示；`--self`/`pi`、`--all`、`--extensions`、`--models`、`--extension` 互斥矩阵；`--force`；release note 渲染）/ `config`（TUI Tab 切 scope；`-l` 要求 trust）；子命令先于主 parseArgs 分流；各支持 `-a/-na`
- Packages（需求 §7.6）：source 解析顺序（`npm:` → 本地路径 → git URL → 回退本地；**裸名按本地路径**）；npm 精确版本=pinned 跳过更新、range semver maxSatisfying；git **pinned ref 不移动但 update 会 reconcile**（reset+clean+依赖安装）；`-e` 临时 scope 安装 `~/.pir/agent/tmp/extensions`（0700）；身份去重（npm 按名 / git 按 host/path / local 按绝对路径）；`autoload:false` delta；过滤语法 glob/`!`/`+`/`-`；`package.json#pi` manifest；`npmCommand` wrapper；离线跳过、网络超时 10s、更新并发 4（调系统 npm/git）
- Project trust 产品化（需求 §7.8）：`trust.json`（路径→bool、父链最近条目、排序写盘、lockfile）；触发条件资源清单（`.pir/` 7 类 + 祖先 `.agents/skills`，`~/.agents/skills` 豁免）；解析优先级链（CLI override > 扩展 `project_trust` 事件 > trust.json > defaultProjectTrust；**无 UI 时 ask=false**）；两阶段加载接线收尾；`/trust` 只写不重载；`pir update` 永不提示
- Export / share：HTML 导出（`--export` 与 `/export`，模板结构对齐上游）、JSONL export、 gist share（**shell 调 `gh gist create --public=false`** + `PIR_SHARE_VIEWER_URL` 拼接，endpoint 可配置）
- llama.cpp 集成：内置 hidden 扩展（`/llama` 命令经扩展注册，**非内置 slash**）；`/login llama.cpp` 与 `LLAMA_BASE_URL`/`LLAMA_API_KEY`；HF 搜索下载 `owner/repo[:quant]`、HF_TOKEN 查找、永不静默卸载/删除（`docs/llama-cpp.md`）
- 产品 endpoint：版本检查 / telemetry / 远程 catalog 在 settings / `PIR_*` 可配置、可关闭（ADR-0002 §8）；`enableInstallTelemetry`(true)/`enableAnalytics`(false opt-in)
- 单文件发布链路验证：musl + rustls 构建、自更新（`update --self`）流程

### Out

- 扩展包（Wasm 包）的安装管理（T15；本任务 packages 机制面向声明式资源包）
- trust 弹窗交互（T12 已含；本任务提供底层决策链）

## 开发要点

- grep/find/ls 的输出格式、limit、截断、排序为对拍契约；原生实现须逐场景与上游 rg/fd 输出比对
- 调系统 npm/git 时注意 PATH 解析、超时与错误透传；`npmCommand` wrapper 语义与上游一致
- 两阶段加载的时序敏感：信任前后资源集合的差集有测试锚点
- telemetry 默认策略：可关、可改 URL，日志不含敏感信息
- HTML 导出模板与上游输出结构对齐（对拍可比对结构而非样式细节）

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] grep/find/ls：各 limit、截断、排序、忽略规则与上游输出 fixtures 对拍
- [ ] 子命令全集：install/remove/list/update/config 各路径（本地包 + 模拟 npm/git 源）；update 互斥矩阵
- [ ] pinned ref reconcile 语义；npm 精确版本跳过；全局与项目级安装位置正确
- [ ] trust：决策优先级链各分支、触发条件清单、两阶段时序、非交互不提示（ask=false）
- [ ] HTML/JSONL export 输出结构与上游 fixtures 对拍
- [ ] gist share：mock gh 调用与 URL 拼接
- [ ] endpoint 配置化：自定义 URL 生效、关闭后不产生请求（测试断言无网络调用）
- [ ] musl release 单文件构建通过；`update --self` 流程 dry-run 验证

## 门禁验收

通用门禁 G1–G7 全过（G4 重点：grep/find 无外部二进制下载）。

任务特有标准：

- [ ] 需求 §3.2、§4.5（可选工具）、§7.6、§7.8、§10 逐条核对有锚点
- [ ] llama.cpp 集成 smoke（本机有 llama.cpp 时；无条件记录豁免）
- [ ] 单文件发布物 smoke：`--help` / `--version` / `--list-models` / 一次 faux 对话

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
