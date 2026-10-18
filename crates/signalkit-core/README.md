# signalkit-core

The engine behind [trendkit](https://github.com/WyattAu/trendkit), extracted
so any counter can be tracked, stored, and scored:

- **Subjects/observations**: one append-only snapshot model
  (`(kind, key)` identity, immutable readings).
- **Velocity**: `Δscore/day` clamped to a one-hour minimum window —
  single-snapshot subjects report zero rather than a fabricated spike.
- **Deterministic time**: timestamps flow through an injected
  [`clock_kit::Clock`] (`MockClock` in tests).
- **Source trait**: async collectors write via a shared `CollectCtx`;
  the future is deliberately `!Send` (sequential pacing by design,
  SQLite confined to the collecting task).

Integrity analysis (fork-ratio verdicts, org-burst detection, velocity
outliers) lives in [`signalkit-integrity`](https://crates.io/crates/signalkit-integrity).
