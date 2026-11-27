//! Windows fast-lane GPU adapter & per-process telemetry via dynamic `dxgi.dll`
//! (`CreateDXGIFactory1`), `gdi32.dll` (`D3DKMTQueryStatistics` / `D3DKMTQueryAdapterInfo`),
//! and `cfgmgr32.dll` (`DEVPKEY_Device_PowerData`), free of WMI and PDH.
//!
//! Gracefully handles headless CI VMs and systems without hardware GPUs without panicking
//! or failing fast-lane performance ticks, and power-gates `KMTQAITYPE_ADAPTERPERFDATA` so
//! sleeping hybrid discrete GPUs in `D1`–`D3` are never woken up.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::c_void;
use std::mem::{size_of, transmute, zeroed};
use std::ptr;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use loom_types::{GpuAdapterSample, GpuEngineSample, ProcessHotSample, ProcessKey};
use windows_sys::Wdk::Graphics::Direct3D::{
    D3DKMT_ADAPTER_PERFDATA, D3DKMT_ADAPTERADDRESS, D3DKMT_ADAPTERTYPE, D3DKMT_CLOSEADAPTER,
    D3DKMT_NODEMETADATA, D3DKMT_OPENADAPTERFROMLUID, D3DKMT_QUERYADAPTERINFO,
    D3DKMT_QUERYSTATISTICS, D3DKMT_QUERYSTATISTICS_ADAPTER, D3DKMT_QUERYSTATISTICS_NODE,
    D3DKMT_QUERYSTATISTICS_PROCESS, D3DKMT_QUERYSTATISTICS_PROCESS_ADAPTER,
    D3DKMT_QUERYSTATISTICS_PROCESS_NODE, D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT,
    D3DKMT_QUERYSTATISTICS_SEGMENT, DXGK_ENGINE_TYPE_3D, DXGK_ENGINE_TYPE_COPY,
    DXGK_ENGINE_TYPE_CRYPTO, DXGK_ENGINE_TYPE_OVERLAY, DXGK_ENGINE_TYPE_SCENE_ASSEMBLY,
    DXGK_ENGINE_TYPE_VIDEO_DECODE, DXGK_ENGINE_TYPE_VIDEO_ENCODE,
    DXGK_ENGINE_TYPE_VIDEO_PROCESSING, KMTQAITYPE_ADAPTERADDRESS, KMTQAITYPE_ADAPTERPERFDATA,
    KMTQAITYPE_ADAPTERTYPE, KMTQAITYPE_DRIVERVERSION, KMTQAITYPE_NODEMETADATA,
};
use windows_sys::Win32::Foundation::{
    DEVPROPKEY, ERROR_SUCCESS, FALSE, FreeLibrary, HMODULE, LUID, STATUS_SUCCESS,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Power::{
    CM_POWER_DATA, PowerDeviceD0, PowerDeviceD1, PowerDeviceD2, PowerDeviceD3,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegOpenKeyExW,
};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_sys::core::GUID;

use crate::handle::{OwnedHandle, query_process_sequence_number, to_wide_null};
use crate::perf::cpu::read_reg_string;

/// Refresh cached DXGI adapter inventory every 30 seconds to detect hot-plugged eGPUs.
const ADAPTER_CACHE_TTL: Duration = Duration::from_secs(30);
/// Throttle interval for `KMTQAITYPE_ADAPTERPERFDATA` when a hybrid discrete GPU is in idle `D0`
/// so runtime power management can transition back to `D3`.
pub(crate) const HYBRID_IDLE_PERFDATA_INTERVAL: Duration = Duration::from_secs(20);
/// Maximum D3DKMT segments/nodes iterated per adapter to defend against malformed driver reports.
const MAX_D3DKMT_ITEMS: u32 = 128;
/// `DXGI_ADAPTER_FLAG_SOFTWARE` bit indicating Microsoft Basic Render Driver (WARP).
const DXGI_ADAPTER_FLAG_SOFTWARE: u32 = 2;

/// `D3DKMT_ADAPTERTYPE` bit 0: `RenderSupported`.
const ADAPTER_TYPE_RENDER_SUPPORTED: u32 = 1 << 0;
/// `D3DKMT_ADAPTERTYPE` bit 2: `SoftwareDevice`.
const ADAPTER_TYPE_SOFTWARE_DEVICE: u32 = 1 << 2;
/// `D3DKMT_ADAPTERTYPE` bit 4: `HybridDiscrete` (switchable/Optimus laptop dGPU).
const ADAPTER_TYPE_HYBRID_DISCRETE: u32 = 1 << 4;
/// `D3DKMT_ADAPTERTYPE` bit 12: `IndirectDisplayDevice` (virtual/remote display mirrors).
const ADAPTER_TYPE_INDIRECT_DISPLAY: u32 = 1 << 12;

/// `GUID_DISPLAY_DEVICE_ARRIVAL` (`{1ca05180-a699-450a-9a0c-de4fbe3ddd89}`).
const GUID_DISPLAY_DEVICE_ARRIVAL: GUID =
    GUID::from_u128(0x1ca0_5180_a699_450a_9a0c_de4f_be3d_dd89);
/// `DEVPKEY_Device_InstanceId` (`{78c34fc8-104a-4aca-9ea4-524d52996e57}`, pid `256`).
const DEVPKEY_DEVICE_INSTANCE_ID: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x78c3_4fc8_104a_4aca_9ea4_524d_5299_6e57),
    pid: 256,
};
/// `DEVPKEY_Device_BusNumber` (`{a45c254e-df1c-4efd-8020-67d146a850e0}`, pid `23`).
const DEVPKEY_DEVICE_BUS_NUMBER: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0xa45c_254e_df1c_4efd_8020_67d1_46a8_50e0),
    pid: 23,
};
/// `DEVPKEY_Device_Address` (`{a45c254e-df1c-4efd-8020-67d146a850e0}`, pid `30`).
const DEVPKEY_DEVICE_ADDRESS: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0xa45c_254e_df1c_4efd_8020_67d1_46a8_50e0),
    pid: 30,
};
/// `DEVPKEY_Device_PowerData` (`{a45c254e-df1c-4efd-8020-67d146a850e0}`, pid `32`).
const DEVPKEY_DEVICE_POWER_DATA: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0xa45c_254e_df1c_4efd_8020_67d1_46a8_50e0),
    pid: 32,
};
/// `CR_SUCCESS`.
const CR_SUCCESS: u32 = 0;
/// `CR_BUFFER_SMALL`.
const CR_BUFFER_SMALL: u32 = 26;
/// `CM_LOCATE_DEVNODE_NORMAL`.
const CM_LOCATE_DEVNODE_NORMAL: u32 = 0;
/// `CM_GET_DEVICE_INTERFACE_LIST_PRESENT`.
const CM_GET_DEVICE_INTERFACE_LIST_PRESENT: u32 = 0;
/// `DEVPROP_TYPE_UINT32`.
const DEVPROP_TYPE_UINT32: u32 = 7;
/// `DEVPROP_TYPE_STRING`.
const DEVPROP_TYPE_STRING: u32 = 18;
/// `DEVPROP_TYPE_BINARY` (`DEVPROP_TYPE_BYTE | DEVPROP_TYPEMOD_ARRAY = 0x1003`).
const DEVPROP_TYPE_BINARY: u32 = 0x1003;

/// `IID_IDXGIFactory1` (`{770aae78-f26f-4dba-a829-253c83d1b387}`).
const IID_IDXGIFACTORY1: GUID = GUID {
    data1: 0x770a_ae78,
    data2: 0xf26f,
    data3: 0x4dba,
    data4: [0xa8, 0x29, 0x25, 0x3c, 0x83, 0xd1, 0xb3, 0x87],
};

type FarProc = unsafe extern "system" fn() -> isize;
type FnCreateDxgiFactory1 =
    unsafe extern "system" fn(riid: *const GUID, pp_factory: *mut *mut c_void) -> i32;
type FnD3dkmtQueryStatistics =
    unsafe extern "system" fn(param0: *const D3DKMT_QUERYSTATISTICS) -> i32;
type FnD3dkmtQueryAdapterInfo =
    unsafe extern "system" fn(param0: *mut D3DKMT_QUERYADAPTERINFO) -> i32;
type FnD3dkmtOpenAdapterFromLuid =
    unsafe extern "system" fn(param0: *mut D3DKMT_OPENADAPTERFROMLUID) -> i32;
type FnD3dkmtCloseAdapter = unsafe extern "system" fn(param0: *const D3DKMT_CLOSEADAPTER) -> i32;

/// Exact Windows `DXGI_ADAPTER_DESC1` ABI layout.
#[repr(C)]
#[derive(Clone, Copy)]
struct DxgiAdapterDesc1 {
    description: [u16; 128],
    vendor_id: u32,
    device_id: u32,
    sub_sys_id: u32,
    revision: u32,
    dedicated_video_memory: usize,
    dedicated_system_memory: usize,
    shared_system_memory: usize,
    adapter_luid: LUID,
    flags: u32,
}

impl Default for DxgiAdapterDesc1 {
    fn default() -> Self {
        // SAFETY: `DxgiAdapterDesc1` is POD.
        unsafe { zeroed() }
    }
}

