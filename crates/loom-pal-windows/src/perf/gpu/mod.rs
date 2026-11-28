//! Windows fast-lane GPU adapter & per-process telemetry via dynamic `dxgi.dll`
//! (`CreateDXGIFactory1`), `gdi32.dll` (`D3DKMTQueryStatistics` / `D3DKMTQueryAdapterInfo`),
//! and `cfgmgr32.dll` (`DEVPKEY_Device_PowerData`), free of WMI and PDH.
//!
//! Gracefully handles headless CI VMs and systems without hardware GPUs without panicking
//! or failing fast-lane performance ticks, defers `dxgi.dll` loading off cold-start construction,
//! and power-gates `KMTQAITYPE_ADAPTERPERFDATA` so sleeping hybrid discrete GPUs in `D1`–`D3`
//! are never woken up.

mod api;
mod power;
mod process;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::mem::transmute;
use std::ptr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use loom_types::{GpuAdapterSample, GpuEngineSample, ProcessHotSample, ProcessKey};
use windows_sys::Win32::Foundation::LUID;

use crate::handle::decode_wide_nul;
use crate::perf::nvml::{NvmlState, NvmlTelemetrySample, PCI_VENDOR_NVIDIA};

use self::api::{
    ComPtr, DXGI_ADAPTER_FLAG_SOFTWARE, DxgiAdapterDesc1, GpuApi, IID_IDXGIFACTORY1,
    engine_sort_rank, pack_luid, query_adapter_memory, query_adapter_temperature,
    query_adapter_topology, query_node_running_time, query_static_adapter_info,
};
use self::power::{
    GpuDevicePowerState, GpuPowerGate, PerfDataGateDecision, discover_display_pnp_devices,
    load_display_driver_versions, match_driver_version, match_pnp_display_instance,
    query_gpu_device_power_state,
};
use self::process::{ProcessGpuNodeKey, ProcessGpuProbeState, sample_process_gpu_metrics_inner};

/// Refresh cached DXGI adapter inventory every 30 seconds to detect hot-plugged eGPUs.
const ADAPTER_CACHE_TTL: Duration = Duration::from_secs(30);

/// Lazy `dxgi.dll` / `gdi32.dll` API initialization state keeping `GpuPerfSampler::new()`
/// completely off the synchronous cold-start critical path.
enum GpuApiState {
    Uninitialized,
    Ready(GpuApi),
    Unavailable,
}

impl GpuApiState {
    fn get_or_load(&mut self) -> Option<&GpuApi> {
        if matches!(self, Self::Uninitialized) {
            *self = match GpuApi::load() {
                Some(api) => Self::Ready(api),
                None => Self::Unavailable,
            };
        }
        match self {
            Self::Ready(api) => Some(api),
            Self::Uninitialized | Self::Unavailable => None,
        }
    }
}

/// Cached static GPU adapter inventory entry.
#[derive(Clone)]
pub(super) struct CachedGpuAdapter {
    pub(super) index: u32,
    pub(super) luid_raw: LUID,
    pub(super) luid: u64,
    pub(super) vendor_id: u32,
    pub(super) name: Arc<str>,
    pub(super) driver_version: Option<Arc<str>>,
    pub(super) directx_version: Option<Arc<str>>,
    pub(super) pci_location: Option<Arc<str>>,
    pub(super) pci_bdf: Option<(u32, u32, u32)>,
    pub(super) dedicated_total_bytes: u64,
    pub(super) shared_total_bytes: u64,
    pub(super) node_count: u32,
    pub(super) segment_count: u32,
    /// Bitmask of segments where `SegmentInformation.Aperture != 0` (shared system memory).
    pub(super) aperture_segment_mask: u128,
    pub(super) node_names: Vec<Arc<str>>,
    /// True when `D3DKMT_ADAPTERTYPE.HybridDiscrete` is set (laptop switchable dGPU).
    pub(super) is_hybrid_discrete: bool,
    /// Matched NUL-terminated PnP Device Instance ID for `DEVPKEY_Device_PowerData` checks.
    pub(super) pnp_instance_id_wide: Option<Vec<u16>>,
}

/// Previous per-node GPU running time counter (`100 ns` units) and wall timestamp.
#[derive(Debug, Clone, Copy)]
pub(super) struct PrevNodeSample {
    pub(super) running_time_100ns: u64,
    pub(super) sampled_at: Instant,
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
    api: GpuApiState,
    nvml: NvmlState,
    adapters: Vec<CachedGpuAdapter>,
    last_discovery: Option<Instant>,
    prev_nodes: HashMap<(u64, u32), PrevNodeSample>,
    power_gates: HashMap<u64, GpuPowerGate>,
    /// Reusable per-adapter engine-aggregation scratch avoiding `BTreeMap` allocations per tick.
    engine_scratch: Vec<(Arc<str>, u16)>,
    /// Reusable per-process GPU engine running-time delta map (zero steady-state reallocation).
    prev_proc_nodes: HashMap<ProcessGpuNodeKey, PrevNodeSample>,
    /// Reusable per-process negative-probe & active-adapter state cache.
    proc_probe_state: HashMap<ProcessKey, ProcessGpuProbeState>,
    /// Monotonic counter of per-process GPU sampling ticks.
    proc_sample_tick: u64,
    /// Reusable set of live `ProcessKey`s observed on the current tick for stale-key pruning.
    live_proc_keys: HashSet<ProcessKey>,
}

