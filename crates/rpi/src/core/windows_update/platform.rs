//! Windows-only Win32 layer of the update machinery: process start times,
//! DELETE-access probes, console control handlers and the trampoline launch
//! (design §11.6). Everything here is `#[cfg(windows)]`; the platform
//! independent logic lives in the sibling modules and is tested on Linux.
//! The cross-target compile probe (`cargo check --target
//! x86_64-pc-windows-gnu`) type-checks this file; the runtime behaviour is
//! verified on the Windows matrix (V19-09 §5).

use std::io;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, FILETIME, GENERIC_WRITE, HANDLE, HANDLE_FLAG_INHERIT,
    INVALID_HANDLE_VALUE, STILL_ACTIVE, SetHandleInformation, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_ALWAYS, CreateFileW, DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Console::{
    CTRL_C_EVENT, GetStdHandle, PHANDLER_ROUTINE, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE, SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::Environment::GetCommandLineW;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, INFINITE,
    InitializeProcThreadAttributeList, OpenProcess, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

/// NUL-terminated UTF-16 form of a path.
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn filetime_u64(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

/// Creation time of `pid` as a `FILETIME` value (100 ns since 1601), or
/// `None` when no such process runs. A pid whose process already exited but
/// is still referenced by a handle (the trampoline holds its child's) is
/// reported by `GetExitCodeProcess` and counts as gone.
pub fn process_start_time(pid: u32) -> Option<u64> {
    // SAFETY: every call takes plain Win32 arguments and writes through
    // out-pointers to live locals; the handle is closed on every path.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let mut exit_code: u32 = 0;
        let alive =
            GetExitCodeProcess(process, &mut exit_code) != 0 && exit_code == STILL_ACTIVE as u32;
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
        let read = GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) != 0;
        CloseHandle(process);
        (alive && read).then(|| filetime_u64(creation))
    }
}

/// Creation time of the calling process (same unit as [`process_start_time`]).
pub fn current_process_start_time() -> Option<u64> {
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no
    // closing; the out-pointers target live locals.
    unsafe {
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
        let read = GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        ) != 0;
        read.then(|| filetime_u64(creation))
    }
}

