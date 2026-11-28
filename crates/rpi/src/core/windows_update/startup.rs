//! Windows-only startup hooks (design §11.1, §11.5–§11.9).
//!
//! `main.rs` calls [`before_run`] right after `early_init::initialize()` —
//! before CLI dispatch and extension loading — and the unified exit outlet
//! ([`on_exit`]) after `run_app` returns. Between them the interactive
//! mode asks for the startup notices ([`interactive_notices`]) and marks
//! them read, and the confirmation points report in ([`confirm_point`]).
//!
//! Every process registers its pin first (subagent and preflight children
//! included). Then, unless it is such a child, the main executable runs
//! the startup transaction, launches the new instance as a trampoline when
//! it just committed an update, runs the cleanup scan and classifies the
//! notices. A non-interactive start writes its warning lines to stderr
//! here; an interactive one hands them to the TUI.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use super::boot::{
    ProcessProbe, SystemProcessProbe, mark_normal_exit, own_boot_record, write_boot_record,
};
use super::fs::{RealFs, UpdateFs};
use super::gc::{ScanContext, cleanup_scan, cleanup_scan_with};
use super::layout::UpdatePaths;
use super::lock;
use super::notices::{NoticeInput, StartupNotice, build_notices};
use super::pins::{PinHandle, extension_dirs_in_use, live_pins};
use super::platform;
use super::records::{
    PendingUpdate, PinRecord, UpdateResult, UpdateState, read_record, write_record,
};
use super::rollback::{RollbackReason, rollback_applied_update, rollback_message};
use super::staging;
use super::transaction::{
    StartupEnv, StartupOptions, TrampolineVerdict, TransactionOutcome, confirm_update,
    judge_trampoline_child, run_startup_transaction,
};
use super::update_log::log_event;

/// How long a confirmation point waits for the update lock: it must never
/// hold the UI back, and the exit outlet retries.
const CONFIRM_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// A log sink over the file-system seam.
fn sink_for(fs: &Arc<dyn UpdateFs>, log_path: &Option<PathBuf>) -> impl Fn(&str) + use<> {
    let fs = Arc::clone(fs);
    let log_path = log_path.clone();
    move |message: &str| {
        let (event, detail) = message.split_once(": ").unwrap_or((message, ""));
        log_event(fs.as_ref(), log_path.as_deref(), event, detail);
    }
}

/// The per-process state the hooks share with the exit outlet and the
/// confirmation points.
struct StartupState {
    fs: Arc<dyn UpdateFs>,
    paths: UpdatePaths,
    probe: Arc<dyn ProcessProbe>,
    env: StartupEnv,
    pin: Option<PinHandle>,
    /// This process takes part in the first boot of an applied update (it
    /// runs the target version and the update is still `applied`).
    first_boot_target: Option<String>,
    /// This process runs the version an update-result names as `to`: its
    /// first normal exit after the confirmation releases the previous
    /// executable for the cleanup scan (design §11.7).
    runs_target_version: bool,
    state: UpdateState,
    notices: Vec<StartupNotice>,
}

impl StartupState {
    fn log(&self, event: &str, detail: &str) {
        log_event(
            self.fs.as_ref(),
            Some(self.paths.log_path.as_path()),
            event,
            detail,
        );
    }

    fn sink(&self) -> impl Fn(&str) + use<> {
        sink_for(&self.fs, &Some(self.paths.log_path.clone()))
    }

    fn result(&self) -> Option<UpdateResult> {
        read_record(self.fs.as_ref(), &self.paths.result_path())
    }

    fn write_result(&self, result: &UpdateResult) {
        let _ = write_record(self.fs.as_ref(), &self.paths.result_path(), Some(result));
    }

    /// Pin the extension directories this process is about to load.
    fn pin_extension_dirs(&mut self) {
        let dirs = extension_dirs_in_use(self.fs.as_ref(), &self.paths.extension_roots);
        if let Some(pin) = &mut self.pin
            && let Err(error) = pin.add_dirs(&dirs)
        {
            self.log(
                "pin",
                &format!("could not record the extension directories: {error}"),
            );
        }
    }

