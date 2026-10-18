//! Integrity signals over a signalkit store.
//!
//! Trending metrics get farmed: forks are cheap, sibling repos burst in
//! coordinated waves, and star counts detach from every other counter.
//! These heuristics do not prove manipulation — they flag shapes that
//! honest repos rarely have:
//!
//! - **Fork-ratio verdicts**: forks/stars far above the 2–10% norm.
//! - **Org-burst detection**: one owner with many subjects spiking in
//!   the same window (the "5 sibling repos trending together" shape).
//! - **Velocity outliers**: robust z-scores (median/MAD) per kind, so
//!   one runaway subject does not define its own baseline.
//!
//! Thresholds are deliberately conservative and printed alongside the
//! numbers: consumers should show the evidence, not just a verdict.

use signalkit_core::model::SourceKind;
use signalkit_core::store::Store;
use signalkit_core::Result;

/// How suspicious a fork ratio is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkVerdict {
    /// forks/stars within the 0–20% norm (or sample too small to judge).
    Normal,
    /// 20–40%: unusual, worth a look.
    Elevated,
    /// >40%: the shape of farmed engagement.
    Extreme,
}

/// Classify a fork ratio. Small repos (< `min_stars` stars) are always
/// [`ForkVerdict::Normal`] — small-sample ratios are noise.
#[must_use]
pub fn fork_verdict(stars: i64, forks: i64, min_stars: i64) -> ForkVerdict {
    if stars < min_stars {
        return ForkVerdict::Normal;
    }
    let ratio = forks as f64 / stars.max(1) as f64;
    if ratio > 0.40 {
        ForkVerdict::Extreme
    } else if ratio > 0.20 {
        ForkVerdict::Elevated
    } else {
        ForkVerdict::Normal
    }
}

/// One owner whose subjects all moved hard in the same window.
#[derive(Debug, Clone, PartialEq)]
pub struct BurstGroup {
    /// Owner login (`owner` half of `owner/repo`).
    pub owner: String,
    /// How many of the owner's subjects cleared the velocity floor.
    pub subjects: usize,
    /// Combined stars-Δ per day across those subjects.
    pub combined_velocity: f64,
    /// The subjects, fastest first.
    pub keys: Vec<String>,
}

