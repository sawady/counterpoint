# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project aims to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Benchmarks against `futures`, `tokio` and `backon` (`benches/`). Those crates and `criterion`
  are development dependencies only.

### Changed

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

[Unreleased]: https://github.com/sawady/counterpoint/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/sawady/counterpoint/releases/tag/v0.1.0
