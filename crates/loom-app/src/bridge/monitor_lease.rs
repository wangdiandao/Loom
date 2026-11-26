//! RAII view-scoped wrapper over [`loom_engine::MonitorLease`].

use loom_engine::{EngineCoordinator, MonitorLease};
use loom_pal::NativeBackend;
use loom_types::MonitorDomain;

/// View-scoped RAII guard keeping an on-demand [`MonitorDomain`] active while mounted.
///
/// Dropping this handle automatically decrements the engine's atomic lease counter so
/// unmounted views incur zero background sampling or projection cost.
#[derive(Debug)]
pub(crate) struct ScopedMonitorLease {
    domain: MonitorDomain,
    _lease: MonitorLease,
}

impl ScopedMonitorLease {
    /// Acquires a [`ScopedMonitorLease`] for `domain` from `coordinator`.
    pub(crate) fn acquire(
        coordinator: &EngineCoordinator<NativeBackend>,
        domain: MonitorDomain,
    ) -> Self {
        let lease = coordinator.acquire_lease(domain);
        Self {
            domain,
            _lease: lease,
        }
    }

    /// Returns the [`MonitorDomain`] kept active by this lease.
    #[inline]
    pub(crate) const fn domain(&self) -> MonitorDomain {
        self.domain
    }
}

#[cfg(test)]
mod tests {
    use loom_config::AppSettings;
    use loom_engine::EngineCoordinator;
    use loom_pal::{NativeBackend, PlatformBackend};
    use loom_types::MonitorDomain;

    use super::ScopedMonitorLease;

    #[test]
    fn scoped_monitor_lease_activates_and_releases_domain() {
        let backend = NativeBackend::initialize().expect("initialize native backend");
        let coordinator =
            EngineCoordinator::new(backend, AppSettings::default()).expect("start coordinator");

        assert!(!coordinator.leases().is_active(MonitorDomain::Details));

        {
            let lease = ScopedMonitorLease::acquire(&coordinator, MonitorDomain::Details);
            assert_eq!(lease.domain(), MonitorDomain::Details);
            assert!(coordinator.leases().is_active(MonitorDomain::Details));
        }

        assert!(!coordinator.leases().is_active(MonitorDomain::Details));

        coordinator.shutdown();
    }
}