/// Detect org bursts: GitHub owners with at least `min_repos` subjects
/// above `velocity_floor` per day inside the window.
///
/// # Errors
/// Propagates store failures.
pub fn org_bursts(
    store: &Store,
    window_days: u32,
    min_repos: usize,
    velocity_floor: f64,
) -> Result<Vec<BurstGroup>> {
    let rows = store.velocity_rows(window_days, 5_000)?;
    let mut groups: std::collections::BTreeMap<String, Vec<(String, f64)>> =
        std::collections::BTreeMap::new();
    for row in rows {
        if row.kind != SourceKind::GithubRepo || row.velocity_per_day < velocity_floor {
            continue;
        }
        let owner = row.key.split('/').next().unwrap_or(&row.key).to_owned();
        groups
            .entry(owner)
            .or_default()
            .push((row.key, row.velocity_per_day));
    }
    let mut out = Vec::new();
    for (owner, mut subjects) in groups {
        if subjects.len() < min_repos {
            continue;
        }
        subjects.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let combined_velocity: f64 = subjects.iter().map(|(_, v)| v).sum();
        out.push(BurstGroup {
            keys: subjects.iter().map(|(k, _)| k.clone()).collect(),
            owner,
            subjects: subjects.len(),
            combined_velocity,
        });
    }
    out.sort_by(|a, b| {
        b.combined_velocity
            .partial_cmp(&a.combined_velocity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
}

/// A velocity outlier with its robust z-score.
#[derive(Debug, Clone, PartialEq)]
pub struct Outlier {
    /// Subject kind.
    pub kind: SourceKind,
    /// Subject key.
    pub key: String,
    /// Velocity per day.
    pub velocity_per_day: f64,
    /// `(v - median) / (1.4826 * MAD)`; >= 3.5 is the classic flag.
    pub robust_z: f64,
}

/// Velocity outliers per kind via median/MAD robust z-scores.
///
/// Kinds with fewer than 8 subjects are skipped (MAD is meaningless on
/// tiny samples).
///
/// # Errors
/// Propagates store failures.
pub fn velocity_outliers(store: &Store, window_days: u32, limit: u32) -> Result<Vec<Outlier>> {
    let rows = store.velocity_rows(window_days, limit)?;
    let mut by_kind: std::collections::BTreeMap<SourceKind, Vec<f64>> =
        std::collections::BTreeMap::new();
    for r in &rows {
        by_kind.entry(r.kind).or_default().push(r.velocity_per_day);
    }

    let mut stats: std::collections::BTreeMap<SourceKind, (f64, f64)> =
        std::collections::BTreeMap::new();
    for (kind, velocities) in &by_kind {
        if velocities.len() < 8 {
            continue;
        }
        let mut sorted = velocities.clone();
        sorted.sort_by(f64::total_cmp);
        let median = sorted[sorted.len() / 2];
        let mut devs: Vec<f64> = velocities.iter().map(|v| (v - median).abs()).collect();
        devs.sort_by(f64::total_cmp);
        let mad = devs[devs.len() / 2].max(1e-9) * 1.4826;
        stats.insert(*kind, (median, mad));
    }

    let mut out = Vec::new();
    for r in &rows {
        let Some((median, mad)) = stats.get(&r.kind) else {
            continue;
        };
        let z = (r.velocity_per_day - median) / mad;
        if z >= 3.5 {
            out.push(Outlier {
                kind: r.kind,
                key: r.key.clone(),
                velocity_per_day: r.velocity_per_day,
                robust_z: z,
            });
        }
    }
    out.sort_by(|a, b| {
        b.robust_z
            .partial_cmp(&a.robust_z)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    use signalkit_core::model::{Observation, SubjectRecord};
    use signalkit_core::store::Store;

    fn repo(key: &str) -> SubjectRecord {
        SubjectRecord {
            kind: SourceKind::GithubRepo,
            key: key.to_owned(),
            title: None,
            lang: None,
            url: None,
            topics: Vec::new(),
            created_at: None,
        }
    }

    fn obs(ts: i64, stars: i64, forks: i64) -> Observation {
        Observation {
            ts,
            score: stars,
            extra1: forks,
            extra2: 0,
        }
    }

    fn seed(store: &Store, key: &str, stars: i64, forks: i64, v_day: f64) {
        let id = store.upsert_subject(&repo(key)).unwrap();
        // Two snapshots 1 day apart: stars grow by v_day, forks grow to `forks`.
        store
            .insert_observation(id, &obs(0, stars - v_day as i64, forks / 2))
            .unwrap();
        store
            .insert_observation(id, &obs(86_400, stars, forks))
            .unwrap();
    }

    #[test]
    fn fork_verdict_thresholds() {
        assert_eq!(fork_verdict(1_000, 50, 200), ForkVerdict::Normal); // 5%
        assert_eq!(fork_verdict(1_000, 250, 200), ForkVerdict::Elevated); // 25%
        assert_eq!(fork_verdict(1_000, 500, 200), ForkVerdict::Extreme); // 50%
                                                                         // Small samples never flag.
        assert_eq!(fork_verdict(50, 40, 200), ForkVerdict::Normal);
    }

    #[test]
    fn org_bursts_group_by_owner() {
        let store = Store::open_in_memory().unwrap();
        seed(&store, "farm/a", 5_000, 2_500, 100.0);
        seed(&store, "farm/b", 4_000, 2_000, 90.0);
        seed(&store, "honest/c", 8_000, 300, 120.0);
        let bursts = org_bursts(&store, 14, 2, 50.0).unwrap();
        assert_eq!(bursts.len(), 1);
        assert_eq!(bursts[0].owner, "farm");
        assert_eq!(bursts[0].subjects, 2);
        assert!((bursts[0].combined_velocity - 190.0).abs() < 1e-9);
    }

    #[test]
    fn velocity_outliers_use_robust_z() {
        let store = Store::open_in_memory().unwrap();
        // Ten calm repos at ~10/day.
        for i in 0..10 {
            seed(&store, &format!("calm/r{i}"), 1_000, 50, 10.0);
        }
        // One runaway at 500/day.
        seed(&store, "wild/x", 1_500, 60, 500.0);
        let out = velocity_outliers(&store, 14, 100).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key, "wild/x");
        assert!(out[0].robust_z >= 3.5);
    }

    #[test]
    fn small_kinds_are_skipped() {
        let store = Store::open_in_memory().unwrap();
        seed(&store, "lonely/a", 1_000, 10, 900.0);
        assert!(velocity_outliers(&store, 14, 100).unwrap().is_empty());
    }
}
