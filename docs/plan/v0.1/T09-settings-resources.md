# T09：Settings 与资源加载

- **状态**：已完成
- **里程碑**：M3
- **依赖**：T01
- **上游对照**：`docs/settings.md`、`docs/skills.md`、`docs/prompt-templates.md`、`docs/themes.md`、`docs/keybindings.md`、`docs/environment-variables.md`、`docs/security.md`；`packages/coding-agent/src/core/{settings-manager,resource-loader,skills,prompt-templates,keybindings,system-prompt}.ts`
- **需求章节**：§3.3（环境变量）、§7.1–§7.5、§7.7
- **预估**：0.7–1 人月（M3 共 3–3.5，与 T07/T08/T16 合计）

---

## 目标

实现 SettingsManager 与 ResourceLoader 统一发现管线，使声明式资源
（context files / skills / prompt templates / themes / keybindings）与 Pi 格式互通。

## 范围

### In

- `SettingsManager`：全局 `~/.rpi/agent/settings.json` + 项目覆盖；**全键清单与默认值**（需求 §7.7，40+ 键）；合并语义（嵌套对象单层浅合并（深度≥2 整体替换）、**数组与原始值整体替换**）；字段级写持久化（只写 session 内改过的字段）+ `fs2` 锁；旧格式迁移 4 条（queueMode→steeringMode、websockets→transport、旧 skills 对象、retry.maxDelayMs→provider.maxRetryDelayMs）；trust=false 时项目 settings 视为空且拒写；parse 错误按 scope 诊断并阻止覆写
- 环境变量模块（需求 §3.3）：进程级 `RPI_*` 全集 + bash 会话注入 5 变量（与 T06 接线）
- `ResourceLoader` 统一发现：全局 → 项目（trust 门控）→ settings 路径 → CLI flags → packages，输出 `LoadedResources { extensions, skills, prompts, themes, context_files, diagnostics }`（设计文档 §6.7）；同名冲突先到先得 + collision 诊断；**资源优先级 rank**（project settings > project auto > user settings > user auto > package）
- Context files：候选优先级 `AGENTS.md` > `AGENTS.MD` > `CLAUDE.md` > `CLAUDE.MD`；全局一份 + cwd 到**文件系统根**祖先链（不以 git root 为界）；`<project_context>`/`<project_instructions path>` 注入格式；**无论 trust 与否都加载**
- `SYSTEM.md` / `APPEND_SYSTEM.md`：项目版需 trust 且优先于全局；`--system-prompt`/`--append-system-prompt` 文件路径 vs 内联文本解析
- Skills：**发现路径全集**（`~/.rpi/agent/skills`、`~/.agents/skills`、`.rpi/skills`、祖先 `.agents/skills` 上界 **git repo root**、packages、settings 数组、CLI）；**两种发现模式**（pi 目录根级散 `.md` 算 skill；`.agents` 只认 SKILL.md）；ignore 文件链（`.gitignore`/`.ignore`/`.fdignore`）、dotdir/node_modules 跳过、符号链接跟随；frontmatter（name 校验仅警告、description 缺失**不加载**、disable-model-invocation）；渐进披露（`<available_skills>` XML，**仅 read 工具激活时注入**）；`/skill:name` 展开格式（`<skill name location>` + References 行 + args 原样追加）；`enableSkillCommands`
- Prompt templates：**非递归** `*.md`；frontmatter（description 缺省首行截 60+`...`、argument-hint）；**展开 DSL 全集**（`$1..$N`、`$@`、`$ARGUMENTS`、`${N:-default}`、`${@:-default}`、`${ARGUMENTS:-default}`、`${@:N}`、`${@:N:L}`；引号感知解析；不递归；缺位空串）
- Themes：内置 dark/light；JSON schema（`name`、`vars`、`colors` **51 必填 + `thinkingMax` 可选回退 thinkingXhigh**、`export` 段，theme-schema.json:38-90）；ColorValue 三形态（hex / 0-255 整数 / `""`）；热重载**仅 watch 全局当前主题文件**；theme 值 `light/dark` 为 auto 配对（`parseAutoThemeSetting`）、主题名正则 `^[^/]+$`；终端配色检测链 OSC 11→COLORFGBG→fallback、动态切换；终端自省 OSC 11 / CSI ?996n / CSI 16t / OSC 9;4（1s keepalive）
- Keybindings：**仅全局** `keybindings.json`；命名空间 id（`tui.editor.*`/`tui.input.*`/`tui.select.*`/`app.*`）；**旧键名迁移表 60+ 项**（ADR-0003 §3 保留项）；平台差异默认值（win32 无 ctrl+z、贴图 alt+v、macOS tree 方向键）

### Out

