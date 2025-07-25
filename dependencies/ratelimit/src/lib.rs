//! This crate provides a simple implementation of a ratelimiter that can be
//! shared between threads.
//!
//! ```
//! use ratelimit::Ratelimiter;
//! use std::time::Duration;
//!
//! // Constructs a ratelimiter that generates 1 tokens/s with no burst. This
//! // can be used to produce a steady rate of requests. The ratelimiter starts
//! // with no tokens available, which means across application restarts, we
//! // cannot exceed the configured ratelimit.
//! let ratelimiter = Ratelimiter::builder(1, Duration::from_secs(1))
//!     .build()
//!     .unwrap();
//!
//! // Another use case might be admission control, where we start with some
//! // initial budget and replenish it periodically. In this example, our
//! // ratelimiter allows 1000 tokens/hour. For every hour long sliding window,
//! // no more than 1000 tokens can be acquired. But all tokens can be used in
//! // a single burst. Additional calls to `try_wait()` will return an error
//! // until the next token addition.
//! //
//! // This is popular approach with public API ratelimits.
//! let ratelimiter = Ratelimiter::builder(1000, Duration::from_secs(3600))
//!     .max_tokens(1000)
//!     .initial_available(1000)
//!     .build()
//!     .unwrap();
//!
//! // For very high rates, we should avoid using too short of an interval due
//! // to limits of system clock resolution. Instead, it's better to allow some
//! // burst and add multiple tokens per interval. The resulting ratelimiter
//! // here generates 50 million tokens/s and allows no more than 50 tokens to
//! // be acquired in any 1 microsecond long window.
//! let ratelimiter = Ratelimiter::builder(50, Duration::from_micros(1))
//!     .max_tokens(50)
//!     .build()
//!     .unwrap();
//!
//! // constructs a ratelimiter that generates 100 tokens/s with no burst
//! let ratelimiter = Ratelimiter::builder(1, Duration::from_millis(10))
//!     .build()
//!     .unwrap();
//!
//! for _ in 0..10 {
//!     // a simple sleep-wait
//!     if let Err(sleep) = ratelimiter.try_wait() {
//!             std::thread::sleep(sleep);
//!             ratelimiter.try_wait().unwrap();
//!     }
//!     
//!     // do some ratelimited action here    
//! }
//! ```

use core::sync::atomic::{AtomicU64, Ordering};
use parking_lot::RwLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Error, Debug, PartialEq, Eq)]
pub enum Error {
  #[error("available tokens cannot be set higher than max tokens")]
  AvailableTokensTooHigh,
  #[error("max tokens cannot be less than the refill amount")]
  MaxTokensTooLow,
  #[error("refill amount cannot exceed the max tokens")]
  RefillAmountTooHigh,
  #[error("refill interval in nanoseconds exceeds maximum u64")]
  RefillIntervalTooLong,
}

#[derive(Clone, Copy, Debug)]
pub enum Alignment {
  Interval,
  Minute,
  Second,
}

#[derive(Debug)]
pub struct Parameters {
  pub capacity: u64,
  pub refill_amount: u64,
  pub refill_interval: Duration,
  pub alignment: Alignment,
  pub start_instant: Instant,
}

pub struct Ratelimiter {
  available: AtomicU64,
  dropped: AtomicU64,
  parameters: RwLock<Parameters>,
  /// Nanoseconds from `start_instant` to the last refill tick.
  last_tick: AtomicU64,
}

impl Ratelimiter {
  /// Initialize a builder that will construct a `Ratelimiter` that adds the
  /// specified `amount` of tokens to the token bucket after each `interval`
  /// has elapsed.
  ///
  /// Note: In practice, the system clock resolution imposes a lower bound on
  /// the `interval`. To be safe, it is recommended to set the interval to be
  /// no less than 1 microsecond. This also means that the number of tokens
  /// per interval should be > 1 to achieve rates beyond 1 million tokens/s.
  pub fn builder(amount: u64, interval: Duration) -> Builder {
    Builder::new(amount, interval)
  }

  /// Return the current effective rate of the Ratelimiter in tokens/second
  pub fn rate(&self) -> f64 {
    let parameters = self.parameters.read();
    if parameters.refill_interval.as_nanos() == 0 {
      return f64::INFINITY;
    }
    parameters.refill_amount as f64 * 1_000_000_000.0 / parameters.refill_interval.as_nanos() as f64
  }

