//! Timeouts.

use core::fmt;
use core::time::Duration;

use crate::op::Op;

/// The error produced when a [`Timeout`] fires. Convert it into your error type with `From`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Elapsed;

impl fmt::Display for Elapsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("operation timed out")
    }
}

impl std::error::Error for Elapsed {}

/// See [`Op::timeout`].
#[derive(Clone)]
#[must_use = "ops do nothing until `.run().await`"]
pub struct Timeout<O> {
    op: O,
    limit: Duration,
}

impl<O> Timeout<O> {
    pub(crate) fn new(op: O, limit: Duration) -> Self {
        Self { op, limit }
    }
}

impl<O> Op for Timeout<O>
where
    O: Op,
    O::Err: From<Elapsed>,
{
    type Ok = O::Ok;
    type Err = O::Err;

    async fn run(&mut self) -> Result<O::Ok, O::Err> {
        if self.limit == Duration::MAX {
            // timeout(∞) = id, without even registering a timer.
            return self.op.run().await;
        }
        match tokio::time::timeout(self.limit, self.op.run()).await {
            Ok(r) => r,
            Err(_) => {
                crate::obs::event!("counterpoint.timeout: elapsed after {:?}", self.limit);
                Err(Elapsed.into())
            }
        }
    }
}
