//! Generational extension placement (design §11.8, write side).
//!
//! Only Windows writes generations. An extension is extracted and verified
//! under `<root>/.staging/<name>-<version>-<pid>/` (same volume as the
//! install root), then moved to `<root>/<name>/<version>/` and a pointer
//! `<root>/<name>/current.json` is flipped to it. Running instances keep
//! the generation they started with: it is pinned, so a placement that
//! finds its generation directory already there reuses it (after checking
//! the manifest is the same) or replaces it only when nobody holds a pin.
//! The logic is platform independent and tested on Linux.

use std::io;
use std::path::{Path, PathBuf};

use rpi_ext_host::install_layout::{
    GenerationPointer, POINTER_SCHEMA_VERSION, RETIRED_MARKER_FILE, read_generation_pointer,
};

use super::fs::UpdateFs;
use super::layout::UpdatePaths;
use super::pins::count_references;
use super::records::PinRecord;

/// Where an extension this process just extracted goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExtensionActivation {
    /// Unix: the in-place three-step swap of
    /// [`crate::core::extension_registry::activate_rpix`].
    #[default]
    InPlace,
    /// Windows `rpi install` / `rpi update --extensions`: a new generation
    /// directory and a pointer flip.
    Generational,
    /// Windows `rpi update --all`: the extension waits in staging for the
    /// startup transaction, together with the executable.
    Stage,
}

impl ExtensionActivation {
    /// The activation the running platform uses for an immediate install.
    pub fn platform_default() -> Self {
        if cfg!(windows) {
            ExtensionActivation::Generational
        } else {
            ExtensionActivation::InPlace
        }
    }

    pub fn uses_staging_dir(self) -> bool {
        matches!(
            self,
            ExtensionActivation::Generational | ExtensionActivation::Stage
        )
    }
}

/// The extraction directory of an extension about to be placed. Windows
/// stages under `<root>/.staging/` (same volume, hidden from discovery);
/// Unix keeps the legacy `<root>/<name>.tmp-<pid>/`.
pub fn extraction_dir(
    root: &Path,
    name: &str,
    version: &str,
    activation: ExtensionActivation,
) -> PathBuf {
    if activation.uses_staging_dir() {
        UpdatePaths::extension_staging_path(root, name, version, std::process::id())
    } else {
        root.join(format!("{name}.tmp-{}", std::process::id()))
    }
}

/// Outcome of [`place_generation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedGeneration {
    /// `<root>/<name>/`.
    pub container: PathBuf,
    /// The directory the pointer now names.
    pub generation: PathBuf,
    /// An existing identical directory was reused instead of replaced.
    pub reused: bool,
    /// The version the pointer named before the flip.
    pub previous: Option<String>,
}

/// The pointer of an installed generation.
pub fn generation_pointer(
    fs: &dyn UpdateFs,
    generation: &Path,
    version: &str,
    previous: Option<&str>,
) -> io::Result<GenerationPointer> {
    let manifest = fs.read(&generation.join("rpi-extension.json"))?;
    let rpi_abi = serde_json::from_slice::<serde_json::Value>(&manifest)
        .ok()
        .and_then(|value| value.get("rpiAbi").and_then(|abi| abi.as_u64()))
        .and_then(|abi| u32::try_from(abi).ok())
        .unwrap_or(0);
    Ok(GenerationPointer {
        schema_version: POINTER_SCHEMA_VERSION,
        version: version.to_owned(),
        path: generation
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| version.to_owned()),
        manifest_sha256: Some(super::fs::sha256_bytes(&manifest)),
        rpi_abi,
        previous: previous.map(str::to_owned),
    })
}

