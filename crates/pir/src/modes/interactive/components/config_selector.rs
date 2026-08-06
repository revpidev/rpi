//! Port of `config-selector.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - Upstream takes `ScopedResolvedPaths` (package-manager output with
//!   per-resource `PathMetadata` + `enabled`) and writes toggles straight
//!   into the `SettingsManager` (pattern-based settings/package arrays). The
//!   local loader's `LoadedResources` carries no per-resource metadata for
//!   prompts/themes/extensions (see resource_loader.rs header) and the
//!   package manager is T14, so the port takes `&LoadedResources` and
//!   exposes write hooks instead: `on_toggle(scope, name, enabled)` and
//!   `on_scope_change(scope)` callbacks; the integration layer (T14 +
//!   interactive-mode) persists the changes. Settings writes are a reported
//!   gap (see README / task report).
//! - `enabled` state: the local loader filters disabled resources out at
//!   discovery, so every listed item starts `enabled: true` and disabling
//!   only flips in-memory state (re-enabling a disabled resource requires
//!   the package-manager path list, T14).
//! - Metadata for prompts/themes/extensions is inferred from the path
//!   location (under `agent_dir` → user, under `<cwd>/.pir` → project);
//!   skills use their real `source_info`. Package-origin items arrive only
//!   through `SourceInfo` (skills), other types are T14.
//! - Project write scope: upstream computes override states (inherit/load/
//!   unload) from the project settings' pattern arrays; the port keeps the
//!   same three-state cycle in an in-memory override map (upstream
//!   `getProjectOverrideState` reads `settingsManager.getProjectSettings()`).
//! - Upstream `requestRender` plumbing (onToggle = () => requestRender()) is
//!   dropped — the component owns its state, so renders are self-consistent.
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
use crate::core::resource_loader::LoadedResources;
use crate::core::skills::{canonicalize_path, SourceOrigin, SourceScope};
use crate::core::themes::Theme;
use crate::modes::interactive::components::dynamic_border::DynamicBorder;
use crate::modes::interactive::components::keybinding_hints::{key_hint, raw_key_hint};

/// `ResourceType` (config-selector.ts:26).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceType {
    Extensions,
    Skills,
    Prompts,
    Themes,
}

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

/// `SettingsScope` (config-selector.ts:28).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsScope {
    User,
    Project,
    Temporary,
}

/// `ResourceOrigin` (config-selector.ts's `PathMetadata.origin` subset).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceOrigin {
    TopLevel,
    Package,
}

/// `ProjectOverrideState` (config-selector.ts:29).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectOverrideState {
    Inherit,
    Load,
    Unload,
}

