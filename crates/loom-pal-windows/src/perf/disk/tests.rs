use std::collections::{BTreeSet, HashMap};
use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use loom_types::DiskMediaType;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Ioctl::DISK_PERFORMANCE;

use super::compute_disk_delta;
use super::safety::{
    CM_NOTIFY_ACTION_DEVICEQUERYREMOVE, CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE,
    CM_NOTIFY_ACTION_DEVICEREMOVEPENDING, HddActivityGuard, SafeEjectGuard, is_safe_eject_action,
    is_storage_controller_driver_allowed, safe_eject_notify_callback,
};

#[test]
fn storage_controller_driver_allowlist_permits_inbox_and_rejects_raid_vmd() {
    assert!(is_storage_controller_driver_allowed("stornvme"));
    assert!(is_storage_controller_driver_allowed("StorNVMe"));
    assert!(is_storage_controller_driver_allowed("storahci"));
    assert!(is_storage_controller_driver_allowed("ATAPI"));
    assert!(is_storage_controller_driver_allowed(" stornvme "));

    assert!(!is_storage_controller_driver_allowed("iaStorVD"));
    assert!(!is_storage_controller_driver_allowed("iaStorAC"));
    assert!(!is_storage_controller_driver_allowed("nvraid"));
    assert!(!is_storage_controller_driver_allowed("megaraid"));
    assert!(!is_storage_controller_driver_allowed(""));
    assert!(!is_storage_controller_driver_allowed("   "));
}

#[test]
fn hdd_activity_guard_skips_idle_hdds_and_allows_non_rotational_media() {
    let mut guard = HddActivityGuard::default();

    // NVMe and SSD are always permitted even with zero I/O changes.
    assert!(guard.should_allow_extra_query(0, DiskMediaType::Nvme, 10, 20));
    assert!(guard.should_allow_extra_query(0, DiskMediaType::Nvme, 10, 20));
    assert!(guard.should_allow_extra_query(1, DiskMediaType::Ssd, 5, 5));

    // Rotational HDD: baseline tick is conservatively skipped; unchanged counters stay skipped.
    assert!(!guard.should_allow_extra_query(2, DiskMediaType::Hdd, 100, 200));
    assert!(!guard.should_allow_extra_query(2, DiskMediaType::Hdd, 100, 200));
    // Activity change permits extra query.
    assert!(guard.should_allow_extra_query(2, DiskMediaType::Hdd, 101, 200));
    assert!(guard.should_allow_extra_query(2, DiskMediaType::Hdd, 101, 205));
    // Returning to idle blocks extra query again.
    assert!(!guard.should_allow_extra_query(2, DiskMediaType::Hdd, 101, 205));

    // Pruning drops removed disks cleanly.
    let live: BTreeSet<u32> = [0, 1].into_iter().collect();
    guard.prune(&live);
    assert!(!guard.should_allow_extra_query(2, DiskMediaType::Hdd, 101, 205));
}

#[test]
fn safe_eject_action_classification_and_callback_update_flag() {
    assert!(is_safe_eject_action(CM_NOTIFY_ACTION_DEVICEQUERYREMOVE));
    assert!(is_safe_eject_action(CM_NOTIFY_ACTION_DEVICEREMOVEPENDING));
    assert!(is_safe_eject_action(CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE));
    assert!(!is_safe_eject_action(0)); // DEVICEINTERFACEARRIVAL
    assert!(!is_safe_eject_action(3)); // DEVICEQUERYREMOVEFAILED

    let flag = Arc::new(AtomicBool::new(false));
    let ptr = (&flag as *const Arc<AtomicBool>).cast::<c_void>();
    // Non-eject action leaves flag false and returns ERROR_SUCCESS.
    let rc1 = unsafe { safe_eject_notify_callback(0, ptr, 0, ptr::null(), 0) };
    assert_eq!(rc1, ERROR_SUCCESS);
    assert!(!flag.load(Ordering::Acquire));

    // DEVICEQUERYREMOVE sets flag true without vetoing (ERROR_SUCCESS).
    let rc2 = unsafe {
        safe_eject_notify_callback(0, ptr, CM_NOTIFY_ACTION_DEVICEQUERYREMOVE, ptr::null(), 0)
    };
    assert_eq!(rc2, ERROR_SUCCESS);
    assert!(flag.load(Ordering::Acquire));

    // Null handle registration fails gracefully.
    assert!(SafeEjectGuard::register(ptr::null_mut()).is_none());
}

#[test]
fn compute_disk_delta_uses_checked_sub_and_handles_counter_resets() {
    let mut prev = HashMap::new();
    let t0 = Instant::now();
    let t1 = t0 + Duration::from_secs(1);
    let t2 = t1 + Duration::from_secs(1);
    let t3 = t2 + Duration::from_secs(1);

    let base = DISK_PERFORMANCE {
        BytesRead: 1_000_000,
        BytesWritten: 500_000,
        ReadTime: 20_000,
        WriteTime: 10_000,
        IdleTime: 5_000_000,
        ReadCount: 100,
        WriteCount: 50,
        QueryTime: 10_000_000,
        ..DISK_PERFORMANCE::default()
    };
    assert_eq!(compute_disk_delta(&mut prev, 0, base, t0), (0, 0, 0, 0));

    // Normal 1-second delta: 400KB read, 200KB write, 60% idle => 40% active (400 permille),
    // 10 ops taking 100_000 * 100ns = 10_000 us => 1_000 us avg response.
    let normal = DISK_PERFORMANCE {
        BytesRead: 1_400_000,
        BytesWritten: 700_000,
        ReadTime: 80_000,
        WriteTime: 50_000,
        IdleTime: 11_000_000,
        ReadCount: 106,
        WriteCount: 54,
        QueryTime: 20_000_000,
        ..DISK_PERFORMANCE::default()
    };
    assert_eq!(
        compute_disk_delta(&mut prev, 0, normal, t1),
        (400, 400_000, 200_000, 1_000)
    );

    // Counter reset (BytesRead/Write and ReadCount/WriteCount drop below previous):
    // must return zeros instead of underflowing or wrapping u32 ReadCount.
    let reset = DISK_PERFORMANCE {
        BytesRead: 10_000,
        BytesWritten: 5_000,
        ReadTime: 1_000,
        WriteTime: 500,
        IdleTime: 1_000_000,
        ReadCount: 2,
        WriteCount: 1,
        QueryTime: 5_000_000,
        ..DISK_PERFORMANCE::default()
    };
    assert_eq!(
        compute_disk_delta(&mut prev, 0, reset, t2),
        (0, 0, 0, 0),
        "counter reset must emit zeros rather than spikes"
    );

    // Next monotonic tick recovers cleanly from the reset baseline.
    let recovered = DISK_PERFORMANCE {
        BytesRead: 110_000,
        BytesWritten: 55_000,
        ReadTime: 11_000,
        WriteTime: 10_500,
        IdleTime: 8_500_000,
        ReadCount: 4,
        WriteCount: 3,
        QueryTime: 15_000_000,
        ..DISK_PERFORMANCE::default()
    };
    assert_eq!(
        compute_disk_delta(&mut prev, 0, recovered, t3),
        (250, 100_000, 50_000, 500)
    );
}
