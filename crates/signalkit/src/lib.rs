//! Umbrella crate for the signalkit estate.
//!
//! Flat re-exports of the engine ([`signalkit_core`]) plus the integrity
//! layer under [`integrity`], so `cargo add signalkit` is enough:
//!
//! ```
//! use signalkit::{Store, SourceKind};
//! use signalkit::integrity::{fork_verdict, ForkVerdict};
//!
//! let store = Store::open_in_memory().expect("memory db");
//! assert_eq!(fork_verdict(1_000, 50, 200), ForkVerdict::Normal);
//! let _ = SourceKind::GithubRepo;
//! ```

pub use signalkit_core::*;

/// Engagement-integrity signals (fork ratios, org bursts, outliers).
pub mod integrity {
    pub use signalkit_integrity::*;
}
