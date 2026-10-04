//! Permission mode — the V16-05 FR-B single authority.
//!
//! rpi-owned surface (`[RPI-OWN]`, no upstream counterpart). The session
//! holds one `Default` / `Plan` state; the interactive keybinding
//! (`app.mode.cycle`), the `setMode` host call, and the footer badge all
//! read and write it. Extensions observe transitions through the
//! `mode_change` event and read/write it through `ctx.getMode()` /
//! `ctx.setMode()`.
//!
//! Only the two states below exist in the first iteration (rpi has no
//! auto-accept middle state); the enum stays explicit so a middle state can
//! be added without a wire change beyond a new lowercase tag (02 design
//! §3.11).
//!
//! Mode state is an interactive-session concept: a session whose
//! `extension_mode` is not `Tui` answers `Default` from
//! [`PermissionMode`] readers and ignores `setMode` (V16-05 §8-5
//! implementation-time decision).

use serde::{Deserialize, Serialize};

/// `PermissionMode` — session-scoped permission state (wire tag
/// `"default"` / `"plan"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    /// Normal operation.
    #[default]
    Default,
    /// Plan mode: the plan-mode extension narrows the tool exposure and
    /// injects planning guidance; the built-in footer shows `⏸ plan`.
    Plan,
}

impl PermissionMode {
    /// Wire / display string (`"default"` / `"plan"`).
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionMode::Default => "default",
            PermissionMode::Plan => "plan",
        }
    }

    /// Parse the wire string; unknown values are rejected (`None`).
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "default" => Some(PermissionMode::Default),
            "plan" => Some(PermissionMode::Plan),
            _ => None,
        }
    }

    /// Cycle `Default` ↔ `Plan` (the `app.mode.cycle` contract).
    pub fn cycle(self) -> Self {
        match self {
            PermissionMode::Default => PermissionMode::Plan,
            PermissionMode::Plan => PermissionMode::Default,
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_tags_round_trip() {
        for mode in [PermissionMode::Default, PermissionMode::Plan] {
            assert_eq!(PermissionMode::parse(mode.as_str()), Some(mode));
            assert_eq!(mode.to_string(), mode.as_str());
        }
        assert_eq!(PermissionMode::parse("auto"), None);
        assert_eq!(PermissionMode::parse(""), None);
    }

    #[test]
    fn cycle_toggles_between_the_two_states() {
        assert_eq!(PermissionMode::Default.cycle(), PermissionMode::Plan);
        assert_eq!(PermissionMode::Plan.cycle(), PermissionMode::Default);
        assert_eq!(PermissionMode::default(), PermissionMode::Default);
    }

    #[test]
    fn serde_uses_lowercase_tags() {
        assert_eq!(
            serde_json::to_value(PermissionMode::Plan).unwrap(),
            serde_json::json!("plan")
        );
        assert_eq!(
            serde_json::from_value::<PermissionMode>(serde_json::json!("default")).unwrap(),
            PermissionMode::Default
        );
    }
}
