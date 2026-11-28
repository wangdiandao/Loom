//! Hybrid dGPU `D1`–`D3` power-state gating (`cfgmgr32.dll` `DEVPKEY_Device_PowerData`)
//! and display-class registry driver version matching.

use std::mem::{size_of, transmute};
use std::ptr;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{DEVPROPKEY, ERROR_SUCCESS};
use windows_sys::Win32::System::Power::{
    CM_POWER_DATA, PowerDeviceD0, PowerDeviceD1, PowerDeviceD2, PowerDeviceD3,
};
use windows_sys::Win32::System::Registry::{HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegOpenKeyExW};
use windows_sys::core::GUID;

use crate::handle::{OwnedModule, OwnedRegKey, decode_wide_nul, to_wide_null};
use crate::perf::cpu::read_reg_string;
use crate::perf::nvml::NvmlTelemetrySample;

/// Throttle interval for `KMTQAITYPE_ADAPTERPERFDATA` when a hybrid discrete GPU is in idle `D0`
/// so runtime power management can transition back to `D3`.
pub(crate) const HYBRID_IDLE_PERFDATA_INTERVAL: Duration = Duration::from_secs(20);

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

type FarProc = unsafe extern "system" fn() -> isize;
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
    last_nvml: NvmlTelemetrySample,
}

impl GpuPowerGate {
    /// Creates a power gate for an adapter.
    pub(crate) fn new(is_hybrid_discrete: bool) -> Self {
        Self {
            is_hybrid_discrete,
            last_perf_query: None,
            last_temperature: None,
            last_nvml: NvmlTelemetrySample::default(),
        }
    }

    /// Evaluates whether `KMTQAITYPE_ADAPTERPERFDATA` / NVML may be called at `now`.
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
            // Never open the adapter or query perfdata/NVML while sleeping.
            self.last_temperature = None;
            self.last_nvml = NvmlTelemetrySample::default();
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

    /// Updates cached supplementary NVML readings after an allowed query.
    #[inline]
    pub(crate) fn record_nvml(&mut self, sample: NvmlTelemetrySample) {
        self.last_nvml = sample;
    }

    /// Returns the most recent cached NVML readings (cleared when entering `D1`–`D3`).
    #[inline]
    pub(crate) fn cached_nvml(&self) -> NvmlTelemetrySample {
        self.last_nvml
    }
}

/// Dynamically resolved `cfgmgr32.dll` PnP power-state & display-interface functions.
struct GpuCfgMgrApi {
    _module: OwnedModule,
    cm_locate_dev_node_w: FnCmLocateDevNodeW,
    cm_get_dev_node_property_w: FnCmGetDevNodePropertyW,
    cm_get_device_interface_list_size_w: Option<FnCmGetDeviceInterfaceListSizeW>,
    cm_get_device_interface_list_w: Option<FnCmGetDeviceInterfaceListW>,
    cm_get_device_interface_property_w: Option<FnCmGetDeviceInterfacePropertyW>,
}

fn gpu_cfgmgr_api() -> Option<&'static GpuCfgMgrApi> {
    static API: OnceLock<Option<GpuCfgMgrApi>> = OnceLock::new();
    API.get_or_init(|| {
        let module = OwnedModule::load("cfgmgr32.dll").ok()?;
        let locate = module.proc_raw(b"CM_Locate_DevNodeW\0")?;
        let devnode_prop = module.proc_raw(b"CM_Get_DevNode_PropertyW\0")?;
        let list_size = module.proc_raw(b"CM_Get_Device_Interface_List_SizeW\0");
        let list = module.proc_raw(b"CM_Get_Device_Interface_ListW\0");
        let iface_prop = module.proc_raw(b"CM_Get_Device_Interface_PropertyW\0");

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
pub(super) struct PnpDisplayDevice {
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
pub(super) fn discover_display_pnp_devices() -> Vec<PnpDisplayDevice> {
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
pub(super) fn query_gpu_device_power_state(dev_instance_id_wide: &[u16]) -> GpuDevicePowerState {
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

/// Matches a DXGI adapter to its PnP display Device Instance ID by exact PCI BDF or VEN/DEV ID.
pub(super) fn match_pnp_display_instance(
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

/// Cached registry display-adapter driver record.
pub(super) struct RegDisplayDriver {
    driver_desc: String,
    matching_id: String,
    driver_version: String,
}

/// Loads installed display adapter driver versions from the Windows Display class registry key.
pub(super) fn load_display_driver_versions() -> Vec<RegDisplayDriver> {
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
        if rc != ERROR_SUCCESS {
            continue;
        }
        let Some(key) = OwnedRegKey::from_raw(hkey) else {
            continue;
        };
        let driver_version = read_reg_string(key.as_raw(), "DriverVersion");
        let driver_desc = read_reg_string(key.as_raw(), "DriverDesc").unwrap_or_default();
        let matching_id = read_reg_string(key.as_raw(), "MatchingDeviceId").unwrap_or_default();
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
pub(super) fn match_driver_version(
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
