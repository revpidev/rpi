//! Port of `packages/tui/src/native-modifiers.ts` @ pi 0.82.1 (2efa728),
//! with the platform gating tracking the 4181f66 revision (73dd066ee added
//! the win32 helper path).
//!
//! Intentional differences:
//! - The upstream loads a native Node addon to query modifier key state
//!   (`native/darwin/prebuilds/darwin-<arch>/darwin-modifiers.node` on macOS,
//!   `native/win32/prebuilds/win32-<arch>/win32-console-mode.node` on Windows
//!   since 73dd066ee); no equivalent native binding is bundled in this Rust
//!   port, so `is_native_modifier_pressed` always returns `false` — the
//!   upstream behavior whenever the addon is unavailable (ADR-0004 keeps the
//!   addon-missing fallback branch). The platform/arch gating logic of
//!   `loadNativeModifiersHelper` is preserved in
//!   `load_native_modifiers_helper`.
//! - `ModifierKey` is an enum instead of a string-literal union;
//!   `ModifierKey::name()` returns the upstream byte values
//!   ("shift" | "command" | "control" | "option").

/// Native macOS modifier key names (`ModifierKey`, native-modifiers.ts:7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifierKey {
    Shift,
    Command,
    Control,
    Option,
}

impl ModifierKey {
    /// Upstream string value (`ModifierKey` literal, native-modifiers.ts:7).
    pub fn name(self) -> &'static str {
        match self {
            ModifierKey::Shift => "shift",
            ModifierKey::Command => "command",
            ModifierKey::Control => "control",
            ModifierKey::Option => "option",
        }
    }

    /// Parses a `ModifierKey` from its upstream string value.
    pub fn from_name(name: &str) -> Option<ModifierKey> {
        match name {
            "shift" => Some(ModifierKey::Shift),
            "command" => Some(ModifierKey::Command),
            "control" => Some(ModifierKey::Control),
            "option" => Some(ModifierKey::Option),
            _ => None,
        }
    }
}

/// Ports the platform/arch gating of `loadNativeModifiersHelper`
/// (native-modifiers.ts:21-53 @ 4181f66): upstream resolves an addon path on
/// darwin and — since 73dd066ee — on win32; other platforms and unsupported
/// arches (anything but x64/arm64) yield no helper. Neither native addon has
/// a Rust equivalent bundled here, so the helper is never available
/// (ADR-0004: keep the upstream addon-missing fallback branch).
fn load_native_modifiers_helper() -> Option<()> {
    if !cfg!(any(target_os = "macos", windows)) {
        return None;
    }
    if !matches!(std::env::consts::ARCH, "x86_64" | "aarch64") {
        return None;
    }
    // No Rust equivalent of the `darwin-modifiers.node` /
    // `win32-console-mode.node` bindings is bundled.
    None
}

/// `isNativeModifierPressed` (native-modifiers.ts:51-58).
///
/// Returns `false` when the native helper is unavailable — which, in this
/// port, is always (see `load_native_modifiers_helper`). Upstream behavior:
/// `const helper = loadNativeModifiersHelper(); if (!helper) return false;
/// try { return helper.isModifierPressed(key) === true; } catch { return false; }`.
pub fn is_native_modifier_pressed(_key: ModifierKey) -> bool {
    if load_native_modifiers_helper().is_none() {
        return false;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_key_names_match_upstream_bytes() {
        assert_eq!(ModifierKey::Shift.name(), "shift");
        assert_eq!(ModifierKey::Command.name(), "command");
        assert_eq!(ModifierKey::Control.name(), "control");
        assert_eq!(ModifierKey::Option.name(), "option");
        assert_eq!(
            ModifierKey::from_name("command"),
            Some(ModifierKey::Command)
        );
        assert_eq!(ModifierKey::from_name("meta"), None);
    }

    #[test]
    fn is_native_modifier_pressed_returns_false_without_helper() {
        // No native binding is bundled; must never panic and always report false.
        assert!(!is_native_modifier_pressed(ModifierKey::Shift));
        assert!(!is_native_modifier_pressed(ModifierKey::Command));
        assert!(!is_native_modifier_pressed(ModifierKey::Control));
        assert!(!is_native_modifier_pressed(ModifierKey::Option));
    }
}
