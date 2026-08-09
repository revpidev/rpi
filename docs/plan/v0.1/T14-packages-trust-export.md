# T14：可选工具 / Packages / Trust / Export / llama / 更新

- **状态**：已完成
- **里程碑**：M7
- **依赖**：T09（资源管线与 settings）、T10（CLI 与子命令骨架）
- **上游对照**：`docs/packages.md`、`docs/settings.md`（trust/telemetry 段）、`docs/security.md`、`docs/llama-cpp.md`、`packages/coding-agent/src/core/{tools/{grep,find,ls},package-manager,trust-manager,project-trust}.ts`、`src/package-manager-cli.ts`、`src/modes/interactive`（share/export 段）
- **需求章节**：§3.2、§4.5（可选工具）、§7.6、§7.8、§10（endpoint/单文件）、llama.cpp 与 export/share（§6.4、§8.4 相关项）
- **预估**：1.5–2.5 人月

---

## 目标

交付可选三工具、包管理、项目信任、导出分享、llama.cpp 集成与产品 endpoint 配置化，
达到 M7「Provider/OAuth 之外的产品能力」验收口径。

## 范围

### In

- **可选工具 grep / find / ls**（`rpi/src/tools/`，**Rust 原生实现**，`ignore`/`globset` crate，**不引入外部 rg/fd 下载**，ADR-0003 §2）：
  - grep：rg 等价行为（`--json --line-number --color=never --hidden` 语义）；输出格式：匹配行 `path:lineno: text`、上下文行 `path-lineno- text`（grep.ts:264-265）；默认 limit=100 匹配（达标即停）；单行截 500 字符；context 回读补行；50KB 截断；`limit=N*2` 提示
  - find：fd 等价行为（`--glob --color=never --hidden` 语义）；repo 外 `--no-require-git` 等价；含 `/` pattern → full-path + `**/` 前缀规则；默认 limit=1000；相对化输出、目录尾斜杠；固定忽略 node_modules/.git
  - ls：默认 limit=500；大小写不敏感排序；目录加 `/`；含 dotfiles；stat 失败跳过；50KB 截断
- 子命令（需求 §3.2）：`rpi install` / `remove`(`uninstall`) / `list`（User/Project 分组、`(filtered)` 标记）/ `update`（裸=self+提示；`--self`/`pi`、`--all`、`--extensions`、`--models`、`--extension` 互斥矩阵；`--force`；release note 渲染）/ `config`（TUI Tab 切 scope；`-l` 要求 trust）；子命令先于主 parseArgs 分流；各支持 `-a/-na`
- Packages（需求 §7.6）：source 解析顺序（`npm:` → 本地路径 → git URL → 回退本地；**裸名按本地路径**）；npm 精确版本=pinned 跳过更新、range semver maxSatisfying；git **pinned ref 不移动但 update 会 reconcile**（reset+clean+依赖安装）；`-e` 临时 scope 安装 `~/.rpi/agent/tmp/extensions`（0700）；身份去重（npm 按名 / git 按 host/path / local 按绝对路径）；`autoload:false` delta；过滤语法 glob/`!`/`+`/`-`；`package.json#pi` manifest；`npmCommand` wrapper；离线跳过、网络超时 10s、更新并发 4（调系统 npm/git）
- Project trust 产品化（需求 §7.8）：`trust.json`（路径→bool、父链最近条目、排序写盘、lockfile）；触发条件资源清单（`.rpi/` 7 类 + 祖先 `.agents/skills`，`~/.agents/skills` 豁免）；解析优先级链（CLI override > 扩展 `project_trust` 事件 > trust.json > defaultProjectTrust；**无 UI 时 ask=false**）；两阶段加载接线收尾；`/trust` 只写不重载；`rpi update` 永不提示
- Export / share：HTML 导出（`--export` 与 `/export`，模板结构对齐上游）、JSONL export、 gist share（**shell 调 `gh gist create --public=false`** + `RPI_SHARE_VIEWER_URL` 拼接，endpoint 可配置）
- llama.cpp 集成：内置 hidden 扩展（`/llama` 命令经扩展注册，**非内置 slash**）；`/login llama.cpp` 与 `LLAMA_BASE_URL`/`LLAMA_API_KEY`；HF 搜索下载 `owner/repo[:quant]`、HF_TOKEN 查找、永不静默卸载/删除（`docs/llama-cpp.md`）
- 产品 endpoint：版本检查 / telemetry / 远程 catalog 在 settings / `RPI_*` 可配置、可关闭（ADR-0002 §8）；`enableInstallTelemetry`(true)/`enableAnalytics`(false opt-in)
- 单文件发布链路验证：musl + rustls 构建、自更新（`update --self`）流程

