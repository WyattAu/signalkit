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
/// above `velocity_floor` per day inside the window. Two-repo orgs
/// qualify when their combined velocity clears `high_velocity_floor` —
/// the Gathin23 shape: 2 repos, 10.6k stars/day combined, invisible to
/// a pure min-repos rule.
///
/// # Errors
/// Propagates store failures.
pub fn org_bursts(
    store: &Store,
    window_days: u32,
    min_repos: usize,
    velocity_floor: f64,
    high_velocity_floor: f64,
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
        let combined: f64 = subjects.iter().map(|(_, v)| v).sum();
        // Full min-repos rule, or the two-repo high-velocity shape
        // (Gathin23: 2 repos, 10.6k stars/day combined).
        let qualifies =
            subjects.len() >= min_repos || (subjects.len() >= 2 && combined >= high_velocity_floor);
        if !qualifies {
            continue;
        }
        subjects.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let combined_velocity = combined;
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

/// A composite engagement-trust assessment over one GitHub subject.
///
/// **Experimental.** The score is a documented linear blend, not a
/// judge: start at 100, subtract penalties for fork-ratio, org-burst
/// membership, and velocity-outlier z, add a small bonus for age. Every
/// component is returned alongside so consumers can show the evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct TrustSummary {
    /// Composite score, 0–100.
    pub score: f64,
    /// Band after the single-signal cap: an extreme fork verdict can
    /// never rate above Watch, however clean the rest of the shape is.
    pub band: TrustBand,
    /// Fork ratio in percent.
    pub fork_ratio_pct: f64,
    /// Whether the subject's owner is in an org burst.
    pub burst_member: bool,
    /// Robust z of the subject's velocity (0 when not an outlier).
    pub outlier_z: f64,
    /// Repo age in days.
    pub age_days: i64,
}

/// Weight: percent fork ratio costs up to 40 points.
pub const WEIGHT_FORK_RATIO: f64 = 0.4;
/// Weight: org-burst membership costs 25 points.
pub const WEIGHT_ORG_BURST: f64 = 25.0;
/// Weight: outlier z costs 2 points per z, capped at 10. Velocity
/// outliers mark *attention-worthy* — a legitimately viral repo has a
/// huge z and must not be nuked to zero for it.
pub const OUTLIER_PENALTY_CAP: f64 = 10.0;
/// Bonus: one point per 30 days of age, up to 10.
pub const BONUS_AGE_PER_DAYS: f64 = 30.0;
/// Bonus cap: 10 points.
pub const BONUS_AGE_CAP: f64 = 10.0;

/// Compute the composite trust summary for one subject's numbers.
#[must_use]
pub fn trust_summary(
    stars: i64,
    forks: i64,
    min_stars: i64,
    burst_member: bool,
    outlier_z: f64,
    age_days: i64,
) -> TrustSummary {
    let fork_ratio_pct = if stars > 0 {
        forks as f64 * 100.0 / stars as f64
    } else {
        0.0
    };
    // Fork-ratio penalty only applies above the small-sample floor.
    let ratio_penalty = if stars >= min_stars {
        fork_ratio_pct
    } else {
        0.0
    };
    let z_penalty = (outlier_z.max(0.0) * 2.0).min(OUTLIER_PENALTY_CAP);
    let age_bonus = (age_days as f64 / BONUS_AGE_PER_DAYS).min(BONUS_AGE_CAP);
    let score = (100.0
        - ratio_penalty * WEIGHT_FORK_RATIO
        - if burst_member { WEIGHT_ORG_BURST } else { 0.0 }
        - z_penalty
        + age_bonus)
        .clamp(0.0, 100.0);
    // Single-signal cap: no single signal may be ignored. An extreme
    // fork verdict holds the band at Watch even when everything else
    // looks clean.
    let verdict = fork_verdict(stars, forks, min_stars);
    let band = match trust_band(score) {
        TrustBand::Trusted if verdict == ForkVerdict::Extreme => TrustBand::Watch,
        other => other,
    };
    TrustSummary {
        score,
        band,
        fork_ratio_pct,
        burst_member,
        outlier_z,
        age_days,
    }
}

