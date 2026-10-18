//! Velocity and personal-relevance scoring.
//!
//! Two deliberate properties:
//!
//! 1. **Honesty about windows.** A velocity over a zero-length window is
//!    meaningless, so windows are clamped to [`MIN_WINDOW_SECS`]; rows with
//!    a single snapshot report zero velocity rather than a fabricated spike.
//! 2. **Explainability.** [`personal_score`] is a small integer-weighted
//!    overlap bonus plus a compressed velocity term — no opaque ML, every
//!    point traceable to a topic, language, or velocity.

/// Clamp for velocity windows: below one hour, treat as one hour.
pub const MIN_WINDOW_SECS: i64 = 3_600;

/// Stars gained per day over `window_secs`, clamped to [`MIN_WINDOW_SECS`].
///
/// Negative velocities are preserved: repos can lose stars.
#[must_use]
pub fn velocity_per_day(stars_prev: i64, stars_now: i64, window_secs: i64) -> f64 {
    let secs = window_secs.max(MIN_WINDOW_SECS);
    (stars_now - stars_prev) as f64 * 86_400.0 / secs as f64
}

/// Personal relevance score for one trending repository.
///
/// `topic_overlap` counts the repo's topics present in the profile,
/// `language_match` is true when the repo's primary language appears in
/// the profile, and `velocity_per_day` is compressed with `ln(1 + v)` so
/// one runaway repo cannot dominate. Every unit of the result is
/// attributable: `2` per shared topic, `3` for a language match, and a
/// logarithmic velocity bonus.
#[must_use]
pub fn personal_score(velocity_per_day: f64, topic_overlap: usize, language_match: bool) -> f64 {
    let overlap = f64::from(u16::try_from(topic_overlap).unwrap_or(u16::MAX)) * 2.0;
    let language = if language_match { 3.0 } else { 0.0 };
    let velocity = velocity_per_day.abs().ln_1p() / 4.0;
    overlap + language + velocity
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn velocity_scales_with_window() {
        // 70 stars over 1 day = 70/day.
        assert!((velocity_per_day(0, 70, 86_400) - 70.0).abs() < 1e-9);
        // 70 stars over 7 days = 10/day.
        assert!((velocity_per_day(0, 70, 7 * 86_400) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn velocity_clamps_tiny_windows() {
        // 7 stars in 1 second must not report 604800/day.
        let v = velocity_per_day(0, 7, 1);
        assert!((v - 7.0 * 86_400.0 / MIN_WINDOW_SECS as f64).abs() < 1e-9);
    }

    #[test]
    fn velocity_can_be_negative() {
        assert!((velocity_per_day(100, 90, 86_400) + 10.0).abs() < 1e-9);
    }

    #[test]
    fn personal_score_is_explained_by_parts() {
        let base = personal_score(0.0, 0, false);
        assert!((base - 0.0).abs() < 1e-9);
        assert!((personal_score(0.0, 2, false) - 4.0).abs() < 1e-9);
        assert!((personal_score(0.0, 0, true) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn personal_score_compresses_velocity() {
        let v1 = personal_score(10.0, 0, false);
        let v2 = personal_score(1_000.0, 0, false);
        // ln compresses: 100x the velocity is far less than 100x the bonus.
        assert!(v2 < v1 * 100.0);
        assert!(v2 > v1);
    }
}
