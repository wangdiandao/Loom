//! Storage controller PnP driver allowlist, HDD spin-up protection, and USB safe-eject
//! `CM_Register_Notification` RAII guard.

use std::collections::{BTreeSet, HashMap};
use std::ffi::c_void;
use std::mem::{size_of, transmute, zeroed};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use loom_types::DiskMediaType;
use windows_sys::Win32::Foundation::{DEVPROPKEY, ERROR_SUCCESS, HANDLE};
use windows_sys::core::GUID;

use crate::handle::{OwnedModule, to_wide_null};

/// Inbox Windows storage controller drivers permitted for optional SMART / temperature IOCTLs.
///
/// Vendor RAID / Intel VMD controllers (such as `iaStorVD`, `iaStorAC`, `nvraid`, `megaraid`)
/// are excluded because vendor pass-through IOCTLs can stall or destabilize firmware queues.
pub(crate) const ALLOWED_STORAGE_CONTROLLER_DRIVERS: &[&str] = &["stornvme", "storahci", "atapi"];

/// `DEVPKEY_Device_Service` (`{a45c254e-df1c-4efd-8020-67d146a850e0}`, pid `6`).
const DEVPKEY_DEVICE_SERVICE: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0xa45c_254e_df1c_4efd_8020_67d1_46a8_50e0),
    pid: 6,
};
/// `CONFIGRET` success value (`CR_SUCCESS`).
const CR_SUCCESS: u32 = 0;
/// `CONFIGRET` buffer-too-small value (`CR_BUFFER_SMALL`).
const CR_BUFFER_SMALL: u32 = 26;
/// `CM_LOCATE_DEVNODE_NORMAL`.
const CM_LOCATE_DEVNODE_NORMAL: u32 = 0;
/// `DEVPROP_TYPE_STRING`.
const DEVPROP_TYPE_STRING: u32 = 18;
/// `CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE`.
const CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE: i32 = 1;
/// `CM_NOTIFY_ACTION_DEVICEQUERYREMOVE`.
pub(crate) const CM_NOTIFY_ACTION_DEVICEQUERYREMOVE: i32 = 2;
/// `CM_NOTIFY_ACTION_DEVICEREMOVEPENDING`.
pub(crate) const CM_NOTIFY_ACTION_DEVICEREMOVEPENDING: i32 = 4;
/// `CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE`.
pub(crate) const CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE: i32 = 5;

type FarProc = unsafe extern "system" fn() -> isize;
type FnCmLocateDevNodeW = unsafe extern "system" fn(
    pdn_dev_inst: *mut u32,
    p_device_id: *const u16,
    ul_flags: u32,
) -> u32;
type FnCmGetParent =
    unsafe extern "system" fn(pdn_dev_inst: *mut u32, dn_dev_inst: u32, ul_flags: u32) -> u32;
type FnCmGetDevNodePropertyW = unsafe extern "system" fn(
    dn_dev_inst: u32,
    property_key: *const DEVPROPKEY,
    property_type: *mut u32,
    property_buffer: *mut u8,
    property_buffer_size: *mut u32,
    ul_flags: u32,
) -> u32;
type FnCmNotifyCallback = unsafe extern "system" fn(
    h_notify: isize,
    context: *const c_void,
    action: i32,
    event_data: *const c_void,
    event_data_size: u32,
) -> u32;
type FnCmRegisterNotification = unsafe extern "system" fn(
    p_filter: *const CmNotifyFilter,
    p_context: *const c_void,
    p_callback: Option<FnCmNotifyCallback>,
    p_notify_context: *mut isize,
) -> u32;
type FnCmUnregisterNotification = unsafe extern "system" fn(notify_context: isize) -> u32;

/// Exact Windows `CM_NOTIFY_FILTER` ABI layout (`cfgmgr32.h`).
#[repr(C)]
#[derive(Clone, Copy)]
struct CmNotifyFilter {
    cb_size: u32,
    flags: u32,
    filter_type: i32,
    reserved: u32,
    u: CmNotifyFilterUnion,
}

