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

/// A GitHub subject whose fork ratio cleared the elevated threshold.
#[derive(Debug, Clone, PartialEq)]
pub struct ForkFlag {
    /// `owner/repo`.
    pub key: String,
    /// Latest star count.
    pub stars: i64,
    /// Latest fork count.
    pub forks: i64,
    /// Forks per 100 stars.
    pub ratio_pct: f64,
    /// The verdict at the store's standard thresholds.
    pub verdict: ForkVerdict,
}

/// GitHub subjects inside the window whose latest fork ratio is at
/// least Elevated (>= 20%), with at least `min_stars`.
///
/// # Errors
/// Propagates store failures.
pub fn fork_flags(store: &Store, window_days: u32, min_stars: i64) -> Result<Vec<ForkFlag>> {
    let window_secs = i64::from(window_days) * 86_400;
    store.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "
            WITH latest AS (SELECT subject_id, MAX(ts) AS ts FROM observations GROUP BY subject_id)
            SELECT s.key, o.score, o.extra1
            FROM observations o
            JOIN latest l ON l.subject_id = o.subject_id AND l.ts = o.ts
            JOIN subjects s ON s.subject_id = o.subject_id
            WHERE s.kind = 'github_repo'
              AND o.ts >= (SELECT MAX(ts) FROM observations) - ?1
              AND o.score >= ?2
            ORDER BY o.extra1 * 1.0 / MAX(o.score, 1) DESC
            ",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![window_secs, min_stars], |row| {
                let key: String = row.get(0)?;
                let stars: i64 = row.get(1)?;
                let forks: i64 = row.get(2)?;
                Ok((key, stars, forks))
            })
            .map_err(rusqlite::Error::from)?;
        let mut out = Vec::new();
        for row in rows {
            let (key, stars, forks) = row.map_err(rusqlite::Error::from)?;
            let ratio_pct = forks as f64 * 100.0 / stars.max(1) as f64;
            let verdict = fork_verdict(stars, forks, min_stars);
            if verdict != ForkVerdict::Normal {
                out.push(ForkFlag {
                    key,
                    stars,
                    forks,
                    ratio_pct,
                    verdict,
                });
            }
        }
        Ok(out)
    })
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
        let mad_raw = devs[devs.len() / 2] * 1.4826;
        // Floor the MAD: when velocities are almost identical (two
        // snapshot HN stories), MAD ~ 0 would flag everything with
        // astronomical z-scores. 5% of median, at least 1/day.
        let mad = mad_raw.max(median.abs() * 0.05).max(1.0);
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

    // Mass-flag guard: when more than 10% of a kind is "outlying", the
    // distribution is the problem, not the subjects — keep only the
    // strongest per kind up to that budget.
    let mut total_by_kind: std::collections::BTreeMap<SourceKind, usize> =
        std::collections::BTreeMap::new();
    for r in &rows {
        *total_by_kind.entry(r.kind).or_default() += 1;
    }
    let mut kept: std::collections::BTreeMap<SourceKind, usize> = std::collections::BTreeMap::new();
    out.retain(|o| {
        let total = total_by_kind.get(&o.kind).copied().unwrap_or(0);
        let count = kept.entry(o.kind).or_default();
        *count += 1;
        *count * 10 <= total
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

    #[test]
    fn fork_flags_flag_only_the_suspicious() {
        let store = Store::open_in_memory().unwrap();
        seed(&store, "farm/a", 5_000, 2_500, 100.0); // 50% -> Extreme
        seed(&store, "clean/b", 8_000, 300, 80.0); // 3.75% -> Normal
        let flags = fork_flags(&store, 14, 200).unwrap();
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].key, "farm/a");
        assert_eq!(flags[0].verdict, ForkVerdict::Extreme);
        assert!((flags[0].ratio_pct - 50.0).abs() < 1e-9);
    }
}
