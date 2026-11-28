//! First-boot records and process identity (design §11.7).
//!
//! While an applied update waits for its first confirmation, every
//! new-version process writes `.update/boot/<pid>.json` with its pid, its
//! start time and `normalExit=false`. The unified exit outlet (and the
//! Ctrl+C handler) flip the flag; a process that dies without it crashed.
//! Liveness is a pid *plus* its start time, so a reused pid never makes a
//! dead record look alive.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::fs::UpdateFs;
use super::records::{BootRecord, read_record, to_json};

/// Process identity and liveness. Production uses the OS (`OpenProcess` +
/// `GetProcessTimes` on Windows, `/proc` on Unix); tests script it.
pub trait ProcessProbe: Send + Sync {
    fn current_pid(&self) -> u32;
    /// Start time of the calling process in platform ticks.
    fn current_process_start(&self) -> u64;
    /// Start time of `pid`, or `None` when no such process runs.
    fn process_start(&self, pid: u32) -> Option<u64>;
}

/// The scripted probe of the tests: the calling process is fixed, every
/// other pid is a table entry that tests add and remove.
pub struct FakeProcessProbe {
    own_pid: u32,
    own_start: u64,
    others: Mutex<HashMap<u32, u64>>,
}

impl FakeProcessProbe {
    pub fn new(own_pid: u32, own_start: u64) -> Self {
        FakeProcessProbe {
            own_pid,
            own_start,
            others: Mutex::new(HashMap::new()),
        }
    }

    /// Make `pid` live with start time `start`.
    pub fn spawn(&self, pid: u32, start: u64) {
        self.others
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(pid, start);
    }

    /// Make `pid` dead (exited or its pid was reused by another process).
    pub fn kill(&self, pid: u32) {
        self.others
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&pid);
    }
}

impl ProcessProbe for FakeProcessProbe {
    fn current_pid(&self) -> u32 {
        self.own_pid
    }

    fn current_process_start(&self) -> u64 {
        self.own_start
    }

    fn process_start(&self, pid: u32) -> Option<u64> {
        if pid == self.own_pid {
            return Some(self.own_start);
        }
        self.others
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&pid)
            .copied()
    }
}

/// Whether the process a record was written for still runs: the pid exists
/// and started at the recorded time (a reused pid is a different process).
pub fn is_live(probe: &dyn ProcessProbe, pid: u32, process_start: u64) -> bool {
    probe.process_start(pid) == Some(process_start)
}

/// Unix start time in clock ticks since boot (`/proc/<pid>/stat` field 22).
#[cfg(unix)]
fn unix_process_start(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The comm field may contain spaces and parentheses: everything after
    // the last `)` is fixed-width.
    let tail = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = tail.split_whitespace().collect();
    // `state` is field 3; starttime is field 22 → index 19 of the tail.
    fields.get(19)?.trim().parse().ok()
}

/// The real probe of the running system.
pub struct SystemProcessProbe {
    own_pid: u32,
    own_start: Option<u64>,
}

impl Default for SystemProcessProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemProcessProbe {
    pub fn new() -> Self {
        let own_pid = std::process::id();
        let own_start = Self::start_of(own_pid);
        SystemProcessProbe { own_pid, own_start }
    }

    fn start_of(pid: u32) -> Option<u64> {
        #[cfg(windows)]
        {
            super::platform::process_start_time(pid)
        }
        #[cfg(unix)]
        {
            unix_process_start(pid)
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = pid;
            None
        }
    }
}

impl ProcessProbe for SystemProcessProbe {
    fn current_pid(&self) -> u32 {
        self.own_pid
    }

    fn current_process_start(&self) -> u64 {
        self.own_start.unwrap_or(0)
    }

    fn process_start(&self, pid: u32) -> Option<u64> {
        Self::start_of(pid)
    }
}

/// Write the caller's boot record (start of a new-version process while the
/// first boot is open).
pub fn write_boot_record(
    fs: &dyn UpdateFs,
    path: &Path,
    record: &BootRecord,
) -> std::io::Result<()> {
    fs.create_dir_all(path.parent().unwrap_or_else(|| Path::new(".")))?;
    fs.write_atomic(path, to_json(record).as_bytes())
}