/// Open `path` with `DELETE` access and every sharing mode, then close the
/// handle. A sharing violation (another handle without share-delete) or an
/// access denial surfaces here, before anything is renamed or deleted.
pub fn open_for_delete(path: &Path) -> io::Result<()> {
    let name = wide(path);
    // SAFETY: plain CreateFileW call; the returned handle is closed.
    unsafe {
        let handle = CreateFileW(
            name.as_ptr(),
            DELETE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        CloseHandle(handle);
    }
    Ok(())
}

/// [`open_for_delete`] for every path, stopping at the first failure.
pub fn open_for_delete_all(paths: &[&Path]) -> io::Result<()> {
    for path in paths {
        open_for_delete(path)?;
    }
    Ok(())
}

/// Whether `path` can be created, written and deleted right now: the
/// staging preflight for a directory (R7.2 / design §11.4 step 1).
pub fn probe_writable_directory(dir: &Path) -> io::Result<()> {
    let probe = dir.join(format!(".rpi-write-probe-{}", std::process::id()));
    let name = wide(&probe);
    // SAFETY: plain CreateFileW call; the returned handle is closed.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE | DELETE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            CREATE_ALWAYS,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the handle was just returned by CreateFileW.
    unsafe {
        CloseHandle(handle);
    }
    std::fs::remove_file(&probe)
}

/// The raw command line of this process (`GetCommandLineW`).
pub fn raw_command_line() -> Option<String> {
    // SAFETY: `GetCommandLineW` returns a pointer to a NUL-terminated
    // process-local string that stays valid for the process lifetime.
    let raw = unsafe {
        let pointer = GetCommandLineW();
        if pointer.is_null() {
            return None;
        }
        let mut length = 0usize;
        while *pointer.add(length) != 0 {
            length += 1;
        }
        std::slice::from_raw_parts(pointer, length)
    };
    String::from_utf16(raw).ok()
}

/// Split a raw command line into argv[0] and the rest, by the MSVC CRT
/// rules: a quoted argv[0] runs to the closing quote (a literal `"` cannot
/// occur in it), a bare one to the first space or tab. The tail starts with
/// that whitespace, or is empty.
pub fn split_command_line(raw: &str) -> (&str, &str) {
    let trimmed = raw.trim_start_matches([' ', '\t']);
    if let Some(rest) = trimmed.strip_prefix('"') {
        return match rest.find('"') {
            Some(end) => (&trimmed[..end + 2], &trimmed[end + 2..]),
            // Unterminated quote: the whole line is argv[0].
            None => (trimmed, ""),
        };
    }
    match trimmed.find([' ', '\t']) {
        Some(index) => (&trimmed[..index], &trimmed[index..]),
        None => (trimmed, ""),
    }
}

/// Quote one argument by the MSVC CRT rules (backslashes before a quote
/// double, a trailing run before the end doubles, the whole is wrapped).
pub fn quote_argument(argument: &str) -> String {
    let mut out = String::with_capacity(argument.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in argument.chars() {
        if ch == '\\' {
            backslashes += 1;
            continue;
        }
        if ch == '"' {
            out.push_str(&"\\".repeat(backslashes * 2 + 1));
        } else {
            out.push_str(&"\\".repeat(backslashes));
        }
        backslashes = 0;
        out.push(ch);
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// The command line of the relaunched instance: the absolute executable
/// path replaces argv[0]; the rest of the original command line is passed
/// through byte for byte.
pub fn trampoline_command_line(executable: &Path, raw_tail: &str) -> String {
    let mut line = quote_argument(&executable.to_string_lossy());
    line.push_str(raw_tail);
    line
}

/// A job object whose last handle closes with this process: when the
/// trampoline dies (its parent closes the terminal, an IDE stops it), the
/// new instance is terminated with it.
struct KillOnCloseJob(HANDLE);

impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        // SAFETY: the handle was created by `CreateJobObjectW` and is
        // closed exactly once here.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn create_kill_on_close_job() -> io::Result<KillOnCloseJob> {
    // SAFETY: plain job-object calls; the information struct is a live
    // local of the documented size.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut information: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&information).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if set == 0 {
            let error = io::Error::last_os_error();
            CloseHandle(job);
            return Err(error);
        }
        Ok(KillOnCloseJob(job))
    }
}

/// The three standard handles. `inheritable` lists the ones the child may
/// inherit (the process attribute list names exactly these); `by_role`
/// keeps each at its stdin/stdout/stderr position, `null` when absent.
struct StdioHandles {
    by_role: [HANDLE; 3],
    inheritable: [HANDLE; 3],
    count: usize,
}

impl StdioHandles {
    fn capture() -> Self {
        let mut by_role = [std::ptr::null_mut(); 3];
        let mut inheritable = [std::ptr::null_mut(); 3];
        let mut count = 0;
        for (role, which) in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
            .into_iter()
            .enumerate()
        {
            // SAFETY: `GetStdHandle` is a trivial accessor; the handle it
            // returns is only flagged here, never closed.
            unsafe {
                let handle = GetStdHandle(which);
                if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                    continue;
                }
                by_role[role] = handle;
                if SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
                    continue;
                }
                inheritable[count] = handle;
                count += 1;
            }
        }
        StdioHandles {
            by_role,
            inheritable,
            count,
        }
    }
}

