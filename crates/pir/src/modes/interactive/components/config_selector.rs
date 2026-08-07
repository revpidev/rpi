//! Port of `config-selector.ts` @ pi 0.82.1 (2efa728), W3 rewiring.
//!
//! T12 delivered this component against `LoadedResources` with in-memory
//! toggle state and write hooks; T14-W3 replaces the input with the package
//! manager's full-resolve output ([`ScopedResolvedPaths`]) and ports the
//! upstream settings persistence (`toggleTopLevelResource` /
//! `togglePackageResource` / the project override cycle write straight into
//! the [`SettingsManager`], config-selector.ts:516-863).
//!
//! Intentional differences (D-042):
//! - The settings manager is shared as `Arc<Mutex<SettingsManager>>`
//!   (upstream reads/writes it synchronously from the component).
//! - Project-scope settings writes are gated upstream by the `pir config`
//!   trust check, so the `Result` of the project setters cannot fail here;
//!   failures are swallowed (upstream would throw).
//! - Item ordering uses codepoint comparison where upstream uses
//!   `localeCompare` (D-039 precedent).
//! - `CONFIG_DIR_NAME` is `.pir` (ADR-0001 rename of `.pi`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pir_tui::components::input::Input;
use pir_tui::keybindings::get_keybindings;
use pir_tui::keys::matches_key;
use pir_tui::tui::{Component, Focusable};
use pir_tui::utils::{truncate_to_width, visible_width};

use crate::config::{self, CONFIG_DIR_NAME};
use crate::core::package_manager::{ResolvedPaths, ResourcePathMetadata};
use crate::core::settings_manager::{
    PackageSource, PackageSourceFilter, Settings, SettingsManager,
};
use crate::core::skills::{canonicalize_path, lexical_relative, SourceOrigin, SourceScope};
use crate::core::themes::Theme;
use crate::modes::interactive::components::dynamic_border::DynamicBorder;
use crate::modes::interactive::components::keybinding_hints::{key_hint, raw_key_hint};
use crate::tools::path_utils::resolve_path;

/// `ResourceType` (config-selector.ts:26).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceType {
    Extensions,
    Skills,
    Prompts,
    Themes,
}

const RESOURCE_TYPES: [ResourceType; 4] = [
    ResourceType::Extensions,
    ResourceType::Skills,
    ResourceType::Prompts,
    ResourceType::Themes,
];

impl ResourceType {
    /// The settings-array key (`settings[arrayKey]`, config-selector.ts:537).
    pub fn as_str(self) -> &'static str {
        match self {
            ResourceType::Extensions => "extensions",
            ResourceType::Skills => "skills",
            ResourceType::Prompts => "prompts",
            ResourceType::Themes => "themes",
        }
    }

    /// `RESOURCE_TYPE_LABELS` (config-selector.ts:34-39).
    fn label(self) -> &'static str {
        match self {
            ResourceType::Extensions => "Extensions",
            ResourceType::Skills => "Skills",
            ResourceType::Prompts => "Prompts",
            ResourceType::Themes => "Themes",
        }
    }
}

/// `ConfigWriteScope` (config-selector.ts:27).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigWriteScope {
    Global,
    Project,
}

impl ConfigWriteScope {
    pub fn as_str(self) -> &'static str {
        match self {
            ConfigWriteScope::Global => "global",
            ConfigWriteScope::Project => "project",
        }
    }
}

/// `ScopedResolvedPaths` (config-selector.ts:30).
#[derive(Debug, Clone)]
pub struct ScopedResolvedPaths {
    pub global: ResolvedPaths,
    pub project: ResolvedPaths,
}

/// `ProjectOverrideState` (config-selector.ts:29).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectOverrideState {
    Inherit,
    Load,
    Unload,
}

/// `ResourceItem` (config-selector.ts:41-49; the Rust port indexes its
/// group/subgroup instead of carrying `groupKey`/`subgroupKey`).
#[derive(Debug, Clone)]
pub struct ResourceItem {
    pub path: String,
    pub enabled: bool,
    pub metadata: ResourcePathMetadata,
    pub resource_type: ResourceType,
    pub display_name: String,
}

/// `ResourceSubgroup` (config-selector.ts:51-55).
struct ResourceSubgroup {
    resource_type: ResourceType,
    label: String,
    items: Vec<ResourceItem>,
}

/// `ResourceGroup` (config-selector.ts:57-64).
struct ResourceGroup {
    label: String,
    scope: SourceScope,
    origin: SourceOrigin,
    source: String,
    subgroups: Vec<ResourceSubgroup>,
}

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

/// `getItemScope` (config-selector.ts:846-848): temporary renders/toggles
/// like user scope.
fn get_item_scope(item: &ResourceItem) -> SourceScope {
    if item.metadata.scope == SourceScope::Project {
        SourceScope::Project
    } else {
        SourceScope::User
    }
}

/// `formatBaseDir` (config-selector.ts:66-81): replace the home prefix with
/// `~` and normalize separators for display.
fn format_base_dir(base_dir: &str) -> String {
    let home_dir = config::user_home_dir().map(|p| p.to_string_lossy().into_owned());
    let display_path = match &home_dir {
        Some(home) if base_dir == home => "~".to_string(),
        Some(home) if base_dir.starts_with(home) => {
            format!("~{}", base_dir[home.len()..].replace('\\', "/"))
        }
        _ => base_dir.replace('\\', "/"),
    };
    if display_path.ends_with('/') {
        display_path
    } else {
        format!("{display_path}/")
    }
}

/// `getGroupLabel` (config-selector.ts:83-97).
fn get_group_label(metadata: &ResourcePathMetadata, agent_dir: &Path) -> String {
    if metadata.origin == SourceOrigin::Package {
        return format!("{} ({})", metadata.source, scope_str(metadata.scope));
    }
    if metadata.source == "auto" {
        if let Some(base_dir) = &metadata.base_dir {
            let base_dir = base_dir.to_string_lossy();
            return match metadata.scope {
                SourceScope::User => format!("User ({})", format_base_dir(&base_dir)),
                _ => format!("Project ({})", format_base_dir(&base_dir)),
            };
        }
        return match metadata.scope {
            SourceScope::User => {
                format!("User ({})", format_base_dir(&agent_dir.to_string_lossy()))
            }
            _ => format!("Project ({CONFIG_DIR_NAME}/)"),
        };
    }
    match metadata.scope {
        SourceScope::User => "User settings".to_string(),
        _ => "Project settings".to_string(),
    }
}

/// `buildGroups` (config-selector.ts:99-180).
fn build_groups(resolved: &ResolvedPaths, agent_dir: &Path) -> Vec<ResourceGroup> {
    let mut groups: Vec<ResourceGroup> = Vec::new();
    let mut group_index: HashMap<String, usize> = HashMap::new();

    // Upstream `addToGroup` call order (config-selector.ts:153-156).
    for (resources, resource_type) in [
        (&resolved.extensions, ResourceType::Extensions),
        (&resolved.skills, ResourceType::Skills),
        (&resolved.prompts, ResourceType::Prompts),
        (&resolved.themes, ResourceType::Themes),
    ] {
        for resource in resources {
            add_to_group(
                &mut groups,
                &mut group_index,
                resource,
                resource_type,
                agent_dir,
            );
        }
    }

    // Sort groups: packages first, then top-level; user before project
    // (config-selector.ts:158-168).
    groups.sort_by(|a, b| {
        let origin_cmp = match (a.origin, b.origin) {
            (SourceOrigin::Package, SourceOrigin::TopLevel) => std::cmp::Ordering::Less,
            (SourceOrigin::TopLevel, SourceOrigin::Package) => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        };
        if origin_cmp != std::cmp::Ordering::Equal {
            return origin_cmp;
        }
        let scope_cmp = match (a.scope, b.scope) {
            (SourceScope::User, SourceScope::User) => std::cmp::Ordering::Equal,
            (SourceScope::User, _) => std::cmp::Ordering::Less,
            (_, SourceScope::User) => std::cmp::Ordering::Greater,
            // Upstream comparator: any non-user pair sorts with the left
            // side after the right (`a.scope === "user" ? -1 : 1`), i.e.
            // temporary before project.
            (SourceScope::Temporary, SourceScope::Project) => std::cmp::Ordering::Less,
            (SourceScope::Project, SourceScope::Temporary) => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        };
        if scope_cmp != std::cmp::Ordering::Equal {
            return scope_cmp;
        }
        a.source.cmp(&b.source)
    });

    // Sort subgroups within each group by type order, and items by name
    // (config-selector.ts:170-177).
    let type_order = |t: ResourceType| match t {
        ResourceType::Extensions => 0,
        ResourceType::Skills => 1,
        ResourceType::Prompts => 2,
        ResourceType::Themes => 3,
    };
    for group in &mut groups {
        group
            .subgroups
            .sort_by_key(|sg| type_order(sg.resource_type));
        for subgroup in &mut group.subgroups {
            subgroup
                .items
                .sort_by(|a, b| a.display_name.cmp(&b.display_name));
        }
    }

    groups
}

