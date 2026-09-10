# subagents 对拍 harness（TE04 G3；双轨重定基 TE13）

驱动钉死版上游 pi-subagents 与本 crate 的 `build_rpi_args` / frontmatter 解析器 /
`get_finalOutput` / fallback 模式表 / 发现入口跑同一组 fixture，归一化后逐项 diff。

## 双轨

| 轨 | 上游 | 用途 | 报告目录 |
|----|------|------|----------|
| `regression`（默认） | 旧 pin v0.48.0（`external/pi-subagents` @ `56f97234`，只读） | 保证现有实现行为不回归 | `fixtures/generated/subagents-parity/` |
| `target` | 新 pin v0.66.0（`0fc0eebb`，仓库外快照） | 新语义对拍与 golden 重录（ADR-0025） | `fixtures/generated/subagents-parity-v066/` |

旧轨保留至 pin 切换完成（TE27，ADR-0025 §8/§9）；两轨 fixture 输入分离：基线用例在
`fixtures.json`，目标轨新增用例在 `fixtures-target.json`（目标轨按模式拼接两者）。

## 运行

```bash
# 一次性准备：tsx 外置安装，绝不写入 external/
mkdir -p /tmp/rpi-subagents-parity-deps && cd /tmp/rpi-subagents-parity-deps \
  && npm init -y && npm install tsx@4 --no-save

cd <repo-root>

# 回归轨（默认；与 TE13 前的 harness 逐项一致，零回归红线）
node scripts/subagents-parity/run-parity.mjs
# 等价写法：node scripts/subagents-parity/run-parity.mjs --track=regression

# 目标轨（v0.66.0）
bash scripts/subagents-parity/setup-target-source.sh   # 抽取仓库外快照 + 其 prod 依赖
node scripts/subagents-parity/run-parity.mjs --track=target

# 重录 argv/env 冻结基线（[RPI-OWN]，ADR-0025 §4）
node scripts/subagents-parity/run-parity.mjs --record-args-golden
```

Rust 腿由 `run-parity.mjs` 自己构建（cargo 缓存命中时近零开销）并**拷贝到私有路径后执行**：
两个插件 crate 都有名为 `parity_runner` 的 example，`target/debug/examples/parity_runner`
归最后构建的 crate 所有，mcp harness 会把它覆盖掉（TE13 实测发现的 harness 缺陷）。
私有拷贝使两套 harness 互不干扰，example 名称与既有文档保持兼容。

退出码：回归轨非 0 = 有差异；目标轨非 0 = 存在**未归因**差异。

### 目标轨 discovery 腿（TE15，R7.1.3）

`--track=target` 多跑一个 `discovery` 模式：同一条 tree 用例由**两侧各自物化**——
上游腿把 fixture 里的 `<CFGDIR>` 落为 `.pi`，Rust 腿落为 `.rpi`，输出再把该段归一
回 `<CFGDIR>`；两侧都调用**真实发现入口**（上游 v0.66 `discoverAgents(cwd, "user")`，
rpi `discover_agents_with_user_dirs_with_diagnostics`），比较过滤到该树后的
`agents`（name/source/path）与 `diagnostics`（path/source/error）。上游腿把 HOME /
USERPROFILE / `PI_CODING_AGENT_DIR` 指向仓库外 sandbox 并设 `PI_OFFLINE=1`（跳过
`npm root -g`），scope `user` 使 v0.66 走 `discoverAgentsUncached`，同一进程内多
用例互不污染；内建 agent 与 `~/.agents` 由两侧按路径过滤排除。symlink 用例仅
非 Windows 平台创建（两侧一致跳过，见 `fixtures/subagents-v066/discovery/materialize.json`）。

## 目标轨上游来源（仓库外，external/ 零写入）