/// Verdict band for a [`TrustSummary`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustBand {
    /// >= 80.
    Trusted,
    /// 50–79.
    Watch,
    /// < 50.
    Suspect,
}

/// Band for a composite score.
#[must_use]
pub fn trust_band(score: f64) -> TrustBand {
    if score >= 80.0 {
        TrustBand::Trusted
    } else if score >= 50.0 {
        TrustBand::Watch
    } else {
        TrustBand::Suspect
    }
}

/// A GitHub subject whose fork ratio cleared the elevated threshold.
#[derive(Debug, Clone, PartialEq)]
pub struct TrustRow {
    /// `owner/repo`.
    pub key: String,
    /// The composite summary (see [`TrustSummary`]).
    pub summary: TrustSummary,
}

/// Composite trust rows for every tracked GitHub subject inside the
/// window: fork ratio + org-burst membership + outlier z + age, blended
/// by [`trust_summary`]. Sorted worst-first.
///
/// # Errors
/// Propagates store failures.
pub fn trust_rows(store: &Store, window_days: u32, min_stars: i64) -> Result<Vec<TrustRow>> {
    let bursts = org_bursts(store, window_days, 3, 25.0, 5_000.0)?;
    let burst_owners: std::collections::HashSet<String> =
        bursts.iter().map(|b| b.owner.clone()).collect();
    let outliers = velocity_outliers(store, window_days, 2_000)?;
    let z_by_key: std::collections::BTreeMap<String, f64> =
        outliers.into_iter().map(|o| (o.key, o.robust_z)).collect();

    let now_epoch_days = {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        secs / 86_400
    };

    let window_secs = i64::from(window_days) * 86_400;
    store.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "
            WITH latest AS (SELECT subject_id, MAX(ts) AS ts FROM observations GROUP BY subject_id)
            SELECT s.key, s.created_at, o.score, o.extra1
            FROM observations o
            JOIN latest l ON l.subject_id = o.subject_id AND l.ts = o.ts
            JOIN subjects s ON s.subject_id = o.subject_id
            WHERE s.kind = 'github_repo'
              AND o.ts >= (SELECT MAX(ts) FROM observations) - ?1
            ORDER BY o.extra1 * 1.0 / MAX(o.score, 1) DESC
            ",
        )?;
        let rows = stmt.query_map(rusqlite::params![window_secs], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (key, created_at, stars, forks) = row?;
            let owner = key.split('/').next().unwrap_or(&key).to_owned();
            let age_days = created_at
                .as_deref()
                .map(|c| age_days_from(c, now_epoch_days))
                .unwrap_or(0);
            let z = z_by_key.get(&key).copied().unwrap_or(0.0);
            let summary = trust_summary(
                stars,
                forks,
                min_stars,
                burst_owners.contains(&owner),
                z,
                age_days,
            );
            out.push(TrustRow { key, summary });
        }
        Ok(out)
    })
}

/// Days between an RFC 3339 date prefix and `now_epoch_days`; 0 when
/// unparseable.
fn age_days_from(created_at: &str, now_epoch_days: i64) -> i64 {
    let Some(day) = created_at.get(0..10) else {
        return 0;
    };
    let mut parts = [1970i64, 1, 1];
    for (i, chunk) in day.split('-').take(3).enumerate() {
        if let Ok(v) = chunk.parse::<i64>() {
            parts[i] = v;
        }
    }
    let (y, m, d) = (parts[0], parts[1], parts[2]);
    let days_before_month = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let y_days = (y - 1970) * 365 + (y - 1969) / 4 - (y - 1901) / 100 + (y - 1921) / 400;
    now_epoch_days - (y_days + days_before_month[(m - 1).clamp(0, 11) as usize] + d - 1)
}

