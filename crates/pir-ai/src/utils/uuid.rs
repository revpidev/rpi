//! Port of `packages/ai/src/utils/uuid.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences: `fillRandomBytes` has no `crypto.getRandomValues`
//! equivalent in the dependency baseline (no `rand`/`uuid` crate,
//! coding-standards appendix A), so random bytes come from a process-wide
//! non-security xorshift64* PRNG — the same class as the upstream
//! `Math.random()` fallback path (uuid.ts:9-11) — seeded from system time
//! (nanos), process id, and a global counter. UUIDs are identifiers, not
//! secrets; this matches the precedent in `utils/provider_retry.rs`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Non-security PRNG byte source (`Math.random` fallback upstream).
fn fill_random_bytes(bytes: &mut [u8]) {
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut state = STATE.load(Ordering::Relaxed);
    if state == 0 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e3779b97f4a7c15);
        let seed = (nanos ^ ((std::process::id() as u64) << 32).rotate_left(7)) | 1;
        state = match STATE.compare_exchange(0, seed, Ordering::Relaxed, Ordering::Relaxed) {
            // Won the race: this thread's seed is now the state.
            Ok(_) => seed,
            // Lost the race: use the seed another thread installed.
            Err(current) => current,
        };
    }
    for chunk in bytes.chunks_mut(8) {
        // xorshift64*
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let next = state.wrapping_mul(0x2545F4914F6CDD1D);
        STATE.store(state, Ordering::Relaxed);
        let raw = next.to_le_bytes();
        chunk.copy_from_slice(&raw[..chunk.len()]);
    }
}

struct V7State {
    last_timestamp: u64,
    sequence: u32,
}

static V7_STATE: Mutex<V7State> = Mutex::new(V7State {
    last_timestamp: 0,
    sequence: 0,
});

fn to_hex(bytes: &[u8; 16]) -> String {
    let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        hex[0..4].concat(),
        hex[4..6].concat(),
        hex[6..8].concat(),
        hex[8..10].concat(),
        hex[10..16].concat()
    )
}

/// Generate a time-ordered UUIDv7 (`uuidv7` upstream).
pub fn uuidv7() -> String {
    let mut random = [0u8; 16];
    fill_random_bytes(&mut random);
    let timestamp = now_ms();

    let mut state = match V7_STATE.lock() {
        Ok(guard) => guard,
        // Poisoned mutex: a panicking thread must not take down id generation.
        Err(poisoned) => poisoned.into_inner(),
    };
    if timestamp > state.last_timestamp {
        state.sequence = u32::from_be_bytes([random[6], random[7], random[8], random[9]]);
        state.last_timestamp = timestamp;
    } else {
        state.sequence = state.sequence.wrapping_add(1);
        if state.sequence == 0 {
            state.last_timestamp += 1;
        }
    }
    let last_timestamp = state.last_timestamp;
    let sequence = state.sequence;
    drop(state);

    let mut bytes = [0u8; 16];
    // `as u8` already truncates to the low byte.
    bytes[0] = (last_timestamp / 0x10000000000) as u8;
    bytes[1] = (last_timestamp / 0x100000000) as u8;
    bytes[2] = (last_timestamp / 0x1000000) as u8;
    bytes[3] = (last_timestamp / 0x10000) as u8;
    bytes[4] = (last_timestamp / 0x100) as u8;
    bytes[5] = last_timestamp as u8;
    bytes[6] = 0x70 | ((sequence >> 28) as u8 & 0x0f);
    bytes[7] = (sequence >> 20) as u8;
    bytes[8] = 0x80 | ((sequence >> 14) as u8 & 0x3f);
    bytes[9] = (sequence >> 6) as u8;
    bytes[10] = ((sequence & 0x3f) as u8) << 2 | (random[10] & 0x03);
    bytes[11..16].copy_from_slice(&random[11..16]);

    to_hex(&bytes)
}

/// Generate a random UUIDv4 (`randomUUID` from node:crypto upstream, used by
/// the coding-agent session entry id generator).
pub fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    fill_random_bytes(&mut bytes);
    bytes[6] = 0x40 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
    to_hex(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID_V7_RE_CHAR_CHECK: fn(&str) -> bool = |id: &str| {
        // ^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$
        let parts: Vec<&str> = id.split('-').collect();
        if parts.len() != 5 {
            return false;
        }
        let hex = |s: &str| {
            s.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        };
        parts[0].len() == 8
            && hex(parts[0])
            && parts[1].len() == 4
            && hex(parts[1])
            && parts[2].len() == 4
            && parts[2].starts_with('7')
            && hex(parts[2])
            && parts[3].len() == 4
            && matches!(parts[3].chars().next(), Some('8' | '9' | 'a' | 'b'))
            && hex(parts[3])
            && parts[4].len() == 12
            && hex(parts[4])
    };

    #[test]
    fn uuidv7_matches_upstream_shape() {
        let id = uuidv7();
        assert!(UUID_V7_RE_CHAR_CHECK(&id), "not a uuidv7: {id}");
    }

    #[test]
    fn uuidv7_is_time_ordered() {
        let a = uuidv7();
        let b = uuidv7();
        assert!(a <= b, "uuidv7 must be monotonic: {a} then {b}");
    }

    #[test]
    fn random_uuid_shape_and_uniqueness() {
        let a = random_uuid();
        let b = random_uuid();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(a.as_bytes()[14], b'4');
    }
}
