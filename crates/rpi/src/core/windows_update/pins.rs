//! Process registrations (pins) for extension generations (design §11.8).
//!
//! A pin is a file, `.pins/<pid>.json` under the global extensions root,
//! naming one rpi process and the extension directories it loads from. The
//! delayed garbage collection keeps every directory a live pin names, so an
//! update can place a new generation while other instances keep the one
//! they started with. Pins are only ever *written* on Windows (the startup
//! hook registers every process, subagent and preflight children included);
//! the logic itself is platform independent and runs over the same file
//! seam as the rest of the module.
//!
//! Liveness is pid plus start time (a reused pid is a different process).
//! Reading the registry also reaps the files of dead processes.

use std::path::{Path, PathBuf};

use super::boot::{ProcessProbe, is_live};
use super::fs::UpdateFs;
use super::records::{PinRecord, read_record, to_json};

/// Comparison key for two spellings of one directory: Windows paths are
/// case-insensitive and separators vary; elsewhere the path is the key.
pub fn path_key(path: &Path) -> String {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy().replace('/', "\\");
        let lowered = text.trim_start_matches(r"\\?\").to_lowercase();
        lowered.trim_end_matches('\\').to_owned()
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy().into_owned()
    }
}

/// Whether two paths name the same directory (see [`path_key`]).
pub fn same_path(left: &Path, right: &Path) -> bool {
    path_key(left) == path_key(right)
}

/// The registration of the calling process. Dropping it removes the file.
pub struct PinHandle {
    fs: std::sync::Arc<dyn UpdateFs>,
    path: PathBuf,
    record: PinRecord,
}

impl Drop for PinHandle {
    fn drop(&mut self) {
        let _ = self.fs.remove_file(&self.path);
    }
}

impl PinHandle {
    /// Write the registration for `record` (creating `.pins/`).
    pub fn register(
        fs: std::sync::Arc<dyn UpdateFs>,
        pins_dir: &Path,
        record: PinRecord,
    ) -> std::io::Result<Self> {
        fs.create_dir_all(pins_dir)?;
        let path = pins_dir.join(format!("{}.json", record.pid));
        let handle = PinHandle { fs, path, record };
        handle.persist()?;
        Ok(handle)
    }

    fn persist(&self) -> std::io::Result<()> {
        self.fs
            .write_atomic(&self.path, to_json(&self.record).as_bytes())
    }

    pub fn record(&self) -> &PinRecord {
        &self.record
    }

    /// Add directories to the registration (before they are loaded). A
    /// directory already listed is a no-op; a failed update is returned and
    /// the in-memory list stays unchanged.
    pub fn add_dirs(&mut self, dirs: &[PathBuf]) -> std::io::Result<()> {
        let mut next = self.record.dirs.clone();
        for dir in dirs {
            if !next.iter().any(|listed| same_path(listed, dir)) {
                next.push(dir.clone());
            }
        }
        if next.len() == self.record.dirs.len() {
            return Ok(());
        }
        self.record.dirs = next;
        self.persist()
    }

    /// Remove the registration explicitly (the drop does the same).
    pub fn release(self) {
        drop(self);
    }
}

/// Whether `record` names `dir`.
pub fn pin_references(record: &PinRecord, dir: &Path) -> bool {
    record.dirs.iter().any(|listed| same_path(listed, dir))
}

/// Number of records naming `dir`.
pub fn count_references<'a>(pins: impl IntoIterator<Item = &'a PinRecord>, dir: &Path) -> usize {
    pins.into_iter()
        .filter(|record| pin_references(record, dir))
        .count()
}