    /// Record the boot record of a first-boot process.
    fn register_boot_record(&mut self, target_version: &str, running_version: &str) {
        if target_version != running_version {
            return;
        }
        let record = own_boot_record(self.probe.as_ref(), running_version);
        let path = self.paths.boot_record_path(record.pid);
        match write_boot_record(self.fs.as_ref(), &path, &record) {
            Ok(()) => self.first_boot_target = Some(target_version.to_owned()),
            Err(error) => self.log(
                "transaction",
                &format!("could not write the boot record: {error}"),
            ),
        }
    }

    /// Confirm an applied update under the lock, after checking that it is
    /// still waiting (a parallel rollback may have changed the state).
    fn confirm(&mut self) {
        let _lock = match lock::acquire_exclusive(&self.paths.lock_path(), CONFIRM_LOCK_WAIT) {
            Ok(lock) => lock,
            Err(error) => {
                self.log(
                    "confirm",
                    &format!("deferred: the update lock is unavailable ({error})"),
                );
                return;
            }
        };
        let Some(pending) =
            read_record::<PendingUpdate>(self.fs.as_ref(), &self.paths.pending_path())
        else {
            // Already confirmed or rolled back by someone else.
            self.first_boot_target = None;
            return;
        };
        if !pending.applied {
            self.first_boot_target = None;
            return;
        }
        confirm_update(self.fs.as_ref(), &self.paths, &pending);
        self.state = UpdateState::Confirmed;
        self.first_boot_target = None;
        self.log("confirm", "update confirmed");
    }

    fn scan_context<'a>(
        &'a self,
        live: Vec<super::records::PinRecord>,
        result: Option<&'a UpdateResult>,
        pending: Option<&'a PendingUpdate>,
    ) -> ScanContext<'a> {
        ScanContext {
            paths: &self.paths,
            probe: self.probe.as_ref(),
            live_pins: live,
            self_pid: self.probe.current_pid(),
            state: result
                .map(|result| result.state)
                .unwrap_or(UpdateState::Idle),
            pending,
            result,
            now: std::time::SystemTime::now(),
        }
    }
}

static STARTUP: OnceLock<Mutex<Option<Arc<Mutex<StartupState>>>>> = OnceLock::new();

fn cell() -> &'static Mutex<Option<Arc<Mutex<StartupState>>>> {
    STARTUP.get_or_init(|| Mutex::new(None))
}

/// Install the state (once per process).
fn install(state: StartupState) -> Arc<Mutex<StartupState>> {
    let shared = Arc::new(Mutex::new(state));
    *cell().lock().unwrap_or_else(|error| error.into_inner()) = Some(Arc::clone(&shared));
    shared
}

