//! Shared data model for snapshot collection and reporting.
//!
//! One subject/observation pair serves every source: a *subject* is a
//! thing being tracked (repo, HN story, crate), an *observation* is one
//! point-in-time reading of its headline counter. Velocity is always
//! `Δscore / Δdays` over the window; what "score" means is documented
//! per [`SourceKind`].

use std::fmt;

/// What kind of thing a subject is, and what its score counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourceKind {
    /// GitHub repository; score = stars.
    GithubRepo,
    /// Hacker News story; score = points.
    HnStory,
    /// crates.io crate; score = total downloads.
    Crate,
    /// HuggingFace model; score = downloads.
    HfModel,
}

impl SourceKind {
    /// Stable string stored in the database.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            SourceKind::GithubRepo => "github_repo",
            SourceKind::HnStory => "hn_story",
            SourceKind::Crate => "crate",
            SourceKind::HfModel => "hf_model",
        }
    }

    /// Short tag for report tables.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            SourceKind::GithubRepo => "GH",
            SourceKind::HnStory => "HN",
            SourceKind::Crate => "CR",
            SourceKind::HfModel => "HF",
        }
    }

    /// Parse the database string back.
    #[must_use]
    pub fn parse(s: &str) -> Option<SourceKind> {
        match s {
            "github_repo" => Some(SourceKind::GithubRepo),
            "hn_story" => Some(SourceKind::HnStory),
            "crate" => Some(SourceKind::Crate),
            "hf_model" => Some(SourceKind::HfModel),
            _ => None,
        }
    }
}

impl fmt::Display for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A thing being tracked, stored once per `(kind, key)` and enriched on
/// later scans. `title` carries the human-readable line (repo
/// description, story title, crate description).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubjectRecord {
    /// Subject type.
    pub kind: SourceKind,
    /// Source-stable identifier: `owner/repo`, HN object id, crate name.
    pub key: String,
    /// Human-readable line.
    pub title: Option<String>,
    /// Primary language (GitHub only today).
    pub lang: Option<String>,
    /// Canonical URL.
    pub url: Option<String>,
    /// Topics/keywords, lowercased by the source.
    pub topics: Vec<String>,
    /// Source creation timestamp (RFC 3339 or epoch), when available.
    pub created_at: Option<String>,
}

/// One point-in-time reading of a subject's counters.
///
/// `extra1`/`extra2` are source-defined (GitHub: forks/watchers, HN:
/// comments, crates.io: recent downloads) — velocity only ever uses
/// `score`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// Unix seconds at reading time.
    pub ts: i64,
    /// Headline counter (stars / points / downloads).
    pub score: i64,
    /// Source-defined secondary counter.
    pub extra1: i64,
    /// Source-defined tertiary counter.
    pub extra2: i64,
}

/// One row from a user's repository listing, used for the personal profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoListed {
    /// GitHub full name (`owner/repo`).
    pub full_name: String,
    /// Primary language, if any.
    pub language: Option<String>,
    /// Repository topics, lowercased by GitHub.
    pub topics: Vec<String>,
    /// Short description, if any.
    pub description: Option<String>,
    /// True for forks (excluded from profile weighting).
    pub is_fork: bool,
    /// Last push timestamp as returned by GitHub, if present.
    pub pushed_at: Option<String>,
    /// Star count at listing time.
    pub stars: i64,
}

/// A subject that kept appearing across distinct days — persistence is
/// the strongest wave signal: single-day spikes fade, multi-day
/// presence compounds.
#[derive(Debug, Clone, PartialEq)]
pub struct PersistentRow {
    /// Subject type.
    pub kind: SourceKind,
    /// Source-stable identifier.
    pub key: String,
    /// Distinct UTC days with at least one observation in the window.
    pub days: i64,
    /// Most recent observed score.
    pub score_now: i64,
    /// Velocity per day across the window.
    pub velocity_per_day: f64,
}

/// A velocity computation for one subject over a snapshot window.
#[derive(Debug, Clone, PartialEq)]
pub struct VelocityRow {
    /// Subject type.
    pub kind: SourceKind,
    /// Source-stable identifier.
    pub key: String,
    /// Human-readable line, if any.
    pub title: Option<String>,
    /// Primary language, if any.
    pub lang: Option<String>,
    /// Canonical URL, if any.
    pub url: Option<String>,
    /// Topics attached to the subject.
    pub topics: Vec<String>,
    /// Most recent observed score.
    pub score_now: i64,
    /// Earliest observed score inside the window.
    pub score_prev: i64,
    /// Seconds between the two observations.
    pub window_secs: i64,
    /// Score gained per day over the window (see [`crate::score`]).
    pub velocity_per_day: f64,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn source_kind_roundtrips() {
        for kind in [
            SourceKind::GithubRepo,
            SourceKind::HnStory,
            SourceKind::Crate,
        ] {
            assert_eq!(SourceKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(SourceKind::parse("nope"), None);
    }

    #[test]
    fn tags_are_distinct() {
        let tags = [
            SourceKind::GithubRepo.tag(),
            SourceKind::HnStory.tag(),
            SourceKind::Crate.tag(),
        ];
        assert_eq!(tags.len(), 3);
        assert_ne!(tags[0], tags[1]);
        assert_ne!(tags[1], tags[2]);
    }
}