/// Flip `normalExit` on the record at `path`. A missing record (already
/// cleaned up by a confirmation or rollback) is not an error.
pub fn mark_normal_exit(fs: &dyn UpdateFs, path: &Path) -> std::io::Result<()> {
    let Some(mut record) = read_record::<BootRecord>(fs, path) else {
        return Ok(());
    };
    record.normal_exit = true;
    fs.write_atomic(path, to_json(&record).as_bytes())
}

/// Every readable boot record with its file.
pub fn read_boot_records(fs: &dyn UpdateFs, boot_dir: &Path) -> Vec<(PathBuf, BootRecord)> {
    let Ok(entries) = fs.read_dir(boot_dir) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter(|entry| !entry.is_dir && entry.path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| {
            read_record::<BootRecord>(fs, &entry.path).map(|record| (entry.path.clone(), record))
        })
        .collect()
}

/// Remove every boot record (confirmation and rollback clear them all, so a
/// stale record never poisons the next update's first boot).
pub fn clear_boot_records(fs: &dyn UpdateFs, boot_dir: &Path) {
    for (path, _) in read_boot_records(fs, boot_dir) {
        let _ = fs.remove_file(&path);
    }
}

/// What the boot records say about an open first boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirstBootVerdict {
    /// No record of the target version: nothing ever started (or the
    /// records were already cleared).
    NoRecords,
    /// Every recorded process has exited with a normal-exit mark (e.g. a
    /// one-shot command that failed): wait for a later confirmation.
    AllExitedNormally,
    /// At least one recorded process still runs: another instance is in its
    /// first boot; this start stays out of the judgement.
    Live,
    /// A recorded process died without the normal-exit mark: the target
    /// version crashed on its first boot.
    Crashed(PathBuf, BootRecord),
}

/// Judge the records of `target_version` only: a process of another version
/// (an old instance that started while the update was already applied) never
/// writes one, and a record of another version never counts.
///
/// A crashed record decides first (design §11.7 "存在…未标记 normalExit 的
/// 记录，执行回滚流程"): rollback is idempotent and keeps live instances
/// working, so one crash is enough. Without one, a live record keeps this
/// start out of the judgement; otherwise every record exited normally.
pub fn judge_first_boot(
    fs: &dyn UpdateFs,
    boot_dir: &Path,
    probe: &dyn ProcessProbe,
    target_version: &str,
) -> FirstBootVerdict {
    let records: Vec<(PathBuf, BootRecord)> = read_boot_records(fs, boot_dir)
        .into_iter()
        .filter(|(_, record)| record.version == target_version)
        .collect();
    if let Some((path, record)) = records.iter().find(|(_, record)| {
        !is_live(probe, record.pid, record.process_start) && !record.normal_exit
    }) {
        return FirstBootVerdict::Crashed(path.clone(), record.clone());
    }
    if records
        .iter()
        .any(|(_, record)| is_live(probe, record.pid, record.process_start))
    {
        return FirstBootVerdict::Live;
    }
    if records.is_empty() {
        FirstBootVerdict::NoRecords
    } else {
        FirstBootVerdict::AllExitedNormally
    }
}

