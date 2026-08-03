//! T09 对拍层：`fixtures/generated/resources/`（上游真实模块产出的黄金
//! JSON，见 `fixtures/generate-resources-golden.mjs`）vs pir 对应实现加载
//! 同一输入的归一化 diff。
//!
//! 归一化约定（沿用 `pir_test_support`，无第二套实现）：
//! - 黄金中的绝对路径已在生成时替换为 `<path>`；Rust 侧对 actual 用
//!   `Normalizer::with_path(root)` 做同一替换后比对。
//! - 比较文本统一经 serde 解析后重新渲染（两侧同格式），diff 由
//!   `diff_text` 完成并定位首个差异。
//! - 主题颜色表两侧均按键排序（Rust `HashMap` 无迭代序；上游 Map 为插入
//!   序，排序是测试侧整形而非归一化）。
//! - `.pi` → `.pir` 是有意改名（需求 §1.4 / ADR-0001）：e2e 黄金已统一为
//!   `.pir` 拼写，测试两侧跑同一棵含 `.pir` + `.pi` 双份的目录树。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use pir::core::keybindings::migrate_keybindings_config;
use pir::core::prompt_templates::{parse_command_args, substitute_args};
use pir::core::resource_loader::{DefaultResourceLoader, DefaultResourceLoaderOptions};
use pir::core::settings_manager::{
    Settings, SettingsManager, SettingsManagerCreateOptions, SettingsScope, SettingsStorage,
    WithLockCallback,
};
use pir::core::skills::{
    load_skills, DiagnosticKind, DiagnosticResourceType, LoadSkillsOptions, ResourceDiagnostic,
    Skill, SourceOrigin, SourceScope,
};
use pir::core::themes::{
    get_resolved_theme_colors, load_theme_from_path, ColorMode, Theme, ALLOWED_COLOR_KEYS,
    BG_COLOR_KEYS,
};
use pir::error::PirError;
use pir_test_support::{diff_text, Normalizer};
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn resources_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/generated/resources")
}

fn load_golden(group: &str) -> Value {
    let text = std::fs::read_to_string(resources_dir().join(group).join("golden.json"))
        .unwrap_or_else(|e| panic!("read {group}/golden.json: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {group}/golden.json: {e}"))
}

fn render(value: &Value) -> String {
    let mut s = serde_json::to_string_pretty(value).expect("render JSON");
    s.push('\n');
    s
}

/// Compare `actual` against the golden subtree `expected`: path-normalize
/// actual with the given roots, render both through serde, `diff_text`.
fn compare(group: &str, case: &str, expected: &Value, actual: &mut Value, roots: &[&Path]) {
    let mut normalizer = Normalizer::new();
    for root in roots {
        normalizer = normalizer.with_path(root.to_string_lossy().into_owned());
    }
    normalizer.normalize_json(actual);
    diff_text(&render(expected), &render(actual))
        .unwrap_or_else(|f| panic!("{group}/{case} parity diff:\n{f}"));
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "pir-parity-resources-{name}-{}-{nanos}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create test dir");
        TestDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// Summary shapes (mirror fixtures/generate-resources-golden.mjs)
// ---------------------------------------------------------------------------

fn scope_str(scope: SourceScope) -> &'static str {
    match scope {
        SourceScope::User => "user",
        SourceScope::Project => "project",
        SourceScope::Temporary => "temporary",
    }
}

fn origin_str(origin: SourceOrigin) -> &'static str {
    match origin {
        SourceOrigin::Package => "package",
        SourceOrigin::TopLevel => "top-level",
    }
}

fn summarize_skill(skill: &Skill) -> Value {
    json!({
        "name": skill.name,
        "description": skill.description,
        "filePath": path_str(&skill.file_path),
        "baseDir": path_str(&skill.base_dir),
        "disableModelInvocation": skill.disable_model_invocation,
        "source": skill.source_info.source,
        "scope": scope_str(skill.source_info.scope),
        "origin": origin_str(skill.source_info.origin),
        "sourceBaseDir": skill.source_info.base_dir.as_ref().map(|p| path_str(p)),
    })
}

fn diagnostic_kind_str(kind: DiagnosticKind) -> &'static str {
    match kind {
        DiagnosticKind::Warning => "warning",
        DiagnosticKind::Error => "error",
        DiagnosticKind::Collision => "collision",
    }
}

fn resource_type_str(resource_type: DiagnosticResourceType) -> &'static str {
    match resource_type {
        DiagnosticResourceType::Extension => "extension",
        DiagnosticResourceType::Skill => "skill",
        DiagnosticResourceType::Prompt => "prompt",
        DiagnosticResourceType::Theme => "theme",
    }
}