- 主题热重载与 TUI 应用（T12）
- Packages 安装来源加载（T14；本任务管线预留 packages 输入口）
- Project trust 两阶段加载（T14；本任务提供「信任前/后」分组能力）

## 开发要点

- 发现顺序与覆盖语义逐项对照上游文档，冲突合并规则写测试锁死
- 上游文档与代码冲突时**以代码为准**（案例：`/skill:name` 参数追加格式；设计原则 4）
- 文件格式兼容用 fixtures 验证（skills / prompts / themes / keybindings 各取上游样例）
- 新增配置路径一律走统一路径模块，不散落拼接（编码规范 §10.1）

## 进度跟踪

- [x] 设计细化
- [x] 实现
- [x] 自测
- [x] 门禁验收
- [x] 文档回写

## 自测清单

- [x] settings 合并语义：嵌套单层浅合并（深度≥2 整体替换）、数组整体替换、字段级持久化、旧格式迁移 4 条——`settings_manager.rs` 内测 `test_deep_merge_*`×4 / `test_migrate_*`×4 / 字段级持久化 4 例；对拍 `parity_resources_test.rs::parity_settings_deep_merge` / `parity_settings_migrations`
- [x] 资源发现顺序、两种 skills 模式、rank 与去重规则测试（构造多级目录 fixture）——`skills_test.rs::skills_discovery_pir_and_agents_modes_end_to_end` / `skills_rank_project_settings_beats_project_auto_on_name_collision` 等；`resource_loader_test.rs` rank 全排序例；对拍 `parity_resource_loader_e2e`
- [x] context files 边界：到文件系统根 vs skills 到 git root（构造 repo 内外用例）——`system_prompt_test.rs::global_then_ancestors_root_side_first`（穿越 .git 界）；`skills_test.rs` 祖先扫描三例（git 上界/无 repo/家目录排除）
- [x] skills：XML 摘要注入（含 read 工具未激活时不注入）、frontmatter 各字段语义、`/skill:name` 展开格式与上游代码一致——`skills_test.rs::skills_xml_injection_gate_and_disable_model_invocation` / `skills_xml_block_exact_shape_from_disk` / `skills_expand_command_exact_format_with_args` / `skills_frontmatter_field_semantics`；对拍 `parity_skills_battery`（上游 13+1 fixture 目录）
- [x] prompt template 展开：DSL 各形态与上游一致（含切片与默认值）——`prompt_templates.rs` 内测 DSL 电池 + 引号感知分词；对拍 `parity_prompt_dsl`（21 对黄金）
- [x] themes：51+1 token 解析、vars 引用、256 色整数、缺失 token 回退、非法值诊断——`themes_test.rs::test_all_51_required_tokens_parse_in_builtin_themes` / `test_thinking_max_optional_fallback` / `test_custom_theme_with_vars` / `test_missing_colors_diagnostics` / `test_invalid_color_value_diagnostics` 等；对拍 `parity_themes`（11 自定义 + dark/light 快照，双色彩模式）
- [x] keybindings：旧键名迁移、平台差异默认值、token 名与上游文档逐条核对——`keybindings_test.rs::test_migration_all_*`×3 / 默认表逐条对拍 3 例 / 平台差异 3 例；迁移写回 `resource_loader_test.rs::keybindings_migration_writes_back_to_disk`；对拍 `parity_keybindings`
- [x] 环境变量：进程级全集生效；bash 注入变量模型切换即时生效——`environment.rs` 内测 13 例；`bash_tool_test.rs::test_pir_session_env_injected` / `test_pir_env_stripped_when_no_session` / `test_session_env_resolved_per_command_start`（本任务补的即时生效锚点）

## 门禁验收

通用门禁 G1–G7 全过（G5 重点：settings/themes/keybindings JSON 形状兼容）。

任务特有标准：

- [x] 需求 §7.1–§7.5、§7.7、§3.3 各条目有测试锚点（验收记录列映射表）
- [x] 上游资源样例 fixtures 加载结果对拍一致——`fixtures/generated/resources/` 6 组黄金（skills-battery / prompt-dsl / themes / keybindings / settings / resource-loader-e2e），`parity_resources_test.rs` 7/7 通过，生成脚本连跑两次字节级可重复

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| D-014 | settings 与资源加载 Rust 落地差异（同步写盘/fs2 flock、保序 map、引擎级文案、TUI 件下沉、占位边界等 11 项） | 已回写 |

## 验收记录

