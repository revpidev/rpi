# T01：工程骨架与类型契约锁定

- **状态**：未开始
- **里程碑**：M0
- **依赖**：—
- **上游对照**：`external/pi/packages/{ai,agent,tui,coding-agent}` 包结构与类型定义
- **需求章节**：§1.4、§4.1、§4.2（类型面）；§10（单文件部署）
- **预估**：0.5–0.7 人月（M0 共 1–1.5，与 T02 合计）

---

## 目标

建立可编译的 Rust workspace 骨架，锁定跨 crate 的核心类型与事件枚举契约，
使后续所有任务在稳定的地基上并行开发。

## 范围

### In

- workspace `Cargo.toml` + `rustfmt.toml` + release profile（编码规范 §15.4）
- 六个 crate 骨架：`pir-ai`、`pir-agent`、`pir-tui`、`pir`（bin + lib）、`pir-ext-host`、`pir-test-support`，依赖方向按设计文档 §2.2
- `pir-ai` 核心类型：`Role` / `Message` / `Context` / `Tool` / `Model` / `ApiKind` / `AssistantMessage`（含 `stopReason` 全集 `stop|length|toolUse|error|aborted`，`pending` 仅瞬时不入 JSONL）
- `pir-ai` `StreamEvent` 枚举完整定义（M0 锁定，见编码规范 §4.1）
- `pir-agent` `AgentEvent` / `AgentTool` / `AgentMessage` 联合类型（含 `bashExecution` / `custom` / `branchSummary` / `compactionSummary`）
- `StreamFn` 类型别名与 `BoxStream` 定义（设计文档 §4.4）
- 各 crate 主错误枚举占位（`AiError` / `AgentError` / …，`thiserror`）
- 上游 pin 校验脚本（比对 `external/pi` HEAD 与 `UPSTREAM.md`）
- 模块风格：无 `mod.rs`（编码规范 §3.1）

### Out

- 任何 API 适配器实现（T03）、agent loop 逻辑（T05）、TUI 渲染（T11）
- 对拍 harness（T02）

## 开发要点

- 类型字段命名镜像上游 TS 定义；线格式相关的 serde 属性本任务可先落 camelCase 骨架（编码规范 §4.4）
- 事件枚举变体顺序、字段命名与上游逐项核对后锁定；锁定后变更必须过门禁 G3 并更新 fixtures
- 每个 crate `lib.rs` 写模块文档，标注对应上游包（编码规范 §14.3）
- pin 校验脚本建议 `scripts/verify-upstream.sh`，输出 commit 并与 `UPSTREAM.md` 期望值比对

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] `cargo build --workspace` / `clippy -D warnings` / `fmt --check` 通过
- [ ] 类型单测：`StreamEvent` / `AgentEvent` 序列化形状快照测试
- [ ] pin 校验脚本在正确 commit 上通过；人为切到错误 commit 时失败（验证脚本有效性后切回）
- [ ] 依赖方向检查：`pir-agent` 不依赖 provider 实现、`pir-tui` 不依赖 `pir-ai`/`pir-agent`（可用 `cargo tree` 核对）

## 门禁验收

通用门禁 G1–G7 全过（G3 本任务以「类型序列化快照测试」替代 fixtures 对拍，验收记录中说明）。

任务特有标准：

- [ ] 六个 crate 均编译通过且依赖方向与设计文档 §2.2 一致
- [ ] `StreamEvent` / `AgentEvent` 与上游 TS 定义逐项核对清单完成（附在验收记录）
- [ ] release profile 已配置且 `cargo build --release` 通过
- [ ] pin 校验脚本落地并验证有效

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