`setup-target-source.sh` 用 `git -C external/pi-subagents archive <pin>` 把 v0.66 源码抽取到
`/tmp/rpi-subagents-parity-target-v066`（`RPI_SUBAGENTS_TARGET_SRC` 可覆盖），不 checkout、
不 `git worktree add`、不改 submodule HEAD——`git -C external/pi-subagents status --porcelain`
保持为空。快照内 `npm install --omit=dev` 装的是快照自带 `package.json` 的 prod 依赖
（v0.66 `utils.ts → formatters.ts → settings.ts → agents/agents.ts` 在运行时 import `yaml`；
v0.48 的链路止于 settings.ts，因此回归轨不需要依赖）。fetch 区间只需一次
`git -C external/pi-subagents fetch --deepen=700 origin`（只读）。

## argv/env 的 [RPI-OWN] 基线

上游 v0.65+ 删除了 `src/runs/shared/pi-args.ts` / `buildPiArgs`（子 agent 改进程内
AgentSession），rpi 子进程模型的 argv/env 组装不再有上游对照物（R7.1.0.4、ADR-0025 §4）：

- 回归轨仍跑 v0.48 `pi-args.ts`（旧轨即现状）；
- 目标轨改为对**冻结黄金文件** `args-golden-v048.json` 比较——该文件由
  `--record-args-golden` 从 v0.48 上游腿录制（session 基座占位化为 `<SESSION_BASE>`；
  只重录 fixtures.json 的非内联用例）；
- M2/M3 因 R7.1.4 系列改动 argv/env 时，由对应任务更新黄金文件并按 G2 登记
  「旧期望 → 新期望 + 依据」；
- **TE18 增补**：无上游录制器的新语义（`--exclude-tools` 等上游从未在 argv 面
  存在的行为）以**内联 [RPI-OWN] 黄金**落在 `fixtures-target.json` 用例的
  `expected` 字段，由编排器 `compareArgsTarget` 直接对 Rust 腿比较（不经上游腿）；
  语义正确性由任务 §3.3 规则 + crate 单测钉死，内联黄金防未来回归。

### TE18 新增：model 解析腿（目标轨）

`--track=target` 多跑一个 `model` 模式：共享 fixture（registry/parentModel/origin）
同时驱动 v0.66 快照的 `resolveSubagentModelOverride` / `buildModelCandidates` 与
rpi 对应实现（`parity::resolve_subagent_model_override_public` /
`build_model_candidates_public`），两侧输出 `{resolved|candidates}` 或 `{error}`
（fail-closed 抛错两侧同形 diff），覆盖空 registry 透传 / 命中规范化 / thinking
后缀重试 / 未命中 fail-closed（#1093）与 origin 感知候选链。

## 归因规则（目标轨）

目标轨的每条差异必须命中 `expected-target-diffs.json`，否则报告落 `### unattributed` 且退出码非 0：

- `upstream-semantics`：新 tag 行为、rpi 尚未采纳（挂 R 条目 + 承接任务）；
- `rpi-deviation`：rpi 既有实现与两个 pin 都不一致的偏差；
- 每条含 `mode/case`、`section`、`r`、`owner`；报告按两节汇总。
- 差异字段为 `null` 表示 Rust 侧函数尚未实现（如 M0 的 `isContextOverflow` /
  `isRetryableModelFailureAttempt`），同样按上述两节归因，不静默跳过。

> **TE14 落地注记（2026-09-09）**：`expected-target-diffs.json` 已清空——
> R7.1.2.1 模式表五项补齐（`REQUEST_LIMIT_EXCEEDED`/`usage limit`/
> `connection (error|reset|closed|aborted)`/`\b500\b`/`internal server error`）、
> `isContextOverflow`（R7.1.2.2）与 `isRetryableModelFailureAttempt`（R7.1.2.3）
> 落地后，fallback 16 用例全部 `MATCH`（目标轨 43/43），报告落
> `fixtures/generated/subagents-parity-v066/parity-report.md`（`RESULT: MATCH`）。
> 后续 TE15–TE18 若产生新差异，按原规则在清单追加归因。

## 组成