/// A launched instance.
pub struct ChildProcess {
    handle: HANDLE,
    pid: u32,
    _job: Option<KillOnCloseJob>,
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        // SAFETY: the process handle from `PROCESS_INFORMATION` is closed
        // exactly once here.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

impl ChildProcess {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Block until the process exits and return its exit code.
    pub fn wait(&self) -> io::Result<u32> {
        // SAFETY: a valid process handle and a live out-pointer.
        unsafe {
            if WaitForSingleObject(self.handle, INFINITE) != WAIT_OBJECT_0 {
                return Err(io::Error::last_os_error());
            }
            let mut code: u32 = 0;
            if GetExitCodeProcess(self.handle, &mut code) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(code)
        }
    }
}

/// Start `executable` with `raw_command_line`, inheriting the console, the
/// working directory, the environment and exactly the three standard
/// handles (design §11.6). `CREATE_BREAKAWAY_FROM_JOB` is tried first so
/// the new instance is not killed when a job the caller belongs to ends;
/// an access denial (a job that forbids breakaway) retries without it.
pub fn launch(
    executable: &Path,
    raw_command_line: &str,
    log: &dyn Fn(&str),
) -> io::Result<ChildProcess> {
    let application = wide(executable);
    let mut command: Vec<u16> = raw_command_line.encode_utf16().chain(Some(0)).collect();
    let stdio = StdioHandles::capture();

    // The handle list restricts inheritance to the standard handles: a
    // lock or file handle this process holds must not leak into the child.
    let mut size: usize = 0;
    // SAFETY: the sizing call deliberately fails with
    // ERROR_INSUFFICIENT_BUFFER and only writes `size`.
    unsafe {
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
    }
    if size == 0 {
        return Err(io::Error::other(format!(
            "could not size the process attribute list: {}",
            io::Error::last_os_error()
        )));
    }
    let mut buffer = vec![0usize; size / std::mem::size_of::<usize>() + 1];
    let list = buffer.as_mut_ptr().cast();
    // SAFETY: the buffer is sized by the call above and aligned by `usize`.
    unsafe {
        if InitializeProcThreadAttributeList(list, 1, 0, &mut size) == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: the attribute list is initialized; the handle array outlives
    // the CreateProcessW call; the list is deleted on every path below.
    let launched = unsafe {
        let updated = UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            stdio.inheritable.as_ptr().cast(),
            stdio.count * std::mem::size_of::<HANDLE>(),
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        if updated == 0 {
            let error = io::Error::last_os_error();
            DeleteProcThreadAttributeList(list);
            return Err(error);
        }
        let mut startup: STARTUPINFOEXW = std::mem::zeroed();
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = stdio.by_role[0];
        startup.StartupInfo.hStdOutput = stdio.by_role[1];
        startup.StartupInfo.hStdError = stdio.by_role[2];
        startup.lpAttributeList = list;
        let mut information: PROCESS_INFORMATION = std::mem::zeroed();
        let mut created = CreateProcessW(
            application.as_ptr(),
            command.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_BREAKAWAY_FROM_JOB,
            std::ptr::null(),
            std::ptr::null(),
            &startup.StartupInfo,
            &mut information,
        );
        if created == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
                log(
                    "trampoline: CREATE_BREAKAWAY_FROM_JOB refused; launching inside the current job",
                );
                created = CreateProcessW(
                    application.as_ptr(),
                    command.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    1,
                    EXTENDED_STARTUPINFO_PRESENT,
                    std::ptr::null(),
                    std::ptr::null(),
                    &startup.StartupInfo,
                    &mut information,
                );
            }
            if created == 0 {
                let error = io::Error::last_os_error();
                DeleteProcThreadAttributeList(list);
                return Err(error);
            }
        }
        DeleteProcThreadAttributeList(list);
        CloseHandle(information.hThread);
        information
    };
    let job = match create_kill_on_close_job() {
        Ok(job) => {
            // SAFETY: both handles are valid.
            let assigned = unsafe { AssignProcessToJobObject(job.0, launched.hProcess) };
            if assigned == 0 {
                log(&format!(
                    "trampoline: could not put the new instance in a kill-on-close job: {}",
                    io::Error::last_os_error()
                ));
                None
            } else {
                Some(job)
            }
        }
        Err(error) => {
            log(&format!(
                "trampoline: could not create a kill-on-close job: {error}"
            ));
            None
        }
    };
    Ok(ChildProcess {
        handle: launched.hProcess,
        pid: launched.dwProcessId,
        _job: job,
    })
}

