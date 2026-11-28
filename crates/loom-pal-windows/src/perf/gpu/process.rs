//! Per-process kernel-mode D3DKMT GPU memory & utilization sampling with negative caching.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use loom_pal_api::PalError;
use loom_types::{ProcessCategory, ProcessHotSample, ProcessKey};
use windows_sys::Wdk::Graphics::Direct3D::{
    D3DKMT_QUERYSTATISTICS, D3DKMT_QUERYSTATISTICS_PROCESS, D3DKMT_QUERYSTATISTICS_PROCESS_ADAPTER,
    D3DKMT_QUERYSTATISTICS_PROCESS_NODE, D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT,
};
use windows_sys::Win32::Foundation::STATUS_SUCCESS;
use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;

use crate::handle::{open_verified_process, query_process_sequence_number};

use super::api::GpuApi;
use super::{CachedGpuAdapter, PrevNodeSample};

/// Minimum tick stride before re-probing an `App`-category process that previously had no GPU context.
pub(super) const APP_NOGPU_RECHECK_STRIDE: u64 = 3;
/// Minimum tick stride before re-probing a `Background`/`System` process that previously had no GPU context.
pub(super) const BG_NOGPU_RECHECK_STRIDE: u64 = 10;
/// Minimum tick stride before re-scanning all adapters for a GPU-active process (detects secondary GPU attachment).
pub(super) const ACTIVE_FULL_RECHECK_STRIDE: u64 = 8;

/// Unique key for per-process, per-adapter, per-node GPU running-time deltas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct ProcessGpuNodeKey {
    pub(super) process: ProcessKey,
    pub(super) sequence_number: Option<u64>,
    pub(super) adapter_luid: u64,
    pub(super) node_id: u32,
}

/// Cached per-process D3DKMT attachment state eliminating redundant `OpenProcess` +
/// `D3DKMTQueryStatistics` kernel roundtrips on hundreds of non-GPU background processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProcessGpuProbeState {
    /// `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` returned `ERROR_ACCESS_DENIED`.
    AccessDenied,
    /// Process had no DxgKrnl context on any adapter; skip probing until `next_recheck_tick`,
    /// unless promoted from non-`App` to `ProcessCategory::App`.
    NoGpuContext {
        next_recheck_tick: u64,
        was_app: bool,
    },
    /// Process had an active DxgKrnl context on adapters in `adapter_mask`; probe every tick.
    Active {
        adapter_mask: u64,
        full_recheck_tick: u64,
    },
}

/// Decides whether a process should be probed on `tick` and which adapter bitmask to scan.
///
/// Returns `None` to skip probing on this tick, `Some(None)` to scan all adapters, or
/// `Some(Some(mask))` to scan only adapters whose bit is set in `mask`.
#[inline]
pub(super) fn should_probe_process(
    state: Option<ProcessGpuProbeState>,
    category: ProcessCategory,
    tick: u64,
) -> Option<Option<u64>> {
    match state {
        None => Some(None),
        Some(ProcessGpuProbeState::AccessDenied) => None,
        Some(ProcessGpuProbeState::NoGpuContext {
            next_recheck_tick,
            was_app,
        }) => {
            if (category == ProcessCategory::App && !was_app) || tick >= next_recheck_tick {
                Some(None)
            } else {
                None
            }
        }
        Some(ProcessGpuProbeState::Active {
            adapter_mask,
            full_recheck_tick,
        }) => {
            if adapter_mask == 0 || tick >= full_recheck_tick {
                Some(None)
            } else {
                Some(Some(adapter_mask))
            }
        }
    }
}