/// RAII wrapper for an `IUnknown`-derived COM pointer calling `Release()` on drop.
struct ComPtr(*mut c_void);

impl ComPtr {
    #[inline]
    fn as_ptr(&self) -> *mut c_void {
        self.0
    }

    /// Reads the `slot`-th raw function pointer from the COM object's vtable.
    #[inline]
    unsafe fn vtable_entry(&self, slot: usize) -> *const c_void {
        // SAFETY: Valid COM objects begin with a pointer to their vtable pointer array.
        let vtable = unsafe { *(self.0.cast::<*const *const c_void>()) };
        unsafe { *vtable.add(slot) }
    }
}

impl Drop for ComPtr {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: Vtable slot 2 is `IUnknown::Release`.
            unsafe {
                let release_ptr = self.vtable_entry(2);
                let release: unsafe extern "system" fn(*mut c_void) -> u32 = transmute(release_ptr);
                release(self.0);
            }
        }
    }
}

/// RAII guard for a DLL explicitly loaded with `LoadLibraryW`.
struct LoadedDll(HMODULE);

// Win32 HMODULE values are process-global immutable addresses safe to send across threads.
unsafe impl Send for LoadedDll {}

impl Drop for LoadedDll {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: Paired with `LoadLibraryW` in `GpuApi::load`.
            unsafe {
                FreeLibrary(self.0);
            }
        }
    }
}

/// Dynamically resolved `dxgi.dll` and `gdi32.dll` D3DKMT entry points.
struct GpuApi {
    _dxgi_dll: LoadedDll,
    create_dxgi_factory1: FnCreateDxgiFactory1,
    d3dkmt_query_statistics: FnD3dkmtQueryStatistics,
    d3dkmt_query_adapter_info: Option<FnD3dkmtQueryAdapterInfo>,
    d3dkmt_open_adapter_from_luid: Option<FnD3dkmtOpenAdapterFromLuid>,
    d3dkmt_close_adapter: Option<FnD3dkmtCloseAdapter>,
}

// Function pointers resolved from system DLLs are safe to move to the fast sampling thread.
unsafe impl Send for GpuApi {}

impl GpuApi {
    fn load() -> Option<Self> {
        let gdi_name = to_wide_null("gdi32.dll");
        // SAFETY: `gdi32.dll` is a core system DLL; `GetModuleHandleW` does not increment refcount.
        let gdi = unsafe { GetModuleHandleW(gdi_name.as_ptr()) };
        if gdi.is_null() {
            return None;
        }

        let query_stats_ptr =
            unsafe { GetProcAddress(gdi, c"D3DKMTQueryStatistics".as_ptr().cast()) }?;
        let query_info_ptr =
            unsafe { GetProcAddress(gdi, c"D3DKMTQueryAdapterInfo".as_ptr().cast()) };
        let open_luid_ptr =
            unsafe { GetProcAddress(gdi, c"D3DKMTOpenAdapterFromLuid".as_ptr().cast()) };
        let close_ptr = unsafe { GetProcAddress(gdi, c"D3DKMTCloseAdapter".as_ptr().cast()) };

        let dxgi_name = to_wide_null("dxgi.dll");
        // SAFETY: Dynamically loads `dxgi.dll` with RAII `FreeLibrary` guard.
        let dxgi_mod = unsafe { LoadLibraryW(dxgi_name.as_ptr()) };
        if dxgi_mod.is_null() {
            return None;
        }
        let dxgi_dll = LoadedDll(dxgi_mod);

        let create_factory_ptr =
            unsafe { GetProcAddress(dxgi_dll.0, c"CreateDXGIFactory1".as_ptr().cast()) }?;

        // SAFETY: All symbol names match their exact Windows SDK `extern "system"` prototypes.
        unsafe {
            Some(Self {
                _dxgi_dll: dxgi_dll,
                create_dxgi_factory1: transmute::<FarProc, FnCreateDxgiFactory1>(
                    create_factory_ptr,
                ),
                d3dkmt_query_statistics: transmute::<FarProc, FnD3dkmtQueryStatistics>(
                    query_stats_ptr,
                ),
                d3dkmt_query_adapter_info: query_info_ptr
                    .map(|p| transmute::<FarProc, FnD3dkmtQueryAdapterInfo>(p)),
                d3dkmt_open_adapter_from_luid: open_luid_ptr
                    .map(|p| transmute::<FarProc, FnD3dkmtOpenAdapterFromLuid>(p)),
                d3dkmt_close_adapter: close_ptr
                    .map(|p| transmute::<FarProc, FnD3dkmtCloseAdapter>(p)),
            })
        }
    }

    fn open_adapter_handle(&self, luid: LUID) -> Option<u32> {
        let open = self.d3dkmt_open_adapter_from_luid?;
        let mut arg = D3DKMT_OPENADAPTERFROMLUID {
            AdapterLuid: luid,
            hAdapter: 0,
        };
        // SAFETY: Valid `D3DKMT_OPENADAPTERFROMLUID` struct passed to `D3DKMTOpenAdapterFromLuid`.
        let status = unsafe { open(&mut arg) };
        if status == STATUS_SUCCESS && arg.hAdapter != 0 {
            Some(arg.hAdapter)
        } else {
            None
        }
    }

    fn close_adapter_handle(&self, handle: u32) {
        if handle != 0
            && let Some(close) = self.d3dkmt_close_adapter
        {
            let arg = D3DKMT_CLOSEADAPTER { hAdapter: handle };
            // SAFETY: Paired with a successful `open_adapter_handle`.
            unsafe {
                let _ = close(&arg);
            }
        }
    }
}

/// Device power state reported by `DEVPKEY_Device_PowerData` (`CM_POWER_DATA`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GpuDevicePowerState {
    /// Power state is unavailable or non-PnP.
    Unknown,
    /// Device is in active working state (`PowerDeviceD0`).
    D0,
    /// Device is in low-power sleep (`PowerDeviceD1`, `PowerDeviceD2`, or `PowerDeviceD3`).
    LowPowerD1ToD3,
}

/// Decision produced by [`GpuPowerGate`] before calling `KMTQAITYPE_ADAPTERPERFDATA`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PerfDataGateDecision {
    /// Safe to query `KMTQAITYPE_ADAPTERPERFDATA` immediately.
    Allow,
    /// Hybrid discrete GPU is sleeping (`D1`–`D3`); skip query and return `None` temperature.
    SkipSleeping,
    /// Hybrid discrete GPU is in idle `D0`; throttle query until 20s have elapsed.
    SkipIdleThrottled,
}

/// Power-aware gate protecting hybrid discrete GPUs from wake-ups caused by
/// `D3DKMTQueryAdapterInfo(KMTQAITYPE_ADAPTERPERFDATA)`.
#[derive(Debug, Clone)]
pub(crate) struct GpuPowerGate {
    is_hybrid_discrete: bool,
    last_perf_query: Option<Instant>,
    last_temperature: Option<i16>,
}

impl GpuPowerGate {
    /// Creates a power gate for an adapter.
    pub(crate) fn new(is_hybrid_discrete: bool) -> Self {
        Self {
            is_hybrid_discrete,
            last_perf_query: None,
            last_temperature: None,
        }
    }

    /// Evaluates whether `KMTQAITYPE_ADAPTERPERFDATA` may be called at `now`.
    pub(crate) fn evaluate(
        &mut self,
        power_state: GpuDevicePowerState,
        has_node_activity: bool,
        now: Instant,
    ) -> PerfDataGateDecision {
        if !self.is_hybrid_discrete {
            self.last_perf_query = Some(now);
            return PerfDataGateDecision::Allow;
        }

        if power_state == GpuDevicePowerState::LowPowerD1ToD3 {
            // Never open the adapter or call `KMTQAITYPE_ADAPTERPERFDATA` while sleeping.
            self.last_temperature = None;
            return PerfDataGateDecision::SkipSleeping;
        }

        if has_node_activity {
            self.last_perf_query = Some(now);
            return PerfDataGateDecision::Allow;
        }

        let interval_elapsed = self.last_perf_query.is_none_or(|last| {
            now.saturating_duration_since(last) >= HYBRID_IDLE_PERFDATA_INTERVAL
        });
        if interval_elapsed {
            self.last_perf_query = Some(now);
            PerfDataGateDecision::Allow
        } else {
            PerfDataGateDecision::SkipIdleThrottled
        }
    }

    /// Updates the cached temperature reading after an allowed query.
    #[inline]
    pub(crate) fn record_temperature(&mut self, temp: Option<i16>) {
        self.last_temperature = temp;
    }

    /// Returns the most recent cached temperature reading (cleared when entering `D1`–`D3`).
    #[inline]
    pub(crate) fn cached_temperature(&self) -> Option<i16> {
        self.last_temperature
    }
}

