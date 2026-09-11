//! External editor flow (T12-S6, group B).
//!
//! Upstream: `packages/coding-agent/src/modes/interactive/external-editor.ts`
//! @ pi 0.82.1 (2efa728) — full 45-line port; the caller-side glue mirrors
//! `handleOpenExternalEditor` (interactive-mode.ts:3846-3866).

use std::path::Path;
use std::sync::{Arc, Mutex};

use rpi_ext_host::interactive_ui::{InteractiveUiError, InteractiveUiErrorKind};
use rpi_tui::tui::TuiStopOptions;

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
        self.ui.stop(TuiStopOptions::default());

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

    /// `ui.editExternal` (R-U11 / V14-23 C3): the interactive-UI-ABI variant
    /// of the host external-editor flow. Resolves the configured editor
    /// (same chain as `Ctrl+G`: settings `externalEditor` → `$VISUAL` →
    /// `$EDITOR` → platform default), then edits `text` in a 0600 temp file.
    /// Returns the edited text, or `None` when the user cancelled (editor
    /// exited non-zero — the `:cq`/Ctrl+C contract of the prompt flow).
    /// No configuration / launch / prepare failures are structured errors
    /// that never block the TUI (R-U11.2).
    ///
    /// Runs the blocking editor wait on `spawn_blocking`: the host-call
    /// parks the CALLING guest thread (`block_on`), never a runtime worker.
    pub(crate) async fn edit_text_external(
        self: &Arc<Self>,
        text: &str,
        language: Option<&str>,
    ) -> Result<Option<String>, InteractiveUiError> {
        let command = self
            .session()
            .settings_manager(|settings| settings.get_external_editor_command());
        let text = text.to_owned();
        let language = language.map(str::to_owned);
        let ui = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            ui.edit_text_external_blocking(&command, &text, language.as_deref())
        })
        .await
        .map_err(|error| {
            InteractiveUiError::new(
                InteractiveUiErrorKind::Call,
                format!("editExternal: editor task failed: {error}"),
            )
        })?
    }

    /// Blocking body of [`InteractiveUi::edit_text_external`] (runs off the
    /// runtime on `spawn_blocking`). Same shape as
    /// [`InteractiveUi::handle_open_external_editor_real`]: stop TUI → edit
    /// → read back / cancel / structured error → cleanup + restart TUI.
    fn edit_text_external_blocking(
        &self,
        command: &str,
        text: &str,
        language: Option<&str>,
    ) -> Result<Option<String>, InteractiveUiError> {
        let call_error =
            |message: String| InteractiveUiError::new(InteractiveUiErrorKind::Call, message);
        let editor_parts: Vec<&str> = command.split_whitespace().collect();
        let Some((program, editor_args)) = editor_parts.split_first() else {
            // R-U11.2 无配置: rpi's resolver always falls back to a platform
            // default, so this arm is defensive (an explicitly blank
            // `externalEditor` with no $VISUAL/$EDITOR and a future resolver
            // change would land here) — still a structured error, never a
            // blocked TUI.
            return Err(call_error(
                "editExternal: no external editor configured".to_owned(),
            ));
        };

        // Per-call unique dir (two extensions may edit concurrently) with a
        // 0600 file (R-U11.2: 临时文件 0600、用后清理).
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("rpi-edit-{}-{unique}", std::process::id()));
        let file_path = dir.join(edit_file_name(language));
        let prepare = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(&dir)?;
            std::fs::write(&file_path, text)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&file_path, std::fs::Permissions::from_mode(0o600))?;
            }
            Ok(())
        })();
        if let Err(error) = prepare {
            return Err(call_error(format!(
                "editExternal: failed to prepare editor file: {error}"
            )));
        }

        // Stop the TUI so the terminal is released to the editor
        // (same contract as the prompt flow, interactive-mode.ts:3849).
        self.ui.stop(TuiStopOptions::default());
        // Printed while the TUI is stopped, so it lands on the live terminal
        // (external-editor.ts:19-20 shape).
        println!("Launching external editor: {command}\nPi will resume when the editor exits.");

        let exit_code = spawn_and_wait(program, editor_args, &file_path);
        let result = match exit_code {
            Some(0) => std::fs::read_to_string(&file_path)
                .map(|edited| Some(edited.strip_suffix('\n').unwrap_or(&edited).to_string()))
                .map_err(|error| {
                    call_error(format!("editExternal: failed to read edited text: {error}"))
                }),
            // Non-zero exit = user cancel (external-editor.ts:33-35 discard
            // branch): `{text: null}` per R-U11.1, the TUI is restored below.
            Some(_) => Ok(None),
            None => Err(call_error(format!(
                "editExternal: failed to launch external editor: {program}"
            ))),
        };

        // Shared finally-path: cleanup + TUI restart on every outcome.
        let _ = std::fs::remove_dir_all(&dir);
        self.ui.start();
        self.ui.request_render(true);
        result
    }
}

