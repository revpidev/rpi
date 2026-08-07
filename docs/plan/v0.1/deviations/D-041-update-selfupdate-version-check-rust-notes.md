# D-041：update 编排 / 自更新 / 版本检查 Rust 落地差异

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已关闭
- **关联任务**：T14（W3：`pir update` 全目标）
- **级别**：实现细节偏离
- **发现日期**：2026-08-06

## 原文档约定

- 文档与章节：`docs/01-requirements.md` §3.2（子命令）、§7.6（Packages）；`docs/plan/v0.1/T14-packages-trust-export.md` W3 契约要点
- 原文约定：update 互斥矩阵（裸=self+提示；`--self`/`pi`、`--all`、`--extensions`、`--models`、`--extension`）、`--force` 语义、release note 渲染、更新并发 4、离线跳过、git pinned reconcile、npm pinned 跳过、`useSavedProjectTrustOnly: true` 不弹窗

## 实际实现与偏离原因

核心落 `crates/pir/src/core/package_manager.rs`（update 编排 + `resolve_all`）、`core/version_check.rs`、`config.rs`（self-update 段）、`cli/package_command.rs`（update 分支）。契约要点全部就位；实现层差异：

1. **并发模型**：上游 `runWithConcurrency` 是 promise worker pool；pir 的包管理方法是同步的，用 `std::thread::scope` worker pool 实现（结果保序，上限 4 各自独立）。多重失败时返回**任务序第一个**错误（上游是时间上第一个 reject）；单失败语义一致。
2. **`--all` 冲突检查补齐**：W6-C 的解析器漏了上游 package-manager-cli.ts:321-327 的两条 `--all` 冲突（`--all` + `--self/--extensions/--models/--extension`、`--all` + 位置参数）；W3 补上并修正了既有测试的消息优先级期望（`--all` 消息先于 `--models` 消息）。
3. **自更新机制按上游忠实移植**（npm/pnpm/yarn/bun 全局重装 + 改名先卸载），**不含 GitHub release 二进制下载**——上游对编译二进制（bun-binary）同样只打印 releases 页提示。pir 恒为原生二进制：`detectInstallMethod` 只查当前可执行文件路径（上游还拼 `__dirname`；pir 无对应物），无法区分「bun-binary」与「unknown」两种上游结局，统一按 bun-binary 处理（打印 `Download from: {SELF_UPDATE_DOWNLOAD_URL}`）。
4. **`PACKAGE_NAME = "pir"` 占位常量**：pir 尚无已发布的 npm 包；release endpoint 的 `packageName` 改名重定向机制完整保留。endpoint 集中在 `core/version_check.rs::LATEST_VERSION_URL` 与 `config.rs::SELF_UPDATE_DOWNLOAD_URL` 两个常量，W6 的 `PIR_*` endpoint 配置化在这两处接口子。
5. **版本检查 HTTP 可注入**：`LatestVersionTransport` trait（生产为 reqwest+rustls；测试为脚本化 stub）；`PIR_OFFLINE` 语义复用 package-manager 的 truthy 判定（上游是「非空即真」，pir 与全仓 PIR_OFFLINE 口径一致为 `1/true/yes`，同 D-040 的口径选择）。UA 为 `pir/{version} ({os}; rust; {arch})`（D-038 先例）。
6. **release note 渲染**走 pir-tui Markdown 组件 + identity 主题（headless 纯文本约定，同 D-040 #13）；宽度取 stdout ioctl TIOCGWINSZ，非 TTY 回退 80（上游 `process.stdout.columns ?? 80`）。
7. **`isSelfUpdatePathWritable`** 用 `libc::access(W_OK)`（unix）；非 unix 回退为目录存在且非只读（Windows 非 v0.1 目标）。`getEntrypointPackageDir` 从当前可执行文件向上找 `package.json`（上游从 `process.argv[1]`）。
8. **`runSelfUpdate` 复用 `PackageCommandRunner::run`**（stdio inherited 语义一致）；进程失败文案沿用 runner 的 `{display} failed with code N`（D-040 #4 引擎级文案差异同型），上游为 `exited with code N`。
9. **Windows 专属分支**（win32 非 npm/pnpm 拒绝、`prepareWindowsNpmSelfUpdate` 隔离）保留判定但隔离准备为 no-op：Windows 不是 v0.1 目标（musl/Linux 优先）。
10. **W2 `resolve()` 包切片的 canonical 去重集合修正为按资源类型独立**（上游 `mapToResolved` 每类型一个 `seen`；W2 四类型共用一个集合，同名文件跨类型（如 `.md` 同时是 skill 和 prompt）会被误删）。行为修正对齐上游。
11. **update 路径信任**：严格 `useSavedProjectTrustOnly`——无资源扫描、无 UI、无扩展事件；`-a/-na` 覆盖；trust.json 读取失败 → `Error: …` exit 1（上游会向上抛，main 层兜底为未处理拒绝）。「`pir update` 永不提示」约束有测试锚点（`update_extensions_skips_untrusted_project_packages`）。

## 影响面

无（纯内部实现）：settings 线格式、安装目录布局、CLI 文案（除 runner 引擎级措辞）与上游一致；exit code 矩阵逐条对齐。

## 处置

- **回写位置**：`docs/01-requirements.md` §7.6（追加实现注记）；`docs/plan/v0.1/T14-packages-trust-export.md` 偏离表
- **回写日期**：2026-08-06
- **ADR**：不需要

## 审查修复补记（2026-08-07 审查修复波次）

1. **版本比较剥 `v`/`V`/`=` 前缀**：`compare_package_versions` 现先
   `strip_version_prefix` 再 `semver::Version::parse`（node-semver 的
   `semver.valid` 接受前缀；不剥离时 `v1.0.0` vs `1.0.0` 落字符串不等回退、
   误判为更新）。新增 `node_semver_prefixes_do_not_misreport_same_version` 锚点。
2. **usage 文案与 `APP_NAME` 绑定**：`UPDATE_USAGE`/`INSTALL_USAGE`/`REMOVE_USAGE`/
   `LIST_USAGE` 中的字面 `pir` 与 `config::APP_NAME` 之间新增
   `usage_lines_start_with_app_name` 测试绑定（改名时提醒同步帮助文本）。
