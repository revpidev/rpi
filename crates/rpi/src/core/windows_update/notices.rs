//! The three startup notices (design §11.9, R7.5.2–R7.5.4).
//!
//! Pure classification and English text, so every sentence is asserted by
//! a test. The interactive start shows a reminder, a degradation or a
//! rollback once (then marks the result read) and at most one warning per
//! start; a non-interactive start writes only the warning, the degradation
//! and the rollback to stderr and marks nothing read.

use std::path::Path;

use super::gc::ScanReport;
use super::records::{PendingUpdate, PinRecord, UpdateResult, UpdateState};

/// One startup notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupNotice {
    /// The first interactive start after an update.
    FirstBootReminder {
        from: String,
        to: String,
        /// The previous version's files are already gone (no trampoline and
        /// this start's scan removed them).
        cleaned: bool,
        /// Other running instances still on the previous version.
        others_on_old: usize,
    },
    /// Old files in use and/or stuck; one per start at most.
    Warning {
        line: String,
    },
    Degraded {
        from: String,
        to: String,
    },
    RolledBack {
        from: String,
        to: String,
    },
}

impl StartupNotice {
    pub fn text(&self, log_path: &Path) -> String {
        match self {
            StartupNotice::FirstBootReminder {
                from,
                to,
                cleaned,
                others_on_old,
            } => reminder_text(from, to, *cleaned, *others_on_old),
            StartupNotice::Warning { line } => line.clone(),
            StartupNotice::Degraded { from, to } => degraded_text(from, to, log_path),
            StartupNotice::RolledBack { from, to } => rolled_back_text(from, to, log_path),
        }
    }

    /// Whether the notice goes to stderr in a non-interactive start.
    pub fn noninteractive_stderr(&self) -> bool {
        matches!(
            self,
            StartupNotice::Warning { .. }
                | StartupNotice::Degraded { .. }
                | StartupNotice::RolledBack { .. }
        )
    }

    /// Whether showing it marks the update result read.
    pub fn marks_read(&self) -> bool {
        matches!(
            self,
            StartupNotice::FirstBootReminder { .. }
                | StartupNotice::Degraded { .. }
                | StartupNotice::RolledBack { .. }
        )
    }
}

/// What the classification needs.
pub struct NoticeInput<'a> {
    pub state: UpdateState,
    pub result: Option<&'a UpdateResult>,
    pub pending: Option<&'a PendingUpdate>,
    pub scan: &'a ScanReport,
    /// This process was launched by the trampoline: the first boot.
    pub first_boot_launch: bool,
    pub self_pid: u32,
    pub trampoline_pid: Option<u32>,
    pub live_pins: &'a [PinRecord],
}

pub fn reminder_text(from: &str, to: &str, cleaned: bool, others_on_old: usize) -> String {
    let mut text = format!(
        "Updated {}.",
        crate::core::version_check::version_transition(from, to)
    );
    if cleaned {
        text.push_str(" The previous version has been removed.");
    } else {
        text.push_str(
            " The previous version is removed automatically the next time rpi starts after you \
             exit this session.",
        );
    }
    if others_on_old > 0 {
        let plural = if others_on_old == 1 {
            "instance is"
        } else {
            "instances are"
        };
        text.push_str(&format!(
            " {others_on_old} other running rpi {plural} still on rpi {from}; they switch over \
             after they restart."
        ));
    }
    text
}

fn instance_plural(count: usize) -> &'static str {
    if count == 1 { "instance" } else { "instances" }
}

fn file_plural(count: usize) -> &'static str {
    if count == 1 { "file" } else { "files" }
}