/// A cluster of same-named repos spread across distinct orgs with
/// farm-shaped engagement — the "template cloned into many throwaway
/// orgs" signature (e.g. four orgs each shipping a `.github` at ~99%
/// fork ratio).
#[derive(Debug, Clone, PartialEq)]
pub struct NameCloneGroup {
    /// Repo name shared by the cluster.
    pub name: String,
    /// Distinct owners in the cluster.
    pub orgs: Vec<String>,
    /// Combined stars.
    pub total_stars: i64,
    /// Combined forks.
    pub total_forks: i64,
    /// Forks per 100 stars across the cluster.
    pub avg_ratio_pct: f64,
}

/// Detect same-name clone clusters: at least `min_orgs` distinct owners
/// shipping a repo with the same name, where the cluster's aggregate
/// fork ratio clears `min_ratio_pct`. Only GitHub subjects inside the
/// window are considered.
///
/// # Errors
/// Propagates store failures.
pub fn name_clones(
    store: &Store,
    window_days: u32,
    min_orgs: usize,
    min_ratio_pct: f64,
) -> Result<Vec<NameCloneGroup>> {
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
              AND o.score > 0
            ",
        )?;
        let rows = stmt.query_map(rusqlite::params![window_secs], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;

        // name -> (owner set, stars, forks)
        let mut clusters: std::collections::BTreeMap<
            String,
            (std::collections::BTreeSet<String>, i64, i64),
        > = std::collections::BTreeMap::new();
        for row in rows {
            let (key, stars, forks) = row?;
            let Some((owner, name)) = key.split_once('/') else {
                continue;
            };
            let entry = clusters.entry(name.to_owned()).or_default();
            entry.0.insert(owner.to_owned());
            entry.1 += stars;
            entry.2 += forks;
        }

        let mut out = Vec::new();
        for (name, (owners, total_stars, total_forks)) in clusters {
            if owners.len() < min_orgs {
                continue;
            }
            let avg_ratio_pct = total_forks as f64 * 100.0 / total_stars.max(1) as f64;
            if avg_ratio_pct < min_ratio_pct {
                continue;
            }
            out.push(NameCloneGroup {
                name,
                orgs: owners.into_iter().collect(),
                total_stars,
                total_forks,
                avg_ratio_pct,
            });
        }
        out.sort_by(|a, b| {
            b.avg_ratio_pct
                .partial_cmp(&a.avg_ratio_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(out)
    })
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
        let rows = stmt.query_map(rusqlite::params![window_secs, min_stars], |row| {
            let key: String = row.get(0)?;
            let stars: i64 = row.get(1)?;
            let forks: i64 = row.get(2)?;
            Ok((key, stars, forks))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (key, stars, forks) = row?;
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
        let bursts = org_bursts(&store, 14, 2, 50.0, 5_000.0).unwrap();
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
    fn trust_summary_penalizes_each_signal() {
        // Clean, old repo: bonus applies, score pins at the cap.
        let clean = trust_summary(5_000, 200, 200, false, 0.0, 600);
        assert!(
            (clean.score - 100.0).abs() < 1e-9,
            "clean pins at 100, got {}",
            clean.score
        );
        assert_eq!(trust_band(clean.score), TrustBand::Trusted);

        // The playgta5 shape: lone repo, ~49% ratio, no burst, no
        // outlier, old enough for the full age bonus — the composite
        // clears 80, but the extreme verdict caps the band at Watch.
        // No single signal may be ignored.
        let lone = trust_summary(1_688, 822, 200, false, 0.0, 400);
        assert!(
            lone.score > 80.0,
            "score {} should clear the trusted line",
            lone.score
        );
        assert_eq!(lone.band, TrustBand::Watch, "extreme verdict caps the band");

        // Farmed: 50% ratio + burst + z=10.
        let farmed = trust_summary(1_000, 500, 200, true, 10.0, 5);
        assert_eq!(trust_band(farmed.score), TrustBand::Suspect);
        // 100 - 50*0.4 (20) - 25 (burst) - min(20,10) (outlier cap) + 5/30
        // age = 45.167.
        assert!(
            (farmed.score - 45.1667).abs() < 0.01,
            "got {}",
            farmed.score
        );

        // Small samples skip the ratio penalty entirely.
        let tiny = trust_summary(50, 40, 200, false, 0.0, 400);
        assert!(tiny.score >= 100.0 - 1e-9);
    }

    #[test]
    fn trust_score_is_clamped() {
        let worst = trust_summary(1_000, 2_000, 200, true, 50.0, 0);
        assert!((worst.score - 0.0).abs() < 1e-9);
    }

    #[test]
    fn name_clones_catch_cross_org_farms() {
        let store = Store::open_in_memory().unwrap();
        // Four orgs, same name `.github`, all farm ratios.
        for owner in ["fort-tool", "cod-tool", "cs2-tools-new", "apex-pro12"] {
            let id = store
                .upsert_subject(&repo(&format!("{owner}/.github")))
                .unwrap();
            store.insert_observation(id, &obs(0, 270, 269)).unwrap();
        }
        // A legit same-name cluster with normal ratios must not flag.
        for owner in ["alice", "bob"] {
            let id = store
                .upsert_subject(&repo(&format!("{owner}/cli")))
                .unwrap();
            store.insert_observation(id, &obs(0, 5_000, 200)).unwrap();
        }

        let groups = name_clones(&store, 14, 3, 40.0).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, ".github");
        assert_eq!(groups[0].orgs.len(), 4);
        assert!(groups[0].avg_ratio_pct > 95.0);
    }

    #[test]
    fn name_clones_respect_min_orgs() {
        let store = Store::open_in_memory().unwrap();
        for owner in ["o1", "o2"] {
            let id = store
                .upsert_subject(&repo(&format!("{owner}/.github")))
                .unwrap();
            store.insert_observation(id, &obs(0, 270, 269)).unwrap();
        }
        assert!(name_clones(&store, 14, 3, 40.0).unwrap().is_empty());
    }

    #[test]
    fn org_bursts_catch_two_repo_high_velocity_orgs() {
        // The Gathin23 shape: two repos, ~10.6k stars/day combined, no
        // third repo — invisible to min_repos=3, caught by the
        // high-velocity floor.
        let store = Store::open_in_memory().unwrap();
        seed(&store, "gathin/tv", 12_500, 1_399, 8_263.0);
        seed(&store, "gathin/vibe", 2_674, 500, 2_389.0);
        let bursts = org_bursts(&store, 14, 3, 25.0, 5_000.0).unwrap();
        assert_eq!(bursts.len(), 1);
        assert_eq!(bursts[0].owner, "gathin");
        assert!((bursts[0].combined_velocity - 10_652.0).abs() < 1.0);
    }

    #[test]
    fn org_bursts_ignore_calm_pairs() {
        let store = Store::open_in_memory().unwrap();
        seed(&store, "calm/a", 1_000, 50, 20.0);
        seed(&store, "calm/b", 1_000, 50, 20.0);
        assert!(org_bursts(&store, 14, 3, 25.0, 5_000.0).unwrap().is_empty());
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

    #[test]
    fn trust_rows_score_farm_below_clean() {
        let store = Store::open_in_memory().unwrap();
        seed(&store, "farm/a", 5_000, 2_500, 100.0); // 50% ratio + burst member
        seed(&store, "farm/b", 4_000, 2_000, 90.0);
        seed(&store, "farm/c", 3_000, 1_500, 80.0); // third repo trips min_repos=3
        seed(&store, "clean/c", 8_000, 300, 120.0);
        let rows = trust_rows(&store, 14, 200).unwrap();
        let farm = rows.iter().find(|r| r.key == "farm/a").unwrap();
        let clean = rows.iter().find(|r| r.key == "clean/c").unwrap();
        assert!(farm.summary.burst_member);
        assert!(!clean.summary.burst_member);
        assert!(
            farm.summary.score < clean.summary.score,
            "farm {} must score below clean {}",
            farm.summary.score,
            clean.summary.score
        );
        // 100 - 20 (50% ratio x 0.4) - 25 (burst) - 0 (no outlier) + 0 age = 55.
        assert_eq!(trust_band(farm.summary.score), TrustBand::Watch);
        assert!(
            (farm.summary.score - 55.0).abs() < 1.0,
            "got {}",
            farm.summary.score
        );
    }
}
