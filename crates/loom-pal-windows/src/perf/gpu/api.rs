//! Dynamic `dxgi.dll` + `gdi32.dll` (`D3DKMT`) FFI wrappers, RAII guards, and engine naming.

use std::ffi::c_void;
use std::mem::{size_of, transmute, zeroed};
use std::sync::Arc;

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
use windows_sys::Win32::Foundation::{LUID, STATUS_SUCCESS};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::core::GUID;

use crate::handle::{OwnedModule, decode_wide_nul, to_wide_null};

/// Maximum D3DKMT segments/nodes iterated per adapter to defend against malformed driver reports.
pub(super) const MAX_D3DKMT_ITEMS: u32 = 128;
/// `DXGI_ADAPTER_FLAG_SOFTWARE` bit indicating Microsoft Basic Render Driver (WARP).
pub(super) const DXGI_ADAPTER_FLAG_SOFTWARE: u32 = 2;

/// `D3DKMT_ADAPTERTYPE` bit 0: `RenderSupported`.
const ADAPTER_TYPE_RENDER_SUPPORTED: u32 = 1 << 0;
/// `D3DKMT_ADAPTERTYPE` bit 2: `SoftwareDevice`.
const ADAPTER_TYPE_SOFTWARE_DEVICE: u32 = 1 << 2;
/// `D3DKMT_ADAPTERTYPE` bit 4: `HybridDiscrete` (switchable/Optimus laptop dGPU).
const ADAPTER_TYPE_HYBRID_DISCRETE: u32 = 1 << 4;
/// `D3DKMT_ADAPTERTYPE` bit 12: `IndirectDisplayDevice` (virtual/remote display mirrors).
const ADAPTER_TYPE_INDIRECT_DISPLAY: u32 = 1 << 12;

/// `IID_IDXGIFactory1` (`{770aae78-f26f-4dba-a829-253c83d1b387}`).
pub(super) const IID_IDXGIFACTORY1: GUID = GUID {
    data1: 0x770a_ae78,
    data2: 0xf26f,
    data3: 0x4dba,
    data4: [0xa8, 0x29, 0x25, 0x3c, 0x83, 0xd1, 0xb3, 0x87],
};

type FarProc = unsafe extern "system" fn() -> isize;
pub(super) type FnCreateDxgiFactory1 =
    unsafe extern "system" fn(riid: *const GUID, pp_factory: *mut *mut c_void) -> i32;
pub(super) type FnD3dkmtQueryStatistics =
    unsafe extern "system" fn(param0: *const D3DKMT_QUERYSTATISTICS) -> i32;
pub(super) type FnD3dkmtQueryAdapterInfo =
    unsafe extern "system" fn(param0: *mut D3DKMT_QUERYADAPTERINFO) -> i32;
type FnD3dkmtOpenAdapterFromLuid =
    unsafe extern "system" fn(param0: *mut D3DKMT_OPENADAPTERFROMLUID) -> i32;
type FnD3dkmtCloseAdapter = unsafe extern "system" fn(param0: *const D3DKMT_CLOSEADAPTER) -> i32;

/// Exact Windows `DXGI_ADAPTER_DESC1` ABI layout.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct DxgiAdapterDesc1 {
    pub(super) description: [u16; 128],
    pub(super) vendor_id: u32,
    pub(super) device_id: u32,
    pub(super) sub_sys_id: u32,
    pub(super) revision: u32,
    pub(super) dedicated_video_memory: usize,
    pub(super) dedicated_system_memory: usize,
    pub(super) shared_system_memory: usize,
    pub(super) adapter_luid: LUID,
    pub(super) flags: u32,
}

impl Default for DxgiAdapterDesc1 {
    fn default() -> Self {
        // SAFETY: `DxgiAdapterDesc1` is POD.
        unsafe { zeroed() }
    }
}

/// RAII wrapper for an `IUnknown`-derived COM pointer calling `Release()` on drop.
pub(super) struct ComPtr(pub(super) *mut c_void);

impl ComPtr {
    #[inline]
    pub(super) fn as_ptr(&self) -> *mut c_void {
        self.0
    }

    /// Reads the `slot`-th raw function pointer from the COM object's vtable.
    #[inline]
    pub(super) unsafe fn vtable_entry(&self, slot: usize) -> *const c_void {
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

/// RAII guard for a `D3DKMT` adapter handle ensuring `D3DKMTCloseAdapter` always runs on drop.
pub(super) struct D3dkmtAdapterGuard<'a> {
    api: &'a GpuApi,
    handle: u32,
}

impl D3dkmtAdapterGuard<'_> {
    #[inline]
    pub(super) fn handle(&self) -> u32 {
        self.handle
    }
}

