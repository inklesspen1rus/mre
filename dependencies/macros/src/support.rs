pub use std::future::poll_fn;

#[doc(hidden)]
pub fn thread_rng_n(_: u32) -> u32 {
  0
}

#[doc(hidden)]
#[inline]
pub fn poll_budget_available(_: &mut Context<'_>) -> Poll<()> {
  Poll::Ready(())
}

pub use std::future::{Future, IntoFuture};
pub use std::pin::Pin;
pub use std::task::{Context, Poll};