### Out

- 扩展包（Wasm 包）的安装管理（T15；本任务 packages 机制面向声明式资源包）
- trust 弹窗交互（T12 已含；本任务提供底层决策链）

## 开发要点

- grep/find/ls 的输出格式、limit、截断、排序为对拍契约；原生实现须逐场景与上游 rg/fd 输出比对
- 调系统 npm/git 时注意 PATH 解析、超时与错误透传；`npmCommand` wrapper 语义与上游一致
- 两阶段加载的时序敏感：信任前后资源集合的差集有测试锚点
- telemetry 默认策略：可关、可改 URL，日志不含敏感信息
- HTML 导出模板与上游输出结构对齐（对拍可比对结构而非样式细节）

## 进度跟踪

- [x] 设计细化
- [x] 实现
- [x] 自测
- [x] 门禁验收
- [x] 文档回写

## 自测清单

- [x] grep/find/ls：各 limit、截断、排序、忽略规则与上游输出 fixtures 对拍（W1：`tests/optional_tools_test.rs` 39 用例（grep 19 / find 10 / ls 7 / wiring 3）；rg 15 / fd 10.4 实机交叉验证逐行 diff 一致，证据见 D-039「影响面」节）
- [x] 子命令全集：install/remove/list/update/config 各路径（本地包 + 模拟 npm/git 源，PackageCommandRunner 注入）；update 互斥矩阵（W2/W3：`package_manager.rs` 70 单测 + `update_tests` 模块 + `config_command.rs` 解析测试）
- [x] pinned ref reconcile 语义；npm 精确版本跳过；全局与项目级安装位置正确（W2/W3：`package_manager.rs` 单测锚定，见 D-040/D-041）
- [x] trust：决策优先级链各分支、触发条件清单、两阶段时序、非交互不提示（ask=false）（W4：`trust_manager.rs` 单测 + `tests/resource_loader_test.rs::set_project_trusted_loads_second_phase_resources` 两阶段差集锚点）
- [x] HTML/JSONL export 输出结构与上游对拍（W5：模板资产逐字节对拍 + SessionData JSON 结构断言；上游无 HTML golden fixtures，其测试为 template.js 源码断言，由字节一致性测试覆盖意图）
- [x] gist share：mock gh 调用与 URL 拼接（W5：ShareRunner mock 全路径 + `RPI_SHARE_VIEWER_URL` 拼接/回退测试）
- [x] endpoint 配置化：自定义 URL 生效、关闭后不产生请求（W6a：`resolve_endpoint` 纯函数测试 + update 流程注入 transport call-count 断言 + telemetry 三门禁零请求断言）
- [x] musl release 单文件构建通过；`update --self` 流程 dry-run 验证（W7：**豁免 musl**——用户决策本次版本不强制 musl 单文件（环境无 sudo 装不了 musl 工具链）；gnu release 构建（29MB）通过，`update --self` dry-run 两路径（新版本→打印 Download 地址 exit 1；同版本→"rpi is already up to date" exit 0）实测通过）

## 门禁验收

通用门禁 G1–G7 全过（G4 重点：grep/find 无外部二进制下载）。

任务特有标准：

