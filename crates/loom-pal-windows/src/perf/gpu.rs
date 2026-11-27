//! Windows fast-lane GPU adapter telemetry via dynamic `dxgi.dll` (`CreateDXGIFactory1`)
//! and `gdi32.dll` (`D3DKMTQueryStatistics` / `D3DKMTQueryAdapterInfo`), free of WMI and PDH.
//!
//! Gracefully handles headless CI VMs and systems without hardware GPUs without panicking
//! or failing fast-lane performance ticks.

use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::mem::{size_of, transmute, zeroed};
use std::ptr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use loom_types::{GpuAdapterSample, GpuEngineSample};
use windows_sys::Wdk::Graphics::Direct3D::{
    D3DKMT_ADAPTER_PERFDATA, D3DKMT_ADAPTERADDRESS, D3DKMT_ADAPTERTYPE, D3DKMT_CLOSEADAPTER,
    D3DKMT_NODEMETADATA, D3DKMT_OPENADAPTERFROMLUID, D3DKMT_QUERYADAPTERINFO,
    D3DKMT_QUERYSTATISTICS, D3DKMT_QUERYSTATISTICS_ADAPTER, D3DKMT_QUERYSTATISTICS_NODE,
    D3DKMT_QUERYSTATISTICS_SEGMENT, DXGK_ENGINE_TYPE_3D, DXGK_ENGINE_TYPE_COPY,
    DXGK_ENGINE_TYPE_CRYPTO, DXGK_ENGINE_TYPE_OVERLAY, DXGK_ENGINE_TYPE_SCENE_ASSEMBLY,
    DXGK_ENGINE_TYPE_VIDEO_DECODE, DXGK_ENGINE_TYPE_VIDEO_ENCODE,
    DXGK_ENGINE_TYPE_VIDEO_PROCESSING, KMTQAITYPE_ADAPTERADDRESS, KMTQAITYPE_ADAPTERPERFDATA,
    KMTQAITYPE_ADAPTERTYPE, KMTQAITYPE_DRIVERVERSION, KMTQAITYPE_NODEMETADATA,
};
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, FreeLibrary, HMODULE, LUID, STATUS_SUCCESS};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegOpenKeyExW,
};
use windows_sys::core::GUID;

use crate::handle::to_wide_null;
use crate::perf::cpu::read_reg_string;

/// Refresh cached DXGI adapter inventory every 30 seconds to detect hot-plugged eGPUs.
const ADAPTER_CACHE_TTL: Duration = Duration::from_secs(30);
/// Maximum D3DKMT segments/nodes iterated per adapter to defend against malformed driver reports.
const MAX_D3DKMT_ITEMS: u32 = 128;
/// `DXGI_ADAPTER_FLAG_SOFTWARE` bit indicating Microsoft Basic Render Driver (WARP).
const DXGI_ADAPTER_FLAG_SOFTWARE: u32 = 2;

/// `D3DKMT_ADAPTERTYPE` bit 0: `RenderSupported`.
const ADAPTER_TYPE_RENDER_SUPPORTED: u32 = 1 << 0;
/// `D3DKMT_ADAPTERTYPE` bit 2: `SoftwareDevice`.
const ADAPTER_TYPE_SOFTWARE_DEVICE: u32 = 1 << 2;
/// `D3DKMT_ADAPTERTYPE` bit 12: `IndirectDisplayDevice` (virtual/remote display mirrors).
const ADAPTER_TYPE_INDIRECT_DISPLAY: u32 = 1 << 12;

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
    node_names: Vec<Arc<str>>,
}

/// Previous per-node GPU running time counter (`100 ns` units) and wall timestamp.
#[derive(Debug, Clone, Copy)]
struct PrevNodeSample {
    running_time_100ns: u64,
    sampled_at: Instant,
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
        }
    }

    /// Samples all hardware GPU adapters into `out` and returns the primary GPU summary.
    pub(crate) fn sample(&mut self, out: &mut Vec<GpuAdapterSample>) -> PrimaryGpuSummary {
        out.clear();
        if self.api.is_none() {
            return PrimaryGpuSummary::default();
        }

        let now = Instant::now();
        let needs_discovery = self
            .last_discovery
            .is_none_or(|t| now.saturating_duration_since(t) >= ADAPTER_CACHE_TTL);
        if needs_discovery {
            self.discover_adapters();
            self.last_discovery = Some(now);
        }

        let Some(ref api) = self.api else {
            return PrimaryGpuSummary::default();
        };

        for adapter in &self.adapters {
            let (dedicated_used, shared_used, seg_ded_limit, seg_shared_limit) =
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

            for node_id in 0..adapter.node_count {
                if let Some(running_time_100ns) =
                    query_node_running_time(api, adapter.luid_raw, node_id)
                {
                    let key = (adapter.luid, node_id);
                    let util =
                        compute_node_util(&mut self.prev_nodes, key, running_time_100ns, now);
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

            let temperature_celsius = query_adapter_temperature(api, adapter.luid_raw);

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

    /// Enumerates hardware DXGI adapters and populates static D3DKMT + registry metadata.
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
            let driver_version =
                match_driver_version(&reg_drivers, desc.vendor_id, desc.device_id, &name_str);

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
                node_names: static_info.node_names,
            });
            display_idx += 1;
        }
    }
}

/// Static D3DKMT adapter attributes cached per hardware GPU.
struct StaticAdapterInfo {
    node_names: Vec<Arc<str>>,
    pci_location: Option<Arc<str>>,
    directx_version: Option<Arc<str>>,
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
            directx_version: None,
        });
    };

    let Some(handle) = api.open_adapter_handle(luid) else {
        for n in 0..node_count {
            node_names.push(Arc::from(default_node_name(n)));
        }
        return Some(StaticAdapterInfo {
            node_names,
            pci_location: None,
            directx_version: None,
        });
    };

    // 0. Filter out software-only, indirect display, or display-only non-render adapters.
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
    let pci_location = if unsafe { query_info(&mut qai_pci) } == STATUS_SUCCESS
        && pci_addr.BusNumber != u32::MAX
        && pci_addr.DeviceNumber <= 0xFF
        && pci_addr.FunctionNumber <= 0xFF
    {
        Some(Arc::from(format!(
            "PCI bus {}, device {}, function {}",
            pci_addr.BusNumber, pci_addr.DeviceNumber, pci_addr.FunctionNumber
        )))
    } else {
        None
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
        directx_version,
    })
}

/// Queries dedicated & shared video memory usage and commit limits across all D3DKMT segments.
fn query_adapter_memory(api: &GpuApi, luid: LUID, segment_count: u32) -> (u64, u64, u64, u64) {
    let mut dedicated_used: u64 = 0;
    let mut shared_used: u64 = 0;
    let mut dedicated_limit: u64 = 0;
    let mut shared_limit: u64 = 0;

    for seg_id in 0..segment_count {
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
            shared_used = shared_used.saturating_add(used);
            shared_limit = shared_limit.saturating_add(seg.CommitLimit);
        }
    }

    (dedicated_used, shared_used, dedicated_limit, shared_limit)
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

/// Computes single-node GPU engine utilization in permille (`0..=1000`).
fn compute_node_util(
    prev_map: &mut HashMap<(u64, u32), PrevNodeSample>,
    key: (u64, u32),
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

    let d_run = running_time_100ns.saturating_sub(prev.running_time_100ns);
    ((u128::from(d_run) * 1_000) / dt_100ns).min(1_000) as u16
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