impl Default for GpuPerfSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl GpuPerfSampler {
    /// Creates a new GPU sampler without loading `dxgi.dll` synchronously on cold start.
    pub(crate) fn new() -> Self {
        Self {
            api: GpuApiState::Uninitialized,
            nvml: NvmlState::Uninitialized,
            adapters: Vec::new(),
            last_discovery: None,
            prev_nodes: HashMap::new(),
            power_gates: HashMap::new(),
            engine_scratch: Vec::new(),
            prev_proc_nodes: HashMap::new(),
            proc_probe_state: HashMap::new(),
            proc_sample_tick: 0,
            live_proc_keys: HashSet::new(),
        }
    }

    /// Ensures `GpuApi` is loaded and the cached hardware GPU adapter inventory is populated.
    fn ensure_adapters_discovered(&mut self, now: Instant) -> bool {
        if self.api.get_or_load().is_none() {
            return false;
        }
        let needs_discovery = self
            .last_discovery
            .is_none_or(|t| now.saturating_duration_since(t) >= ADAPTER_CACHE_TTL);
        if needs_discovery {
            self.discover_adapters();
            self.last_discovery = Some(now);
        }
        true
    }

    /// Samples per-process dedicated VRAM, shared GPU memory, and GPU engine utilization
    /// directly into `samples` using pure kernel-mode `gdi32!D3DKMTQueryStatistics`
    /// with short-lived, `create_time_100ns`-verified handles.
    pub(crate) fn sample_process_gpu_metrics(&mut self, samples: &mut [ProcessHotSample<()>]) {
        if samples.is_empty() {
            return;
        }
        let now = Instant::now();
        if !self.ensure_adapters_discovered(now) || self.adapters.is_empty() {
            return;
        }
        let GpuApiState::Ready(ref api) = self.api else {
            return;
        };

        self.proc_sample_tick = self.proc_sample_tick.saturating_add(1);
        sample_process_gpu_metrics_inner(
            api,
            &self.adapters,
            samples,
            now,
            self.proc_sample_tick,
            &mut self.prev_proc_nodes,
            &mut self.proc_probe_state,
            &mut self.live_proc_keys,
        );
    }

    /// Samples all hardware GPU adapters into `out` and returns the primary GPU summary.
    pub(crate) fn sample(&mut self, out: &mut Vec<GpuAdapterSample>) -> PrimaryGpuSummary {
        out.clear();
        let now = Instant::now();
        if !self.ensure_adapters_discovered(now) {
            return PrimaryGpuSummary::default();
        }
        let GpuApiState::Ready(ref api) = self.api else {
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

            // Sample per-node running time deltas and aggregate by unique engine name
            // using our reusable small scratch Vec instead of allocating a BTreeMap per tick.
            self.engine_scratch.clear();
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
                    if let Some((_, existing_util)) = self
                        .engine_scratch
                        .iter_mut()
                        .find(|(name, _)| *name == engine_name)
                    {
                        *existing_util = (*existing_util).max(util);
                    } else {
                        self.engine_scratch.push((engine_name, util));
                    }
                }
            }

            let mut engines: Vec<GpuEngineSample> = self
                .engine_scratch
                .drain(..)
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
            let gate_decision = gate.evaluate(power_state, any_node_activity, now);
            let d3_asleep = matches!(gate_decision, PerfDataGateDecision::SkipSleeping);
            let (temperature_celsius, nvml_sample) = match gate_decision {
                PerfDataGateDecision::Allow => {
                    let nvml_sample = if adapter.vendor_id == PCI_VENDOR_NVIDIA
                        && let Some(bdf) = adapter.pci_bdf
                    {
                        self.nvml.query_active_device(bdf)
                    } else {
                        NvmlTelemetrySample::default()
                    };
                    let temp = query_adapter_temperature(api, adapter.luid_raw)
                        .or(nvml_sample.temperature_celsius);
                    gate.record_temperature(temp);
                    gate.record_nvml(nvml_sample);
                    (temp, nvml_sample)
                }
                PerfDataGateDecision::SkipSleeping => (None, NvmlTelemetrySample::default()),
                PerfDataGateDecision::SkipIdleThrottled => {
                    (gate.cached_temperature(), gate.cached_nvml())
                }
            };

            out.push(GpuAdapterSample {
                index: adapter.index,
                luid: adapter.luid,
                name: Arc::clone(&adapter.name),
                driver_version: adapter.driver_version.clone(),
                directx_version: adapter.directx_version.clone(),
                pci_location: adapter.pci_location.clone(),
                temperature_celsius,
                power_draw_mw: nvml_sample.power_draw_mw,
                fan_speed_percent: nvml_sample.fan_speed_percent,
                core_clock_mhz: nvml_sample.core_clock_mhz,
                memory_clock_mhz: nvml_sample.memory_clock_mhz,
                util_permille: adapter_max_util,
                dedicated_used_bytes: dedicated_used_clamped,
                dedicated_total_bytes: dedicated_total,
                shared_used_bytes: shared_used_clamped,
                shared_total_bytes: shared_total,
                d3_asleep,
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
        let GpuApiState::Ready(ref api) = self.api else {
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
                vendor_id: desc.vendor_id,
                name,
                driver_version,
                directx_version: static_info.directx_version,
                pci_location: static_info.pci_location,
                pci_bdf: static_info.pci_bdf,
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

        // Retain power-gate state only for currently present adapter LUIDs, and clear
        // cached per-process GPU probe state and NVML handles so hot-plugged GPUs are probed.
        self.power_gates
            .retain(|luid, _| self.adapters.iter().any(|a| a.luid == *luid));
        self.proc_probe_state.clear();
        self.nvml.clear_device_cache();
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
