//! Error type for trendkit-core.

use thiserror::Error;

/// Fallible result alias used throughout trendkit.
pub type Result<T> = std::result::Result<T, TrendError>;

/// Errors surfaced by trendkit-core.
#[derive(Debug, Error)]
pub enum TrendError {
    /// A SQLite store operation failed.
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    /// An HTTP fetch failed.
    #[error("fetch error: {0}")]
    Fetch(#[from] fetch_kit::FetchError),

    /// A filesystem operation failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A source stayed rate-limited after exhausting the retry budget.
    #[error("rate limited after {attempts} attempts on {endpoint}")]
    RateLimited {
        /// The API endpoint that kept answering 403/429.
        endpoint: String,
        /// How many attempts were made.
        attempts: u32,
    },

    /// Configuration could not be loaded or is contradictory.
    #[error("config error: {0}")]
    Config(String),

    /// Anything that does not fit a typed variant yet.
    #[error("{0}")]
    Other(String),
}
