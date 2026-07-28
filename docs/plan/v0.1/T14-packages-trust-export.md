# T14：Packages / Trust / Export / llama / 更新

- **状态**：未开始
- **里程碑**：M7
- **依赖**：T09（资源管线与 settings）、T10（CLI 与子命令骨架）
- **上游对照**：`docs/packages.md`、`docs/settings.md`（trust/telemetry 段）、`packages/coding-agent/src/core/` packages/trust/export/llama 相关模块
- **需求章节**：§3.2、§7.6、§7.8、§10（endpoint/单文件）、llama.cpp 与 export/share（§6.3、§8.4 相关项）
- **预估**：1.5–2 人月

---

## 目标

交付包管理、项目信任、导出分享、llama.cpp 集成与产品 endpoint 配置化，
达到 M7「Provider/OAuth 之外的产品能力」验收口径。

## 范围

### In

- 子命令（需求 §3.2）：`pir install` / `remove`(`uninstall`) / `list` / `update`（`--self` / `--all` / `--extensions` / `--models` / 单包）/ `config`（启用禁用 extensions、skills、prompts、themes）
- Packages：`npm:` / `git:` / URL / 本地；全局与 `-l` 项目级；`package.json#pi` manifest 解析；pinned ref 不被 update 升级；`npmCommand` wrapper（调系统 npm/git）
- Project trust：`trust.json`、两阶段加载接线（T09/T12 能力的产品化）、`defaultProjectTrust: ask|always|never`、`--approve/--no-approve` 语义收尾
- Export / share：HTML 导出（`--export` 与 `/export`）、JSONL export、 gist share（endpoint 可配置）
- llama.cpp 集成：`/llama` 交互、本地模型管理（router provider 已在 T13）
- 产品 endpoint：版本检查 / telemetry 在 settings / `PIR_*` 可配置、可关闭（ADR-0002 §8）；未配置时合理默认或关闭
- 单文件发布链路验证：musl + rustls 构建、自更新（`update --self`）流程

### Out

- 扩展包（Wasm 包）的安装管理（T15；本任务 packages 机制面向声明式资源包）

## 开发要点

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

- [ ] 子命令全集：install/remove/list/update/config 各路径（本地包 + 模拟 npm/git 源）
- [ ] pinned ref 不被升级；全局与项目级安装位置正确（`~/.pir/agent/` 与 `.pir/`）
- [ ] trust：两阶段加载时序、`defaultProjectTrust` 三态、非交互不提示
- [ ] HTML/JSONL export 输出结构与上游 fixtures 对拍
- [ ] endpoint 配置化：自定义 URL 生效、关闭后不产生请求（测试断言无网络调用）
- [ ] musl release 单文件构建通过；`update --self` 流程 dry-run 验证

## 门禁验收

通用门禁 G1–G7 全过。

任务特有标准：

- [ ] 需求 §3.2、§7.6、§7.8、§10 逐条核对有锚点
- [ ] llama.cpp 集成 smoke（本机有 llama.cpp 时；无条件记录豁免）
- [ ] 单文件发布物 smoke：`--help` / `--version` / `--list-models` / 一次 faux 对话

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