/// One resource pushed into its group/subgroup (config-selector.ts:102-151).
fn add_to_group(
    groups: &mut Vec<ResourceGroup>,
    group_index: &mut HashMap<String, usize>,
    resource: &crate::core::package_manager::ResolvedResource,
    resource_type: ResourceType,
    agent_dir: &Path,
) {
    let metadata = &resource.metadata;
    let group_key = format!(
        "{}:{}:{}:{}",
        origin_str(metadata.origin),
        scope_str(metadata.scope),
        metadata.source,
        metadata
            .base_dir
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    let group_idx = *group_index.entry(group_key).or_insert_with(|| {
        groups.push(ResourceGroup {
            label: get_group_label(metadata, agent_dir),
            scope: metadata.scope,
            origin: metadata.origin,
            source: metadata.source.clone(),
            subgroups: Vec::new(),
        });
        groups.len() - 1
    });

    let subgroup_idx = {
        let group = &mut groups[group_idx];
        let mut found = None;
        for (i, sg) in group.subgroups.iter().enumerate() {
            if sg.resource_type == resource_type {
                found = Some(i);
                break;
            }
        }
        found.unwrap_or_else(|| {
            group.subgroups.push(ResourceSubgroup {
                resource_type,
                label: resource_type.label().to_string(),
                items: Vec::new(),
            });
            group.subgroups.len() - 1
        })
    };

    // displayName (config-selector.ts:131-140).
    let path = &resource.path;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parent_folder = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let display_name = match resource_type {
        ResourceType::Extensions if parent_folder != "extensions" => {
            format!("{parent_folder}/{file_name}")
        }
        ResourceType::Skills if file_name == "SKILL.md" => parent_folder,
        _ => file_name,
    };

    groups[group_idx].subgroups[subgroup_idx]
        .items
        .push(ResourceItem {
            path: resource.path.to_string_lossy().into_owned(),
            enabled: resource.enabled,
            metadata: metadata.clone(),
            resource_type,
            display_name,
        });
}

/// `FlatEntry` (config-selector.ts:182-185): a flattened group → subgroup →
/// item entry. Indices into the current write scope's groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlatEntry {
    Group {
        group: usize,
    },
    Subgroup {
        group: usize,
        subgroup: usize,
    },
    Item {
        group: usize,
        subgroup: usize,
        item: usize,
    },
}

/// `ConfigSelectorHeader` (config-selector.ts:187-220).
struct ConfigSelectorHeader {
    write_scope: Arc<Mutex<ConfigWriteScope>>,
    project_mode_available: bool,
    theme: Arc<Theme>,
}

impl ConfigSelectorHeader {
    fn new(
        theme: Arc<Theme>,
        write_scope: Arc<Mutex<ConfigWriteScope>>,
        project_mode_available: bool,
    ) -> Self {
        Self {
            write_scope,
            project_mode_available,
            theme,
        }
    }

    fn scope(&self) -> ConfigWriteScope {
        *lock(&self.write_scope)
    }

    /// `render` (config-selector.ts:202-219).
    fn render(&self, width: usize) -> Vec<String> {
        let title = Theme::bold(if self.scope() == ConfigWriteScope::Project {
            "Project Local Resources"
        } else {
            "Global Resources"
        });
        let sep = self.theme.fg("muted", " · ");
        let switch_hint = if self.project_mode_available {
            format!(
                "{}{}",
                key_hint(&self.theme, "tui.input.tab", "switch mode"),
                sep
            )
        } else {
            String::new()
        };
        let action_hint = if self.scope() == ConfigWriteScope::Project {
            raw_key_hint(&self.theme, "space", "cycle inherit/+/-")
        } else {
            raw_key_hint(&self.theme, "space", "toggle")
        };
        let hint = format!(
            "{switch_hint}{action_hint}{sep}{}",
            raw_key_hint(&self.theme, "esc", "close")
        );
        let spacing = 1usize.max(
            width
                .saturating_sub(visible_width(&title))
                .saturating_sub(visible_width(&hint)),
        );
        let scope_hint = if self.scope() == ConfigWriteScope::Project {
            self.theme.fg(
                "muted",
                &format!("{CONFIG_DIR_NAME}/settings.json · inherited global resources are dimmed"),
            )
        } else {
            self.theme
                .fg("muted", &format!("~/{CONFIG_DIR_NAME}/agent/settings.json"))
        };

        vec![
            truncate_to_width(
                &format!("{title}{}{hint}", " ".repeat(spacing)),
                width,
                "",
                false,
            ),
            truncate_to_width(&scope_hint, width, "", false),
        ]
    }
}

/// `isLocalPath` (utils/paths.ts:41-56) — local copy (the package-manager
/// twin is module-private).
fn is_local_path(value: &str) -> bool {
    let trimmed = value.trim();
    !(trimmed.starts_with("npm:")
        || trimmed.starts_with("git:")
        || trimmed.starts_with("github:")
        || trimmed.starts_with("http:")
        || trimmed.starts_with("https:")
        || trimmed.starts_with("ssh:"))
}