#[repr(C)]
#[derive(Clone, Copy)]
union CmNotifyFilterUnion {
    device_handle: HANDLE,
    _instance_id_sizing: [u16; 200],
}

/// Dynamically resolved `cfgmgr32.dll` PnP device property and notification entry points.
struct CfgMgrApi {
    _module: OwnedModule,
    cm_locate_dev_node_w: FnCmLocateDevNodeW,
    cm_get_parent: FnCmGetParent,
    cm_get_dev_node_property_w: FnCmGetDevNodePropertyW,
    cm_register_notification: Option<FnCmRegisterNotification>,
    cm_unregister_notification: Option<FnCmUnregisterNotification>,
}

/// Lazily loads `cfgmgr32.dll` once for process lifetime.
fn cfgmgr_api() -> Option<&'static CfgMgrApi> {
    static API: OnceLock<Option<CfgMgrApi>> = OnceLock::new();
    API.get_or_init(|| {
        let module = OwnedModule::load("cfgmgr32.dll").ok()?;
        let locate = module.proc_raw(b"CM_Locate_DevNodeW\0")?;
        let parent = module.proc_raw(b"CM_Get_Parent\0")?;
        let prop = module.proc_raw(b"CM_Get_DevNode_PropertyW\0")?;
        let reg = module.proc_raw(b"CM_Register_Notification\0");
        let unreg = module.proc_raw(b"CM_Unregister_Notification\0");

        // SAFETY: All symbols match their documented `cfgmgr32.h` prototypes.
        unsafe {
            Some(CfgMgrApi {
                _module: module,
                cm_locate_dev_node_w: transmute::<FarProc, FnCmLocateDevNodeW>(locate),
                cm_get_parent: transmute::<FarProc, FnCmGetParent>(parent),
                cm_get_dev_node_property_w: transmute::<FarProc, FnCmGetDevNodePropertyW>(prop),
                cm_register_notification: reg
                    .map(|p| transmute::<FarProc, FnCmRegisterNotification>(p)),
                cm_unregister_notification: unreg
                    .map(|p| transmute::<FarProc, FnCmUnregisterNotification>(p)),
            })
        }
    })
    .as_ref()
}

/// Returns `true` if `service_name` matches Loom's inbox storage controller allowlist
/// (`stornvme`, `storahci`, `atapi`), rejecting unsafe RAID/VMD controllers (e.g. `iaStorVD`).
pub(crate) fn is_storage_controller_driver_allowed(service_name: &str) -> bool {
    let trimmed = service_name.trim();
    if trimmed.is_empty() {
        return false;
    }
    ALLOWED_STORAGE_CONTROLLER_DRIVERS
        .iter()
        .any(|allowed| trimmed.eq_ignore_ascii_case(allowed))
}

/// Queries `DEVPKEY_Device_Service` for a `DEVINST` via dynamically loaded `cfgmgr32.dll`.
fn query_devnode_service(api: &CfgMgrApi, devinst: u32) -> Option<String> {
    let mut prop_type: u32 = 0;
    let mut needed_bytes: u32 = 0;
    // SAFETY: Null buffer queries required byte length for `DEVPKEY_Device_Service`.
    let rc = unsafe {
        (api.cm_get_dev_node_property_w)(
            devinst,
            &DEVPKEY_DEVICE_SERVICE,
            &mut prop_type,
            ptr::null_mut(),
            &mut needed_bytes,
            0,
        )
    };
    if (rc != CR_BUFFER_SMALL && rc != CR_SUCCESS) || needed_bytes == 0 || needed_bytes > 4096 {
        return None;
    }

    let mut wide_buf = vec![0u16; (needed_bytes as usize).div_ceil(size_of::<u16>())];
    let mut out_bytes = (wide_buf.len() * size_of::<u16>()) as u32;
    // SAFETY: `wide_buf` is sized for at least `needed_bytes`.
    let rc2 = unsafe {
        (api.cm_get_dev_node_property_w)(
            devinst,
            &DEVPKEY_DEVICE_SERVICE,
            &mut prop_type,
            wide_buf.as_mut_ptr().cast::<u8>(),
            &mut out_bytes,
            0,
        )
    };
    if rc2 != CR_SUCCESS || prop_type != DEVPROP_TYPE_STRING {
        return None;
    }
    let code_units = (out_bytes as usize / size_of::<u16>()).min(wide_buf.len());
    let slice = &wide_buf[..code_units];
    let len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    let service = String::from_utf16_lossy(&slice[..len]).trim().to_owned();
    (!service.is_empty()).then_some(service)
}

