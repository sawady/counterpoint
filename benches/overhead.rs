//! CPU overhead of `counterpoint` against the same orchestration written directly with
//! `futures`, `tokio` and `backon`.
//!
//! The leaves do no real work, so these numbers are the *worst case* for any orchestration
//! layer: all that is measured is the machinery around the leaf. Every sample includes one
//! `Runtime::block_on` on a current-thread runtime (the `baseline` group shows what that costs).
//!
//! `cargo bench --bench overhead`

use std::cell::Cell;
use std::future::ready;
use std::hint::black_box;
use std::time::Duration;

use backon::{ConstantBuilder, Retryable};
use counterpoint::prelude::*;
use criterion::{Criterion, criterion_group, criterion_main};
use futures::future::{self, Either};
use futures::stream::{self, StreamExt};
use tokio::runtime::{Builder, Runtime};

#[derive(Debug, Clone, Copy)]
struct E;

impl From<Elapsed> for E {
    fn from(_: Elapsed) -> Self {
        E
    }
}

const FAR: Duration = Duration::from_secs(3600);

async fn leaf(x: u64) -> Result<u64, E> {
    Ok(black_box(x))
}

/// A leaf that returns `Pending` once, so the waker paths are exercised too.
async fn yielding(x: u64) -> Result<u64, E> {
    tokio::task::yield_now().await;
    Ok(black_box(x))
}

fn runtime() -> Runtime {
    Builder::new_current_thread().enable_time().build().unwrap()
}

fn sequencing(c: &mut Criterion) {
    let rt = runtime();
    let mut g = c.benchmark_group("baseline");
    g.bench_function("leaf_direct", |b| b.iter(|| rt.block_on(leaf(1))));
    let mut op = from_fn(|| leaf(1));
    g.bench_function("leaf_from_fn", |b| b.iter(|| rt.block_on(op.run())));
    g.finish();

    let mut g = c.benchmark_group("chain");
    g.bench_function("async_fn", |b| {
        b.iter(|| {
            rt.block_on(async {
                let x = leaf(1).await? + 1;
                let y = leaf(x).await?;
                Ok::<_, E>(y * 2)
            })
        })
    });
    let mut op = from_fn(|| leaf(1))
        .map(|x| x + 1)
        .and_then(async |x| leaf(x).await)
        .map(|x| x * 2);
    g.bench_function("counterpoint", |b| b.iter(|| rt.block_on(op.run())));
    g.finish();
}

fn parallel(c: &mut Criterion) {
    let rt = runtime();

    let mut g = c.benchmark_group("zip_ready");
    g.bench_function("futures", |b| {
        b.iter(|| rt.block_on(future::try_join(leaf(1), leaf(2))))
    });
    let mut op = from_fn(|| leaf(1)).zip(from_fn(|| leaf(2)));
    g.bench_function("counterpoint", |b| b.iter(|| rt.block_on(op.run())));
    g.finish();

    let mut g = c.benchmark_group("zip_yielding");
    g.bench_function("futures", |b| {
        b.iter(|| rt.block_on(future::try_join(yielding(1), yielding(2))))
    });
    let mut op = from_fn(|| yielding(1)).zip(from_fn(|| yielding(2)));
    g.bench_function("counterpoint", |b| b.iter(|| rt.block_on(op.run())));
    g.finish();

    let mut g = c.benchmark_group("race_yielding");
    g.bench_function("futures", |b| {
        b.iter(|| {
            rt.block_on(async {
                let a = std::pin::pin!(yielding(1));
                let b = std::pin::pin!(yielding(2));
                match future::select(a, b).await {
                    Either::Left((r, _)) | Either::Right((r, _)) => r,
                }
            })
        })
    });
    let mut op = from_fn(|| yielding(1)).race(from_fn(|| yielding(2)));
    g.bench_function("counterpoint", |b| b.iter(|| rt.block_on(op.run())));
    g.finish();

    let mut g = c.benchmark_group("all_16");
    g.bench_function("futures_try_join_all", |b| {
        b.iter(|| rt.block_on(future::try_join_all((0..16).map(leaf))))
    });
    let mut op = all((0..16).map(|i| from_fn(move || leaf(i))));
    g.bench_function("counterpoint", |b| b.iter(|| rt.block_on(op.run())));
    g.finish();

    let mut g = c.benchmark_group("all_16_limit_4");
    g.bench_function("futures_buffered", |b| {
        b.iter(|| {
            rt.block_on(async {
                let out: Vec<_> = stream::iter((0..16).map(leaf)).buffered(4).collect().await;
                out.into_iter().collect::<Result<Vec<_>, E>>()
            })
        })
    });
    let mut op = all((0..16).map(|i| from_fn(move || leaf(i)))).limit(4);
    g.bench_function("counterpoint", |b| b.iter(|| rt.block_on(op.run())));
    g.finish();

    // Every leaf yields once, so all 16 are started before the first one wins.
    let mut g = c.benchmark_group("race_all_16");
    g.bench_function("futures_select_all", |b| {
        b.iter(|| {
            rt.block_on(async {
                future::select_all((0..16).map(|i| Box::pin(yielding(i))))
                    .await
                    .0
            })
        })
    });
    let mut op = race_all((0..16).map(|i| from_fn(move || yielding(i))));
    g.bench_function("counterpoint", |b| b.iter(|| rt.block_on(op.run())));
    g.finish();
}