  /// Return the current interval between refills.
  pub fn refill_interval(&self) -> Duration {
    self.parameters.read().refill_interval
  }

  /// Allows for changing the interval between refills at runtime.
  pub fn set_refill_interval(&self, duration: core::time::Duration) -> Result<(), Error> {
    if duration.as_nanos() > u64::MAX as u128 {
      return Err(Error::RefillIntervalTooLong);
    }
    self.parameters.write().refill_interval = duration;
    Ok(())
  }

  /// Return the current number of tokens to be added on each refill.
  pub fn refill_amount(&self) -> u64 {
    self.parameters.read().refill_amount
  }

  /// Allows for changing the number of tokens to be added on each refill.
  pub fn set_refill_amount(&self, amount: u64) -> Result<(), Error> {
    let mut parameters = self.parameters.write();
    if amount > parameters.capacity {
      Err(Error::RefillAmountTooHigh)
    } else {
      parameters.refill_amount = amount;
      Ok(())
    }
  }

  /// Returns the maximum number of tokens that can be held by the ratelimiter.
  pub fn max_tokens(&self) -> u64 {
    self.parameters.read().capacity
  }

  /// Allows for changing the maximum number of tokens that can be held by the
  /// ratelimiter for immediate use. This effectively sets the burst size. The
  /// configured value must be greater than or equal to the refill amount.
  ///
  /// This function does not reduce the number of currently available tokens,
  /// even if the new capacity is smaller. The available tokens will naturally
  /// decrease as they are used, and subsequent refills will be capped at the
  /// new, lower capacity.
  ///
  /// If the new capacity is greater than the old one, the difference is 
  /// added to the currently available tokens
  pub fn set_max_tokens(&self, amount: u64) -> Result<(), Error> {
    let mut parameters = self.parameters.write();
    if amount < parameters.refill_amount {
      return Err(Error::MaxTokensTooLow);
    }
    let old_capacity = parameters.capacity;
    parameters.capacity = amount;

    // We only want to modify 'available' if the capacity is increasing,
    // which might lead to a proportional increase in available tokens.
    if amount > old_capacity {
      loop {
        let available = self.available();

        // Calculate the proportionally increased amount.
        let new_amount = available.saturating_add(amount - old_capacity);

        // If the calculation somehow results in a lower or equal value, do nothing.
        // This also handles the case where 'available' might be higher than 'old_capacity'
        // due to a previous capacity reduction.
        if new_amount <= available {
          break;
        }

        // Attempt to set the new, higher value.
        if self
          .available
          .compare_exchange(available, new_amount, Ordering::AcqRel, Ordering::Acquire)
          .is_ok()
        {
          break;
        }
      }
    }
    // If amount <= old_capacity, we do nothing to 'available', honoring the documentation.

    Ok(())
  }

  /// Returns the number of tokens currently available.
  pub fn available(&self) -> u64 {
    self.available.load(Ordering::Relaxed)
  }

  /// Sets the number of tokens available to some amount. Returns an error if
  /// the amount exceeds the bucket capacity.
  pub fn set_available(&self, amount: u64) -> Result<(), Error> {
    if amount > self.parameters.read().capacity {
      Err(Error::AvailableTokensTooHigh)
    } else {
      self.available.store(amount, Ordering::Relaxed);
      Ok(())
    }
  }

  /// Returns the time of the next refill as a monotonic `Instant`.
  pub fn next_refill(&self) -> Instant {
    let parameters = self.parameters.read();
    let interval_nanos = parameters.refill_interval.as_nanos() as u64;
    let last_tick_nanos = self.last_tick.load(Ordering::Relaxed);

    let next_refill_nanos_from_start = last_tick_nanos + interval_nanos;

    parameters.start_instant + Duration::from_nanos(next_refill_nanos_from_start)
  }

  /// Returns the number of tokens that have been dropped due to bucket
  /// overflowing.
  pub fn dropped(&self) -> u64 {
    self.dropped.load(Ordering::Relaxed)
  }