/// Resolves the parent storage controller's `DEVPKEY_Device_Service` driver name for a disk
/// PnP Device Instance ID using `CM_Locate_DevNodeW` -> `CM_Get_Parent` -> `CM_Get_DevNode_PropertyW`.
#[allow(dead_code)]
pub(crate) fn query_parent_controller_service(disk_dev_instance_id: &str) -> Option<String> {
    if disk_dev_instance_id.trim().is_empty() {
        return None;
    }
    let api = cfgmgr_api()?;
    let wide_id = to_wide_null(disk_dev_instance_id);
    let mut devinst: u32 = 0;
    // SAFETY: `wide_id` is null-terminated UTF-16.
    let rc = unsafe {
        (api.cm_locate_dev_node_w)(&mut devinst, wide_id.as_ptr(), CM_LOCATE_DEVNODE_NORMAL)
    };
    if rc != CR_SUCCESS || devinst == 0 {
        return None;
    }

    let mut parent: u32 = 0;
    // SAFETY: `devinst` was located successfully above.
    let rc_parent = unsafe { (api.cm_get_parent)(&mut parent, devinst, 0) };
    if rc_parent != CR_SUCCESS || parent == 0 {
        return None;
    }
    query_devnode_service(api, parent)
}

/// Returns `true` only when a disk's parent PnP storage controller runs an allowlisted inbox
/// driver (`stornvme`, `storahci`, `atapi`), safely gating optional SMART/temperature IOCTLs.
#[allow(dead_code)]
pub(crate) fn is_disk_controller_smart_safe(disk_dev_instance_id: &str) -> bool {
    query_parent_controller_service(disk_dev_instance_id)
        .is_some_and(|svc| is_storage_controller_driver_allowed(&svc))
}

/// Rotational HDD activity-change guard preventing extra SMART/temperature queries from
/// spinning up idle/parked mechanical hard drives.
#[derive(Debug, Default)]
pub(crate) struct HddActivityGuard {
    last_ops: HashMap<u32, (u32, u32)>,
}

impl HddActivityGuard {
    /// Returns `true` if extra optional queries (such as SMART or temperature) are permitted.
    ///
    /// - Non-rotational media (`Nvme`, `Ssd`, `Unknown`) are always allowed.
    /// - Rotational HDDs (`DiskMediaType::Hdd`) are allowed ONLY when `read_count` or
    ///   `write_count` changed since the previous sample (first baseline sample returns `false`
    ///   so a spun-down HDD is never woken unnecessarily).
    pub(crate) fn should_allow_extra_query(
        &mut self,
        disk_index: u32,
        media_type: DiskMediaType,
        read_count: u32,
        write_count: u32,
    ) -> bool {
        if media_type != DiskMediaType::Hdd {
            return true;
        }
        let current = (read_count, write_count);
        match self.last_ops.insert(disk_index, current) {
            None => false,
            Some(prev) => prev != current,
        }
    }

    /// Prunes entries for removed physical disks.
    pub(crate) fn prune(&mut self, live_indices: &BTreeSet<u32>) {
        self.last_ops.retain(|idx, _| live_indices.contains(idx));
    }
}

/// Returns `true` if a `CM_NOTIFY_ACTION` indicates safe-eject / device removal.
#[inline]
pub(crate) fn is_safe_eject_action(action: i32) -> bool {
    matches!(
        action,
        CM_NOTIFY_ACTION_DEVICEQUERYREMOVE
            | CM_NOTIFY_ACTION_DEVICEREMOVEPENDING
            | CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE
    )
}