fn resilience(c: &mut Criterion) {
    let rt = runtime();

    let mut g = c.benchmark_group("timeout");
    g.bench_function("tokio", |b| {
        b.iter(|| rt.block_on(async { tokio::time::timeout(FAR, leaf(1)).await }))
    });
    let mut op = from_fn(|| leaf(1)).timeout(FAR);
    g.bench_function("counterpoint", |b| b.iter(|| rt.block_on(op.run())));
    g.finish();

    let backoff = ConstantBuilder::default()
        .with_delay(Duration::ZERO)
        .with_max_times(3);

    // The op succeeds at once: the cost of having a retry policy on the happy path.
    let mut g = c.benchmark_group("retry_no_failure");
    g.bench_function("backon", |b| {
        b.iter(|| rt.block_on((|| leaf(1)).retry(backoff)))
    });
    let mut op = from_fn(|| leaf(1)).retry(schedule::recurs(3));
    g.bench_function("counterpoint", |b| b.iter(|| rt.block_on(op.run())));
    g.finish();

    // Fails three times, no delays. `backon` always goes through its sleeper, even for a zero
    // delay, while `counterpoint` skips the timer, so it is also shown with a no-op sleeper.
    let calls = Cell::new(0u32);
    let flaky = || {
        let n = calls.get();
        calls.set(n + 1);
        async move { if n < 3 { Err(E) } else { Ok(u64::from(n)) } }
    };
    let mut g = c.benchmark_group("retry_3_failures_zero_delay");
    g.bench_function("backon", |b| {
        b.iter(|| {
            calls.set(0);
            rt.block_on(flaky.retry(backoff))
        })
    });
    g.bench_function("backon_noop_sleeper", |b| {
        b.iter(|| {
            calls.set(0);
            rt.block_on(flaky.retry(backoff).sleep(|_| ready(())))
        })
    });
    let mut op = from_fn(flaky).retry(schedule::recurs(3));
    g.bench_function("counterpoint", |b| {
        b.iter(|| {
            calls.set(0);
            rt.block_on(op.run())
        })
    });
    g.finish();

    // retry(timeout(zip)) on the happy path, as it would be written in an application.
    let mut g = c.benchmark_group("pipeline");
    g.bench_function("backon_tokio_futures", |b| {
        b.iter(|| {
            rt.block_on(
                (|| async {
                    tokio::time::timeout(FAR, future::try_join(leaf(1), leaf(2)))
                        .await
                        .map_err(E::from_elapsed)?
                })
                .retry(backoff),
            )
        })
    });
    let mut op = from_fn(|| leaf(1))
        .zip(from_fn(|| leaf(2)))
        .timeout(FAR)
        .retry(schedule::recurs(3));
    g.bench_function("counterpoint", |b| b.iter(|| rt.block_on(op.run())));
    g.finish();
}

impl E {
    fn from_elapsed(_: tokio::time::error::Elapsed) -> E {
        E
    }
}

criterion_group!(benches, sequencing, parallel, resilience);
criterion_main!(benches);
