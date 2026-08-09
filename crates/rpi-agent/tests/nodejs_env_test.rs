//! Port of `external/pi/packages/agent/test/harness/nodejs-env.test.ts` @ 2efa728.
//!
//! Test-intent mapping notes:
//! - Platform-only entries: the win32-only "settles after the shell exits when
//!   a detached descendant retains inherited stdio" case is not ported (it
//!   needs a Windows `taskkill` cleanup path). The legacy-WSL-stdin case IS
//!   ported: upstream fakes `process.platform === "win32"` and chdirs + PATH
//!   patches the process; the Rust port does the same process-global changes
//!   under a guard, so the test is serialized with itself only. `pathExists`
//!   and the spawn resolve the relative `C:\Windows\System32\bash.exe` shell
//!   path against the process cwd, exactly like the upstream test.
//! - "executes commands in cwd with env overrides" uses an absolute cwd, so
//!   `$PWD` is comparable without a canonicalization step.
//! - Timeout test uses whole seconds (`timeout: 1`) — the harness types fix
//!   `ShellExecOptions.timeout` to `Option<u64>` seconds, so the upstream
//!   fractional `0.01` is not expressible.
//! - "cleanup terminates active shell processes" / "aborted commands" spawn
//!   the exec future on a tokio task (a Rust future is lazy; JS starts running
//!   the async function immediately).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rpi_agent::harness::env::nodejs::NodeExecutionEnv;
use rpi_agent::harness::types::{
    CreateDirOptions, CreateTempFileOptions, ExecutionErrorCode, FileErrorCode, FileKind,
    FileSystem, ReadTextLinesOptions, RemoveOptions, Shell, ShellExecOptions,
};
use rpi_agent::harness::utils::shell_output::{
    execute_shell_with_capture, sanitize_binary_output, ShellCaptureOptions,
};
use rpi_ai::utils::uuid::uuidv7;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Fresh temp directory removed on drop (upstream `createTempDir` from
/// session-test-utils). Each test owns one; `Drop` removes it, so repeated
/// runs do not accumulate directories under the system temp root.
struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let base = std::env::temp_dir();
        let base = std::fs::canonicalize(&base).unwrap_or(base);
        let dir = base.join(format!(
            "rpi-nodejs-env-test-{}-{}",
            std::process::id(),
            uuidv7()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TestDir(dir)
    }

    fn root(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `NodeExecutionEnv` rooted at `root` (upstream `createTempDir()` + `new
/// NodeExecutionEnv({ cwd: root })`).
fn env_at(root: &Path) -> NodeExecutionEnv {
    NodeExecutionEnv::new(root.to_string_lossy().into_owned())
}

/// Minimal `pathToFileURL` for POSIX paths (the temp paths only contain hex,
/// dashes and spaces, so `%20` is the only encoding needed).
fn file_url(path: &Path) -> String {
    format!("file://{}", path.to_string_lossy().replace(' ', "%20"))
}

/// Restores the process cwd and PATH (upstream test's chdir + PATH patch,
/// nodejs-env.test.ts:293-316). Process-global state: only used by the WSL
/// stdin test, which changes PATH to a superset so concurrent spawns are
/// unaffected.
struct ProcessEnvGuard {
    original_cwd: PathBuf,
    original_path: Option<OsString>,
}

impl ProcessEnvGuard {
    fn enter(root: &Path) -> Self {
        let original_cwd = std::env::current_dir().expect("current dir");
        let original_path = std::env::var_os("PATH");
        std::env::set_current_dir(root).expect("chdir");
        let mut new_path = root.as_os_str().to_os_string();
        new_path.push(":");
        if let Some(path) = &original_path {
            new_path.push(path);
        }
        std::env::set_var("PATH", new_path);
        ProcessEnvGuard {
            original_cwd,
            original_path,
        }
    }
}

impl Drop for ProcessEnvGuard {
    fn drop(&mut self) {
        match &self.original_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
        let _ = std::env::set_current_dir(&self.original_cwd);
    }
}

// ---------------------------------------------------------------------------
// File operations (nodejs-env.test.ts:71-249)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_reads_writes_lists_removes_files_and_dirs() {
    let root = TestDir::new();
    let env = env_at(root.root());
    assert_eq!(
        env.absolute_path("nested/child", None).await.unwrap(),
        root.root().join("nested/child").to_string_lossy()
    );
    assert_eq!(
        env.join_path(
            &[
                root.root().to_string_lossy().into_owned(),
                "nested".into(),
                "child".into()
            ],
            None
        )
        .await
        .unwrap(),
        root.root().join("nested/child").to_string_lossy()
    );
    env.create_dir("nested/child", CreateDirOptions::default())
        .await
        .unwrap();
    env.write_file("nested/child/file.txt", b"hel", None)
        .await
        .unwrap();
    env.append_file("nested/child/file.txt", b"lo", None)
        .await
        .unwrap();
    assert_eq!(
        env.read_text_file("nested/child/file.txt", None)
            .await
            .unwrap(),
        "hello"
    );
    assert_eq!(
        env.read_text_lines(
            "nested/child/file.txt",
            ReadTextLinesOptions {
                max_lines: Some(1),
                ..Default::default()
            }
        )
        .await
        .unwrap(),
        vec!["hello"]
    );
    assert_eq!(
        env.read_binary_file("nested/child/file.txt", None)
            .await
            .unwrap(),
        b"hello".to_vec()
    );

    let entries = env.list_dir("nested/child", None).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "file.txt");
    assert_eq!(
        entries[0].path,
        root.root().join("nested/child/file.txt").to_string_lossy()
    );
    assert_eq!(entries[0].kind, FileKind::File);
    assert_eq!(entries[0].size, 5);
    assert!(entries[0].mtime_ms.is_finite() && entries[0].mtime_ms > 0.0);

    assert!(env.exists("nested/child/file.txt", None).await.unwrap());
    env.remove("nested/child/file.txt", RemoveOptions::default())
        .await
        .unwrap();
    assert!(!env.exists("nested/child/file.txt", None).await.unwrap());
}

#[tokio::test]
async fn test_expands_home_relative_paths_and_file_urls() {
    let root = TestDir::new();
    let env = env_at(root.root());
    let home = std::env::var_os("HOME").expect("HOME must be set");
    assert_eq!(
        env.absolute_path("~/pi-node-env-test", None).await.unwrap(),
        PathBuf::from(home)
            .join("pi-node-env-test")
            .to_string_lossy()
    );
    let file_path = root.root().join("file with spaces.txt");
    assert_eq!(
        env.absolute_path(&file_url(&file_path), None)
            .await
            .unwrap(),
        file_path.to_string_lossy()
    );
}

#[tokio::test]
async fn test_file_info_for_files_dirs_symlinks_without_following() {
    let root = TestDir::new();
    let env = env_at(root.root());
    env.create_dir("dir", CreateDirOptions::default())
        .await
        .unwrap();
    env.write_file("dir/file.txt", b"hello", None)
        .await
        .unwrap();
    std::os::unix::fs::symlink(
        root.root().join("dir/file.txt"),
        root.root().join("file-link"),
    )
    .unwrap();
    std::os::unix::fs::symlink(root.root().join("dir"), root.root().join("dir-link")).unwrap();

    let info = env.file_info("dir", None).await.unwrap();
    assert_eq!(info.name, "dir");
    assert_eq!(info.path, root.root().join("dir").to_string_lossy());
    assert_eq!(info.kind, FileKind::Directory);

    let info = env.file_info("dir/file.txt", None).await.unwrap();
    assert_eq!(info.name, "file.txt");
    assert_eq!(info.kind, FileKind::File);
    assert_eq!(info.size, 5);

    assert_eq!(
        env.file_info("file-link", None).await.unwrap().kind,
        FileKind::Symlink
    );
    assert_eq!(
        env.file_info("dir-link", None).await.unwrap().kind,
        FileKind::Symlink
    );
    assert_eq!(
        env.canonical_path("file-link", None).await.unwrap(),
        std::fs::canonicalize(root.root().join("dir/file.txt"))
            .unwrap()
            .to_string_lossy()
    );
}

#[tokio::test]
async fn test_lists_symlinks_as_symlinks() {
    let root = TestDir::new();
    let env = env_at(root.root());
    env.write_file("target.txt", b"hello", None).await.unwrap();
    std::os::unix::fs::symlink(root.root().join("target.txt"), root.root().join("link.txt"))
        .unwrap();

    let mut entries = env.list_dir(".", None).await.unwrap();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let kinds: Vec<(String, FileKind)> = entries
        .iter()
        .map(|entry| (entry.name.clone(), entry.kind))
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("link.txt".to_string(), FileKind::Symlink),
            ("target.txt".to_string(), FileKind::File),
        ]
    );
}