/// Computes the next [`ProcessGpuProbeState`] after probing a process on `tick`.
#[inline]
pub(super) fn next_probe_state_after_scan(
    attached_mask: u64,
    attached_any: bool,
    pid: u32,
    category: ProcessCategory,
    tick: u64,
) -> ProcessGpuProbeState {
    if attached_any {
        let stagger = u64::from(pid) % ACTIVE_FULL_RECHECK_STRIDE;
        ProcessGpuProbeState::Active {
            adapter_mask: attached_mask,
            full_recheck_tick: tick.saturating_add(ACTIVE_FULL_RECHECK_STRIDE + stagger),
        }
    } else {
        let is_app = category == ProcessCategory::App;
        let stride = if is_app {
            APP_NOGPU_RECHECK_STRIDE
        } else {
            BG_NOGPU_RECHECK_STRIDE
        };
        let stagger = u64::from(pid) % stride;
        ProcessGpuProbeState::NoGpuContext {
            next_recheck_tick: tick.saturating_add(stride + stagger),
            was_app: is_app,
        }
    }
}

/// Samples per-process dedicated VRAM, shared GPU memory, and GPU engine utilization
/// directly into `samples` using short-lived, `create_time_100ns`-verified handles.
#[allow(clippy::too_many_arguments)]
pub(super) fn sample_process_gpu_metrics_inner(
    api: &GpuApi,
    adapters: &[CachedGpuAdapter],
    samples: &mut [ProcessHotSample<()>],
    now: Instant,
    tick: u64,
    prev_proc_nodes: &mut HashMap<ProcessGpuNodeKey, PrevNodeSample>,
    proc_probe_state: &mut HashMap<ProcessKey, ProcessGpuProbeState>,
    live_proc_keys: &mut HashSet<ProcessKey>,
) {
    live_proc_keys.clear();

    for sample in samples.iter_mut() {
        live_proc_keys.insert(sample.key);
        if sample.key.pid == 0 || sample.key.pid == 4 {
            continue;
        }

        let prev_state = proc_probe_state.get(&sample.key).copied();
        let Some(adapter_mask_filter) = should_probe_process(prev_state, sample.category, tick)
        else {
            continue;
        };

        // Open a strictly short-lived handle verified against `sample.key.create_time_100ns`
        // so PID reuse can never misattribute another process's GPU memory or utilization.
        let proc_handle = match open_verified_process(sample.key, PROCESS_QUERY_LIMITED_INFORMATION)
        {
            Ok(handle) => handle,
            Err(PalError::ElevationRequired) => {
                proc_probe_state.insert(sample.key, ProcessGpuProbeState::AccessDenied);
                continue;
            }
            Err(_) => continue,
        };

        let mut dedicated_bytes: u64 = 0;
        let mut shared_bytes: u64 = 0;
        let mut max_util_permille: u16 = 0;
        let mut cached_seq_num: Option<Option<u64>> = None;
        let mut attached_mask: u64 = 0;
        let mut attached_any = false;

        for (adapter_idx, adapter) in adapters.iter().enumerate() {
            if let Some(mask) = adapter_mask_filter
                && adapter_idx < 64
                && (mask & (1u64 << adapter_idx)) == 0
            {
                continue;
            }

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

            attached_any = true;
            if adapter_idx < 64 {
                attached_mask |= 1u64 << adapter_idx;
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
                let util = compute_process_node_util(prev_proc_nodes, key, running_time_100ns, now);
                max_util_permille = max_util_permille.max(util);
            }
        }

        let next_state = next_probe_state_after_scan(
            attached_mask,
            attached_any,
            sample.key.pid,
            sample.category,
            tick,
        );
        proc_probe_state.insert(sample.key, next_state);

        sample.gpu_dedicated_bytes = dedicated_bytes;
        sample.gpu_shared_bytes = shared_bytes;
        sample.gpu_util_permille = max_util_permille;
    }

    prune_stale_process_gpu_nodes(prev_proc_nodes, live_proc_keys);
    proc_probe_state.retain(|k, _| live_proc_keys.contains(k));
}

/// Computes per-process single-node GPU utilization in permille (`0..=1000`) using `checked_sub`
/// counter-reset safety.
pub(super) fn compute_process_node_util(
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
pub(super) fn prune_stale_process_gpu_nodes(
    prev_map: &mut HashMap<ProcessGpuNodeKey, PrevNodeSample>,
    live_keys: &HashSet<ProcessKey>,
) {
    prev_map.retain(|k, _| live_keys.contains(&k.process));
}
