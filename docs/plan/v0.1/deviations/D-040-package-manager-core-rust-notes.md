# D-040：Packages 包管理核心 Rust 落地差异（hosted-git-info 子集 / semver 翻译层 / runner 注入等）

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已关闭
- **关联任务**：T14（W2：packages 核心 + install/remove/list 子命令）
- **级别**：实现细节偏离
- **发现日期**：2026-08-06

## 原文档约定

- 文档与章节：`docs/01-requirements.md` §7.6（Packages）、§3.2（子命令）；`docs/plan/v0.1/T14-packages-trust-export.md` W2 契约要点
- 原文约定：source 解析顺序、npm pinned/range maxSatisfying、git pinned reconcile、`-e` 临时 scope 0700、身份去重、`autoload:false` delta、过滤语法、`package.json#pi` manifest、`npmCommand` wrapper（PATH 解析、超时 10s、错误透传）、离线跳过、更新并发 4；子命令 install/remove(uninstall)/list 分组与 `(filtered)` 标记、`-a/-na`

## 实际实现与偏离原因

核心落 `crates/pir/src/core/package_manager.rs` + `core/git_url.rs` + `cli/package_command.rs`，全部契约要点就位；以下为实现层差异（不影响对拍契约的语义边界逐条列出）：

1. **hosted-git-info@9.0.3 用 Rust 子集自实现**（`core/git_url.rs`）：五内置 host（github/gist/bitbucket/gitlab/sourcehut）的 `extract`、github shorthand 修正、scp-style URL 修正、`correctProtocol` 协议表全部按 `lib/from-url.js`/`parse-url.js`/`hosts.js` 移植；URL 解析用 `url` crate（同为 WHATWG URL 标准）。未实现：LRU 缓存、`fromManifest`、`browse/tarball` 等 URL 生成模板（`parseGitUrl` 不消费）。
2. **npm semver 用 `semver` crate + 翻译层**：`||` 并集、`x/X/*` 通配、部分版本（`1.2` → `>=1.2.0 <1.3.0`）、完整版连字符范围（`a - b` → `>=a <=b`）、空格分隔交集翻译为逗号。已知语义边界：含 prerelease/build 元数据的 range 形式不翻译（视为无效 range，等价上游 `validRange(...) ?? undefined` 回退）；prerelease 匹配的 npm 特殊规则（同 tuple 预发布参与）与 Rust semver 的 req-prerelease 规则在极端输入下可能不同。pi 生态包版本基本都是精确版本或 `^`/`~` range，常见语法已逐条测试对齐。
3. **glob 引擎复用 T09 内置 matcher**（`skills.rs` 的 `glob_match`，支持 `*`/`?`/`**`）：上游 minimatch 的 brace 展开（`{a,b}`）与字符类（`[abc]`）不支持（T09 D-014 已记同构差异）；manifest glob 条目（`collectFilesFromManifestEntries`）用 `ignore`-crate WalkBuilder + `glob_match` 替代 `globSync`（`dot:false`/`nodir:false`/不读 ignore 文件的语义保留）。
4. **`PackageCommandRunner` trait 注入**替代上游直接 `spawnProcess` + 测试 `vi.spyOn`：CLI 用 `SystemPackageCommandRunner`（同步 `std::process`，capture 超时用轮询 + kill 实现 10s 网络超时），测试用 fake runner，无真实网络/进程。spawn 失败与信号终止的错误文案为 Rust 侧组合（`failed with code null` 保留上游 quirk；io::Error 文案与 Node ENOENT 文案不同），属引擎级文案差异（同 D-039 先例）。
5. **`getEnv()`（package-manager.ts:6-23）不移植**：那是 Bun 运行时 `/proc/self/environ` workaround；`std::process::Command` 默认继承父进程环境。
6. **legacy global npm root 查找（`npm root -g` / `pnpm list -g`）无缓存**：上游 `globalNpmRoot` memo 是纯性能优化，行为一致。
7. **`resolve()` 只移植包切片**（`resolvePackageSources`）：top-level settings 条目与 auto-discovery 已由 T09 落 `resource_loader.rs`/`skills.rs`；本模块输出 `ResolvedPackagePaths` 并提供 `to_package_resource_paths()` 转换，会话启动接线（含 `onMissing` 安装提示）留给后续波次。
8. **settings `packages` 数组中的畸形项**（非字符串亦非合法对象）在 add/remove 重写数组时会被丢弃（`get_packages` 的 `filter_map` 语义，T09 D-014 已有同型记录）；上游会原样保留。
9. **上游 `list` 接受并静默忽略位置参数**（`parsePackageCommand` 不分命令收集 `source`，list 分支不用它）：按上游保留该 quirk。
10. **`markPathIgnoredByCloudSync`** 直接 best-effort 调 `setfattr`/`xattr`（不过 runner），与上游 fire-and-forget 一致。
11. **目录遍历输出为确定性排序**（沿用 T09 约定；上游为 raw readdir 顺序）。
12. **信任解析走 T10 headless `resolve_project_trusted`**（无 UI 时 ask→false，无 `project_trust` 扩展事件）：包命令的交互式信任弹窗属 W4；`-a`/`-na` override、trust.json、`defaultProjectTrust` 链路已接线。
13. **CLI 输出为纯文本**（headless stdout；上游 chalk 样式仅 TTY），与 W6-C `update --models` 已确立的约定一致。

## 影响面

无（纯内部实现）：settings 线格式（`packages` 的 string/object 形状、camelCase 字段）与上游逐字段一致；安装目录布局（`~/.pir/agent/npm|git|tmp/extensions`、`.pir/npm|git`）与上游同构（`.pi`→`.pir` 为 ADR-0001 授权重命名）。

## 处置

- **回写位置**：`docs/01-requirements.md` §7.6（追加实现注记）；`docs/plan/v0.1/T14-packages-trust-export.md` 偏离表
- **回写日期**：2026-08-06
- **ADR**：不需要

## 终审补记（2026-08-07）

14. **`PIR_OFFLINE` 判定采 `main.ts` 的 `isTruthyEnvFlag` 语义**（`1`/`true`/`yes`，
    大小写不敏感）：上游 `package-manager.ts:42-46` 为 `Boolean(env)`（任何非空值即离线），
    与 `main.ts:476` 自身不一致；pir 统一走 `environment::is_truthy_env_flag`。
    即 `PIR_OFFLINE=0` 时上游 package-manager 视为离线、pir 视为在线（实现细节级，
    仅影响离线门判定）。
15. **`getNpmInstallPath` 吞 trust 错误**：`get_npm_install_path` 以 `unwrap_or_default`
    吞掉 project scope 未信任错误（上游 throw）；CLI 流程不可达（未信任项目
    SettingsManager 返回空 settings），保持 `PathBuf` 返回形状，已在代码注释注明。
16. **`CommandRequest::display()` 脱敏 URL userinfo**：上游错误前缀原样回显 argv
    （含 `https://user:token@host`）；pir 红线禁止凭据进错误消息，display 时将
    `scheme://user:pass@` 改写为 `scheme://***@`（仅当 userinfo 含 `:`；执行仍用原始
    argv；无 userinfo 时输出与上游逐字一致）。