/// The one warning of a start (in use and/or stuck), if any. In-use files
/// whose holder is a pinned rpi instance name its count; files the DELETE
/// probe found held by something else (no pin: a scanner, an instance older
/// than the pins) are reported without a count.
pub fn warning_text(scan: &ScanReport, log_path: &Path) -> Option<String> {
    let in_use = scan.holders().len();
    let stuck = scan.stuck.len();
    if scan.live.is_empty() && stuck == 0 {
        return None;
    }
    let in_use_text = |n: usize| -> String {
        if n > 0 {
            format!("by {n} running rpi {}", instance_plural(n))
        } else {
            "by another running process".to_owned()
        }
    };
    let reasons: Vec<String> = {
        let mut reasons: Vec<String> = scan.stuck.iter().map(|item| item.reason.clone()).collect();
        reasons.sort();
        reasons.dedup();
        reasons
    };
    let reason = reasons.join(", ");
    match (in_use, stuck) {
        (n, 0) => Some(format!(
            "Old version files are still in use {}. rpi removes them on a later start; run \
             \"rpi update --cleanup\" to retry.",
            in_use_text(n)
        )),
        (0, m) => Some(format!(
            "{m} old version {} could not be removed ({reason}). See {}. Run \"rpi update \
             --cleanup\" to retry.",
            file_plural(m),
            log_path.display()
        )),
        (n, m) => Some(format!(
            "Old version files are still in use {} and {m} {} could not be removed ({reason}). \
             rpi removes them on a later start; run \"rpi update --cleanup\" to retry. See {}.",
            in_use_text(n),
            file_plural(m),
            log_path.display()
        )),
    }
}

pub fn degraded_text(from: &str, to: &str, log_path: &Path) -> String {
    format!(
        "The update to rpi {to} could not be applied. rpi is still on rpi {from}; see {} for \
         the manual steps.",
        log_path.display()
    )
}

pub fn rolled_back_text(from: &str, to: &str, log_path: &Path) -> String {
    format!(
        "The update to rpi {to} failed and was rolled back to rpi {from}. See {}.",
        log_path.display()
    )
}

