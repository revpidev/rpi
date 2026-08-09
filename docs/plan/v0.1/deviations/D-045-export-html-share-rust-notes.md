# D-045：HTML export / gist share 的 Rust 落地差异

- **状态**：已关闭
- **关联任务**：T14（W5）
- **级别**：实现细节偏离
- **发现日期**：2026-08-07

## 原文档约定

- 上游基准：`packages/coding-agent/src/core/export-html/index.ts`、
  `src/modes/interactive/interactive-mode.ts`（`handleExportCommand` /
  `handleShareCommand` / `getPathCommandArgument`）、`src/config.ts`
  （`getShareViewerUrl`）、`src/main.ts`（`--export`）@ 0.82.1（2efa728）。
- 需求 §3.2（`--export <file> [output.html]`）、§6.4（import/export JSONL、
  HTML export、gist share）。
- T14 任务文件 W5：模板结构对齐上游；gist share 走
  `gh gist create --public=false` + `RPI_SHARE_VIEWER_URL` 拼接；gh 进程调用可注入。

## 实际实现与偏离原因

1. **模板资产编译期内嵌**：`template.html/.css/.js` 与
   `vendor/{marked,highlight}.min.js` 从上游逐字节复制到
   `crates/rpi/src/core/export_html/` 并 `include_str!` 内嵌（单文件二进制，
   ADR-0002 §8），替代上游运行时 `getExportTemplateDir`（config.ts:412-419）
   读包目录；无 `RPI_PACKAGE_DIR` 式模板覆盖。字节一致性由
   `embedded_assets_match_upstream_byte_for_byte` 测试钉死。
2. **`ToolHtmlRenderer` / `renderedTools` 未移植**：上游用扩展 TUI 渲染器经
   ANSI→HTML 管线（`tool-renderer.ts` / `ansi-to-html.ts`）预渲染自定义工具；
   rpi 无 JS 扩展渲染器，`renderedTools` 永不出现，viewer
   （`template.js` 内 `renderedTools?.[call.id]` 的可选链）回退通用工具渲染。
   内置 bash/read/write/edit/ls 仍由模板直接渲染，与上游一致。
3. **theme vars 按键排序**：`--key: value;` 行按 key 字典序输出（Rust
   `HashMap` 无插入序，排序保证确定性）；上游为主题 JSON 插入序。仅影响
   CSS 变量声明顺序，不影响取值。
4. **主题回退链去全局**：`themeName ?? currentThemeName ?? getDefaultTheme()`
   （theme.ts:1023）中的 `currentThemeName` TUI 全局未移植；`None` 直接落到
   终端探测默认主题（`get_default_theme()`）。
5. **异步签名同步化**：`AgentSession::export_to_html` 为同步 fn（纯 CPU +
   文件 IO），RPC `export_html` 调用点去 `.await`；线协议不变。
6. **`/share` 并发模型**：gh 调用集中在 `core/share.rs` 的 `ShareRunner`
   trait（W2 `PackageCommandRunner` 模式；测试注入 mock）。
   `SystemShareRunner::gist_create` 以 50ms 轮询 `try_wait` +
   `Arc<AtomicBool>` 取消标志 kill 子进程，替代上游 `proc.kill()`；loader
   abort 与 worker 完成经 `UiCommand::ShareAbort` / `ShareCompleted` 在
   drain 结算（T12 组件锁契约），替代上游 promise 内联续体。share 进行中
   再开其它选择器等极端交错下 abort 无条件恢复编辑器，与上游
   `restoreEditor()` 口径一致。
7. **`RPI_SHARE_VIEWER_URL`**：`PI_` → `RPI_` 前缀改名（ADR-0001），读取集中在
   `config::get_share_viewer_url`（唯一 env 读取点，W6 endpoint 配置化口子）。

## 影响面

无（协议 / session 格式 / 扩展 API / TUI 行为均不变）。导出 HTML 的模板
与 `SessionData` schema 与上游逐字节/逐字段一致；仅自定义工具的预渲染
保真度差异（当前无 JS 扩展，无实际影响）。

## 处置

- **回写位置**：`02-design.md` §12 映射表（export-html / share 两行）+
  §6.1 启动管线注记（`--export` 已落地）+ RPC 注记（`export_html` 已接
  真实导出）；T14 偏离表
- **回写日期**：2026-08-07
- **ADR**：不需要

## 终审补记（2026-08-07）

- **`/share` 临时 HTML 改为唯一子目录**：上游固定 `os.tmpdir()/session.html`，同用户
  两个 rpi 实例并发 `/share` 会互相覆盖导出（可把 B 会话发布成 A 的 secret gist）。
  rpi 改为 `{tmpdir}/rpi-share-{pid}-{nanos}/session.html`（basename 不变，gh 以其为
  gist 文件名，对外可见行为与上游一致），结算时经
  `share::cleanup_share_tmp_file` 连带删除目录（`remove_dir` 拒删非空目录，父目录
  带 `rpi-share-` 前缀校验，无误删面）。

## 审查修复补记（2026-08-07 审查修复波次）

1. **`/share` 失败路径清理**：`handle_share_command` 建唯一子目录后若
   `export_to_html` 失败（in-memory 会话 / IO 错误），此前提前 return 泄漏空
   `rpi-share-*` 目录；现失败路径调用 `cleanup_share_tmp_file` 连带删除。
2. **临时导出文件权限**：`session.html`（含私有会话内容）在共享多用户机器上默认
   0644 世界可读（上游同暴露）。新增 `share::restrict_share_tmp_file_permissions`
   （unix 下文件 0600 + 目录 0700，best-effort），`handle_share_command` 导出成功后
   调用；属上游 parity 残余的修复，不改变对外行为。
3. **未知主题吞错注释登记**：`generate_html`/`generate_theme_vars` 对未知主题名
   `unwrap_or_default` 回退派生色（上游 throw「Theme not found」）——两个调用点均
   已过滤主题名（CLI 无主题→恒内置默认；交互路径 `get_theme_by_name` 过滤），实际
   不可达，仅加注释说明，行为不变。
4. **`share_command_abort_cancels` 测试等待条件修正**：原测试轮询旧的固定路径
   `{tmpdir}/session.html`（D-045 补记前的布局），在新唯一子目录布局下永不命中、
   空转 1s；改为经 mock runner 的进入标志等待 worker 在途。

## T15 W7 复核结论（2026-08-08）

`ToolHtmlRenderer` / `renderedTools` 维持「不移植」登记，复核理由：W4 落的
扩展渲染是 TUI 声明式组件树（JSON 描述符，extension-abi.md §7），而上游
HTML export 的预渲染管线吃的是 ANSI 文本（`ansi-to-html.ts`）；rpi 的扩展
渲染树没有 ANSI 形态可直接喂 export，移植该管线需另起 JSON 树→HTML 渲染器，
超出 parity 范围且无实际消费者（扩展工具在导出 HTML 中回退通用渲染，与
viewer 的可选链行为一致）。如未来需要，另立任务而非本偏离解决。