- 验收日期：2026-08-03
- 验收人：单人开发，按 gates.md §1 清单逐项自证
- G1 构建/静态检查：通过（`cargo build --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check` 全部无警告）
- G2 测试：通过（`cargo test --workspace`：1193 passed, 0 failed；live 测试默认跳过；非 live 不访问网络）
- G3 对拍：通过。`fixtures/generated/resources/` 6 组黄金（生成脚本 `fixtures/generate-resources-golden.mjs`，钉死上游 2efa728 dist 驱动，连跑两次 `diff -r` 为空）；`cargo test -p rpi --test parity_resources_test` 7/7 通过（skills-battery 13+1 上游 fixture 目录、prompt-dsl 21 对、themes 11+2、keybindings 5、settings 5+8、resource-loader-e2e 全树）。逐条对拍基准映射表见下（`docs/keybindings.md` 默认绑定表逐条锚点含在 keybindings_test.rs）
- G4 红线：通过（`external/pi` HEAD=2efa728 且 `git status --porcelain` 为空；无 JS/TS 执行能力引入——fixtures 生成脚本属既有 fixtures 纪律；未读写 `~/.pi`；锁仅 settings/keybindings 迁移写回（fs2），session 无锁；非测试代码无 unwrap/expect）
- G5 线格式：通过（settings/themes/keybindings JSON serde 形状与上游核对，camelCase；`serde_json/preserve_order` 保持 JS 对象插入序；对拍随 G3 全过）
- G6 文档同步：通过（全部移植文件有 `Port of ... @ pi 0.82.1 (2efa728)` 溯源注释；回写：`02-design.md` §6.7 Rust 落地注记、§12 映射表 resource-loader 行；`fixtures/README.md` §3.1 resources 用例组与引擎级排除口径、§5.3 settings 接线状态 ✅）
- G7 偏离闭环：通过（D-014 已登记并回写，状态「已回写」；本文件「偏离记录」已列出）
- 结论：通过

### 需求条目 → 测试锚点映射表（任务特有标准 1）

**§3.3 环境变量**

| 条目 | 锚点 |
|------|------|
| 进程级 `RPI_*` 全集（marker/PACKAGE_DIR/OFFLINE/SKIP_VERSION_CHECK/TELEMETRY/CACHE_RETENTION/SHARE_VIEWER_URL/STARTUP_BENCHMARK/TUI_WRITE_LOG/VISUAL/EDITOR 等） | `core/environment.rs` 内测 13 例（`test_coding_agent_marker` / `test_package_dir_override` / `test_is_offline_truthy_flag` / `test_skip_version_check_any_non_empty` / `test_telemetry_enabled_override` / `test_cache_retention_long_exact_match` / `test_share_viewer_base_url_fallback` / `test_startup_benchmark_truthy_flag` / `test_tui_write_log_path` / `test_external_editor_from_env_precedence` 等） |
| bash 会话注入 5 变量（每次命令启动解析、模型切换即时生效、未启用删除继承） | `rpi/tests/bash_tool_test.rs::test_pir_session_env_injected` / `test_pir_env_stripped_when_no_session` / `test_expose_session_env_false` / `test_session_env_resolved_per_command_start` |

**§7.1 Context files**

| 条目 | 锚点 |
|------|------|
| 候选优先级 AGENTS.md>AGENTS.MD>CLAUDE.md>CLAUDE.MD | `system_prompt_test.rs::candidate_priority_full_order`、`candidate_that_is_a_directory_is_skipped` |
| 全局 + cwd 到文件系统根祖先链（根侧在前、按路径去重） | `system_prompt_test.rs::global_then_ancestors_root_side_first`、`agent_dir_overlapping_ancestor_is_deduplicated` |
| `<project_context>`/`<project_instructions path>` 注入格式 | `system_prompt_test.rs::context_files_injected_byte_exact_into_default_prompt` |
| `-nc` 禁用；无论 trust 与否都加载 | `resource_loader_test.rs::should_skip_context_files_when_no_context_files_is_true` / `should_skip_project_resources_that_require_trust_when_project_is_not_trusted` |
| SYSTEM.md/APPEND_SYSTEM.md trust 门控与项目优先；`--system-prompt` 文件 vs 内联 | `system_prompt_test.rs::system_md_project_requires_trust_and_beats_global` / `append_system_md_same_gate_and_priority` / `missing_path_is_inline_text` |
| 对拍 | `parity_resources_test.rs::parity_resource_loader_e2e` |

**§7.2 Skills**