#[tokio::test]
async fn test_read_text_lines_stops_at_requested_limit() {
    let root = TestDir::new();
    let env = env_at(root.root());
    env.write_file("file.txt", b"one\ntwo\nthree", None)
        .await
        .unwrap();
    assert_eq!(
        env.read_text_lines(
            "file.txt",
            ReadTextLinesOptions {
                max_lines: Some(1),
                ..Default::default()
            }
        )
        .await
        .unwrap(),
        vec!["one"]
    );
    // `crlfDelay: Infinity` semantics: `\r` is not a line break.
    env.write_file("crlf.txt", b"a\r\nb\n", None).await.unwrap();
    assert_eq!(
        env.read_text_lines("crlf.txt", ReadTextLinesOptions::default())
            .await
            .unwrap(),
        vec!["a\r", "b"]
    );
    // A final unterminated line is emitted at EOF; an empty file yields no
    // lines (upstream readline `for await` over an empty stream).
    env.write_file("partial.txt", b"one", None).await.unwrap();
    assert_eq!(
        env.read_text_lines("partial.txt", ReadTextLinesOptions::default())
            .await
            .unwrap(),
        vec!["one"]
    );
    env.write_file("empty.txt", b"", None).await.unwrap();
    assert_eq!(
        env.read_text_lines("empty.txt", ReadTextLinesOptions::default())
            .await
            .unwrap(),
        Vec::<String>::new()
    );
}