// ---------------------------------------------------------------------------
// Console control handlers (design §11.6 / §11.7)
// ---------------------------------------------------------------------------

static TRAMPOLINE_CTRL_C: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn trampoline_handler(ctrl_type: u32) -> windows_sys::core::BOOL {
    if ctrl_type == CTRL_C_EVENT {
        // Record and swallow: the new instance owns the Ctrl+C.
        TRAMPOLINE_CTRL_C.store(true, Ordering::SeqCst);
        1
    } else {
        0
    }
}

/// The trampoline's own Ctrl+C handler. `SetConsoleCtrlHandler(NULL, TRUE)`
/// is deliberately not used: the "ignore Ctrl+C" property is inherited and
/// would keep the new instance from ever seeing Ctrl+C.
pub fn install_trampoline_ctrl_handler() -> bool {
    let handler: PHANDLER_ROUTINE = Some(trampoline_handler);
    // SAFETY: the handler is a plain `extern "system"` function.
    unsafe { SetConsoleCtrlHandler(handler, 1) != 0 }
}

/// Whether the trampoline swallowed at least one Ctrl+C.
pub fn trampoline_saw_ctrl_c() -> bool {
    TRAMPOLINE_CTRL_C.load(Ordering::SeqCst)
}

type MarkCallback = Box<dyn Fn() + Send + Sync>;
static MARK_CALLBACK: OnceLock<MarkCallback> = OnceLock::new();

unsafe extern "system" fn marking_handler(ctrl_type: u32) -> windows_sys::core::BOOL {
    if ctrl_type == CTRL_C_EVENT
        && let Some(callback) = MARK_CALLBACK.get()
    {
        // Mark the normal exit first, then let the default handler
        // terminate the process: a Ctrl+C is never a crash.
        callback();
    }
    0
}

/// The new instance's Ctrl+C handler: runs `mark` (the normal-exit mark)
/// and returns `FALSE`, so the default handling still ends the process.
pub fn install_marking_ctrl_handler(mark: MarkCallback) -> bool {
    let _ = MARK_CALLBACK.set(mark);
    let handler: PHANDLER_ROUTINE = Some(marking_handler);
    // SAFETY: the handler is a plain `extern "system"` function.
    unsafe { SetConsoleCtrlHandler(handler, 1) != 0 }
}