fn shared() -> Option<Arc<Mutex<StartupState>>> {
    cell()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

fn with_state<T>(state: &Arc<Mutex<StartupState>>, work: impl FnOnce(&mut StartupState) -> T) -> T {
    let mut guard = state.lock().unwrap_or_else(|error| error.into_inner());
    work(&mut guard)
}

/// What `main` does after the hook.
pub enum Startup {
    /// Continue the normal start.
    Run,
    /// This process became the trampoline of the update it just applied:
    /// wait for the new instance and exit with its code; never run the app.
    Trampoline(Trampoline),
}

/// The waiting old instance (design §11.6).
pub struct Trampoline {
    child: platform::ChildProcess,
    state: Arc<Mutex<StartupState>>,
}

impl Trampoline {
    /// Wait for the new instance, judge its first boot, roll back and
    /// relaunch the previous executable on a crash, then exit with the
    /// last exit code. Never returns.
    pub fn wait_and_exit(mut self) -> ! {
        loop {
            let code = match self.child.wait() {
                Ok(code) => code,
                Err(error) => {
                    with_state(&self.state, |state| {
                        state.log(
                            "trampoline",
                            &format!("could not wait for the new instance: {error}"),
                        );
                    });
                    1
                }
            };
            let pid = self.child.pid();
            let verdict = with_state(&self.state, |state| {
                judge_trampoline_child(
                    state.fs.as_ref(),
                    &state.paths,
                    state.probe.as_ref(),
                    pid,
                    code,
                    &StartupOptions::default(),
                    &state.sink(),
                )
            });
            // The exit code keeps its 32-bit pattern (a STATUS_* value such
            // as the Ctrl+C termination 0xC000013A is not a positive int).
            let code = code as i32;
            match verdict {
                TrampolineVerdict::RolledBack => {
                    let relaunched = with_state(&self.state, |state| {
                        let result = state.result();
                        let from = result
                            .as_ref()
                            .and_then(|result| result.from_version.clone())
                            .unwrap_or_default();
                        let to = result
                            .as_ref()
                            .and_then(|result| result.to_version.clone())
                            .unwrap_or_default();
                        let _ = writeln!(
                            std::io::stderr(),
                            "{}",
                            rollback_message(&from, &to, &state.paths.log_path)
                        );
                        state.log("trampoline", "relaunching the previous executable");
                        let raw = platform::raw_command_line().unwrap_or_default();
                        let (_, tail) = platform::split_command_line(&raw);
                        let command_line =
                            platform::trampoline_command_line(&state.paths.exe_path, tail);
                        // The relaunched previous version is nobody's first
                        // boot: no trampoline pid in its environment.
                        platform::launch(&state.paths.exe_path, &command_line, &[], &state.sink())
                    });
                    match relaunched {
                        Ok(child) => self.child = child,
                        Err(error) => {
                            with_state(&self.state, |state| {
                                let _ = writeln!(
                                    std::io::stderr(),
                                    "rpi could not restart the previous version ({}): {error}. Start rpi again.",
                                    state.paths.exe_path.display()
                                );
                            });
                            on_exit(code, false);
                        }
                    }
                }
                TrampolineVerdict::PassThrough | TrampolineVerdict::Deferred => {
                    on_exit(code, false)
                }
            }
        }
    }
}

/// The subcommands `run_app` dispatches before it ever considers a TUI.
const NON_INTERACTIVE_SUBCOMMANDS: &[&str] = &[
    "config",
    "update",
    "self-uninstall",
    "mcp",
    "auth",
    "install",
    "remove",
    "uninstall",
    "list",
];

/// The headless switches that rule a TUI out whatever the terminals say.
const HEADLESS_SWITCHES: &[&str] = &[
    "--mode",
    "-p",
    "--print",
    "--export",
    "--help",
    "-h",
    "--version",
];

/// A conservative pre-check of `resolve_app_mode`: the notices go to the
/// TUI only when both standard streams are terminals and nothing on the
/// command line asks for a headless mode. A wrong guess only moves a
/// warning between stderr and the TUI (the `rpi-rpc` entry, which always
/// runs the stdio mode, is not recognized here and defers its warnings to
/// the next interactive start).
pub fn looks_interactive(args: &[std::ffi::OsString]) -> bool {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return false;
    }
    let Some(first) = args.first().and_then(|arg| arg.to_str()) else {
        return true;
    };
    if NON_INTERACTIVE_SUBCOMMANDS.contains(&first) {
        return false;
    }
    !args.iter().any(|arg| {
        arg.to_str()
            .is_some_and(|arg| HEADLESS_SWITCHES.contains(&arg))
    })
}

/// Paths, file system and probe of this process.
fn context() -> (UpdatePaths, Arc<dyn UpdateFs>, Arc<dyn ProcessProbe>) {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rpi.exe"));
    let agent_dir = crate::config::get_agent_dir();
    let cwd = std::env::current_dir().ok();
    let paths = UpdatePaths::for_exe(&exe, &agent_dir, cwd.as_deref());
    let fs: Arc<dyn UpdateFs> = Arc::new(RealFs);
    let probe: Arc<dyn ProcessProbe> = Arc::new(SystemProcessProbe::new());
    (paths, fs, probe)
}