/// `(settings[key] ?? []) as string[]` (non-string entries dropped).
fn settings_string_array(settings: &Settings, key: &str) -> Vec<String> {
    settings
        .as_map()
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// `[...(settings.packages ?? [])]` (malformed entries dropped, D-040 #8).
fn packages_of(settings: &Settings) -> Vec<PackageSource> {
    settings
        .as_map()
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|value| serde_json::from_value(value.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn package_source_string(pkg: &PackageSource) -> &str {
    match pkg {
        PackageSource::Source(source) => source,
        PackageSource::Filtered(filter) => &filter.source,
    }
}

/// `getPatternEntryTarget` (config-selector.ts:838-840).
fn get_pattern_entry_target(entry: &str) -> &str {
    match entry.strip_prefix(['!', '+', '-']) {
        Some(stripped) => stripped,
        None => entry,
    }
}

fn filter_array(filter: &PackageSourceFilter, resource_type: ResourceType) -> Option<&Vec<String>> {
    match resource_type {
        ResourceType::Extensions => filter.extensions.as_ref(),
        ResourceType::Skills => filter.skills.as_ref(),
        ResourceType::Prompts => filter.prompts.as_ref(),
        ResourceType::Themes => filter.themes.as_ref(),
    }
}

fn set_filter_array(
    filter: &mut PackageSourceFilter,
    resource_type: ResourceType,
    entries: Option<Vec<String>>,
) {
    match resource_type {
        ResourceType::Extensions => filter.extensions = entries,
        ResourceType::Skills => filter.skills = entries,
        ResourceType::Prompts => filter.prompts = entries,
        ResourceType::Themes => filter.themes = entries,
    }
}

/// `ResourceList` (config-selector.ts:222-864).
pub struct ResourceList {
    groups_global: Vec<ResourceGroup>,
    groups_project: Vec<ResourceGroup>,
    flat_items: Vec<FlatEntry>,
    filtered_items: Vec<FlatEntry>,
    selected_index: usize,
    search_input: Input,
    max_visible: usize,
    settings_manager: Arc<Mutex<SettingsManager>>,
    cwd: PathBuf,
    agent_dir: PathBuf,
    write_scope: Arc<Mutex<ConfigWriteScope>>,
    /// `inheritedEnabledByKey` (config-selector.ts:233) — built from the
    /// global groups.
    inherited_enabled_by_key: HashMap<String, bool>,
    theme: Arc<Theme>,
    project_mode_available: bool,
    focused: bool,

    /// `onCancel` (config-selector.ts:235).
    pub on_cancel: Option<Box<dyn FnMut() + Send>>,
    /// `onExit` (config-selector.ts:236).
    pub on_exit: Option<Box<dyn FnMut() + Send>>,
}

impl ResourceList {
    #[allow(clippy::too_many_arguments)] // mirrors the upstream constructor
    fn new(
        resolved_paths: &ScopedResolvedPaths,
        settings_manager: Arc<Mutex<SettingsManager>>,
        theme: Arc<Theme>,
        write_scope: Arc<Mutex<ConfigWriteScope>>,
        cwd: &Path,
        agent_dir: &Path,
        terminal_height: Option<usize>,
        project_mode_available: bool,
    ) -> Self {
        let groups_global = build_groups(&resolved_paths.global, agent_dir);
        let groups_project = build_groups(&resolved_paths.project, agent_dir);
        let inherited_enabled_by_key = Self::build_inherited_enabled_map(&groups_global);
        let mut list = Self {
            groups_global,
            groups_project,
            flat_items: Vec::new(),
            filtered_items: Vec::new(),
            selected_index: 0,
            search_input: Input::new(),
            // 8 lines of chrome: top spacer + top border + spacer + header
            // (2 lines) + spacer + bottom spacer + bottom border
            // (config-selector.ts:264-266).
            max_visible: 5usize.max(terminal_height.unwrap_or(24).saturating_sub(8)),
            settings_manager,
            cwd: cwd.to_path_buf(),
            agent_dir: agent_dir.to_path_buf(),
            write_scope,
            inherited_enabled_by_key,
            theme,
            project_mode_available,
            focused: false,
            on_cancel: None,
            on_exit: None,
        };
        list.build_flat_list();
        list.filtered_items = list.flat_items.clone();
        list
    }

    fn scope(&self) -> ConfigWriteScope {
        *lock(&self.write_scope)
    }

    /// `get groups()` (config-selector.ts:277-279).
    fn groups(&self) -> &Vec<ResourceGroup> {
        match self.scope() {
            ConfigWriteScope::Global => &self.groups_global,
            ConfigWriteScope::Project => &self.groups_project,
        }
    }

    fn groups_mut(&mut self) -> &mut Vec<ResourceGroup> {
        match self.scope() {
            ConfigWriteScope::Global => &mut self.groups_global,
            ConfigWriteScope::Project => &mut self.groups_project,
        }
    }

    /// `switchWriteScope` (config-selector.ts:933-937) — the upstream
    /// `onSwitchMode` closure inlines this plus a render request (the
    /// component owns its state, so renders are self-consistent).
    fn switch_write_scope(&mut self) {
        {
            let mut scope = lock(&self.write_scope);
            *scope = match *scope {
                ConfigWriteScope::Global => ConfigWriteScope::Project,
                ConfigWriteScope::Project => ConfigWriteScope::Global,
            };
        }
        self.build_flat_list();
        let query = self.search_input.get_value().to_string();
        self.filter_items(&query);
    }

    /// `buildInheritedEnabledMap` (config-selector.ts:281-291).
    fn build_inherited_enabled_map(groups: &[ResourceGroup]) -> HashMap<String, bool> {
        let mut result = HashMap::new();
        for group in groups {
            for subgroup in &group.subgroups {
                for item in &subgroup.items {
                    result.insert(get_resource_item_key(item), item.enabled);
                }
            }
        }
        result
    }

    /// `buildFlatList` (config-selector.ts:293-307): start selection on the
    /// first item (not header).
    fn build_flat_list(&mut self) {
        self.flat_items = Vec::new();
        let scope = self.scope();
        let groups = match scope {
            ConfigWriteScope::Global => &self.groups_global,
            ConfigWriteScope::Project => &self.groups_project,
        };
        let mut flat_items = std::mem::take(&mut self.flat_items);
        for (group_idx, group) in groups.iter().enumerate() {
            flat_items.push(FlatEntry::Group { group: group_idx });
            for (subgroup_idx, subgroup) in group.subgroups.iter().enumerate() {
                flat_items.push(FlatEntry::Subgroup {
                    group: group_idx,
                    subgroup: subgroup_idx,
                });
                for item_idx in 0..subgroup.items.len() {
                    flat_items.push(FlatEntry::Item {
                        group: group_idx,
                        subgroup: subgroup_idx,
                        item: item_idx,
                    });
                }
            }
        }
        self.flat_items = flat_items;
        self.selected_index = self
            .flat_items
            .iter()
            .position(|e| matches!(e, FlatEntry::Item { .. }))
            .unwrap_or(0);
    }

    /// `findNextItem` (config-selector.ts:309-318).
    fn find_next_item(&self, from_index: usize, direction: isize) -> usize {
        let mut idx = from_index as isize + direction;
        while idx >= 0 && (idx as usize) < self.filtered_items.len() {
            if matches!(self.filtered_items[idx as usize], FlatEntry::Item { .. }) {
                return idx as usize;
            }
            idx += direction;
        }
        from_index // Stay at current if no item found
    }

    /// `filterItems` (config-selector.ts:320-369).
    fn filter_items(&mut self, query: &str) {
        if query.trim().is_empty() {
            self.filtered_items = self.flat_items.clone();
            self.select_first_item();
            return;
        }

        let lower_query = query.to_lowercase();
        let mut matching_items: HashSet<(usize, usize, usize)> = HashSet::new();
        let mut matching_subgroups: HashSet<(usize, usize)> = HashSet::new();
        let mut matching_groups: HashSet<usize> = HashSet::new();

        for entry in &self.flat_items {
            if let FlatEntry::Item {
                group,
                subgroup,
                item,
            } = entry
            {
                let resource = &self.groups()[*group].subgroups[*subgroup].items[*item];
                if resource.display_name.to_lowercase().contains(&lower_query)
                    || resource.resource_type.as_str().contains(&lower_query)
                    || resource.path.to_lowercase().contains(&lower_query)
                {
                    matching_items.insert((*group, *subgroup, *item));
                }
            }
        }

        // Find which subgroups and groups contain matching items.
        for (group_idx, group) in self.groups().iter().enumerate() {
            for (subgroup_idx, subgroup) in group.subgroups.iter().enumerate() {
                for item_idx in 0..subgroup.items.len() {
                    if matching_items.contains(&(group_idx, subgroup_idx, item_idx)) {
                        matching_subgroups.insert((group_idx, subgroup_idx));
                        matching_groups.insert(group_idx);
                    }
                }
            }
        }

        self.filtered_items = self
            .flat_items
            .iter()
            .filter(|entry| match entry {
                FlatEntry::Group { group } => matching_groups.contains(group),
                FlatEntry::Subgroup { group, subgroup } => {
                    matching_subgroups.contains(&(*group, *subgroup))
                }
                FlatEntry::Item {
                    group,
                    subgroup,
                    item,
                } => matching_items.contains(&(*group, *subgroup, *item)),
            })
            .copied()
            .collect();

        self.select_first_item();
    }

    /// `selectFirstItem` (config-selector.ts:371-374).
    fn select_first_item(&mut self) {
        let first_item_index = self
            .filtered_items
            .iter()
            .position(|e| matches!(e, FlatEntry::Item { .. }));
        self.selected_index = first_item_index.unwrap_or(0);
    }

    /// `updateItem` (config-selector.ts:376-388): set `enabled` on the
    /// toggled item (and any duplicate entries of it in the current view).
    fn update_item(&mut self, path: &str, resource_type: ResourceType, enabled: bool) {
        for group in self.groups_mut() {
            for subgroup in &mut group.subgroups {
                for item in &mut subgroup.items {
                    if item.path == path && item.resource_type == resource_type {
                        item.enabled = enabled;
                    }
                }
            }
        }
    }

    /// `render` (config-selector.ts:392-452).
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();

        // Search input.
        lines.extend(self.search_input.render(width));
        lines.push(String::new());

        if self.filtered_items.is_empty() {
            lines.push(self.theme.fg("muted", "  No resources found"));
            return lines;
        }

        // Calculate visible range (config-selector.ts:405-409).
        let start_index = self
            .selected_index
            .saturating_sub(self.max_visible / 2)
            .min(self.filtered_items.len().saturating_sub(self.max_visible));
        let end_index = (start_index + self.max_visible).min(self.filtered_items.len());

        for i in start_index..end_index {
            let entry = self.filtered_items[i];
            let is_selected = i == self.selected_index;

            match entry {
                FlatEntry::Group { group } => {
                    // Main group header (no cursor).
                    let inherited = self.scope() == ConfigWriteScope::Project
                        && self.groups()[group].scope == SourceScope::User;
                    let label = Theme::bold(&format!(
                        "{}{}",
                        self.groups()[group].label,
                        if inherited {
                            " · inherited global"
                        } else {
                            ""
                        }
                    ));
                    let group_line = self
                        .theme
                        .fg(if inherited { "dim" } else { "accent" }, &label);
                    lines.push(truncate_to_width(
                        &format!("  {group_line}"),
                        width,
                        "",
                        false,
                    ));
                }
                FlatEntry::Subgroup { group, subgroup } => {
                    // Subgroup header (indented, no cursor).
                    let color = if self.scope() == ConfigWriteScope::Project
                        && self.groups()[group].scope == SourceScope::User
                    {
                        "dim"
                    } else {
                        "muted"
                    };
                    let subgroup_line = self
                        .theme
                        .fg(color, &self.groups()[group].subgroups[subgroup].label);
                    lines.push(truncate_to_width(
                        &format!("    {subgroup_line}"),
                        width,
                        "",
                        false,
                    ));
                }
                FlatEntry::Item {
                    group,
                    subgroup,
                    item,
                } => {
                    // Resource item (cursor only on items).
                    let item = &self.groups()[group].subgroups[subgroup].items[item];
                    let cursor = if is_selected { "> " } else { "  " };
                    let dimmed = self.is_dimmed_item(item);
                    let name_text = if is_selected && !dimmed {
                        Theme::bold(&item.display_name)
                    } else {
                        item.display_name.clone()
                    };
                    let name = if dimmed {
                        self.theme.fg("dim", &name_text)
                    } else {
                        name_text
                    };
                    lines.push(truncate_to_width(
                        &format!(
                            "{cursor}    {} {name}{}",
                            self.render_checkbox(item),
                            self.get_item_suffix(item),
                        ),
                        width,
                        "...",
                        false,
                    ));
                }
            }
        }

        // Scroll indicator (config-selector.ts:444-449).
        if start_index > 0 || end_index < self.filtered_items.len() {
            let item_count = self
                .filtered_items
                .iter()
                .filter(|e| matches!(e, FlatEntry::Item { .. }))
                .count();
            let current_item_index = self.filtered_items[..self.selected_index]
                .iter()
                .filter(|e| matches!(e, FlatEntry::Item { .. }))
                .count()
                + 1;
            lines.push(
                self.theme
                    .fg("dim", &format!("  ({current_item_index}/{item_count})")),
            );
        }

        lines
    }

    /// `handleInput` (config-selector.ts:454-514).
    fn handle_input(&mut self, data: &str) {
        let kb = get_keybindings();
        let read = kb.read().unwrap_or_else(|poisoned| poisoned.into_inner());

        if read.matches_id(data, "tui.select.up") {
            self.selected_index = self.find_next_item(self.selected_index, -1);
            return;
        }
        if read.matches_id(data, "tui.select.down") {
            self.selected_index = self.find_next_item(self.selected_index, 1);
            return;
        }
        if read.matches_id(data, "tui.select.pageUp") {
            // Jump up by maxVisible, then find nearest item
            // (config-selector.ts:465-475).
            let mut target = self.selected_index.saturating_sub(self.max_visible);
            while target < self.filtered_items.len()
                && !matches!(self.filtered_items[target], FlatEntry::Item { .. })
            {
                target += 1;
            }
            if target < self.filtered_items.len() {
                self.selected_index = target;
            }
            return;
        }
        if read.matches_id(data, "tui.select.pageDown") {
            // Jump down by maxVisible, then find nearest item
            // (config-selector.ts:476-486).
            if self.filtered_items.is_empty() {
                return;
            }
            let mut target = self
                .selected_index
                .saturating_add(self.max_visible)
                .min(self.filtered_items.len() - 1) as isize;
            while target >= 0
                && !matches!(self.filtered_items[target as usize], FlatEntry::Item { .. })
            {
                target -= 1;
            }
            if target >= 0 {
                self.selected_index = target as usize;
            }
            return;
        }
        if read.matches_id(data, "tui.select.cancel") {
            if let Some(on_cancel) = self.on_cancel.as_mut() {
                on_cancel();
            }
            return;
        }
        if matches_key(data, "ctrl+c") {
            if let Some(on_exit) = self.on_exit.as_mut() {
                on_exit();
            }
            return;
        }
        if read.matches_id(data, "tui.input.tab") {
            if self.project_mode_available {
                self.switch_write_scope();
            }
            return;
        }
        if data == " " || read.matches_id(data, "tui.select.confirm") {
            if let Some(FlatEntry::Item {
                group,
                subgroup,
                item,
            }) = self.filtered_items.get(self.selected_index)
            {
                let (group, subgroup, item) = (*group, *subgroup, *item);
                // config-selector.ts:501: project scope toggles anything,
                // global scope only user-scope items.
                let allowed = {
                    let resource = &self.groups()[group].subgroups[subgroup].items[item];
                    self.scope() == ConfigWriteScope::Project
                        || get_item_scope(resource) == SourceScope::User
                };
                if allowed {
                    if let Some((path, resource_type, enabled)) =
                        self.toggle_resource(group, subgroup, item)
                    {
                        self.update_item(&path, resource_type, enabled);
                    }
                }
            }
            return;
        }

        // Pass to search input.
        drop(read);
        self.search_input.handle_input(data);
        let query = self.search_input.get_value().to_string();
        self.filter_items(&query);
    }

    /// `toggleResource` (config-selector.ts:516-530): returns
    /// `(path, resource_type, new enabled)` for the `updateItem` call, or
    /// `None` when the write failed.
    fn toggle_resource(
        &mut self,
        group: usize,
        subgroup: usize,
        item: usize,
    ) -> Option<(String, ResourceType, bool)> {
        let item_ref = self.groups()[group].subgroups[subgroup].items[item].clone();
        if self.scope() == ConfigWriteScope::Project {
            let state = self.get_next_override_state(&item_ref);
            if !self.set_project_resource_override(&item_ref, state) {
                return None;
            }
            let enabled = match state {
                ProjectOverrideState::Inherit => self.get_inherited_enabled(&item_ref),
                ProjectOverrideState::Load => true,
                ProjectOverrideState::Unload => false,
            };
            return Some((item_ref.path, item_ref.resource_type, enabled));
        }

        let enabled = !item_ref.enabled;
        if item_ref.metadata.origin == SourceOrigin::TopLevel {
            self.toggle_top_level_resource(&item_ref, enabled);
        } else {
            self.toggle_package_resource(&item_ref, enabled);
        }
        Some((item_ref.path, item_ref.resource_type, enabled))
    }

    /// `toggleTopLevelResource` (config-selector.ts:532-578).
    fn toggle_top_level_resource(&self, item: &ResourceItem, enabled: bool) {
        let scope = get_item_scope(item);
        let pattern = self.get_resource_pattern(item);
        let replacement = if enabled {
            format!("+{pattern}")
        } else {
            format!("-{pattern}")
        };
        let mut manager = lock(&self.settings_manager);
        let settings = Self::settings_for_scope(&manager, scope);
        let current = settings_string_array(&settings, item.resource_type.as_str());
        let mut updated: Vec<String> = current
            .into_iter()
            .filter(|p| get_pattern_entry_target(p) != pattern)
            .collect();
        updated.push(replacement);
        // User-scope writes are infallible (M5: only project writes can
        // fail, surfaced in the project override path).
        let _ = self.set_top_level_paths(&mut manager, scope, item.resource_type, updated);
    }

    /// `togglePackageResource` (config-selector.ts:580-637).
    fn toggle_package_resource(&self, item: &ResourceItem, enabled: bool) {
        let scope = get_item_scope(item);
        let pattern = self.get_package_resource_pattern(item);
        let replacement = if enabled {
            format!("+{pattern}")
        } else {
            format!("-{pattern}")
        };
        let mut manager = lock(&self.settings_manager);
        let settings = Self::settings_for_scope(&manager, scope);
        let mut packages = packages_of(&settings);
        let Some(pkg_index) = packages
            .iter()
            .position(|pkg| package_source_string(pkg) == item.metadata.source)
        else {
            return;
        };

        // Convert string to object form if needed (config-selector.ts:596-599).
        let mut filter = match &packages[pkg_index] {
            PackageSource::Source(source) => PackageSourceFilter {
                source: source.clone(),
                ..PackageSourceFilter::default()
            },
            PackageSource::Filtered(filter) => filter.clone(),
        };

        let current = filter_array(&filter, item.resource_type)
            .cloned()
            .unwrap_or_default();
        let mut updated: Vec<String> = current
            .into_iter()
            .filter(|p| get_pattern_entry_target(p) != pattern)
            .collect();
        updated.push(replacement);
        set_filter_array(
            &mut filter,
            item.resource_type,
            if updated.is_empty() {
                None
            } else {
                Some(updated)
            },
        );

        // Clean up empty filter object (config-selector.ts:624-630).
        let has_filters = RESOURCE_TYPES
            .iter()
            .any(|key| filter_array(&filter, *key).is_some());
        packages[pkg_index] = if has_filters {
            PackageSource::Filtered(filter)
        } else {
            PackageSource::Source(filter.source)
        };

        // User-scope writes are infallible (M5).
        let _ = self.write_packages(&mut manager, scope, packages);
    }

    /// `renderCheckbox` (config-selector.ts:639-647).
    fn render_checkbox(&self, item: &ResourceItem) -> String {
        if self.scope() == ConfigWriteScope::Project {
            match self.get_project_override_state(item) {
                ProjectOverrideState::Load => return self.theme.fg("success", "[+]"),
                ProjectOverrideState::Unload => return self.theme.fg("warning", "[-]"),
                ProjectOverrideState::Inherit => {}
            }
            return self
                .theme
                .fg("dim", if item.enabled { "[x]" } else { "[ ]" });
        }
        if item.enabled {
            self.theme.fg("success", "[x]")
        } else {
            self.theme.fg("dim", "[ ]")
        }
    }

    /// `getItemSuffix` (config-selector.ts:649-655).
    fn get_item_suffix(&self, item: &ResourceItem) -> String {
        if self.scope() != ConfigWriteScope::Project {
            return String::new();
        }
        match self.get_project_override_state(item) {
            ProjectOverrideState::Load => self.theme.fg("muted", "  project load"),
            ProjectOverrideState::Unload => self.theme.fg("muted", "  project unload"),
            ProjectOverrideState::Inherit => {
                if self.is_inherited_global_item(item) {
                    self.theme.fg("dim", "  inherited global")
                } else {
                    String::new()
                }
            }
        }
    }

    /// `isDimmedItem` (config-selector.ts:657-663).
    fn is_dimmed_item(&self, item: &ResourceItem) -> bool {
        self.scope() == ConfigWriteScope::Project
            && self.is_inherited_global_item(item)
            && self.get_project_override_state(item) == ProjectOverrideState::Inherit
    }

    /// `setProjectResourceOverride` (config-selector.ts:665-669).
    fn set_project_resource_override(
        &self,
        item: &ResourceItem,
        state: ProjectOverrideState,
    ) -> bool {
        if item.metadata.origin == SourceOrigin::TopLevel {
            self.set_project_top_level_override(item, state)
        } else {
            self.set_project_package_override(item, state)
        }
    }

    /// `setProjectTopLevelOverride` (config-selector.ts:671-687).
    fn set_project_top_level_override(
        &self,
        item: &ResourceItem,
        state: ProjectOverrideState,
    ) -> bool {
        let inherited = self.is_inherited_global_item(item);
        let pattern = if inherited {
            item.path.clone()
        } else {
            self.get_resource_pattern_for_scope(item, SourceScope::Project)
        };
        let patterns = self.get_top_level_override_patterns(item, SourceScope::Project);
        let mut manager = lock(&self.settings_manager);
        let current =
            settings_string_array(&manager.get_project_settings(), item.resource_type.as_str());
        let mut updated: Vec<String> = current
            .into_iter()
            .filter(|entry| {
                let target = get_pattern_entry_target(entry);
                if entry.starts_with(['!', '+', '-']) && patterns.contains(target) {
                    return false;
                }
                !(state == ProjectOverrideState::Inherit && inherited && target == pattern)
            })
            .collect();
        if state != ProjectOverrideState::Inherit {
            if inherited && !updated.contains(&pattern) {
                updated.push(pattern.clone());
            }
            let prefix = if state == ProjectOverrideState::Load {
                '+'
            } else {
                '-'
            };
            updated.push(format!("{prefix}{pattern}"));
        }
        self.set_top_level_paths(
            &mut manager,
            SourceScope::Project,
            item.resource_type,
            updated,
        )
        .map_err(|error| {
            // Surface the write failure (T14 review M5): the toggle stays
            // un-applied (caller returns `None`), mirroring the upstream
            // throw. Stderr keeps the failure visible without a component
            // notification channel.
            eprintln!("pir config: failed to write project settings: {error}");
            error
        })
        .is_ok()
    }

    /// `setProjectPackageOverride` (config-selector.ts:696-729).
    fn set_project_package_override(
        &self,
        item: &ResourceItem,
        state: ProjectOverrideState,
    ) -> bool {
        let item_scope = get_item_scope(item);
        let pattern = self.get_package_resource_pattern(item);
        let mut manager = lock(&self.settings_manager);
        let mut packages = packages_of(&manager.get_project_settings());
        let pkg_index = packages.iter().position(|pkg| {
            self.package_source_string_matches(
                &item.metadata.source,
                item_scope,
                package_source_string(pkg),
                SourceScope::Project,
            )
        });
        let pkg_index = match pkg_index {
            Some(index) => index,
            None => {
                if state == ProjectOverrideState::Inherit {
                    return false;
                }
                packages.push(self.create_package_override_source(item));
                packages.len() - 1
            }
        };

        let mut filter = match &packages[pkg_index] {
            PackageSource::Source(source) => PackageSourceFilter {
                source: source.clone(),
                ..PackageSourceFilter::default()
            },
            PackageSource::Filtered(filter) => filter.clone(),
        };
        let mut updated: Vec<String> = filter_array(&filter, item.resource_type)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| get_pattern_entry_target(entry) != pattern)
            .collect();
        if state != ProjectOverrideState::Inherit {
            let prefix = if state == ProjectOverrideState::Load {
                '+'
            } else {
                '-'
            };
            updated.push(format!("{prefix}{pattern}"));
        }
        set_filter_array(
            &mut filter,
            item.resource_type,
            if updated.is_empty() {
                None
            } else {
                Some(updated)
            },
        );
        let has_filters = RESOURCE_TYPES
            .iter()
            .any(|key| filter_array(&filter, *key).is_some());
        if !has_filters {
            if filter.autoload == Some(false) {
                packages.remove(pkg_index);
            } else {
                packages[pkg_index] = PackageSource::Source(filter.source);
            }
        } else {
            packages[pkg_index] = PackageSource::Filtered(filter);
        }
        self.write_packages(&mut manager, SourceScope::Project, packages)
            .map_err(|error| {
                // Surface the write failure (T14 review M5): the toggle
                // stays un-applied (caller returns `None`), mirroring the
                // upstream throw.
                eprintln!("pir config: failed to write project settings: {error}");
                error
            })
            .is_ok()
    }

    /// `getNextOverrideState` (config-selector.ts:731-737).
    fn get_next_override_state(&self, item: &ResourceItem) -> ProjectOverrideState {
        let state = self.get_project_override_state(item);
        let inherited_enabled = self.get_inherited_enabled(item);
        match state {
            ProjectOverrideState::Inherit => {
                if inherited_enabled {
                    ProjectOverrideState::Unload
                } else {
                    ProjectOverrideState::Load
                }
            }
            ProjectOverrideState::Unload => {
                if inherited_enabled {
                    ProjectOverrideState::Load
                } else {
                    ProjectOverrideState::Inherit
                }
            }
            ProjectOverrideState::Load => {
                if inherited_enabled {
                    ProjectOverrideState::Inherit
                } else {
                    ProjectOverrideState::Unload
                }
            }
        }
    }

    /// `getProjectOverrideState` (config-selector.ts:739-757).
    fn get_project_override_state(&self, item: &ResourceItem) -> ProjectOverrideState {
        if self.scope() != ConfigWriteScope::Project {
            return ProjectOverrideState::Inherit;
        }
        if item.metadata.origin == SourceOrigin::TopLevel {
            let entries = {
                let manager = lock(&self.settings_manager);
                settings_string_array(&manager.get_project_settings(), item.resource_type.as_str())
            };
            let patterns = self.get_top_level_override_patterns(item, SourceScope::Project);
            return Self::get_override_state_from_entries(&entries, &patterns, false);
        }
        let pkg = {
            let manager = lock(&self.settings_manager);
            self.find_matching_package_source(&manager, item, SourceScope::Project)
        };
        let Some(PackageSource::Filtered(filter)) = pkg else {
            return ProjectOverrideState::Inherit;
        };
        let Some(entries) = filter_array(&filter, item.resource_type) else {
            return ProjectOverrideState::Inherit;
        };
        let patterns = HashSet::from([self.get_package_resource_pattern(item)]);
        Self::get_override_state_from_entries(entries, &patterns, filter.autoload != Some(false))
    }

    /// `getOverrideStateFromEntries` (config-selector.ts:759-772).
    fn get_override_state_from_entries(
        entries: &[String],
        patterns: &HashSet<String>,
        empty_array_is_unload: bool,
    ) -> ProjectOverrideState {
        if entries.is_empty() && empty_array_is_unload {
            return ProjectOverrideState::Unload;
        }
        let mut state = ProjectOverrideState::Inherit;
        for entry in entries {
            if !patterns.contains(get_pattern_entry_target(entry)) {
                continue;
            }
            if entry.starts_with('!') || entry.starts_with('-') {
                state = ProjectOverrideState::Unload;
            } else {
                state = ProjectOverrideState::Load;
            }
        }
        state
    }

    /// `getInheritedEnabled` (config-selector.ts:774-779).
    fn get_inherited_enabled(&self, item: &ResourceItem) -> bool {
        self.inherited_enabled_by_key
            .get(&get_resource_item_key(item))
            .copied()
            .unwrap_or_else(|| {
                if get_item_scope(item) == SourceScope::User {
                    item.enabled
                } else {
                    true
                }
            })
    }

    /// `isInheritedGlobalItem` (config-selector.ts:781-783).
    fn is_inherited_global_item(&self, item: &ResourceItem) -> bool {
        get_item_scope(item) == SourceScope::User
            || self
                .inherited_enabled_by_key
                .contains_key(&get_resource_item_key(item))
    }

    /// `getTopLevelOverridePatterns` (config-selector.ts:785-794).
    fn get_top_level_override_patterns(
        &self,
        item: &ResourceItem,
        scope: SourceScope,
    ) -> HashSet<String> {
        let base_dir = self.get_top_level_base_dir(scope);
        let mut patterns = HashSet::from([
            self.get_resource_pattern_for_scope(item, scope),
            item.path.clone(),
            lexical_relative(&base_dir, Path::new(&item.path))
                .to_string_lossy()
                .into_owned(),
        ]);
        if let Some(base_dir) = &item.metadata.base_dir {
            patterns.insert(
                lexical_relative(base_dir, Path::new(&item.path))
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        patterns
    }

    /// `getResourcePatternForScope` (config-selector.ts:796-801).
    fn get_resource_pattern_for_scope(&self, item: &ResourceItem, scope: SourceScope) -> String {
        let source_scope = get_item_scope(item);
        if scope != source_scope {
            return item.path.clone();
        }
        let base_dir = item
            .metadata
            .base_dir
            .clone()
            .unwrap_or_else(|| self.get_top_level_base_dir(source_scope));
        lexical_relative(&base_dir, Path::new(&item.path))
            .to_string_lossy()
            .into_owned()
    }

    /// `createPackageOverrideSource` (config-selector.ts:803-808).
    fn create_package_override_source(&self, item: &ResourceItem) -> PackageSource {
        let source = &item.metadata.source;
        if !is_local_path(source) {
            return PackageSource::Filtered(PackageSourceFilter {
                source: source.clone(),
                autoload: Some(false),
                ..PackageSourceFilter::default()
            });
        }
        let base_dir = self.get_top_level_base_dir(get_item_scope(item));
        let source_path = resolve_path(source.trim(), &base_dir);
        let project_base_dir = self.get_top_level_base_dir(SourceScope::Project);
        let relative = lexical_relative(&project_base_dir, &source_path);
        let relative = relative.to_string_lossy();
        PackageSource::Filtered(PackageSourceFilter {
            source: if relative.is_empty() {
                ".".to_string()
            } else {
                relative.into_owned()
            },
            autoload: Some(false),
            ..PackageSourceFilter::default()
        })
    }

    /// `packageSourceStringMatches` (config-selector.ts:810-821).
    fn package_source_string_matches(
        &self,
        left_source: &str,
        left_scope: SourceScope,
        right_source: &str,
        right_scope: SourceScope,
    ) -> bool {
        if left_source == right_source {
            return true;
        }
        if !is_local_path(left_source) || !is_local_path(right_source) {
            return false;
        }
        let left = resolve_path(left_source.trim(), &self.get_top_level_base_dir(left_scope));
        let right = resolve_path(
            right_source.trim(),
            &self.get_top_level_base_dir(right_scope),
        );
        left == right
    }

    /// `findMatchingPackageSource` (config-selector.ts:823-836).
    fn find_matching_package_source(
        &self,
        manager: &SettingsManager,
        item: &ResourceItem,
        target_scope: SourceScope,
    ) -> Option<PackageSource> {
        let settings = Self::settings_for_scope(manager, target_scope);
        packages_of(&settings).into_iter().find(|pkg| {
            self.package_source_string_matches(
                &item.metadata.source,
                get_item_scope(item),
                package_source_string(pkg),
                target_scope,
            )
        })
    }

    /// `getTopLevelBaseDir` (config-selector.ts:850-852).
    fn get_top_level_base_dir(&self, scope: SourceScope) -> PathBuf {
        if scope == SourceScope::Project {
            self.cwd.join(CONFIG_DIR_NAME)
        } else {
            self.agent_dir.clone()
        }
    }

    /// `getResourcePattern` (config-selector.ts:854-858).
    fn get_resource_pattern(&self, item: &ResourceItem) -> String {
        let scope = get_item_scope(item);
        let base_dir = item
            .metadata
            .base_dir
            .clone()
            .unwrap_or_else(|| self.get_top_level_base_dir(scope));
        lexical_relative(&base_dir, Path::new(&item.path))
            .to_string_lossy()
            .into_owned()
    }

    /// `getPackageResourcePattern` (config-selector.ts:860-863).
    fn get_package_resource_pattern(&self, item: &ResourceItem) -> String {
        let path = Path::new(&item.path);
        let base_dir = item.metadata.base_dir.clone().unwrap_or_else(|| {
            path.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        });
        lexical_relative(&base_dir, path)
            .to_string_lossy()
            .into_owned()
    }

    /// Settings of one write target (`scope === "project" ? getProjectSettings()
    /// : getGlobalSettings()`).
    fn settings_for_scope(manager: &SettingsManager, scope: SourceScope) -> Settings {
        if scope == SourceScope::Project {
            manager.get_project_settings()
        } else {
            manager.get_global_settings()
        }
    }

    /// The `setProject*Paths` / `set*Paths` dispatch
    /// (config-selector.ts:557-577, 689-694). Project writes are trust-gated
    /// by the `pir config` command, so the setter `Result` cannot fail here
    /// (upstream would throw).
    /// The `setProject*Paths` / `set*Paths` dispatch
    /// (config-selector.ts:557-577, 689-694). Project writes are trust-gated
    /// by the `pir config` command, but can still fail (pre-existing
    /// unparseable `.pir/settings.json`, storage IO errors) — the error is
    /// surfaced to the caller (T14 review M5; upstream would throw). User
    /// scope setters are infallible.
    fn set_top_level_paths(
        &self,
        manager: &mut SettingsManager,
        scope: SourceScope,
        resource_type: ResourceType,
        paths: Vec<String>,
    ) -> Result<(), String> {
        match (scope == SourceScope::Project, resource_type) {
            (true, ResourceType::Extensions) => manager
                .set_project_extension_paths(paths)
                .map_err(|e| e.to_string()),
            (true, ResourceType::Skills) => manager
                .set_project_skill_paths(paths)
                .map_err(|e| e.to_string()),
            (true, ResourceType::Prompts) => manager
                .set_project_prompt_template_paths(paths)
                .map_err(|e| e.to_string()),
            (true, ResourceType::Themes) => manager
                .set_project_theme_paths(paths)
                .map_err(|e| e.to_string()),
            (false, ResourceType::Extensions) => {
                manager.set_extension_paths(paths);
                Ok(())
            }
            (false, ResourceType::Skills) => {
                manager.set_skill_paths(paths);
                Ok(())
            }
            (false, ResourceType::Prompts) => {
                manager.set_prompt_template_paths(paths);
                Ok(())
            }
            (false, ResourceType::Themes) => {
                manager.set_theme_paths(paths);
                Ok(())
            }
        }
    }

    /// `setPackages` / `setProjectPackages` (config-selector.ts:632-636, 727).
    fn write_packages(
        &self,
        manager: &mut SettingsManager,
        scope: SourceScope,
        packages: Vec<PackageSource>,
    ) -> Result<(), String> {
        if scope == SourceScope::Project {
            manager
                .set_project_packages(packages)
                .map_err(|e| e.to_string())
        } else {
            manager.set_packages(packages);
            Ok(())
        }
    }
}

/// `getResourceItemKey` (config-selector.ts:842-844):
/// `${resourceType}:${canonicalizePath(path)}`.
fn get_resource_item_key(item: &ResourceItem) -> String {
    format!(
        "{}:{}",
        item.resource_type.as_str(),
        canonicalize_path(Path::new(&item.path)).to_string_lossy()
    )
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Component for ResourceList {
    fn render(&self, width: usize) -> Vec<String> {
        ResourceList::render(self, width)
    }

    fn handle_input(&mut self, data: &str) {
        ResourceList::handle_input(self, data);
    }

    fn invalidate(&mut self) {}
}

impl Focusable for ResourceList {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.search_input.set_focused(focused);
    }
}

/// `ConfigSelectorComponent` (config-selector.ts:866-941).
pub struct ConfigSelectorComponent {
    top_border: DynamicBorder,
    header: ConfigSelectorHeader,
    resource_list: ResourceList,
    bottom_border: DynamicBorder,
    write_scope: Arc<Mutex<ConfigWriteScope>>,
    focused: bool,
}

impl ConfigSelectorComponent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        resolved_paths: ScopedResolvedPaths,
        settings_manager: Arc<Mutex<SettingsManager>>,
        theme: Arc<Theme>,
        cwd: &str,
        agent_dir: &str,
        terminal_height: Option<usize>,
        write_scope: ConfigWriteScope,
        project_mode_available: bool,
        on_cancel: Option<Box<dyn FnMut() + Send>>,
        on_exit: Option<Box<dyn FnMut() + Send>>,
    ) -> Self {
        let write_scope_cell = Arc::new(Mutex::new(write_scope));
        let border_color = {
            let theme = Arc::clone(&theme);
            Box::new(move |text: &str| theme.fg("border", text))
        };
        let header = ConfigSelectorHeader::new(
            Arc::clone(&theme),
            Arc::clone(&write_scope_cell),
            project_mode_available,
        );
        let mut resource_list = ResourceList::new(
            &resolved_paths,
            settings_manager,
            Arc::clone(&theme),
            Arc::clone(&write_scope_cell),
            Path::new(cwd),
            Path::new(agent_dir),
            terminal_height,
            project_mode_available,
        );
        resource_list.on_cancel = on_cancel;
        resource_list.on_exit = on_exit;
        Self {
            top_border: DynamicBorder::new(border_color.clone()),
            header,
            resource_list,
            bottom_border: DynamicBorder::new(border_color),
            write_scope: write_scope_cell,
            focused: false,
        }
    }

    /// The current write scope (upstream `this.writeScope`).
    pub fn write_scope(&self) -> ConfigWriteScope {
        *lock(&self.write_scope)
    }

    /// `getResourceList` (config-selector.ts:939-941).
    pub fn get_resource_list(&mut self) -> &mut ResourceList {
        &mut self.resource_list
    }
}

impl Component for ConfigSelectorComponent {
    fn render(&self, width: usize) -> Vec<String> {
        // Chrome matches upstream's child order (config-selector.ts:901-930):
        // Spacer / DynamicBorder / Spacer / header / Spacer / list /
        // Spacer / DynamicBorder (T14 review m1 — the four spacers were
        // omitted while the list's `terminalHeight - 8` chrome constant is
        // upstream's; the Rust layout now lines up with upstream's).
        let mut lines = Vec::new();
        lines.push(String::new()); // Spacer
        lines.extend(self.top_border.render(width));
        lines.push(String::new()); // Spacer
        lines.extend(self.header.render(width));
        lines.push(String::new()); // Spacer
        lines.extend(self.resource_list.render(width));
        lines.push(String::new()); // Spacer
        lines.extend(self.bottom_border.render(width));
        lines
    }

    fn handle_input(&mut self, data: &str) {
        self.resource_list.handle_input(data);
    }

    fn invalidate(&mut self) {}
}

impl Focusable for ConfigSelectorComponent {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.resource_list.set_focused(focused);
    }
}