fn summarize_diagnostic(d: &ResourceDiagnostic) -> Value {
    json!({
        "type": diagnostic_kind_str(d.kind),
        "message": d.message,
        "path": d.path.as_ref().map(|p| path_str(p)),
        "collision": d.collision.as_ref().map(|c| json!({
            "resourceType": resource_type_str(c.resource_type),
            "name": c.name,
            "winnerPath": path_str(&c.winner_path),
            "loserPath": path_str(&c.loser_path),
            "winnerSource": c.winner_source,
            "loserSource": c.loser_source,
        })),
    })
}

// ---------------------------------------------------------------------------
// 1. skills-battery
// ---------------------------------------------------------------------------

#[test]
fn parity_skills_battery() {
    let golden = load_golden("skills-battery");
    // Canonicalize: `load_skills` resolves paths, so the raw
    // `CARGO_MANIFEST_DIR/../../...` spelling would not match as a prefix.
    let input_root = std::fs::canonicalize(resources_dir().join("skills-battery").join("input"))
        .expect("canonicalize skills-battery input");
    let agent_dir = input_root.join("nonexistent-agent-dir");

    for case in golden["cases"].as_array().expect("cases array") {
        let name = case["name"].as_str().expect("case name");
        let skill_paths: Vec<PathBuf> = case["skillPaths"]
            .as_array()
            .expect("skillPaths array")
            .iter()
            .map(|p| input_root.join(p.as_str().expect("skill path")))
            .collect();
        let result = load_skills(&LoadSkillsOptions {
            cwd: input_root.clone(),
            agent_dir: agent_dir.clone(),
            skill_paths,
            include_defaults: false,
        });
        // Parser-engine YAML error texts are not contractual: the golden
        // records `message: null` for these cases; assert non-empty instead.
        let engine_dependent = case["engineDependentMessages"].as_bool().unwrap_or(false);
        let diagnostics: Vec<Value> = result
            .diagnostics
            .iter()
            .map(|d| {
                let mut summary = summarize_diagnostic(d);
                if engine_dependent {
                    assert!(
                        !d.message.is_empty(),
                        "skills-battery/{name}: diagnostic message must be non-empty"
                    );
                    summary["message"] = Value::Null;
                }
                summary
            })
            .collect();
        let mut actual = json!({
            "skills": result.skills.iter().map(summarize_skill).collect::<Vec<_>>(),
            "diagnostics": diagnostics,
        });
        // serde_yaml vs JS yaml block-scalar chomping at EOF: compare
        // descriptions with trailing newlines trimmed on both sides.
        let mut expected = case["expected"].clone();
        if case["engineDependentTrailingNewline"].as_bool() == Some(true) {
            for value in [&mut expected, &mut actual] {
                if let Some(skills) = value["skills"].as_array_mut() {
                    for skill in skills {
                        if let Some(description) = skill["description"].as_str() {
                            let trimmed = description.trim_end_matches('\n').to_owned();
                            skill["description"] = Value::String(trimmed);
                        }
                    }
                }
            }
        }
        compare(
            "skills-battery",
            name,
            &expected,
            &mut actual,
            &[&input_root],
        );
    }
}

// ---------------------------------------------------------------------------
// 2. prompt-dsl
// ---------------------------------------------------------------------------

#[test]
fn parity_prompt_dsl() {
    let golden = load_golden("prompt-dsl");
    for case in golden["cases"].as_array().expect("cases array") {
        let name = case["name"].as_str().expect("case name");
        let args_string = case["argsString"].as_str().expect("argsString");
        let content = case["content"].as_str().expect("content");
        let args = parse_command_args(args_string);
        let mut actual = json!({
            "args": args,
            "expected": substitute_args(content, &args),
        });
        let expected = json!({
            "args": case["args"],
            "expected": case["expected"],
        });
        compare("prompt-dsl", name, &expected, &mut actual, &[]);
    }
}

// ---------------------------------------------------------------------------
// 3. themes
// ---------------------------------------------------------------------------

fn summarize_theme(theme: &Theme) -> Value {
    let bg_keys: std::collections::HashSet<&str> = BG_COLOR_KEYS.iter().copied().collect();
    let mut fg_colors = BTreeMap::new();
    let mut bg_colors = BTreeMap::new();
    for key in ALLOWED_COLOR_KEYS {
        if bg_keys.contains(key) {
            bg_colors.insert(*key, theme.get_bg_ansi(key).to_owned());
        } else {
            fg_colors.insert(*key, theme.get_fg_ansi(key).to_owned());
        }
    }
    json!({
        "name": theme.name,
        "fgColors": fg_colors,
        "bgColors": bg_colors,
    })
}

const THEME_MODES: [(&str, ColorMode); 2] = [
    ("truecolor", ColorMode::TrueColor),
    ("256color", ColorMode::Color256),
];

