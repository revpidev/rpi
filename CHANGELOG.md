# Changelog

## [0.1.3] - 2026-09-03

### 新增

- **statusline 实时 token 计数**（V13-10，先行 PR）：`statusLine.liveTokens` 键开启流式期间 1Hz 级脚本重跑；stdin 新增 `rpi.live_output` 纯测量块；随行宿主八事件载荷转发 parity 补齐 + `ctx.sessionFile` additive host-call（ADR-0022），双开同 cwd 串数据治本（TE-D34 §1）。
- **subagents 父会话权威定位**（V13-02）：`parent_session` 优先 `ctx.sessionFile`，目录启发式降级为兜底并加固（mtime 下界 + stem 形状过滤），四消费点改用权威 session id（关闭 TE-D16）。

### 修复

- **流式请求总超时误杀活跃流**（V13-08，先行）：总 deadline 误映射 → 三段式超时（connect/headers/body 块间静默 idle，每 chunk 重置）；9 个 SSE adapter 全覆盖，codex / openrouter_images 两处有意例外。
- **write 大文件流式渲染 O(n²)**（V13-09，先行）：分层缓存（稳定前缀窗口跳重算 + 可见内容指纹跳重建 + repair_json 惰性化 + 拷贝瘦身 3→1）；400 行流式 2250ms → 245ms（9.2×）。
- **扩展 UI 换装撕帧**（V13-05，并发正确性）：widget 单锁原子化（跨容器 add-then-remove）+ selector 单锁 clear+add+set_focus——不再有缺失帧 / 裸 editor 帧。
- **流式渲染热路径**（V13-06）：MessageUpdate 队列连续段折叠保尾（同帧 K 条 delta → 1 次 update_content）+ update_content 引用化消除调用侧深拷贝。
- **subagents 子进程事件落盘治理**（V13-01）：events.jsonl / transcript 持久单句柄（50MiB 上限静默丢弃）+ status.json 100ms 写入门控（终态/state 变化同步落盘）。
- **子进程 update 帧节流**（V13-03）：事件路径帧签名门控（同活动跳推 + 1s 心跳 + 首帧/终态必推）+ subagent_wait 轮询脏检查——50 相同事件 toolUpdate 从 50 帧压到 3 帧。
- **smart-fetch 进度节流**（V13-04）：body_progress 100ms/64KiB 门控 + batch 快照 1% 签名短路；终态帧与帧形状零变化。

### 内部

- **低档杂项清理**（V13-07）：mcp-adapter `!command` 秘钥解析改 spawn_blocking；无 UI 期重试免推 status bar；`getAllTools` 惰性查询；statusline 变化 tick 复用 fetch_ctx（12→6 host call）；TUI 每帧尺寸读取 4→1 次 ioctl。
- 偏离登记汇总：TE-D16 关闭、TE-D35/36/37、D-088/089/090/091。
- M0 收口：两支先行 PR（`fix/stream-idle-timeout-write-perf` / `feat/statusline-live-token-count`）合入 main + 门禁清账（clippy 1.97 lint）。
- workspace 版本 bump 0.1.3 + Cargo.lock 同步；全量门禁 5367 用例零失败。

## [0.1.2] - 2026-08-19

### 新增

- **第一方插件 rpi-ext-statusline**：CC 兼容脚本式自定义 statusline（L0 原生插件）。`settings.json` 写 `statusLine` 键即启用（命令 + padding/裁剪参数），两档 placement；脚本按 CC statusline JSON 协议 stdin/stdout 驱动，零新增 ABI。含实机追补：切换模型/思考档位/分支即时刷新、新会话 transcript latch 竞态修复、数据指纹轮询自愈。

### 修复

- `/new` 与 `/resume` 切换 session 后 extension host 丢失 UI bridge——mcp 状态栏消失、MCP 工具审批被静默拒绝（#1）。
- `models.json` 配置 apiKey 后仍强制要求 `auth.json`——字面 key 被误当环境变量名，改按上游 config-value DSL 解析（#3）。
- `/changelog` 恒显示 "No changelog entries found."——changelog 资产从未落地；现 `CHANGELOG.md` 嵌入二进制 + `parseChangelog` 移植 + onboarding 显示半链（#5）。
- `model_select` 事件从未发出——切换后捕获 previous 恒等短路。
- onboarding 启动 header 品牌残留：上游 "Pi" 逐字文案改 rpi 实际能力表述，去掉 rpi 未带的 docs 查询承诺、指向官网（#7）。

### 内部

- subagents 编排 skill 文档与 prompt 模板按结构化入口本地化（ADR-0021）：去除 `workflowScript` 教学与未实现机制段落，安装侧 `.rpi-layout-version` marker 自动升级旧版；工具描述补齐 `tasks`/`steps` 组合入口（ADR-0018 决策 5）。
- registry / package_manager / package_command rustfmt 清账（纯格式化）。

## [0.1.1] - 2026-08-16

### 新增

- **扩展分发与安装**：`rpi install <name>`（revpi.dev registry 渠道，semver 选版 + sha256 校验）、`rpi install github:<owner>/<repo>`（Release artifact 渠道）、`.rpix` 包格式与原子安装；`remove` / `list` / `update` 全链路支持。
- **第一方插件**：rpi-ext-mcp-adapter（MCP 客户端适配器）、rpi-ext-subagents（结构化子代理委派）、rpi-ext-smart-fetch（web_fetch 全管线）随主版本发布，官网索引自动收录。
- **上游对齐 Pi v0.84.1**：rpi-ai 消息类型与流终止语义、provider 修复簇、models refresh 事务化、rpi-tui 渲染器重构 / LaTeX 与 Mermaid 渲染 / 布局引擎 / 全屏渲染器（alt screen / mouse / kitty）、UI 模式接线。
- 官网 revpi.dev：扩展索引 API、下载边缘代理、插件目录页。

### 修复

- 主屏渲染器超宽行 `panic!` 杀死会话、满宽行换行漂移花屏——改截断继续渲染 + 悲观宽度保守截断（ADR-0020 / D-086）。
- `$$`/`\[` 公式块内孤行 `=`/`-` 被误解析为 setext 标题，公式在进入数学渲染前被切断——parse 前 shadow source 等长改写（D-078 补记）。
- LaTeX 遇符号表不认识的命令时整块公式回退原文——按 KaTeX 清单全量补全 78 项缺口（`\blacksquare`/`\Box`/箭头长尾/否定关系单宏/带参宏降级渲染，D-087）。
- 词级 diff 多字节字符（中文/全角）`not a char boundary` panic 杀死渲染线程——按末字符长度推进 trim 边界。
- 多 native 插件共载失败（abi_stable 按类型 memoize 根因，改 per-path 加载）；SSE 行上限对齐 10MiB；全屏热切换输入失效与 `/settings` 卡死。

## [0.1.0] - 2026-08-15

- Initial release：交互 TUI / JSON-RPC / print 三模式，多 provider 模型运行时（rpi-ai），agent 会话与压缩，技能 / 提示模板 / 主题资源体系，bash / read / edit / write 内置工具，扩展宿主（wasm 沙箱 + native L0）。
