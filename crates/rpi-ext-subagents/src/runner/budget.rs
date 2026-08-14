//! Depth guard and P0 spawn caps (FR-P0-07).
//!
//! Port of the recursion-depth section of pi-subagents `src/shared/types.ts`
//! @ v0.48.0 (56f97234) (2086-2150) with `PI_*` → `RPI_*` renames.
//!
//! P0 implements the depth cap and the `maxSubagentSpawnsPerRun` ceiling as a
//! per-process counter (single foreground runs spawn exactly one child, so the
//! counter guards nested fanout children); the cross-process claim ledger
//! upstream builds in temp dirs lands with FR-P1-04 (design §3.6).

pub const DEFAULT_SUBAGENT_MAX_DEPTH: u64 = 2;
pub const DEFAULT_MAX_SUBAGENT_SPAWNS_PER_RUN: u64 = 64;
pub const SUBAGENT_DEPTH_ENV: &str = "RPI_SUBAGENT_DEPTH";
pub const SUBAGENT_MAX_DEPTH_ENV: &str = "RPI_SUBAGENT_MAX_DEPTH";
pub const MAX_SPAWNS_PER_RUN_ENV: &str = "RPI_SUBAGENT_MAX_SPAWNS_PER_RUN";

/// `randomUUID().slice(0, 8)` — 8 lowercase hex chars.
pub fn random_run_id() -> String {
    let mut bytes = [0u8; 4];
    if let Ok(mut source) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        if source.read_exact(&mut bytes).is_ok() {
            return bytes.iter().map(|b| format!("{b:02x}")).collect();
        }
    }
    // Fallback: time + pid + counter, hex, 8 chars.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mix = (nanos as u64)
        ^ ((std::process::id() as u64) << 32)
        ^ COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(0x9e3779b9);
    format!("{:08x}", mix & 0xffff_ffff)
}

/// `normalizeNonNegativeInteger` — Number() coercion of string or number,
/// integer ≥ 0, else None.
fn normalize_non_negative(value: Option<&str>) -> Option<u64> {
    let raw = value?;
    let parsed: f64 = raw.trim().parse().ok()?;
    if !parsed.is_finite() || parsed.fract() != 0.0 || parsed < 0.0 || parsed > u64::MAX as f64 {
        return None;
    }
    Some(parsed as u64)
}

/// `resolveCurrentMaxSubagentDepth` (types.ts:2100-2104): env > config > 2.
pub fn resolve_current_max_depth(config_max_depth: Option<u64>) -> u64 {
    normalize_non_negative(std::env::var(SUBAGENT_MAX_DEPTH_ENV).ok().as_deref())
        .or(config_max_depth)
        .unwrap_or(DEFAULT_SUBAGENT_MAX_DEPTH)
}

/// `resolveChildMaxSubagentDepth` (types.ts:2106-2110): agent values only
/// tighten (min).
pub fn resolve_child_max_depth(parent_max: u64, agent_max: Option<u64>) -> u64 {
    match agent_max {
        Some(agent_max) => parent_max.min(agent_max),
        None => parent_max,
    }
}

/// `checkSubagentDepth` (types.ts:2112-2117).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DepthCheck {
    pub blocked: bool,
    pub depth: u64,
    pub max_depth: u64,
}

pub fn check_depth(config_max_depth: Option<u64>) -> DepthCheck {
    // Non-numeric / missing env parses to NaN upstream → blocked=false via
    // Number.isFinite; treat unparsable as 0.
    let depth =
        normalize_non_negative(std::env::var(SUBAGENT_DEPTH_ENV).ok().as_deref()).unwrap_or(0);
    let max_depth = resolve_current_max_depth(config_max_depth);
    DepthCheck {
        blocked: depth >= max_depth,
        depth,
        max_depth,
    }
}

/// Block message (executor 5726-5741), verbatim.
pub fn depth_blocked_message(check: &DepthCheck) -> String {
    format!(
        "Nested subagent call blocked (depth={}, max={}). You are running at the maximum subagent nesting depth. Complete your current task directly without delegating to further subagents.",
        check.depth, check.max_depth
    )
}

/// `getSubagentDepthEnv` (types.ts:2119-2126): child depth = parent + 1, and
/// the child's max depth rides along.
pub fn depth_env_for_child(child_max_depth: u64) -> Vec<(String, String)> {
    let parent = normalize_non_negative(std::env::var(SUBAGENT_DEPTH_ENV).ok().as_deref())
        .map(|v| v + 1)
        .unwrap_or(1);
    vec![
        (SUBAGENT_DEPTH_ENV.to_string(), parent.to_string()),
        (
            SUBAGENT_MAX_DEPTH_ENV.to_string(),
            child_max_depth.to_string(),
        ),
    ]
}

/// `resolveMaxSubagentSpawnsPerRun` (types.ts:2141-2150): env (>0 only) >
/// config (>0 only) > 64.
pub fn resolve_max_spawns_per_run(config_value: Option<u64>) -> u64 {
    let normalize_positive = |value: Option<u64>| value.filter(|v| *v > 0);
    normalize_positive(normalize_non_negative(
        std::env::var(MAX_SPAWNS_PER_RUN_ENV).ok().as_deref(),
    ))
    .or_else(|| normalize_positive(config_value))
    .unwrap_or(DEFAULT_MAX_SUBAGENT_SPAWNS_PER_RUN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_id_shape() {
        let id = random_run_id();
        assert_eq!(id.len(), 8);
        assert!(id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn normalize_and_defaults() {
        assert_eq!(normalize_non_negative(Some("2")), Some(2));
        assert_eq!(normalize_non_negative(Some("-1")), None);
        assert_eq!(normalize_non_negative(Some("1.5")), None);
        assert_eq!(normalize_non_negative(Some("junk")), None);
        assert_eq!(resolve_current_max_depth(None), 2);
        assert_eq!(resolve_child_max_depth(2, Some(1)), 1);
        assert_eq!(
            resolve_child_max_depth(2, Some(5)),
            2,
            "agent can only tighten"
        );
        assert_eq!(resolve_max_spawns_per_run(None), 64);
        assert_eq!(resolve_max_spawns_per_run(Some(0)), 64, "0 falls back");
        assert_eq!(resolve_max_spawns_per_run(Some(8)), 8);
    }
}