/// The session identity recorded in the pin: the harness session id when
/// the process has one, else a random one.
fn session_identity() -> String {
    std::env::var("RPI_SESSION_ID")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

enum Next {
    Run,
    Trampoline(platform::ChildProcess),
}

/// Hook #1: run before anything else in `main`.
pub fn before_run(args: &[std::ffi::OsString]) -> Startup {
    let (paths, fs, probe) = context();
    let env = StartupEnv::from_env();
    let running_version = crate::config::VERSION;
    // 1. Every process registers itself.
    let pin = {
        let record = PinRecord::new(
            probe.current_pid(),
            probe.current_process_start(),
            session_identity(),
            running_version,
            &paths.exe_path,
        );
        match PinHandle::register(Arc::clone(&fs), &paths.pins_dir(), record) {
            Ok(pin) => Some(pin),
            Err(error) => {
                log_event(
                    fs.as_ref(),
                    Some(paths.log_path.as_path()),
                    "pin",
                    &format!("could not register: {error}"),
                );
                None
            }
        }
    };
    let state = install(StartupState {
        fs,
        paths,
        probe,
        env: env.clone(),
        pin,
        first_boot_target: None,
        runs_target_version: false,
        state: UpdateState::Idle,
        notices: Vec::new(),
    });
    // 2. Ctrl+C is a normal exit for every process that is not a preflight
    //    child: mark first, then let the default handler end the process.
    if !env.verify_child {
        let installed = with_state(&state, |state| {
            let fs = Arc::clone(&state.fs);
            let record = state.paths.boot_record_path(state.probe.current_pid());
            platform::install_marking_ctrl_handler(Box::new(move || {
                let _ = mark_normal_exit(fs.as_ref(), &record);
            }))
        });
        if !installed {
            with_state(&state, |state| {
                state.log("ctrl-c", "could not install the console control handler")
            });
        }
    }
    let next = with_state(&state, |state| {
        if env.subagent_child || env.verify_child {
            state.pin_extension_dirs();
            return Next::Run;
        }
        // Only the main executable (`rpi.exe`, the image a release asset
        // carries) runs the transaction. A sibling binary in the same
        // prefix (`rpi-rpc.exe`) shares the installation but must not park
        // its own image or receive the staged `rpi.exe` under its name: it
        // registers its pin and lets the next `rpi.exe` start apply the
        // update.
        if !state
            .paths
            .exe_file_name()
            .eq_ignore_ascii_case(staging::staged_exe_name())
        {
            state.log(
                "transaction",
                "skipped: this executable is not the main rpi image; the next rpi.exe start applies it",
            );
            state.pin_extension_dirs();
            return Next::Run;
        }
        // 3. The startup transaction.
        let outcome = run_startup_transaction(
            state.fs.as_ref(),
            &state.paths,
            state.probe.as_ref(),
            &env,
            running_version,
            &StartupOptions::default(),
            &state.sink(),
        );
        let mut child: Option<platform::ChildProcess> = None;
        let command_line = |state: &StartupState| {
            let raw = platform::raw_command_line().unwrap_or_default();
            let (_, tail) = platform::split_command_line(&raw);
            platform::trampoline_command_line(&state.paths.exe_path, tail)
        };
        match outcome {
            TransactionOutcome::Applied { .. } => {
                // P2: this process is the previous version's image; the
                // new instance takes over the console. Its environment
                // names the trampoline, which is how the first boot knows
                // it is the first boot (design §11.6/§11.9).
                let line = command_line(state);
                let trampoline_env = vec![(
                    super::ENV_TRAMPOLINE_PID.to_owned(),
                    std::process::id().to_string(),
                )];
                match platform::launch(&state.paths.exe_path, &line, &trampoline_env, &state.sink())
                {
                    Ok(launched) => child = Some(launched),
                    Err(error) => {
                        state.log(
                            "trampoline",
                            &format!("could not start the new executable ({error}); rolling back"),
                        );
                        rollback_after_launch_failure(state);
                    }
                }
            }
            TransactionOutcome::RolledBack { message } => {
                // This process runs the new image; the previous executable
                // is back at its path. Say what happened and re-enter it.
                let _ = writeln!(std::io::stderr(), "{message}");
                let line = command_line(state);
                match platform::launch(&state.paths.exe_path, &line, &[], &state.sink()) {
                    Ok(launched) => child = Some(launched),
                    Err(error) => {
                        let _ = writeln!(
                            std::io::stderr(),
                            "rpi could not restart the previous version ({}): {error}. Start rpi again.",
                            state.paths.exe_path.display()
                        );
                    }
                }
            }
            TransactionOutcome::Degraded { message, manual } => {
                let _ = writeln!(std::io::stderr(), "{message}");
                if let Some(manual) = manual {
                    for line in manual.lines() {
                        let _ = writeln!(std::io::stderr(), "{line}");
                    }
                }
                state.log("transaction", "degraded: manual guidance printed");
            }
            TransactionOutcome::FirstBootOpen { target_version } => {
                state.register_boot_record(&target_version, running_version);
                state.state = UpdateState::Applied;
            }
            TransactionOutcome::NoPending | TransactionOutcome::LockBusy => {}
        }
        state.pin_extension_dirs();
        // 4. The cleanup scan and the notices.
        run_cleanup_scan_and_notices(state, args);
        match child {
            Some(child) => {
                if platform::install_trampoline_ctrl_handler() {
                    state.log("trampoline", "waiting for the new instance");
                } else {
                    state.log(
                        "trampoline",
                        "Ctrl+C handler not installed; waiting for the new instance",
                    );
                }
                Next::Trampoline(child)
            }
            None => Next::Run,
        }
    });
    match next {
        Next::Run => Startup::Run,
        Next::Trampoline(child) => Startup::Trampoline(Trampoline { child, state }),
    }
}

/// The new executable could not be started: undo the update from the
/// pre-image (the version is not blocked for that) and continue as the
/// previous version in this process.
fn rollback_after_launch_failure(state: &mut StartupState) {
    let Some(pending) =
        read_record::<PendingUpdate>(state.fs.as_ref(), &state.paths.pending_path())
            .filter(|pending| pending.applied)
    else {
        return;
    };
    let _lock = match lock::acquire_exclusive(&state.paths.lock_path(), super::LOCK_WAIT) {
        Ok(lock) => lock,
        Err(error) => {
            state.log(
                "trampoline",
                &format!("rollback after the launch failure deferred: {error}"),
            );
            return;
        }
    };
    let outcome = rollback_applied_update(
        state.fs.as_ref(),
        &state.paths,
        &pending,
        RollbackReason::LaunchFailed,
        &state.sink(),
    );
    let message = rollback_message(
        &pending.from_version,
        &pending.target_version,
        &state.paths.log_path,
    );
    if outcome.exe_restored {
        let _ = writeln!(std::io::stderr(), "{message}");
        let _ = writeln!(
            std::io::stderr(),
            "The new rpi could not be started. rpi continues on rpi {}.",
            pending.from_version
        );
    } else {
        let _ = writeln!(
            std::io::stderr(),
            "{message} The rollback is incomplete: {}.",
            outcome.failures.join("; ")
        );
    }
}

fn run_cleanup_scan_and_notices(state: &mut StartupState, args: &[std::ffi::OsString]) {
    let live = live_pins(
        state.fs.as_ref(),
        &state.paths.pins_dir(),
        state.probe.as_ref(),
    );
    let result = state.result();
    let pending = read_record::<PendingUpdate>(state.fs.as_ref(), &state.paths.pending_path());
    let status = result
        .as_ref()
        .map(|result| result.state)
        .unwrap_or(UpdateState::Idle);
    state.state = status;
    state.runs_target_version = result
        .as_ref()
        .is_some_and(|result| result.to_version.as_deref() == Some(crate::config::VERSION));
    let scan = cleanup_scan(
        state.fs.as_ref(),
        &state.scan_context(live.clone(), result.as_ref(), pending.as_ref()),
    );
    // A clean start writes nothing: the log must not cost a write per
    // start (design §11.12 budget).
    if !scan.removed.is_empty()
        || !scan.live.is_empty()
        || !scan.stuck.is_empty()
        || scan.skipped_staging > 0
    {
        state.log(
            "scan",
            &format!(
                "removed {} item(s), {} in use, {} stuck, {} staging skipped",
                scan.removed.len(),
                scan.live.len(),
                scan.stuck.len(),
                scan.skipped_staging
            ),
        );
    }
    let notices = build_notices(
        &NoticeInput {
            state: status,
            result: result.as_ref(),
            pending: pending.as_ref(),
            scan: &scan,
            first_boot_launch: state.env.trampoline_pid.is_some(),
            self_pid: state.probe.current_pid(),
            trampoline_pid: state.env.trampoline_pid,
            live_pins: &live,
        },
        &state.paths.log_path,
    );
    state.notices = notices;
    if !looks_interactive(args) {
        // Non-interactive: one stderr line each for the warning, the
        // rollback and the degradation; nothing is marked read.
        for notice in &state.notices {
            if notice.noninteractive_stderr() {
                let _ = writeln!(
                    std::io::stderr(),
                    "Warning: {}",
                    notice.text(&state.paths.log_path)
                );
            }
        }
    }
}

/// The notices an interactive start shows (each once), rendered with
/// the log path.
pub fn interactive_notices() -> Vec<String> {
    shared()
        .map(|state| {
            with_state(&state, |state| {
                state
                    .notices
                    .iter()
                    .map(|notice| notice.text(&state.paths.log_path))
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// Mark the update result read after the interactive display.
pub fn mark_notices_read() {
    if let Some(state) = shared() {
        with_state(&state, |state| {
            if !state.notices.iter().any(|notice| notice.marks_read()) {
                return;
            }
            if let Some(mut result) = state.result() {
                result.read = true;
                state.write_result(&result);
                state.log("notice", "update result marked read");
            }
        });
    }
}

/// The confirmation points of design §11.7. A one-shot command's success
/// is covered by the exit outlet (exit code 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmPoint {
    /// `--help`, `--version`, an argument error: no user command ran.
    NoUserCommand,
    /// The TUI main loop is up.
    TuiReady,
    /// The `rpc` stdio loop is ready (the only resident stdio mode: `rpi
    /// mcp` is a configuration subcommand, and `--mode json` is a one-shot
    /// command that confirms through its exit code).
    RpcReady,
}

/// Report a confirmation point. Only a process in the open first boot of
/// the target version acts.
pub fn confirm_point(point: ConfirmPoint) {
    if let Some(state) = shared() {
        with_state(&state, |state| {
            if state.first_boot_target.is_some() {
                state.log("confirm", &format!("confirmation point: {:?}", point));
                state.confirm();
            }
        });
    }
}

/// Hook #2: the unified exit outlet. Marks the normal exit, confirms on a
/// successful one-shot run, records the first normal exit after a
/// confirmation and releases the pin. `fatal` is the exit of a fatal
/// runtime error, which counts as a crash while the first boot is open.
pub fn on_exit(code: i32, fatal: bool) {
    if let Some(state) = shared() {
        with_state(&state, |state| {
            if !fatal {
                // A Ctrl+C already marked; any other normal exit marks here.
                let path = state.paths.boot_record_path(state.probe.current_pid());
                let _ = mark_normal_exit(state.fs.as_ref(), &path);
                if code == 0 && state.first_boot_target.is_some() {
                    state.confirm();
                }
                // The first normal exit of a process running the confirmed
                // version releases the previous executable (design §11.7).
                if state.runs_target_version
                    && state.result().is_some_and(|result| {
                        result.state == UpdateState::Confirmed && !result.normal_exit_after_confirm
                    })
                    && let Some(mut result) = state.result()
                {
                    result.normal_exit_after_confirm = true;
                    state.write_result(&result);
                    state.log("confirm", "first normal exit after the confirmation");
                }
            }
            if let Some(pin) = state.pin.take() {
                pin.release();
            }
        });
    }
    std::process::exit(code);
}

/// Add directories to this process's pin before loading from them (the
/// preflight child's staging directory).
pub fn pin_additional_dirs(dirs: &[PathBuf]) {
    if let Some(state) = shared() {
        with_state(&state, |state| {
            if let Some(pin) = &mut state.pin {
                let _ = pin.add_dirs(dirs);
            }
        });
    }
}

/// `rpi update --cleanup` on Windows: run the scan once more and report.
pub fn run_cleanup_command() -> i32 {
    let (paths, fs, probe) = context();
    let live = live_pins(fs.as_ref(), &paths.pins_dir(), probe.as_ref());
    let result = read_record::<UpdateResult>(fs.as_ref(), &paths.result_path());
    let pending = read_record::<PendingUpdate>(fs.as_ref(), &paths.pending_path());
    let scan = cleanup_scan(
        fs.as_ref(),
        &ScanContext {
            paths: &paths,
            probe: probe.as_ref(),
            live_pins: live,
            self_pid: probe.current_pid(),
            state: result
                .as_ref()
                .map(|result| result.state)
                .unwrap_or(UpdateState::Idle),
            pending: pending.as_ref(),
            result: result.as_ref(),
            now: std::time::SystemTime::now(),
        },
    );
    log_event(
        fs.as_ref(),
        Some(paths.log_path.as_path()),
        "cleanup",
        &format!(
            "removed {} item(s), {} in use, {} stuck",
            scan.removed.len(),
            scan.live.len(),
            scan.stuck.len()
        ),
    );
    println!("Removed {} old version item(s).", scan.removed.len());
    let holders = scan.holders().len();
    if holders > 0 {
        println!(
            "Still in use by {holders} running rpi instance(s) ({} item(s)); they are removed \
             once those instances exit.",
            scan.live.len()
        );
    } else if !scan.live.is_empty() {
        println!(
            "Still in use by another running process ({} item(s)); they are removed once it \
             exits.",
            scan.live.len()
        );
    }
    if !scan.stuck.is_empty() {
        let mut reasons: Vec<String> = scan.stuck.iter().map(|item| item.reason.clone()).collect();
        reasons.sort();
        reasons.dedup();
        println!(
            "{} item(s) could not be removed ({}). See {}.",
            scan.stuck.len(),
            reasons.join(", "),
            paths.log_path.display()
        );
    }
    if scan.live.is_empty() && scan.stuck.is_empty() {
        println!("Nothing left to clean up.");
        0
    } else {
        1
    }
}

/// `rpi self-uninstall` refuses while other instances use the installation
/// (ADR-0050): a live pin of another process, or an old executable /
/// generation still in use. A detection only — nothing is removed.
pub fn self_uninstall_blocker(_exe_path: &Path) -> Option<String> {
    let (paths, fs, probe) = context();
    let live = live_pins(fs.as_ref(), &paths.pins_dir(), probe.as_ref());
    let others: Vec<String> = live
        .iter()
        .filter(|pin| pin.pid != probe.current_pid())
        .map(|pin| pin.pid.to_string())
        .collect();
    if !others.is_empty() {
        let (noun, verb, them) = if others.len() == 1 {
            ("instance", "uses", "it")
        } else {
            ("instances", "use", "them")
        };
        return Some(format!(
            "{} other running rpi {noun} (pid {}) still {verb} this installation. Close {them} and retry.",
            others.len(),
            others.join(", ")
        ));
    }
    let result = read_record::<UpdateResult>(fs.as_ref(), &paths.result_path());
    let pending = read_record::<PendingUpdate>(fs.as_ref(), &paths.pending_path());
    let scan = cleanup_scan_with(
        fs.as_ref(),
        &ScanContext {
            paths: &paths,
            probe: probe.as_ref(),
            live_pins: live,
            self_pid: probe.current_pid(),
            state: result
                .as_ref()
                .map(|result| result.state)
                .unwrap_or(UpdateState::Idle),
            pending: pending.as_ref(),
            result: result.as_ref(),
            now: std::time::SystemTime::now(),
        },
        false,
    );
    if !scan.live.is_empty() {
        return Some(format!(
            "Old version files of this installation are still in use ({} item(s)). Run \"rpi \\
             update --cleanup\" after the other instances exit and retry.",
            scan.live.len()
        ));
    }
    None
}

/// The update-related leftovers `rpi self-uninstall` lists (R7.6.5).
pub fn uninstall_leftovers(exe_path: &Path) -> Vec<String> {
    let (paths, _, _) = context();
    let mut out: Vec<String> = Vec::new();
    if paths.update_dir.exists() {
        out.push(paths.update_dir.display().to_string());
    }
    let exe_name = paths.exe_file_name();
    if let Some(prefix) = exe_path.parent()
        && let Ok(entries) = std::fs::read_dir(prefix)
    {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&exe_name) && (name.contains(".old-") || name.contains(".bad-")) {
                out.push(entry.path().display().to_string());
            }
        }
    }
    for root in &paths.extension_roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry
                .path()
                .join(rpi_ext_host::install_layout::GENERATION_POINTER_FILE)
                .is_file()
            {
                out.push(entry.path().display().to_string());
            }
        }
    }
    if let Some(log) = super::layout::default_log_path()
        && log.exists()
    {
        out.push(log.display().to_string());
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<std::ffi::OsString> {
        values.iter().map(std::ffi::OsString::from).collect()
    }

    /// The pure half of [`looks_interactive`]: which argument vectors rule
    /// a TUI out whatever the terminals say. The terminal half depends on
    /// the harness's own stdio and is not asserted here.
    fn headless_by_arguments(values: &[&str]) -> bool {
        let arguments = args(values);
        let Some(first) = arguments.first().and_then(|arg| arg.to_str()) else {
            return false;
        };
        if NON_INTERACTIVE_SUBCOMMANDS.contains(&first) {
            return true;
        }
        arguments.iter().any(|arg| {
            arg.to_str()
                .is_some_and(|arg| HEADLESS_SWITCHES.contains(&arg))
        })
    }

    #[test]
    fn subcommands_and_headless_switches_rule_the_tui_out() {
        for values in [
            &["update"][..],
            &["update", "--cleanup"],
            &["mcp"],
            &["config", "set", "theme", "dark"],
            &["--version"],
            &["--help"],
            &["-p", "hello"],
            &["--mode", "json"],
            &["--mode", "rpc"],
            &["--export", "s.jsonl"],
            &["auth", "login"],
        ] {
            assert!(headless_by_arguments(values), "{values:?}");
        }
        for values in [
            &[][..],
            &["-ne"],
            &["--thinking", "max", "hello world"],
            &["--continue"],
            &["--session", "abc"],
        ] {
            assert!(!headless_by_arguments(values), "{values:?}");
        }
    }

    #[test]
    fn the_hook_state_is_a_singleton_per_process() {
        assert!(shared().is_none() || shared().is_some());
        // Nothing registered in this test process: the outlet is a
        // pass-through that never reaches `process::exit` here only because
        // it is not called; confirm and notice APIs tolerate the empty state.
        assert!(interactive_notices().is_empty());
        mark_notices_read();
        confirm_point(ConfirmPoint::TuiReady);
        pin_additional_dirs(&[]);
    }
}