#[cfg(test)]
mod tests {
    //! Port of the config-selector intent: grouping/render, search,
    //! navigation, the global toggle and the project override cycle — with
    //! settings persistence asserted against real settings files.

    use super::*;
    use crate::core::package_manager::{ResolvedPaths, ResolvedResource};
    use crate::core::settings_manager::SettingsManagerCreateOptions;
    use crate::core::themes::load_theme;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDirs {
        root: PathBuf,
        cwd: PathBuf,
        agent_dir: PathBuf,
    }

    impl TestDirs {
        fn new() -> Self {
            let unique = format!(
                "pir-config-selector-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            );
            let root = std::env::temp_dir().join(unique);
            let cwd = root.join("proj");
            let agent_dir = root.join("agent");
            std::fs::create_dir_all(&cwd).unwrap();
            std::fs::create_dir_all(&agent_dir).unwrap();
            TestDirs {
                root,
                cwd,
                agent_dir,
            }
        }
    }

    impl Drop for TestDirs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn theme() -> Arc<Theme> {
        Arc::new(load_theme("dark", None).expect("builtin dark theme"))
    }

    /// Install the global keybindings table (tui.select.*, tui.input.tab, ...).
    fn install_keybindings() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        // Legacy key sequences (the Kitty flag is process-global test state).
        pir_tui::keys::set_kitty_protocol_active(false);
    }

