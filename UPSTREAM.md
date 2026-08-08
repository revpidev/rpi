# 上游对照（Upstream Pin）

本仓库行为金标准固定为以下 Pi 版本，**不要**在未立 ADR 的情况下擅自更新。

| 项 | 值 |
|----|-----|
| 远程 | https://github.com/earendil-works/pi.git |
| 本地 | `external/pi/` |
| npm 版本 | `0.84.1`（coding-agent；HEAD 含 0.84.1 之后的少量未发布变更） |
| Git commit | `4181f66e6b3ccbef760c2966ecd8b596b926fec6` |
| 短哈希 | `4181f66` |
| 提交说明 | `docs(agent): tighten durable harness design` |
| 提交时间 | 2026-08-08 |

升级说明：v0.11 将对照基线从 `2efa728`（v0.82.1）提升至 `4181f66`（v0.84.1+），跨度 461 commits / 655 文件。变更需求与设计见 [`docs/v0.11/`](./docs/v0.11/)。

历史基线：v0.1 钉死 `2efa728d2ee90ef597626e96b1e28ef2b279f07c`（v0.82.1，2026-07-27），决策记录 [ADR-0002](./docs/adr/0002-baseline-decisions.md)。

校验：

```bash
cd external/pi && git rev-parse HEAD
# 期望: 4181f66e6b3ccbef760c2966ecd8b596b926fec6
```
