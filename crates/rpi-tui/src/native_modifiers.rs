//! Port of `packages/tui/src/native-modifiers.ts` @ pi 1.4.0 (`4a080252f`),
//! with the platform gating tracking the `4181f66` revision (`73dd066ee`
//! added the win32 helper path).
//!
//! Upstream loads a native Node addon to query modifier key state
//! (`native/darwin/prebuilds/darwin-<arch>/darwin-modifiers.node` on macOS,
//! `native/win32/prebuilds/win32-<arch>/win32-console-mode.node` on Windows
//! since `73dd066ee`). This port implements the macOS arm with a
//! CoreGraphics FFI query (V17-10 #69, ADR-0004's reserved helper seam):
//! Apple Terminal's Shift+Enter normalization needs the physical modifier
//! state. Windows keeps the fallback used when a native helper is
//! unavailable (`native_helper_supported` is macOS-only — the win32
//! addon's modifier query is not ported); the VT-input half of the win32
//! helper is implemented in `terminal.rs`.
//! - `ModifierKey` is an enum instead of a string-literal union;
//!   `ModifierKey::name()` returns the upstream byte values
//!   ("shift" | "command" | "control" | "option").

/// Native macOS modifier key names (`ModifierKey`, native-platform.ts:7;
/// re-exported by native-modifiers.ts:3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifierKey {
    Shift,
    Command,
    Control,
    Option,
}

impl ModifierKey {
    /// Upstream string value (`ModifierKey` literal, native-platform.ts:7).
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

/// Whether a native helper exists for this platform/build
/// (`loadNativeModifiersHelper`, native-modifiers.ts:21-53 @ 4181f66):
/// upstream resolves an addon on darwin and win32 for x64/arm64. The Rust
/// port ships the macOS CoreGraphics arm; Windows stays on the upstream
/// fallback (no console-mode modifier query).
fn native_helper_supported() -> bool {
    cfg!(target_os = "macos") && matches!(std::env::consts::ARCH, "x86_64" | "aarch64")
}

/// `isNativeModifierPressed` (native-modifiers.ts:51-58).
///
/// Returns `false` when the native helper is unavailable — upstream:
/// `const helper = loadNativeModifiersHelper(); if (!helper) return false;
/// try { return helper.isModifierPressed(key) === true; } catch { return
/// false; }`. The `false` fallback also covers non-macOS platforms and
/// unsupported architectures (ADR-0004: the helper-absent fallback
/// branch).
pub fn is_native_modifier_pressed(key: ModifierKey) -> bool {
    if !native_helper_supported() {
        return false;
    }
    is_native_modifier_pressed_with(key, &native_modifier_query)
}

/// The query logic behind [`is_native_modifier_pressed`], with the native
/// call injected so the fallback mapping is unit-testable off-platform
/// (upstream `try { helper.isModifierPressed(key) === true } catch
/// { false }`).
fn is_native_modifier_pressed_with(
    key: ModifierKey,
    query: &dyn Fn(ModifierKey) -> Option<bool>,
) -> bool {
    query(key).unwrap_or(false)
}

/// The production query. Only reachable on macOS (`native_helper_supported`);
/// other platforms keep the helper-unavailable fallback.
fn native_modifier_query(key: ModifierKey) -> Option<bool> {
    #[cfg(target_os = "macos")]
    {
        Some(macos::is_modifier_pressed(key))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = key;
        None
    }
}

/// macOS CoreGraphics FFI (ADR-0004's reserved helper seam): queries the
/// modifier flags of the combined session event state, the same
/// information the upstream `darwin-modifiers.node` addon exposes.
#[cfg(target_os = "macos")]
mod macos {
    use super::ModifierKey;

    /// `kCGEventSourceStateCombinedSessionState` (CGEventSource.h).
    const COMBINED_SESSION_STATE: u32 = 0;
    /// `kCGEventFlagMaskShift` / `kCGEventFlagMaskControl` /
    /// `kCGEventFlagMaskAlternate` (Option) / `kCGEventFlagMaskCommand`
    /// (CGEventTypes.h).
    const FLAG_SHIFT: u64 = 0x0002_0000;
    const FLAG_CONTROL: u64 = 0x0004_0000;
    const FLAG_OPTION: u64 = 0x0008_0000;
    const FLAG_COMMAND: u64 = 0x0010_0000;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        /// `CGEventFlags CGEventSourceFlagsState(CGEventSourceStateID)`
        /// (CGEventSource.h): the current combined-session modifier flags.
        fn CGEventSourceFlagsState(state_id: u32) -> u64;
    }

    /// The upstream `isModifierPressed(key)` query.
    pub(super) fn is_modifier_pressed(key: ModifierKey) -> bool {
        // SAFETY: `CGEventSourceFlagsState` is a pure query with no
        // pointer arguments; `COMBINED_SESSION_STATE` is a documented
        // constant from CGEventSource.h.
        let flags = unsafe { CGEventSourceFlagsState(COMBINED_SESSION_STATE) };
        let mask = match key {
            ModifierKey::Shift => FLAG_SHIFT,
            ModifierKey::Command => FLAG_COMMAND,
            ModifierKey::Control => FLAG_CONTROL,
            ModifierKey::Option => FLAG_OPTION,
        };
        flags & mask != 0
    }
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

    /// ADR-0004 fallback: platforms without the native helper always report
    /// `false` and never panic. On macOS the physical modifier state
    /// decides, so the check is the helper-absent branch only.
    #[test]
    fn fallback_is_false_without_the_native_helper() {
        if native_helper_supported() {
            return;
        }
        assert!(!is_native_modifier_pressed(ModifierKey::Shift));
        assert!(!is_native_modifier_pressed(ModifierKey::Command));
        assert!(!is_native_modifier_pressed(ModifierKey::Control));
        assert!(!is_native_modifier_pressed(ModifierKey::Option));
    }

    /// The upstream try/catch contract through the mockable seam: a query
    /// result drives the answer, a failed/absent helper is `false`, and
    /// every key reaches the query unchanged.
    #[test]
    fn query_seam_maps_pressed_state_and_failures() {
        let pressed = |key: ModifierKey| Some(key == ModifierKey::Shift);
        assert!(is_native_modifier_pressed_with(
            ModifierKey::Shift,
            &pressed
        ));
        assert!(!is_native_modifier_pressed_with(
            ModifierKey::Command,
            &pressed
        ));
        assert!(!is_native_modifier_pressed_with(
            ModifierKey::Shift,
            &|_| None
        ));
        for key in [
            ModifierKey::Shift,
            ModifierKey::Command,
            ModifierKey::Control,
            ModifierKey::Option,
        ] {
            let seen = std::cell::Cell::new(None);
            let query = |queried: ModifierKey| {
                seen.set(Some(queried));
                Some(true)
            };
            assert!(is_native_modifier_pressed_with(key, &query));
            assert_eq!(seen.get(), Some(key));
        }
    }
}
