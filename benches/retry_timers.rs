//! Retries that really sleep, compared on tokio's paused clock so that the measured time is
//! the CPU work (schedule + timer registration) and not the delays themselves.
//!
//! This is a separate target because once a process has paused the clock, tokio's `Instant::now`
//! stays on a slower path for the rest of that process, which would distort `overhead`.
//!
//! `cargo bench --bench retry_timers`

use std::cell::Cell;
use std::time::Duration;

use backon::{ConstantBuilder, Retryable};
use counterpoint::prelude::*;
use criterion::{Criterion, criterion_group, criterion_main};
use tokio::runtime::Builder;

#[derive(Debug, Clone, Copy)]
struct E;

fn retry_timers(c: &mut Criterion) {
    let rt = Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .unwrap();
    let d = Duration::from_millis(1);

    let calls = Cell::new(0u32);
    let flaky = || {
        let n = calls.get();
        calls.set(n + 1);
        async move { if n < 3 { Err(E) } else { Ok(u64::from(n)) } }
    };

    let mut g = c.benchmark_group("retry_3_failures_1ms_paused_clock");
    let backoff = ConstantBuilder::default().with_delay(d).with_max_times(3);
    g.bench_function("backon", |b| {
        b.iter(|| {
            calls.set(0);
            rt.block_on(flaky.retry(backoff))
        })
    });
    let mut op = from_fn(flaky).retry(schedule::spaced(d) & schedule::recurs(3));
    g.bench_function("counterpoint", |b| {
        b.iter(|| {
            calls.set(0);
            rt.block_on(op.run())
        })
    });
    g.finish();
}

criterion_group!(benches, retry_timers);
criterion_main!(benches);