type FnCmLocateDevNodeW = unsafe extern "system" fn(
    pdn_dev_inst: *mut u32,
    p_device_id: *const u16,
    ul_flags: u32,
) -> u32;
type FnCmGetDevNodePropertyW = unsafe extern "system" fn(
    dn_dev_inst: u32,
    property_key: *const DEVPROPKEY,
    property_type: *mut u32,
    property_buffer: *mut u8,
    property_buffer_size: *mut u32,
    ul_flags: u32,
) -> u32;
type FnCmGetDeviceInterfaceListSizeW = unsafe extern "system" fn(
    pul_len: *mut u32,
    interface_class_guid: *const GUID,
    p_device_id: *const u16,
    ul_flags: u32,
) -> u32;
type FnCmGetDeviceInterfaceListW = unsafe extern "system" fn(
    interface_class_guid: *const GUID,
    p_device_id: *const u16,
    buffer: *mut u16,
    buffer_len: u32,
    ul_flags: u32,
) -> u32;
type FnCmGetDeviceInterfacePropertyW = unsafe extern "system" fn(
    psz_device_interface: *const u16,
    property_key: *const DEVPROPKEY,
    property_type: *mut u32,
    property_buffer: *mut u8,
    property_buffer_size: *mut u32,
    ul_flags: u32,
) -> u32;

/// Dynamically resolved `cfgmgr32.dll` PnP power-state & display-interface functions.
struct GpuCfgMgrApi {
    _module: HMODULE,
    cm_locate_dev_node_w: FnCmLocateDevNodeW,
    cm_get_dev_node_property_w: FnCmGetDevNodePropertyW,
    cm_get_device_interface_list_size_w: Option<FnCmGetDeviceInterfaceListSizeW>,
    cm_get_device_interface_list_w: Option<FnCmGetDeviceInterfaceListW>,
    cm_get_device_interface_property_w: Option<FnCmGetDeviceInterfacePropertyW>,
}

unsafe impl Send for GpuCfgMgrApi {}
unsafe impl Sync for GpuCfgMgrApi {}

fn gpu_cfgmgr_api() -> Option<&'static GpuCfgMgrApi> {
    static API: OnceLock<Option<GpuCfgMgrApi>> = OnceLock::new();
    API.get_or_init(|| {
        let dll = to_wide_null("cfgmgr32.dll");
        // SAFETY: `cfgmgr32.dll` is a core Windows system DLL retained for process lifetime.
        let module = unsafe { LoadLibraryW(dll.as_ptr()) };
        if module.is_null() {
            return None;
        }
        let locate = unsafe { GetProcAddress(module, c"CM_Locate_DevNodeW".as_ptr().cast()) }?;
        let devnode_prop =
            unsafe { GetProcAddress(module, c"CM_Get_DevNode_PropertyW".as_ptr().cast()) }?;
        let list_size = unsafe {
            GetProcAddress(
                module,
                c"CM_Get_Device_Interface_List_SizeW".as_ptr().cast(),
            )
        };
        let list =
            unsafe { GetProcAddress(module, c"CM_Get_Device_Interface_ListW".as_ptr().cast()) };
        let iface_prop =
            unsafe { GetProcAddress(module, c"CM_Get_Device_Interface_PropertyW".as_ptr().cast()) };

        // SAFETY: Symbol names match exact `cfgmgr32.h` prototypes.
        unsafe {
            Some(GpuCfgMgrApi {
                _module: module,
                cm_locate_dev_node_w: transmute::<FarProc, FnCmLocateDevNodeW>(locate),
                cm_get_dev_node_property_w: transmute::<FarProc, FnCmGetDevNodePropertyW>(
                    devnode_prop,
                ),
                cm_get_device_interface_list_size_w: list_size
                    .map(|p| transmute::<FarProc, FnCmGetDeviceInterfaceListSizeW>(p)),
                cm_get_device_interface_list_w: list
                    .map(|p| transmute::<FarProc, FnCmGetDeviceInterfaceListW>(p)),
                cm_get_device_interface_property_w: iface_prop
                    .map(|p| transmute::<FarProc, FnCmGetDeviceInterfacePropertyW>(p)),
            })
        }
    })
    .as_ref()
}

/// Discovered PnP display device instance for matching a DXGI adapter to `DEVPKEY_Device_PowerData`.
struct PnpDisplayDevice {
    instance_id_upper: String,
    instance_id_wide: Vec<u16>,
    pci_bdf: Option<(u32, u32, u32)>,
}

/// Queries a `u32` PnP devnode property via `CM_Get_DevNode_PropertyW`.
fn query_devnode_u32(api: &GpuCfgMgrApi, devinst: u32, key: &DEVPROPKEY) -> Option<u32> {
    let mut prop_type: u32 = 0;
    let mut value: u32 = 0;
    let mut bytes = size_of::<u32>() as u32;
    // SAFETY: Output buffer is a valid 4-byte `u32`.
    let rc = unsafe {
        (api.cm_get_dev_node_property_w)(
            devinst,
            key,
            &mut prop_type,
            (&mut value as *mut u32).cast::<u8>(),
            &mut bytes,
            0,
        )
    };
    if rc == CR_SUCCESS && prop_type == DEVPROP_TYPE_UINT32 && bytes == size_of::<u32>() as u32 {
        Some(value)
    } else {
        None
    }
}