/// Write `<container>/current.json` through a temporary file and a rename,
/// retrying transient sharing violations (design §11.5).
pub fn flip_pointer(
    fs: &dyn UpdateFs,
    container: &Path,
    pointer: &GenerationPointer,
    retries: u32,
    sleep: &dyn Fn(std::time::Duration),
) -> io::Result<()> {
    let target = container.join(rpi_ext_host::install_layout::GENERATION_POINTER_FILE);
    let mut temp = target.as_os_str().to_owned();
    temp.push(format!(".tmp-{}", std::process::id()));
    let temp = PathBuf::from(temp);
    fs.write_atomic(&temp, super::records::to_json(pointer).as_bytes())?;
    let result = super::fs::rename_with_retry(fs, &temp, &target, retries, sleep);
    if result.is_err() {
        let _ = fs.remove_file(&temp);
    }
    result
}

/// Place `staged_path` as generation `version` of `<root>/<name>/` and flip
/// the pointer to it.
///
/// Three rules when the generation directory already exists (design
/// §11.5): no pin names it → replace it (delete, then move); it cannot be
/// deleted → reuse it when its manifest is identical; a pin names it →
/// reuse it when its manifest is identical. A different manifest behind a
/// pin or an undeletable directory is an error: the update is refused, not
/// forced.
pub fn place_generation(
    fs: &dyn UpdateFs,
    root: &Path,
    name: &str,
    version: &str,
    staged_path: &Path,
    live_pins: &[PinRecord],
) -> Result<PlacedGeneration, String> {
    crate::core::extension_registry::ensure_safe_extension_name(name)?;
    let container = root.join(name);
    fs.create_dir_all(&container)
        .map_err(|error| error.to_string())?;
    // `rpi install` clears a retirement left by `rpi remove`.
    let retired = container.join(RETIRED_MARKER_FILE);
    if fs.exists(&retired).map_err(|error| error.to_string())? {
        fs.remove_file(&retired)
            .map_err(|error| error.to_string())?;
    }
    let previous = read_generation_pointer(&container).map(|pointer| pointer.version);
    let generation = container.join(super::layout::safe_version_component(version));
    let mut reused = false;
    if fs.exists(&generation).map_err(|error| error.to_string())? {
        let pinned = count_references(live_pins, &generation) > 0;
        let replaced = if pinned {
            false
        } else {
            fs.remove_dir_all(&generation).is_ok()
        };
        if !replaced {
            let staged_manifest = fs
                .read(&staged_path.join("rpi-extension.json"))
                .map_err(|error| format!("could not read the staged manifest: {error}"))?;
            let existing = fs.read(&generation.join("rpi-extension.json")).ok();
            if existing.as_deref() != Some(staged_manifest.as_slice()) {
                return Err(format!(
                    "{name} {version} is already installed as a different build and is in use by a \
                     running rpi instance; close it and retry",
                ));
            }
            reused = true;
        }
    }
    if !reused {
        fs.rename(staged_path, &generation).map_err(|error| {
            format!(
                "could not move the staged {name} {version} into place ({}): {error}",
                generation.display()
            )
        })?;
    } else {
        let _ = fs.remove_dir_all(staged_path);
    }
    let pointer = generation_pointer(fs, &generation, version, previous.as_deref())
        .map_err(|error| format!("could not read the placed manifest: {error}"))?;
    flip_pointer(
        fs,
        &container,
        &pointer,
        super::RENAME_RETRIES,
        &|duration| fs.retry_sleep(duration),
    )
    .map_err(|error| format!("could not update the {name} pointer: {error}"))?;
    // This process may have resolved the container while planning the
    // update; it must now report the generation it just placed.
    rpi_ext_host::install_layout::refresh_install_resolution(&container);
    Ok(PlacedGeneration {
        container,
        generation,
        reused,
        previous,
    })
}