/// The record a new-version process writes for itself at the start of its
/// first boot.
pub fn own_boot_record(probe: &dyn ProcessProbe, version: &str) -> BootRecord {
    BootRecord {
        schema_version: super::records::STATE_SCHEMA_VERSION,
        pid: probe.current_pid(),
        process_start: probe.current_process_start(),
        version: version.to_owned(),
        normal_exit: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::windows_update::fs::SpyFs;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rpi-wu-boot-{}-{}",
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
    fn boot_record_round_trips_and_marks_the_normal_exit() {
        let fs = SpyFs::new();
        let dir = temp_dir();
        let probe = FakeProcessProbe::new(11, 100);
        let path = dir.join("11.json");
        let record = own_boot_record(&probe, "0.1.9-rc.4");
        write_boot_record(&fs, &path, &record).unwrap();
        assert!(!read_record::<BootRecord>(&fs, &path).unwrap().normal_exit);
        mark_normal_exit(&fs, &path).unwrap();
        let marked = read_record::<BootRecord>(&fs, &path).unwrap();
        assert!(marked.normal_exit);
        assert_eq!(marked.pid, 11);
        assert_eq!(marked.process_start, 100);
        // A missing record is fine (already cleaned up).
        mark_normal_exit(&fs, &dir.join("999.json")).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fake_probe_distinguishes_live_dead_and_reused_pids() {
        let probe = FakeProcessProbe::new(1, 10);
        probe.spawn(2, 20);
        assert!(is_live(&probe, 1, 10));
        assert!(is_live(&probe, 2, 20));
        // A reused pid has a different start time: the old record is dead.
        assert!(!is_live(&probe, 2, 21));
        probe.kill(2);
        assert!(!is_live(&probe, 2, 20));
        assert_eq!(probe.process_start(3), None);
    }

    fn record(pid: u32, start: u64, version: &str, normal_exit: bool) -> BootRecord {
        BootRecord {
            schema_version: super::super::records::STATE_SCHEMA_VERSION,
            pid,
            process_start: start,
            version: version.to_owned(),
            normal_exit,
        }
    }

    #[test]
    fn first_boot_verdicts_follow_the_records_of_the_target_version() {
        let fs = SpyFs::new();
        let dir = temp_dir();
        let probe = FakeProcessProbe::new(1000, 1);

        // Nothing recorded.
        assert_eq!(
            judge_first_boot(&fs, &dir, &probe, "1.0.0"),
            FirstBootVerdict::NoRecords
        );

        // Another version's record never counts.
        write_boot_record(&fs, &dir.join("5.json"), &record(5, 50, "0.9.0", false)).unwrap();
        assert_eq!(
            judge_first_boot(&fs, &dir, &probe, "1.0.0"),
            FirstBootVerdict::NoRecords
        );

        // A live first boot: this start stays out of the judgement.
        probe.spawn(6, 60);
        write_boot_record(&fs, &dir.join("6.json"), &record(6, 60, "1.0.0", false)).unwrap();
        assert_eq!(
            judge_first_boot(&fs, &dir, &probe, "1.0.0"),
            FirstBootVerdict::Live
        );

        // Exited normally (a failed one-shot command): no crash.
        probe.kill(6);
        write_boot_record(&fs, &dir.join("6.json"), &record(6, 60, "1.0.0", true)).unwrap();
        assert_eq!(
            judge_first_boot(&fs, &dir, &probe, "1.0.0"),
            FirstBootVerdict::AllExitedNormally
        );

        // A reused pid is not the recorded process: the record is dead.
        probe.spawn(6, 61);
        write_boot_record(&fs, &dir.join("6.json"), &record(6, 60, "1.0.0", false)).unwrap();
        assert!(matches!(
            judge_first_boot(&fs, &dir, &probe, "1.0.0"),
            FirstBootVerdict::Crashed(..)
        ));

        // Exited without the mark: a crash.
        probe.kill(6);
        assert!(matches!(
            judge_first_boot(&fs, &dir, &probe, "1.0.0"),
            FirstBootVerdict::Crashed(..)
        ));

        // A crash decides even while another first-boot instance is alive.
        probe.spawn(7, 70);
        write_boot_record(&fs, &dir.join("7.json"), &record(7, 70, "1.0.0", false)).unwrap();
        assert!(matches!(
            judge_first_boot(&fs, &dir, &probe, "1.0.0"),
            FirstBootVerdict::Crashed(..)
        ));
        // With the crashed record gone, the live instance keeps this start
        // out of the judgement.
        fs.remove_file(&dir.join("6.json")).unwrap();
        assert_eq!(
            judge_first_boot(&fs, &dir, &probe, "1.0.0"),
            FirstBootVerdict::Live
        );

        clear_boot_records(&fs, &dir);
        assert_eq!(
            judge_first_boot(&fs, &dir, &probe, "1.0.0"),
            FirstBootVerdict::NoRecords
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn system_probe_sees_the_calling_process() {
        let probe = SystemProcessProbe::new();
        assert_eq!(probe.current_pid(), std::process::id());
        let start = probe.current_process_start();
        assert!(is_live(&probe, std::process::id(), start));
        assert!(probe.process_start(u32::MAX).is_none() || !is_live(&probe, u32::MAX, start));
    }
}
