//! SQLite snapshot store.
//!
//! Schema v2 is source-unified: `subjects` holds identity per
//! `(kind, key)`, `observations` holds immutable readings keyed by
//! `(subject_id, ts)`, and `collect_runs` records each pass. Databases
//! created by schema v1 (milestone ① `repos`/`snapshots`) are migrated
//! by dropping the legacy tables — v1 history is single-day and the
//! schema landed before any real history accumulated; this is tracked in
//! `PRAGMA user_version` and asserted by a test.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{Result, TrendError};
use crate::model::{Observation, RepoListed, SourceKind, SubjectRecord, VelocityRow};
use crate::score;

/// Topics are joined with this separator inside a single TEXT column.
pub const TOPIC_SEP: char = '\u{0}';

/// Current schema version.
pub const SCHEMA_VERSION: i64 = 2;

/// A handle to the trendkit SQLite database.
///
/// The connection sits behind a mutex so a [`Store`] is `Send + Sync`:
/// sources run sequentially today, but later milestones run them
/// concurrently against one store. Individual statements are short; no
/// lock is held across `.await` points.
#[derive(Debug)]
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open (and migrate) the database at `path`.
    ///
    /// # Errors
    /// Fails when the file cannot be opened or the schema cannot migrate.
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path).map_err(TrendError::from)?;
        let store = Store {
            conn: Mutex::new(conn),
        };
        store.configure_and_migrate()?;
        Ok(store)
    }

    /// Open an in-memory database (tests).
    ///
    /// # Errors
    /// Fails when the in-memory database cannot be created or migrated.
    pub fn open_in_memory() -> Result<Store> {
        let conn = Connection::open_in_memory().map_err(TrendError::from)?;
        let store = Store {
            conn: Mutex::new(conn),
        };
        store.configure_and_migrate()?;
        Ok(store)
    }

    /// Adopt an existing connection (tests: legacy-schema migration).
    ///
    /// # Errors
    /// Fails when the schema cannot migrate.
    pub fn from_connection(conn: Connection) -> Result<Store> {
        let store = Store {
            conn: Mutex::new(conn),
        };
        store.configure_and_migrate()?;
        Ok(store)
    }

    /// Lock the connection; poisoning maps to a typed error.
    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| TrendError::Other("store mutex poisoned".to_owned()))
    }

    /// Run `f` with the underlying connection — the escape hatch for
    /// downstream analysts (e.g. `signalkit-integrity`) that need custom
    /// queries without duplicating the schema. The connection is never
    /// exposed across an `.await` point.
    ///
    /// # Errors
    /// Propagates lock poisoning and anything `f` returns.
    pub fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> std::result::Result<T, rusqlite::Error>,
    ) -> Result<T> {
        let conn = self.lock()?;
        f(&conn).map_err(TrendError::from)
    }

    fn configure_and_migrate(&self) -> Result<()> {
        let conn = self.lock()?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(TrendError::from)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(TrendError::from)?;

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(TrendError::from)?;
        if version > SCHEMA_VERSION {
            return Err(TrendError::Config(format!(
                "database schema v{version} is newer than this binary (v{SCHEMA_VERSION})"
            )));
        }
        if version < SCHEMA_VERSION {
            // Schema v1 tables are superseded; drop rather than translate.
            conn.execute_batch(
                "DROP TABLE IF EXISTS snapshots;
                 DROP TABLE IF EXISTS repos;",
            )
            .map_err(TrendError::from)?;
        }

        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS subjects (
                subject_id INTEGER PRIMARY KEY,
                kind       TEXT NOT NULL,
                key        TEXT NOT NULL,
                title      TEXT,
                lang       TEXT,
                url        TEXT,
                topics     TEXT NOT NULL DEFAULT '',
                created_at TEXT,
                UNIQUE(kind, key)
            );
            CREATE TABLE IF NOT EXISTS observations (
                subject_id INTEGER NOT NULL REFERENCES subjects(subject_id),
                ts         INTEGER NOT NULL,
                score      INTEGER NOT NULL,
                extra1     INTEGER NOT NULL DEFAULT 0,
                extra2     INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (subject_id, ts)
            );
            CREATE TABLE IF NOT EXISTS profile_repos (
                full_name   TEXT PRIMARY KEY,
                language    TEXT,
                topics      TEXT NOT NULL DEFAULT '',
                description TEXT,
                is_fork     INTEGER NOT NULL DEFAULT 0,
                pushed_at   TEXT,
                stars       INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS collect_runs (
                run_id  INTEGER PRIMARY KEY,
                ts      INTEGER NOT NULL,
                source  TEXT NOT NULL,
                pages   INTEGER NOT NULL,
                items   INTEGER NOT NULL
            );
            ",
        )
        .map_err(TrendError::from)?;
        if version < SCHEMA_VERSION {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(TrendError::from)?;
        }
        Ok(())
    }

    /// Insert or update a subject identity row; returns its `subject_id`.
    ///
    /// Non-null existing fields win over absent ones (`COALESCE`), so a
    /// sparse source cannot blank out data a richer source contributed.
    ///
    /// # Errors
    /// Propagates SQLite failures.
    pub fn upsert_subject(&self, subject: &SubjectRecord) -> Result<i64> {
        let sep = TOPIC_SEP.to_string();
        let topics = subject.topics.join(&sep);
        let conn = self.lock()?;
        conn.query_row(
            "
            INSERT INTO subjects (kind, key, title, lang, url, topics, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(kind, key) DO UPDATE SET
                title      = COALESCE(excluded.title, subjects.title),
                lang       = COALESCE(excluded.lang, subjects.lang),
                url        = COALESCE(excluded.url, subjects.url),
                topics     = CASE WHEN excluded.topics != '' THEN excluded.topics ELSE subjects.topics END,
                created_at = COALESCE(excluded.created_at, subjects.created_at)
            RETURNING subject_id
            ",
            params![
                subject.kind.as_str(),
                subject.key,
                subject.title,
                subject.lang,
                subject.url,
                topics,
                subject.created_at,
            ],
            |row| row.get(0),
        )
        .map_err(TrendError::from)
    }

    /// Append one observation for `subject_id`.
    ///
    /// # Errors
    /// Propagates SQLite failures (including missing foreign key).
    pub fn insert_observation(&self, subject_id: i64, obs: &Observation) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "
            INSERT OR REPLACE INTO observations (subject_id, ts, score, extra1, extra2)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ",
            params![subject_id, obs.ts, obs.score, obs.extra1, obs.extra2],
        )
        .map_err(TrendError::from)?;
        Ok(())
    }

    /// Record a completed collection pass.
    ///
    /// # Errors
    /// Propagates SQLite failures.
    pub fn record_run(&self, ts: i64, source: &str, pages: i64, items: i64) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO collect_runs (ts, source, pages, items) VALUES (?1, ?2, ?3, ?4)",
            params![ts, source, pages, items],
        )
        .map_err(TrendError::from)?;
        Ok(())
    }

    /// The most recent collection pass, if any.
    ///
    /// # Errors
    /// Propagates SQLite failures.
    pub fn last_run(&self) -> Result<Option<(i64, String, i64)>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT ts, source, items FROM collect_runs ORDER BY run_id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(TrendError::from)
    }

    /// `(subject count, observation count)` for report headers.
    ///
    /// # Errors
    /// Propagates SQLite failures.
    pub fn counts(&self) -> Result<(i64, i64)> {
        let conn = self.lock()?;
        let subjects: i64 = conn
            .query_row("SELECT COUNT(*) FROM subjects", [], |row| row.get(0))
            .map_err(TrendError::from)?;
        let observations: i64 = conn
            .query_row("SELECT COUNT(*) FROM observations", [], |row| row.get(0))
            .map_err(TrendError::from)?;
        Ok((subjects, observations))
    }

    /// Highest-velocity subjects over the last `window_days` days.
    ///
    /// Subjects with a single observation inside the window are included
    /// with zero velocity rather than skipped, so early-stage databases
    /// still show what is being tracked.
    ///
    /// # Errors
    /// Propagates SQLite failures.
    pub fn velocity_rows(&self, window_days: u32, limit: u32) -> Result<Vec<VelocityRow>> {
        let window_secs = i64::from(window_days) * 86_400;
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "
                WITH latest AS (
                    SELECT subject_id, MAX(ts) AS ts FROM observations GROUP BY subject_id
                ),
                earliest AS (
                    SELECT o.subject_id, MIN(o.ts) AS ts
                    FROM observations o
                    JOIN latest l ON l.subject_id = o.subject_id
                    WHERE o.ts >= l.ts - ?1
                    GROUP BY o.subject_id
                )
                SELECT s.kind, s.key, s.title, s.lang, s.url, s.topics,
                       lo.score, eo.score, (lo.ts - eo.ts)
                FROM latest l
                JOIN earliest e ON e.subject_id = l.subject_id
                JOIN observations lo ON lo.subject_id = l.subject_id AND lo.ts = l.ts
                JOIN observations eo ON eo.subject_id = e.subject_id AND eo.ts = e.ts
                JOIN subjects s ON s.subject_id = l.subject_id
                ORDER BY (lo.score - eo.score) * 1.0 / MAX((lo.ts - eo.ts), 3600) DESC
                LIMIT ?2
                ",
            )
            .map_err(TrendError::from)?;

        let rows = stmt
            .query_map(params![window_secs, i64::from(limit)], |row| {
                let kind_raw: String = row.get(0)?;
                let key: String = row.get(1)?;
                let title: Option<String> = row.get(2)?;
                let lang: Option<String> = row.get(3)?;
                let url: Option<String> = row.get(4)?;
                let topics_raw: String = row.get(5)?;
                let score_now: i64 = row.get(6)?;
                let score_prev: i64 = row.get(7)?;
                let window: i64 = row.get(8)?;
                Ok(VelocityRow {
                    kind: SourceKind::parse(&kind_raw).unwrap_or(SourceKind::GithubRepo),
                    key,
                    title,
                    lang,
                    url,
                    topics: split_topics(&topics_raw),
                    score_now,
                    score_prev,
                    window_secs: window,
                    velocity_per_day: score::velocity_per_day(score_prev, score_now, window),
                })
            })
            .map_err(TrendError::from)?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(TrendError::from)?);
        }
        Ok(out)
    }

    /// Subjects observed on at least `min_days` distinct UTC days inside
    /// the window, strongest velocity first — the persistence signal.
    ///
    /// # Errors
    /// Propagates SQLite failures.
    pub fn persistent_subjects(
        &self,
        window_days: u32,
        min_days: i64,
        limit: u32,
    ) -> Result<Vec<crate::model::PersistentRow>> {
        let window_secs = i64::from(window_days) * 86_400;
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "
                WITH window_obs AS (
                    SELECT o.subject_id, o.ts, o.score
                    FROM observations o
                    WHERE o.ts >= (SELECT MAX(ts) FROM observations) - ?1
                ),
                per_subject AS (
                    SELECT subject_id,
                           COUNT(DISTINCT date(ts, 'unixepoch')) AS days,
                           MIN(ts) AS first_ts,
                           MAX(ts) AS last_ts
                    FROM window_obs GROUP BY subject_id
                )
                SELECT s.kind, s.key, p.days,
                       (SELECT score FROM window_obs w
                         WHERE w.subject_id = p.subject_id
                           AND w.ts = p.last_ts),
                       (SELECT score FROM window_obs w
                         WHERE w.subject_id = p.subject_id
                           AND w.ts = p.first_ts),
                       p.last_ts - p.first_ts
                FROM per_subject p
                JOIN subjects s ON s.subject_id = p.subject_id
                WHERE p.days >= ?2
                ORDER BY p.days DESC,
                         (SELECT score FROM window_obs w
                           WHERE w.subject_id = p.subject_id
                             AND w.ts = p.last_ts)
                       - (SELECT score FROM window_obs w
                           WHERE w.subject_id = p.subject_id
                             AND w.ts = p.first_ts) DESC
                LIMIT ?3
                ",
            )?;
            let rows = stmt.query_map(
                rusqlite::params![window_secs, min_days, i64::from(limit)],
                |row| {
                    let kind_raw: String = row.get(0)?;
                    let key: String = row.get(1)?;
                    let days: i64 = row.get(2)?;
                    let now: i64 = row.get(3)?;
                    let before: i64 = row.get(4)?;
                    let span_secs: i64 = row.get(5)?;
                    Ok(crate::model::PersistentRow {
                        kind: SourceKind::parse(&kind_raw).unwrap_or(SourceKind::GithubRepo),
                        key,
                        days,
                        score_now: now,
                        velocity_per_day: score::velocity_per_day(before, now, span_secs),
                    })
                },
            )?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    /// Replace the stored personal profile with `rows`.
    ///
    /// # Errors
    /// Propagates SQLite failures.
    pub fn save_profile_rows(&self, rows: &[RepoListed]) -> Result<()> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM profile_repos", [])
            .map_err(TrendError::from)?;
        for r in rows {
            let topics = r.topics.join(&TOPIC_SEP.to_string());
            conn.execute(
                "
                INSERT INTO profile_repos (full_name, language, topics, description, is_fork, pushed_at, stars)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ",
                params![
                    r.full_name,
                    r.language,
                    topics,
                    r.description,
                    i64::from(r.is_fork),
                    r.pushed_at,
                    r.stars,
                ],
            )
            .map_err(TrendError::from)?;
        }
        Ok(())
    }

    /// The stored personal profile rows, if any.
    ///
    /// # Errors
    /// Propagates SQLite failures.
    pub fn load_profile_rows(&self) -> Result<Vec<RepoListed>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT full_name, language, topics, description, is_fork, pushed_at, stars
                 FROM profile_repos ORDER BY full_name",
            )
            .map_err(TrendError::from)?;
        let rows = stmt
            .query_map([], |row| {
                let full_name: String = row.get(0)?;
                let language: Option<String> = row.get(1)?;
                let topics_raw: String = row.get(2)?;
                let description: Option<String> = row.get(3)?;
                let is_fork: i64 = row.get(4)?;
                let pushed_at: Option<String> = row.get(5)?;
                let stars: i64 = row.get(6)?;
                Ok(RepoListed {
                    full_name,
                    language,
                    topics: split_topics(&topics_raw),
                    description,
                    is_fork: is_fork != 0,
                    pushed_at,
                    stars,
                })
            })
            .map_err(TrendError::from)?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(TrendError::from)?);
        }
        Ok(out)
    }
}

