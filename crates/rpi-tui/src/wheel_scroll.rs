//! Port of `packages/tui/src/wheel-scroll.ts` @ a13d35a74 (`Release v1.0.0`,
//! #9758).
//!
//! [`WheelScrollAccelerator`] converts mouse-wheel events into line counts.
//! In [`WheelScrollLines::Auto`] mode on terminals that do not accelerate
//! wheel input, the count follows event velocity: an isolated notch moves one
//! line, while a fast spin moves up to six lines per event.
//!
//! Intentional differences:
//! - Upstream `WheelScrollLines = number | "auto"` becomes an explicit enum;
//!   [`WheelScrollLines::Lines`] carries the `u64` count (upstream accepts
//!   non-integer / non-finite numbers, normalized with
//!   `Math.max(1, Math.floor(n))` — the Rust host normalizes at construction).
//! - `process.platform === "darwin"` is `cfg!(target_os = "macos")`; the
//!   `SSH_*` "undefined" checks are `std::env::var_os(..).is_none()` (an empty
//!   value is still defined, matching JS).

/// Lines moved per mouse-wheel event, or [`Auto`](WheelScrollLines::Auto) to
/// accelerate fast wheel spins (upstream `WheelScrollLines`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WheelScrollLines {
    /// Accelerate with wheel velocity (upstream `"auto"`).
    #[default]
    Auto,
    /// A fixed number of lines per event (upstream `number`).
    Lines(u64),
}

impl WheelScrollLines {
    /// Construct a fixed line count (upstream `number`).
    pub fn lines(n: u64) -> Self {
        WheelScrollLines::Lines(n)
    }

    /// The fixed line count this value represents, `1` for
    /// [`Auto`](WheelScrollLines::Auto) (upstream's base rate is one line per
    /// notch).
    pub fn as_lines(self) -> u64 {
        match self {
            WheelScrollLines::Auto => 1,
            WheelScrollLines::Lines(n) => n.max(1),
        }
    }
}

impl From<u64> for WheelScrollLines {
    fn from(n: u64) -> Self {
        WheelScrollLines::Lines(n)
    }
}

// Several events closer than this belong to one physical notch (Ghostty emits
// them ~4 ms apart) or come from a high-resolution source. They move one line
// each and do not accelerate.
const BURST_GAP_MS: f64 = 5.0;
// A pause longer than this ends a scroll gesture.
const GESTURE_GAP_MS: f64 = 200.0;
// Average event gap that maps to one line per event. Faster events scale up
// proportionally.
const REFERENCE_GAP_MS: f64 = 100.0;
const MAX_AUTO_LINES: f64 = 6.0;

/// Local macOS terminals receive wheel and trackpad deltas that the OS has
/// already accelerated, and they emit one event per line. Other platforms, and
/// SSH sessions where the client platform is unknown, usually send one event
/// per wheel notch (`terminalAcceleratesWheel`).
pub fn terminal_accelerates_wheel() -> bool {
    cfg!(target_os = "macos")
        && std::env::var_os("SSH_CONNECTION").is_none()
        && std::env::var_os("SSH_CLIENT").is_none()
        && std::env::var_os("SSH_TTY").is_none()
}

/// Converts wheel events into line counts (upstream `WheelScrollAccelerator`).
#[derive(Debug, Clone)]
pub struct WheelScrollAccelerator {
    lines: WheelScrollLines,
    accelerate: bool,
    last_time: f64,
    last_direction: i8,
    average_gap: Option<f64>,
    carry: f64,
}

impl WheelScrollAccelerator {
    /// Upstream `constructor(lines = "auto", accelerate = !terminalAcceleratesWheel())`.
    pub fn new(lines: WheelScrollLines, accelerate: bool) -> Self {
        WheelScrollAccelerator {
            lines,
            accelerate,
            last_time: f64::NEG_INFINITY,
            last_direction: 0,
            average_gap: None,
            carry: 0.0,
        }
    }

    /// Upstream default `accelerate = !terminalAcceleratesWheel()`.
    pub fn new_with_platform_acceleration(lines: WheelScrollLines) -> Self {
        Self::new(lines, !terminal_accelerates_wheel())
    }

    /// Upstream `setLines`: set the line count and reset the gesture state.
    pub fn set_lines(&mut self, lines: WheelScrollLines) {
        self.lines = lines;
        self.reset();
    }