  /// Internal function to refill the token bucket. Called as part of `try_wait()`.
  /// This function is now robust against "lost time" and uses the monotonic clock.
  fn maybe_refill(&self) {
    let parameters = self.parameters.read();
    let interval_nanos = parameters.refill_interval.as_nanos() as u64;

    if interval_nanos == 0 {
      // Avoid division by zero if interval is zero.
      return;
    }

    let last = self.last_tick.load(Ordering::Relaxed);
    let now_nanos = parameters.start_instant.elapsed().as_nanos() as u64;

    let ticks_passed = now_nanos.saturating_sub(last) / interval_nanos;

    if ticks_passed == 0 {
      return;
    }

    let new_last_tick = last + ticks_passed * interval_nanos;

    // Attempt to claim the refilling work. If another thread wins, we're done.
    if self
      .last_tick
      .compare_exchange(last, new_last_tick, Ordering::AcqRel, Ordering::Acquire)
      .is_ok()
    {
      let refill = parameters.refill_amount;
      let capacity = parameters.capacity;
      let tokens_to_add = ticks_passed * refill;

      // Atomically add the new tokens, capped at capacity.
      let mut current = self.available.load(Ordering::Relaxed);
      loop {
        let new_available = (current + tokens_to_add).min(capacity);
        match self.available.compare_exchange_weak(
          current,
          new_available,
          Ordering::AcqRel,
          Ordering::Acquire,
        ) {
          Ok(_) => {
            // Success
            if current + tokens_to_add > capacity {
              let dropped = (current + tokens_to_add) - capacity;
              self.dropped.fetch_add(dropped, Ordering::Relaxed);
            }
            break;
          }
          Err(x) => current = x, // Contention, retry with the new value
        }
      }
    }
  }

  /// This is like `try_wait()` but allows acquiring multiple tokens at once.
  /// On success, the specified number of tokens have been acquired. On failure,
  /// a `Duration` hinting at when the next refill would occur is returned.
  ///
  /// # Arguments
  /// * `tokens` - The number of tokens to attempt to acquire
  pub fn try_wait_n(&self, tokens: u64) -> Result<(), std::time::Duration> {
    self.maybe_refill();

    loop {
      let current = self.available.load(Ordering::Relaxed);

      if current >= tokens {
        if self
          .available
          .compare_exchange(
            current,
            current - tokens,
            Ordering::AcqRel,
            Ordering::Acquire,
          )
          .is_ok()
        {
          return Ok(());
        }
      // If CAS fails, loop again.
      } else {
        let now = Instant::now();
        let next = self.next_refill();
        // Return the duration to wait. If next is in the past, return zero.
        return Err(next.duration_since(now));
      }
    }
  }

  /// Non-blocking function to "wait" for a single token. On success, a single
  /// token has been acquired. On failure, a `Duration` hinting at when the
  /// next refill would occur is returned.
  pub fn try_wait(&self) -> Result<(), std::time::Duration> {
    self.try_wait_n(1)
  }

  /// Resynchronizes the ratelimiter clock with an external server timestamp.
  ///
  /// This recalculates the time window phase to align with the server time,
  /// using the currently configured `Alignment` type.
  /// This is crucial to avoid clock drift.
  ///
  /// # Arguments
  /// * `server_ms_since_epoch` - The current server time in milliseconds since the UNIX Epoch.
  pub fn sync_time(&self, server_ms_since_epoch: u64) {
    let mut params = self.parameters.write();

    let in_now = Instant::now();
    let since_unix_epoch = Duration::from_millis(server_ms_since_epoch);

    let time_since_window_start_ns = match params.alignment {
      Alignment::Minute => since_unix_epoch.as_nanos() % Duration::from_secs(60).as_nanos(),
      Alignment::Second => since_unix_epoch.as_nanos() % Duration::from_secs(1).as_nanos(),
      Alignment::Interval => {
        let interval_ns = params.refill_interval.as_nanos();
        if interval_ns == 0 {
          0
        } else {
          since_unix_epoch.as_nanos() % interval_ns
        }
      }
    };

    let time_since_window_start = Duration::from_nanos(time_since_window_start_ns as u64);

    // Update start_instant and last_tick to reflect the new timing
    params.start_instant = in_now - time_since_window_start;
    self
      .last_tick
      .store(time_since_window_start.as_nanos() as u64, Ordering::Relaxed);
  }