/// `maxLines` on a large file returns only the requested prefix — functional
/// equivalence for the chunked read (the IO saving itself is asserted by
/// `test_read_text_lines_max_lines_does_not_wait_for_eof`).
#[tokio::test]
async fn test_read_text_lines_max_lines_on_large_file() {
    let root = TestDir::new();
    let env = env_at(root.root());
    let mut content = String::with_capacity(10 * 1024 * 1024 + 16);
    for index in 0..100_000 {
        content.push_str(&format!("line {index}\n"));
    }
    env.write_file("large.txt", content.as_bytes(), None)
        .await
        .unwrap();
    assert_eq!(
        env.read_text_lines(
            "large.txt",
            ReadTextLinesOptions {
                max_lines: Some(1),
                ..Default::default()
            }
        )
        .await
        .unwrap(),
        vec!["line 0"]
    );
    assert_eq!(
        env.read_text_lines(
            "large.txt",
            ReadTextLinesOptions {
                max_lines: Some(3),
                ..Default::default()
            }
        )
        .await
        .unwrap(),
        vec!["line 0", "line 1", "line 2"]
    );
}

/// `maxLines` stops the read before EOF: the reader must return the first
/// line while the FIFO writer still holds the write end open. The old
/// whole-file read blocks until the writer closes (and would return
/// `["one", "two"]` instead of `["one"]`).
#[cfg(unix)]
#[tokio::test]
async fn test_read_text_lines_max_lines_does_not_wait_for_eof() {
    let root = TestDir::new();
    let env = env_at(root.root());
    let fifo = root.root().join("stream.txt");
    let fifo_c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).expect("CString");
    let mkfifo_result = unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) };
    assert_eq!(mkfifo_result, 0, "mkfifo failed");

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let writer = std::thread::spawn({
        let fifo = fifo.clone();
        move || {
            use std::io::Write;
            // Opening the write end unblocks the reader's open; both sides
            // proceed once the pair is established.
            let mut handle = std::fs::OpenOptions::new()
                .write(true)
                .open(&fifo)
                .expect("open fifo for writing");
            handle.write_all(b"one\n").expect("write first line");
            // Hold the write end open: no EOF until the test signals.
            let _ = rx.recv();
        }
    });

    let lines = tokio::time::timeout(
        Duration::from_secs(5),
        env.read_text_lines(
            "stream.txt",
            ReadTextLinesOptions {
                max_lines: Some(1),
                ..Default::default()
            },
        ),
    )
    .await
    .expect("max_lines=1 must not wait for EOF")
    .expect("read");
    tx.send(()).expect("signal writer to close");
    writer.join().expect("writer thread");

    assert_eq!(lines, vec!["one"]);
}

