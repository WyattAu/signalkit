//! Source trait and collection context.

use std::sync::Arc;
use std::time::Duration;

use clock_kit::Clock;

use crate::error::Result;
use crate::store::Store;

/// Everything a [`TrendSource`] needs for one collection pass.
#[derive(Clone)]
pub struct CollectCtx {
    /// Injected time source (SystemClock in production, MockClock in tests).
    pub clock: Arc<dyn Clock + Send + Sync>,
    /// Snapshot store to write into.
    pub store: Arc<Store>,
    /// How many pages to pull from paginated endpoints.
    pub pages: u32,
    /// How far back "recent" reaches, in days.
    pub window_days: u32,
}

impl std::fmt::Debug for CollectCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollectCtx")
            .field("pages", &self.pages)
            .field("window_days", &self.window_days)
            .finish_non_exhaustive()
    }
}

/// Summary of one completed collection pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectReport {
    /// Source name (`"github"`, later `"hn"`, `"cratesio"`).
    pub source: &'static str,
    /// Observations written.
    pub items: usize,
    /// Pages pulled.
    pub pages: usize,
}

/// A named trend data source.
///
/// Object safety is deliberately not a goal: sources are constructed
/// concretely by the CLI and dispatched by an enum there, which keeps the
/// trait compatible with async-fn-in-trait.
pub trait TrendSource {
    /// Stable source identifier used in `collect_runs`.
    fn name(&self) -> &'static str;

    /// Run one collection pass, writing snapshots into `ctx.store`.
    ///
    /// The returned future is deliberately **not** `Send`: collection is
    /// sequential by design (API pacing) and the SQLite connection must
    /// stay confined to the collecting task.
    fn collect(&self, ctx: &CollectCtx)
        -> impl std::future::Future<Output = Result<CollectReport>>;
}

/// Minimum spacing applied between paged requests to a rate-limited
/// endpoint (shared by sources; currently only the GitHub Search API at
/// 30 req/min authenticated needs it).
pub const SEARCH_PACING: Duration = Duration::from_millis(2_200);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn pacing_meets_github_search_budget() {
        // Authenticated search budget is 30 req/min = one per 2s.
        assert!(SEARCH_PACING >= Duration::from_secs(2));
    }
}