  /// Changes the ratelimiter's window alignment type at runtime.
  ///
  /// WARNING: This is a destructive operation for the current window count.
  /// The time window is immediately recalculated and reset based on the new
  /// alignment and the local system clock.
  ///
  /// To align with an external clock, call `sync_time()` after `set_alignment()`.
  ///
  /// # Arguments
  /// * `new_alignment` - The new `Alignment` to use (Minute, Second, or Interval).
  pub fn set_alignment(&self, new_alignment: Alignment) {
    let mut params = self.parameters.write();
    params.alignment = new_alignment;

    // Since the alignment change invalidates the current window phase,
    // we need to resynchronize. We use the local clock as a base.
    let in_now = Instant::now();
    let since_unix_epoch = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("SystemTime before UNIX EPOCH");

    let time_since_window_start_ns = match params.alignment {
      Alignment::Minute => since_unix_epoch.as_nanos() % Duration::from_secs(60).as_nanos(),
      Alignment::Second => since_unix_epoch.as_nanos() % Duration::from_secs(1).as_nanos(),
      Alignment::Interval => {
        let interval_ns = params.refill_interval.as_nanos();
        if interval_ns == 0 {
          0
        } else {
          since_unix_epoch.as_nanos() % interval_ns
        }
      }
    };

    let time_since_window_start = Duration::from_nanos(time_since_window_start_ns as u64);

    // Update start_instant and last_tick to reflect the new alignment
    params.start_instant = in_now - time_since_window_start;
    self
      .last_tick
      .store(time_since_window_start.as_nanos() as u64, Ordering::Relaxed);
  }
}

pub struct Builder {
  initial_available: u64,
  max_tokens: u64,
  refill_amount: u64,
  refill_interval: Duration,
  alignment: Alignment,
  /// Optional server time synchronization info.
  /// Stores (server_ms_at_capture, local_instant_at_capture).
  sync_info: Option<(u64, Instant)>,
}

impl Builder {
  fn new(amount: u64, interval: Duration) -> Self {
    Self {
      initial_available: 0,
      max_tokens: amount, // A sensible default: max_tokens = refill_amount
      refill_amount: amount,
      refill_interval: interval,
      alignment: Alignment::Interval, // default
      sync_info: None,
    }
  }

  pub fn max_tokens(mut self, tokens: u64) -> Self {
    self.max_tokens = tokens;
    self
  }

  pub fn initial_available(mut self, tokens: u64) -> Self {
    self.initial_available = tokens;
    self
  }

  pub fn alignment(mut self, alignment: Alignment) -> Self {
    self.alignment = alignment;
    self
  }

  /// Optionally synchronizes the alignment logic to a remote server's clock.
  ///
  /// # Arguments
  /// * `server_ms_since_epoch` - The server's current time as a u64 representing
  ///   milliseconds since the UNIX epoch. This is often available from API
  ///   response headers (e.g., a `Date` header).
  pub fn sync_time(mut self, server_ms_since_epoch: u64) -> Self {
    // Store the server time and the local monotonic time it was captured at.
    self.sync_info = Some((server_ms_since_epoch, Instant::now()));
    self
  }

