//! External editor flow (T12-S6, group B).
//!
//! Upstream: `packages/coding-agent/src/modes/interactive/external-editor.ts`
//! @ pi 0.82.1 (2efa728) — full 45-line port; the caller-side glue mirrors
//! `handleOpenExternalEditor` (interactive-mode.ts:3846-3866).

use std::path::Path;
use std::sync::Mutex;

use crate::modes::interactive::interactive_mode::InteractiveUi;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl InteractiveUi {
    /// `handleOpenExternalEditor` (interactive-mode.ts:3846-3866): write the
    /// editor text to a temp `prompt.md`, stop the TUI, launch the external
    /// editor (settings `externalEditor` → `$VISUAL` → `$EDITOR` → nano /
    /// notepad), then restore the editor text on a clean exit and restart
    /// the TUI with a forced full re-render.
    ///
    /// Blocking note: upstream uses `spawn` + a `close` callback
    /// (external-editor.ts:24-31) so the Node event loop keeps running while
    /// the editor owns the terminal. Here the TUI is stopped for the
    /// editor's whole lifetime — no input or events can be processed
    /// anyway — so the calling (driver) thread blocks on `wait()` instead.
    /// `spawn` + `wait` is still used rather than `spawnSync` because the
    /// editor runs in a real child process with the terminal handed over
    /// (upstream's Windows note, external-editor.ts:21-23, applies to
    /// console-input races; the TUI stop avoids them here).
    pub(crate) fn handle_open_external_editor_real(&self) {
        let command = self
            .session()
            .settings_manager(|settings| settings.get_external_editor_command());
        let content = lock(&self.editor).get_expanded_text();
        let editor_parts: Vec<&str> = command.split_whitespace().collect();
        let (program, editor_args) = match editor_parts.split_first() {
            Some((program, args)) => (*program, args),
            None => {
                self.show_status("No external editor configured");
                return;
            }
        };

        // Stop the TUI so the terminal is released to the editor
        // (interactive-mode.ts:3849). The editor handle stays reachable —
        // stopping does not destroy the component tree.
        self.ui.stop();

        // Temp dir `pi-editor-{pid}/prompt.md` (external-editor.ts:14-17;
        // pid-scoped instead of mkdtemp so the path is reproducible).
        let dir = std::env::temp_dir().join(format!("pi-editor-{}", std::process::id()));
        let file_path = dir.join("prompt.md");
        let prepare = (|| -> std::io::Result<()> {
            let _ = std::fs::remove_dir_all(&dir); // stale dir from a crashed run
            std::fs::create_dir_all(&dir)?;
            std::fs::write(&file_path, content)?;
            Ok(())
        })();
        if let Err(error) = prepare {
            self.show_status(&format!("Failed to prepare editor file: {error}"));
            self.resume_after_external_editor();
            return;
        }

        // external-editor.ts:19-20 — printed while the TUI is stopped, so it
        // lands on the live terminal instead of the alternate screen.
        println!("Launching external editor: {command}\nPi will resume when the editor exits.");

        let exit_code = spawn_and_wait(program, editor_args, &file_path);

        if let Some(code) = exit_code {
            if code == 0 {
                // Read back the edited text, stripping the single trailing
                // newline (external-editor.ts:37 `replace(/\n$/, "")`).
                match std::fs::read_to_string(&file_path)
                    .map(|text| text.strip_suffix('\n').unwrap_or(&text).to_string())
                {
                    Ok(text) => lock(&self.editor).set_text(&text),
                    Err(error) => {
                        self.show_status(&format!("Failed to read editor output: {error}"))
                    }
                }
            } else {
                // Non-zero exit: discard the edit (external-editor.ts:33-35).
                self.show_status(&format!(
                    "External editor exited with code {code}; changes discarded"
                ));
            }
        } else {
            self.show_status(&format!("Failed to launch external editor: {program}"));
        }

        self.resume_after_external_editor();
    }

    /// Shared finally-path (interactive-mode.ts:3858-3861): best-effort temp
    /// cleanup (external-editor.ts:39-43), restart the TUI, force a full
    /// re-render.
    fn resume_after_external_editor(&self) {
        let dir = std::env::temp_dir().join(format!("pi-editor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        self.ui.start();
        self.ui.request_render(true);
    }
}

/// Spawn the editor with `file_path` appended and block for its exit code
/// (external-editor.ts:24-31). `None` means the process could not be spawned
/// (upstream's `error` event resolves the promise with `null`).
fn spawn_and_wait(program: &str, args: &[&str], file_path: &Path) -> Option<i32> {
    #[cfg(windows)]
    {
        // Upstream runs through the shell on Windows (external-editor.ts:27).
        let command_line = format!(
            "\"{program}\" {args} \"{file}\"",
            args = args.join(" "),
            file = file_path.display()
        );
        let mut child = std::process::Command::new("cmd")
            .arg("/C")
            .arg(command_line)
            .spawn()
            .ok()?;
        child.wait().ok().map(|status| status.code().unwrap_or(1))
    }
    #[cfg(not(windows))]
    {
        let mut child = std::process::Command::new(program)
            .args(args)
            .arg(file_path)
            .spawn()
            .ok()?;
        child.wait().ok().map(|status| status.code().unwrap_or(1))
    }
}

// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::modes::interactive::interactive_mode::{InteractiveMode, InteractiveModeOptions};
    use crate::modes::interactive::test_support::{
        build_test_session, TempDir, TestSession, TestTerminal,
    };
    use pir_tui::tui::Component;

    /// Serializes `$VISUAL` mutation (process-global env) against the other
    /// editor tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Write an executable fake-editor script that runs `body` with `$1` set
    /// to the prompt file path.
    fn fake_editor_script(dir: &TempDir, body: &str) -> std::path::PathBuf {
        let script = dir.path().join("fake-editor.sh");
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).expect("write fake editor");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake editor");
        }
        script
    }

    async fn harness_with_editor(
        script: &std::path::Path,
    ) -> (InteractiveMode, Arc<TestTerminal>, TempDir, EnvRestore) {
        let terminal = Arc::new(TestTerminal::new());
        let harness = build_test_session().await;
        let TestSession { _tmp, runtime, .. } = harness;
        let mode = InteractiveMode::with_terminal(
            runtime,
            InteractiveModeOptions::default(),
            Box::new(TestTerminal::clone(&terminal)),
        );
        // `getExternalEditorCommand` resolves `$VISUAL` before the platform
        // default (settings-manager.ts:854-864). The restore guard lives in
        // the returned tuple so `$VISUAL` stays set for the whole test.
        let previous = std::env::var("VISUAL").ok();
        std::env::set_var("VISUAL", script.display().to_string());
        (mode, terminal, _tmp, EnvRestore { previous })
    }

    struct EnvRestore {
        previous: Option<String>,
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var("VISUAL", value),
                None => std::env::remove_var("VISUAL"),
            }
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn external_editor_writes_edited_text_and_cleans_up() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new();
        let script = fake_editor_script(&tmp, "echo edited > \"$1\"");
        let (mode, terminal, _tmp_keep, _restore) = harness_with_editor(&script).await;
        let ui = &mode.ui_state;
        lock(&ui.editor).set_text("original");

        ui.handle_open_external_editor_real();

        // Editor text replaced by the editor's output (trailing newline
        // stripped, external-editor.ts:37).
        assert_eq!(lock(&ui.editor).get_text(), "edited");
        // Temp dir removed (external-editor.ts:39-43).
        let dir = std::env::temp_dir().join(format!("pi-editor-{}", std::process::id()));
        assert!(!dir.exists(), "temp editor dir must be cleaned up");
        // TUI restarted after the editor exits (interactive-mode.ts:3859).
        assert!(terminal.is_started(), "TUI must be restarted");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn external_editor_nonzero_exit_discards_changes() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new();
        let script = fake_editor_script(&tmp, "echo partial > \"$1\"\nexit 3");
        let (mode, terminal, _tmp_keep, _restore) = harness_with_editor(&script).await;
        let ui = &mode.ui_state;
        lock(&ui.editor).set_text("keep me");

        ui.handle_open_external_editor_real();

        // Non-zero exit: the edit is discarded and a status message is shown.
        assert_eq!(lock(&ui.editor).get_text(), "keep me");
        let rendered = lock(&ui.chat_container).render(60).join("\n");
        assert!(
            rendered.contains("exited with code 3"),
            "status must mention the exit code: {rendered}"
        );
        assert!(terminal.is_started(), "TUI must be restarted");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn external_editor_spawn_failure_reports_and_restarts() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new();
        let missing = tmp.path().join("no-such-editor.sh");
        let (mode, terminal, _tmp_keep, _restore) = harness_with_editor(&missing).await;
        let ui = &mode.ui_state;
        lock(&ui.editor).set_text("untouched");

        ui.handle_open_external_editor_real();

        assert_eq!(lock(&ui.editor).get_text(), "untouched");
        let rendered = lock(&ui.chat_container).render(60).join("\n");
        assert!(
            rendered.contains("Failed to launch external editor"),
            "status must report the spawn failure: {rendered}"
        );
        assert!(terminal.is_started(), "TUI must be restarted");
        let dir = std::env::temp_dir().join(format!("pi-editor-{}", std::process::id()));
        assert!(!dir.exists(), "temp editor dir must be cleaned up");
    }
}