impl Drop for D3dkmtAdapterGuard<'_> {
    fn drop(&mut self) {
        if self.handle != 0
            && let Some(close) = self.api.d3dkmt_close_adapter
        {
            let arg = D3DKMT_CLOSEADAPTER {
                hAdapter: self.handle,
            };
            // SAFETY: Paired with a successful `open_adapter`.
            unsafe {
                let _ = close(&arg);
            }
        }
    }
}

/// Dynamically resolved `dxgi.dll` and `gdi32.dll` D3DKMT entry points.
pub(super) struct GpuApi {
    _dxgi_dll: OwnedModule,
    pub(super) create_dxgi_factory1: FnCreateDxgiFactory1,
    pub(super) d3dkmt_query_statistics: FnD3dkmtQueryStatistics,
    pub(super) d3dkmt_query_adapter_info: Option<FnD3dkmtQueryAdapterInfo>,
    d3dkmt_open_adapter_from_luid: Option<FnD3dkmtOpenAdapterFromLuid>,
    d3dkmt_close_adapter: Option<FnD3dkmtCloseAdapter>,
}

// Function pointers resolved from system DLLs are safe to move to the fast sampling thread.
unsafe impl Send for GpuApi {}

impl GpuApi {
    pub(super) fn load() -> Option<Self> {
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

        let dxgi_dll = OwnedModule::load("dxgi.dll").ok()?;
        let create_factory_ptr = dxgi_dll.proc_raw(b"CreateDXGIFactory1\0")?;

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

    pub(super) fn open_adapter(&self, luid: LUID) -> Option<D3dkmtAdapterGuard<'_>> {
        let open = self.d3dkmt_open_adapter_from_luid?;
        let mut arg = D3DKMT_OPENADAPTERFROMLUID {
            AdapterLuid: luid,
            hAdapter: 0,
        };
        // SAFETY: Valid `D3DKMT_OPENADAPTERFROMLUID` struct passed to `D3DKMTOpenAdapterFromLuid`.
        let status = unsafe { open(&mut arg) };
        if status == STATUS_SUCCESS && arg.hAdapter != 0 {
            Some(D3dkmtAdapterGuard {
                api: self,
                handle: arg.hAdapter,
            })
        } else {
            None
        }
    }
}

/// Static D3DKMT adapter attributes cached per hardware GPU.
pub(super) struct StaticAdapterInfo {
    pub(super) node_names: Vec<Arc<str>>,
    pub(super) pci_location: Option<Arc<str>>,
    pub(super) pci_bdf: Option<(u32, u32, u32)>,
    pub(super) directx_version: Option<Arc<str>>,
    pub(super) is_hybrid_discrete: bool,
}

/// Queries D3DKMT adapter node count and memory segment count.
pub(super) fn query_adapter_topology(api: &GpuApi, luid: LUID) -> (u32, u32) {
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
pub(super) fn query_static_adapter_info(
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

    let Some(adapter_guard) = api.open_adapter(luid) else {
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
    let handle = adapter_guard.handle();

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
pub(super) fn query_adapter_memory(
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
pub(super) fn query_node_running_time(api: &GpuApi, luid: LUID, node_id: u32) -> Option<u64> {
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
pub(super) fn query_adapter_temperature(api: &GpuApi, luid: LUID) -> Option<i16> {
    let query_info = api.d3dkmt_query_adapter_info?;
    let adapter_guard = api.open_adapter(luid)?;

    let mut perf = D3DKMT_ADAPTER_PERFDATA::default();
    let mut qai = D3DKMT_QUERYADAPTERINFO {
        hAdapter: adapter_guard.handle(),
        Type: KMTQAITYPE_ADAPTERPERFDATA,
        pPrivateDriverData: (&mut perf as *mut D3DKMT_ADAPTER_PERFDATA).cast::<c_void>(),
        PrivateDriverDataSize: size_of::<D3DKMT_ADAPTER_PERFDATA>() as u32,
    };
    // SAFETY: `qai` points to valid `D3DKMT_ADAPTER_PERFDATA` storage.
    let status = unsafe { query_info(&mut qai) };

    // `D3DKMT_ADAPTER_PERFDATA.Temperature` is reported in deci-Celsius (tenths of °C).
    if status == STATUS_SUCCESS && perf.Temperature >= 10 && perf.Temperature <= 1500 {
        Some((perf.Temperature / 10) as i16)
    } else {
        None
    }
}

#[inline]
pub(super) fn pack_luid(luid: LUID) -> u64 {
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

pub(super) fn engine_sort_rank(name: &str) -> u8 {
    match name {
        "3D" => 0,
        "Copy" => 1,
        "Video Decode" => 2,
        "Video Encode" => 3,
        "Video Processing" => 4,
        _ => 10,
    }
}