/// Retire a whole container (`rpi remove`, R7.4.8): drop the pointer, mark
/// it retired (every reader then sees "not installed"), and let the cleanup
/// scan remove the files once no process uses them.
pub fn retire_container(fs: &dyn UpdateFs, container: &Path) -> io::Result<()> {
    if fs.exists(container)? {
        fs.remove_file(&container.join(rpi_ext_host::install_layout::GENERATION_POINTER_FILE))?;
        fs.write_atomic(&container.join(RETIRED_MARKER_FILE), b"")?;
    }
    rpi_ext_host::install_layout::refresh_install_resolution(container);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::windows_update::boot::FakeProcessProbe;
    use crate::core::windows_update::fs::SpyFs;
    use crate::core::windows_update::pins::PinHandle;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        dir: PathBuf,
        fs: Arc<SpyFs>,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "rpi-wu-gen-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            ));
            let root = dir.join("agent/extensions");
            std::fs::create_dir_all(&root).unwrap();
            Fixture {
                dir,
                fs: Arc::new(SpyFs::new()),
                root,
            }
        }

        fn stage(&self, name: &str, version: &str, payload: &str) -> PathBuf {
            let staged = UpdatePaths::extension_staging_path(&self.root, name, version, 7);
            std::fs::create_dir_all(staged.join("lib")).unwrap();
            std::fs::write(
                staged.join("rpi-extension.json"),
                format!(
                    r#"{{"name":"{name}","version":"{version}","native":"lib/lib.so","rpiAbi":1}}"#
                ),
            )
            .unwrap();
            std::fs::write(staged.join("lib/lib.so"), payload).unwrap();
            staged
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn extraction_dir_follows_the_activation_mode() {
        let root = Path::new("/root/extensions");
        let generational = extraction_dir(
            root,
            "rpi-todo",
            "0.1.9-rc.4",
            ExtensionActivation::Generational,
        );
        assert_eq!(
            generational,
            root.join(".staging")
                .join(format!("rpi-todo-0.1.9-rc.4-{}", std::process::id()))
        );
        assert_eq!(
            extraction_dir(root, "rpi-todo", "0.1.9-rc.4", ExtensionActivation::Stage),
            generational
        );
        assert_eq!(
            extraction_dir(root, "rpi-todo", "0.1.9-rc.4", ExtensionActivation::InPlace),
            root.join(format!("rpi-todo.tmp-{}", std::process::id()))
        );
        assert_eq!(
            ExtensionActivation::platform_default(),
            if cfg!(windows) {
                ExtensionActivation::Generational
            } else {
                ExtensionActivation::InPlace
            }
        );
    }

    #[test]
    fn first_generation_over_a_flat_install_flips_the_pointer() {
        let f = Fixture::new();
        // Legacy flat install.
        std::fs::create_dir_all(f.root.join("rpi-todo")).unwrap();
        std::fs::write(
            f.root.join("rpi-todo/rpi-extension.json"),
            r#"{"name":"rpi-todo","version":"0.1.0"}"#,
        )
        .unwrap();
        let staged = f.stage("rpi-todo", "0.2.0", "new");
        let placed = place_generation(&*f.fs, &f.root, "rpi-todo", "0.2.0", &staged, &[]).unwrap();
        assert!(!placed.reused);
        assert_eq!(placed.previous, None);
        assert_eq!(placed.generation, f.root.join("rpi-todo/0.2.0"));
        assert!(!staged.exists());
        let pointer = read_generation_pointer(&f.root.join("rpi-todo")).unwrap();
        assert_eq!(pointer.version, "0.2.0");
        assert_eq!(pointer.path, "0.2.0");
        assert_eq!(pointer.rpi_abi, 1);
        assert_eq!(pointer.manifest_sha256.as_deref().map(str::len), Some(64));
        assert!(pointer.previous.is_none());
        assert_eq!(
            rpi_ext_host::install_layout::resolve_install_dir(&f.root.join("rpi-todo")),
            Some(f.root.join("rpi-todo/0.2.0")),
            "the resolver sees the new generation"
        );
        // The flat files stay until the cleanup scan reclaims them.
        assert!(f.root.join("rpi-todo/rpi-extension.json").exists());
    }

    #[test]
    fn an_existing_unpinned_generation_is_replaced() {
        let f = Fixture::new();
        let staged = f.stage("rpi-todo", "0.2.0", "v2");
        place_generation(&*f.fs, &f.root, "rpi-todo", "0.2.0", &staged, &[]).unwrap();
        // Someone leaves a same-version directory with other content and no pointer.
        std::fs::write(f.root.join("rpi-todo/0.2.0/lib/lib.so"), "tampered").unwrap();
        std::fs::remove_file(f.root.join("rpi-todo/current.json")).unwrap();
        let staged = f.stage("rpi-todo", "0.2.0", "v2");
        let placed = place_generation(&*f.fs, &f.root, "rpi-todo", "0.2.0", &staged, &[]).unwrap();
        assert!(!placed.reused);
        assert_eq!(
            std::fs::read_to_string(f.root.join("rpi-todo/0.2.0/lib/lib.so")).unwrap(),
            "v2"
        );
    }

    #[test]
    fn a_pinned_identical_generation_is_reused_and_a_different_one_is_refused() {
        let f = Fixture::new();
        let staged = f.stage("rpi-todo", "0.2.0", "same");
        let placed = place_generation(&*f.fs, &f.root, "rpi-todo", "0.2.0", &staged, &[]).unwrap();
        let running = PinRecord {
            dirs: vec![placed.generation.clone()],
            ..PinRecord::new(77, 770, "s77".to_owned(), "1.0.0", &f.dir.join("rpi.exe"))
        };

        // Identical build: reused, the running instance is undisturbed.
        let staged = f.stage("rpi-todo", "0.2.0", "same");
        let again = place_generation(
            &*f.fs,
            &f.root,
            "rpi-todo",
            "0.2.0",
            &staged,
            std::slice::from_ref(&running),
        )
        .unwrap();
        assert!(again.reused);
        assert!(
            !staged.exists(),
            "the redundant staging directory is dropped"
        );
        assert_eq!(
            std::fs::read_to_string(f.root.join("rpi-todo/0.2.0/lib/lib.so")).unwrap(),
            "same"
        );

        // A different manifest behind a pin is refused.
        let staged = f.stage("rpi-todo", "0.2.0", "other");
        std::fs::write(
            staged.join("rpi-extension.json"),
            r#"{"name":"rpi-todo","version":"0.2.0","native":"lib/lib.so","rpiAbi":1,"author":"other"}"#,
        )
        .unwrap();
        let error = place_generation(
            &*f.fs,
            &f.root,
            "rpi-todo",
            "0.2.0",
            &staged,
            std::slice::from_ref(&running),
        )
        .unwrap_err();
        assert!(
            error.contains("in use by a running rpi instance"),
            "{error}"
        );
        assert!(
            staged.exists(),
            "the refused staging directory is left for the caller to clean"
        );
    }

    #[test]
    fn an_undeletable_generation_is_reused_only_when_identical() {
        let f = Fixture::new();
        let staged = f.stage("rpi-todo", "0.2.0", "same");
        place_generation(&*f.fs, &f.root, "rpi-todo", "0.2.0", &staged, &[]).unwrap();
        // A scanner holds the library: deletion fails, the build is the same.
        f.fs.fail(
            crate::core::windows_update::fs::FsOp::RemoveDirAll,
            "rpi-todo/0.2.0",
            1,
        );
        let staged = f.stage("rpi-todo", "0.2.0", "same");
        let placed = place_generation(&*f.fs, &f.root, "rpi-todo", "0.2.0", &staged, &[]).unwrap();
        assert!(placed.reused);
        assert_eq!(placed.previous.as_deref(), Some("0.2.0"));

        f.fs.fail(
            crate::core::windows_update::fs::FsOp::RemoveDirAll,
            "rpi-todo/0.2.0",
            1,
        );
        std::fs::write(
            f.root.join("rpi-todo/0.2.0/rpi-extension.json"),
            r#"{"name":"rpi-todo","version":"0.2.0","native":"lib/lib.so","rpiAbi":1,"author":"x"}"#,
        )
        .unwrap();
        let staged = f.stage("rpi-todo", "0.2.0", "same");
        let error =
            place_generation(&*f.fs, &f.root, "rpi-todo", "0.2.0", &staged, &[]).unwrap_err();
        assert!(error.contains("in use"), "{error}");
    }

    #[test]
    fn newer_generation_records_the_previous_one_and_reinstall_clears_retirement() {
        let f = Fixture::new();
        let staged = f.stage("rpi-todo", "0.2.0", "a");
        place_generation(&*f.fs, &f.root, "rpi-todo", "0.2.0", &staged, &[]).unwrap();
        let staged = f.stage("rpi-todo", "0.3.0", "b");
        let placed = place_generation(&*f.fs, &f.root, "rpi-todo", "0.3.0", &staged, &[]).unwrap();
        assert_eq!(placed.previous.as_deref(), Some("0.2.0"));
        let pointer = read_generation_pointer(&f.root.join("rpi-todo")).unwrap();
        assert_eq!(pointer.previous.as_deref(), Some("0.2.0"));
        assert!(
            f.root.join("rpi-todo/0.2.0").exists(),
            "older generations stay for the GC"
        );

        // `rpi remove` retires; `rpi install` clears the marker.
        retire_container(&*f.fs, &f.root.join("rpi-todo")).unwrap();
        assert!(rpi_ext_host::install_layout::is_retired(
            &f.root.join("rpi-todo")
        ));
        assert!(!f.root.join("rpi-todo/current.json").exists());
        let staged = f.stage("rpi-todo", "0.4.0", "c");
        place_generation(&*f.fs, &f.root, "rpi-todo", "0.4.0", &staged, &[]).unwrap();
        assert!(!rpi_ext_host::install_layout::is_retired(
            &f.root.join("rpi-todo")
        ));
        assert_eq!(
            read_generation_pointer(&f.root.join("rpi-todo"))
                .unwrap()
                .version,
            "0.4.0"
        );
    }

    #[test]
    fn pointer_flip_retries_transient_failures_and_leaves_no_temp_file() {
        let f = Fixture::new();
        let staged = f.stage("rpi-todo", "0.2.0", "x");
        f.fs.fail_with(
            crate::core::windows_update::fs::FsOp::Rename,
            "current.json",
            2,
            io::Error::from_raw_os_error(32),
        );
        place_generation(&*f.fs, &f.root, "rpi-todo", "0.2.0", &staged, &[]).unwrap();
        assert_eq!(f.fs.sleeps().len(), 2);
        let mut leftovers: Vec<String> = std::fs::read_dir(f.root.join("rpi-todo"))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        leftovers.sort();
        assert_eq!(leftovers, vec!["0.2.0", "current.json"]);

        // A persistent failure removes the temporary pointer.
        f.fs.fail_with(
            crate::core::windows_update::fs::FsOp::Rename,
            "current.json",
            6,
            io::Error::from_raw_os_error(32),
        );
        let staged = f.stage("rpi-todo", "0.3.0", "y");
        assert!(place_generation(&*f.fs, &f.root, "rpi-todo", "0.3.0", &staged, &[]).is_err());
        let leftovers: Vec<String> = std::fs::read_dir(f.root.join("rpi-todo"))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("current.json"))
            .collect();
        assert_eq!(
            leftovers,
            vec!["current.json"],
            "only the intact pointer remains"
        );
    }

    #[test]
    fn pin_handle_round_trip_with_dirs() {
        let f = Fixture::new();
        let probe = FakeProcessProbe::new(5, 50);
        probe.spawn(77, 770);
        let mut handle = PinHandle::register(
            f.fs.clone() as Arc<dyn UpdateFs>,
            &f.root.join(".pins"),
            PinRecord::new(77, 770, "s".to_owned(), "1.2.3", &f.dir.join("rpi.exe")),
        )
        .unwrap();
        handle.add_dirs(&[f.root.join("rpi-todo/0.2.0")]).unwrap();
        assert_eq!(handle.record().dirs.len(), 1);
        let live = super::super::pins::live_pins(&*f.fs, &f.root.join(".pins"), &probe);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].exe_version, "1.2.3");
    }
}