    fn auto_metadata(dirs: &TestDirs, scope: SourceScope) -> ResourcePathMetadata {
        ResourcePathMetadata {
            source: "auto".to_string(),
            scope,
            origin: SourceOrigin::TopLevel,
            base_dir: Some(match scope {
                SourceScope::User => dirs.agent_dir.clone(),
                _ => dirs.cwd.join(CONFIG_DIR_NAME),
            }),
        }
    }

    fn resource(path: PathBuf, enabled: bool, metadata: ResourcePathMetadata) -> ResolvedResource {
        ResolvedResource {
            path,
            enabled,
            metadata,
        }
    }

    /// A global view with one user auto skill + one user auto prompt, and a
    /// project view adding a project auto skill.
    fn sample_paths(dirs: &TestDirs) -> ScopedResolvedPaths {
        let user_skill = dirs.agent_dir.join("skills/format/SKILL.md");
        let user_prompt = dirs.agent_dir.join("prompts/review.md");
        let project_skill = dirs.cwd.join(".pir/skills/deploy/SKILL.md");
        let global = ResolvedPaths {
            skills: vec![resource(
                user_skill.clone(),
                true,
                auto_metadata(dirs, SourceScope::User),
            )],
            prompts: vec![resource(
                user_prompt,
                true,
                auto_metadata(dirs, SourceScope::User),
            )],
            ..ResolvedPaths::default()
        };
        let project = ResolvedPaths {
            skills: vec![
                resource(user_skill, true, auto_metadata(dirs, SourceScope::User)),
                resource(
                    project_skill,
                    true,
                    auto_metadata(dirs, SourceScope::Project),
                ),
            ],
            ..ResolvedPaths::default()
        };
        ScopedResolvedPaths { global, project }
    }