#[tokio::test]
async fn test_file_error_for_missing_paths_and_exists_false() {
    let root = TestDir::new();
    let env = env_at(root.root());
    let error = env.file_info("missing.txt", None).await.unwrap_err();
    assert_eq!(error.code, FileErrorCode::NotFound);
    assert!(!env.exists("missing.txt", None).await.unwrap());
}

#[tokio::test]
async fn test_file_error_for_listing_non_directories() {
    let root = TestDir::new();
    let env = env_at(root.root());
    env.write_file("file.txt", b"hello", None).await.unwrap();
    let error = env.list_dir("file.txt", None).await.unwrap_err();
    assert_eq!(error.code, FileErrorCode::NotDirectory);
}

#[tokio::test]
async fn test_appends_to_new_files_and_creates_parent_dirs() {
    let root = TestDir::new();
    let env = env_at(root.root());
    env.append_file("new/nested/file.txt", b"a", None)
        .await
        .unwrap();
    env.append_file("new/nested/file.txt", b"b", None)
        .await
        .unwrap();
    assert_eq!(
        env.read_text_file("new/nested/file.txt", None)
            .await
            .unwrap(),
        "ab"
    );
}

#[tokio::test]
async fn test_creates_temp_dirs_and_files() {
    let root = TestDir::new();
    let env = env_at(root.root());
    let temp_dir = env
        .create_temp_dir(Some("node-env-test-"), None)
        .await
        .unwrap();
    assert!(Path::new(&temp_dir).exists());
    assert!(Path::new(&temp_dir).is_dir());
    let temp_file = env
        .create_temp_file(CreateTempFileOptions {
            prefix: Some("prefix-".to_string()),
            suffix: Some(".txt".to_string()),
            abort_signal: None,
        })
        .await
        .unwrap();
    assert!(Path::new(&temp_file).exists());
    assert!(temp_file.ends_with(".txt"));
    assert!(temp_file.contains("prefix-"));
}

