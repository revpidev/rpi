# Changelog

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