- [x] 需求 §3.2、§4.5（可选工具）、§7.6、§7.8、§10 逐条核对有锚点（映射表见下方验收记录 G3）
- [x] llama.cpp 集成 smoke（W6b：本机无 llama.cpp 路由器，记录豁免；契约由 17 个 loopback 集成测试 + 编排流 fake-UI 测试覆盖，含 SSE 进度/取消恢复/HF 搜索下载/永不静默卸载路径）
- [x] 单文件发布物 smoke：`--help` / `--version` / `--list-models` / 一次 faux 对话（W7 全过；终审 2026-08-07 复验 `--version`=0.1.0 与 `--help` 正常）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| D-039 | 可选工具 grep/find/ls Rust 原生落地差异（ignore/globset 替代 rg/fd；ls 码位排序；原生错误文案等 8 项） | 已关闭 |
| D-040 | Packages 包管理核心 Rust 落地差异（hosted-git-info 五 host 子集自实现；semver crate + npm range 翻译层；PackageCommandRunner 注入；resolve 仅包切片；list 忽略位置参数 quirk 保留等 13 项；终审补记 3 条：RPI_OFFLINE 采 main.ts isTruthyEnvFlag 语义、getNpmInstallPath 吞 trust 错误、display() 脱敏 URL userinfo） | 已关闭 |
| D-041 | update 编排/自更新/版本检查 Rust 落地差异（worker pool 并发；`--all` 冲突补齐；原生二进制自更新按 bun-binary 结局；endpoint 集中常量留 W6 等 11 项） | 已关闭 |
| D-042 | config 子命令与 config-selector 真接线差异（`ScopedResolvedPaths` + `resolve_all`；settings 持久化移植；ctrl+c→onExit 死代码 quirk 保留等 7 项） | 已关闭 |
| D-043 | Project trust 产品化收尾 Rust 落地差异（`ProjectTrustContext` 闭包化；resolve 同步 + 事件预发射参数；启动弹窗泵线程同步；`getProjectTrustOptions` 落核心层；switchSession 异 cwd 提示析出为 D-044） | 已关闭 |
| D-044 | 交互模式 switchSession 异 cwd 信任提示降级为 headless 判定（行为级，ADR-0006；T15 异步选择器桥接就位后接线关闭） | 已回写 |
| D-045 | HTML export / gist share Rust 落地差异（模板资产 include_str! 内嵌；renderedTools/ANSI→HTML 管线不移植；theme vars 按键排序；去 currentThemeName 全局；export_to_html 同步化；ShareRunner 注入 + UiCommand drain 结算；RPI_SHARE_VIEWER_URL 集中 config） | 已关闭 |
| D-046 | 产品 endpoint 配置化与 install telemetry Rust 落地差异（统一 resolve_endpoint；三个 RPI_*_URL + 三个 settings 键 rpi 专有；启动版本检查与 update 流程接线；changelog 触发以版本不等近似待 T15；catalog 解析器就位待注册波次等 8 项） | 已关闭 |
| D-047 | llama.cpp 集成与 /login api-key 通路 Rust 落地差异（内置 hidden 扩展登记表 + dispatch fall-through 替代 T15 宿主；provider 进程级单例 + services drain 注册；LlamaView 双半结构；CancellationToken/select!/generation debounce；reqwest 连接错误分类；Models/ModelRuntime login/logout/get_provider_auth 补齐 + supports_login 标志；OAuth 对话框仍 stub 属 T13 遗留） | 已关闭 |

## 验收记录