/// Enumerates present `GUID_DISPLAY_DEVICE_ARRIVAL` PnP device instances with PCI bus/device/function.
fn discover_display_pnp_devices() -> Vec<PnpDisplayDevice> {
    let Some(api) = gpu_cfgmgr_api() else {
        return Vec::new();
    };
    let (Some(list_size_fn), Some(list_fn), Some(iface_prop_fn)) = (
        api.cm_get_device_interface_list_size_w,
        api.cm_get_device_interface_list_w,
        api.cm_get_device_interface_property_w,
    ) else {
        return Vec::new();
    };

    let mut needed_chars: u32 = 0;
    // SAFETY: Queries required UTF-16 character count for present display device interfaces.
    let rc = unsafe {
        list_size_fn(
            &mut needed_chars,
            &GUID_DISPLAY_DEVICE_ARRIVAL,
            ptr::null(),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if rc != CR_SUCCESS || needed_chars <= 1 || needed_chars > 65_536 {
        return Vec::new();
    }

    let mut wide_list = vec![0u16; needed_chars as usize];
    // SAFETY: `wide_list` holds `needed_chars` UTF-16 code units.
    let rc2 = unsafe {
        list_fn(
            &GUID_DISPLAY_DEVICE_ARRIVAL,
            ptr::null(),
            wide_list.as_mut_ptr(),
            wide_list.len() as u32,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if rc2 != CR_SUCCESS {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < wide_list.len() && wide_list[pos] != 0 {
        let start = pos;
        while pos < wide_list.len() && wide_list[pos] != 0 {
            pos += 1;
        }
        let iface_ptr = wide_list[start..].as_ptr();
        pos += 1; // skip NUL

        let mut prop_type: u32 = 0;
        let mut needed_bytes: u32 = 0;
        // SAFETY: `iface_ptr` points to a NUL-terminated UTF-16 symbolic link inside `wide_list`.
        let prc = unsafe {
            iface_prop_fn(
                iface_ptr,
                &DEVPKEY_DEVICE_INSTANCE_ID,
                &mut prop_type,
                ptr::null_mut(),
                &mut needed_bytes,
                0,
            )
        };
        if (prc != CR_BUFFER_SMALL && prc != CR_SUCCESS) || needed_bytes == 0 || needed_bytes > 4096
        {
            continue;
        }

        let mut id_wide = vec![0u16; (needed_bytes as usize).div_ceil(size_of::<u16>()) + 1];
        let mut out_bytes = ((id_wide.len() - 1) * size_of::<u16>()) as u32;
        // SAFETY: `id_wide` is sized for at least `needed_bytes` plus trailing NUL.
        let prc2 = unsafe {
            iface_prop_fn(
                iface_ptr,
                &DEVPKEY_DEVICE_INSTANCE_ID,
                &mut prop_type,
                id_wide.as_mut_ptr().cast::<u8>(),
                &mut out_bytes,
                0,
            )
        };
        if prc2 != CR_SUCCESS || prop_type != DEVPROP_TYPE_STRING {
            continue;
        }
        let code_units = (out_bytes as usize / size_of::<u16>()).min(id_wide.len() - 1);
        id_wide[code_units] = 0;
        let id_str = decode_wide_nul(&id_wide[..code_units]);
        if id_str.is_empty() {
            continue;
        }

        let mut devinst: u32 = 0;
        // SAFETY: `id_wide` is NUL-terminated UTF-16.
        let pci_bdf = if unsafe {
            (api.cm_locate_dev_node_w)(&mut devinst, id_wide.as_ptr(), CM_LOCATE_DEVNODE_NORMAL)
        } == CR_SUCCESS
            && devinst != 0
        {
            let bus = query_devnode_u32(api, devinst, &DEVPKEY_DEVICE_BUS_NUMBER);
            let addr = query_devnode_u32(api, devinst, &DEVPKEY_DEVICE_ADDRESS);
            match (bus, addr) {
                (Some(b), Some(a)) => Some((b, (a >> 16) & 0xFFFF, a & 0xFFFF)),
                _ => None,
            }
        } else {
            None
        };

        out.push(PnpDisplayDevice {
            instance_id_upper: id_str.to_ascii_uppercase(),
            instance_id_wide: id_wide,
            pci_bdf,
        });
    }
    out
}

/// Queries the current PnP device power state (`D0` vs `D1`–`D3`) for a GPU devnode via
/// `DEVPKEY_Device_PowerData` (`CM_POWER_DATA.PD_MostRecentPowerState`) without waking the GPU.
fn query_gpu_device_power_state(dev_instance_id_wide: &[u16]) -> GpuDevicePowerState {
    if dev_instance_id_wide.is_empty() {
        return GpuDevicePowerState::Unknown;
    }
    let Some(api) = gpu_cfgmgr_api() else {
        return GpuDevicePowerState::Unknown;
    };

    let mut devinst: u32 = 0;
    // SAFETY: `dev_instance_id_wide` is NUL-terminated UTF-16.
    let rc = unsafe {
        (api.cm_locate_dev_node_w)(
            &mut devinst,
            dev_instance_id_wide.as_ptr(),
            CM_LOCATE_DEVNODE_NORMAL,
        )
    };
    if rc != CR_SUCCESS || devinst == 0 {
        return GpuDevicePowerState::Unknown;
    }

    let mut power_data = CM_POWER_DATA::default();
    let mut prop_type: u32 = 0;
    let mut bytes = size_of::<CM_POWER_DATA>() as u32;
    // SAFETY: `power_data` matches `CM_POWER_DATA` returned for `DEVPKEY_Device_PowerData`.
    let prc = unsafe {
        (api.cm_get_dev_node_property_w)(
            devinst,
            &DEVPKEY_DEVICE_POWER_DATA,
            &mut prop_type,
            (&mut power_data as *mut CM_POWER_DATA).cast::<u8>(),
            &mut bytes,
            0,
        )
    };
    if prc != CR_SUCCESS
        || prop_type != DEVPROP_TYPE_BINARY
        || (bytes as usize) < size_of::<u32>() * 2
    {
        return GpuDevicePowerState::Unknown;
    }

    let state = power_data.PD_MostRecentPowerState;
    if state == PowerDeviceD0 {
        GpuDevicePowerState::D0
    } else if state == PowerDeviceD1 || state == PowerDeviceD2 || state == PowerDeviceD3 {
        GpuDevicePowerState::LowPowerD1ToD3
    } else {
        GpuDevicePowerState::Unknown
    }
}

/// Cached static GPU adapter inventory entry.
#[derive(Clone)]
struct CachedGpuAdapter {
    index: u32,
    luid_raw: LUID,
    luid: u64,
    name: Arc<str>,
    driver_version: Option<Arc<str>>,
    directx_version: Option<Arc<str>>,
    pci_location: Option<Arc<str>>,
    dedicated_total_bytes: u64,
    shared_total_bytes: u64,
    node_count: u32,
    segment_count: u32,
    /// Bitmask of segments where `SegmentInformation.Aperture != 0` (shared system memory).
    aperture_segment_mask: u128,
    node_names: Vec<Arc<str>>,
    /// True when `D3DKMT_ADAPTERTYPE.HybridDiscrete` is set (laptop switchable dGPU).
    is_hybrid_discrete: bool,
    /// Matched NUL-terminated PnP Device Instance ID for `DEVPKEY_Device_PowerData` checks.
    pnp_instance_id_wide: Option<Vec<u16>>,
}

/// Previous per-node GPU running time counter (`100 ns` units) and wall timestamp.
#[derive(Debug, Clone, Copy)]
struct PrevNodeSample {
    running_time_100ns: u64,
    sampled_at: Instant,
}

/// Unique key for per-process, per-adapter, per-node GPU running-time deltas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ProcessGpuNodeKey {
    process: ProcessKey,
    sequence_number: Option<u64>,
    adapter_luid: u64,
    node_id: u32,
}

/// Primary GPU summary metrics returned for [`loom_types::SystemPerfSample`].
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct PrimaryGpuSummary {
    pub(crate) util_permille: u16,
    pub(crate) dedicated_used_bytes: u64,
    pub(crate) dedicated_total_bytes: u64,
    pub(crate) shared_used_bytes: u64,
}

/// Fast-lane Windows GPU telemetry sampler owned by [`crate::WindowsFastSampler`].
pub(crate) struct GpuPerfSampler {
    api: Option<GpuApi>,
    adapters: Vec<CachedGpuAdapter>,
    last_discovery: Option<Instant>,
    prev_nodes: HashMap<(u64, u32), PrevNodeSample>,
    power_gates: HashMap<u64, GpuPowerGate>,
    /// Reusable per-process GPU engine running-time delta map (zero steady-state reallocation).
    prev_proc_nodes: HashMap<ProcessGpuNodeKey, PrevNodeSample>,
    /// Reusable set of live `ProcessKey`s observed on the current tick for stale-key pruning.
    live_proc_keys: HashSet<ProcessKey>,
}

impl Default for GpuPerfSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl GpuPerfSampler {
    /// Creates a new GPU sampler, resolving `dxgi.dll` and `gdi32.dll` dynamically.
    pub(crate) fn new() -> Self {
        Self {
            api: GpuApi::load(),
            adapters: Vec::new(),
            last_discovery: None,
            prev_nodes: HashMap::with_capacity(32),
            power_gates: HashMap::with_capacity(4),
            prev_proc_nodes: HashMap::with_capacity(256),
            live_proc_keys: HashSet::with_capacity(512),
        }
    }

    /// Ensures the cached hardware GPU adapter inventory is populated and fresh.
    fn ensure_adapters_discovered(&mut self, now: Instant) {
        let needs_discovery = self
            .last_discovery
            .is_none_or(|t| now.saturating_duration_since(t) >= ADAPTER_CACHE_TTL);
        if needs_discovery {
            self.discover_adapters();
            self.last_discovery = Some(now);
        }
    }

    /// Samples per-process dedicated VRAM, shared GPU memory, and GPU engine utilization
    /// directly into `samples` using pure kernel-mode `gdi32!D3DKMTQueryStatistics`
    /// (`PROCESS` / `PROCESS_ADAPTER` / `PROCESS_SEGMENT` / `PROCESS_NODE`) with short-lived
    /// `PROCESS_QUERY_LIMITED_INFORMATION` handles (immediately dropped per process).
    ///
    /// Gracefully reports zeros for inaccessible/kernel processes or systems without hardware GPUs.
    pub(crate) fn sample_process_gpu_metrics(&mut self, samples: &mut [ProcessHotSample<()>]) {
        if self.api.is_none() || samples.is_empty() {
            return;
        }
        let now = Instant::now();
        self.ensure_adapters_discovered(now);
        if self.adapters.is_empty() {
            return;
        }

        let Some(ref api) = self.api else {
            return;
        };

        self.live_proc_keys.clear();

        for sample in samples.iter_mut() {
            self.live_proc_keys.insert(sample.key);
            if sample.key.pid == 0 || sample.key.pid == 4 {
                continue;
            }

            // Open a strictly short-lived handle (`PROCESS_QUERY_LIMITED_INFORMATION`) that is
            // closed immediately when `proc_handle` drops at the end of this iteration.
            // SAFETY: `OpenProcess` takes plain integer arguments and returns null on error.
            let raw_handle =
                unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, sample.key.pid) };
            let Some(proc_handle) = OwnedHandle::from_raw(raw_handle) else {
                continue;
            };

            let mut dedicated_bytes: u64 = 0;
            let mut shared_bytes: u64 = 0;
            let mut max_util_permille: u16 = 0;
            let mut cached_seq_num: Option<Option<u64>> = None;

            for adapter in &self.adapters {
                // 1. Fast kernel pre-check (`D3DKMT_QUERYSTATISTICS_PROCESS`): skip immediately
                // if this process has no DxgKrnl GPU context on this adapter.
                let proc_query = D3DKMT_QUERYSTATISTICS {
                    Type: D3DKMT_QUERYSTATISTICS_PROCESS,
                    AdapterLuid: adapter.luid_raw,
                    hProcess: proc_handle.as_raw(),
                    ..D3DKMT_QUERYSTATISTICS::default()
                };
                // SAFETY: `proc_query` is configured for `D3DKMT_QUERYSTATISTICS_PROCESS`.
                if unsafe { (api.d3dkmt_query_statistics)(&proc_query) } != STATUS_SUCCESS {
                    continue;
                }

                // 2. Check adapter-specific attachment (`D3DKMT_QUERYSTATISTICS_PROCESS_ADAPTER`)
                // so processes using only an iGPU skip all segment/node queries on an idle dGPU.
                let proc_adapter_query = D3DKMT_QUERYSTATISTICS {
                    Type: D3DKMT_QUERYSTATISTICS_PROCESS_ADAPTER,
                    AdapterLuid: adapter.luid_raw,
                    hProcess: proc_handle.as_raw(),
                    ..D3DKMT_QUERYSTATISTICS::default()
                };
                // SAFETY: Configured for `D3DKMT_QUERYSTATISTICS_PROCESS_ADAPTER`.
                if unsafe { (api.d3dkmt_query_statistics)(&proc_adapter_query) } != STATUS_SUCCESS {
                    continue;
                }

                let sequence_number = *cached_seq_num
                    .get_or_insert_with(|| query_process_sequence_number(proc_handle.as_raw()));

                // 3. Per-segment dedicated vs shared GPU memory (`D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT`).
                for seg_id in 0..adapter.segment_count {
                    let mut seg_query = D3DKMT_QUERYSTATISTICS {
                        Type: D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT,
                        AdapterLuid: adapter.luid_raw,
                        hProcess: proc_handle.as_raw(),
                        ..D3DKMT_QUERYSTATISTICS::default()
                    };
                    seg_query.Anonymous.QueryProcessSegment.SegmentId = seg_id;

                    // SAFETY: Configured for `D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT`.
                    if unsafe { (api.d3dkmt_query_statistics)(&seg_query) } != STATUS_SUCCESS {
                        continue;
                    }
                    // SAFETY: `Type == D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT` populates `ProcessSegmentInformation`.
                    let seg_info = unsafe { seg_query.QueryResult.ProcessSegmentInformation };
                    let bytes = seg_info.BytesCommitted;
                    if bytes == 0 {
                        continue;
                    }
                    let is_shared = ((adapter.aperture_segment_mask >> seg_id) & 1) != 0;
                    if is_shared {
                        shared_bytes = shared_bytes.saturating_add(bytes);
                    } else {
                        dedicated_bytes = dedicated_bytes.saturating_add(bytes);
                    }
                }

                // 4. Per-node running time delta (`D3DKMT_QUERYSTATISTICS_PROCESS_NODE`).
                for node_id in 0..adapter.node_count {
                    let mut node_query = D3DKMT_QUERYSTATISTICS {
                        Type: D3DKMT_QUERYSTATISTICS_PROCESS_NODE,
                        AdapterLuid: adapter.luid_raw,
                        hProcess: proc_handle.as_raw(),
                        ..D3DKMT_QUERYSTATISTICS::default()
                    };
                    node_query.Anonymous.QueryProcessNode.NodeId = node_id;

                    // SAFETY: Configured for `D3DKMT_QUERYSTATISTICS_PROCESS_NODE`.
                    if unsafe { (api.d3dkmt_query_statistics)(&node_query) } != STATUS_SUCCESS {
                        continue;
                    }
                    // SAFETY: `Type == D3DKMT_QUERYSTATISTICS_PROCESS_NODE` populates `ProcessNodeInformation`.
                    let node_info = unsafe { node_query.QueryResult.ProcessNodeInformation };
                    let running_time_100ns = node_info.RunningTime.max(0) as u64;
                    let key = ProcessGpuNodeKey {
                        process: sample.key,
                        sequence_number,
                        adapter_luid: adapter.luid,
                        node_id,
                    };
                    let util = compute_process_node_util(
                        &mut self.prev_proc_nodes,
                        key,
                        running_time_100ns,
                        now,
                    );
                    max_util_permille = max_util_permille.max(util);
                }
            }

            sample.gpu_dedicated_bytes = dedicated_bytes;
            sample.gpu_shared_bytes = shared_bytes;
            sample.gpu_util_permille = max_util_permille;
        }

        prune_stale_process_gpu_nodes(&mut self.prev_proc_nodes, &self.live_proc_keys);
    }

    /// Samples all hardware GPU adapters into `out` and returns the primary GPU summary.
    pub(crate) fn sample(&mut self, out: &mut Vec<GpuAdapterSample>) -> PrimaryGpuSummary {
        out.clear();
        if self.api.is_none() {
            return PrimaryGpuSummary::default();
        }

        let now = Instant::now();
        self.ensure_adapters_discovered(now);

        let Some(ref api) = self.api else {
            return PrimaryGpuSummary::default();
        };

        for adapter in &self.adapters {
            let (dedicated_used, shared_used, seg_ded_limit, seg_shared_limit, _) =
                query_adapter_memory(api, adapter.luid_raw, adapter.segment_count);

            let dedicated_total = if adapter.dedicated_total_bytes > 0 {
                adapter.dedicated_total_bytes
            } else {
                seg_ded_limit
            };
            let shared_total = if adapter.shared_total_bytes > 0 {
                adapter.shared_total_bytes
            } else {
                seg_shared_limit
            };

            let dedicated_used_clamped = if dedicated_total > 0 {
                dedicated_used.min(dedicated_total)
            } else {
                dedicated_used
            };
            let shared_used_clamped = if shared_total > 0 {
                shared_used.min(shared_total)
            } else {
                shared_used
            };

            // Sample per-node running time deltas and aggregate by unique engine name.
            let mut engine_max: BTreeMap<Arc<str>, u16> = BTreeMap::new();
            let mut adapter_max_util: u16 = 0;
            let mut any_node_activity = false;

            for node_id in 0..adapter.node_count {
                if let Some(running_time_100ns) =
                    query_node_running_time(api, adapter.luid_raw, node_id)
                {
                    let key = (adapter.luid, node_id);
                    let (util, delta_100ns) =
                        compute_node_util(&mut self.prev_nodes, key, running_time_100ns, now);
                    if delta_100ns > 0 || util > 0 {
                        any_node_activity = true;
                    }
                    adapter_max_util = adapter_max_util.max(util);
                    let engine_name = adapter
                        .node_names
                        .get(node_id as usize)
                        .cloned()
                        .unwrap_or_else(|| Arc::from(format!("Engine {node_id}")));
                    engine_max
                        .entry(engine_name)
                        .and_modify(|v| *v = (*v).max(util))
                        .or_insert(util);
                }
            }

            let mut engines: Vec<GpuEngineSample> = engine_max
                .into_iter()
                .map(|(name, util_permille)| GpuEngineSample {
                    name,
                    util_permille,
                })
                .collect();
            engines.sort_by(|a, b| {
                engine_sort_rank(&a.name)
                    .cmp(&engine_sort_rank(&b.name))
                    .then_with(|| a.name.cmp(&b.name))
            });

            // Power-gate `KMTQAITYPE_ADAPTERPERFDATA` for hybrid discrete GPUs so sleeping
            // laptop dGPUs in `D1`–`D3` are never woken up.
            let power_state = if adapter.is_hybrid_discrete {
                adapter
                    .pnp_instance_id_wide
                    .as_deref()
                    .map_or(GpuDevicePowerState::Unknown, query_gpu_device_power_state)
            } else {
                GpuDevicePowerState::D0
            };
            let gate = self
                .power_gates
                .entry(adapter.luid)
                .or_insert_with(|| GpuPowerGate::new(adapter.is_hybrid_discrete));
            let temperature_celsius = match gate.evaluate(power_state, any_node_activity, now) {
                PerfDataGateDecision::Allow => {
                    let temp = query_adapter_temperature(api, adapter.luid_raw);
                    gate.record_temperature(temp);
                    temp
                }
                PerfDataGateDecision::SkipSleeping => None,
                PerfDataGateDecision::SkipIdleThrottled => gate.cached_temperature(),
            };

            out.push(GpuAdapterSample {
                index: adapter.index,
                luid: adapter.luid,
                name: Arc::clone(&adapter.name),
                driver_version: adapter.driver_version.clone(),
                directx_version: adapter.directx_version.clone(),
                pci_location: adapter.pci_location.clone(),
                temperature_celsius,
                util_permille: adapter_max_util,
                dedicated_used_bytes: dedicated_used_clamped,
                dedicated_total_bytes: dedicated_total,
                shared_used_bytes: shared_used_clamped,
                shared_total_bytes: shared_total,
                d3_asleep: false,
                engines,
            });
        }

        // Aggregate primary GPU onto SystemPerfSample.
        if out.is_empty() {
            return PrimaryGpuSummary::default();
        }
        let max_util = out.iter().map(|g| g.util_permille).max().unwrap_or(0);
        let primary = out
            .iter()
            .max_by_key(|g| (g.dedicated_total_bytes, g.util_permille))
            .unwrap_or(&out[0]);
        PrimaryGpuSummary {
            util_permille: max_util,
            dedicated_used_bytes: primary.dedicated_used_bytes,
            dedicated_total_bytes: primary.dedicated_total_bytes,
            shared_used_bytes: primary.shared_used_bytes,
        }
    }

    /// Enumerates hardware DXGI adapters and populates static D3DKMT + registry + PnP metadata.
    fn discover_adapters(&mut self) {
        self.adapters.clear();
        let Some(ref api) = self.api else {
            return;
        };

        let mut factory_raw: *mut c_void = ptr::null_mut();
        // SAFETY: `CreateDXGIFactory1` creates a COM `IDXGIFactory1` interface pointer.
        let hr = unsafe { (api.create_dxgi_factory1)(&IID_IDXGIFACTORY1, &mut factory_raw) };
        if hr < 0 || factory_raw.is_null() {
            return;
        }
        let factory = ComPtr(factory_raw);
        let reg_drivers = load_display_driver_versions();
        let pnp_displays = discover_display_pnp_devices();

        let mut display_idx: u32 = 0;
        let mut enum_idx: u32 = 0;
        while enum_idx < 32 {
            let mut adapter_raw: *mut c_void = ptr::null_mut();
            // SAFETY: Vtable slot 12 on `IDXGIFactory1` is `EnumAdapters1`.
            let enum_hr = unsafe {
                let func_ptr = factory.vtable_entry(12);
                let enum_fn: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32 =
                    transmute(func_ptr);
                enum_fn(factory.as_ptr(), enum_idx, &mut adapter_raw)
            };
            enum_idx += 1;

            if enum_hr < 0 || adapter_raw.is_null() {
                break;
            }
            let adapter = ComPtr(adapter_raw);

            let mut desc = DxgiAdapterDesc1::default();
            // SAFETY: Vtable slot 10 on `IDXGIAdapter1` is `GetDesc1`.
            let desc_hr = unsafe {
                let func_ptr = adapter.vtable_entry(10);
                let get_desc1: unsafe extern "system" fn(
                    *mut c_void,
                    *mut DxgiAdapterDesc1,
                ) -> i32 = transmute(func_ptr);
                get_desc1(adapter.as_ptr(), &mut desc)
            };
            if desc_hr < 0 {
                continue;
            }

            // Skip Microsoft Basic Render Driver (software WARP adapter).
            if (desc.flags & DXGI_ADAPTER_FLAG_SOFTWARE) != 0 {
                continue;
            }

            let luid = pack_luid(desc.adapter_luid);
            // Deduplicate identical LUIDs if DXGI reports a mirror entry.
            if self.adapters.iter().any(|a| a.luid == luid) {
                continue;
            }

            let name_str = decode_wide_nul(&desc.description);
            let name: Arc<str> = if name_str.is_empty() {
                Arc::from(format!("GPU {display_idx}"))
            } else {
                Arc::from(name_str.as_str())
            };

            let (node_count, segment_count) = query_adapter_topology(api, desc.adapter_luid);
            let Some(static_info) = query_static_adapter_info(api, desc.adapter_luid, node_count)
            else {
                // Skip software-only or indirect virtual display mirror adapters.
                continue;
            };
            // Guard against virtual/indirect display mirrors duplicating a real PCI hardware GPU
            // with an unassigned PCI address.
            if static_info.pci_location.is_none()
                && self
                    .adapters
                    .iter()
                    .any(|a| a.pci_location.is_some() && a.name == name)
            {
                continue;
            }
            let (_, _, _, _, aperture_segment_mask) =
                query_adapter_memory(api, desc.adapter_luid, segment_count);
            let driver_version =
                match_driver_version(&reg_drivers, desc.vendor_id, desc.device_id, &name_str);
            let pnp_instance_id_wide = match_pnp_display_instance(
                &pnp_displays,
                static_info.pci_bdf,
                desc.vendor_id,
                desc.device_id,
            );

            self.adapters.push(CachedGpuAdapter {
                index: display_idx,
                luid_raw: desc.adapter_luid,
                luid,
                name,
                driver_version,
                directx_version: static_info.directx_version,
                pci_location: static_info.pci_location,
                dedicated_total_bytes: desc.dedicated_video_memory as u64,
                shared_total_bytes: desc.shared_system_memory as u64,
                node_count,
                segment_count,
                aperture_segment_mask,
                node_names: static_info.node_names,
                is_hybrid_discrete: static_info.is_hybrid_discrete,
                pnp_instance_id_wide,
            });
            display_idx += 1;
        }

        // Retain power-gate state only for currently present adapter LUIDs.
        self.power_gates
            .retain(|luid, _| self.adapters.iter().any(|a| a.luid == *luid));
    }
}

/// Matches a DXGI adapter to its PnP display Device Instance ID by exact PCI BDF or VEN/DEV ID.
fn match_pnp_display_instance(
    devices: &[PnpDisplayDevice],
    pci_bdf: Option<(u32, u32, u32)>,
    vendor_id: u32,
    device_id: u32,
) -> Option<Vec<u16>> {
    if let Some(bdf) = pci_bdf
        && let Some(dev) = devices.iter().find(|d| d.pci_bdf == Some(bdf))
    {
        return Some(dev.instance_id_wide.clone());
    }
    let needle = format!("VEN_{vendor_id:04X}&DEV_{device_id:04X}");
    devices
        .iter()
        .find(|d| d.instance_id_upper.contains(&needle))
        .map(|d| d.instance_id_wide.clone())
}

/// Static D3DKMT adapter attributes cached per hardware GPU.
struct StaticAdapterInfo {
    node_names: Vec<Arc<str>>,
    pci_location: Option<Arc<str>>,
    pci_bdf: Option<(u32, u32, u32)>,
    directx_version: Option<Arc<str>>,
    is_hybrid_discrete: bool,
}

/// Queries D3DKMT adapter node count and memory segment count.
fn query_adapter_topology(api: &GpuApi, luid: LUID) -> (u32, u32) {
    let query = D3DKMT_QUERYSTATISTICS {
        Type: D3DKMT_QUERYSTATISTICS_ADAPTER,
        AdapterLuid: luid,
        ..D3DKMT_QUERYSTATISTICS::default()
    };

    // SAFETY: `query` is initialized with `Type = D3DKMT_QUERYSTATISTICS_ADAPTER`.
    let status = unsafe { (api.d3dkmt_query_statistics)(&query) };
    if status != STATUS_SUCCESS {
        return (0, 0);
    }

    // SAFETY: `Type == D3DKMT_QUERYSTATISTICS_ADAPTER` populates `AdapterInformation`.
    let info = unsafe { query.QueryResult.AdapterInformation };
    (
        info.NodeCount.min(MAX_D3DKMT_ITEMS),
        info.NbSegments.min(MAX_D3DKMT_ITEMS),
    )
}

/// Queries static per-node engine names, PCI address, and WDDM/DirectX version for an adapter.
///
/// Returns `None` if D3DKMT reports this adapter as a software-only, indirect-display, or
/// non-render virtual mirror device.
fn query_static_adapter_info(
    api: &GpuApi,
    luid: LUID,
    node_count: u32,
) -> Option<StaticAdapterInfo> {
    let mut node_names = Vec::with_capacity(node_count as usize);
    let Some(query_info) = api.d3dkmt_query_adapter_info else {
        for n in 0..node_count {
            node_names.push(Arc::from(default_node_name(n)));
        }
        return Some(StaticAdapterInfo {
            node_names,
            pci_location: None,
            pci_bdf: None,
            directx_version: None,
            is_hybrid_discrete: false,
        });
    };

    let Some(handle) = api.open_adapter_handle(luid) else {
        for n in 0..node_count {
            node_names.push(Arc::from(default_node_name(n)));
        }
        return Some(StaticAdapterInfo {
            node_names,
            pci_location: None,
            pci_bdf: None,
            directx_version: None,
            is_hybrid_discrete: false,
        });
    };

    // 0. Filter out software-only, indirect display, or display-only non-render adapters,
    // and capture `HybridDiscrete` for D3 power-gating.
    let mut is_hybrid_discrete = false;
    let mut adapter_type = D3DKMT_ADAPTERTYPE::default();
    let mut qai_type = D3DKMT_QUERYADAPTERINFO {
        hAdapter: handle,
        Type: KMTQAITYPE_ADAPTERTYPE,
        pPrivateDriverData: (&mut adapter_type as *mut D3DKMT_ADAPTERTYPE).cast::<c_void>(),
        PrivateDriverDataSize: size_of::<D3DKMT_ADAPTERTYPE>() as u32,
    };
    if unsafe { query_info(&mut qai_type) } == STATUS_SUCCESS {
        // SAFETY: `D3DKMT_ADAPTERTYPE_0` is a union over `Value: u32`.
        let bits = unsafe { adapter_type.Anonymous.Value };
        if (bits & ADAPTER_TYPE_SOFTWARE_DEVICE) != 0
            || (bits & ADAPTER_TYPE_INDIRECT_DISPLAY) != 0
            || (bits & ADAPTER_TYPE_RENDER_SUPPORTED) == 0
        {
            api.close_adapter_handle(handle);
            return None;
        }
        is_hybrid_discrete = (bits & ADAPTER_TYPE_HYBRID_DISCRETE) != 0;
    }

    // 1. Resolve per-node engine metadata (`KMTQAITYPE_NODEMETADATA`).
    for node_id in 0..node_count {
        let mut meta = D3DKMT_NODEMETADATA {
            NodeOrdinalAndAdapterIndex: node_id,
            ..D3DKMT_NODEMETADATA::default()
        };
        let mut qai = D3DKMT_QUERYADAPTERINFO {
            hAdapter: handle,
            Type: KMTQAITYPE_NODEMETADATA,
            pPrivateDriverData: (&mut meta as *mut D3DKMT_NODEMETADATA).cast::<c_void>(),
            PrivateDriverDataSize: size_of::<D3DKMT_NODEMETADATA>() as u32,
        };
        // SAFETY: `qai` points to valid `D3DKMT_NODEMETADATA` storage.
        let status = unsafe { query_info(&mut qai) };
        if status == STATUS_SUCCESS {
            let engine_type = meta.NodeData.EngineType;
            let friendly = meta.NodeData.FriendlyName;
            node_names.push(Arc::from(format_engine_name(
                engine_type,
                &friendly,
                node_id,
            )));
        } else {
            node_names.push(Arc::from(default_node_name(node_id)));
        }
    }

    // 2. Query PCI bus/device/function (`KMTQAITYPE_ADAPTERADDRESS`).
    let mut pci_addr = D3DKMT_ADAPTERADDRESS::default();
    let mut qai_pci = D3DKMT_QUERYADAPTERINFO {
        hAdapter: handle,
        Type: KMTQAITYPE_ADAPTERADDRESS,
        pPrivateDriverData: (&mut pci_addr as *mut D3DKMT_ADAPTERADDRESS).cast::<c_void>(),
        PrivateDriverDataSize: size_of::<D3DKMT_ADAPTERADDRESS>() as u32,
    };
    let (pci_location, pci_bdf) = if unsafe { query_info(&mut qai_pci) } == STATUS_SUCCESS
        && pci_addr.BusNumber != u32::MAX
        && pci_addr.DeviceNumber <= 0xFF
        && pci_addr.FunctionNumber <= 0xFF
    {
        (
            Some(Arc::from(format!(
                "PCI bus {}, device {}, function {}",
                pci_addr.BusNumber, pci_addr.DeviceNumber, pci_addr.FunctionNumber
            ))),
            Some((
                pci_addr.BusNumber,
                pci_addr.DeviceNumber,
                pci_addr.FunctionNumber,
            )),
        )
    } else {
        (None, None)
    };

    // 3. Query WDDM / DirectX driver model (`KMTQAITYPE_DRIVERVERSION`).
    let mut driver_model: i32 = 0;
    let mut qai_ver = D3DKMT_QUERYADAPTERINFO {
        hAdapter: handle,
        Type: KMTQAITYPE_DRIVERVERSION,
        pPrivateDriverData: (&mut driver_model as *mut i32).cast::<c_void>(),
        PrivateDriverDataSize: size_of::<i32>() as u32,
    };
    let directx_version =
        if unsafe { query_info(&mut qai_ver) } == STATUS_SUCCESS && driver_model > 0 {
            Some(Arc::from(format_directx_version(driver_model)))
        } else {
            None
        };

    api.close_adapter_handle(handle);
    Some(StaticAdapterInfo {
        node_names,
        pci_location,
        pci_bdf,
        directx_version,
        is_hybrid_discrete,
    })
}

/// Queries dedicated & shared video memory usage, commit limits, and the aperture segment
/// bitmask (`bit i == 1` => segment `i` is shared aperture memory) across all D3DKMT segments.
fn query_adapter_memory(
    api: &GpuApi,
    luid: LUID,
    segment_count: u32,
) -> (u64, u64, u64, u64, u128) {
    let mut dedicated_used: u64 = 0;
    let mut shared_used: u64 = 0;
    let mut dedicated_limit: u64 = 0;
    let mut shared_limit: u64 = 0;
    let mut aperture_mask: u128 = 0;

    for seg_id in 0..segment_count.min(MAX_D3DKMT_ITEMS) {
        let mut query = D3DKMT_QUERYSTATISTICS {
            Type: D3DKMT_QUERYSTATISTICS_SEGMENT,
            AdapterLuid: luid,
            ..D3DKMT_QUERYSTATISTICS::default()
        };
        query.Anonymous.QuerySegment.SegmentId = seg_id;

        // SAFETY: `query` is configured for `D3DKMT_QUERYSTATISTICS_SEGMENT`.
        let status = unsafe { (api.d3dkmt_query_statistics)(&query) };
        if status != STATUS_SUCCESS {
            continue;
        }

        // SAFETY: `Type == D3DKMT_QUERYSTATISTICS_SEGMENT` populates `SegmentInformation`.
        let seg = unsafe { query.QueryResult.SegmentInformation };
        let used = if seg.BytesCommitted > 0 {
            seg.BytesCommitted
        } else {
            seg.BytesResident
        };

        if seg.Aperture == 0 {
            dedicated_used = dedicated_used.saturating_add(used);
            dedicated_limit = dedicated_limit.saturating_add(seg.CommitLimit);
        } else {
            aperture_mask |= 1u128 << seg_id;
            shared_used = shared_used.saturating_add(used);
            shared_limit = shared_limit.saturating_add(seg.CommitLimit);
        }
    }

    (
        dedicated_used,
        shared_used,
        dedicated_limit,
        shared_limit,
        aperture_mask,
    )
}

/// Queries cumulative global GPU running time (`100 ns` ticks) for `node_id`.
fn query_node_running_time(api: &GpuApi, luid: LUID, node_id: u32) -> Option<u64> {
    let mut query = D3DKMT_QUERYSTATISTICS {
        Type: D3DKMT_QUERYSTATISTICS_NODE,
        AdapterLuid: luid,
        ..D3DKMT_QUERYSTATISTICS::default()
    };
    query.Anonymous.QueryNode.NodeId = node_id;

    // SAFETY: `query` is configured for `D3DKMT_QUERYSTATISTICS_NODE`.
    let status = unsafe { (api.d3dkmt_query_statistics)(&query) };
    if status != STATUS_SUCCESS {
        return None;
    }

    // SAFETY: `Type == D3DKMT_QUERYSTATISTICS_NODE` populates `NodeInformation`.
    let node_info = unsafe { query.QueryResult.NodeInformation };
    Some(node_info.GlobalInformation.RunningTime.max(0) as u64)
}

/// Queries optional GPU core temperature in whole degrees Celsius via `KMTQAITYPE_ADAPTERPERFDATA`.
fn query_adapter_temperature(api: &GpuApi, luid: LUID) -> Option<i16> {
    let query_info = api.d3dkmt_query_adapter_info?;
    let handle = api.open_adapter_handle(luid)?;

    let mut perf = D3DKMT_ADAPTER_PERFDATA::default();
    let mut qai = D3DKMT_QUERYADAPTERINFO {
        hAdapter: handle,
        Type: KMTQAITYPE_ADAPTERPERFDATA,
        pPrivateDriverData: (&mut perf as *mut D3DKMT_ADAPTER_PERFDATA).cast::<c_void>(),
        PrivateDriverDataSize: size_of::<D3DKMT_ADAPTER_PERFDATA>() as u32,
    };
    // SAFETY: `qai` points to valid `D3DKMT_ADAPTER_PERFDATA` storage.
    let status = unsafe { query_info(&mut qai) };
    api.close_adapter_handle(handle);

    // `D3DKMT_ADAPTER_PERFDATA.Temperature` is reported in deci-Celsius (tenths of °C).
    if status == STATUS_SUCCESS && perf.Temperature >= 10 && perf.Temperature <= 1500 {
        Some((perf.Temperature / 10) as i16)
    } else {
        None
    }
}

/// Computes single-node GPU engine utilization in permille (`0..=1000`) and raw `100 ns`
/// delta using `checked_sub` counter-reset safety.
fn compute_node_util(
    prev_map: &mut HashMap<(u64, u32), PrevNodeSample>,
    key: (u64, u32),
    running_time_100ns: u64,
    now: Instant,
) -> (u16, u64) {
    let next = PrevNodeSample {
        running_time_100ns,
        sampled_at: now,
    };
    let Some(prev) = prev_map.insert(key, next) else {
        return (0, 0);
    };

    let dt_100ns = now.saturating_duration_since(prev.sampled_at).as_nanos() / 100;
    if dt_100ns == 0 {
        return (0, 0);
    }

    let Some(d_run) = running_time_100ns.checked_sub(prev.running_time_100ns) else {
        return (0, 0);
    };
    let util = ((u128::from(d_run) * 1_000) / dt_100ns).min(1_000) as u16;
    (util, d_run)
}

/// Computes per-process single-node GPU utilization in permille (`0..=1000`) using `checked_sub`
/// counter-reset safety.
fn compute_process_node_util(
    prev_map: &mut HashMap<ProcessGpuNodeKey, PrevNodeSample>,
    key: ProcessGpuNodeKey,
    running_time_100ns: u64,
    now: Instant,
) -> u16 {
    let next = PrevNodeSample {
        running_time_100ns,
        sampled_at: now,
    };
    let Some(prev) = prev_map.insert(key, next) else {
        return 0;
    };

    let dt_100ns = now.saturating_duration_since(prev.sampled_at).as_nanos() / 100;
    if dt_100ns == 0 {
        return 0;
    }

    let Some(d_run) = running_time_100ns.checked_sub(prev.running_time_100ns) else {
        return 0;
    };
    ((u128::from(d_run) * 1_000) / dt_100ns).min(1_000) as u16
}

/// Removes exited `ProcessKey` entries from the per-process GPU running-time delta cache.
fn prune_stale_process_gpu_nodes(
    prev_map: &mut HashMap<ProcessGpuNodeKey, PrevNodeSample>,
    live_keys: &HashSet<ProcessKey>,
) {
    prev_map.retain(|k, _| live_keys.contains(&k.process));
}

/// Cached registry display-adapter driver record.
struct RegDisplayDriver {
    driver_desc: String,
    matching_id: String,
    driver_version: String,
}

/// Loads installed display adapter driver versions from the Windows Display class registry key.
fn load_display_driver_versions() -> Vec<RegDisplayDriver> {
    let mut out = Vec::new();
    for idx in 0..16u32 {
        let subkey = format!(
            r"SYSTEM\CurrentControlSet\Control\Class\{{4d36e968-e325-11ce-bfc1-08002be10318}}\{idx:04}"
        );
        let wide = to_wide_null(&subkey);
        let mut hkey: HKEY = ptr::null_mut();
        // SAFETY: Standard read-only registry open under `HKEY_LOCAL_MACHINE`.
        let rc =
            unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, wide.as_ptr(), 0, KEY_READ, &mut hkey) };
        if rc != ERROR_SUCCESS || hkey.is_null() {
            continue;
        }
        let driver_version = read_reg_string(hkey, "DriverVersion");
        let driver_desc = read_reg_string(hkey, "DriverDesc").unwrap_or_default();
        let matching_id = read_reg_string(hkey, "MatchingDeviceId").unwrap_or_default();
        // SAFETY: `hkey` was successfully opened above.
        unsafe {
            RegCloseKey(hkey);
        }
        if let Some(ver) = driver_version
            && !ver.trim().is_empty()
        {
            out.push(RegDisplayDriver {
                driver_desc,
                matching_id: matching_id.to_ascii_uppercase(),
                driver_version: ver.trim().to_owned(),
            });
        }
    }
    out
}

