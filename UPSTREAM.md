# 上游对照（Upstream Pin）

本仓库行为金标准固定为以下 Pi 版本，**不要**在未立 ADR 的情况下擅自更新。

| 项 | 值 |
|----|-----|
| 远程 | https://github.com/earendil-works/pi.git |
| 本地 | `external/pi/` |
| npm 版本 | `0.82.1` |
| Git commit | `2efa728d2ee90ef597626e96b1e28ef2b279f07c` |
| 短哈希 | `2efa728` |
| 提交说明 | `fix(coding-agent): support concurrent user bash cancellation (#7103)` |
| 提交时间 | 2026-07-27 |

决策记录：[ADR-0002](./docs/adr/0002-baseline-decisions.md)

校验：

```bash
cd external/pi && git rev-parse HEAD
# 期望: 2efa728d2ee90ef597626e96b1e28ef2b279f07c
```