/// `CM_Register_Notification` callback that records removal requests without ever vetoing eject.
pub(super) unsafe extern "system" fn safe_eject_notify_callback(
    _h_notify: isize,
    context: *const c_void,
    action: i32,
    _event_data: *const c_void,
    _event_data_size: u32,
) -> u32 {
    if !context.is_null() && is_safe_eject_action(action) {
        // SAFETY: `context` points to a heap-allocated `Arc<AtomicBool>` kept alive until
        // after `CM_Unregister_Notification` finishes waiting for active callbacks.
        let flag = unsafe { &*(context.cast::<Arc<AtomicBool>>()) };
        flag.store(true, Ordering::Release);
    }
    ERROR_SUCCESS
}

/// RAII `CM_Register_Notification` device-handle safe-eject guard.
///
/// Listens for `CM_NOTIFY_ACTION_DEVICEQUERYREMOVE` / `DEVICEREMOVEPENDING` /
/// `DEVICEREMOVECOMPLETE` on an open device handle so callers can promptly close handles and
/// never block Windows USB "Safely Remove Hardware".
#[allow(dead_code)]
pub(crate) struct SafeEjectGuard {
    notify_handle: isize,
    unregister: FnCmUnregisterNotification,
    flag: Arc<AtomicBool>,
    context_ptr: *mut Arc<AtomicBool>,
}

// `SafeEjectGuard` owns a kernel notification handle and an `Arc<AtomicBool>` context.
unsafe impl Send for SafeEjectGuard {}
unsafe impl Sync for SafeEjectGuard {}

#[allow(dead_code)]
impl SafeEjectGuard {
    /// Registers a `CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE` notification for `device_handle`.
    ///
    /// Returns `None` if `cfgmgr32.dll` notifications are unavailable or registration fails.
    pub(crate) fn register(device_handle: HANDLE) -> Option<Self> {
        if device_handle.is_null() {
            return None;
        }
        let api = cfgmgr_api()?;
        let register = api.cm_register_notification?;
        let unregister = api.cm_unregister_notification?;

        let flag = Arc::new(AtomicBool::new(false));
        let context_ptr = Box::into_raw(Box::new(Arc::clone(&flag)));

        let mut filter: CmNotifyFilter = unsafe { zeroed() };
        filter.cb_size = size_of::<CmNotifyFilter>() as u32;
        filter.flags = 0;
        filter.filter_type = CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE;
        filter.reserved = 0;
        filter.u.device_handle = device_handle;

        let mut notify_handle: isize = 0;
        // SAFETY: `filter` matches `CM_NOTIFY_FILTER` for `DEVICEHANDLE`, and `context_ptr`
        // remains valid until after `CM_Unregister_Notification` completes in `Drop`.
        let rc = unsafe {
            register(
                &filter,
                context_ptr.cast::<c_void>(),
                Some(safe_eject_notify_callback),
                &mut notify_handle,
            )
        };
        if rc != CR_SUCCESS || notify_handle == 0 {
            // SAFETY: Registration failed, so no callback can run; reclaim `context_ptr`.
            unsafe {
                drop(Box::from_raw(context_ptr));
            }
            return None;
        }

        Some(Self {
            notify_handle,
            unregister,
            flag,
            context_ptr,
        })
    }

    /// Returns `true` once Windows has signaled a safe-eject / removal event for this handle.
    #[inline]
    pub(crate) fn is_removal_requested(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Returns a cloned atomic flag that callers can poll across threads.
    #[inline]
    pub(crate) fn removal_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.flag)
    }
}

impl Drop for SafeEjectGuard {
    fn drop(&mut self) {
        if self.notify_handle != 0 {
            // SAFETY: `CM_Unregister_Notification` synchronizes with any active callback before
            // returning, guaranteeing `context_ptr` is no longer dereferenced afterwards.
            unsafe {
                let _ = (self.unregister)(self.notify_handle);
            }
            self.notify_handle = 0;
        }
        if !self.context_ptr.is_null() {
            // SAFETY: Reclaims the `Box<Arc<AtomicBool>>` created in `register`.
            unsafe {
                drop(Box::from_raw(self.context_ptr));
            }
            self.context_ptr = ptr::null_mut();
        }
    }
}