| 文件 | 职责 |
|------|------|
| `fixtures.json` | 基线共享用例：9 组 argv/env 输入、6 组 frontmatter 内容、5 组 message 数组 |
| `fixtures-target.json` | 目标轨新增：frontmatter（inherit/false、excludeTools、坏 frontmatter、thinking）、final-output、fallback 向量、discovery tree（TE15）、notify（TE17）、**argv 内联 [RPI-OWN] 黄金（TE18：excludeTools 面，无上游录制器，期望内联在用例里）与 model 解析向量（TE18 R7.1.4.4/.5，直接对拍 v0.66 `model-fallback.ts`）** |
| `args-golden-v048.json` | argv/env 冻结黄金文件（[RPI-OWN]；只覆盖 fixtures.json 的 9 例，`--record-args-golden` 只重录非内联用例） |
| `expected-target-diffs.json` | 目标轨差异归因清单（R + 承接任务） |
| `upstream-runner.mjs` | tsx 直跑上游模块：回归轨 v0.48；目标轨 frontmatter/final-output/fallback 走 v0.66 快照、args 走黄金文件、discovery 走 v0.66 `discoverAgents` |
| `setup-target-source.sh` | 仓库外抽取 v0.66 快照 + 安装其 prod 依赖（external/ 零写入） |
| `examples/parity_runner.rs` | 本 crate 同 fixture 驱动（parity facade，`lib.rs::parity`）；由编排器构建并私有拷贝后执行 |
| `run-parity.mjs` | 编排 + 归一化 diff + 归因 + 报告落盘；物化 fixture 与 Rust 二进制拷贝落仓库外临时目录 |

`PI_CODING_AGENT_PACKAGE_ROOT=/tmp` 短路上游 `resolvePiPackageRoot` 的
`import.meta.resolve`（包未安装时该函数抛错，上游以 env 优先）。

## 归一化白名单（豁免与依据）

1. **session 路径具象化**：fixture 中 `/sess/root` 由编排器重写为共享
   temp 目录（两侧同值原样比较，`--session-dir`/`--session` 值逐字节一致）；
   比较时 `${SESSION_BASE}/sess/root` → `<SESSION_BASE>`，使跨运行录制的
   冻结黄金文件可直接比较。
2. **temp 目录名**：mkdtemp 前缀 `pi-subagent-*` / `rpi-subagent-*`
   （ADR-0001 改名）→ `<TMPDIR>`。
3. **`--extension` 值**：上游注入自身源文件（prompt-runtime.ts /
   fanout-child.ts / 权限系统），rpi 注入本插件 cdylib（一个库承担
   prompt-runtime + fanout-child 两职，TE-D17）→ 全部归一为 `<EXT>`；
   连续的 `<EXT> --extension <EXT>` 运行折叠为一项（上游双源文件 vs
   rpi 单 cdylib 的已知差）。
4. **env 键序**：JS 插入序 vs Rust BTreeMap 序 → 两侧按键排序比较。
5. **上游专属 env 键丢弃**：`PI_SUBAGENT_RUNTIME_ACKNOWLEDGED_EXTENSIONS`
   （runtime-ack 扩展回执，P1）、`PI_CODING_AGENT_PACKAGE_ROOT` /
   `PI_SUBAGENTS_PI_CODING_AGENT_PACKAGE_ROOT`（node 包根传播，rpi 无对应物，
   两个历史名都丢弃）。其余键含 `PI_SUBAGENT_*` → `RPI_SUBAGENT_*`
   改名对齐。
6. **rpi 专属 env 键丢弃（TE05 新增；TE18 增补）**：`RPI_SUBAGENT_STEER_INBOX`、
   `RPI_SUBAGENT_SUPERVISOR_CHANNEL_DIR`——rpi 原生的 steer 收件箱与
   supervisor 通道目录槽位（FR-P1-04/10）；`RPI_NO_GLOBAL_CONTEXT`（TE18 /
   ADR-0026，上游 #1560 的进程内 `inheritGlobalContext:false` 默认在 rpi 侧的
   env 开关，两 pin 均无 argv/env 对应物；其存在性由 crate 单测 + e2e env dump
   钉死而非本 diff）。上述键从 diff 中剔除。