  pub fn build(self) -> Result<Ratelimiter, Error> {
    if self.max_tokens < self.refill_amount {
      return Err(Error::MaxTokensTooLow);
    }

    if self.refill_interval.as_nanos() > u64::MAX as u128 {
      return Err(Error::RefillIntervalTooLong);
    }

    if self.initial_available > self.max_tokens {
      return Err(Error::AvailableTokensTooHigh);
    }

    // --- Hybrid Alignment Logic ---

    // 1. Capture local monotonic time and determine the wall-clock time source.
    let in_now = Instant::now();

    let since_unix_epoch = if let Some((server_ms_at_capture, instant_at_capture)) = self.sync_info
    {
      // A. Use the synchronized server time.
      // Calculate how much time has passed locally since we received the server time.
      let elapsed_since_capture = in_now.saturating_duration_since(instant_at_capture);
      // Add that elapsed time to the server time to get the "current" server time.
      let current_server_ms =
        server_ms_at_capture.saturating_add(elapsed_since_capture.as_millis() as u64);
      Duration::from_millis(current_server_ms)
    } else {
      // B. Fallback to local system time.
      SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("SystemTime before UNIX EPOCH")
    };

    // 2. Calculate how long ago the current window started, based on the determined time.
    let time_since_window_start_ns = match self.alignment {
      Alignment::Minute => {
        let ns_in_minute = Duration::from_secs(60).as_nanos();
        since_unix_epoch.as_nanos() % ns_in_minute
      }
      Alignment::Second => {
        let ns_in_second = Duration::from_secs(1).as_nanos();
        since_unix_epoch.as_nanos() % ns_in_second
      }
      Alignment::Interval => {
        let interval_ns = self.refill_interval.as_nanos();
        if interval_ns == 0 {
          0
        } else {
          since_unix_epoch.as_nanos() % interval_ns
        }
      }
    };

    let time_since_window_start = Duration::from_nanos(time_since_window_start_ns as u64);

    // 3. Project the window start time onto the monotonic clock's timeline.
    // This becomes our aligned "monotonic epoch".
    let monotonic_start = in_now - time_since_window_start;

    // The last tick is the duration from our new epoch to the actual start of the first tick.
    let last_tick_nanos = time_since_window_start.as_nanos() as u64;

    let parameters = Parameters {
      capacity: self.max_tokens,
      refill_amount: self.refill_amount,
      refill_interval: self.refill_interval,
      alignment: self.alignment,
      start_instant: monotonic_start,
    };

    Ok(Ratelimiter {
      available: AtomicU64::new(self.initial_available),
      dropped: AtomicU64::new(0),
      parameters: parameters.into(),
      last_tick: AtomicU64::new(last_tick_nanos),
    })
  }
}

#[cfg(test)]
mod tests {
  use crate::*;
  use std::time::{Duration, Instant};

  macro_rules! approx_eq {
    ($value:expr, $target:expr) => {
      let value: f64 = $value;
      let target: f64 = $target;
      assert!(value >= target * 0.999, "{value} >= {}", target * 0.999);
      assert!(value <= target * 1.001, "{value} <= {}", target * 1.001);
    };
  }

  // test that the configured rate and calculated effective rate are close
  #[test]
  pub fn rate() {
    // amount + interval
    let rl = Ratelimiter::builder(4, Duration::from_nanos(333))
      .max_tokens(4)
      .build()
      .unwrap();

    approx_eq!(rl.rate(), 12012012.0);
  }

  // quick test that a ratelimiter yields tokens at the desired rate
  #[test]
  pub fn wait() {
    let rl = Ratelimiter::builder(1, Duration::from_micros(10))
      .build()
      .unwrap();

    let mut count = 0;

    let now = Instant::now();
    let end = now + Duration::from_millis(10);
    while Instant::now() < end {
      if rl.try_wait().is_ok() {
        count += 1;
      }
    }

    assert!(count >= 600);
    assert!(count <= 1400);
  }

  // quick test that an idle ratelimiter doesn't build up excess capacity
  #[test]
  pub fn idle() {
    let rl = Ratelimiter::builder(1, Duration::from_millis(1))
      .initial_available(1)
      .build()
      .unwrap();

    std::thread::sleep(Duration::from_millis(10));
    assert!(rl.next_refill() < clocksource::precise::Instant::now());

    assert!(rl.try_wait().is_ok());
    assert!(rl.try_wait().is_err());
    assert!(rl.dropped() >= 8);
    assert!(rl.next_refill() >= clocksource::precise::Instant::now());

    std::thread::sleep(Duration::from_millis(5));
    assert!(rl.next_refill() < clocksource::precise::Instant::now());
  }

  // quick test that capacity acts as expected
  #[test]
  pub fn capacity() {
    let rl = Ratelimiter::builder(1, Duration::from_millis(10))
      .max_tokens(10)
      .initial_available(0)
      .build()
      .unwrap();

    std::thread::sleep(Duration::from_millis(100));
    assert!(rl.try_wait().is_ok());
    assert!(rl.try_wait().is_ok());
    assert!(rl.try_wait().is_ok());
    assert!(rl.try_wait().is_ok());
    assert!(rl.try_wait().is_ok());
    assert!(rl.try_wait().is_ok());
    assert!(rl.try_wait().is_ok());
    assert!(rl.try_wait().is_ok());
    assert!(rl.try_wait().is_ok());
    assert!(rl.try_wait().is_ok());
    assert!(rl.try_wait().is_err());
  }
}