- 验收日期：2026-08-07
- 验收人：终审波次（单人流程自证，gates.md §1；命令输出摘要见下）
- G1 构建/静态检查：通过。`cargo build --workspace` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --all -- --check` 全过（终审修复后复跑：`Finished dev profile`，clippy 无警告，fmt `FMT_OK`）
- G2 测试：通过。`cargo test --workspace` 连续两遍均 `passed=3688 failed=0 ignored=0`（live 测试未设 `RPI_LIVE_TEST` 默认跳过且无失败）；第二遍专看 flake——W5 报告的 `test_edit_crlf_match_lf_oldtext` 偶发失败已定位为测试基建竞态（`tests/edit.rs` 的 `TempDir` 用 pid+纳秒命名，并行测试同一时钟滴答下撞目录、先结束一方的 Drop 删掉另一方文件），终审已加进程内原子计数修复，本两遍及此前 7 轮隔离复跑均绿
- G3 对拍：通过。对拍证据汇总：
  - **grep/find/ls（W1，§4.5）**：`tests/optional_tools_test.rs` 39 用例（grep 19 / find 10 / ls 7 / wiring 3）锚定契约（limit/截断/分隔符/忽略规则/排序）；rg 15 / fd 10.4 实机交叉验证混合树逐行 diff 一致（find 仅差契约规定的 node_modules/.git 剪枝；ls 排序差异已立 ADR-0005）——证据记录于 D-039「影响面」节
  - **export 模板（W5）**：`core/export_html.rs::embedded_assets_match_upstream_byte_for_byte` 对 template.html/template.css/template.js/marked.min.js/highlight.min.js 五资产与 `external/pi` 逐字节断言（运行时直读上游文件比对）；SessionData JSON 结构断言；`tests/export_cli_test.rs` CLI 端到端
  - **trust 两阶段（W4，§7.8）**：`tests/resource_loader_test.rs::set_project_trusted_loads_second_phase_resources` 信任前后资源差集锚点
  - **llama（W6b）**：`tests/llama_extension.rs` 17 个 loopback 集成测试（SSE 进度/取消恢复/HF 搜索下载/永不静默卸载）
  - 需求逐条锚点映射表：

    | 需求条目 | 实现锚点 | 测试锚点 |
    |----------|----------|----------|
    | §3.2 install/remove/list | `core/package_manager.rs`、`cli/package_command.rs` | package_manager 70 单测（安装路径/身份去重/list 分组） |
    | §3.2 update（互斥矩阵/--force/release note/self） | `cli/package_command.rs`、`core/version_check.rs`、`config.rs` self-update 段 | `package_manager.rs::update_tests`、W7 `update --self` dry-run 两路径实测 |
    | §3.2 config | `cli/config_command.rs`、`components/config_selector.rs` | `config_command.rs::test_parse_config_flags` 等 |
    | §4.5 grep/find/ls | `tools/{grep,find,ls}.rs` | `tests/optional_tools_test.rs` 39 用例（grep 19 / find 10 / ls 7 / wiring 3） + rg/fd 实机交叉验证 |
    | §7.6 Packages（source 解析/pinned/semver/tmp 0700/过滤/去重/离线/并发 4） | `core/package_manager.rs`、`core/git_url.rs` | package_manager 单测（`translate_range_token`/`resolve_managed_path` 穿越拒绝/reconcile/tmp 权限） |
    | §7.8 Trust（trust.json/父链/优先级/两阶段/非交互 false） | `core/trust_manager.rs`、`modes/interactive/startup_ui.rs`、`app.rs` 两阶段 | trust_manager 单测 + resource_loader 两阶段差集测试 |
    | §10 endpoint 配置化/可关闭 | `core/telemetry.rs`、`core/version_check.rs`、`core/remote_catalog_provider.rs`、`settings_manager.rs` 三键 | resolve_endpoint 纯函数测试 + 零请求断言（D-046） |
    | §10 单文件发布 | gnu release 29MB（musl 本次豁免，用户决策） | W7 smoke：`--version`/`--help`/`--list-models`(faux)/faux 对话/`update --self` dry-run |
    | §6.4/§8.4 export/share | `core/export_html.rs`、`core/share.rs` | 字节对拍测试 + export_cli_test + ShareRunner mock 全路径 |

- G4 红线：通过（终审逐条复核）
  - [x] `external/pi` 干净（`git status --porcelain` 为空）且 HEAD=`2efa728d2ee90ef597626e96b1e28ef2b279f07c`
  - [x] 未引入 JS/TS 执行能力（T14 新增依赖仅 globset/semver/sha2，纯 Rust）
  - [x] 未读写 `~/.pi`/`.pi`（全 diff grep 零命中）
  - [x] Session 存储仅 JSONL（无新后端）
  - [x] token 估算算法与常量未触碰（diff 不含 token/usage 文件）
  - [x] 非测试代码无 `unwrap()`/`expect()`（全 diff 扫描：生产代码零命中；新增仅 `huggingface.rs` 三处正则编译 expect 带不变式注释，符合豁免；test_support 为既有测试基建）
  - [x] 日志/错误消息无凭据：终审发现 `CommandRequest::display()` 会原样回显 git URL userinfo（上游 parity）——已修复为 `scheme://***@` 脱敏（附单测），补记 D-040 第 16 条；HF_TOKEN 仅进 Authorization header（llama 独立抽查确认）
  - [x] 无 server/evals/bun 对应物等范围排除项
  - [x] grep/find 无外部 rg/fd 二进制下载（纯 ignore/globset 实现，三文件零 `std::process` 调用）
  - [x] session 文件写入未加锁（锁仅限 auth/settings/trust，diff 未触碰 session 写路径）
- G5 线格式：通过。settings 新键 `versionCheckUrl`/`telemetryUrl`/`modelCatalogUrl` 为 rpi 专有 camelCase（ADR-0002 §8，D-046 已登记）；`enableInstallTelemetry`/`enableAnalytics`/`defaultProjectTrust` 与上游 settings-manager.ts:104-105 逐字段一致；trust.json 为路径→bool 扁平映射（无字段命名面）；`package.json#pi` manifest 键与上游一致；settings_manager  serde 结构全 `rename_all = "camelCase"`
- G6 文档同步：通过。T14 全部 15 个新源文件均有溯源头（上游路径 + 0.82.1/2efa728）；`01-requirements.md` §3.2/§4.5/§7.6/§7.8/§10、`02-design.md` §6.1/§8/§12 回写在 W1–W6 完成，终审抽查一致；终审修复已补记 D-015/D-040/D-045/D-047
- G7 偏离闭环：通过。D-039~D-047 全部登记在案（一事一记 + 登记表）；D-039 第 1 条行为级 → ADR-0005、D-044 行为级 → ADR-0006；验收通过后 D-039~D-043、D-045~D-047 状态置「已关闭」（登记表与各偏离文件同步），D-044 保持「已回写」（T15 异步选择器桥接就位后接线关闭）
- 结论：通过

