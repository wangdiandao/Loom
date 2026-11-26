//! Per-target slow-lane circuit breaker protecting Loom against hanging drives and Shell APIs.
//!
//! Any slow-lane probe whose execution time exceeds [`CircuitBreaker::DEFAULT_TIMEOUT`]
//! (`300 ms`) opens the circuit for that target (such as a drive letter or UNC share)
//! for [`CircuitBreaker::DEFAULT_COOLDOWN`] (`60 s`), immediately returning
//! [`PalError::CircuitOpen`] on subsequent checks until the cooldown elapses.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use loom_pal_api::PalError;
use parking_lot::Mutex;

/// Thread-safe, per-target timeout circuit breaker for slow-lane bulkhead operations.
#[derive(Debug)]
pub struct CircuitBreaker {
    timeout: Duration,
    cooldown: Duration,
    open_until: Mutex<HashMap<String, Instant>>,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

impl CircuitBreaker {
    /// Default single-operation latency threshold before tripping the circuit (`300 ms`).
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(300);

    /// Default cooldown window during which a tripped target is bypassed (`60 s`).
    pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(60);

    /// Creates a circuit breaker with the default `300 ms` timeout and `60 s` cooldown.
    pub fn new() -> Self {
        Self::with_config(Self::DEFAULT_TIMEOUT, Self::DEFAULT_COOLDOWN)
    }

    /// Creates a circuit breaker with custom `timeout` and `cooldown` durations.
    pub fn with_config(timeout: Duration, cooldown: Duration) -> Self {
        Self {
            timeout,
            cooldown,
            open_until: Mutex::new(HashMap::new()),
        }
    }

    /// Returns the configured latency threshold.
    #[inline]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Returns the configured cooldown duration.
    #[inline]
    pub fn cooldown(&self) -> Duration {
        self.cooldown
    }

    /// Checks whether `target` is currently allowed to execute using [`Instant::now`].
    ///
    /// Returns `Err(PalError::CircuitOpen)` if the circuit is currently open.
    pub fn check(&self, target: &str) -> Result<(), PalError> {
        self.check_at(target, Instant::now())
    }

    /// Deterministic variant of [`Self::check`] using an explicit monotonic timestamp `now`.
    pub fn check_at(&self, target: &str, now: Instant) -> Result<(), PalError> {
        let mut map = self.open_until.lock();
        if let Some(&until) = map.get(target) {
            if now < until {
                let remaining = until.saturating_duration_since(now);
                let retry_after_ms = (remaining.as_millis() as u64).max(1);
                return Err(PalError::CircuitOpen {
                    target: target.to_owned(),
                    retry_after_ms,
                });
            }
            // Cooldown elapsed: close the circuit and allow the probe to retry.
            map.remove(target);
        }
        Ok(())
    }

    /// Marks a probe for `target` as healthy, clearing any open circuit state.
    pub fn record_success(&self, target: &str) {
        self.open_until.lock().remove(target);
    }

    /// Evaluates `elapsed` against the configured timeout using [`Instant::now`].
    ///
    /// If `elapsed > self.timeout`, trips the circuit for `self.cooldown` and returns `true`.
    pub fn record_failure_or_timeout(&self, target: &str, elapsed: Duration) -> bool {
        self.record_failure_or_timeout_at(target, elapsed, Instant::now())
    }

    /// Deterministic variant of [`Self::record_failure_or_timeout`] using an explicit
    /// monotonic timestamp `now`.
    ///
    /// If `elapsed > self.timeout`, opens the circuit for `target` until `now + self.cooldown`
    /// and returns `true`; otherwise returns `false`.
    pub fn record_failure_or_timeout_at(
        &self,
        target: &str,
        elapsed: Duration,
        now: Instant,
    ) -> bool {
        if elapsed > self.timeout {
            let until = now + self.cooldown;
            self.open_until.lock().insert(target.to_owned(), until);
            true
        } else {
            false
        }
    }

    /// Immediately trips the circuit for `target` using [`Instant::now`].
    pub fn trip(&self, target: &str) {
        self.trip_at(target, Instant::now());
    }

    /// Deterministic variant of [`Self::trip`] using an explicit monotonic timestamp `now`.
    pub fn trip_at(&self, target: &str, now: Instant) {
        self.open_until
            .lock()
            .insert(target.to_owned(), now + self.cooldown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circuit_breaker_trips_blocks_during_cooldown_and_recovers() {
        let cb = CircuitBreaker::new();
        assert_eq!(cb.timeout(), Duration::from_millis(300));
        assert_eq!(cb.cooldown(), Duration::from_secs(60));

        let t0 = Instant::now();

        // Initially closed.
        assert!(cb.check_at("E:", t0).is_ok());

        // Operations within the 300ms threshold do not trip the breaker.
        assert!(!cb.record_failure_or_timeout_at("E:", Duration::from_millis(300), t0));
        assert!(cb.check_at("E:", t0).is_ok());

        // An operation exceeding 300ms trips the breaker for 60 seconds.
        assert!(cb.record_failure_or_timeout_at("E:", Duration::from_millis(301), t0));

        // During cooldown, check returns PalError::CircuitOpen with remaining ms.
        let t_mid = t0 + Duration::from_secs(25);
        match cb.check_at("E:", t_mid) {
            Err(PalError::CircuitOpen {
                target,
                retry_after_ms,
            }) => {
                assert_eq!(target, "E:");
                assert!(
                    (34_000..=35_500).contains(&retry_after_ms),
                    "unexpected retry_after_ms: {retry_after_ms}"
                );
            }
            other => panic!("expected PalError::CircuitOpen, got {other:?}"),
        }

        // Unrelated targets remain unaffected.
        assert!(cb.check_at("C:", t_mid).is_ok());

        // After the 60s cooldown elapses, the target recovers automatically.
        let t_after = t0 + Duration::from_secs(60);
        assert!(cb.check_at("E:", t_after).is_ok());

        // Explicit record_success clears an open breaker immediately.
        cb.trip_at("F:", t_after);
        assert!(matches!(
            cb.check_at("F:", t_after + Duration::from_secs(1)),
            Err(PalError::CircuitOpen { .. })
        ));
        cb.record_success("F:");
        assert!(cb.check_at("F:", t_after + Duration::from_secs(1)).is_ok());
    }
}
