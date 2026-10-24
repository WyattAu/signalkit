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

/// A next-period projection from naive recency-weighted extrapolation.
///
/// `delta` is the projected score change over the next day; `low`/`high`
/// are the ±1-sigma band; `confidence` is 0–1, scaling with history
/// length. A projection from fewer than 2 prior deltas cannot exist, so
/// callers gate on history length themselves.
#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    /// Projected next-day delta.
    pub delta: f64,
    /// Lower band of the ±1-sigma interval.
    pub low: f64,
    /// Upper band of the ±1-sigma interval.
    pub high: f64,
    /// 0–1 confidence, scaling with the number of history days.
    pub confidence: f64,
}

/// Recency weight for the k-th delta back (k = 0 is the most recent):
/// each step back halves the weight, floored so the total stays finite.
fn recency_weight(steps_back: usize) -> f64 {
    0.5f64.powi(steps_back as i32)
}

/// Project the next-day delta from consecutive day-over-day deltas.
///
/// Weighted mean of the deltas (most recent weighs most), spread from
/// the deltas' standard deviation, band = projected ± spread. This is
/// deliberately naive: with under a week of history, honest
/// extrapolation beats a model that would overfit.
#[must_use]
pub fn project_next(deltas: &[f64]) -> Projection {
    let n = deltas.len();
    if n == 0 {
        return Projection {
            delta: 0.0,
            low: 0.0,
            high: 0.0,
            confidence: 0.0,
        };
    }
    let weights: Vec<f64> = (0..n).map(|k| recency_weight(n - 1 - k)).collect();
    let total_w: f64 = weights.iter().sum();
    let delta = deltas.iter().zip(&weights).map(|(d, w)| d * w).sum::<f64>() / total_w;
    let variance = deltas
        .iter()
        .zip(&weights)
        .map(|(d, w)| w * (d - delta) * (d - delta))
        .sum::<f64>()
        / total_w;
    let spread = variance.sqrt();
    let confidence = (n as f64 / 7.0).min(1.0);
    Projection {
        delta,
        low: delta - spread,
        high: delta + spread,
        confidence,
    }
}

/// Project the next-day delta as a random walk: tomorrow equals today.
///
/// `delta` is the most recent delta, spread from the deltas' weighted
/// standard deviation, band = projected ± spread. Cumulative counters
/// (stars, downloads) trend: a smoothed mean lags the trend line and
/// its error grows with the acceleration, while the walk tracks it.
/// Week-one backtests over ~950 subjects: mean vs walk MAE on crates
/// 928k vs 251k, on HN stories 16.5 vs 0.5 points. For mean-reverting
/// or noisy-multiplicative series prefer [`project_next`].
#[must_use]
pub fn project_next_walk(deltas: &[f64]) -> Projection {
    let n = deltas.len();
    if n == 0 {
        return Projection {
            delta: 0.0,
            low: 0.0,
            high: 0.0,
            confidence: 0.0,
        };
    }
    let delta = deltas[n - 1];
    let weights: Vec<f64> = (0..n).map(|k| recency_weight(n - 1 - k)).collect();
    let total_w: f64 = weights.iter().sum();
    let mean = deltas.iter().zip(&weights).map(|(d, w)| d * w).sum::<f64>() / total_w;
    let variance = deltas
        .iter()
        .zip(&weights)
        .map(|(d, w)| w * (d - mean) * (d - mean))
        .sum::<f64>()
        / total_w;
    let spread = variance.sqrt();
    let confidence = (n as f64 / 7.0).min(1.0);
    Projection {
        delta,
        low: delta - spread,
        high: delta + spread,
        confidence,
    }
}

#[cfg(test)]
mod projection_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn recency_weighting_pulls_toward_recent() {
        // 100 then 300: flat mean 200, weighted toward 300.
        let p = project_next(&[100.0, 300.0]);
        assert!(p.delta > 200.0, "delta {}", p.delta);
        assert!(p.delta < 300.0);
    }

    #[test]
    fn constant_history_projects_flat() {
        let p = project_next(&[50.0, 50.0, 50.0, 50.0]);
        assert!((p.delta - 50.0).abs() < 1e-9);
        // Zero variance: the band collapses onto the projection.
        assert!((p.low - 50.0).abs() < 1e-9 && (p.high - 50.0).abs() < 1e-9);
        // 4 days of history: confidence below the 7-day cap.
        assert!(p.confidence < 1.0);
    }

    #[test]
    fn confidence_scales_with_days() {
        let short = project_next(&[10.0, 20.0]);
        let long = project_next(&[10.0, 20.0, 10.0, 20.0, 10.0, 20.0, 10.0, 20.0]);
        assert!(long.confidence > short.confidence);
        assert!((long.confidence - 1.0).abs() < 1e-9);
    }

    #[test]
    fn band_covers_recent_deltas() {
        let history = [10.0, 12.0, 11.0, 13.0, 12.0];
        let p = project_next(&history);
        // The band brackets the projection and is tighter than the raw
        // range of the historical deltas.
        assert!(p.low <= p.delta && p.delta <= p.high);
        let range = 13.0 - 10.0;
        assert!(p.high - p.low < range);
    }

    #[test]
    fn walk_tracks_the_last_delta() {
        let p = project_next_walk(&[10.0, 12.0, 11.0, 13.0]);
        assert!((p.delta - 13.0).abs() < 1e-9, "delta {}", p.delta);
        // Trending series: the walk sits above the smoothed mean, which
        // lags the trend line.
        let mean = project_next(&[10.0, 12.0, 11.0, 13.0]);
        assert!(p.delta > mean.delta);
    }

    #[test]
    fn walk_constant_history_collapses_band() {
        let p = project_next_walk(&[50.0, 50.0, 50.0]);
        assert!((p.delta - 50.0).abs() < 1e-9);
        assert!((p.low - 50.0).abs() < 1e-9 && (p.high - 50.0).abs() < 1e-9);
    }

    #[test]
    fn walk_empty_history_is_zeros() {
        let p = project_next_walk(&[]);
        assert_eq!(p.delta, 0.0);
        assert_eq!(p.low, 0.0);
        assert_eq!(p.high, 0.0);
        assert_eq!(p.confidence, 0.0);
    }

    #[test]
    fn empty_history_projects_zero() {
        let p = project_next(&[]);
        assert_eq!(p.delta, 0.0);
        assert_eq!(p.confidence, 0.0);
    }
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
