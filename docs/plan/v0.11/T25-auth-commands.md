# T25：auth 命令族

- **状态**：未开始
- **里程碑**：M3
- **依赖**：T21
- **上游对照**：`99e34013d`（print-api-key/print-bearer-token #7168）、`a261366bd`（auth check）；`src/cli/auth-command.ts`（125 行）、`auth-check.ts`（73 行）、`credential-print.ts`（87 行）
- **需求章节**：v0.11 需求 R3.6；设计 §5.4
- **预估**：0.2 人月

---

## 目标

落地 `pir auth` 三个子命令，逻辑复用 T21 的 OAuth/凭证能力，CLI 层为薄壳。

## 范围

### In

- `pir auth print-api-key` / `pir auth print-bearer-token`：导出凭证给外部客户端；自动 OAuth 刷新；`--min-expiry <duration>`（默认 5 分钟最小有效期，复用 T21 的 `min_oauth_validity_ms`）
- `pir auth check`：provider/model 认证预检；`--json` / `--credentials` / `--no-refresh`；**退出码 ready=0 / not_ready=1 / invalid=2**
- CLI 解析器（v0.1 手写解析器）新增子命令路由；帮助文本对齐上游
- 输出脱敏检查：print 类命令的 stdout 即凭据本体（合法），但日志/错误消息不得夹带（G4）

### Out

- OAuth 刷新/超时机制本身（T21）
- 交互式 `/login` 流程变更（无变更）

## 开发要点

- 退出码三态是对拍契约，集成测试逐码覆盖
- `--min-expiry` 的 duration 解析格式与上游一致（核对 `auth-command.ts` 的解析实现）
- `auth check` 的 ready/not_ready/invalid 判定矩阵以上游 `auth-check.ts` 为准逐条移植

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] `auth check` 退出码 0/1/2 三态 + `--json`/`--credentials`/`--no-refresh` 组合
- [ ] `print-api-key`/`print-bearer-token`：有效凭据直出、临近过期自动刷新、`--min-expiry` 不足报错
- [ ] 帮助文本与上游一致；CLI 解析 84 移植测试（v0.1）扩充新子命令用例
- [ ] 错误路径无凭据泄漏（日志脱敏断言）

## 门禁验收

通用门禁 G1–G7 全过（G4 重点：凭据脱敏）。

任务特有标准：

- [ ] 需求 R3.6 两条逐条核对（含退出码表）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| （待登记） | | |

## 验收记录

（按 gates §3 模板填写）
