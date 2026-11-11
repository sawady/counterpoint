# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project aims to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- `race_all` with up to 16 ops no longer goes through `FuturesUnordered`: the ops are polled
  from one pinned slice, in input order. A race of 16 yielding ops now makes one allocation for
  the futures instead of one per op (4 heap allocations per run instead of 21, three of them
  from the leaves) and takes 620 ns instead of 2.36 µs, about 15% less than
  `futures::future::select_all`. Exact ties go to the earliest op, as with iterated `race`.
  Past 16 ops it still uses `FuturesUnordered`. No new `unsafe`: it reuses the pin projection of
  `all`. Laws and cancellation are unchanged, and the tests cover both sides of the switch.

## [0.2.1] - 2026-11-11

### Added

- `Hedge::when(pred)`: errors for which `pred` is false are final, like a success: the hedge
  returns them at once and cancels the other attempts. Without it every error is hedged past,
  as before.
- `Breaker::when(pred)` and the `breaker::CountIf` trait: errors for which `pred` is false do
  not count for the breaker. They neither add to nor reset the consecutive failures, and a
  half-open probe that ends with one gives its slot back. Without it every error counts, as
  before.
- Property tests for both, against extended models of hedging and of the breaker.

### Changed

- `all` (and `limit` and `settled`) with up to 16 ops in flight no longer goes through
  `FuturesUnordered`: the ops are polled from one pinned slice, or from a pool of `limit` slots.
  `all` of 16 ready ops now makes 2 heap allocations per run instead of 19 and runs as fast as
  `try_join_all` (it was nine times slower); with `limit(4)` it is seven times faster than
  `buffered(4)`. Past 16 ops in flight it still uses `FuturesUnordered`. Laws and cancellation
  are unchanged, and the law tests now cover both sides of the switch.

## [0.2.0] - 2026-11-10

### Added

- Benchmarks against `futures`, `tokio` and `backon` (`benches/`). Those crates and `criterion`
  are development dependencies only.
- Schedules with input: `ScheduleFor<In>`, `schedule::recur_while`, `schedule::recur_until`,
  `schedule::from_input` and `Schedule::contramap`. A schedule sees the error when retrying and
  the value when repeating.
- `Op::repeat`: run again after each success according to a schedule.
- `Op::hedge`: staggered copies of an op, driven by a schedule; the first success wins.
- `zip3` to `zip8`: flat parallel products.
- `CircuitBreaker::with_schedule`: the cool-down of each opening comes from a schedule.
- `Clone` for `Succeed`, `Fail` and `Never`.
- Property tests for all of the above, including `tests/breaker_laws.rs`.

### Changed

- **Breaking:** `Schedule::next` moved to the new `ScheduleFor<In>` trait and takes the input
  (`next(&input, step)`). `Schedule` keeps `fresh` and the combinators, so code that only builds
  and combines schedules is unaffected. Implement `ScheduleFor<In>` for `In` generic to port a
  custom schedule, and replace `S: Schedule` or `impl Schedule` with `ScheduleFor<E>` where the
  schedule is passed to `retry`.
- `CircuitBreaker` and `Breaker` gained a schedule type parameter with a default, so
  `CircuitBreaker::new` and the plain type name keep working.
- README: the "zero-cost" claim is replaced by the measured overhead (new Performance section).

## [0.1.0] - 2026-11-04

First release.

### Added

- The `Op` trait: a re-runnable description of a fallible async computation, built with
  `from_fn`, `succeed`, `fail` and `never`.
- Combinators: `map`, `map_err`, `and_then`, `ensure`, `or`, `or_else`, `zip`, `join`, `race`,
  `timeout`, `retry` (with `when` and `on_retry`) and `with_breaker`.
- Collections: `all` (with `limit` for bounded concurrency and `settled` to keep partial
  results) and `race_all`.
- `Schedule`, an algebra of retry schedules: `stop`, `forever`, `recurs`, `spaced`,
  `exponential`, `fibonacci`, `within`, `|`, `&`, `then`, `take`, `cap`, `jitter`,
  `full_jitter` and `seed`.
- `CircuitBreaker`.
- Optional `tracing` feature with debug events for retries, timeouts, fallbacks and breaker
  transitions.
- Property tests for the laws of `Op` and of schedules on tokio's paused clock, behaviour tests
  (cancellation, concurrency limit, breaker, hooks, `Send`) and doctests.
- `examples/dashboard.rs`.
- CI: fmt, clippy, tests on stable and on the MSRV (1.88), and docs.
- `CONTRIBUTING.md` and this changelog.

[Unreleased]: https://github.com/sawady/counterpoint/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/sawady/counterpoint/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/sawady/counterpoint/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/sawady/counterpoint/releases/tag/v0.1.0
