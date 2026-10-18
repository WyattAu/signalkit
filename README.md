# signalkit

Snapshot, store, and score **any counter** — the engine behind
[trendkit](https://github.com/WyattAu/trendkit), extracted as reusable
layers.

| Crate | Role |
|---|---|
| [`signalkit-core`](https://crates.io/crates/signalkit-core) | subject/observation SQLite store, clamped velocity scoring, source trait, injectable time |
| [`signalkit-integrity`](https://crates.io/crates/signalkit-integrity) | fork-ratio verdicts, org-burst detection, robust velocity outliers |

## Design notes

- **Velocity is honest or zero**: `Δscore/day` clamps to a one-hour
  window; a subject with one snapshot reports `0.0` instead of a
  fabricated spike.
- **Time is injected**: every timestamp comes from a
  [`clock_kit::Clock`](https://crates.io/crates/clock-kit), so tests drive
  collection with `MockClock` and `step_wall`.
- **Integrity is evidence-first**: verdicts ship with the numbers that
  produced them; thresholds are conservative and documented.

Apache-2.0.
