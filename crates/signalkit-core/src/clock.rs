//! Time helpers on top of `clock-kit`.
//!
//! All timestamps persisted by trendkit are wall-clock unix seconds taken
//! from an injected [`Clock`], never from `SystemTime` directly, so tests
//! can drive collection deterministically with `clock_kit::MockClock`.

use clock_kit::Clock;

/// Nanoseconds in one second.
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Current wall-clock time as unix seconds, from the injected clock.
///
/// Saturates at `i64::MAX` for absurdly far-future clocks instead of
/// panicking; the trait object keeps the monotonic-floor guarantees made
/// by `clock-kit`.
#[must_use]
pub fn unix_seconds(clock: &dyn Clock) -> i64 {
    let nanos = clock.now().wall.unix_nanos();
    i64::try_from(nanos / NANOS_PER_SEC).unwrap_or(i64::MAX)
}

/// Render unix seconds as `YYYY-MM-DD` (UTC), for GitHub search qualifiers.
///
/// Implements Howard Hinnant's `civil_from_days` so no date crate is
/// pulled in for one string.
#[must_use]
pub fn iso_date_from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use clock_kit::{MockClock, SystemClock};
    use std::time::Duration;

    #[test]
    fn system_clock_unix_seconds_is_plausible() {
        let now = unix_seconds(&SystemClock);
        // Anything between 2026-01-01 and 2100-01-01 counts as plausible.
        assert!(
            now > 1_767_225_600 && now < 4_102_444_800,
            "implausible system time: {now}"
        );
    }

    #[test]
    fn mock_clock_wall_seconds_move_with_step_wall() {
        let mock = MockClock::start_at(0, 1_000_000_000_000);
        assert_eq!(unix_seconds(&mock), 1_000);
        mock.step_wall(90_000_000_000);
        assert_eq!(unix_seconds(&mock), 1_090);
    }

    #[test]
    fn advance_is_mono_only_by_design() {
        // clock-kit's idiom: `advance` moves the monotonic field only;
        // wall time steps via `step_wall`. Snapshot timestamps are wall
        // scale, so tests that need time to pass must step the wall.
        let mock = MockClock::start_at(0, 1_000_000_000_000);
        mock.advance(Duration::from_secs(90));
        assert_eq!(unix_seconds(&mock), 1_000);
    }

    #[test]
    fn iso_dates_match_known_days() {
        assert_eq!(iso_date_from_unix(0), "1970-01-01");
        assert_eq!(iso_date_from_unix(86_399), "1970-01-01");
        assert_eq!(iso_date_from_unix(86_400), "1970-01-02");
        // 2026-10-17 00:00:00 UTC
        assert_eq!(iso_date_from_unix(1_792_214_400), "2026-10-17");
    }
}