7. **prompt 临时文件内容不比较**：rpi 在文件头额外前置边界指令块
   （`<active_agent>` 之后、正文之前，TE-D17 机制等价替代）；argv/env
   层面的路径与 flag 一致即可。

## v0.66 共享面变化（目标轨实读，ADR-0025 附录 D）

- `src/runs/shared/pi-args.ts` **已删除** → argv/env 转 [RPI-OWN]（上节）；
- `src/agents/frontmatter.ts` v0.48→v0.66 **逐字节不变**（frontmatter 用例两轨同形）；
- `src/shared/utils.ts`：`getFinalOutput` 增 `stripPiTurnTimingFooter`（#1792，rpi 无该输出、
  [N/A]，故不设 footer 用例）；`hasEmptyTerminalAssistantResponse` 扩「空文本终态」语义
  （R7.1.1.2，TE14）；
- `src/runs/shared/model-fallback.ts`：新增 `REQUEST_LIMIT_EXCEEDED`/`usage limit`/
  `connection (error|reset|closed|aborted)`/`500`/`internal server error` 模式与
  `isRetryableModelFailureAttempt`/`isContextOverflow`/`recordRetryableModelFailure`
  （R7.1.2.1–.3，TE14）；`isRetryableModelFailure` 与 `formatModelAttemptNote` 语义未变。
- `src/agents/agents.ts` 发现面：`DISCOVERY_PRUNED_DIR_NAMES` 补 `.pi`/`sync-backups`
  （#1596/671bc27c）；symlink 目录按目录跟随 + `visitedDirectories` realpath 集
  （#1505/#1510/9433419a）；单文件 try/catch → `AgentDiscoveryDiagnostic`
  （#1200/e973fa3c）。rpi 以 `.rpi` 替代 `.pi`（ADR-0001），其余按同语义对拍（TE15）。

## 环境隔离（运行前须知）

`run-parity.mjs` 两条腿都以**清除后的环境**启动子进程：所有 `PI_SUBAGENT*` /
`RPI_SUBAGENT*` 前缀的环境键一律删除（`cleanSessionEnv`，harness 自己的
`RPI_SUBAGENTS_PARITY_TRACK` 在清除之后再加）。原因：

- 上游腿的 `pi-args.ts` 会回退读取 shell 里的 `PI_*` 值，rust 腿读 `RPI_*`
  （桥接层把 `PI_SUBAGENT_*` 改名为 `RPI_SUBAGENT_*`）——若在外层 shell 导出过
  其中任一键，它的值只到达一条腿，args 模式会有假 MISMATCH（`fork-session-file`
  之外的用例都走环境回退）。2026-08-15 前的 harness 未做此隔离；如需复现旧行为
  可手动导出该键并观察误报。
- **pi 子代理会话**会把父进程环境以 `PI_SUBAGENTS_` 前缀转发给子代理（实见
  `PI_SUBAGENTS_PI_CODING_AGENT_PACKAGE_ROOT`，即 `pi-args.ts:641` 复制进子
  进程 env 的包根键），因此只清 `PI_SUBAGENT_PARENT_SESSION` 不够——在 pi 子
  代理会话里跑对拍会出现 8 个 args 假 MISMATCH。2026-09-09 修复后前缀键全清，
  两种环境（普通 shell / pi 子代理）均 `MATCH`。

## 已知不适用面

- 工具描述全文：入口从 workflowScript 换成结构化参数（ADR-0016），
  文案必然不同；custom 模板机制与 SAFETY 段结构由 crate 单测覆盖。
- 会话条目过滤：上游在子进程 context 事件内过滤，rpi 在 fork 分支文件
  上过滤（设计 §3.4），结果等价但层不同（e2e 场景 3 覆盖）。
- turn-timing footer（#1792）：rpi 无该输出，按 [N/A] 不设对拍用例
  （03 附录 C.3）。
