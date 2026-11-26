//! Cross-platform, OS-independent, and UI-independent domain models for Loom.
//!
//! `loom-types` sits at the root of the workspace dependency DAG and intentionally
//! avoids any platform FFI, UI toolkit, or async runtime dependencies.

#![warn(missing_docs)]

pub mod control;
pub mod history;
pub mod performance;
pub mod prefs;
pub mod process;
pub mod snapshot;
pub mod time;

pub use control::{CapabilityFlags, RunTaskRequest, TreeKillItem, TreeKillReport, WaitChainNode};
pub use history::{HistoryRing, HistoryRingIter};
pub use performance::{PerformanceSlice, SystemPerfSample};
pub use prefs::{
    DetailsSortColumn, DetailsViewMode, LanguagePreference, ProcessSortColumn, ProcessViewMode,
    RefreshSpeed, ResourceDisplayMode, SortDirection, StartupPage, ThemePreference,
};
pub use process::{
    CpuAffinityMask, ExeIconRgba, ExeStaticAsset, ProcessArch, ProcessCategory, ProcessColdMeta,
    ProcessHotSample, ProcessKey, ProcessPriority, ProcessStatusFlags, UacVirtualization,
};
pub use snapshot::{
    AppHistoryRecord, AppHistorySlice, AppSnapshot, DetailsRowProjection, DetailsTableSlice,
    DomainRevisions, MonitorDomain, ProcessRowKind, ProcessRowProjection, ProcessTableSlice,
    ServiceRecord, ServiceState, ServicesSlice, StartupEntry, StartupImpact, StartupSlice,
    UserSessionRecord, UserSessionState, UsersSlice,
};
pub use time::Timestamp100Ns;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_arithmetic_and_conversions() {
        let t1 = Timestamp100Ns::new(25_000_000); // 2.5s
        let t2 = Timestamp100Ns::new(10_000_000); // 1.0s

        let diff = t1.saturating_sub(t2);
        assert_eq!(diff.as_raw(), 15_000_000);
        assert_eq!(diff.as_millis(), 1_500);
        assert!((diff.as_secs_f64() - 1.5).abs() < 1e-12);

        // Saturates at 0 when subtracting a later timestamp.
        assert_eq!(t2.saturating_sub(t1), Timestamp100Ns(0));
        assert_eq!(Timestamp100Ns(9_999).as_millis(), 0);
        assert_eq!(Timestamp100Ns(10_000).as_millis(), 1);
    }

    #[test]
    fn process_key_ordering_and_parent_validation() {
        let a = ProcessKey::new(100, 1_000);
        let b = ProcessKey::new(100, 2_000);
        let c = ProcessKey::new(200, 1_500);

        // Lexicographical ordering: pid first, then create_time_100ns.
        assert!(a < b);
        assert!(b < c);
        assert_ne!(a, b);

        // Parent validation guards against PID reuse and self-parenting.
        assert!(a.is_valid_parent_of(c));
        assert!(!b.is_valid_parent_of(c)); // parent created AFTER child (reused PID)
        assert!(!a.is_valid_parent_of(a)); // self cannot be own parent
        assert!(!a.is_valid_parent_of(b)); // same PID cannot be parent of recycled PID
    }

    #[test]
    fn history_ring_push_wraparound_and_f32_conversion() {
        let mut ring = HistoryRing::<4>::new();
        assert!(ring.is_empty());
        assert_eq!(ring.len(), 0);
        assert_eq!(ring.latest(), None);
        assert_eq!(ring.iter_ordered().collect::<Vec<_>>(), Vec::<u16>::new());

        ring.push(100);
        ring.push(250);
        assert!(!ring.is_empty());
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.latest(), Some(250));
        assert_eq!(ring.iter_ordered().collect::<Vec<_>>(), vec![100, 250]);

        // Clamp values > 1000 permille and test wraparound past capacity 4.
        ring.push(500);
        ring.push(1250); // clamped to 1000
        assert_eq!(ring.len(), 4);
        assert_eq!(ring.latest(), Some(1000));
        assert_eq!(
            ring.iter_ordered().collect::<Vec<_>>(),
            vec![100, 250, 500, 1000]
        );

        ring.push(750);
        ring.push(0);
        assert_eq!(ring.len(), 4);
        assert_eq!(ring.latest(), Some(0));
        assert_eq!(
            ring.iter_ordered().collect::<Vec<_>>(),
            vec![500, 1000, 750, 0]
        );
        assert_eq!(ring.to_vec_f32(), vec![50.0, 100.0, 75.0, 0.0]);

        // Verify Copy semantics.
        let copy = ring;
        ring.clear();
        assert!(ring.is_empty());
        assert_eq!(copy.len(), 4);
    }

    #[test]
    fn cpu_affinity_mask_supports_more_than_64_cpus() {
        // Test 128 logical CPUs (2 full groups)
        let mask128 = CpuAffinityMask::all(128);
        assert_eq!(mask128.group_count(), 2);
        assert_eq!(mask128.enabled_count(), 128);
        assert!(!mask128.is_empty());
        assert!(mask128.is_enabled(0));
        assert!(mask128.is_enabled(63));
        assert!(mask128.is_enabled(64));
        assert!(mask128.is_enabled(127));
        assert!(!mask128.is_enabled(128));

        // Test 192 + 5 = 197 logical CPUs (4 groups, spills beyond inline SmallVec<[u64; 2]>)
        let mut mask197 = CpuAffinityMask::all(197);
        assert_eq!(mask197.group_count(), 4);
        assert_eq!(mask197.enabled_count(), 197);
        assert!(mask197.is_enabled(196));
        assert!(!mask197.is_enabled(197));

        assert!(mask197.set(64, false));
        assert!(mask197.set(196, false));
        assert!(!mask197.set(197, true)); // out of bounds
        assert!(!mask197.is_enabled(64));
        assert!(!mask197.is_enabled(196));
        assert_eq!(mask197.enabled_count(), 195);

        // Test empty mask and enabling specific high CPUs
        let mut empty = CpuAffinityMask::empty(96);
        assert!(empty.is_empty());
        assert_eq!(empty.enabled_count(), 0);
        assert!(empty.set(0, true));
        assert!(empty.set(70, true));
        assert!(empty.set(95, true));
        assert!(!empty.is_empty());
        assert_eq!(empty.enabled_count(), 3);
        assert!(empty.is_enabled(0));
        assert!(empty.is_enabled(70));
        assert!(empty.is_enabled(95));
        assert!(!empty.is_enabled(64));
    }

    #[test]
    fn process_hot_sample_is_copy_for_zst_ext() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<ProcessHotSample<()>>();
        assert_copy::<HistoryRing<60>>();
        assert_copy::<SystemPerfSample>();
    }
}