    fn manager(dirs: &TestDirs, project_trusted: bool) -> Arc<Mutex<SettingsManager>> {
        Arc::new(Mutex::new(SettingsManager::create(
            &dirs.cwd,
            Some(&dirs.agent_dir),
            SettingsManagerCreateOptions { project_trusted },
        )))
    }

    fn component(
        dirs: &TestDirs,
        paths: ScopedResolvedPaths,
        settings_manager: Arc<Mutex<SettingsManager>>,
        write_scope: ConfigWriteScope,
        project_mode_available: bool,
    ) -> ConfigSelectorComponent {
        install_keybindings();
        ConfigSelectorComponent::new(
            paths,
            settings_manager,
            theme(),
            &dirs.cwd.to_string_lossy(),
            &dirs.agent_dir.to_string_lossy(),
            Some(24),
            write_scope,
            project_mode_available,
            None,
            None,
        )
    }

    /// Strip ANSI escape sequences (shared with pir-tui tests).
    fn plain(lines: Vec<String>) -> Vec<String> {
        lines
            .into_iter()
            .map(|line| {
                let mut out = String::with_capacity(line.len());
                let mut chars = line.chars().peekable();
                while let Some(c) = chars.next() {
                    if c == '\x1b' && chars.peek() == Some(&'[') {
                        chars.next();
                        for c in chars.by_ref() {
                            if c == 'm' {
                                break;
                            }
                        }
                    } else {
                        out.push(c);
                    }
                }
                out
            })
            .collect()
    }