/// `ResourceItem` (config-selector.ts:41-49).
#[derive(Debug, Clone)]
pub struct ResourceItem {
    pub path: String,
    pub enabled: bool,
    pub resource_type: ResourceType,
    pub display_name: String,
    pub scope: SettingsScope,
    pub origin: ResourceOrigin,
    pub source: String,
    pub base_dir: Option<String>,
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
    scope: SettingsScope,
    origin: ResourceOrigin,
    source: String,
    subgroups: Vec<ResourceSubgroup>,
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
fn get_group_label(
    scope: SettingsScope,
    origin: ResourceOrigin,
    source: &str,
    base_dir: Option<&str>,
    agent_dir: &str,
) -> String {
    match origin {
        ResourceOrigin::Package => {
            let scope_str = match scope {
                SettingsScope::User => "user",
                SettingsScope::Project => "project",
                SettingsScope::Temporary => "temporary",
            };
            format!("{source} ({scope_str})")
        }
        ResourceOrigin::TopLevel if source == "auto" => {
            if let Some(base_dir) = base_dir {
                match scope {
                    SettingsScope::User => format!("User ({})", format_base_dir(base_dir)),
                    _ => format!("Project ({})", format_base_dir(base_dir)),
                }
            } else {
                match scope {
                    SettingsScope::User => format!("User ({})", format_base_dir(agent_dir)),
                    _ => format!("Project ({CONFIG_DIR_NAME}/)"),
                }
            }
        }
        ResourceOrigin::TopLevel => match scope {
            SettingsScope::User => "User settings".to_string(),
            _ => "Project settings".to_string(),
        },
    }
}

/// Infer metadata for resources without `SourceInfo` (prompts, themes,
/// extensions; see module header). Auto-discovered resources carry the
/// scope base dir (package-manager.ts:2312-2318), so the inferred
/// `base_dir` is the agent dir / `<cwd>/.pir`, keeping them in the same
/// group as skills from the same scope.
fn infer_metadata(
    path: &Path,
    cwd: &Path,
    agent_dir: &Path,
) -> (SettingsScope, ResourceOrigin, &'static str, Option<String>) {
    let project_dir = cwd.join(CONFIG_DIR_NAME);
    let (scope, base_dir) = if path.starts_with(agent_dir) {
        (
            SettingsScope::User,
            Some(agent_dir.to_string_lossy().into_owned()),
        )
    } else if path.starts_with(&project_dir) {
        (
            SettingsScope::Project,
            Some(project_dir.to_string_lossy().into_owned()),
        )
    } else {
        // CLI (`--skill` etc.) and other paths: upstream marks them
        // "temporary", which renders like the project branch in
        // `getGroupLabel` / the inherited-global checks.
        (SettingsScope::Project, None)
    };
    (scope, ResourceOrigin::TopLevel, "auto", base_dir)
}

/// `buildGroups` (config-selector.ts:99-180).
fn build_groups(resources: &LoadedResources, cwd: &Path, agent_dir: &Path) -> Vec<ResourceGroup> {
    let mut groups: Vec<ResourceGroup> = Vec::new();
    let mut group_index: HashMap<String, usize> = HashMap::new();

    // Extensions first — upstream `addToGroup` call order
    // (config-selector.ts:153-156).
    for extension in &resources.extensions.paths {
        let (scope, origin, source, base_dir) = infer_metadata(extension, cwd, agent_dir);
        add_to_group(
            &mut groups,
            &mut group_index,
            extension.clone(),
            ResourceType::Extensions,
            scope,
            origin,
            source.to_string(),
            base_dir,
            agent_dir,
        );
    }

    // Skills carry real metadata (`source_info`).
    for skill in &resources.skills {
        let path = skill.file_path.clone();
        let (scope, origin, source, base_dir) = (
            match skill.source_info.scope {
                SourceScope::User => SettingsScope::User,
                SourceScope::Project => SettingsScope::Project,
                SourceScope::Temporary => SettingsScope::Temporary,
            },
            match skill.source_info.origin {
                SourceOrigin::TopLevel => ResourceOrigin::TopLevel,
                SourceOrigin::Package => ResourceOrigin::Package,
            },
            skill.source_info.source.clone(),
            skill
                .source_info
                .base_dir
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
        );
        add_to_group(
            &mut groups,
            &mut group_index,
            path,
            ResourceType::Skills,
            scope,
            origin,
            source,
            base_dir,
            agent_dir,
        );
    }

    for prompt in &resources.prompts {
        let (scope, origin, source, base_dir) = infer_metadata(&prompt.file_path, cwd, agent_dir);
        add_to_group(
            &mut groups,
            &mut group_index,
            prompt.file_path.clone(),
            ResourceType::Prompts,
            scope,
            origin,
            source.to_string(),
            base_dir,
            agent_dir,
        );
    }

    for theme in &resources.themes {
        let path = theme.source_path.clone().unwrap_or_default();
        let (scope, origin, source, base_dir) = infer_metadata(&path, cwd, agent_dir);
        add_to_group(
            &mut groups,
            &mut group_index,
            path,
            ResourceType::Themes,
            scope,
            origin,
            source.to_string(),
            base_dir,
            agent_dir,
        );
    }

    // Sort groups: packages first, then top-level; user before project
    // (config-selector.ts:158-168).
    groups.sort_by(|a, b| {
        let origin_cmp = match (a.origin, b.origin) {
            (ResourceOrigin::Package, ResourceOrigin::TopLevel) => std::cmp::Ordering::Less,
            (ResourceOrigin::TopLevel, ResourceOrigin::Package) => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        };
        if origin_cmp != std::cmp::Ordering::Equal {
            return origin_cmp;
        }
        let scope_cmp = match (a.scope, b.scope) {
            (SettingsScope::User, SettingsScope::User) => std::cmp::Ordering::Equal,
            (SettingsScope::User, _) => std::cmp::Ordering::Less,
            (_, SettingsScope::User) => std::cmp::Ordering::Greater,
            // Upstream comparator: any non-user pair sorts with the left
            // side after the right (`a.scope === "user" ? -1 : 1`), i.e.
            // temporary before project.
            (SettingsScope::Temporary, SettingsScope::Project) => std::cmp::Ordering::Less,
            (SettingsScope::Project, SettingsScope::Temporary) => std::cmp::Ordering::Greater,
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
#[allow(clippy::too_many_arguments)]
fn add_to_group(
    groups: &mut Vec<ResourceGroup>,
    group_index: &mut HashMap<String, usize>,
    path: PathBuf,
    resource_type: ResourceType,
    scope: SettingsScope,
    origin: ResourceOrigin,
    source: String,
    base_dir: Option<String>,
    agent_dir: &Path,
) {
    let group_key = format!(
        "{}:{}:{}:{}",
        match origin {
            ResourceOrigin::TopLevel => "top-level",
            ResourceOrigin::Package => "package",
        },
        match scope {
            SettingsScope::User => "user",
            SettingsScope::Project => "project",
            SettingsScope::Temporary => "temporary",
        },
        source,
        base_dir.clone().unwrap_or_default(),
    );
    let group_idx = *group_index.entry(group_key.clone()).or_insert_with(|| {
        let source = source.clone();
        groups.push(ResourceGroup {
            label: get_group_label(
                scope,
                origin,
                &source,
                base_dir.as_deref(),
                &agent_dir.to_string_lossy(),
            ),
            scope,
            origin,
            source,
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

    let item = ResourceItem {
        path: path.to_string_lossy().into_owned(),
        enabled: true,
        resource_type,
        display_name,
        scope,
        origin,
        source,
        base_dir,
    };
    groups[group_idx].subgroups[subgroup_idx].items.push(item);
}

/// `FlatEntry` (config-selector.ts:182-185): a flattened group → subgroup →
/// item entry. Indices into `ResourceList::groups`.
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

/// `ResourceList` (config-selector.ts:222-864).
pub struct ResourceList {
    groups: Vec<ResourceGroup>,
    flat_items: Vec<FlatEntry>,
    filtered_items: Vec<FlatEntry>,
    selected_index: usize,
    search_input: Input,
    max_visible: usize,
    write_scope: Arc<Mutex<ConfigWriteScope>>,
    /// `inheritedEnabledByKey` (config-selector.ts:233) — built from the
    /// (global) groups.
    inherited_enabled_by_key: HashMap<String, bool>,
    /// Project override states (see module header).
    overrides: HashMap<String, ProjectOverrideState>,
    theme: Arc<Theme>,
    project_mode_available: bool,
    focused: bool,

    /// `onCancel` (config-selector.ts:235).
    pub on_cancel: Option<Box<dyn FnMut() + Send>>,
    /// `onExit` (config-selector.ts:236).
    pub on_exit: Option<Box<dyn FnMut() + Send>>,
    /// `onToggle` (config-selector.ts:237) — local form:
    /// `(writeScope, displayName, enabled)`.
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    pub on_toggle: Option<Box<dyn FnMut(&str, &str, bool) + Send>>,
    /// `onSwitchMode` (config-selector.ts:239) — local form: fired with the
    /// *new* write scope after a successful Tab switch.
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    pub on_scope_change: Option<Box<dyn FnMut(&str) + Send>>,
}

impl ResourceList {
    fn new(
        resources: &LoadedResources,
        theme: Arc<Theme>,
        write_scope: Arc<Mutex<ConfigWriteScope>>,
        cwd: &str,
        agent_dir: &str,
        terminal_height: Option<usize>,
        project_mode_available: bool,
    ) -> Self {
        let groups = build_groups(resources, Path::new(cwd), Path::new(agent_dir));
        let inherited_enabled_by_key = Self::build_inherited_enabled_map(&groups);
        let mut list = Self {
            groups,
            flat_items: Vec::new(),
            filtered_items: Vec::new(),
            selected_index: 0,
            search_input: Input::new(),
            // 8 lines of chrome: top spacer + top border + spacer + header
            // (2 lines) + spacer + bottom spacer + bottom border
            // (config-selector.ts:264-266).
            max_visible: 5usize.max(terminal_height.unwrap_or(24).saturating_sub(8)),
            write_scope,
            inherited_enabled_by_key,
            overrides: HashMap::new(),
            theme,
            project_mode_available,
            focused: false,
            on_cancel: None,
            on_exit: None,
            on_toggle: None,
            on_scope_change: None,
        };
        list.build_flat_list();
        list.filtered_items = list.flat_items.clone();
        list
    }

    fn scope(&self) -> ConfigWriteScope {
        *lock(&self.write_scope)
    }

    /// `switchWriteScope` (config-selector.ts:933-937) + the component's
    /// `onSwitchMode` hook (config-selector.ts:921-925).
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
        let new_scope = self.scope().as_str().to_string();
        if let Some(on_scope_change) = self.on_scope_change.as_mut() {
            on_scope_change(&new_scope);
        }
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
        for (group_idx, group) in self.groups.iter().enumerate() {
            self.flat_items.push(FlatEntry::Group { group: group_idx });
            for (subgroup_idx, subgroup) in group.subgroups.iter().enumerate() {
                self.flat_items.push(FlatEntry::Subgroup {
                    group: group_idx,
                    subgroup: subgroup_idx,
                });
                for item_idx in 0..subgroup.items.len() {
                    self.flat_items.push(FlatEntry::Item {
                        group: group_idx,
                        subgroup: subgroup_idx,
                        item: item_idx,
                    });
                }
            }
        }
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
                let resource = &self.groups[*group].subgroups[*subgroup].items[*item];
                if resource.display_name.to_lowercase().contains(&lower_query)
                    || resource.resource_type.as_str().contains(&lower_query)
                    || resource.path.to_lowercase().contains(&lower_query)
                {
                    matching_items.insert((*group, *subgroup, *item));
                }
            }
        }

        // Find which subgroups and groups contain matching items.
        for (group_idx, group) in self.groups.iter().enumerate() {
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

    /// `updateItem` (config-selector.ts:376-388): flip `enabled` in the
    /// groups (and thus every flat/filtered view).
    fn update_item(&mut self, path: &str, resource_type: ResourceType, enabled: bool) {
        for group in &mut self.groups {
            for subgroup in &mut group.subgroups {
                for item in &mut subgroup.items {
                    if item.path == path && item.resource_type == resource_type {
                        item.enabled = enabled;
                        return;
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
                        && self.groups[group].scope == SettingsScope::User;
                    let label = Theme::bold(&format!(
                        "{}{}",
                        self.groups[group].label,
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
                        && self.groups[group].scope == SettingsScope::User
                    {
                        "dim"
                    } else {
                        "muted"
                    };
                    let subgroup_line = self
                        .theme
                        .fg(color, &self.groups[group].subgroups[subgroup].label);
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
                    let item = &self.groups[group].subgroups[subgroup].items[item];
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
                let resource = &self.groups[group].subgroups[subgroup].items[item];
                // config-selector.ts:501: project scope toggles anything,
                // global scope only user-scope items (`getItemScope`
                // maps temporary → user too, config-selector.ts:846-848).
                if self.scope() == ConfigWriteScope::Project
                    || resource.scope != SettingsScope::Project
                {
                    if let Some(enabled) = self.toggle_resource(group, subgroup, item) {
                        let name = self.groups[group].subgroups[subgroup].items[item]
                            .display_name
                            .clone();
                        let scope_str = self.scope().as_str();
                        if let Some(on_toggle) = self.on_toggle.as_mut() {
                            on_toggle(scope_str, &name, enabled);
                        }
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

    /// `toggleResource` (config-selector.ts:516-530). Global scope flips the
    /// in-memory `enabled` (upstream additionally writes settings arrays —
    /// deferred to the integration layer via `on_toggle`); project scope
    /// cycles the override state.
    fn toggle_resource(&mut self, group: usize, subgroup: usize, item: usize) -> Option<bool> {
        if self.scope() == ConfigWriteScope::Project {
            let state =
                self.get_next_override_state(&self.groups[group].subgroups[subgroup].items[item]);
            let enabled = match state {
                ProjectOverrideState::Inherit => {
                    self.get_inherited_enabled(&self.groups[group].subgroups[subgroup].items[item])
                }
                ProjectOverrideState::Load => true,
                ProjectOverrideState::Unload => false,
            };
            let key = get_resource_item_key(&self.groups[group].subgroups[subgroup].items[item]);
            self.overrides.insert(key, state);
            Some(enabled)
        } else {
            let item_ref = &self.groups[group].subgroups[subgroup].items[item];
            let enabled = !item_ref.enabled;
            let path = item_ref.path.clone();
            let resource_type = item_ref.resource_type;
            self.update_item(&path, resource_type, enabled);
            Some(enabled)
        }
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

    /// `getProjectOverrideState` (config-selector.ts:739-757) — local form:
    /// reads the in-memory override map instead of the project settings
    /// pattern arrays (see module header).
    fn get_project_override_state(&self, item: &ResourceItem) -> ProjectOverrideState {
        if self.scope() != ConfigWriteScope::Project {
            return ProjectOverrideState::Inherit;
        }
        self.overrides
            .get(&get_resource_item_key(item))
            .copied()
            .unwrap_or(ProjectOverrideState::Inherit)
    }

    /// `getInheritedEnabled` (config-selector.ts:774-779).
    fn get_inherited_enabled(&self, item: &ResourceItem) -> bool {
        self.inherited_enabled_by_key
            .get(&get_resource_item_key(item))
            .copied()
            .unwrap_or_else(|| {
                if item.scope == SettingsScope::User {
                    item.enabled
                } else {
                    true
                }
            })
    }

    /// `isInheritedGlobalItem` (config-selector.ts:781-783).
    fn is_inherited_global_item(&self, item: &ResourceItem) -> bool {
        item.scope == SettingsScope::User
            || self
                .inherited_enabled_by_key
                .contains_key(&get_resource_item_key(item))
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
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    pub fn new(
        resources: &LoadedResources,
        theme: Arc<Theme>,
        cwd: &str,
        agent_dir: &str,
        terminal_height: Option<usize>,
        write_scope: ConfigWriteScope,
        project_mode_available: bool,
        #[allow(clippy::type_complexity)] // mirrors the upstream callback type
        on_toggle: Option<Box<dyn FnMut(&str, &str, bool) + Send>>,
        on_scope_change: Option<Box<dyn FnMut(&str) + Send>>,
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
            resources,
            Arc::clone(&theme),
            Arc::clone(&write_scope_cell),
            cwd,
            agent_dir,
            terminal_height,
            project_mode_available,
        );
        resource_list.on_toggle = on_toggle;
        resource_list.on_scope_change = on_scope_change;
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
        let mut lines = Vec::new();
        // Container children (config-selector.ts:901-930): Spacer(1),
        // DynamicBorder, Spacer(1), header, Spacer(1), resource list,
        // Spacer(1), DynamicBorder.
        lines.push(String::new());
        lines.extend(self.top_border.render(width));
        lines.push(String::new());
        lines.extend(self.header.render(width));
        lines.push(String::new());
        lines.extend(self.resource_list.render(width));
        lines.push(String::new());
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
    use super::*;
    use crate::core::prompt_templates::PromptTemplate;
    use crate::core::skills::{Skill, SourceInfo};
    use crate::core::themes::load_theme;

    fn theme() -> Arc<Theme> {
        Arc::new(load_theme("dark", None).expect("builtin dark theme"))
    }

    /// Install the global 73-entry keybindings table (tui.select.*,
    /// tui.input.tab, ...).
    fn install_keybindings() {
        crate::modes::interactive::interactive_mode::install_global_keybindings();
    }

    fn skill(path: &str, source: &str, scope: SourceScope) -> Skill {
        // Auto-discovered skills carry `base_dir` = the discovery root
        // (the agent dir / project `.pir` dir, skills.rs:1096-1101).
        let base_dir = if scope == SourceScope::User {
            "/home/tester/.pir/agent"
        } else {
            "/home/tester/proj/.pir"
        };
        Skill {
            name: Path::new(path)
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            description: String::new(),
            file_path: PathBuf::from(path),
            base_dir: PathBuf::from(base_dir),
            source_info: SourceInfo {
                path: PathBuf::from(path),
                source: source.to_string(),
                scope,
                origin: SourceOrigin::TopLevel,
                base_dir: Some(PathBuf::from(base_dir)),
            },
            disable_model_invocation: false,
        }
    }

    #[allow(clippy::field_reassign_with_default)] // test fixture builder
    fn sample_resources() -> LoadedResources {
        let mut resources = LoadedResources::default();
        resources.skills = vec![
            skill(
                "/home/tester/.pir/agent/skills/format/SKILL.md",
                "auto",
                SourceScope::User,
            ),
            skill(
                "/home/tester/proj/.pir/skills/deploy/SKILL.md",
                "auto",
                SourceScope::Project,
            ),
        ];
        resources.prompts = vec![PromptTemplate {
            name: "review".to_string(),
            description: "code review".to_string(),
            argument_hint: None,
            content: String::new(),
            file_path: PathBuf::from("/home/tester/.pir/agent/prompts/review.md"),
        }];
        let mut nord = crate::core::themes::load_theme("dark", None).expect("builtin dark theme");
        nord.name = Some("nord".to_string());
        nord.source_path = Some(PathBuf::from("/home/tester/.pir/agent/themes/nord.json"));
        resources.themes = vec![nord];
        resources.extensions.paths = vec![
            PathBuf::from("/home/tester/.pir/agent/extensions/my-ext"),
            PathBuf::from("/home/tester/proj/.pir/extensions/local-helper"),
        ];
        resources
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

    #[test]
    fn renders_grouped_resources_with_global_header() {
        install_keybindings();
        let component = ConfigSelectorComponent::new(
            &sample_resources(),
            theme(),
            "/home/tester/proj",
            "/home/tester/.pir/agent",
            Some(24),
            ConfigWriteScope::Global,
            true,
            None,
            None,
            None,
            None,
        );
        let lines = plain(component.render(80));
        // Spacer + border + spacer + header (2 lines).
        assert!(lines[3].starts_with("Global Resources"));
        assert!(lines[3].contains("tab"));
        assert!(lines[3].contains("space"));
        assert!(lines[4].contains("~/.pir/agent/settings.json"));
        // User group (accent) appears before project group.
        let joined = lines.join("\n");
        assert!(joined.contains("User ("));
        assert!(joined.contains("Project ("));
        assert!(joined.contains("Extensions"));
        assert!(joined.contains("Skills"));
        assert!(joined.contains("Prompts"));
        assert!(joined.contains("Themes"));
        // Skills display by parent folder, extensions keep folder prefix.
        assert!(joined.contains("format"));
        assert!(joined.contains("deploy"));
        assert!(joined.contains("my-ext"));
        assert!(joined.contains("local-helper"));
        assert!(joined.contains("review.md"));
        assert!(joined.contains("nord.json"));
        // Selection starts on the first item.
        assert!(joined.contains("> "));
    }

    #[test]
    fn toggles_item_in_global_scope_and_fires_on_toggle() {
        install_keybindings();
        let fired = Arc::new(Mutex::new(Vec::<(String, String, bool)>::new()));
        #[allow(clippy::type_complexity)] // mirrors the upstream callback type
        let on_toggle: Option<Box<dyn FnMut(&str, &str, bool) + Send>> = Some(Box::new({
            let fired = Arc::clone(&fired);
            move |scope, name, enabled| {
                fired
                    .lock()
                    .unwrap()
                    .push((scope.to_string(), name.to_string(), enabled));
            }
        }));
        let mut component = ConfigSelectorComponent::new(
            &sample_resources(),
            theme(),
            "/home/tester/proj",
            "/home/tester/.pir/agent",
            Some(24),
            ConfigWriteScope::Global,
            true,
            on_toggle,
            None,
            None,
            None,
        );
        // First item = first user-scope item (my-ext: the extensions
        // subgroup sorts before skills).
        let before = plain(component.render(80));
        assert!(before
            .iter()
            .any(|l| l.contains("> ") && l.contains("my-ext")));

        // Enter confirms and toggles off.
        component.handle_input("\r");
        let after = plain(component.render(80));
        assert!(after
            .iter()
            .any(|l| l.contains("my-ext") && l.contains("[ ]")));
        let fired = fired.lock().unwrap();
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].0, "global");
        assert_eq!(fired[0].1, "my-ext");
        assert!(!fired[0].2);
    }

    #[test]
    fn moves_selection_and_clamps_at_ends() {
        install_keybindings();
        let mut component = ConfigSelectorComponent::new(
            &sample_resources(),
            theme(),
            "/home/tester/proj",
            "/home/tester/.pir/agent",
            Some(24),
            ConfigWriteScope::Global,
            true,
            None,
            None,
            None,
            None,
        );
        // Item order: my-ext, format, review.md, nord.json, local-helper,
        // deploy. Selection starts on the first item (my-ext); down moves
        // item-to-item (headers skipped).
        component.handle_input("\x1b[B");
        let lines = plain(component.render(80));
        assert!(lines
            .iter()
            .any(|l| l.contains("> ") && l.contains("format")));
        // Five more downs reach the last item (deploy).
        for _ in 0..5 {
            component.handle_input("\x1b[B");
        }
        let lines = plain(component.render(80));
        assert!(lines
            .iter()
            .any(|l| l.contains("> ") && l.contains("deploy")));
        // Upstream findNextItem does NOT wrap (config-selector.ts:309-318):
        // down at the last item stays, up moves back.
        component.handle_input("\x1b[B");
        let lines = plain(component.render(80));
        assert!(lines
            .iter()
            .any(|l| l.contains("> ") && l.contains("deploy")));
        component.handle_input("\x1b[A");
        let lines = plain(component.render(80));
        assert!(lines
            .iter()
            .any(|l| l.contains("> ") && l.contains("local-helper")));
        // Up from the first item stays put.
        for _ in 0..5 {
            component.handle_input("\x1b[A");
        }
        let lines = plain(component.render(80));
        assert!(lines
            .iter()
            .any(|l| l.contains("> ") && l.contains("my-ext")));
    }

    #[test]
    fn page_keys_jump_by_window() {
        install_keybindings();
        let mut component = ConfigSelectorComponent::new(
            &sample_resources(),
            theme(),
            "/home/tester/proj",
            "/home/tester/.pir/agent",
            Some(24),
            ConfigWriteScope::Global,
            true,
            None,
            None,
            None,
            None,
        );
        component.handle_input("\x1b[6~"); // pageDown
        let lines = plain(component.render(80));
        // Jumps maxVisible (16) rows down, clamped to the last item.
        assert!(lines
            .iter()
            .any(|l| l.contains("> ") && l.contains("deploy")));
        component.handle_input("\x1b[5~"); // pageUp
        let lines = plain(component.render(80));
        // Jumps back up by the window, landing on the first item.
        assert!(lines
            .iter()
            .any(|l| l.contains("> ") && l.contains("my-ext")));
    }

    #[test]
    fn tab_switches_write_scope_and_fires_on_scope_change() {
        install_keybindings();
        let fired = Arc::new(Mutex::new(Vec::<String>::new()));
        #[allow(clippy::type_complexity)] // mirrors the upstream callback type
        let on_scope_change: Option<Box<dyn FnMut(&str) + Send>> = Some(Box::new({
            let fired = Arc::clone(&fired);
            move |scope| {
                fired.lock().unwrap().push(scope.to_string());
            }
        }));
        let mut component = ConfigSelectorComponent::new(
            &sample_resources(),
            theme(),
            "/home/tester/proj",
            "/home/tester/.pir/agent",
            Some(24),
            ConfigWriteScope::Global,
            true,
            None,
            on_scope_change,
            None,
            None,
        );
        component.handle_input("\t");
        let lines = plain(component.render(80));
        assert!(lines.iter().any(|l| l.contains("Project Local Resources")));
        assert!(lines.iter().any(|l| l.contains(".pir/settings.json")));
        assert_eq!(*fired.lock().unwrap(), vec!["project".to_string()]);
        // User groups are dimmed and marked inherited in project scope.
        let joined = lines.join("\n");
        assert!(joined.contains("inherited global"));
        assert!(joined.contains("project load") || joined.contains("cycle"));

        // Back to global.
        component.handle_input("\t");
        let lines = plain(component.render(80));
        assert!(lines.iter().any(|l| l.contains("Global Resources")));
        assert_eq!(
            *fired.lock().unwrap(),
            vec!["project".to_string(), "global".to_string()]
        );
    }

    #[test]
    fn project_scope_cycles_override_states() {
        install_keybindings();
        let fired = Arc::new(Mutex::new(Vec::<(String, String, bool)>::new()));
        #[allow(clippy::type_complexity)] // mirrors the upstream callback type
        let on_toggle: Option<Box<dyn FnMut(&str, &str, bool) + Send>> = Some(Box::new({
            let fired = Arc::clone(&fired);
            move |scope, name, enabled| {
                fired
                    .lock()
                    .unwrap()
                    .push((scope.to_string(), name.to_string(), enabled));
            }
        }));
        let mut component = ConfigSelectorComponent::new(
            &sample_resources(),
            theme(),
            "/home/tester/proj",
            "/home/tester/.pir/agent",
            Some(24),
            ConfigWriteScope::Global,
            true,
            on_toggle,
            None,
            None,
            None,
        );
        component.handle_input("\t"); // → project scope

        // First item is a user-scope item ("my-ext", inherited enabled).
        // Cycle: inherit → unload → load → inherit.
        component.handle_input(" ");
        let lines = plain(component.render(80));
        assert!(lines
            .iter()
            .any(|l| l.contains("[-]") && l.contains("my-ext") && l.contains("project unload")));
        component.handle_input(" ");
        let lines = plain(component.render(80));
        assert!(lines
            .iter()
            .any(|l| l.contains("[+]") && l.contains("my-ext") && l.contains("project load")));
        component.handle_input(" ");
        let lines = plain(component.render(80));
        assert!(lines
            .iter()
            .any(|l| l.contains("my-ext") && l.contains("inherited global")));

        let fired = fired.lock().unwrap();
        assert_eq!(fired.len(), 3);
        assert_eq!(
            fired[0],
            ("project".to_string(), "my-ext".to_string(), false)
        );
        assert_eq!(
            fired[1],
            ("project".to_string(), "my-ext".to_string(), true)
        );
        assert_eq!(
            fired[2],
            ("project".to_string(), "my-ext".to_string(), true)
        );
    }

    #[test]
    fn escape_cancels_and_ctrl_c_exits() {
        install_keybindings();
        let cancelled = Arc::new(Mutex::new(0usize));
        let exited = Arc::new(Mutex::new(0usize));
        let on_cancel: Option<Box<dyn FnMut() + Send>> = Some(Box::new({
            let cancelled = Arc::clone(&cancelled);
            move || {
                *cancelled.lock().unwrap() += 1;
            }
        }));
        let on_exit: Option<Box<dyn FnMut() + Send>> = Some(Box::new({
            let exited = Arc::clone(&exited);
            move || {
                *exited.lock().unwrap() += 1;
            }
        }));
        let mut component = ConfigSelectorComponent::new(
            &sample_resources(),
            theme(),
            "/home/tester/proj",
            "/home/tester/.pir/agent",
            Some(24),
            ConfigWriteScope::Global,
            true,
            None,
            None,
            on_cancel,
            on_exit,
        );
        // Escape matches tui.select.cancel → onCancel (upstream
        // config-selector.ts:487-490 cancels even with search text).
        component.handle_input("\x1b");
        assert_eq!(*cancelled.lock().unwrap(), 1);
        assert_eq!(*exited.lock().unwrap(), 0);
        // Ctrl+C also matches tui.select.cancel by default.
        component.handle_input("\x03");
        assert_eq!(*cancelled.lock().unwrap(), 2);
    }

    #[test]
    fn search_filters_items() {
        install_keybindings();
        let mut component = ConfigSelectorComponent::new(
            &sample_resources(),
            theme(),
            "/home/tester/proj",
            "/home/tester/.pir/agent",
            Some(24),
            ConfigWriteScope::Global,
            true,
            None,
            None,
            None,
            None,
        );
        component.handle_input("d");
        component.handle_input("e");
        let lines = plain(component.render(80));
        let joined = lines.join("\n");
        assert!(joined.contains("deploy"));
        assert!(!joined.contains("format"));
        // Clearing the search restores everything.
        component.handle_input("\x7f");
        component.handle_input("\x7f");
        let lines = plain(component.render(80));
        assert!(lines.join("\n").contains("format"));
    }
}
