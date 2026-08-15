# subagents 对拍 harness（TE04 G3）

驱动钉死版上游 pi-subagents（`external/pi-subagents` @ v0.48.0 /
56f97234，只读）与本 crate 的 `build_rpi_args` / frontmatter 解析器 /
`getFinalOutput` 跑同一组 fixture，归一化后逐项 diff。

## 运行

```bash
# 一次性准备（tsx 外置安装，绝不写入 external/）
mkdir -p /tmp/rpi-subagents-parity-deps && cd /tmp/rpi-subagents-parity-deps \
  && npm init -y && npm install tsx@4 --no-save

cargo build -p rpi-ext-subagents --example parity_runner
node scripts/subagents-parity/run-parity.mjs
```

退出码非 0 = 有差异。报告与两侧原始输出落
`fixtures/generated/subagents-parity/parity-report.md`。

## 组成

| 文件 | 职责 |
|------|------|
| `fixtures.json` | 共享用例：9 组 argv/env 输入、6 组 frontmatter 内容、5 组 message 数组 |
| `upstream-runner.mjs` | tsx 直跑钉死上游模块（`pi-args.ts` / `frontmatter.ts` / `utils.ts`），归一化输出 |
| `examples/parity_runner.rs` | 本 crate 同 fixture 驱动（parity facade，`lib.rs::parity`） |
| `run-parity.mjs` | 编排 + 归一化 diff + 报告落盘 |

`PI_CODING_AGENT_PACKAGE_ROOT=/tmp` 短路上游 `resolvePiPackageRoot` 的
`import.meta.resolve`（包未安装时该函数抛错，上游以 env 优先）。

## 归一化白名单（豁免与依据）

1. **session 路径具象化**：fixture 中 `/sess/root` 由编排器重写为共享
   temp 目录（两侧同值原样比较，`--session-dir`/`--session` 值逐字节一致）。
2. **temp 目录名**：mkdtemp 前缀 `pi-subagent-*` / `rpi-subagent-*
   （ADR-0001 改名）→ `<TMPDIR>`。
3. **`--extension` 值**：上游注入自身源文件（prompt-runtime.ts /
   fanout-child.ts / 权限系统），rpi 注入本插件 cdylib（一个库承担
   prompt-runtime + fanout-child 两职，TE-D17）→ 全部归一为 `<EXT>`；
   连续的 `<EXT> --extension <EXT>` 运行折叠为一项（上游双源文件 vs
   rpi 单 cdylib 的已知差）。
4. **env 键序**：JS 插入序 vs Rust BTreeMap 序 → 两侧按键排序比较。
5. **上游专属 env 键丢弃**：`PI_SUBAGENT_RUNTIME_ACKNOWLEDGED_EXTENSIONS`
   （runtime-ack 扩展回执，P1）、`PI_CODING_AGENT_PACKAGE_ROOT`（node 包
   根传播，rpi 无对应物）。其余键含 `PI_SUBAGENT_*` → `RPI_SUBAGENT_*`
   改名对齐。
6. **rpi 专属 env 键丢弃（TE05 新增）**：`RPI_SUBAGENT_STEER_INBOX`、
   `RPI_SUBAGENT_SUPERVISOR_CHANNEL_DIR`——rpi 原生的 steer 收件箱与
   supervisor 通道目录槽位（FR-P1-04/10），上游等价物在 prompt-runtime
   扩展内部且 fixture 从不设置；两键在 rpi 侧恒为清空值，逐 case 豁免
   改为统一从 diff 中剔除。
6. **prompt 临时文件内容不比较**：rpi 在文件头额外前置边界指令块
   （`<active_agent>` 之后、正文之前，TE-D17 机制等价替代）；argv/env
   层面的路径与 flag 一致即可。

对拍结论留档 `fixtures/generated/subagents-parity/parity-report.md`
（当前 9+6+5 用例全 MATCH；其中 fanout 变量真值、orchestrator
session-id 等两处实现缺口即由对拍发现并修复）。

## 环境隔离（运行前须知）

`run-parity.mjs` 两条腿都以**清除后的环境**启动子进程：`PI_SUBAGENT_PARENT_SESSION`
与 `RPI_SUBAGENT_PARENT_SESSION` 一律删除（`cleanSessionEnv`）。原因：上游腿的
`pi-args.ts` 会回退读取 shell 里的 `PI_*` 值，rust 腿读 `RPI_*`（桥接层把
`PI_SUBAGENT_*` 改名为 `RPI_SUBAGENT_*`）——若在外层 shell 导出过其中任一键，
它的值只到达一条腿，args 模式会有 8 例假 MISMATCH（`fork-session-file` 之外的
用例都走环境回退）。2026-08-15 前的 harness 未做此隔离；如需复现旧行为可手动
导出该键并观察误报。

## 已知不适用面

- 工具描述全文：入口从 workflowScript 换成结构化参数（ADR-0016），
  文案必然不同；custom 模板机制与 SAFETY 段结构由 crate 单测覆盖。
- 会话条目过滤：上游在子进程 context 事件内过滤，rpi 在 fork 分支文件
  上过滤（设计 §3.4），结果等价但层不同（e2e 场景 3 覆盖）。
