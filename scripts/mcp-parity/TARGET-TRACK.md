# mcp-parity 目标轨骨架与重录清单（TE13 交付；实际重录归 TE23/TE24）

> **pin 已切换（TE27，2026-09-11；ADR-0025 已采纳）**：`external/pi-mcp-adapter` @
> `10a45367e033a32026987a75d6f401e37340c86f`（v2.32.1，90 commits）。
> 本文件保留作目标轨历史记录；缺省驱动（submodule 工作树）即 v2.32.1，快照路径
> `/tmp/rpi-mcp-parity-target-v2321` 仅作独立对照源使用。

## 1. 骨架就位内容（TE13 已完成）

| 项 | 落点 | 说明 |
|----|------|------|
| 上游根可切换 | `run-mcp-parity.mjs` / `run-oauth-parity.mjs` / `render-call-upstream.mjs` | 均读 `RPI_MCP_PARITY_UPSTREAM`，缺省 = submodule 工作树（TE27 起 = v2.32.1） |
| 目标源码/依赖外置 | `setup-target-source.sh` | `git archive` 抽取 v2.32.1 到 `/tmp/rpi-mcp-parity-target-v2321` 并用其 lockfile `npm ci`（external/ 零写入） |
| conformance 基线重生成入口 | 本文件 §3 + `run-parity-suite.sh conformance` | 基线为 rpi 客户端预期失败清单（与上游 tag 无关；重生成入口 TE24 已验收） |
| 行为对拍新增面清单 | 本文件 §4 | 审批作用域/退避可见性/503·202·401 分类/嵌套参数/OAuth 401（均已由 TE21/TE22 落地验收） |

**本骨架不做**：不切换默认驱动、不重录 `conformance-baseline.yml`、不重录任何 golden
向量、不改 crate 实现——这些分别属 TE23/TE24/TE21/TE22（G10「对拍先于实现」的落地顺序见
各任务文档）。

## 2. 目标轨跑法（骨架验证）

```bash
# 一次性：外置快照 + 其 lockfile 闭包
bash scripts/mcp-parity/setup-target-source.sh
export RPI_MCP_PARITY_UPSTREAM=/tmp/rpi-mcp-parity-target-v2321
export RPI_MCP_PARITY_DEPS=/tmp/rpi-mcp-parity-target-v2321

# 协议腿 / renderCall 腿（用目标 pin 源码 + 目标闭包）
node scripts/mcp-parity/run-mcp-parity.mjs --out-dir /tmp/mcp-target-parity
node scripts/mcp-parity/run-render-call-parity.mjs
```

目标轨在 M1–M3 完成前**预期产生差异**（新 namespace 工具、请求头、退避、命名等）；
差异清单即 §4 各承接任务的验收入口。TE23/TE24 完成对应批次后，目标轨须逐步收敛为
零差异（golden 按新 pin 重录）。

### TE13 骨架实测（2026-09-08）

| 腿 | 目标轨结果 | 说明 |
|----|------------|------|
| renderCall 纯函数（24 例） | **24/24 逐字节一致**（exit 0） | v2.32.1 renderer 新增 `truncateToWidth`/`visibleWidth` 运行时导入；stub 已按 pi-tui 的 printable-ASCII 快路径补齐（`render-call-host-pi-tui.mjs`），用例均为短 ASCII 行 |
| 协议腿（7 场景） | 6 DIFF + `http-auth-401` MATCH（exit 1） | DIFF 均为 v2.32.1 新增面（namespace 工具/请求头等），属预期，归 TE23/TE24 重录后收敛 |

- 报告头 pin 由 `RPI_MCP_PARITY_UPSTREAM_PIN` 控制（缺省 `3d953f90`；目标轨设 `10a45367`），
  避免目标轨报告误标旧 pin。
- 目标轨输出务必用 `--out-dir` / `RPI_MCP_PARITY_OUT_DIR` 指向 scratch 目录，避免覆盖
  `fixtures/generated/mcp-parity/` 的回归证据。
- **stub 局限**（§4.3 输入）：当前 pi-tui stub 只实现 printable-ASCII 快路径；若 TE24
  重录的 render 向量含 ANSI/宽字符，需把 `@earendil-works/pi-tui` 映射切到目标依赖根里的
  真实包（`render-call-hooks.mjs` 的映射点），并在任务文档登记。

