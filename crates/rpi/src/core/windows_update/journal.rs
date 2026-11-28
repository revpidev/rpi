//! The startup-transaction journal and its undos (design §11.5).
//!
//! Every step of the transaction is appended to `journal.json` *before* it
//! runs, carrying what its undo needs. The journal is removed when the
//! transaction commits; a journal that is still there on the next start
//! belongs to a transaction that died mid-way, and its steps are undone in
//! reverse order before anything else is judged. Each undo is idempotent by
//! target state: a step whose outcome was already restored by someone else
//! (`install.ps1` preflight, a manual rename) is skipped, a step that
//! cannot be undone is logged and the loop continues — startup is never
//! blocked by an undo.

use std::io;
use std::path::{Path, PathBuf};

use super::fs::UpdateFs;
use super::layout::UpdatePaths;
use super::records::{Journal, JournalStep, read_record, to_json};

/// What an undo needs: the paths, the file system and a log sink.
pub struct UndoContext<'a> {
    pub paths: &'a UpdatePaths,
    pub fs: &'a dyn UpdateFs,
    pub log: &'a dyn Fn(&str),
}

impl UndoContext<'_> {
    fn note(&self, message: String) {
        (self.log)(&message);
    }
}

/// Read the journal; `None` when absent (no transaction ran or it
/// committed) or malformed (treated as committed: nothing to undo).
pub fn load(fs: &dyn UpdateFs, path: &Path) -> Option<Journal> {
    read_record::<Journal>(fs, path)
}

/// Create an empty, uncommitted journal (over any leftover).
pub fn create(fs: &dyn UpdateFs, path: &Path) -> io::Result<()> {
    let journal = Journal {
        schema_version: super::records::STATE_SCHEMA_VERSION,
        steps: Vec::new(),
    };
    fs.create_dir_all(path.parent().unwrap_or_else(|| Path::new(".")))?;
    fs.write_atomic(path, to_json(&journal).as_bytes())
}

/// Record `step` (before it runs).
pub fn append(fs: &dyn UpdateFs, path: &Path, step: JournalStep) -> io::Result<()> {
    let mut journal = load(fs, path).unwrap_or(Journal {
        schema_version: super::records::STATE_SCHEMA_VERSION,
        steps: Vec::new(),
    });
    journal.steps.push(step);
    fs.write_atomic(path, to_json(&journal).as_bytes())
}

/// Commit: the journal goes away, the steps are now the applied update's
/// history (kept in `undo.json` for a first-boot rollback).
pub fn commit(fs: &dyn UpdateFs, path: &Path) -> io::Result<()> {
    fs.remove_file(path)
}

/// Undo a transaction that never committed: reverse the recorded steps and
/// remove the journal. Returns the failures (already logged).
pub fn rollback_uncommitted(context: &UndoContext<'_>) -> Vec<String> {
    let journal_path = context.paths.journal_path();
    let Some(journal) = load(context.fs, &journal_path) else {
        return Vec::new();
    };
    context.note(format!(
        "journal: uncommitted transaction with {} step(s) found; undoing",
        journal.steps.len()
    ));
    let failures = undo_steps(context, &journal.steps);
    if let Err(error) = context.fs.remove_file(&journal_path) {
        context.note(format!(
            "journal: could not remove {}: {error}",
            journal_path.display()
        ));
    }
    failures
}

/// Undo `steps` in reverse order; a failing step is logged and skipped.
pub fn undo_steps(context: &UndoContext<'_>, steps: &[JournalStep]) -> Vec<String> {
    let mut failures = Vec::new();
    for step in steps.iter().rev() {
        if let Err(error) = undo_step(context, step) {
            context.note(format!(
                "journal: undo failed (continuing): {}: {error}",
                describe(step)
            ));
            failures.push(format!("{}: {error}", describe(step)));
        }
    }
    failures
}