/// The notices of this start, in display order.
pub fn build_notices(input: &NoticeInput<'_>, log_path: &Path) -> Vec<StartupNotice> {
    let mut notices = Vec::new();
    let read = input.result.is_some_and(|result| result.read);
    match input.state {
        // The reminder follows the update through its whole life: the
        // versions come from the pending while the update waits for its
        // first start (`applied`) and from the update result afterwards
        // (`confirmed`, where the confirmation already deleted the pending).
        UpdateState::Applied | UpdateState::Confirmed => {
            // A target without the first-boot protocol cannot show this
            // reminder (it has no start notices), so nothing is kept for
            // it: a later version must not inherit a stale reminder.
            let versions = input
                .pending
                .filter(|pending| pending.applied && pending.first_boot_protocol)
                .map(|pending| (pending.from_version.clone(), pending.target_version.clone()))
                .or_else(|| {
                    input
                        .result
                        .filter(|result| result.first_boot_protocol)
                        .and_then(|result| {
                            result.from_version.clone().zip(result.to_version.clone())
                        })
                });
            if let Some((from, to)) = versions
                && !read
            {
                // Other instances still on the previous version: the
                // holders of the parked `.old` image (by image identity),
                // plus — for a cross-version update, where the version
                // string does identify the previous image — every pinned
                // instance of that version. A same-version reinstall parks
                // an image whose version string every new instance shares,
                // so there the image identity alone counts.
                let mut others_on_old: Vec<u32> = input.scan.old_exe_holders.clone();
                for live in &input.scan.live {
                    if live.kind == crate::core::windows_update::gc::LeftoverKind::OldExe {
                        others_on_old.extend(live.holders.iter().copied());
                    }
                }
                if from != to {
                    others_on_old.extend(
                        input
                            .live_pins
                            .iter()
                            .filter(|pin| pin.exe_version == from)
                            .map(|pin| pin.pid),
                    );
                }
                others_on_old.sort_unstable();
                others_on_old.dedup();
                let others_on_old = others_on_old
                    .into_iter()
                    .filter(|pid| *pid != input.self_pid && Some(*pid) != input.trampoline_pid)
                    .count();
                notices.push(StartupNotice::FirstBootReminder {
                    from,
                    to,
                    cleaned: !input.first_boot_launch && input.scan.old_exe_gone,
                    others_on_old,
                });
            }
        }
        UpdateState::Degraded => {
            if !read {
                notices.push(StartupNotice::Degraded {
                    from: input
                        .result
                        .and_then(|result| result.from_version.clone())
                        .unwrap_or_default(),
                    to: input
                        .result
                        .and_then(|result| result.to_version.clone())
                        .unwrap_or_default(),
                });
            }
        }
        UpdateState::RolledBack => {
            if !read {
                notices.push(StartupNotice::RolledBack {
                    from: input
                        .result
                        .and_then(|result| result.from_version.clone())
                        .unwrap_or_default(),
                    to: input
                        .result
                        .and_then(|result| result.to_version.clone())
                        .unwrap_or_default(),
                });
            }
        }
        UpdateState::Staged | UpdateState::Idle => {}
    }
    // In-use files belong to the starts after the first boot (the
    // reminder covers them there); a stuck leftover is reported on every
    // start, and both merge into one warning.
    let warning_scan = if input.first_boot_launch {
        ScanReport {
            live: Vec::new(),
            stuck: input.scan.stuck.clone(),
            ..ScanReport::default()
        }
    } else {
        input.scan.clone()
    };
    if let Some(line) = warning_text(&warning_scan, log_path) {
        notices.push(StartupNotice::Warning { line });
    }
    notices
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::windows_update::gc::{LeftoverKind, LiveLeftover, StuckLeftover};
    use crate::core::windows_update::records::PinRecord;

    const LOG: &str = "/logs/update.log";

    fn log_path() -> &'static Path {
        Path::new(LOG)
    }

    fn applied(from: &str, to: &str) -> PendingUpdate {
        PendingUpdate {
            schema_version: 1,
            target_version: to.to_owned(),
            channel: "stable".to_owned(),
            written_at: "2026-11-28T00:00:00Z".to_owned(),
            from_version: from.to_owned(),
            allow_non_monotonic: false,
            first_boot_protocol: true,
            applied: true,
            binary: crate::core::windows_update::records::StagedBinary {
                staged_path: std::path::PathBuf::from("/x"),
                sha256: String::new(),
                source_url: String::new(),
                asset_sha256: String::new(),
                target: String::new(),
            },
            extensions: Vec::new(),
        }
    }

    fn pin(pid: u32, exe_version: &str) -> PinRecord {
        PinRecord::new(
            pid,
            pid as u64,
            format!("s{pid}"),
            exe_version,
            Path::new("/rpi.exe"),
        )
    }

    #[test]
    fn reminder_texts() {
        assert_eq!(
            reminder_text("0.1.9-rc.3", "0.1.9-rc.4", false, 0),
            "Updated rpi 0.1.9-rc.3 → 0.1.9-rc.4. The previous version is removed \
             automatically the next time rpi starts after you exit this session."
        );
        assert_eq!(
            reminder_text("0.1.9-rc.3", "0.1.9-rc.4", true, 0),
            "Updated rpi 0.1.9-rc.3 → 0.1.9-rc.4. The previous version has been removed."
        );
        assert_eq!(
            reminder_text("0.1.9-rc.3", "0.1.9-rc.4", false, 1),
            "Updated rpi 0.1.9-rc.3 → 0.1.9-rc.4. The previous version is removed \
             automatically the next time rpi starts after you exit this session. 1 other running \
             rpi instance is still on rpi 0.1.9-rc.3; they switch over after they restart."
        );
        assert!(
            reminder_text("a", "b", false, 2)
                .contains("2 other running rpi instances are still on rpi a;")
        );
    }

    #[test]
    fn warning_texts() {
        let mut scan = ScanReport::default();
        assert_eq!(warning_text(&scan, log_path()), None);
        scan.live.push(LiveLeftover {
            kind: LeftoverKind::OldExe,
            path: "/o".into(),
            holders: vec![7],
        });
        scan.live.push(LiveLeftover {
            kind: LeftoverKind::Generation,
            path: "/g".into(),
            holders: vec![7, 9],
        });
        assert_eq!(
            warning_text(&scan, log_path()).as_deref(),
            Some(
                "Old version files are still in use by 2 running rpi instances. rpi removes them \
                 on a later start; run \"rpi update --cleanup\" to retry."
            )
        );
        let mut scan = ScanReport::default();
        scan.stuck.push(StuckLeftover {
            kind: LeftoverKind::NewExe,
            path: "/a".into(),
            reason: "access denied".to_owned(),
        });
        assert_eq!(
            warning_text(&scan, log_path()).as_deref(),
            Some(
                "1 old version file could not be removed (access denied). See /logs/update.log. \
                 Run \"rpi update --cleanup\" to retry."
            )
        );
        scan.stuck.push(StuckLeftover {
            kind: LeftoverKind::Generation,
            path: "/b".into(),
            reason: "access denied".to_owned(),
        });
        scan.stuck.push(StuckLeftover {
            kind: LeftoverKind::Generation,
            path: "/c".into(),
            reason: "I/O error".to_owned(),
        });
        scan.live.push(LiveLeftover {
            kind: LeftoverKind::OldExe,
            path: "/o".into(),
            holders: vec![7],
        });
        assert_eq!(
            warning_text(&scan, log_path()).as_deref(),
            Some(
                "Old version files are still in use by 1 running rpi instance and 3 files could \
                 not be removed (I/O error, access denied). rpi removes them on a later start; run \
                 \"rpi update --cleanup\" to retry. See /logs/update.log."
            )
        );
    }

    #[test]
    fn degraded_and_rollback_texts() {
        assert_eq!(
            degraded_text("1.0.0", "1.1.0", log_path()),
            "The update to rpi 1.1.0 could not be applied. rpi is still on rpi 1.0.0; see \
             /logs/update.log for the manual steps."
        );
        assert_eq!(
            rolled_back_text("1.0.0", "1.1.0", log_path()),
            "The update to rpi 1.1.0 failed and was rolled back to rpi 1.0.0. See \
             /logs/update.log."
        );
    }

    #[test]
    fn the_first_boot_launch_shows_the_reminder_without_an_in_use_warning() {
        let pending = applied("0.1.9-rc.3", "0.1.9-rc.4");
        let result = UpdateResult::new(UpdateState::Applied, None, None);
        let mut scan = ScanReport::default();
        scan.live.push(LiveLeftover {
            kind: LeftoverKind::OldExe,
            path: "/o".into(),
            holders: vec![77],
        });
        scan.retained.push("/retained".into());
        let pins = vec![
            pin(77, "0.1.9-rc.3"),
            pin(4242, "0.1.9-rc.4"),
            pin(88, "0.1.9-rc.3"),
        ];
        let notices = build_notices(
            &NoticeInput {
                state: UpdateState::Applied,
                result: Some(&result),
                pending: Some(&pending),
                scan: &scan,
                first_boot_launch: true,
                self_pid: 4242,
                trampoline_pid: Some(77),
                live_pins: &pins,
            },
            log_path(),
        );
        assert_eq!(
            notices,
            vec![StartupNotice::FirstBootReminder {
                from: "0.1.9-rc.3".to_owned(),
                to: "0.1.9-rc.4".to_owned(),
                cleaned: false,
                others_on_old: 1,
            }]
        );
        let reminder = &notices[0];
        assert!(reminder.marks_read());
        assert!(
            !reminder.noninteractive_stderr(),
            "the reminder never goes to stderr"
        );

        // Read already (shown by an earlier interactive start): nothing.
        let read = UpdateResult {
            read: true,
            ..result.clone()
        };
        assert!(
            build_notices(
                &NoticeInput {
                    state: UpdateState::Applied,
                    result: Some(&read),
                    pending: Some(&pending),
                    scan: &scan,
                    first_boot_launch: true,
                    self_pid: 4242,
                    trampoline_pid: Some(77),
                    live_pins: &pins,
                },
                log_path()
            )
            .is_empty()
        );
    }

    #[test]
    fn later_starts_warn_while_old_files_are_in_use_and_stop_when_they_are_gone() {
        let result = UpdateResult {
            read: true,
            ..UpdateResult::new(UpdateState::Confirmed, None, None)
        };
        let mut scan = ScanReport::default();
        scan.live.push(LiveLeftover {
            kind: LeftoverKind::OldExe,
            path: "/o".into(),
            holders: vec![88],
        });
        let pins = vec![pin(4242, "0.1.9-rc.4"), pin(88, "0.1.9-rc.3")];
        let notices = build_notices(
            &NoticeInput {
                state: UpdateState::Confirmed,
                result: Some(&result),
                pending: None,
                scan: &scan,
                first_boot_launch: false,
                self_pid: 4242,
                trampoline_pid: None,
                live_pins: &pins,
            },
            log_path(),
        );
        assert_eq!(notices.len(), 1);
        assert!(
            matches!(&notices[0], StartupNotice::Warning { line } if line.contains("1 running rpi instance"))
        );
        assert!(notices[0].noninteractive_stderr());
        assert!(!notices[0].marks_read());

        // The instances exited: no warning any more.
        let scan = ScanReport::default();
        assert!(
            build_notices(
                &NoticeInput {
                    state: UpdateState::Confirmed,
                    result: Some(&result),
                    pending: None,
                    scan: &scan,
                    first_boot_launch: false,
                    self_pid: 4242,
                    trampoline_pid: None,
                    live_pins: &pins,
                },
                log_path(),
            )
            .is_empty()
        );
    }

    #[test]
    fn a_first_boot_without_a_trampoline_reports_the_completed_cleanup_and_a_stuck_warning() {
        let pending = applied("0.1.9-rc.3", "0.1.9-rc.4");
        let result = UpdateResult::new(UpdateState::Applied, None, None);
        let mut scan = ScanReport {
            old_exe_gone: true,
            ..ScanReport::default()
        };
        scan.stuck.push(StuckLeftover {
            kind: LeftoverKind::Generation,
            path: "/g".into(),
            reason: "in use".to_owned(),
        });
        let pins = vec![pin(4242, "0.1.9-rc.4")];
        let notices = build_notices(
            &NoticeInput {
                state: UpdateState::Applied,
                result: Some(&result),
                pending: Some(&pending),
                scan: &scan,
                first_boot_launch: false,
                self_pid: 4242,
                trampoline_pid: None,
                live_pins: &pins,
            },
            log_path(),
        );
        assert_eq!(notices.len(), 2);
        assert!(matches!(
            &notices[0],
            StartupNotice::FirstBootReminder {
                cleaned: true,
                others_on_old: 0,
                ..
            }
        ));
        assert!(
            matches!(&notices[1], StartupNotice::Warning { line } if line == "1 old version file could not be removed (in use). See /logs/update.log. Run \"rpi update --cleanup\" to retry.")
        );
    }

    #[test]
    fn a_first_boot_launch_with_only_stuck_files_still_warns() {
        let read = UpdateResult {
            read: true,
            ..UpdateResult::new(UpdateState::Confirmed, None, None)
        };
        let mut scan = ScanReport::default();
        scan.live.push(LiveLeftover {
            kind: LeftoverKind::OldExe,
            path: "/o".into(),
            holders: vec![77],
        });
        scan.stuck.push(StuckLeftover {
            kind: LeftoverKind::FlatFiles,
            path: "/f".into(),
            reason: "access denied".to_owned(),
        });
        let pins = Vec::new();
        let notices = build_notices(
            &NoticeInput {
                state: UpdateState::Confirmed,
                result: Some(&read),
                pending: None,
                scan: &scan,
                first_boot_launch: true,
                self_pid: 4242,
                trampoline_pid: Some(77),
                live_pins: &pins,
            },
            log_path(),
        );
        assert_eq!(notices.len(), 1);
        assert!(
            matches!(&notices[0], StartupNotice::Warning { line } if line.starts_with("1 old version file could not be removed (access denied)"))
        );
    }

    /// The steady state after a confirmation: the pending is gone, the
    /// update result alone drives the reminder (when unread) and the
    /// warnings are still classified.
    #[test]
    fn a_confirmed_result_without_a_pending_still_shows_the_reminder_and_warns() {
        let mut result = UpdateResult::new(
            UpdateState::Confirmed,
            Some("0.1.9-rc.3".to_owned()),
            Some("0.1.9-rc.4".to_owned()),
        );
        result.first_boot_protocol = true;
        let mut scan = ScanReport::default();
        scan.live.push(LiveLeftover {
            kind: LeftoverKind::OldExe,
            path: "/o".into(),
            holders: vec![88],
        });
        scan.stuck.push(StuckLeftover {
            kind: LeftoverKind::Generation,
            path: "/g".into(),
            reason: "access denied".to_owned(),
        });
        // `rpi --version` confirmed, then the TUI starts: not the first
        // boot (no trampoline), so the in-use warning applies.
        let notices = build_notices(
            &NoticeInput {
                state: UpdateState::Confirmed,
                result: Some(&result),
                pending: None,
                scan: &scan,
                first_boot_launch: false,
                self_pid: 4242,
                trampoline_pid: None,
                live_pins: &[],
            },
            log_path(),
        );
        assert_eq!(notices.len(), 2);
        // The instance that holds the old image is "another instance
        // still on the previous version".
        assert!(
            matches!(&notices[0], StartupNotice::FirstBootReminder { from, to, cleaned: false, others_on_old: 1 } if from == "0.1.9-rc.3" && to == "0.1.9-rc.4")
        );
        assert!(
            matches!(&notices[1], StartupNotice::Warning { line } if line.contains("1 running rpi instance and 1 file could not be removed"))
        );
        // Read: the reminder is gone, the warning stays.
        let read = UpdateResult {
            read: true,
            ..result.clone()
        };
        let notices = build_notices(
            &NoticeInput {
                state: UpdateState::Confirmed,
                result: Some(&read),
                pending: None,
                scan: &scan,
                first_boot_launch: false,
                self_pid: 4242,
                trampoline_pid: None,
                live_pins: &[],
            },
            log_path(),
        );
        assert_eq!(notices.len(), 1);
        assert!(matches!(&notices[0], StartupNotice::Warning { .. }));
    }

    /// A target without the first-boot protocol cannot show the reminder;
    /// a later version that reads its result shows nothing stale either.
    #[test]
    fn a_target_without_the_protocol_leaves_no_reminder_behind() {
        let mut result = UpdateResult::new(
            UpdateState::Confirmed,
            Some("0.1.9-rc.3".to_owned()),
            Some("0.1.9-rc.2".to_owned()),
        );
        result.first_boot_protocol = false;
        result.read = false;
        let notices = build_notices(
            &NoticeInput {
                state: UpdateState::Confirmed,
                result: Some(&result),
                pending: None,
                scan: &ScanReport::default(),
                first_boot_launch: false,
                self_pid: 4242,
                trampoline_pid: None,
                live_pins: &[],
            },
            log_path(),
        );
        assert!(notices.is_empty(), "{notices:?}");
    }

    #[test]
    fn degraded_and_rolled_back_results_show_once_until_read() {
        let degraded = UpdateResult::new(
            UpdateState::Degraded,
            Some("1.0.0".to_owned()),
            Some("1.1.0".to_owned()),
        );
        let rolled = UpdateResult::new(
            UpdateState::RolledBack,
            Some("1.0.0".to_owned()),
            Some("1.1.0".to_owned()),
        );
        let scan = ScanReport::default();
        let pins = Vec::new();
        let build = |result: &UpdateResult| {
            build_notices(
                &NoticeInput {
                    state: result.state,
                    result: Some(result),
                    pending: None,
                    scan: &scan,
                    first_boot_launch: false,
                    self_pid: 1,
                    trampoline_pid: None,
                    live_pins: &pins,
                },
                log_path(),
            )
        };
        let notices = build(&degraded);
        assert_eq!(
            notices,
            vec![StartupNotice::Degraded {
                from: "1.0.0".to_owned(),
                to: "1.1.0".to_owned()
            }]
        );
        assert!(notices[0].noninteractive_stderr() && notices[0].marks_read());
        let notices = build(&rolled);
        assert!(matches!(&notices[0], StartupNotice::RolledBack { .. }));
        assert_eq!(
            notices[0].text(log_path()),
            "The update to rpi 1.1.0 failed and was rolled back to rpi 1.0.0. See \
             /logs/update.log."
        );
        let read = UpdateResult {
            read: true,
            ..degraded.clone()
        };
        assert!(build(&read).is_empty());
        let idle = UpdateResult::new(UpdateState::Idle, None, None);
        let staged = UpdateResult::new(UpdateState::Staged, None, None);
        assert!(build(&idle).is_empty() && build(&staged).is_empty());
    }
}