| 条目 | 锚点 |
|------|------|
| 发现路径全集 + 两种发现模式 + trust 门控 | `skills_test.rs::skills_discovery_pir_and_agents_modes_end_to_end` / `skills_trust_gate_excludes_project_auto_discovery` |
| 祖先 `.agents/skills` 上界 git repo root（无 repo 到根；`~/.agents` 排除） | `skills_test.rs` 祖先扫描三例 |
| ignore 文件链 / dotdir / node_modules / 符号链接 | `skills_test.rs::skills_discovery_respects_fdignore_chain` 等 |
| frontmatter 语义（name 仅警告 / description 缺失不加载 / disable-model-invocation） | `skills_test.rs::skills_frontmatter_field_semantics`；对拍 `parity_skills_battery` |
| 同名先到先得 + collision 诊断；rank | `skills_test.rs::skills_rank_project_settings_beats_project_auto_on_name_collision`；`resource_loader_test.rs` rank 例 |
| `<available_skills>` XML（仅 read 激活注入） | `skills_test.rs::skills_xml_injection_gate_and_disable_model_invocation` / `skills_xml_block_exact_shape_from_disk` |
| `/skill:name` 展开格式（args 原样追加）；`enableSkillCommands` | `skills_test.rs::skills_expand_command_exact_format_with_args`；`settings_manager.rs` enableSkillCommands 例 |

**§7.3 Prompt Templates**

| 条目 | 锚点 |
|------|------|
| 非递归 `*.md` / 符号链接跟随 | `prompt_templates_test.rs::non_recursive_and_missing_dir` / `follows_symlinks_and_skips_broken_ones` |
| frontmatter（description 缺省截 60+`...` / argument-hint） | `prompt_templates_test.rs::frontmatter_description_and_argument_hint` / `description_defaults_to_first_non_empty_line` / `description_truncates_at_60_chars` |
| 展开 DSL 全集（含 `${@:N:L}` 切片、引号感知、缺位空串、不递归） | `prompt_templates.rs` 内测 DSL 电池；对拍 `parity_prompt_dsl`（21 对） |

**§7.4 Themes**

| 条目 | 锚点 |
|------|------|
| 内置 dark/light；51 必填 + thinkingMax 回退 thinkingXhigh | `themes_test.rs::test_all_51_required_tokens_parse_in_builtin_themes` / `test_thinking_max_optional_fallback` |
| ColorValue 三形态 / vars 引用 / 缺失与非法值诊断 | `themes_test.rs::test_custom_theme_with_vars` / `_256_color_int` / `_empty_string_default` / `test_missing_colors_diagnostics` / `test_invalid_color_value_diagnostics` |
| `parseAutoThemeSetting` light/dark 配对；主题名正则 `^[^/]+$` | `themes.rs` 内测 parse_auto/resolve 例；`themes_test.rs::test_theme_name_slash_rejected` / `test_auto_theme_full_cycle` |
| 终端检测链 OSC 11→COLORFGBG→fallback；自省序列常量 | `themes.rs` 内测 OSC 11/COLORFGBG/luminance/CSI 997 例（TUI 接线 T12） |
| 热重载仅 watch 全局当前主题 | `themes.rs::get_theme_watch_path`（watcher 本体 T12） |
| 对拍 | `parity_resources_test.rs::parity_themes` |

**§7.5 Keybindings**

| 条目 | 锚点 |
|------|------|
| 仅全局 keybindings.json；值 string\|string[] | `keybindings_test.rs` 文件加载例（空/缺文件/字符串/数组/混合） |
| 命名空间 id 全集（73 个）与默认表逐条（docs/keybindings.md 基准） | `keybindings_test.rs` 默认表逐条对拍 3 例；`keybindings.rs` 内测 73 定义数断言 |
| 旧键名迁移表 59 项 + 冲突新名胜 | `keybindings_test.rs::test_migration_all_app` / `_all_tui_editor` / `_all_tui_input_select` / `test_file_load_mixed_legacy_and_modern`；对拍 `parity_keybindings` |
| 平台差异默认值（win32/macOS） | `keybindings.rs` 内测平台差异 3 例 |
| 迁移写回磁盘（fs2 锁） | `resource_loader_test.rs::keybindings_migration_writes_back_to_disk` |

**§7.7 Settings**

| 条目 | 锚点 |
|------|------|
| 全键清单与默认值（40+ 键） | `settings_manager.rs` 内测 `test_getter_defaults` 总表 + 各 getter 专项例 |
| 合并语义（单层浅合并 / 深度≥2 替换 / 数组与原始值替换） | `settings_manager.rs::test_deep_merge_*`×4；对拍 `parity_settings_deep_merge` |
| 字段级写持久化（只写改动字段、嵌套按键合并）+ fs2 锁 | `settings_manager.rs` 持久化 4 例 |
| 旧格式迁移 4 条 | `settings_manager.rs::test_migrate_*`×4；对拍 `parity_settings_migrations`（8 子例） |
| trust=false 项目 settings 视为空且拒写；parse 错误按 scope 阻止覆写 | `settings_manager.rs` trust 5 例 / drainErrors 例 |
