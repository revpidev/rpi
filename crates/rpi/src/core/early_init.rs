//! Process initialization required before argument parsing and runtime creation.

/// Standard glibc override for the maximum number of malloc arenas.
pub const ENV_MALLOC_ARENA_MAX: &str = "MALLOC_ARENA_MAX";

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn valid_malloc_arena_override(value: &str) -> bool {
    matches!(value.parse::<libc::c_int>(), Ok(limit) if limit > 0)
}

/// Initialize process-wide allocator behavior before argument parsing or worker threads.
///
/// Linux GNU defaults to one malloc arena unless `MALLOC_ARENA_MAX` is a
/// positive 32-bit integer. Invalid values warn and use one; no environment
/// variable is written. Other targets are unchanged.
pub fn initialize() {
    platform::initialize();
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod platform {
    use std::env::VarError;

    use super::{ENV_MALLOC_ARENA_MAX, valid_malloc_arena_override};

    pub(super) fn initialize() {
        match std::env::var(ENV_MALLOC_ARENA_MAX) {
            Ok(value) if valid_malloc_arena_override(&value) => return,
            Ok(_) | Err(VarError::NotUnicode(_)) => {
                eprintln!(
                    "Warning: invalid MALLOC_ARENA_MAX; expected a positive 32-bit integer; using 1"
                );
            }
            Err(VarError::NotPresent) => {}
        }
        // SAFETY: both entry points call this before argument collection and
        // runtime creation, so no worker threads can race the allocator setting.
        if unsafe { libc::mallopt(libc::M_ARENA_MAX, 1) } == 0 {
            eprintln!("Warning: failed to set M_ARENA_MAX to 1");
        }
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
mod platform {
    pub(super) fn initialize() {}
}

#[cfg(all(test, target_os = "linux", target_env = "gnu"))]
mod tests {
    use super::valid_malloc_arena_override;

    #[test]
    fn malloc_arena_override_validation() {
        for valid in ["1", "2", "4", "+2", "01", "2147483647"] {
            assert!(valid_malloc_arena_override(valid), "value={valid:?}");
        }
        for invalid in [
            "",
            "0",
            "-1",
            "abc",
            "2x",
            " 2",
            "2 ",
            "1.0",
            "2147483648",
            "999999999999999999999",
        ] {
            assert!(!valid_malloc_arena_override(invalid), "value={invalid:?}");
        }
    }
}