#[tokio::test]
async fn test_honors_create_dir_recursive_false_and_remove_options() {
    let root = TestDir::new();
    let env = env_at(root.root());
    let error = env
        .create_dir(
            "missing/child",
            CreateDirOptions {
                recursive: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, FileErrorCode::NotFound);

    env.write_file("dir/child/file.txt", b"hello", None)
        .await
        .unwrap();
    let error = env
        .remove(
            "dir",
            RemoveOptions {
                recursive: false,
                force: false,
                abort_signal: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, FileErrorCode::IsDirectory);
    env.remove(
        "dir",
        RemoveOptions {
            recursive: true,
            force: false,
            abort_signal: None,
        },
    )
    .await
    .unwrap();
    assert!(!env.exists("dir", None).await.unwrap());

    let error = env
        .remove(
            "missing",
            RemoveOptions {
                recursive: false,
                force: false,
                abort_signal: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, FileErrorCode::NotFound);
    env.remove(
        "missing",
        RemoveOptions {
            recursive: false,
            force: true,
            abort_signal: None,
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn test_aborted_results_for_pre_aborted_cancellable_ops() {
    let root = TestDir::new();
    let env = env_at(root.root());
    env.write_file("file.txt", b"hello", None).await.unwrap();
    let signal = CancellationToken::new();
    signal.cancel();

    let results = vec![
        env.read_text_file("file.txt", Some(signal.clone()))
            .await
            .unwrap_err()
            .code,
        env.read_text_lines(
            "file.txt",
            ReadTextLinesOptions {
                max_lines: None,
                abort_signal: Some(signal.clone()),
            },
        )
        .await
        .unwrap_err()
        .code,
        env.read_binary_file("file.txt", Some(signal.clone()))
            .await
            .unwrap_err()
            .code,
        env.write_file("other.txt", b"hello", Some(signal.clone()))
            .await
            .unwrap_err()
            .code,
        env.list_dir(".", Some(signal.clone()))
            .await
            .unwrap_err()
            .code,
    ];
    for code in results {
        assert_eq!(code, FileErrorCode::Aborted);
    }
}

#[tokio::test]
async fn test_cleanup_is_best_effort() {
    let root = TestDir::new();
    let env = env_at(root.root());
    FileSystem::cleanup(&env).await;
}

// ---------------------------------------------------------------------------
// exec (nodejs-env.test.ts:251-437)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_executes_commands_in_cwd_with_env_overrides() {
    let root = TestDir::new();
    let env = env_at(root.root());
    let mut extra_env = BTreeMap::new();
    extra_env.insert("NODE_ENV_TEST".to_string(), "ok".to_string());
    let result = env
        .exec(
            "printf '%s:%s' \"$PWD\" \"$NODE_ENV_TEST\"",
            Some(ShellExecOptions {
                env: Some(extra_env),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    let expected_cwd = root.root().to_string_lossy();
    assert_eq!(result.stdout, format!("{expected_cwd}:ok"));
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 0);
}

#[tokio::test]
async fn test_exec_can_replace_instead_of_inherit_default_env() {
    let root = TestDir::new();
    let mut shell_env = BTreeMap::new();
    shell_env.insert(
        "PI_NODE_ENV_CONFIGURED_TEST".to_string(),
        "configured".to_string(),
    );
    let env = env_at(root.root()).with_shell_env(shell_env);
    let mut explicit_env = BTreeMap::new();
    explicit_env.insert(
        "PI_NODE_ENV_EXPLICIT_TEST".to_string(),
        "explicit".to_string(),
    );
    let result = env
        .exec(
            "printf '%s:%s:%s' \"${PI_NODE_ENV_INHERITED_TEST-}\" \"${PI_NODE_ENV_CONFIGURED_TEST-}\" \"${PI_NODE_ENV_EXPLICIT_TEST-}\"",
            Some(ShellExecOptions {
                inherit_env: Some(false),
                env: Some(explicit_env),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    assert_eq!(result.stdout, "::explicit");
}

#[tokio::test]
async fn test_uses_stdin_command_transport_for_legacy_wsl_bash_paths() {
    // Upstream fakes `process.platform === "win32"` and points `shellPath` at
    // `C:\Windows\System32\bash.exe` relative to the process cwd
    // (nodejs-env.test.ts:285-316). On POSIX a real file with that name is
    // created under the temp root and found via PATH.
    let root = TestDir::new();
    let env = env_at(root.root());
    let shell_path = "C:\\Windows\\System32\\bash.exe";
    env.write_file(
        shell_path,
        b"#!/bin/sh\nprintf 'args:%s\\n' \"$*\" >&2\nexec /bin/bash \"$@\"\n",
        None,
    )
    .await
    .unwrap();
    let script = root.root().join(shell_path);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let guard = ProcessEnvGuard::enter(root.root());
    let result = env
        .with_shell_path(shell_path)
        .exec("name='World'; echo \"Hello, ${name}!\"", None)
        .await
        .unwrap();
    drop(guard);

    assert_eq!(result.stdout, "Hello, World!\n");
    assert_eq!(result.stderr, "args:-s\n");
    assert_eq!(result.exit_code, 0);
}

#[tokio::test]
async fn test_cleanup_terminates_active_shell_processes() {
    let root = TestDir::new();
    let env = Arc::new(env_at(root.root()));
    let execution = tokio::spawn({
        let env = Arc::clone(&env);
        async move { env.exec("touch started; sleep 60", None).await }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if env.exists("started", None).await.unwrap_or(false) {
            break;
        }
        assert!(Instant::now() < deadline, "started marker never appeared");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    FileSystem::cleanup(&*env).await;
    let result = tokio::time::timeout(Duration::from_secs(5), execution)
        .await
        .expect("exec never settled")
        .expect("exec task panicked")
        .expect("exec returned an error");
    // Killed by SIGKILL → `code ?? 0`.
    assert_eq!(result.exit_code, 0);
}

#[tokio::test]
async fn test_streams_stdout_and_stderr_chunks() {
    let root = TestDir::new();
    let env = env_at(root.root());
    let stdout_chunks = Arc::new(std::sync::Mutex::new(String::new()));
    let stderr_chunks = Arc::new(std::sync::Mutex::new(String::new()));
    let stdout_cb = Arc::clone(&stdout_chunks);
    let stderr_cb = Arc::clone(&stderr_chunks);
    let result = env
        .exec(
            "printf out; printf err >&2",
            Some(ShellExecOptions {
                on_stdout: Some(Box::new(move |chunk| {
                    stdout_cb.lock().unwrap().push_str(chunk)
                })),
                on_stderr: Some(Box::new(move |chunk| {
                    stderr_cb.lock().unwrap().push_str(chunk)
                })),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        result,
        rpi_agent::harness::types::ShellExecResult {
            stdout: "out".to_string(),
            stderr: "err".to_string(),
            exit_code: 0,
        }
    );
    assert_eq!(*stdout_chunks.lock().unwrap(), "out");
    assert_eq!(*stderr_chunks.lock().unwrap(), "err");
}

#[tokio::test]
async fn test_reports_missing_working_directory_before_spawning() {
    let root = TestDir::new();
    let env = NodeExecutionEnv::new(root.root().join("missing").to_string_lossy().into_owned());
    let error = env.exec("printf ok", None).await.unwrap_err();
    assert_eq!(error.code, ExecutionErrorCode::SpawnError);
    assert!(
        error.message.contains("Working directory does not exist"),
        "{:?}",
        error.message
    );
}

#[tokio::test]
async fn test_returns_non_zero_exit_codes_as_successful_results() {
    let root = TestDir::new();
    let env = env_at(root.root());
    let result = env.exec("exit 7", None).await.unwrap();
    assert_eq!(result.stdout, "");
    assert_eq!(result.stderr, "");
    assert_eq!(result.exit_code, 7);
}

#[tokio::test]
async fn test_returns_timeout_errors_for_commands_exceeding_timeout() {
    let root = TestDir::new();
    let env = env_at(root.root());
    let error = env
        .exec(
            "sleep 5",
            Some(ShellExecOptions {
                timeout: Some(1),
                ..Default::default()
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ExecutionErrorCode::Timeout);
}

#[tokio::test]
async fn test_returns_callback_errors_from_exec_stream_handlers() {
    let root = TestDir::new();
    let env = env_at(root.root());
    let error = env
        .exec(
            "printf out",
            Some(ShellExecOptions {
                on_stdout: Some(Box::new(|_| panic!("callback failed"))),
                ..Default::default()
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ExecutionErrorCode::CallbackError);
    assert_eq!(error.message, "callback failed");
}

#[tokio::test]
async fn test_returns_shell_unavailable_and_spawn_errors() {
    let root = TestDir::new();
    let missing_shell =
        env_at(root.root()).with_shell_path(root.root().join("missing-shell").to_string_lossy());
    let error = missing_shell.exec("printf ok", None).await.unwrap_err();
    assert_eq!(error.code, ExecutionErrorCode::ShellUnavailable);

    let env = env_at(root.root());
    env.write_file("not-executable-shell", b"not executable", None)
        .await
        .unwrap();
    let spawn_error_env =
        env.with_shell_path(root.root().join("not-executable-shell").to_string_lossy());
    let error = spawn_error_env.exec("printf ok", None).await.unwrap_err();
    assert_eq!(error.code, ExecutionErrorCode::SpawnError);
}

#[tokio::test]
async fn test_returns_aborted_results_for_aborted_commands() {
    let root = TestDir::new();
    let env = Arc::new(env_at(root.root()));
    let signal = CancellationToken::new();
    let execution = tokio::spawn({
        let env = Arc::clone(&env);
        let signal = signal.clone();
        async move {
            env.exec(
                "sleep 5",
                Some(ShellExecOptions {
                    abort_signal: Some(signal),
                    ..Default::default()
                }),
            )
            .await
        }
    });
    signal.cancel();
    let error = execution
        .await
        .expect("exec task panicked")
        .expect_err("expected an aborted error");
    assert_eq!(error.code, ExecutionErrorCode::Aborted);
}

// ---------------------------------------------------------------------------
// executeShellWithCapture (nodejs-env.test.ts:438-447 + shell-output semantics)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_captures_large_shell_output_to_full_output_file() {
    let root = TestDir::new();
    let env = env_at(root.root());
    let result = execute_shell_with_capture(&env, "yes line | head -n 15000", None)
        .await
        .unwrap();
    assert!(result.truncated);
    let full_output_path = result.full_output_path.as_ref().expect("full output path");
    let full_output = env.read_text_file(full_output_path, None).await.unwrap();
    assert!(full_output.split('\n').count() > 10_000);
    assert!(result.output.len() < full_output.len());
}

#[tokio::test]
async fn test_capture_reports_truncation_counts() {
    let root = TestDir::new();
    let env = env_at(root.root());
    let result = execute_shell_with_capture(&env, "yes line | head -n 15000", None)
        .await
        .unwrap();
    assert!(result.truncation.truncated);
    assert!(result.truncation.total_lines > 2000);
    assert!(result.truncation.total_bytes > 50 * 1024);
    // The 100 KB tail buffer holds ~20480 `line\n` lines, so `truncateTail`
    // truncates it by the 2000-line limit and reports `lines` (shell-output.ts
    // :100-103 keeps that as the final `truncatedBy`).
    assert_eq!(
        result.truncation.truncated_by,
        Some(rpi_agent::harness::utils::truncate::TruncatedBy::Lines)
    );
    assert!(result.output.ends_with("line"));
    assert!(!result.output.ends_with("line\n"));
}

#[tokio::test]
async fn test_execution_errors_returned_in_result_when_requested() {
    let root = TestDir::new();
    let env = NodeExecutionEnv::new(root.root().join("missing").to_string_lossy().into_owned());
    let result = execute_shell_with_capture(
        &env,
        "printf ok",
        Some(ShellCaptureOptions {
            return_execution_errors: true,
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    assert!(!result.cancelled);
    assert!(result.exit_code.is_none());
    let error = result
        .execution_error
        .as_ref()
        .expect("execution error present");
    assert_eq!(error.code, ExecutionErrorCode::SpawnError);
}

#[tokio::test]
async fn test_aborted_capture_reports_cancelled() {
    let root = TestDir::new();
    let env = env_at(root.root());
    let signal = CancellationToken::new();
    signal.cancel();
    let result = execute_shell_with_capture(
        &env,
        "sleep 5",
        Some(ShellCaptureOptions {
            abort_signal: Some(signal),
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    assert!(result.cancelled);
    assert!(result.exit_code.is_none());
    assert!(result.execution_error.is_none());
}

/// Stream chunks concatenate to the exact command output, in order — chunk
/// boundaries are arbitrary, but the reassembled stream must equal the full
/// stdout (upstream shell-output semantics; `one` / `three` alone would leave
/// `two` and the ordering unverified).
#[tokio::test]
async fn test_capture_streams_chunks_with_progress() {
    let root = TestDir::new();
    let env = env_at(root.root());
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let on_chunk = {
        let seen = Arc::clone(&seen);
        Box::new(move |chunk: &str, _progress: &rpi_agent::harness::utils::shell_output::ShellCaptureProgress| {
            seen.lock().unwrap().push(chunk.to_string());
        })
    };
    let result = execute_shell_with_capture(
        &env,
        "printf 'one\ntwo\nthree'",
        Some(ShellCaptureOptions {
            on_chunk: Some(on_chunk),
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    assert_eq!(result.exit_code, Some(0));
    let combined: String = seen.lock().unwrap().concat();
    assert_eq!(combined, "one\ntwo\nthree");
}

#[test]
fn test_sanitize_binary_output_removes_control_chars() {
    assert_eq!(sanitize_binary_output("a\x00b\x07c"), "abc");
    assert_eq!(sanitize_binary_output("a\tb\nc\rd"), "a\tb\nc\rd");
    // Interlinear annotation range 0xFFF9-0xFFFB is stripped.
    assert_eq!(sanitize_binary_output("ok\u{fffa}end"), "okend");
    assert_eq!(sanitize_binary_output("héllo 🙂"), "héllo 🙂");
}