#[test]
fn parity_themes() {
    let golden = load_golden("themes");
    let tmp = TestDir::new("themes");

    for case in golden["cases"].as_array().expect("cases array") {
        let name = case["name"].as_str().expect("case name");
        let content = case["expected"]["content"].as_str().expect("content");
        let theme_path = tmp.path().join(format!("{name}.json"));
        std::fs::write(&theme_path, content).expect("write theme file");

        for (mode_name, mode) in THEME_MODES {
            let expected_mode = &case["expected"][mode_name];
            let label = format!("{name}/{mode_name}");
            match load_theme_from_path(&theme_path, Some(mode)) {
                Ok(theme) => {
                    let mut actual = summarize_theme(&theme);
                    compare("themes", &label, expected_mode, &mut actual, &[tmp.path()]);
                }
                Err(err) => {
                    let message = match &err {
                        PirError::Resource(msg) => msg.clone(),
                        other => other.to_string(),
                    };
                    if expected_mode.get("error").is_some() {
                        let mut actual = json!({ "error": message });
                        compare("themes", &label, expected_mode, &mut actual, &[tmp.path()]);
                    } else {
                        // Engine/validator-dependent error text: pin the
                        // stable prefix and marker substrings only.
                        let normalized = Normalizer::new()
                            .with_path(path_str(tmp.path()))
                            .normalize_string(&message);
                        let prefix = expected_mode["errorPrefix"]
                            .as_str()
                            .expect("errorPrefix in golden");
                        assert!(
                            normalized.starts_with(prefix),
                            "themes/{label}: error {normalized:?} does not start with {prefix:?}"
                        );
                        for marker in expected_mode["errorContains"]
                            .as_array()
                            .expect("errorContains array")
                        {
                            let marker = marker.as_str().expect("marker string");
                            assert!(
                                normalized.contains(marker),
                                "themes/{label}: error {normalized:?} missing {marker:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    // Builtin dark/light resolved-color snapshots.
    for name in ["dark", "light"] {
        let resolved = get_resolved_theme_colors(name).expect("builtin theme resolves");
        let sorted: BTreeMap<String, String> = resolved.into_iter().collect();
        let mut actual = serde_json::to_value(sorted).expect("to value");
        compare(
            "themes",
            &format!("builtin-{name}"),
            &golden["builtins"][name],
            &mut actual,
            &[],
        );
    }
}

// ---------------------------------------------------------------------------
// 4. keybindings
// ---------------------------------------------------------------------------

#[test]
fn parity_keybindings() {
    let golden = load_golden("keybindings");
    for case in golden["cases"].as_array().expect("cases array") {
        let name = case["name"].as_str().expect("case name");
        let input = case["input"].as_object().expect("input object").clone();
        let (config, migrated) = migrate_keybindings_config(&input);
        let mut actual = json!({ "config": Value::Object(config), "migrated": migrated });
        compare("keybindings", name, &case["expected"], &mut actual, &[]);
    }
}

// ---------------------------------------------------------------------------
// 5. settings
// ---------------------------------------------------------------------------

/// Storage backend preset with raw JSON strings (the JS side presets
/// `InMemorySettingsStorage.global/project` the same way).
struct PresetStorage {
    global: Option<String>,
    project: Option<String>,
}

impl SettingsStorage for PresetStorage {
    fn with_lock(
        &mut self,
        scope: SettingsScope,
        f: &mut WithLockCallback<'_>,
    ) -> Result<(), PirError> {
        let current = match scope {
            SettingsScope::Global => self.global.as_deref(),
            SettingsScope::Project => self.project.as_deref(),
        };
        let next = f(current)?;
        if let Some(next) = next {
            match scope {
                SettingsScope::Global => self.global = Some(next),
                SettingsScope::Project => self.project = Some(next),
            }
        }
        Ok(())
    }
}

#[test]
fn parity_settings_deep_merge() {
    let golden = load_golden("settings");
    for case in golden["deepMerge"].as_array().expect("deepMerge array") {
        let name = case["name"].as_str().expect("case name");
        let storage = PresetStorage {
            global: Some(case["global"].to_string()),
            project: Some(case["project"].to_string()),
        };
        let manager =
            SettingsManager::from_storage(storage, SettingsManagerCreateOptions::default());
        let compaction = manager.get_compaction_settings();
        let branch_summary = manager.get_branch_summary_settings();
        let retry = manager.get_retry_settings();
        let provider_retry = manager.get_provider_retry_settings();
        let mut actual = json!({
            "compaction": {
                "enabled": compaction.enabled,
                "reserveTokens": compaction.reserve_tokens,
                "keepRecentTokens": compaction.keep_recent_tokens,
            },
            "branchSummary": {
                "reserveTokens": branch_summary.reserve_tokens,
                "skipPrompt": branch_summary.skip_prompt,
            },
            "retry": {
                "enabled": retry.enabled,
                "maxRetries": retry.max_retries,
                "baseDelayMs": retry.base_delay_ms,
            },
            "providerRetry": {
                "timeoutMs": provider_retry.timeout_ms,
                "maxRetries": provider_retry.max_retries,
                "maxRetryDelayMs": provider_retry.max_retry_delay_ms,
            },
            "packages": serde_json::to_value(manager.get_packages()).expect("packages"),
            "extensionPaths": manager.get_extension_paths(),
            "themeSetting": manager.get_theme_setting(),
            "quietStartup": manager.get_quiet_startup(),
            "steeringMode": serde_json::to_value(manager.get_steering_mode()).expect("steering"),
        });
        compare("settings", name, &case["expected"], &mut actual, &[]);
    }
}

#[test]
fn parity_settings_migrations() {
    let golden = load_golden("settings");
    for case in golden["migrations"].as_array().expect("migrations array") {
        let name = case["name"].as_str().expect("case name");
        let input: Map<String, Value> = case["input"].as_object().expect("input object").clone();
        let manager = SettingsManager::in_memory(
            Settings::from_map(input),
            SettingsManagerCreateOptions::default(),
        );
        let mut actual = Value::Object(manager.get_global_settings().into_map());
        compare("settings", name, &case["expected"], &mut actual, &[]);
    }
}

// ---------------------------------------------------------------------------
// 6. resource-loader-e2e
// ---------------------------------------------------------------------------

fn copy_dir_all(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create copy destination");
    for entry in std::fs::read_dir(src).expect("read source dir") {
        let entry = entry.expect("dir entry");
        let target = dst.join(entry.file_name());
        let file_type = entry.file_type().expect("file type");
        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

/// Mirror of `prepareE2eTree` in the generator: copy the committed input
/// tree, duplicate every `.pir` dir as `.pi` (upstream reads `.pi`, pir
/// reads `.pir`), create the untrackable `repo/.git` marker.
fn prepare_e2e_tree(input: &Path, dest: &Path) {
    copy_dir_all(input, dest);
    for dir in ["repo", "repo/sub"] {
        let pir_dir = dest.join(dir).join(".pir");
        if pir_dir.exists() {
            copy_dir_all(&pir_dir, &dest.join(dir).join(".pi"));
        }
    }
    std::fs::create_dir_all(dest.join("repo").join(".git")).expect("create .git marker");
}

#[test]
fn parity_resource_loader_e2e() {
    let golden = load_golden("resource-loader-e2e");
    let tmp = TestDir::new("e2e");
    let root = tmp.path();
    prepare_e2e_tree(
        &resources_dir().join("resource-loader-e2e").join("input"),
        root,
    );

    let cwd = root.join("repo").join("sub");
    let agent_dir = root.join("agent");
    let mut options = DefaultResourceLoaderOptions::new(cwd, agent_dir);
    options.home_dir = Some(root.join("home"));
    options.no_extensions = true;
    options.additional_skill_paths = vec![
        path_str(&root.join("cli").join("cli-skill")),
        path_str(&root.join("cli").join("cli-single.md")),
    ];
    let mut loader = DefaultResourceLoader::new(options);
    loader.reload();

    let resources = loader.resources();
    let mut actual = json!({
        "skills": resources.skills.iter().map(summarize_skill).collect::<Vec<_>>(),
        "skillDiagnostics": loader.skill_diagnostics().iter().map(summarize_diagnostic).collect::<Vec<_>>(),
        "prompts": resources.prompts.iter().map(|p| json!({
            "name": p.name,
            "description": p.description,
            "argumentHint": p.argument_hint,
            "filePath": path_str(&p.file_path),
        })).collect::<Vec<_>>(),
        "promptDiagnostics": loader.prompt_diagnostics().iter().map(summarize_diagnostic).collect::<Vec<_>>(),
        "themes": resources.themes.iter().map(|t| json!({
            "name": t.name,
            "sourcePath": t.source_path.as_ref().map(|p| path_str(p)),
        })).collect::<Vec<_>>(),
        "themeDiagnostics": loader.theme_diagnostics().iter().map(summarize_diagnostic).collect::<Vec<_>>(),
        "contextFiles": resources.context_files.iter().map(|f| json!({
            "path": path_str(&f.path),
            "content": f.content,
        })).collect::<Vec<_>>(),
        "systemPrompt": resources.system_prompt,
        "appendSystemPrompt": resources.append_system_prompt,
    });
    compare(
        "resource-loader-e2e",
        "e2e",
        &golden["expected"],
        &mut actual,
        &[root],
    );
}