## 3. 重录清单：命名类 golden（D-R6，承接 **TE23**，先重录后改实现）

> **TE23 落地（2026-09-09）**：3.1/3.2 已按目标轨重录（`RPI_MCP_FIXTURE_UPSTREAM` /
> `RPI_MCP_FIXTURE_PIN=10a45367` / `RPI_MCP_FIXTURE_ONLY=names,glob`）；`glob_cases.json`
> 因候选集语义属 FR-B，同批重录（期望布尔值零变化、候选列表扩展）。
> `golden_names.rs`/`golden_glob.rs` 未改断言形状，仅补齐新签名的 `other_current_candidates` 参数。

| # | 对象 | 动作 | 依据 |
|---|------|------|------|
| 3.1 | `crates/rpi-ext-mcp-adapter/tests/fixtures/name_format_cases.json` | **已完成**（TE23）：按 v2.32.1 命名规则重录（BREAKING：server 前缀保留 `-`/`_`，`a-b` 不再编码为 `a_2d_b`） | R7.2.4、需求附录 A |
| 3.2 | `crates/rpi-ext-mcp-adapter/tests/golden_names.rs` | **已完成**（TE23）：期望由 3.1 驱动；「旧期望 → 新期望 + 上游 commit」在 TE23 §7 逐类登记（G2） | R7.2.4.4 |
| 3.3 | `changes/` 单列 | **已完成**（TE23）：BREAKING 条目 + `toolPrefix:"none"` 兼容指引；不做新旧双注册 | G10 |

## 4. 重录清单：conformance 与 golden 向量（D-R7，承接 **TE24**）

| # | 对象 | 动作 | 依据 |
|---|------|------|------|
| 4.1 | `scripts/mcp-parity/conformance-baseline.yml` | 用目标快照重跑官方 referee 后重生成（含新 namespace/请求头/bearer 面）；本文件 §2 命令 + `run-parity-suite.sh conformance` 的归档路径 | R7.2.4.4、设计 §4.9 |
| 4.2 | `tests/golden_config_merge.rs` / `golden_config_hash.rs` / `golden_glob.rs` / `golden_search.rs` / `golden_tsshape.rs` | 按 v2.32.1 纯函数语义重录；逐条登记 G2 期望变更 | 设计 §4.9 |
| 4.3 | `scripts/mcp-parity/render-call-fixtures.json` + `render-call-parity` 产物 | 渲染面按新 tag 重录（工具名/描述/渲染分支） | R7.2.10 |

## 5. 行为对拍新增面（D-R7b，承接 TE21/TE22/TE24）

| 面 | 承接 | 验收形态 |
|----|------|----------|
| 审批参数作用域（参数 A 批准后参数 B 仍拦截） | TE21 | fixture 用例 + 持久化 `/resume` 恢复 |
| 退避可见性（连续失败后的状态/诊断） | TE22 | **已落地**（`tests/te22_backoff_oauth.rs`：失败注入 → status/list/search/describe/instructions/direct 逐面断言 + 过期恢复 + `/mcp status|tools`） |
| 503/202/401 分类 | TE22 | 协议腿响应分类向量（401 腿 `http-auth-401` 回归轨/目标轨均 MATCH；503/202 归 TE24） |
| 嵌套参数（对象/数组参数哈希稳定） | TE21 | 参数作用域向量 |
| OAuth 401 与重注册 | TE22 | **已落地**（`tests/te22_backoff_oauth.rs`：MemorySecretStore 注入 + 401 compare-and-delete；stub AS `invalid_grant` → DCR 请求体逐字段断言）；`run-oauth-parity.mjs` 回归轨/目标轨均 MATCH |

## 6. 形状/口径

- 目标轨复用现有 harness 的归一化白名单与豁免（见 `README.md`）；新增面在对应任务
  文档中登记豁免与依据。
- 重录产物进 git 作证据链（与现有 `fixtures/generated/mcp-parity/` 一致），归一化
  剔除运行期易变值。
- 本文件与 `scripts/subagents-parity/expected-target-diffs.json` 的分工：mcp 侧重录
  在 TE23/TE24 一次完成（golden 直接替换）；subagents 侧因实现分批，目标轨用归因
  清单过渡。
