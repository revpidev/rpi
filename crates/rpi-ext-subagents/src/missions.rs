//! Durable missions (V19-03 S02; upstream `src/missions/*` @ v0.88.1).
//!
//! rpi has no automatic mission store; this module implements the explicit
//! `mission` / `missionId` launch face and the `mission.show` / `mission.close`
//! management actions. The completion rule is the #3078 fix: an explicitly
//! requested mission stays `waiting` while any linked run (or workflow child)
//! reports `pending`, `claimed`, `review-required` or `rejected` acceptance.
//! Auto-created missions keep the old "all runs complete ⇒ completed" rule.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MISSION_STATUSES: [&str; 7] = [
    "planned",
    "active",
    "waiting",
    "needs_decision",
    "completed",
    "failed",
    "cancelled",
];

/// Acceptance ledger statuses that mean nobody accepted the result.
const UNACCEPTED: [&str; 4] = ["pending", "claimed", "review-required", "rejected"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MissionRunLink {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub async_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_index: Option<u32>,
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance: Option<String>,
    /// False for an explicit `mission`/`missionId` request.
    #[serde(default = "default_auto_created")]
    pub auto_created: bool,
}

fn default_auto_created() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MissionChildLink {
    pub key: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MissionRecord {
    pub schema_version: u32,
    pub id: String,
    pub title: String,
    pub objective: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_session_id: Option<String>,
    #[serde(default)]
    pub runs: Vec<MissionRunLink>,
    #[serde(default)]
    pub workflow_children: Vec<MissionChildLink>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl MissionRecord {
    pub fn explicit(&self) -> bool {
        self.runs.iter().any(|run| !run.auto_created)
    }
}

/// The explicit launch request resolved from `mission` / `missionId`.
#[derive(Debug, Clone)]
pub struct MissionLaunch {
    pub id: String,
    pub auto_created: bool,
}

static PENDING: std::sync::LazyLock<Mutex<HashMap<String, MissionLaunch>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

fn now_iso() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    crate::artifacts::format_iso8601(millis)
}

/// `<project>/.rpi/subagents/missions`.
pub fn missions_dir(cwd: &Path) -> PathBuf {
    cwd.join(crate::paths::PROJECT_SUBAGENTS_RELATIVE_DIR)
        .join("missions")
}

fn record_path(cwd: &Path, id: &str) -> PathBuf {
    missions_dir(cwd).join(format!("{id}.json"))
}

fn valid_mission_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn write_record(record: &MissionRecord) -> Result<(), String> {
    let path = record_path(Path::new(&record.cwd), &record.id);
    if let Some(parent) = path.parent() {
        crate::paths::create_private_dir_all(parent)
            .map_err(|error| format!("Failed to create mission folder: {error}"))?;
    }
    let raw = serde_json::to_string_pretty(record)
        .map_err(|error| format!("Failed to serialize mission: {error}"))?;
    crate::artifacts::write_metadata(&path, &serde_json::from_str(&raw).unwrap_or(Value::Null))
        .map_err(|error| format!("Failed to write mission '{}': {error}", path.display()))
}

pub fn read_record(cwd: &Path, id: &str) -> Result<MissionRecord, String> {
    if !valid_mission_id(id) {
        return Err(format!("Invalid mission id '{id}'."));
    }
    let path = record_path(cwd, id);
    let raw = std::fs::read_to_string(&path).map_err(|error| {
        format!(
            "Mission '{id}' was not found at '{}': {error}",
            path.display()
        )
    })?;
    serde_json::from_str(&raw)
        .map_err(|error| format!("Invalid mission file '{}': {error}", path.display()))
}

/// Validate `mission` and resolve `missionId` before any process/file side
/// effect. Returns `None` when the call carries no mission request.
pub fn prepare_launch(
    object: &serde_json::Map<String, Value>,
    cwd: &Path,
    session_id: Option<&str>,
) -> Result<Option<MissionLaunch>, String> {
    let mission = object.get("mission");
    let mission_id = object.get("missionId");
    if mission.is_some() && mission_id.is_some() {
        return Err("mission and missionId cannot both be set.".to_string());
    }
    if let Some(value) = mission {
        if value == &Value::Bool(false) {
            return Ok(None);
        }
        let input = value
            .as_object()
            .ok_or_else(|| "mission must be an object or false.".to_string())?;
        let title = input
            .get("title")
            .or_else(|| input.get("summary"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .ok_or_else(|| {
                "mission.title or mission.summary must be a non-empty string.".to_string()
            })?;
        if input.get("title").is_some() && input.get("summary").is_some() {
            return Err("mission.title and mission.summary cannot both be set.".to_string());
        }
        let objective = input
            .get("objective")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|objective| !objective.is_empty())
            .unwrap_or(title);
        let id = format!("m-{}", crate::runner::budget::random_run_id());
        let now = now_iso();
        let record = MissionRecord {
            schema_version: 1,
            id: id.clone(),
            title: title.to_string(),
            objective: objective.to_string(),
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now,
            cwd: cwd.to_string_lossy().to_string(),
            owner_session_id: session_id.map(str::to_string),
            runs: Vec::new(),
            workflow_children: Vec::new(),
            summary: None,
        };
        write_record(&record)?;
        return Ok(Some(MissionLaunch {
            id,
            auto_created: false,
        }));
    }
    if let Some(value) = mission_id {
        let id = value
            .as_str()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| "missionId must be a non-empty string.".to_string())?;
        let record = read_record(cwd, id)?;
        return Ok(Some(MissionLaunch {
            id: record.id,
            auto_created: false,
        }));
    }
    Ok(None)
}

/// Remember the launch until its dispatch returns a receipt/result.
pub fn remember(run_id: &str, launch: MissionLaunch) {
    PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(run_id.to_string(), launch);
}

/// Attach the dispatch outcome's run(s) to the pending mission.
pub fn attach_outcome(run_id: &str, details: &Value) {
    let launch = PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(run_id);
    let Some(launch) = launch else {
        return;
    };
    let cwd = details
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok());
    let Some(cwd) = cwd else {
        return;
    };
    let mode = details
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("single")
        .to_string();
    let async_dir = details
        .get("statusFile")
        .and_then(Value::as_str)
        .and_then(|path| {
            Path::new(path)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
        });
    let status = details
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_string);
    let acceptance = mission_acceptance_from_details(details);
    let link = MissionRunLink {
        run_id: run_id.to_string(),
        async_dir,
        child_index: None,
        mode,
        status,
        acceptance,
        auto_created: launch.auto_created,
    };
    let mut record = match read_record(&cwd, &launch.id) {
        Ok(record) => record,
        Err(_) => return,
    };
    if let Some(existing) = record
        .runs
        .iter_mut()
        .find(|existing| existing.run_id == link.run_id)
    {
        *existing = link;
    } else {
        record.runs.push(link);
    }
    if let Some(children) = details.get("results").and_then(Value::as_array) {
        for (index, result) in children.iter().enumerate() {
            let key = result
                .get("key")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("step:{index}"));
            let status = result
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let acceptance_status = result
                .get("acceptance")
                .and_then(|acceptance| acceptance.get("status"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let link = MissionChildLink {
                key: key.clone(),
                status,
                run_id: result
                    .get("runId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                agent: result
                    .get("agent")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                acceptance_status,
            };
            if let Some(existing) = record
                .workflow_children
                .iter_mut()
                .find(|existing| existing.key == key)
            {
                *existing = link;
            } else {
                record.workflow_children.push(link);
            }
        }
    }
    record.updated_at = now_iso();
    let _ = write_record(&record);
}

/// `missionAcceptanceStatus` (lifecycle.ts:114-118): the first unaccepted
/// status, else the first status found.
pub fn mission_acceptance_status(results: &[Value]) -> Option<String> {
    let statuses: Vec<String> = results
        .iter()
        .filter_map(|result| {
            result
                .get("acceptance")
                .and_then(|acceptance| acceptance.get("status"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    statuses
        .iter()
        .find(|status| UNACCEPTED.contains(&status.as_str()))
        .cloned()
        .or_else(|| statuses.first().cloned())
}

fn mission_acceptance_from_details(details: &Value) -> Option<String> {
    if let Some(results) = details.get("results").and_then(Value::as_array) {
        return mission_acceptance_status(results);
    }
    details
        .get("acceptance")
        .and_then(|acceptance| acceptance.get("status"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// `finishedMissionStatus` (lifecycle.ts:129-137): an explicit mission stays
/// waiting while any linked run/child is unaccepted.
pub fn finished_mission_status(
    record: &MissionRecord,
    auto_created: bool,
    run_acceptance: &[(String, Option<String>)],
) -> &'static str {
    if auto_created {
        return "completed";
    }
    let unaccepted = |status: &Option<String>| {
        status
            .as_deref()
            .is_some_and(|status| UNACCEPTED.contains(&status))
    };
    if run_acceptance.iter().any(|(_, status)| unaccepted(status)) {
        return "waiting";
    }
    let reported: std::collections::HashSet<&str> = run_acceptance
        .iter()
        .map(|(run_id, _)| run_id.as_str())
        .collect();
    if record.workflow_children.iter().any(|child| {
        !child
            .run_id
            .as_deref()
            .is_some_and(|run_id| reported.contains(run_id))
            && unaccepted(&child.acceptance_status)
    }) {
        return "waiting";
    }
    "completed"
}

fn refresh_linked_runs(record: &mut MissionRecord) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut run_acceptance: Vec<(String, Option<String>)> = Vec::new();
    for run in &mut record.runs {
        let Some(async_dir) = &run.async_dir else {
            continue;
        };
        let status_path = Path::new(async_dir).join("status.json");
        let Ok(raw) = std::fs::read_to_string(&status_path) else {
            continue;
        };
        let Ok(status) = serde_json::from_str::<Value>(&raw) else {
            warnings.push(format!(
                "Failed to read linked run status '{}'.",
                status_path.display()
            ));
            continue;
        };
        let state = status["state"].as_str().unwrap_or("");
        if !state.is_empty() {
            run.status = Some(state.to_string());
        }
        let steps = status["steps"].as_array().cloned().unwrap_or_default();
        run.acceptance = mission_acceptance_status(&steps);
        run_acceptance.push((run.run_id.clone(), run.acceptance.clone()));
    }
    let states: Vec<&str> = record
        .runs
        .iter()
        .filter_map(|run| run.status.as_deref())
        .collect();
    if matches!(record.status.as_str(), "completed" | "failed" | "cancelled") {
        return warnings;
    }
    let next = if states
        .iter()
        .any(|state| matches!(*state, "queued" | "running" | "active"))
    {
        "active"
    } else if states.contains(&"paused") {
        "waiting"
    } else if states.contains(&"failed") {
        "failed"
    } else if states
        .iter()
        .any(|state| matches!(*state, "stopped" | "rejected" | "cancelled"))
    {
        "cancelled"
    } else if !states.is_empty()
        && states
            .iter()
            .all(|state| matches!(*state, "complete" | "completed"))
    {
        finished_mission_status(record, !record.explicit(), &run_acceptance)
    } else {
        record.status.as_str()
    };
    if next != record.status {
        record.status = next.to_string();
        record.updated_at = now_iso();
        if let Err(error) = write_record(record) {
            warnings.push(error);
        }
    }
    warnings
}

pub fn format_mission(record: &MissionRecord) -> String {
    let mut lines = vec![
        format!("Mission: {}", record.id),
        format!("Title: {}", record.title),
        format!("Status: {}", record.status),
        format!("Objective: {}", record.objective),
        format!("Updated: {}", record.updated_at),
    ];
    if let Some(summary) = &record.summary {
        lines.push(format!("Summary: {summary}"));
    }
    if !record.runs.is_empty() {
        lines.push("Runs:".to_string());
        for run in &record.runs {
            let acceptance = run
                .acceptance
                .as_deref()
                .map(|status| format!("; acceptance {status}"))
                .unwrap_or_default();
            let status = run
                .status
                .as_deref()
                .map(|status| format!(", {status}"))
                .unwrap_or_default();
            let dir = run
                .async_dir
                .as_deref()
                .map(|dir| format!(" — {dir}"))
                .unwrap_or_default();
            lines.push(format!(
                "  {} ({}{status}{acceptance}){dir}",
                run.run_id, run.mode
            ));
        }
    }
    if !record.workflow_children.is_empty() {
        lines.push("Workflow children:".to_string());
        for child in &record.workflow_children {
            let acceptance = child
                .acceptance_status
                .as_deref()
                .map(|status| format!("; acceptance {status}"))
                .unwrap_or_default();
            let agent = child
                .agent
                .as_deref()
                .map(|agent| format!(" — {agent}"))
                .unwrap_or_default();
            lines.push(format!(
                "  {}: {}{agent}{acceptance}",
                child.key, child.status
            ));
        }
    }
    lines.join("\n")
}

/// `mission.show`: refresh linked run status/acceptance from status.json.
pub fn show(cwd: &Path, id: &str) -> Result<(MissionRecord, Vec<String>), String> {
    let mut record = read_record(cwd, id)?;
    let warnings = refresh_linked_runs(&mut record);
    Ok((record, warnings))
}

/// `mission.close`: terminal status + optional summary.
pub fn close(
    cwd: &Path,
    id: &str,
    status: &str,
    summary: Option<&str>,
) -> Result<MissionRecord, String> {
    if !MISSION_STATUSES.contains(&status)
        || !matches!(status, "completed" | "failed" | "cancelled")
    {
        return Err(
            "mission.close missionStatus must be completed, failed, or cancelled.".to_string(),
        );
    }
    let mut record = read_record(cwd, id)?;
    record.status = status.to_string();
    if let Some(summary) = summary.map(str::trim).filter(|summary| !summary.is_empty()) {
        record.summary = Some(summary.to_string());
    }
    record.updated_at = now_iso();
    write_record(&record)?;
    Ok(record)
}

pub fn list(cwd: &Path) -> (Vec<MissionRecord>, Vec<String>) {
    let mut records = Vec::new();
    let mut warnings = Vec::new();
    let Ok(entries) = std::fs::read_dir(missions_dir(cwd)) else {
        return (records, warnings);
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read_to_string(&path)
            .map_err(|error| error.to_string())
            .and_then(|raw| serde_json::from_str::<MissionRecord>(&raw).map_err(|e| e.to_string()))
        {
            Ok(record) => records.push(record),
            Err(error) => warnings.push(format!(
                "Skipped corrupt mission '{}': {error}",
                path.display()
            )),
        }
    }
    records.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    (records, warnings)
}

/// Test seam: drop the pending launch map.
#[allow(dead_code)]
pub fn reset_for_test() {
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cwd() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rpi-mission-test-{}-{}",
            std::process::id(),
            crate::runner::budget::random_run_id()
        ));
        std::fs::create_dir_all(&dir).expect("temp cwd");
        dir
    }

    fn record(cwd: &Path, _explicit: bool) -> MissionRecord {
        MissionRecord {
            schema_version: 1,
            id: "m-test".to_string(),
            title: "T".to_string(),
            objective: "O".to_string(),
            status: "active".to_string(),
            created_at: now_iso(),
            updated_at: now_iso(),
            cwd: cwd.to_string_lossy().to_string(),
            owner_session_id: None,
            runs: Vec::new(),
            workflow_children: Vec::new(),
            summary: None,
        }
    }

    #[test]
    fn explicit_mission_stays_waiting_for_unaccepted_evidence() {
        let cwd = temp_cwd();
        let record = record(&cwd, true);
        assert_eq!(
            finished_mission_status(
                &record,
                false,
                &[("r1".into(), Some("review-required".into()))]
            ),
            "waiting"
        );
        assert_eq!(
            finished_mission_status(&record, false, &[("r1".into(), Some("verified".into()))]),
            "completed"
        );
        assert_eq!(
            finished_mission_status(&record, true, &[("r1".into(), Some("rejected".into()))]),
            "completed",
            "auto-created missions keep the old rule"
        );
    }

    #[test]
    fn show_refreshes_acceptance_and_keeps_waiting() {
        let cwd = temp_cwd();
        let mut record = record(&cwd, true);
        let run_dir = cwd.join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(
            run_dir.join("status.json"),
            serde_json::json!({
                "runId": "r1",
                "state": "complete",
                "steps": [{"status": "complete", "acceptance": {"status": "claimed"}}]
            })
            .to_string(),
        )
        .unwrap();
        record.runs.push(MissionRunLink {
            run_id: "r1".into(),
            async_dir: Some(run_dir.to_string_lossy().into()),
            child_index: None,
            mode: "single".into(),
            status: Some("complete".into()),
            acceptance: None,
            auto_created: false,
        });
        write_record(&record).unwrap();
        let (shown, warnings) = show(&cwd, "m-test").unwrap();
        assert!(warnings.is_empty());
        assert_eq!(shown.status, "waiting");
        let text = format_mission(&shown);
        assert!(text.contains("acceptance claimed"), "{text}");
    }

    #[test]
    fn close_rejects_non_terminal_status() {
        let cwd = temp_cwd();
        let record = record(&cwd, true);
        write_record(&record).unwrap();
        assert!(close(&cwd, "m-test", "waiting", None).is_err());
        assert_eq!(
            close(&cwd, "m-test", "completed", Some("done"))
                .unwrap()
                .status,
            "completed"
        );
    }
}
