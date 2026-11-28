//! Generational install layout for extension containers (ADR-0050, V19-09).
//!
//! rpi-specific; no upstream counterpart. Upstream Pi installs packages into
//! `<root>/<name>/` and replaces the directory in place, which Windows
//! forbids while a native plugin's library is loaded. ADR-0050 keeps that
//! flat layout on Unix and adds a generational one that is only *written* on
//! Windows:
//!
//! ```text
//! <extensions_root>/<name>/current.json   pointer to the live generation
//! <extensions_root>/<name>/<version>/...   generation directories (never
//!                                          rewritten once placed)
//! <extensions_root>/<name>/.retired        written by `rpi remove`
//! ```
//!
//! The *read* side is platform independent and lives here so that both the
//! host loader ([`crate::loader`]) and the package manager share one
//! resolver (design §11.8). A container without a pointer is the legacy flat
//! layout and resolves to itself; a container with a `.retired` marker
//! resolves to nothing (it is uninstalled and must not fall back to flat
//! files that are still queued for delayed removal).
//!
//! Resolution is fixed per process: once a container has resolved to a
//! directory, that answer is cached for the rest of the process lifetime,
//! so `/reload` keeps loading the generation the instance started with even
//! after another process flipped the pointer. The cache is compiled on every
//! platform (so the semantics are unit-tested on Linux); on Unix no pointer
//! or retirement marker is ever written, so the cached answer is always the
//! unchanged flat one.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Pointer file at the container root naming the live generation.
pub const GENERATION_POINTER_FILE: &str = "current.json";
/// Marker written by `rpi remove`; a container carrying it is uninstalled.
pub const RETIRED_MARKER_FILE: &str = ".retired";
/// The only pointer schema this build reads.
pub const POINTER_SCHEMA_VERSION: u32 = 1;

/// `<container>/current.json` (design §11.8). Unknown fields are ignored so
/// later additive revisions still load.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationPointer {
    pub schema_version: u32,
    /// Version of the generation the pointer names.
    pub version: String,
    /// Generation directory, relative to the container: one path component.
    pub path: String,
    /// sha256 of the generation's `rpi-extension.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_sha256: Option<String>,
    /// `rpiAbi` of the generation's manifest.
    #[serde(default)]
    pub rpi_abi: u32,
    /// Version the pointer named before the flip; `None` when the install
    /// used the flat layout before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
}

impl GenerationPointer {
    /// A pointer for `version` in the generation directory `path`.
    pub fn new(version: &str, path: &str) -> Self {
        GenerationPointer {
            schema_version: POINTER_SCHEMA_VERSION,
            version: version.to_owned(),
            path: path.to_owned(),
            manifest_sha256: None,
            rpi_abi: 0,
            previous: None,
        }
    }
}

/// A generation directory name is a single normal path component: version
/// strings never contain separators, but the pointer is data on disk, so it
/// is validated rather than trusted (a pointer must not escape its
/// container).
pub fn is_safe_generation_path(path: &str) -> bool {
    let mut components = Path::new(path).components();
    match components.next() {
        Some(Component::Normal(_)) => {}
        _ => return false,
    }
    components.next().is_none()
}

/// Whether `name` (a directory entry under an extensions root or a
/// container) denotes a generation directory rather than container
/// metadata or installer state. `current.json` and `.retired` are files, so
/// callers apply this to directories only.
pub fn is_reserved_container_entry(name: &str) -> bool {
    name == GENERATION_POINTER_FILE
        || name == RETIRED_MARKER_FILE
        || name == ".staging"
        || name == ".pins"
        || name.starts_with(".rpi-")
}

/// The pointer of `container`, or `None` when absent, unreadable or not a
/// pointer this build understands.
pub fn read_generation_pointer(container: &Path) -> Option<GenerationPointer> {
    let text = std::fs::read_to_string(container.join(GENERATION_POINTER_FILE)).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let pointer: GenerationPointer = serde_json::from_str(text).ok()?;
    (pointer.schema_version == POINTER_SCHEMA_VERSION && is_safe_generation_path(&pointer.path))
        .then_some(pointer)
}

/// Whether `container` was retired by `rpi remove`.
pub fn is_retired(container: &Path) -> bool {
    container.join(RETIRED_MARKER_FILE).exists()
}