/// Matches a DXGI adapter to its installed display driver version from the registry.
fn match_driver_version(
    drivers: &[RegDisplayDriver],
    vendor_id: u32,
    device_id: u32,
    adapter_name: &str,
) -> Option<Arc<str>> {
    let needle = format!("VEN_{vendor_id:04X}&DEV_{device_id:04X}");
    if let Some(entry) = drivers.iter().find(|d| d.matching_id.contains(&needle)) {
        return Some(Arc::from(entry.driver_version.as_str()));
    }
    if !adapter_name.is_empty()
        && let Some(entry) = drivers
            .iter()
            .find(|d| d.driver_desc.eq_ignore_ascii_case(adapter_name))
    {
        return Some(Arc::from(entry.driver_version.as_str()));
    }
    None
}

#[inline]
fn pack_luid(luid: LUID) -> u64 {
    (u64::from(luid.HighPart as u32) << 32) | u64::from(luid.LowPart)
}

fn format_engine_name(engine_type: i32, friendly_name: &[u16; 32], node_id: u32) -> String {
    match engine_type {
        DXGK_ENGINE_TYPE_3D => "3D".to_owned(),
        DXGK_ENGINE_TYPE_COPY => "Copy".to_owned(),
        DXGK_ENGINE_TYPE_VIDEO_DECODE => "Video Decode".to_owned(),
        DXGK_ENGINE_TYPE_VIDEO_ENCODE => "Video Encode".to_owned(),
        DXGK_ENGINE_TYPE_VIDEO_PROCESSING => "Video Processing".to_owned(),
        DXGK_ENGINE_TYPE_SCENE_ASSEMBLY => "Scene Assembly".to_owned(),
        DXGK_ENGINE_TYPE_OVERLAY => "Overlay".to_owned(),
        DXGK_ENGINE_TYPE_CRYPTO => "Crypto".to_owned(),
        _ => {
            let friendly = decode_wide_nul(friendly_name);
            if !friendly.is_empty() {
                friendly
            } else {
                default_node_name(node_id)
            }
        }
    }
}