### 终审修复清单（2026-08-07）

1. `tests/edit.rs` TempDir 命名加进程内原子计数，消除并行测试目录碰撞竞态（即 W5 报告的 CRLF flake 根因）。
2. `core/package_manager.rs::CommandRequest::display()` 脱敏 URL userinfo（凭据不进错误消息，红线），附单测。
3. `core/package_manager.rs::is_offline_mode_enabled` 收敛到 `environment::is_truthy_env_flag` 并注明与上游 package-manager.ts 的语义差（D-040 补记 14）。
4. `core/package_manager.rs::get_npm_install_path` 吞 trust 错误补注释（D-040 补记 15）。
5. `/share` 临时 HTML 改唯一子目录 `rpi-share-{pid}-{nanos}/session.html`（消除双实例并发覆盖→串号发布风险；basename 不变保持 gist 文件名），结算经 `share::cleanup_share_tmp_file` 连带清目录；测试断言同步（D-045 补记）。
6. D-015 补记：`auth_guidance` 登录帮助文案无条件打印 `{exe}/docs/*.md`（上游同构；单文件形态路径无效但不影响功能，同根因已由 D-015 第 4 条登记，不单列新偏离）。
7. D-047 补记：llama HF 搜索缓存不跨进入存活（纯性能差异，独立抽查结论无阻断/应修项）。

### 遗留（带 T15）

- D-044：switchSession 异 cwd 信任提示接线（ADR-0006，T15 异步选择器桥接后关闭）。
- D-046：changelog 触发以版本不等近似（待 T15  changelog 资产）。
- D-047：`/llama` 内置 hidden 扩展登记表迁移到 T15 真扩展宿主（关闭条件见 D-047 文件）。
- 既有非 T14 项：`core/environment.rs:171-181` `share_viewer_base_url` 与 config.rs 重复且生产无调用方（T13 遗留死代码，建议 T15 清理）；gh stderr 原样透传、theme 色值注入导出 CSS 等均为上游 parity 仅提示项，不处理。

## 审查修复波次补记（2026-08-07，验收后修复）

对验收后独立全面审查发现的问题（均无 blocker）的修复记录：

- **测试零网络（M1）**：interactive 模式单元测试原经生产 transport 向 pi.dev 发真实
  遥测/版本检查请求（代理陷阱实测）；现 `InteractiveUi` 双 transport 注入点 +
  `test_support::install_noop_product_transports`，锚点
  `init_and_run_make_no_product_network_requests`。见 D-046 修复补记。
- **llama SSE 15s 超时（M2）**：`watch()` 改走无总超时 `stream_http` client；SSE 帧
  改字节级累积解码；watcher 复用宿主 client；正则 LazyLock。见 D-047 修复补记。
- **ls 取消粒度（M4）**：条目循环内逐条检查 CancellationToken。见 D-039 修复补记。
- **config_selector 项目写错误吞没（M5）**：写失败 eprintln + toggle 不翻转；
  settings 写原子化（temp+rename）。见 D-042 修复补记。
- **trust 存储健壮性**：trust.json 原子写、lockfile 释放即删（proper-lockfile 对齐）、
  key 排序 UTF-16 码元、`home_dir()` passwd 回退。见 D-043 修复补记。
- **share**：失败路径清理临时目录、导出文件 0600/目录 0700、abort 测试等待条件修正。
  见 D-045 修复补记。
- **版本比较**：剥 `v`/`V`/`=` 前缀；usage 文案与 APP_NAME 绑定测试。见 D-041 修复补记。
- **grep 流式化**：整文件读入改 64KB 块流式搜索（NUL 抑制带回滚）；新增 `.git`
  内容搜索测试锚点。见 D-039 修复补记。
- **文档数字更正**：`optional_tools_test.rs` 实际 38 用例（原记 50），修复后 39。
- **复验**：`cargo build --workspace` / `clippy --all-targets -D warnings` /
  `fmt --check` 全过；`cargo test --workspace` 全量 `3692 passed / 0 failed`
  （原 3688 + 新增 4 锚点）。