/// Whether `name` is the aside name the Windows cleanup scan gives a
/// leftover directory before deleting it: `<name>.garbage-<pid>` or
/// `<name>.garbage-<pid>.t<stamp>`, with a non-empty `<name>`. Such a
/// directory is on its way out — a half-deleted tree may still carry a
/// pointer and a manifest — and is never resolved as an install.
pub fn is_garbage_aside_name(name: &str) -> bool {
    let Some((head, tail)) = name.rsplit_once(".garbage-") else {
        return false;
    };
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    !head.is_empty()
        && (digits(tail)
            || matches!(tail.rsplit_once(".t"), Some((pid, stamp)) if digits(pid) && digits(stamp)))
}

/// Resolve `container` without consulting the process cache. `None` means
/// retired (uninstalled); otherwise the directory that holds the extension
/// files: the generation the pointer names, or the container itself.
///
/// A missing or corrupt pointer falls back to the flat layout. So does a
/// pointer whose generation directory has no manifest (a half-removed
/// generation): loading nothing would be worse than loading the files that
/// are still there.
pub fn resolve_install_dir_uncached(container: &Path) -> Option<PathBuf> {
    if is_retired(container)
        || container
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_garbage_aside_name)
    {
        return None;
    }
    let Some(pointer) = read_generation_pointer(container) else {
        return Some(container.to_path_buf());
    };
    let generation = container.join(&pointer.path);
    if generation.join("rpi-extension.json").is_file() {
        Some(generation)
    } else {
        Some(container.to_path_buf())
    }
}

/// Process-lifetime record of the generation each container resolved to.
/// Entries are never invalidated: an instance keeps the generation it
/// started with until it exits (design §11.8 "process-fixed").
#[derive(Default)]
pub struct FixedGenerations {
    entries: Mutex<HashMap<PathBuf, PathBuf>>,
}

impl FixedGenerations {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached resolution for `container`, computing and caching it on
    /// the first call. Retired containers are not cached: a later install
    /// of the same name clears the marker, and until then every lookup
    /// keeps answering "not installed".
    pub fn resolve(&self, container: &Path) -> Option<PathBuf> {
        let key = cache_key(container);
        if let Some(cached) = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
        {
            return Some(cached.clone());
        }
        let resolved = resolve_install_dir_uncached(container)?;
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(key, resolved.clone());
        Some(resolved)
    }

    /// Drop the cached answer for `container` and resolve it again. The
    /// generational write path calls this after it flipped a pointer, so a
    /// process that resolved the container while planning an install reports
    /// the generation it just placed (not the one it started with).
    pub fn refresh(&self, container: &Path) -> Option<PathBuf> {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&cache_key(container));
        self.resolve(container)
    }

    /// Number of cached containers (diagnostics and tests).
    pub fn cached_count(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }
}

/// Cache identity: Windows paths are case-insensitive, so the key is
/// lower-cased there; elsewhere the path itself is the key.
fn cache_key(container: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let lowered: String = container.to_string_lossy().to_lowercase();
        PathBuf::from(lowered)
    }
    #[cfg(not(windows))]
    {
        container.to_path_buf()
    }
}

fn process_fixed() -> &'static FixedGenerations {
    static FIXED: OnceLock<FixedGenerations> = OnceLock::new();
    FIXED.get_or_init(FixedGenerations::new)
}

/// Resolve `container` the way every reader does, through the process cache
/// (see [`FixedGenerations`]). On Unix no pointer is ever written, so the
/// cached answer is the flat container itself, exactly as before the
/// generational layout existed.
pub fn resolve_install_dir(container: &Path) -> Option<PathBuf> {
    process_fixed().resolve(container)
}

/// Resolve `container` through the process cache and return the directory
/// that will be loaded. The Windows startup hook calls it for every
/// container under the install roots before any extension loads, so the pin
/// it registers names exactly the directories in use (design §11.8).
pub fn pin_install_resolution(container: &Path) -> Option<PathBuf> {
    process_fixed().resolve(container)
}