fn describe(step: &JournalStep) -> String {
    match step {
        JournalStep::PlaceExtension { name, .. } => format!("placeExtension {name}"),
        JournalStep::FlipPointer { container, .. } => {
            format!("flipPointer {}", container.display())
        }
        JournalStep::ExeSwap { .. } => "exeSwap".to_owned(),
        JournalStep::WriteManifest { .. } => "writeManifest".to_owned(),
    }
}

/// The idempotent undo of one step.
pub fn undo_step(context: &UndoContext<'_>, step: &JournalStep) -> io::Result<()> {
    match step {
        JournalStep::PlaceExtension {
            generation_path,
            staged_path,
            ..
        } => {
            // The move happened when the generation exists and the staging
            // directory is gone; the pointer was not flipped yet (pointer
            // steps are undone after this one), so nothing references it.
            if context.fs.exists(generation_path)? && !context.fs.exists(staged_path)? {
                context.fs.remove_dir_all(generation_path)?;
            }
            Ok(())
        }
        JournalStep::FlipPointer {
            container,
            previous_text,
            new_text,
        } => {
            let pointer = container.join(rpi_ext_host::install_layout::GENERATION_POINTER_FILE);
            match context.fs.read(&pointer) {
                Ok(current) if current == new_text.as_bytes() => {
                    restore_text(context.fs, &pointer, previous_text.as_deref())
                }
                // Already restored by someone else, or never flipped.
                _ => Ok(()),
            }
        }
        JournalStep::ExeSwap { old_name } => {
            // The first rename happened when the parked image exists; the
            // second may or may not have. Renaming the parked image back
            // over the executable covers both (a missing parked image means
            // the swap never started or was already restored).
            let parked = context
                .paths
                .exe_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(old_name);
            if context.fs.exists(&parked)? {
                super::fs::rename_with_retry(
                    context.fs,
                    &parked,
                    &context.paths.exe_path,
                    super::RENAME_RETRIES,
                    &|duration| std::thread::sleep(duration),
                )
            } else {
                Ok(())
            }
        }
        JournalStep::WriteManifest { previous_text } => restore_text(
            context.fs,
            &context.paths.manifest_path,
            previous_text.as_deref(),
        ),
    }
}

/// Restore a text file to `previous` (`None`: it did not exist). A file
/// that already carries the previous text — or is already absent — is left
/// alone.
pub fn restore_text(fs: &dyn UpdateFs, path: &Path, previous: Option<&str>) -> io::Result<()> {
    let current = fs.read(path).ok();
    match (previous, current) {
        (None, None) => Ok(()),
        (None, Some(_)) => fs.remove_file(path),
        (Some(previous), Some(current)) if current == previous.as_bytes() => Ok(()),
        (Some(previous), _) => fs.write_atomic(path, previous.as_bytes()),
    }
}

