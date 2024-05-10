//! Spawn, join and cancel tasks in runtime agnostic way

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

pub use spawns_core::*;

struct YieldNow {
    yielded: bool,
}

impl YieldNow {
    fn new() -> Self {
        Self { yielded: false }
    }
}

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.yielded {
            return Poll::Ready(());
        }
        self.yielded = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// Yields execution of current task.
pub async fn yield_now() {
    YieldNow::new().await
}

#[cfg(test)]
mod tests {
    use std::future::pending;

    use crate::{select, task};

    #[crate::test(crate = "crate")]
    async fn yield_now() {
        select! {
            _ = task::yield_now() => unreachable!(),
            default => {},
        };
        select! {
            _ = task::yield_now() => {},
            _ = pending() => unreachable!(),
        };
    }
}
