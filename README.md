# counterpoint

[![CI](https://github.com/sawady/counterpoint/actions/workflows/ci.yml/badge.svg)](https://github.com/sawady/counterpoint/actions/workflows/ci.yml)
[![Sponsor](https://img.shields.io/github/sponsors/sawady?label=Sponsor&logo=GitHub)](https://github.com/sponsors/sawady)

**Lawful combinators for orchestrating async Rust.**

In musical counterpoint, independent voices are combined under strict rules. `counterpoint`
does the same for async computations on [tokio](https://tokio.rs): you describe what should
happen — in sequence, in parallel, as a race, with timeouts, retries, fallbacks and guards —
with a small set of combinators whose behaviour is pinned down by **algebraic laws**, and every
law is checked by property-based tests on tokio's virtual clock.

> Status: early release (0.1). The API may still change between 0.x versions.

```rust
use counterpoint::prelude::*;
use std::time::Duration;

let ms = Duration::from_millis;

let profile = from_fn(|| fetch_profile(user))           // a re-runnable async computation
    .timeout(ms(100))                                     // each attempt: at most 100ms
    .retry(                                               // a retry *schedule* is an algebra:
        schedule::exponential(ms(10)).full_jitter().cap(ms(200))
            & schedule::recurs(5)                         //   & = both must agree (max delay)
            & schedule::within(ms(400)),                  //   | = either may continue (min delay)
    )
    .or(from_fn(|| profile_cache(user)))                  // fallback
    .ensure(|p| !p.name.is_empty(), |_| Error::Invalid);  // guard

let config = from_fn(|| mirror_eu()).race(from_fn(|| mirror_us()));  // first mirror to answer

let recs = all(items.map(|i| from_fn(move || recommend(i))))
    .limit(4)                                             // bounded concurrency
    .settled();                                           // keep partial results

let mut dashboard = profile.zip(config).zip(recs);         // run all three in parallel
let ((profile, config), recs) = dashboard.run().await?;
```

A complete, runnable version lives in [`examples/dashboard.rs`](examples/dashboard.rs)
(`cargo run --example dashboard`).

## Installation

```toml
[dependencies]
counterpoint = "0.1"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "time"] }
```

Or `cargo add counterpoint`. With the optional `tracing` feature, every retry, timeout,
fallback and breaker transition is emitted as a `tracing` debug event.

It needs Rust 1.88 or newer and runs on [tokio](https://tokio.rs).

## Why another one?

Rust already has excellent pieces: `futures` and `futures-concurrency` for joins and races,
`tokio::time` for timeouts, `backon`/`tokio-retry`/`relentless` for retries, `tower` for
service middleware, `failsafe` for circuit breakers. `counterpoint` aims at the gap between
them:

- **One vocabulary** for sequencing, parallelism, racing, timeouts, retries, fallbacks, guards,
  bounded concurrency and circuit breaking, composing freely with each other.
- **Laws, written down and tested.** Each combinator states the equations it satisfies
  (below), and `tests/laws.rs` / `tests/schedule_laws.rs` check them with `proptest` on
  tokio's paused clock — including *timing* laws such as "`zip` takes the max, `race` takes
  the min, `and_then` adds".
- **Retry schedules as an algebra**, in the spirit of ZIO's `Schedule`: `|` and `&` are the
  pointwise `min`/`max` of delays, so schedules form a distributive lattice with `stop()` and
  `forever()` as bounds, and `then` sequences them as a monoid.
- **Cancel-safe, with the overhead measured.** Ops are plain generic structs: no `Box<dyn>`, no
  spawning, no `'static` bound. Dropping a running op cancels everything inside it (losing race
  branches, sibling branches after a failure, in-flight attempts, backoff sleeps, breaker
  probes). The fixed combinators cost about the same as writing the code by hand; `all` and
  retries are measurably not free. See [Performance](#performance).
- **Typed errors.** Timeouts and an open breaker surface as `Elapsed` / `CircuitOpen`, which
  you convert into your own error type with `From`.

## The model

The central abstraction is the `Op` trait: a *re-runnable description* of a fallible async
computation (a Rust `Future` is one-shot, so it cannot be retried):

```rust
pub trait Op {
    type Ok;
    type Err;
    fn run(&mut self) -> impl Future<Output = Result<Self::Ok, Self::Err>>;
    // ... plus combinators as provided methods
}
```

Build ops with `from_fn(|| async { .. })` (any async closure), `succeed(v)`, `fail(e)` and
`never()`, then compose. Nothing runs until you `.run().await`.

## Laws

`≡` means *observationally equivalent*: same result **and** same virtual running time.
"Except on ties" means: unless both sides finish at the exact same instant.

| Combinator | Laws |
|---|---|
| `and_then` | `succeed(a).and_then(f) ≡ f(a)` · `m.and_then(succeed) ≡ m` · associative · durations add |
| `map` | `m.map(id) ≡ m` · `m.map(f).map(g) ≡ m.map(g ∘ f)` |
| `zip` | associative and commutative up to tuple isomorphism · unit `succeed(())` · duration is the max · fails fast |
| `join` | never fails · duration is the max |
| `race` | associative · identity `never()` · commutative except on ties · duration is the min |
| `or` | associative · left identity `fail(e)` · left zero `succeed(a)` · *no* right identity |
| `ensure` | `ensure(true) ≡ id` · `ensure(p).ensure(q) ≡ ensure(p ∧ q)` |
| `timeout` | `timeout(∞) ≡ id` · `timeout(t) ≡ id` when the op is faster · `timeout(a).timeout(b) ≡ timeout(min(a, b))` except on ties |
| `retry` | `retry(stop()) ≡ retry(recurs(0)) ≡ id` · `succeed(a).retry(s) ≡ succeed(a)` · `retry(recurs(n))` runs at most `n + 1` times · nested retries multiply |
| `repeat` | `repeat(stop()) ≡ id` · `fail(e).repeat(s) ≡ fail(e)` · `repeat` is `retry` with success and failure swapped · nested repeats multiply |
| `hedge` | `hedge(stop()) ≡ id` · `succeed(a).hedge(s) ≡ succeed(a)` · a success is never slower than the primary's · with succeeding attempts, one hedge after `d` is a `race` against the second attempt delayed by `d`, except on ties · matches an independent model |
| `zip3`…`zip8` | `zipN` is `zip(N-1)` zipped with one more op and flattened, ties included · duration is the max · fails fast |
| `all` | equals its model (all values in order, or the earliest error) · with `limit(k)` the makespan is greedy list scheduling on `k` slots |
| schedules | `\|`, `&`: associative, commutative, idempotent, absorptive, distributive · `stop()` / `forever()` are the bounds · `then`: associative with unit `stop()` · `cap(a).cap(b) ≡ cap(min(a, b))` · `jitter(0) ≡ id` · all of them also hold for traces of inputs |
| schedules with input | `contramap(id) ≡ id` · `contramap` composes and distributes over `\|`, `&`, `then` · `recur_while(true) ≡ forever()` · `recur_while(false) ≡ stop()` · `recur_until(p) ≡ recur_while(¬p)` · `m.retry(s & recur_while(p)) ≡ m.retry(s).when(p)` |
| circuit breaker | `with_schedule(n, spaced(d)) ≡ new(n, d)` · the `k`-th consecutive opening lasts the `k`-th delay of the schedule · a stopped schedule stays open · matches an independent model |

## Beyond retries

Schedules also drive polling, hedging and the circuit breaker, and they can look at what happened:

```rust
// Retry only what is worth retrying, honouring the delay the server asks for.
let policy = schedule::from_input(|e: &Error, _| match e {
    Error::RateLimited(wait) => Some(*wait),
    Error::Unavailable => Some(ms(50)),
    Error::BadRequest => None,
})
.take(5);
let mut call = from_fn(|| fetch()).retry(policy);

// Poll until the job is done (`repeat` is `retry` for successes).
let mut job = from_fn(|| status(id))
    .repeat(schedule::spaced(ms(500)) & schedule::recur_until(|s: &Status| s.is_done()));

// Tail latency: start a second copy if the first has not answered in 50ms.
let mut read = from_fn(|| replica_read()).hedge(schedule::spaced(ms(50)).take(1));

// Flat products of up to 8 ops of different types.
let (user, cart, flags) = zip3(load_user, load_cart, load_flags).run().await?;

// A breaker that stays open longer while the dependency stays down.
let breaker = CircuitBreaker::with_schedule(5, schedule::exponential(ms(100)).cap(ms(30_000)));
```

## Performance

Benchmarks against the same orchestration written directly with `futures`, `tokio` and `backon`
live in [`benches/`](benches) (`cargo bench`). The leaves do no work, so this is the worst case
for any orchestration layer. Times are the median of two runs (criterion, rustc 1.94.1, 8-core
x86-64 under WSL2) and include about 130 ns of `Runtime::block_on`; differences of a few ns are
within the noise between runs.

| Scenario | Direct | `counterpoint` |
|---|---|---|
| leaf (`from_fn`) | 131 ns | 130 ns |
| `map` + `and_then` chain vs `async fn` | 132 ns | 129 ns |
| `zip`, ready leaves vs `try_join` | 138 ns | 140 ns |
| `zip`, yielding leaves vs `try_join` | 302 ns | 305 ns |
| `race` vs `select` | 286 ns | 290 ns |
| `timeout` vs `tokio::time::timeout` | 171 ns | 174 ns |
| `retry(timeout(zip))`, happy path, vs `backon` + `tokio` + `futures` | 199 ns | 200 ns |
| `retry`, no failure, vs `backon` | 146 ns | 155 ns |
| `retry`, 3 failures without delay, vs `backon` with a no-op sleeper | 145 ns | 213 ns |
| `retry`, 3 failures, 1 ms delays, paused clock, vs `backon` | 1.29 µs | 1.37 µs |
| `all` of 16 vs `try_join_all` | 172 ns | 1.51 µs |
| `all` of 16 with `limit(4)` vs `buffered(4)` | 1.48 µs | 1.36 µs |

Future size and heap allocations per run (`cargo bench --bench footprint`), which do not depend
on the machine:

| Scenario | Direct | `counterpoint` |
|---|---|---|
| `zip` of two yielding leaves vs `try_join` | 96 bytes | 112 bytes |
| `race` of two yielding leaves vs `select` | 104 bytes | 112 bytes |
| `timeout` vs `tokio::time::timeout` | 136 bytes | 152 bytes |
| `retry` vs `backon` | 192 bytes | 176 bytes |
| allocations, `zip` / `race` / `timeout` / `retry` | same as direct | same as direct |
| allocations, `all` of 16 vs `try_join_all` | 2 | 19 |
| allocations, `all` of 16 with `limit(4)` vs `buffered(4)` | 21 | 19 |

What this says:

- `zip`, `race`, `timeout`, sequencing and a typical `retry(timeout(zip))` happy path have no
  overhead that can be told apart from noise, and no extra allocations. Futures are up to 17%
  larger.
- `retry` is not free: about 10 ns more than `backon` when nothing fails (it reads the clock once
  per run), and about 22 ns more per failed attempt with no delay (it reads the clock again to
  fill the `Step`). With a real timer, the difference is about 6%. In exchange it skips the timer
  for zero delays: `backon` with its default sleeper took 3.3 ms in the zero-delay scenario,
  because each `sleep(0)` waits for a tick of the timer.
- `all` is **not** zero-cost: it always goes through `FuturesUnordered`, with one allocation per
  op. It is about nine times slower than `try_join_all` on 16 ready leaves, which is a fair price
  only when the ops do real work. With `limit` it is on par with `buffered`.

## Related work

- [`futures`](https://crates.io/crates/futures) and
  [`futures-concurrency`](https://crates.io/crates/futures-concurrency): joins and races over
  futures, runtime-agnostic.
- [`backon`](https://crates.io/crates/backon), [`tokio-retry`](https://crates.io/crates/tokio-retry),
  [`relentless`](https://crates.io/crates/relentless): retries; `relentless` composes wait and stop
  strategies with operators.
- [`tower`](https://crates.io/crates/tower): retry/timeout/limit middleware for `Service`s.
- [`failsafe`](https://crates.io/crates/failsafe): circuit breaker.
- [`asupersync`](https://crates.io/crates/asupersync): a cancel-correct runtime whose combinators
  also come with stated laws.
- ZIO's `Schedule` and cats-effect's `IO` (Scala): the inspiration for re-runnable descriptions and
  for retry policies as an algebra.

## Feature flags

- `tracing` — emit `tracing` debug events for retries, timeouts, fallbacks and breaker
  transitions.

## Minimum supported Rust version

1.88 (async closures and let-chains, edition 2024).

## Contributing

Issues are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md). Changes are listed in
[CHANGELOG.md](CHANGELOG.md).

## Sponsoring

counterpoint is free software. If it is useful to you, you can support its development through
[GitHub Sponsors](https://github.com/sponsors/sawady).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.