/// The journal file's parent (for tests that build a path set).
pub fn journal_parent(paths: &UpdatePaths) -> PathBuf {
    paths
        .journal_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::windows_update::fs::SpyFs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        dir: PathBuf,
        fs: SpyFs,
        paths: UpdatePaths,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "rpi-wu-journal-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(dir.join("prefix/.update")).unwrap();
            std::fs::create_dir_all(dir.join("agent/extensions/rpi-todo")).unwrap();
            let fs = SpyFs::new();
            let paths = UpdatePaths::new(
                &dir.join("prefix/rpi.exe"),
                dir.join("logs/update.log"),
                &dir.join("agent"),
                None,
            );
            Fixture { dir, fs, paths }
        }

        fn context<'a>(&'a self, log: &'a dyn Fn(&str)) -> UndoContext<'a> {
            UndoContext {
                paths: &self.paths,
                fs: &self.fs,
                log,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn full_journal(f: &Fixture) -> Vec<JournalStep> {
        vec![
            JournalStep::PlaceExtension {
                name: "rpi-todo".to_owned(),
                staged_path: f.dir.join("agent/extensions/.staging/rpi-todo-0.2.0-7"),
                generation_path: f.dir.join("agent/extensions/rpi-todo/0.2.0"),
            },
            JournalStep::FlipPointer {
                container: f.dir.join("agent/extensions/rpi-todo"),
                previous_text: None,
                new_text: r#"{"schemaVersion":1,"version":"0.2.0","path":"0.2.0"}"#.to_owned(),
            },
            JournalStep::ExeSwap {
                old_name: "rpi.exe.old-0.1.0".to_owned(),
            },
            JournalStep::WriteManifest {
                previous_text: Some(r#"{"version":"0.1.0"}"#.to_owned()),
            },
        ]
    }

    #[test]
    fn journal_append_load_and_commit() {
        let f = Fixture::new();
        let path = f.paths.journal_path();
        assert!(load(&f.fs, &path).is_none());
        create(&f.fs, &path).unwrap();
        assert!(load(&f.fs, &path).unwrap().steps.is_empty());
        append(&f.fs, &path, full_journal(&f).remove(0)).unwrap();
        append(
            &f.fs,
            &path,
            JournalStep::WriteManifest {
                previous_text: None,
            },
        )
        .unwrap();
        let journal = load(&f.fs, &path).unwrap();
        assert_eq!(journal.steps.len(), 2);
        assert!(matches!(
            journal.steps[0],
            JournalStep::PlaceExtension { .. }
        ));
        commit(&f.fs, &path).unwrap();
        assert!(load(&f.fs, &path).is_none());
    }

    #[test]
    fn undo_reverses_every_step_idempotently() {
        let f = Fixture::new();
        let fs = &f.fs;
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = {
            let log = std::sync::Arc::clone(&log);
            move |message: &str| log.lock().unwrap().push(message.to_owned())
        };
        let context = f.context(&sink);
        let steps = full_journal(&f);
        let prefix = f.dir.join("prefix");
        let container = f.dir.join("agent/extensions/rpi-todo");

        // The committed state of every step.
        let staged = f.dir.join("agent/extensions/.staging/rpi-todo-0.2.0-7");
        std::fs::create_dir_all(staged.join("lib")).unwrap();
        std::fs::write(staged.join("rpi-extension.json"), "{}").unwrap();
        std::fs::rename(&staged, container.join("0.2.0")).unwrap();
        fs.write_atomic(
            &container.join("current.json"),
            br#"{"schemaVersion":1,"version":"0.2.0","path":"0.2.0"}"#,
        )
        .unwrap();
        std::fs::write(prefix.join("rpi.exe"), b"old exe").unwrap();
        std::fs::rename(prefix.join("rpi.exe"), prefix.join("rpi.exe.old-0.1.0")).unwrap();
        std::fs::write(prefix.join("rpi.exe"), b"new exe").unwrap();
        fs.write_atomic(&f.paths.manifest_path, br#"{"version":"0.2.0"}"#)
            .unwrap();

        assert!(undo_steps(&context, &steps).is_empty());
        assert_eq!(std::fs::read(prefix.join("rpi.exe")).unwrap(), b"old exe");
        assert!(!prefix.join("rpi.exe.old-0.1.0").exists());
        assert!(
            !container.join("current.json").exists(),
            "a pointer that did not exist is removed"
        );
        assert_eq!(
            std::fs::read(&f.paths.manifest_path).unwrap(),
            br#"{"version":"0.1.0"}"#
        );
        assert!(
            !container.join("0.2.0").exists(),
            "the placed generation is removed (nothing referenced it yet)"
        );
        assert!(!staged.exists());

        // Running the same undo again changes nothing and reports no failure.
        assert!(undo_steps(&context, &steps).is_empty());
        assert_eq!(std::fs::read(prefix.join("rpi.exe")).unwrap(), b"old exe");

        // A pointer restored by someone else to a third text is left alone.
        fs.write_atomic(&container.join("current.json"), br#"{"someone":"else"}"#)
            .unwrap();
        undo_step(&context, &steps[1]).unwrap();
        assert_eq!(
            std::fs::read(container.join("current.json")).unwrap(),
            br#"{"someone":"else"}"#
        );
    }

    #[test]
    fn undo_of_a_half_done_swap_restores_the_executable() {
        let f = Fixture::new();
        let prefix = f.dir.join("prefix");
        std::fs::write(prefix.join("rpi.exe"), b"old exe").unwrap();
        // Only the first rename happened.
        std::fs::rename(prefix.join("rpi.exe"), prefix.join("rpi.exe.old-0.1.0")).unwrap();
        assert!(!prefix.join("rpi.exe").exists());
        undo_step(
            &f.context(&|_: &str| {}),
            &JournalStep::ExeSwap {
                old_name: "rpi.exe.old-0.1.0".to_owned(),
            },
        )
        .unwrap();
        assert_eq!(std::fs::read(prefix.join("rpi.exe")).unwrap(), b"old exe");

        // Restored externally (install.ps1 preflight): nothing to do.
        undo_step(
            &f.context(&|_: &str| {}),
            &JournalStep::ExeSwap {
                old_name: "rpi.exe.old-0.1.0".to_owned(),
            },
        )
        .unwrap();
        assert_eq!(std::fs::read(prefix.join("rpi.exe")).unwrap(), b"old exe");
    }

    #[test]
    fn rollback_uncommitted_undoes_and_removes_the_journal() {
        let f = Fixture::new();
        let journal_path = f.paths.journal_path();
        create(&f.fs, &journal_path).unwrap();
        append(
            &f.fs,
            &journal_path,
            JournalStep::WriteManifest {
                previous_text: Some(r#"{"version":"0.1.0"}"#.to_owned()),
            },
        )
        .unwrap();
        f.fs.write_atomic(&f.paths.manifest_path, br#"{"version":"0.2.0"}"#)
            .unwrap();
        let logged = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = {
            let logged = std::sync::Arc::clone(&logged);
            move |message: &str| logged.lock().unwrap().push(message.to_owned())
        };
        assert!(rollback_uncommitted(&f.context(&sink)).is_empty());
        assert!(load(&f.fs, &journal_path).is_none());
        assert_eq!(
            std::fs::read(&f.paths.manifest_path).unwrap(),
            br#"{"version":"0.1.0"}"#
        );
        assert!(
            logged
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains("1 step(s)")),
            "{:?}",
            logged.lock().unwrap()
        );
        // Without a journal it is a no-op.
        assert!(rollback_uncommitted(&f.context(&sink)).is_empty());
    }

    #[test]
    fn a_failing_undo_is_logged_and_the_loop_continues() {
        let f = Fixture::new();
        let prefix = f.dir.join("prefix");
        std::fs::write(prefix.join("rpi.exe"), b"old exe").unwrap();
        std::fs::rename(prefix.join("rpi.exe"), prefix.join("rpi.exe.old-0.1.0")).unwrap();
        // Inject a failure on the swap undo only.
        f.fs.fail(
            crate::core::windows_update::fs::FsOp::Rename,
            "prefix/rpi.exe",
            1,
        );
        let logged = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = {
            let logged = std::sync::Arc::clone(&logged);
            move |message: &str| logged.lock().unwrap().push(message.to_owned())
        };
        let steps = vec![
            JournalStep::ExeSwap {
                old_name: "rpi.exe.old-0.1.0".to_owned(),
            },
            JournalStep::WriteManifest {
                previous_text: None,
            },
        ];
        let failures = undo_steps(&f.context(&sink), &steps);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("exeSwap"));
        assert!(
            logged
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains("undo failed (continuing)")),
            "{:?}",
            logged.lock().unwrap()
        );
        assert!(
            !f.paths.manifest_path.exists(),
            "the later undo (manifest) still ran"
        );
    }
}
