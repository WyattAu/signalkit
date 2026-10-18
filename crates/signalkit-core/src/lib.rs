//! signalkit — snapshot, store, and score any counter.
//!
//! The engine extracted from trendkit: one subject/observation model, a
//! SQLite store, clamped velocity scoring, and an injectable
//! [`clock_kit::Clock`] so every collector is deterministic under test.
//! Sources implement [`source::TrendSource`]; integrity analysis lives
//! in `signalkit-integrity`.

pub mod clock;
pub mod error;
pub mod model;
pub mod score;
pub mod source;
pub mod store;

pub use error::{Result, TrendError};
pub use model::{Observation, SourceKind, SubjectRecord, VelocityRow};
pub use source::{CollectCtx, CollectReport, TrendSource};
pub use store::Store;
