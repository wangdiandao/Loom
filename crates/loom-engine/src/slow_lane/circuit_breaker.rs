//! Latency-based [`CircuitBreaker`] isolating slow drives, UNC shares, or targets.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Latency-based circuit breaker isolating slow drives, UNC shares, or targets.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    timeout: Duration,
    cooldown: Duration,
    tripped: HashMap<String, Instant>,
}

impl CircuitBreaker {
    /// Creates a new [`CircuitBreaker`] with the given slow-probe latency threshold
    /// (typically `300 ms`) and cooldown duration (typically `60 s`).
    pub fn new(timeout: Duration, cooldown: Duration) -> Self {
        Self {
            timeout,
            cooldown,
            tripped: HashMap::new(),
        }
    }

    /// Returns the configured latency threshold beyond which probes trip the breaker.
    #[inline]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Returns the configured cooldown duration while a tripped target is bypassed.
    #[inline]
    pub const fn cooldown(&self) -> Duration {
        self.cooldown
    }

    /// Returns `true` if `target` is currently in cooldown and should be bypassed.
    pub fn is_open(&mut self, target: &str, now: Instant) -> bool {
        if let Some(&until) = self.tripped.get(target) {
            if now < until {
                return true;
            }
            self.tripped.remove(target);
        }
        false
    }

    /// Records the observed latency and timeout status of a probe against `target`.
    ///
    /// Returns `true` if the circuit breaker tripped (or was refreshed) for `target`.
    pub fn record_probe(
        &mut self,
        target: &str,
        elapsed: Duration,
        timed_out_error: bool,
        now: Instant,
    ) -> bool {
        if elapsed >= self.timeout || timed_out_error {
            self.tripped.insert(target.to_owned(), now + self.cooldown);
            tracing::warn!(
                target_key = target,
                elapsed_ms = elapsed.as_millis() as u64,
                cooldown_secs = self.cooldown.as_secs(),
                "slow-lane circuit breaker tripped"
            );
            true
        } else {
            false
        }
    }

    /// Derives a coarse volume/share target key from an executable path so one
    /// stalled drive or UNC share trips the breaker for all binaries on that volume.
    pub fn target_key_for_path(path: &str) -> String {
        let trimmed = path.trim();
        let stripped = trimmed
            .strip_prefix(r"\\?\UNC\")
            .or_else(|| trimmed.strip_prefix(r"\\?\"))
            .unwrap_or(trimmed);

        // UNC share: \\server\share\...
        if let Some(rest) = stripped.strip_prefix(r"\\") {
            let mut parts = rest.split(['\\', '/']).filter(|p| !p.is_empty());
            if let (Some(server), Some(share)) = (parts.next(), parts.next()) {
                return format!(
                    r"\\{}\{}",
                    server.to_ascii_lowercase(),
                    share.to_ascii_lowercase()
                );
            }
        }

        // Windows drive letter: C:\...
        let bytes = stripped.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            let drive = (bytes[0] as char).to_ascii_uppercase();
            return format!("{drive}:");
        }

        // Unix / generic fallback: use first two path segments or normalized path.
        if let Some(rest) = stripped.strip_prefix('/') {
            let first = rest.split('/').next().unwrap_or("");
            return format!("/{first}");
        }

        stripped.to_ascii_lowercase()
    }
}