/// Whether this process has a console attached to its standard output
/// (false when output is redirected to a file or pipe and no console
/// exists).
pub fn console_mode_queryable() -> bool {
    // SAFETY: `GetStdHandle` is a trivial accessor; `GetConsoleMode` writes
    // one u32.
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut mode: u32 = 0;
        windows_sys::Win32::System::Console::GetConsoleMode(handle, &mut mode) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use windows_sys::Win32::Foundation::FreeLibrary;
    use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;

    #[test]
    fn command_lines_split_by_the_crt_rules() {
        assert_eq!(split_command_line("rpi --version"), ("rpi", " --version"));
        assert_eq!(split_command_line("  rpi  -p hi "), ("rpi", "  -p hi "));
        assert_eq!(
            split_command_line(r#""C:\Program Files\rpi\rpi.exe" -p "a b""#),
            (r#""C:\Program Files\rpi\rpi.exe""#, r#" -p "a b""#)
        );
        assert_eq!(split_command_line("rpi"), ("rpi", ""));
        assert_eq!(
            split_command_line(r#""unterminated"#),
            (r#""unterminated"#, "")
        );
        assert_eq!(split_command_line(""), ("", ""));
    }

    #[test]
    fn arguments_are_quoted_like_the_crt() {
        assert_eq!(
            quote_argument(r"C:\Program Files\rpi\rpi.exe"),
            r#""C:\Program Files\rpi\rpi.exe""#
        );
        assert_eq!(quote_argument("plain"), "\"plain\"");
        assert_eq!(quote_argument("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_argument("a\\\"b"), "\"a\\\\\\\"b\"");
        assert_eq!(quote_argument("trailing\\"), "\"trailing\\\\\"");
        assert_eq!(quote_argument(""), "\"\"");
    }

    #[test]
    fn the_trampoline_command_line_keeps_the_original_tail() {
        let line = trampoline_command_line(
            Path::new(r"C:\Users\me\AppData\Local\Programs\rpi\rpi.exe"),
            r#" -p "hello world" --thinking max"#,
        );
        assert_eq!(
            line,
            r#""C:\Users\me\AppData\Local\Programs\rpi\rpi.exe" -p "hello world" --thinking max"#
        );
        let (argv0, tail) = split_command_line(&line);
        assert!(argv0.starts_with('"') && argv0.ends_with('"'));
        assert_eq!(tail, r#" -p "hello world" --thinking max"#);
    }

    #[test]
    fn this_process_is_alive_with_a_start_time() {
        let started = current_process_start_time().unwrap();
        assert!(started > 0);
        assert_eq!(process_start_time(std::process::id()), Some(started));
        // The idle process and the system process are not queryable.
        assert!(process_start_time(0).is_none());
    }

    #[test]
    fn delete_probes_follow_the_file_system() {
        let dir = std::env::temp_dir().join(format!("rpi-wu-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("probe.txt");
        std::fs::write(&file, b"x").unwrap();
        open_for_delete(&file).unwrap();
        open_for_delete(&dir).unwrap();
        open_for_delete_all(&[&file, &dir]).unwrap();
        assert_eq!(
            open_for_delete(&dir.join("missing")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        probe_writable_directory(&dir).unwrap();
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "the write probe leaves nothing behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- Windows-only probes of the design premises (V19-09 §1, FR-W0) -----
    //
    // These run on the Windows test leg only. They pin the three Win32
    // facts the trampoline and the transaction rest on, so a Windows
    // release that changes one of them fails here before it reaches a user.

    /// Renames `from` back to `to` when dropped, so a failed assertion
    /// never leaves the test binary (or a temp directory) under its probe
    /// name.
    struct RestoreOnDrop {
        from: std::path::PathBuf,
        to: std::path::PathBuf,
    }

    impl Drop for RestoreOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::rename(&self.from, &self.to);
        }
    }

    struct FreeOnDrop(usize);

    impl Drop for FreeOnDrop {
        fn drop(&mut self) {
            // SAFETY: the module handle was returned by `LoadLibraryW`.
            unsafe {
                FreeLibrary(self.0 as _);
            }
        }
    }

    fn probe_temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rpi-wu-probe-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Premise 1 (design §11.8 "the executable follows rpi.exe"): a running
    /// image can be renamed, `current_exe()` keeps reporting its original
    /// path, and the running image cannot be deleted.
    #[test]
    fn a_running_image_is_renamable_keeps_its_path_and_is_not_deletable() {
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_string_lossy().into_owned();
        let parked = exe.with_file_name(format!("{name}.old-probe-{}", std::process::id()));
        std::fs::rename(&exe, &parked).expect("a running image can be renamed");
        let restore = RestoreOnDrop {
            from: parked.clone(),
            to: exe.clone(),
        };
        assert_eq!(
            std::env::current_exe().unwrap(),
            exe,
            "current_exe() keeps the original path after the rename"
        );
        let delete = std::fs::remove_file(&parked).unwrap_err();
        assert_ne!(
            delete.kind(),
            io::ErrorKind::NotFound,
            "the delete reached the file and was refused"
        );
        // A DELETE-access probe still succeeds (it is the delete that is
        // refused): the cleanup scan must rely on pins for old images.
        open_for_delete(&parked).unwrap();
        drop(restore);
        assert!(exe.exists(), "the test binary is back under its name");
        assert!(!parked.exists());
    }

    /// Premise 2 (ADR-0050 background): a directory holding a loaded
    /// library can be neither renamed nor deleted until the library is
    /// unloaded — the reason extensions install as generations.
    #[test]
    fn a_directory_with_a_loaded_library_is_neither_renamable_nor_deletable() {
        let Ok(source) = std::env::var("RPI_TEST_NATIVE_PLUGIN_DLL") else {
            eprintln!(
                "skipping: RPI_TEST_NATIVE_PLUGIN_DLL is unset (the Windows test leg \
                 sets it to the built native-plugin fixture)"
            );
            return;
        };
        let dir = probe_temp_dir("dll");
        let library = dir.join("probe.dll");
        std::fs::copy(&source, &library).unwrap();
        let wide: Vec<u16> = library.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: a NUL-terminated path to a file that exists.
        let module = unsafe { LoadLibraryW(wide.as_ptr()) };
        assert!(
            !module.is_null() && module as usize != usize::MAX,
            "the fixture loads: {}",
            io::Error::last_os_error()
        );
        let _free = FreeOnDrop(module as usize);
        let renamed = dir.with_file_name(format!(
            "{}.renamed",
            dir.file_name().unwrap().to_string_lossy()
        ));
        std::fs::rename(&dir, &renamed).unwrap_err();
        std::fs::remove_dir_all(&dir).unwrap_err();
        assert!(
            library.exists(),
            "the loaded library survived the delete attempt"
        );
        drop(_free);
        std::fs::rename(&dir, &renamed).expect("unloaded: the directory renames again");
        std::fs::remove_dir_all(&renamed).unwrap();
    }

    fn system_cmd() -> Option<PathBuf> {
        let root = std::env::var_os("SystemRoot")?;
        Some(PathBuf::from(root).join("System32").join("cmd.exe"))
    }

    /// The trampoline relays the child's exit code and its quoted command
    /// line (design §11.6).
    #[test]
    fn launch_relays_exit_codes_and_quoted_arguments() {
        let Some(cmd) = system_cmd() else {
            eprintln!("skipping: SystemRoot is unset");
            return;
        };
        let child = launch(
            &cmd,
            &trampoline_command_line(&cmd, " /c exit 3"),
            &|_: &str| {},
        )
        .unwrap();
        assert_eq!(child.wait().unwrap(), 3);
        let child = launch(
            &cmd,
            &trampoline_command_line(&cmd, " /c exit 0"),
            &|_: &str| {},
        )
        .unwrap();
        assert_eq!(child.wait().unwrap(), 0);

        // A path with spaces reaches the child as one argument.
        let out = probe_temp_dir("quoted space").join("out.txt");
        let line = trampoline_command_line(
            &cmd,
            &format!(r#" /c echo "a b" > "{}""#, out.to_string_lossy()),
        );
        let child = launch(&cmd, &line, &|_: &str| {}).unwrap();
        assert_eq!(child.wait().unwrap(), 0);
        assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), "a b");
        let _ = std::fs::remove_dir_all(out.parent().unwrap());
    }

    /// Closing the trampoline's side (the process dies, its handles close)
    /// terminates the new instance through the kill-on-close job.
    #[test]
    fn dropping_the_trampoline_side_kills_the_child() {
        let Some(cmd) = system_cmd() else {
            eprintln!("skipping: SystemRoot is unset");
            return;
        };
        let child = launch(
            &cmd,
            &trampoline_command_line(&cmd, " /c ping -n 60 127.0.0.1 > nul"),
            &|_: &str| {},
        )
        .unwrap();
        let pid = child.pid();
        assert!(process_start_time(pid).is_some(), "the child runs");
        drop(child);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while process_start_time(pid).is_some() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            process_start_time(pid).is_none(),
            "the child died with the job"
        );
    }

    #[test]
    fn the_raw_command_line_starts_with_this_test_binary() {
        let raw = raw_command_line().unwrap();
        let (argv0, tail) = split_command_line(&raw);
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_string_lossy().into_owned();
        assert!(argv0.contains(&name), "{argv0:?} vs {name:?}");
        let _ = tail;
    }
}