/// Split the internal topic-joined column back into topics.
fn split_topics(raw: &str) -> Vec<String> {
    if raw.is_empty() {
        Vec::new()
    } else {
        raw.split(TOPIC_SEP).map(str::to_owned).collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn subject(kind: SourceKind, key: &str, lang: Option<&str>, topics: &[&str]) -> SubjectRecord {
        SubjectRecord {
            kind,
            key: key.to_owned(),
            title: Some("desc".to_owned()),
            lang: lang.map(str::to_owned),
            url: None,
            topics: topics.iter().map(|t| (*t).to_owned()).collect(),
            created_at: None,
        }
    }

    fn obs(ts: i64, score: i64) -> Observation {
        Observation {
            ts,
            score,
            extra1: 1,
            extra2: 1,
        }
    }

    #[test]
    fn upsert_is_idempotent_on_kind_and_key() {
        let store = Store::open_in_memory().unwrap();
        let id1 = store
            .upsert_subject(&subject(
                SourceKind::GithubRepo,
                "a/b",
                Some("Rust"),
                &["cli"],
            ))
            .unwrap();
        let id2 = store
            .upsert_subject(&subject(
                SourceKind::GithubRepo,
                "a/b",
                Some("Rust"),
                &["cli"],
            ))
            .unwrap();
        assert_eq!(id1, id2);
        // Same key, different kind = different subject.
        let id3 = store
            .upsert_subject(&subject(SourceKind::Crate, "a/b", None, &[]))
            .unwrap();
        assert_ne!(id1, id3);
        let (subjects, observations) = store.counts().unwrap();
        assert_eq!(subjects, 2);
        assert_eq!(observations, 0);
    }

    #[test]
    fn upsert_preserves_richer_fields() {
        let store = Store::open_in_memory().unwrap();
        let rich = SubjectRecord {
            kind: SourceKind::GithubRepo,
            key: "a/b".to_owned(),
            title: Some("full description".to_owned()),
            lang: Some("Rust".to_owned()),
            url: Some("https://github.com/a/b".to_owned()),
            topics: vec!["cli".to_owned()],
            created_at: Some("2026-10-01T00:00:00Z".to_owned()),
        };
        store.upsert_subject(&rich).unwrap();
        let sparse = SubjectRecord {
            kind: SourceKind::GithubRepo,
            key: "a/b".to_owned(),
            title: None,
            lang: None,
            url: None,
            topics: vec![],
            created_at: None,
        };
        store.upsert_subject(&sparse).unwrap();
        let rows = store.velocity_rows(14, 10).unwrap();
        assert_eq!(rows.len(), 0); // no observations yet; identity checked via upsert returning same id
    }

    #[test]
    fn velocity_ranks_fastest_first_across_kinds() {
        let store = Store::open_in_memory().unwrap();
        let slow = store
            .upsert_subject(&subject(SourceKind::GithubRepo, "s/slow", Some("Go"), &[]))
            .unwrap();
        let fast = store
            .upsert_subject(&subject(SourceKind::HnStory, "42", None, &[]))
            .unwrap();
        store.insert_observation(slow, &obs(0, 100)).unwrap();
        store.insert_observation(slow, &obs(86_400, 110)).unwrap(); // +10/day
        store.insert_observation(fast, &obs(0, 100)).unwrap();
        store.insert_observation(fast, &obs(86_400, 170)).unwrap(); // +70/day

        let rows = store.velocity_rows(14, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, SourceKind::HnStory);
        assert_eq!(rows[0].key, "42");
        assert!((rows[0].velocity_per_day - 70.0).abs() < 1e-9);
        assert!((rows[1].velocity_per_day - 10.0).abs() < 1e-9);
    }

    #[test]
    fn single_observation_reports_zero_velocity() {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .upsert_subject(&subject(
                SourceKind::Crate,
                "serde",
                None,
                &["serialization"],
            ))
            .unwrap();
        store.insert_observation(id, &obs(86_400, 5_000)).unwrap();
        let rows = store.velocity_rows(14, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert!((rows[0].velocity_per_day - 0.0).abs() < 1e-9);
        assert_eq!(rows[0].score_now, 5_000);
        assert_eq!(rows[0].score_prev, 5_000);
        assert_eq!(rows[0].topics, vec!["serialization".to_owned()]);
    }

    #[test]
    fn persistence_counts_distinct_days() {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .upsert_subject(&subject(
                SourceKind::GithubRepo,
                "wave/persistent",
                None,
                &[],
            ))
            .unwrap();
        // Day 1: two observations (same UTC day counts once).
        store.insert_observation(id, &obs(0, 100)).unwrap();
        store.insert_observation(id, &obs(3_600, 150)).unwrap();
        // Day 2.
        store.insert_observation(id, &obs(86_400, 400)).unwrap();
        // Day 3.
        store.insert_observation(id, &obs(172_800, 900)).unwrap();

        let rows = store.persistent_subjects(14, 2, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].days, 3);
        // 800 stars over 2 days = 400/day.
        assert!((rows[0].velocity_per_day - 400.0).abs() < 1e-9);
        assert_eq!(rows[0].score_now, 900);

        // min_days=4 filters it out.
        assert!(store.persistent_subjects(14, 4, 10).unwrap().is_empty());
    }

    #[test]
    fn profile_roundtrip_preserves_topics() {
        let store = Store::open_in_memory().unwrap();
        let rows = vec![RepoListed {
            full_name: "WyattAu/crawlkit".to_owned(),
            language: Some("Rust".to_owned()),
            topics: vec!["crawler".to_owned(), "seo".to_owned()],
            description: None,
            is_fork: false,
            pushed_at: None,
            stars: 42,
        }];
        store.save_profile_rows(&rows).unwrap();
        let loaded = store.load_profile_rows().unwrap();
        assert_eq!(loaded, rows);
    }

    #[test]
    fn runs_are_recorded_and_latest_read_back() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.last_run().unwrap().is_none());
        store.record_run(1, "github", 3, 300).unwrap();
        store.record_run(2, "hn", 2, 200).unwrap();
        let Some((ts, source, items)) = store.last_run().unwrap() else {
            panic!("expected a recorded run");
        };
        assert_eq!((ts, source.as_str(), items), (2, "hn", 200));
    }

    #[test]
    fn legacy_v1_schema_is_migrated() {
        // Build a v1 database (repos/snapshots, user_version 0).
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE repos (
                repo_id INTEGER PRIMARY KEY,
                full_name TEXT NOT NULL UNIQUE,
                owner TEXT NOT NULL,
                name TEXT NOT NULL,
                description TEXT,
                language TEXT,
                html_url TEXT,
                created_at TEXT,
                topics TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE snapshots (
                repo_id INTEGER NOT NULL REFERENCES repos(repo_id),
                ts INTEGER NOT NULL,
                stars INTEGER NOT NULL,
                forks INTEGER NOT NULL,
                watchers INTEGER NOT NULL,
                open_issues INTEGER NOT NULL,
                PRIMARY KEY (repo_id, ts)
            );
            INSERT INTO repos (repo_id, full_name, owner, name) VALUES (1, 'old/row', 'old', 'row');
            INSERT INTO snapshots (repo_id, ts, stars, forks, watchers, open_issues)
                VALUES (1, 1, 1, 0, 0, 0);
            ",
        )
        .unwrap();

        let store = Store::from_connection(conn).unwrap();
        // v1 data is gone, v2 tables exist and work.
        let (subjects, observations) = store.counts().unwrap();
        assert_eq!((subjects, observations), (0, 0));
        let id = store
            .upsert_subject(&subject(SourceKind::GithubRepo, "new/row", None, &[]))
            .unwrap();
        store.insert_observation(id, &obs(1, 1)).unwrap();
        let version: i64 = store
            .lock()
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn newer_schema_is_rejected() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        let result = Store::from_connection(conn);
        assert!(result.is_err());
    }
}
