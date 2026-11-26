//! Lock-free RAII [`MonitorLease`] registry for on-demand monitoring domains.
//!
//! Hidden UI tabs and minimized windows drop their [`MonitorLease`] handles so
//! `loom-engine` skips sampling and projection for unleased domains with zero
//! polling overhead.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crossbeam_channel::Sender;
use loom_types::MonitorDomain;

/// Lock-free reference-count registry tracking active [`MonitorLease`] handles
/// per [`MonitorDomain`].
#[derive(Debug)]
pub struct MonitorLeaseRegistry {
    counts: [AtomicUsize; MonitorDomain::COUNT],
    wakeup: Option<Sender<()>>,
}

impl Default for MonitorLeaseRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl MonitorLeaseRegistry {
    /// Creates an empty registry without a coordinator wakeup channel.
    #[inline]
    pub fn new() -> Self {
        Self {
            counts: std::array::from_fn(|_| AtomicUsize::new(0)),
            wakeup: None,
        }
    }

    /// Creates a registry that signals `wakeup` whenever a domain transitions
    /// between `0` and `1` active leases.
    #[inline]
    pub fn with_wakeup(wakeup: Sender<()>) -> Self {
        Self {
            counts: std::array::from_fn(|_| AtomicUsize::new(0)),
            wakeup: Some(wakeup),
        }
    }

    /// Acquires an RAII [`MonitorLease`] for `domain`.
    ///
    /// When `domain` transitions from `0` to `1` active leases, the registered
    /// coordinator wakeup hook (if any) is notified non-blockingly.
    pub fn acquire(self: &Arc<Self>, domain: MonitorDomain) -> MonitorLease {
        let prev = self.counts[domain.index()].fetch_add(1, Ordering::AcqRel);
        if prev == 0 {
            self.notify_wakeup();
        }
        MonitorLease {
            registry: Arc::clone(self),
            domain,
        }
    }

    /// Returns `true` if at least one active [`MonitorLease`] is held for `domain`.
    #[inline]
    pub fn is_active(&self, domain: MonitorDomain) -> bool {
        self.count(domain) > 0
    }

    /// Returns the number of active [`MonitorLease`] handles currently held for `domain`.
    #[inline]
    pub fn count(&self, domain: MonitorDomain) -> usize {
        self.counts[domain.index()].load(Ordering::Acquire)
    }

    #[inline]
    fn notify_wakeup(&self) {
        if let Some(ref tx) = self.wakeup {
            let _ = tx.try_send(());
        }
    }
}

/// RAII lease keeping a [`MonitorDomain`] active while held.
///
/// Dropping this handle atomically decrements the domain's reference count and
/// notifies the engine coordinator if the last lease for that domain was released.
#[derive(Debug)]
pub struct MonitorLease {
    registry: Arc<MonitorLeaseRegistry>,
    domain: MonitorDomain,
}

impl MonitorLease {
    /// Returns the [`MonitorDomain`] guarded by this lease.
    #[inline]
    pub const fn domain(&self) -> MonitorDomain {
        self.domain
    }
}

impl Drop for MonitorLease {
    fn drop(&mut self) {
        let prev = self.registry.counts[self.domain.index()].fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev > 0, "MonitorLease refcount underflow");
        if prev == 1 {
            self.registry.notify_wakeup();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_tracks_refcounts_and_wakes_on_edges() {
        let (tx, rx) = crossbeam_channel::bounded(4);
        let reg = Arc::new(MonitorLeaseRegistry::with_wakeup(tx));

        assert!(!reg.is_active(MonitorDomain::Details));
        assert_eq!(reg.count(MonitorDomain::Details), 0);

        let lease1 = reg.acquire(MonitorDomain::Details);
        assert!(reg.is_active(MonitorDomain::Details));
        assert_eq!(reg.count(MonitorDomain::Details), 1);
        assert!(rx.try_recv().is_ok(), "0 -> 1 transition should wake");

        let lease2 = reg.acquire(MonitorDomain::Details);
        assert_eq!(reg.count(MonitorDomain::Details), 2);
        assert!(rx.try_recv().is_err(), "1 -> 2 transition should not wake");

        drop(lease1);
        assert!(reg.is_active(MonitorDomain::Details));
        assert_eq!(reg.count(MonitorDomain::Details), 1);
        assert!(rx.try_recv().is_err(), "2 -> 1 transition should not wake");

        drop(lease2);
        assert!(!reg.is_active(MonitorDomain::Details));
        assert_eq!(reg.count(MonitorDomain::Details), 0);
        assert!(rx.try_recv().is_ok(), "1 -> 0 transition should wake");
    }
}