/// Temp-file name for a `ui.editExternal` call: the sanitized `language` as
/// the extension (editor syntax highlighting; advisory only), `txt` fallback.
fn edit_file_name(language: Option<&str>) -> String {
    let extension = language
        .map(str::to_ascii_lowercase)
        .filter(|language| {
            !language.is_empty()
                && language.len() <= 8
                && language.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
        .unwrap_or_else(|| "txt".to_owned());
    format!("edit.{extension}")
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
    use rpi_tui::tui::Component;

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

    // =========================================================================
    // `ui.editExternal` (V14-23 C3, R-U11): the ABI variant of the flow —
    // 0600 temp file, editor exit codes, structured errors, TUI restore.
    // =========================================================================

    /// Bare mode harness (no `$VISUAL` juggling): the blocking body takes
    /// the editor command as a parameter, so most cases inject it directly.
    async fn mode_harness_bare() -> (InteractiveMode, Arc<TestTerminal>) {
        let terminal = Arc::new(TestTerminal::new());
        let harness = build_test_session().await;
        let TestSession { _tmp, runtime, .. } = harness;
        let mode = InteractiveMode::with_terminal(
            runtime,
            InteractiveModeOptions::default(),
            Box::new(TestTerminal::clone(&terminal)),
        );
        (mode, terminal)
    }

    /// No `rpi-edit-{pid}-*` temp dirs left behind (R-U11.2 用后清理).
    fn no_edit_temp_dirs_left() -> bool {
        let prefix = format!("rpi-edit-{}", std::process::id());
        std::fs::read_dir(std::env::temp_dir())
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .all(|entry| !entry.file_name().to_string_lossy().starts_with(&prefix))
            })
            .unwrap_or(true)
    }

    /// R-U11.1 success: edited text returned, trailing newline stripped,
    /// temp file 0600 (unix), language maps the file name, temp cleaned up,
    /// TUI restarted.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // temp-dir scan serialized via ENV_LOCK
    async fn edit_external_returns_edited_text_and_cleans_up() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (mode, terminal) = mode_harness_bare().await;
        let ui = &mode.ui_state;
        let tmp = TempDir::new();
        let mode_side = tmp.path().join("mode.txt");
        let name_side = tmp.path().join("name.txt");
        let script = fake_editor_script(
            &tmp,
            &format!(
                "stat -c %a \"$1\" > {}; basename \"$1\" > {}; echo edited > \"$1\"",
                mode_side.display(),
                name_side.display()
            ),
        );

        let result = ui
            .edit_text_external_blocking(
                script.to_str().unwrap_or_default(),
                "original",
                Some("markdown"),
            )
            .expect("edited");
        assert_eq!(result.as_deref(), Some("edited"));
        // TUI stopped for the editor and restarted afterwards.
        assert!(terminal.is_started(), "TUI must be restarted");
        // Temp dir removed after use (unique per call).
        assert!(no_edit_temp_dirs_left(), "temp edit dir must be cleaned up");
        // R-U11.2: the temp file was 0600 while it existed.
        #[cfg(unix)]
        {
            let mode_bits = std::fs::read_to_string(&mode_side).unwrap_or_default();
            assert_eq!(mode_bits.trim(), "600", "temp file must be 0600");
        }
        // The sanitized language named the file: lowercase ASCII-alnum
        // `markdown` (≤8 chars) is used verbatim as the extension.
        let file_name = std::fs::read_to_string(&name_side).unwrap_or_default();
        assert_eq!(file_name.trim(), "edit.markdown");
    }

    /// R-U11.1 cancel: a non-zero editor exit resolves `{text: null}` — same
    /// discard contract as the prompt flow (external-editor.ts:33-35).
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // temp-dir scan serialized via ENV_LOCK
    async fn edit_external_cancelled_returns_none() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (mode, terminal) = mode_harness_bare().await;
        let ui = &mode.ui_state;
        let tmp = TempDir::new();
        let script = fake_editor_script(&tmp, "echo partial > \"$1\"\nexit 3");

        let result = ui
            .edit_text_external_blocking(script.to_str().unwrap_or_default(), "keep me", None)
            .expect("cancel is not an error");
        assert_eq!(result, None);
        assert!(terminal.is_started(), "TUI must be restarted after cancel");
        assert!(no_edit_temp_dirs_left());
    }

    /// R-U11.2 无配置: a blank resolved command is a structured `call` error
    /// and never stops the TUI (rpi's resolver always falls back to a
    /// platform default, so this arm is defensive).
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // temp-dir scan serialized via ENV_LOCK
    async fn edit_external_no_config_is_structured_error() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (mode, terminal) = mode_harness_bare().await;
        let ui = &mode.ui_state;

        let error = ui
            .edit_text_external_blocking("   ", "draft", None)
            .expect_err("no config");
        assert_eq!(
            error.kind,
            rpi_ext_host::interactive_ui::InteractiveUiErrorKind::Call
        );
        assert!(
            error.message.contains("no external editor configured"),
            "{error}"
        );
        // The failure path returns before the TUI is ever stopped, so the
        // resume `start` never runs either (the harness TUI starts out
        // stopped; a stop→start roundtrip would leave it started).
        assert!(
            !terminal.is_started(),
            "no-config must not run the TUI stop/start roundtrip"
        );
        assert!(no_edit_temp_dirs_left());
    }

    /// R-U11.2 启动失败: a spawn failure is a structured `call` error; the
    /// TUI is restored (stop → start roundtrip), not blocked.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // temp-dir scan serialized via ENV_LOCK
    async fn edit_external_spawn_failure_is_structured_error() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (mode, terminal) = mode_harness_bare().await;
        let ui = &mode.ui_state;
        let tmp = TempDir::new();
        let missing = tmp.path().join("definitely-missing-editor.sh");

        let error = ui
            .edit_text_external_blocking(missing.to_str().unwrap_or_default(), "draft", None)
            .expect_err("spawn failure");
        assert_eq!(
            error.kind,
            rpi_ext_host::interactive_ui::InteractiveUiErrorKind::Call
        );
        assert!(
            error.message.contains("failed to launch external editor"),
            "{error}"
        );
        assert!(terminal.is_started(), "TUI must be restarted");
        assert!(no_edit_temp_dirs_left());
    }

    /// Full async path (`ui.editExternal` host-call shape): settings→env
    /// resolution + `spawn_blocking` editor wait through the real bridge
    /// surface (`edit_text_external`).
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test env-guard held across awaits
    async fn edit_external_full_path_resolves_configured_editor() {
        let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new();
        let script = fake_editor_script(&tmp, "echo from-visual > \"$1\"");
        let (mode, terminal, _tmp_keep, _restore) = harness_with_editor(&script).await;
        let ui = Arc::clone(&mode.ui_state);

        let result = ui.edit_text_external("draft", None).await.expect("edited");
        assert_eq!(result.as_deref(), Some("from-visual"));
        assert!(terminal.is_started(), "TUI must be restarted");
        assert!(no_edit_temp_dirs_left());
    }
}