    fn global_settings(dirs: &TestDirs) -> serde_json::Value {
        let content = std::fs::read_to_string(dirs.agent_dir.join("settings.json")).unwrap();
        serde_json::from_str(&content).unwrap()
    }

    fn project_settings(dirs: &TestDirs) -> serde_json::Value {
        let content = std::fs::read_to_string(dirs.cwd.join(".pir/settings.json")).unwrap();
        serde_json::from_str(&content).unwrap()
    }

    #[test]
    fn renders_grouped_resources_with_global_header() {
        let dirs = TestDirs::new();
        let component = component(
            &dirs,
            sample_paths(&dirs),
            manager(&dirs, true),
            ConfigWriteScope::Global,
            true,
        );
        let lines = plain(component.render(100));
        let text = lines.join("\n");
        assert!(text.contains("Global Resources"), "{text}");
        assert!(text.contains("~/.pir/agent/settings.json"), "{text}");
        assert!(text.contains("format"), "{text}");
        assert!(text.contains("review.md"), "{text}");
    }

    #[test]
    fn global_toggle_writes_disable_then_enable_pattern() {
        let dirs = TestDirs::new();
        let mut component = component(
            &dirs,
            sample_paths(&dirs),
            manager(&dirs, true),
            ConfigWriteScope::Global,
            true,
        );
        // First item is the user skill (skills subgroup sorts first).
        component.handle_input(" ");
        let settings = global_settings(&dirs);
        assert_eq!(
            settings["skills"][0].as_str().unwrap(),
            "-skills/format/SKILL.md"
        );

        component.handle_input(" ");
        let settings = global_settings(&dirs);
        assert_eq!(
            settings["skills"][0].as_str().unwrap(),
            "+skills/format/SKILL.md"
        );
    }