/// Every registration whose process still runs, each verified by pid *and*
/// start time. Files of dead processes are removed as they are found (the
/// dead-pid pins are reaped, R7.5.1); a malformed file is removed
/// unless its name is the pid of a live process (leave a live process's
/// corrupt file alone: its owner deletes it on exit).
pub fn live_pins(fs: &dyn UpdateFs, pins_dir: &Path, probe: &dyn ProcessProbe) -> Vec<PinRecord> {
    let Ok(entries) = fs.read_dir(pins_dir) else {
        return Vec::new();
    };
    let mut live = Vec::new();
    for entry in entries {
        if entry.is_dir {
            continue;
        }
        match read_record::<PinRecord>(fs, &entry.path) {
            Some(record) => {
                if is_live(probe, record.pid, record.process_start) {
                    live.push(record);
                } else {
                    let _ = fs.remove_file(&entry.path);
                }
            }
            None => {
                let pid = entry
                    .path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .and_then(|stem| stem.parse::<u32>().ok());
                let owner_alive = pid.is_some_and(|pid| probe.process_start(pid).is_some());
                if !owner_alive {
                    let _ = fs.remove_file(&entry.path);
                }
            }
        }
    }
    live.sort_by_key(|record| record.pid);
    live
}

/// The directories under `roots` that an instance about to load extensions
/// uses: the generation or flat directory every container resolves to. The
/// Windows startup hook pins exactly these before anything loads.
pub fn extension_dirs_in_use(fs: &dyn UpdateFs, roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for root in roots {
        let Ok(entries) = fs.read_dir(root) else {
            continue;
        };
        for entry in entries {
            if !entry.is_dir {
                continue;
            }
            let Some(name) = entry.path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if rpi_ext_host::install_layout::is_reserved_container_entry(name) {
                continue;
            }
            if let Some(dir) = rpi_ext_host::install_layout::pin_install_resolution(&entry.path)
                && dir.join("rpi-extension.json").is_file()
            {
                dirs.push(dir);
            }
        }
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::windows_update::fs::SpyFs;
    use std::sync::Arc;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rpi-wu-pins-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn path_keys_ignore_case_and_verbatim_prefixes_on_windows_only() {
        assert!(same_path(Path::new("/a/b"), Path::new("/a/b")));
        assert!(!same_path(Path::new("/a/b"), Path::new("/a/bc")));
        #[cfg(windows)]
        {
            assert!(same_path(
                Path::new(r"C:\Users\Me\x"),
                Path::new(r"c:\users\me\X")
            ));
            assert!(same_path(
                Path::new(r"C:\Users\Me\x"),
                Path::new(r"\\?\C:\Users\Me\x")
            ));
        }
    }

    #[test]
    fn a_pin_is_written_updated_and_removed_by_its_owner() {
        let fs = Arc::new(SpyFs::new());
        let dir = temp_dir();
        let pins = dir.join(".pins");
        let record = PinRecord::new(
            4242,
            99,
            "session-a".to_owned(),
            "1.0.0",
            Path::new("/bin/rpi.exe"),
        );
        let mut handle =
            PinHandle::register(Arc::clone(&fs) as Arc<dyn UpdateFs>, &pins, record.clone())
                .unwrap();
        let path = pins.join("4242.json");
        let stored: PinRecord = read_record(&*fs, &path).unwrap();
        assert_eq!(stored, record);
        assert!(stored.dirs.is_empty());

        let generation = dir.join("extensions/rpi-todo/1.0.0");
        handle.add_dirs(std::slice::from_ref(&generation)).unwrap();
        // Adding the same directory again does not rewrite the file.
        fs.reset();
        handle.add_dirs(std::slice::from_ref(&generation)).unwrap();
        assert!(!fs.called(
            crate::core::windows_update::fs::FsOp::WriteAtomic,
            "4242.json"
        ));
        let stored: PinRecord = read_record(&*fs, &path).unwrap();
        assert_eq!(stored.dirs, vec![generation.clone()]);

        handle.release();
        assert!(!path.exists());
        assert!(pins.exists(), "the registry directory stays");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn live_pins_reap_dead_owners_and_tell_a_reused_pid() {
        let fs = SpyFs::new();
        let dir = temp_dir();
        let pins = dir.join(".pins");
        let probe = crate::core::windows_update::boot::FakeProcessProbe::new(1, 1);
        probe.spawn(10, 100);
        probe.spawn(11, 110);
        let make = |pid: u32, start: u64| {
            PinRecord::new(
                pid,
                start,
                format!("s{pid}"),
                "1.0.0",
                Path::new("/bin/rpi.exe"),
            )
        };
        for record in [make(10, 100), make(11, 110), make(12, 120)] {
            fs.write_atomic(
                &pins.join(format!("{}.json", record.pid)),
                to_json(&record).as_bytes(),
            )
            .unwrap();
        }
        // 11 was reused by a new process: the old registration is dead.
        probe.kill(11);
        probe.spawn(11, 111);
        // Malformed files: pid 99 is dead (reaped), pid 10 is alive (kept:
        // its owner removes the file on exit).
        fs.write_atomic(&pins.join("99.json"), b"{not json")
            .unwrap();
        fs.write_atomic(&pins.join("98.txt"), b"{not json").unwrap();
        let live = live_pins(&fs, &pins, &probe);
        assert_eq!(
            live.iter().map(|record| record.pid).collect::<Vec<_>>(),
            vec![10]
        );
        assert!(
            !pins.join("11.json").exists(),
            "the reused pid's file was reaped"
        );
        assert!(
            !pins.join("12.json").exists(),
            "the dead pid's file was reaped"
        );
        assert!(!pins.join("99.json").exists());
        assert!(!pins.join("98.txt").exists());
        assert!(live_pins(&fs, &dir.join("missing"), &probe).is_empty());

        // A malformed file of a live owner is left alone and never reported.
        fs.write_atomic(&pins.join("10.json"), b"{truncated")
            .unwrap();
        assert!(live_pins(&fs, &pins, &probe).is_empty());
        assert!(pins.join("10.json").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn references_are_counted_by_directory() {
        let base = PathBuf::from("/root/extensions");
        let mut a = PinRecord::new(1, 1, "a".to_owned(), "1.0.0", Path::new("/r"));
        a.dirs = vec![base.join("rpi-todo/1.0.0"), base.join("rpi-plan-mode")];
        let mut b = PinRecord::new(2, 2, "b".to_owned(), "1.0.0", Path::new("/r"));
        b.dirs = vec![base.join("rpi-todo/2.0.0")];
        assert_eq!(count_references([&a, &b], &base.join("rpi-todo/1.0.0")), 1);
        assert_eq!(count_references([&a, &b], &base.join("rpi-todo")), 0);
        assert_eq!(count_references([&a, &b], &base.join("rpi-plan-mode")), 1);
        assert!(pin_references(&b, &base.join("rpi-todo/2.0.0")));
    }

    #[test]
    fn extension_dirs_in_use_resolve_every_container_of_every_root() {
        let fs = SpyFs::new();
        let dir = temp_dir();
        let root = dir.join("extensions");
        std::fs::create_dir_all(root.join("flat")).unwrap();
        std::fs::write(root.join("flat/rpi-extension.json"), "{}").unwrap();
        std::fs::create_dir_all(root.join("gen/0.2.0")).unwrap();
        std::fs::write(root.join("gen/0.2.0/rpi-extension.json"), "{}").unwrap();
        std::fs::write(
            root.join("gen/current.json"),
            r#"{"schemaVersion":1,"version":"0.2.0","path":"0.2.0"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".staging/x-1-2")).unwrap();
        std::fs::write(root.join(".staging/x-1-2/rpi-extension.json"), "{}").unwrap();
        std::fs::create_dir_all(root.join("retired")).unwrap();
        std::fs::write(root.join("retired/rpi-extension.json"), "{}").unwrap();
        std::fs::write(root.join("retired/.retired"), "").unwrap();
        std::fs::create_dir_all(root.join("empty")).unwrap();
        let missing_root = dir.join("missing-root");
        let mut dirs = extension_dirs_in_use(&fs, &[root.clone(), missing_root]);
        dirs.sort();
        assert_eq!(
            dirs,
            vec![root.join("flat"), root.join("gen/0.2.0")],
            "flat and the live generation are in use; staging, retired and empty containers are not"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