    /// Return the positive line count for a wheel event in `direction`
    /// (`-1` or `1`) at time `now` (milliseconds). Upstream `next`.
    pub fn next(&mut self, direction: i8, now_ms: f64) -> u64 {
        if self.lines != WheelScrollLines::Auto {
            return self.lines.as_lines();
        }
        if !self.accelerate {
            return 1;
        }

        let gap = now_ms - self.last_time;
        let same_gesture = direction == self.last_direction && gap <= GESTURE_GAP_MS;
        self.last_time = now_ms;
        self.last_direction = direction;
        if !same_gesture {
            self.average_gap = None;
            self.carry = 0.0;
            return 1;
        }
        if gap < BURST_GAP_MS {
            return 1;
        }

        let average_gap = match self.average_gap {
            None => gap,
            Some(average_gap) => (average_gap + gap) / 2.0,
        };
        self.average_gap = Some(average_gap);
        let lines = MAX_AUTO_LINES.min(REFERENCE_GAP_MS / average_gap).max(1.0) + self.carry;
        let whole = lines.floor();
        self.carry = lines - whole;
        // `whole` is always in `1..=MAX_AUTO_LINES` here.
        whole as u64
    }

    /// Upstream private `reset`.
    fn reset(&mut self) {
        self.last_time = f64::NEG_INFINITY;
        self.last_direction = 0;
        self.average_gap = None;
        self.carry = 0.0;
    }
}

// =============================================================================
// Tests (port of packages/tui/test/wheel-scroll.test.ts @ a13d35a74, 1:1)
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn scroll(accelerator: &mut WheelScrollAccelerator, times: &[f64], direction: i8) -> Vec<u64> {
        times
            .iter()
            .map(|time| accelerator.next(direction, *time))
            .collect()
    }

    // #9758: fullscreen wheel scrolling was one line per notch on terminals
    // that do not accelerate wheels.
    #[test]
    fn uses_fixed_line_counts_regardless_of_timing() {
        let mut accelerator = WheelScrollAccelerator::new(WheelScrollLines::Lines(3), true);
        assert_eq!(
            scroll(&mut accelerator, &[0.0, 10.0, 20.0, 1000.0], 1),
            vec![3, 3, 3, 3]
        );
        accelerator.set_lines(WheelScrollLines::Lines(0));
        assert_eq!(accelerator.next(1, 2000.0), 1);
    }

    #[test]
    fn keeps_one_line_per_event_in_auto_mode_when_the_terminal_already_accelerates() {
        let mut accelerator = WheelScrollAccelerator::new(WheelScrollLines::Auto, false);
        assert_eq!(
            scroll(&mut accelerator, &[0.0, 10.0, 20.0, 30.0], 1),
            vec![1, 1, 1, 1]
        );
    }

    #[test]
    fn scales_auto_mode_with_wheel_velocity() {
        let mut accelerator = WheelScrollAccelerator::new(WheelScrollLines::Auto, true);
        assert_eq!(
            scroll(&mut accelerator, &[0.0, 150.0, 300.0, 450.0], 1),
            vec![1, 1, 1, 1]
        );
        assert_eq!(
            scroll(&mut accelerator, &[1000.0, 1050.0, 1100.0, 1150.0], 1),
            vec![1, 2, 2, 2]
        );
        assert_eq!(
            scroll(&mut accelerator, &[2000.0, 2020.0, 2040.0, 2060.0], 1),
            vec![1, 5, 5, 5]
        );
        assert_eq!(
            scroll(&mut accelerator, &[3000.0, 3010.0, 3020.0, 3030.0], 1),
            vec![1, 6, 6, 6]
        );
    }

    #[test]
    fn does_not_accelerate_bursts_of_events_for_a_single_notch() {
        let mut accelerator = WheelScrollAccelerator::new(WheelScrollLines::Auto, true);
        assert_eq!(
            scroll(&mut accelerator, &[0.0, 3.0, 6.0, 9.0], 1),
            vec![1, 1, 1, 1]
        );
    }

    #[test]
    fn resets_acceleration_on_direction_changes_and_pauses() {
        let mut accelerator = WheelScrollAccelerator::new(WheelScrollLines::Auto, true);
        assert_eq!(
            scroll(&mut accelerator, &[0.0, 20.0, 40.0], 1),
            vec![1, 5, 5]
        );
        assert_eq!(accelerator.next(-1, 60.0), 1);
        assert_eq!(scroll(&mut accelerator, &[500.0, 520.0], 1), vec![1, 5]);
    }

    #[test]
    fn carries_fractional_lines_between_events() {
        let mut accelerator = WheelScrollAccelerator::new(WheelScrollLines::Auto, true);
        assert_eq!(
            scroll(&mut accelerator, &[0.0, 40.0, 80.0, 120.0, 160.0], 1),
            vec![1, 2, 3, 2, 3]
        );
    }
}