    #[test]
    fn global_package_toggle_converts_entry_to_object_form() {
        let dirs = TestDirs::new();
        let package_root = dirs.agent_dir.join("npm/node_modules/pkg");
        let package_metadata = ResourcePathMetadata {
            source: "npm:pkg".to_string(),
            scope: SourceScope::User,
            origin: SourceOrigin::Package,
            base_dir: Some(package_root.clone()),
        };
        let paths = ScopedResolvedPaths {
            global: ResolvedPaths {
                themes: vec![resource(
                    package_root.join("themes/nord.json"),
                    true,
                    package_metadata,
                )],
                ..ResolvedPaths::default()
            },
            project: ResolvedPaths::default(),
        };
        let settings_manager = manager(&dirs, true);
        lock(&settings_manager).set_packages(vec![PackageSource::Source("npm:pkg".to_string())]);
        let mut component = component(
            &dirs,
            paths,
            settings_manager,
            ConfigWriteScope::Global,
            true,
        );
        component.handle_input(" ");
        let settings = global_settings(&dirs);
        assert_eq!(
            settings["packages"][0]["source"].as_str().unwrap(),
            "npm:pkg"
        );
        assert_eq!(
            settings["packages"][0]["themes"][0].as_str().unwrap(),
            "-themes/nord.json"
        );

        // Toggling back writes the explicit enable pattern (upstream keeps
        // the object form with `+<pattern>`, config-selector.ts:616-620).
        component.handle_input(" ");
        let settings = global_settings(&dirs);
        assert_eq!(
            settings["packages"][0]["themes"][0].as_str().unwrap(),
            "+themes/nord.json"
        );
    }

    #[test]
    fn project_scope_cycles_override_states_into_project_settings() {
        let dirs = TestDirs::new();
        let mut component = component(
            &dirs,
            sample_paths(&dirs),
            manager(&dirs, true),
            ConfigWriteScope::Project,
            true,
        );
        // First project item: the inherited user skill → inherit → unload.
        component.handle_input(" ");
        let settings = project_settings(&dirs);
        let skill_path = dirs.agent_dir.join("skills/format/SKILL.md");
        let skill_path = skill_path.to_string_lossy();
        let entries: Vec<&str> = settings["skills"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            entries,
            vec![skill_path.as_ref(), &format!("-{skill_path}")]
        );

        // unload → load (inherited enabled).
        component.handle_input(" ");
        let settings = project_settings(&dirs);
        let entries: Vec<&str> = settings["skills"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            entries,
            vec![skill_path.as_ref(), &format!("+{skill_path}")]
        );

        // load → inherit: the override entries are removed
        // (config-selector.ts:675-680).
        component.handle_input(" ");
        let settings = project_settings(&dirs);
        let entries: Vec<&str> = settings["skills"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(entries, Vec::<&str>::new());
    }

    #[test]
    fn project_package_override_creates_autoload_false_entry() {
        let dirs = TestDirs::new();
        // A user-scope npm package skill; the project settings have no
        // packages entry for it yet. The global view lists it too (the
        // inherited-enabled map is built from the global groups).
        let package_root = dirs.agent_dir.join("npm/node_modules/pkg");
        let package_metadata = ResourcePathMetadata {
            source: "npm:pkg".to_string(),
            scope: SourceScope::User,
            origin: SourceOrigin::Package,
            base_dir: Some(package_root.clone()),
        };
        let skill_resource = resource(
            package_root.join("skills/tool/SKILL.md"),
            true,
            package_metadata,
        );
        let paths = ScopedResolvedPaths {
            global: ResolvedPaths {
                skills: vec![skill_resource.clone()],
                ..ResolvedPaths::default()
            },
            project: ResolvedPaths {
                skills: vec![skill_resource],
                ..ResolvedPaths::default()
            },
        };
        let mut component = component(
            &dirs,
            paths,
            manager(&dirs, true),
            ConfigWriteScope::Project,
            true,
        );
        component.handle_input(" ");
        let settings = project_settings(&dirs);
        assert_eq!(
            settings["packages"][0]["source"].as_str().unwrap(),
            "npm:pkg"
        );
        assert_eq!(settings["packages"][0]["autoload"], false);
        // Inherited enabled → first cycle state is "unload".
        assert_eq!(
            settings["packages"][0]["skills"][0].as_str().unwrap(),
            "-skills/tool/SKILL.md"
        );

        // Cycle back to inherit: the override entry is removed entirely.
        component.handle_input(" ");
        component.handle_input(" ");
        let settings = project_settings(&dirs);
        assert_eq!(settings["packages"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn tab_switches_write_scope() {
        let dirs = TestDirs::new();
        let mut component = component(
            &dirs,
            sample_paths(&dirs),
            manager(&dirs, true),
            ConfigWriteScope::Global,
            true,
        );
        assert_eq!(component.write_scope(), ConfigWriteScope::Global);
        component.handle_input("\t");
        assert_eq!(component.write_scope(), ConfigWriteScope::Project);
        let lines = plain(component.render(100));
        let text = lines.join("\n");
        assert!(text.contains("Project Local Resources"), "{text}");
        assert!(text.contains("deploy"), "{text}");
        component.handle_input("\t");
        assert_eq!(component.write_scope(), ConfigWriteScope::Global);
    }

    #[test]
    fn escape_and_ctrl_c_cancel() {
        // `tui.select.cancel` binds both escape and ctrl+c
        // (keybindings.ts:130-132), so the explicit ctrl+c → `onExit`
        // branch (config-selector.ts:491-493) is unreachable upstream;
        // the port preserves that quirk.
        use std::sync::Mutex as StdMutex;
        let dirs = TestDirs::new();
        let cancelled = Arc::new(StdMutex::new(0));
        let cancelled_clone = Arc::clone(&cancelled);
        install_keybindings();
        let mut component = ConfigSelectorComponent::new(
            sample_paths(&dirs),
            manager(&dirs, true),
            theme(),
            &dirs.cwd.to_string_lossy(),
            &dirs.agent_dir.to_string_lossy(),
            Some(24),
            ConfigWriteScope::Global,
            true,
            Some(Box::new(move || {
                *cancelled_clone.lock().unwrap_or_else(|e| e.into_inner()) += 1;
            })),
            Some(Box::new(|| {
                panic!("onExit is unreachable (upstream quirk)")
            })),
        );
        component.handle_input("\x1b");
        component.handle_input("\x03");
        assert_eq!(*cancelled.lock().unwrap_or_else(|e| e.into_inner()), 2);
    }

    #[test]
    fn search_filters_items() {
        let dirs = TestDirs::new();
        let mut component = component(
            &dirs,
            sample_paths(&dirs),
            manager(&dirs, true),
            ConfigWriteScope::Global,
            true,
        );
        component.handle_input("r");
        component.handle_input("e");
        let lines = plain(component.render(100));
        let text = lines.join("\n");
        assert!(text.contains("review.md"), "{text}");
        assert!(!text.contains("format"), "{text}");
    }
}