fn default_node_name(node_id: u32) -> String {
    match node_id {
        0 => "3D".to_owned(),
        1 => "Copy".to_owned(),
        2 => "Video Decode".to_owned(),
        3 => "Video Encode".to_owned(),
        _ => format!("Engine {node_id}"),
    }
}

fn format_directx_version(wddm_version: i32) -> String {
    let major = wddm_version / 1000;
    let minor = (wddm_version % 1000) / 100;
    if wddm_version >= 2000 {
        format!("12 (WDDM {major}.{minor})")
    } else if wddm_version >= 1200 {
        format!("11 (WDDM {major}.{minor})")
    } else {
        format!("WDDM {major}.{minor}")
    }
}

fn engine_sort_rank(name: &str) -> u8 {
    match name {
        "3D" => 0,
        "Copy" => 1,
        "Video Decode" => 2,
        "Video Encode" => 3,
        "Video Processing" => 4,
        _ => 10,
    }
}

fn decode_wide_nul(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len]).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_power_gate_skips_sleeping_hybrid_dgpu_and_clears_cached_temp() {
        let mut gate = GpuPowerGate::new(true);
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);

        // First active D0 tick allows query and caches temperature.
        assert_eq!(
            gate.evaluate(GpuDevicePowerState::D0, true, t0),
            PerfDataGateDecision::Allow
        );
        gate.record_temperature(Some(52));
        assert_eq!(gate.cached_temperature(), Some(52));

        // Sleeping D1-D3 must ALWAYS skip and clear cached temperature to None.
        assert_eq!(
            gate.evaluate(GpuDevicePowerState::LowPowerD1ToD3, false, t1),
            PerfDataGateDecision::SkipSleeping
        );
        assert_eq!(gate.cached_temperature(), None);

        // Even if node activity flag were somehow true while in D3, D3 still skips.
        assert_eq!(
            gate.evaluate(
                GpuDevicePowerState::LowPowerD1ToD3,
                true,
                t1 + Duration::from_secs(30)
            ),
            PerfDataGateDecision::SkipSleeping
        );
    }

    #[test]
    fn gpu_power_gate_throttles_idle_d0_to_20s_and_allows_active_d0_immediately() {
        let mut gate = GpuPowerGate::new(true);
        let t0 = Instant::now();

        // Initial idle D0 tick is allowed (establishes baseline).
        assert_eq!(
            gate.evaluate(GpuDevicePowerState::D0, false, t0),
            PerfDataGateDecision::Allow
        );
        gate.record_temperature(Some(41));

        // Subsequent idle D0 ticks before 20s must be throttled so dGPU can re-enter D3.
        assert_eq!(
            gate.evaluate(GpuDevicePowerState::D0, false, t0 + Duration::from_secs(5)),
            PerfDataGateDecision::SkipIdleThrottled
        );
        assert_eq!(gate.cached_temperature(), Some(41));
        assert_eq!(
            gate.evaluate(GpuDevicePowerState::D0, false, t0 + Duration::from_secs(19)),
            PerfDataGateDecision::SkipIdleThrottled
        );

        // At 20s elapsed, idle D0 is allowed again.
        let t20 = t0 + HYBRID_IDLE_PERFDATA_INTERVAL;
        assert_eq!(
            gate.evaluate(GpuDevicePowerState::D0, false, t20),
            PerfDataGateDecision::Allow
        );

        // Active D0 is allowed immediately even 1s after the previous query.
        assert_eq!(
            gate.evaluate(GpuDevicePowerState::D0, true, t20 + Duration::from_secs(1)),
            PerfDataGateDecision::Allow
        );
    }

    #[test]
    fn gpu_power_gate_always_allows_non_hybrid_adapters() {
        let mut gate = GpuPowerGate::new(false);
        let t0 = Instant::now();

        assert_eq!(
            gate.evaluate(GpuDevicePowerState::D0, false, t0),
            PerfDataGateDecision::Allow
        );
        assert_eq!(
            gate.evaluate(GpuDevicePowerState::D0, false, t0 + Duration::from_secs(1)),
            PerfDataGateDecision::Allow
        );
        assert_eq!(
            gate.evaluate(
                GpuDevicePowerState::LowPowerD1ToD3,
                false,
                t0 + Duration::from_secs(2)
            ),
            PerfDataGateDecision::Allow
        );
    }

    #[test]
    fn per_process_gpu_delta_math_counter_reset_clamping_and_stale_pruning() {
        let mut prev = HashMap::new();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        let t2 = t1 + Duration::from_secs(1);
        let t3 = t2 + Duration::from_secs(1);

        let p1 = ProcessKey::new(1001, 111_111);
        let p2 = ProcessKey::new(2002, 222_222);
        let k1 = ProcessGpuNodeKey {
            process: p1,
            sequence_number: Some(10),
            adapter_luid: 0xAA,
            node_id: 0,
        };
        let k2 = ProcessGpuNodeKey {
            process: p2,
            sequence_number: Some(20),
            adapter_luid: 0xAA,
            node_id: 0,
        };

        // Cold sample returns 0.
        assert_eq!(compute_process_node_util(&mut prev, k1, 5_000_000, t0), 0);
        assert_eq!(compute_process_node_util(&mut prev, k2, 1_000_000, t0), 0);

        // 2_500_000 * 100ns = 0.25s over 1.0s => 250 permille (25.0%).
        assert_eq!(compute_process_node_util(&mut prev, k1, 7_500_000, t1), 250);

        // Overshoot (>100%) clamps cleanly to 1000 permille.
        assert_eq!(
            compute_process_node_util(&mut prev, k2, 20_000_000, t1),
            1_000
        );

        // Counter reset (running_time drops below previous): returns 0 instead of underflowing.
        assert_eq!(compute_process_node_util(&mut prev, k1, 100_000, t2), 0);

        // Recovery after reset computes accurate delta from new baseline.
        assert_eq!(compute_process_node_util(&mut prev, k1, 5_100_000, t3), 500);

        // Stale-key pruning removes exited processes while keeping live ones.
        let mut live = HashSet::new();
        live.insert(p1);
        prune_stale_process_gpu_nodes(&mut prev, &live);
        assert!(prev.contains_key(&k1));
        assert!(!prev.contains_key(&k2));
    }
}