/// Re-resolve `container` after its pointer changed (see
/// [`FixedGenerations::refresh`]).
pub fn refresh_install_resolution(container: &Path) -> Option<PathBuf> {
    process_fixed().refresh(container)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let unique = format!(
                "rpi-install-layout-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            );
            let root = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&root).unwrap();
            TempDir(root)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn manifest(dir: &Path, name: &str, version: &str, native: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("rpi-extension.json"),
            format!(r#"{{"name":"{name}","version":"{version}","native":"{native}","rpiAbi":1}}"#),
        )
        .unwrap();
        std::fs::write(dir.join(native), b"lib").unwrap();
    }

    fn pointer(container: &Path, version: &str, path: &str) {
        std::fs::create_dir_all(container).unwrap();
        std::fs::write(
            container.join(GENERATION_POINTER_FILE),
            serde_json::to_string_pretty(&GenerationPointer::new(version, path)).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn flat_container_resolves_to_itself() {
        let temp = TempDir::new();
        let container = temp.0.join("rpi-todo");
        manifest(&container, "rpi-todo", "0.1.0", "libtodo.so");
        assert_eq!(
            resolve_install_dir_uncached(&container),
            Some(container.clone())
        );
        assert!(!is_retired(&container));
    }

    #[test]
    fn pointer_selects_the_generation_directory() {
        let temp = TempDir::new();
        let container = temp.0.join("rpi-todo");
        // Legacy flat files plus a migrated generation: the pointer wins.
        manifest(&container, "rpi-todo", "0.1.0", "libtodo.so");
        let generation = container.join("0.2.0");
        manifest(&generation, "rpi-todo", "0.2.0", "libtodo.so");
        pointer(&container, "0.2.0", "0.2.0");
        assert_eq!(
            resolve_install_dir_uncached(&container),
            Some(generation.clone())
        );
        assert_eq!(
            read_generation_pointer(&container).map(|pointer| pointer.previous),
            Some(None)
        );
    }

    #[test]
    fn missing_or_corrupt_pointer_falls_back_to_flat() {
        let temp = TempDir::new();
        let container = temp.0.join("rpi-todo");
        manifest(&container, "rpi-todo", "0.1.0", "libtodo.so");
        std::fs::write(container.join(GENERATION_POINTER_FILE), b"{not json").unwrap();
        assert_eq!(
            resolve_install_dir_uncached(&container),
            Some(container.clone()),
            "a corrupt pointer must not hide the flat install"
        );
        // Unknown schema revision.
        std::fs::write(
            container.join(GENERATION_POINTER_FILE),
            r#"{"schemaVersion":2,"version":"9.0.0","path":"9.0.0"}"#,
        )
        .unwrap();
        assert_eq!(
            resolve_install_dir_uncached(&container),
            Some(container.clone())
        );
        // Missing path field.
        std::fs::write(
            container.join(GENERATION_POINTER_FILE),
            r#"{"schemaVersion":1,"version":"9.0.0"}"#,
        )
        .unwrap();
        assert_eq!(
            resolve_install_dir_uncached(&container),
            Some(container.clone())
        );
    }

    #[test]
    fn unsafe_pointer_path_is_ignored() {
        let temp = TempDir::new();
        let container = temp.0.join("rpi-todo");
        manifest(&container, "rpi-todo", "0.1.0", "libtodo.so");
        for unsafe_path in ["../other", "/abs", "a/b", "..", ".", ""] {
            std::fs::write(
                container.join(GENERATION_POINTER_FILE),
                format!(r#"{{"schemaVersion":1,"version":"9.0.0","path":"{unsafe_path}"}}"#),
            )
            .unwrap();
            assert_eq!(
                resolve_install_dir_uncached(&container),
                Some(container.clone()),
                "path {unsafe_path:?} must not escape the container"
            );
            assert!(!is_safe_generation_path(unsafe_path), "{unsafe_path:?}");
        }
        assert!(is_safe_generation_path("0.1.9-rc.3"));
    }

    #[test]
    fn pointer_without_generation_manifest_falls_back_to_flat() {
        let temp = TempDir::new();
        let container = temp.0.join("rpi-todo");
        manifest(&container, "rpi-todo", "0.1.0", "libtodo.so");
        pointer(&container, "0.2.0", "0.2.0");
        assert_eq!(
            resolve_install_dir_uncached(&container),
            Some(container.clone())
        );
    }

    #[test]
    fn an_aside_directory_resolves_to_nothing_whatever_it_still_contains() {
        // A half-deleted retired container that lost its marker but kept
        // its pointer and generation is never loaded while it waits for
        // its deletion.
        for name in [
            "rpi-todo.retired-77.garbage-78",
            "rpi-todo.garbage-78.t1730000000000",
        ] {
            assert!(is_garbage_aside_name(name), "{name}");
        }
        for name in [
            "rpi-todo",
            "rpi.garbage",
            ".garbage-7",
            "rpi-todo.garbage-",
            "rpi-todo.garbage-x",
        ] {
            assert!(!is_garbage_aside_name(name), "{name}");
        }
        let temp = TempDir::new();
        let dir = &temp.0;
        let container = dir.join("rpi-todo.garbage-78");
        std::fs::create_dir_all(container.join("0.1.0")).unwrap();
        std::fs::write(container.join("0.1.0/rpi-extension.json"), "{}").unwrap();
        std::fs::write(
            container.join("current.json"),
            r#"{"schemaVersion":1,"version":"0.1.0","path":"0.1.0"}"#,
        )
        .unwrap();
        assert_eq!(resolve_install_dir_uncached(&container), None);
        let plain = dir.join("rpi-todo");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(resolve_install_dir_uncached(&plain), Some(plain.clone()));
    }

    #[test]
    fn retired_container_resolves_to_nothing() {
        let temp = TempDir::new();
        let container = temp.0.join("rpi-todo");
        manifest(&container, "rpi-todo", "0.1.0", "libtodo.so");
        let generation = container.join("0.2.0");
        manifest(&generation, "rpi-todo", "0.2.0", "libtodo.so");
        pointer(&container, "0.2.0", "0.2.0");
        std::fs::write(container.join(RETIRED_MARKER_FILE), b"").unwrap();
        assert_eq!(resolve_install_dir_uncached(&container), None);
    }

    #[test]
    fn fixed_generations_keep_the_generation_a_pointer_moved_away_from() {
        let temp = TempDir::new();
        let container = temp.0.join("rpi-todo");
        let first = container.join("0.2.0");
        manifest(&first, "rpi-todo", "0.2.0", "libtodo.so");
        pointer(&container, "0.2.0", "0.2.0");
        let fixed = FixedGenerations::new();
        assert_eq!(fixed.resolve(&container), Some(first.clone()));
        // Another process installs 0.3.0 and flips the pointer.
        let second = container.join("0.3.0");
        manifest(&second, "rpi-todo", "0.3.0", "libtodo.so");
        pointer(&container, "0.3.0", "0.3.0");
        assert_eq!(
            fixed.resolve(&container),
            Some(first.clone()),
            "/reload keeps loading the generation the process started with"
        );
        assert_eq!(
            resolve_install_dir_uncached(&container),
            Some(second.clone()),
            "an uncached reader sees the new generation"
        );
        assert_eq!(fixed.cached_count(), 1);
    }

    #[test]
    fn fixed_generations_do_not_cache_a_retired_container() {
        let temp = TempDir::new();
        let container = temp.0.join("rpi-todo");
        std::fs::create_dir_all(&container).unwrap();
        std::fs::write(container.join(RETIRED_MARKER_FILE), b"").unwrap();
        let fixed = FixedGenerations::new();
        assert_eq!(fixed.resolve(&container), None);
        assert_eq!(fixed.cached_count(), 0);
        // A reinstall clears the marker and places a generation.
        std::fs::remove_file(container.join(RETIRED_MARKER_FILE)).unwrap();
        let generation = container.join("0.2.0");
        manifest(&generation, "rpi-todo", "0.2.0", "libtodo.so");
        pointer(&container, "0.2.0", "0.2.0");
        assert_eq!(fixed.resolve(&container), Some(generation.clone()));
    }

    #[test]
    fn pin_install_resolution_fixes_the_generation_in_the_process_cache() {
        let temp = TempDir::new();
        // A unique container: the process cache is shared by every test in
        // this binary.
        let unique = format!(
            "rpi-install-layout-pin-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        let container = temp.0.join(unique);
        let generation = container.join("0.2.0");
        manifest(&generation, "rpi-todo", "0.2.0", "libtodo.so");
        pointer(&container, "0.2.0", "0.2.0");
        assert_eq!(pin_install_resolution(&container), Some(generation.clone()));
        let next = container.join("0.3.0");
        manifest(&next, "rpi-todo", "0.3.0", "libtodo.so");
        pointer(&container, "0.3.0", "0.3.0");
        assert_eq!(
            pin_install_resolution(&container),
            Some(generation.clone()),
            "the process cache answers with the pinned generation"
        );
        assert_eq!(resolve_install_dir_uncached(&container), Some(next));
    }

    #[test]
    fn reserved_container_entries_are_recognized() {
        for reserved in [
            GENERATION_POINTER_FILE,
            RETIRED_MARKER_FILE,
            ".staging",
            ".pins",
            ".rpi-install-source",
        ] {
            assert!(is_reserved_container_entry(reserved), "{reserved}");
        }
        for generation in ["0.1.9-rc.3", "0.1.8", "1.0.0"] {
            assert!(!is_reserved_container_entry(generation), "{generation}");
        }
    }
}
