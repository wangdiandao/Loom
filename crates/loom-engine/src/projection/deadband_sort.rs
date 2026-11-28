//! Transitivity-safe sort hysteresis (deadband) and process/details comparators.

use std::cmp::Ordering;
use std::hash::Hasher as _;

use loom_types::{
    DetailsSortColumn, ProcessArch, ProcessRowProjection, ProcessSortColumn, ProcessStatusFlags,
    SortDirection, UacVirtualization,
};

use super::state::{
    ComputedProcess, GroupAggregate, arch_rank, category_index, cmp_str_ascii_ci, effective_status,
    process_display_name_ref, uac_rank,
};
use crate::slow_lane::ColdMetaCache;
use crate::tree::FastHasher;

/// Hysteresis deadband for CPU sort columns (`2` permille = `0.2%`).
pub const CPU_SORT_DEADBAND_PERMILLE: u16 = 2;

/// Hysteresis deadband for GPU utilization sort column (`2` permille = `0.2%`).
pub const GPU_SORT_DEADBAND_PERMILLE: u16 = 2;

/// Hysteresis deadband for disk and network throughput sort columns (`16 KiB/s`).
pub const RATE_SORT_DEADBAND_BPS: u64 = 16 * 1024;

/// Dense per-process previous-tick deadband state indexed directly by `computed` index
/// so `slice::sort_by` performs `O(1)` array indexing with zero `HashMap` lookups.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct DenseDeadbandEntry {
    pub(super) prev_rank: usize,
    pub(super) prev_sticky: u64,
    pub(super) has_prev: bool,
}

/// Borrowed sort hysteresis deadband state for a single projection pass.
#[derive(Clone, Copy)]
pub(super) struct DeadbandContext<'a> {
    pub(super) deadband: u64,
    pub(super) dense_prev: &'a [DenseDeadbandEntry],
}

#[inline]
pub(super) const fn process_column_deadband(column: ProcessSortColumn) -> Option<u64> {
    match column {
        ProcessSortColumn::Cpu => Some(CPU_SORT_DEADBAND_PERMILLE as u64),
        ProcessSortColumn::Gpu => Some(GPU_SORT_DEADBAND_PERMILLE as u64),
        ProcessSortColumn::Disk | ProcessSortColumn::Network => Some(RATE_SORT_DEADBAND_BPS),
        _ => None,
    }
}

#[inline]
pub(super) const fn details_column_deadband(column: DetailsSortColumn) -> Option<u64> {
    match column {
        DetailsSortColumn::Cpu => Some(CPU_SORT_DEADBAND_PERMILLE as u64),
        _ => None,
    }
}

#[inline]
pub(super) const fn process_row_deadband_metric(
    row: &ProcessRowProjection,
    column: ProcessSortColumn,
) -> u64 {
    match column {
        ProcessSortColumn::Cpu => row.cpu_permille as u64,
        ProcessSortColumn::Gpu => row.gpu_util_permille as u64,
        ProcessSortColumn::Disk => row.disk_bytes_per_sec,
        ProcessSortColumn::Network => row.net_bytes_per_sec,
        _ => 0,
    }
}

/// Computes the hysteresis-stabilized effective sort metric from a pre-populated dense entry.
///
/// Because this depends solely on the element's own live value and immutable previous-tick
/// sticky metric (never on pairs of compared elements), sorting by the returned value defines
/// a strict total order and can never violate `slice::sort_by` transitivity, while avoiding
/// any `HashMap` lookup inside the `O(N log N)` comparator.
#[inline(always)]
pub(super) fn resolve_sticky_metric_dense(
    entry: DenseDeadbandEntry,
    live_val: u64,
    deadband: u64,
) -> u64 {
    if entry.has_prev && live_val.abs_diff(entry.prev_sticky) < deadband {
        entry.prev_sticky
    } else {
        live_val
    }
}

#[inline]
pub(super) const fn details_column_is_static(column: DetailsSortColumn) -> bool {
    matches!(
        column,
        DetailsSortColumn::Name
            | DetailsSortColumn::Pid
            | DetailsSortColumn::UserName
            | DetailsSortColumn::SessionId
            | DetailsSortColumn::Arch
            | DetailsSortColumn::UacVirtualization
            | DetailsSortColumn::Publisher
            | DetailsSortColumn::Description
            | DetailsSortColumn::ExePath
            | DetailsSortColumn::CommandLine
    )
}

#[inline]
pub(super) fn compute_details_static_keys_hash(computed: &[ComputedProcess]) -> u64 {
    let mut h = FastHasher::new();
    h.write_usize(computed.len());
    for proc in computed {
        h.write_u32(proc.key.pid);
        h.write_u64(proc.key.create_time_100ns);
        h.write_u32(proc.session_id);
        h.write_usize(proc.name.as_ptr() as usize);
        h.write_usize(proc.name.len());
        h.write_u8(proc.category as u8);
        h.write_u16(
            (proc.status & (ProcessStatusFlags::KERNEL | ProcessStatusFlags::SYSTEM_IDLE)).bits(),
        );
    }
    h.finish()
}

#[inline]
#[allow(clippy::too_many_arguments)]
pub(super) fn compare_process_items(
    ia: usize,
    ib: usize,
    computed: &[ComputedProcess],
    agg_a: Option<&GroupAggregate>,
    agg_b: Option<&GroupAggregate>,
    cold_cache: &ColdMetaCache,
    column: ProcessSortColumn,
    direction: SortDirection,
    frozen_ranks: Option<&[usize]>,
    deadband_ctx: Option<DeadbandContext<'_>>,
) -> Ordering {
    let a = &computed[ia];
    let b = &computed[ib];
    if let Some(ranks) = frozen_ranks {
        let frozen_ord = ranks[ia].cmp(&ranks[ib]);
        if frozen_ord != Ordering::Equal {
            return frozen_ord;
        }
    }
    let ord = cmp_process_sort_column(ia, ib, a, b, agg_a, agg_b, cold_cache, column, deadband_ctx);
    let directed = match direction {
        SortDirection::Ascending => ord,
        SortDirection::Descending => ord.reverse(),
    };
    directed
        .then_with(|| {
            if let Some(db) = deadband_ctx {
                db.dense_prev[ia]
                    .prev_rank
                    .cmp(&db.dense_prev[ib].prev_rank)
            } else {
                Ordering::Equal
            }
        })
        .then_with(|| cmp_str_ascii_ci(&a.name, &b.name))
        .then_with(|| a.key.cmp(&b.key))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn sort_process_indices_with_aggregates_slice(
    computed: &[ComputedProcess],
    indices: &mut [usize],
    group_agg: &[Option<GroupAggregate>],
    cold_cache: &ColdMetaCache,
    column: ProcessSortColumn,
    direction: SortDirection,
    frozen_ranks: Option<&[usize]>,
    deadband_ctx: Option<DeadbandContext<'_>>,
) {
    indices.sort_by(|&ia, &ib| {
        compare_process_items(
            ia,
            ib,
            computed,
            group_agg[ia].as_ref(),
            group_agg[ib].as_ref(),
            cold_cache,
            column,
            direction,
            frozen_ranks,
            deadband_ctx,
        )
    });
}

#[allow(clippy::too_many_arguments)]
pub(super) fn sort_process_indices_slice(
    computed: &[ComputedProcess],
    indices: &mut [usize],
    cold_cache: &ColdMetaCache,
    column: ProcessSortColumn,
    direction: SortDirection,
    frozen_ranks: Option<&[usize]>,
    deadband_ctx: Option<DeadbandContext<'_>>,
) {
    indices.sort_by(|&ia, &ib| {
        compare_process_items(
            ia,
            ib,
            computed,
            None,
            None,
            cold_cache,
            column,
            direction,
            frozen_ranks,
            deadband_ctx,
        )
    });
}

#[allow(clippy::too_many_arguments)]
pub(super) fn sort_details_indices_slice(
    computed: &[ComputedProcess],
    indices: &mut [usize],
    cold_cache: &ColdMetaCache,
    column: DetailsSortColumn,
    direction: SortDirection,
    frozen_ranks: Option<&[usize]>,
    deadband_ctx: Option<DeadbandContext<'_>>,
) {
    indices.sort_by(|&ia, &ib| {
        let a = &computed[ia];
        let b = &computed[ib];
        if let Some(ranks) = frozen_ranks {
            let frozen_ord = ranks[ia].cmp(&ranks[ib]);
            if frozen_ord != Ordering::Equal {
                return frozen_ord;
            }
        }
        let ord = match column {
            DetailsSortColumn::Name => cmp_str_ascii_ci(&a.name, &b.name),
            DetailsSortColumn::Pid => a.key.pid.cmp(&b.key.pid),
            DetailsSortColumn::Status => effective_status(a, cold_cache)
                .contains(ProcessStatusFlags::SUSPENDED)
                .cmp(&effective_status(b, cold_cache).contains(ProcessStatusFlags::SUSPENDED)),
            DetailsSortColumn::UserName => {
                let ua = cold_cache
                    .get_entry(a.key)
                    .and_then(|e| e.user_name_short.as_deref())
                    .unwrap_or("");
                let ub = cold_cache
                    .get_entry(b.key)
                    .and_then(|e| e.user_name_short.as_deref())
                    .unwrap_or("");
                cmp_str_ascii_ci(ua, ub)
            }
            DetailsSortColumn::SessionId => a.session_id.cmp(&b.session_id),
            DetailsSortColumn::Cpu => {
                if let Some(db) = deadband_ctx {
                    let sa = resolve_sticky_metric_dense(
                        db.dense_prev[ia],
                        u64::from(a.cpu_permille),
                        db.deadband,
                    );
                    let sb = resolve_sticky_metric_dense(
                        db.dense_prev[ib],
                        u64::from(b.cpu_permille),
                        db.deadband,
                    );
                    sa.cmp(&sb)
                } else {
                    a.cpu_permille.cmp(&b.cpu_permille)
                }
            }
            DetailsSortColumn::CpuTime => a.cpu_time_100ns.cmp(&b.cpu_time_100ns),
            DetailsSortColumn::Memory => a
                .private_working_set_bytes
                .cmp(&b.private_working_set_bytes),
            DetailsSortColumn::WorkingSet => a.working_set_bytes.cmp(&b.working_set_bytes),
            DetailsSortColumn::WorkingSetDelta => {
                a.working_set_delta_bytes.cmp(&b.working_set_delta_bytes)
            }
            DetailsSortColumn::CommitSize => a.commit_bytes.cmp(&b.commit_bytes),
            DetailsSortColumn::Priority => a.priority.cmp(&b.priority),
            DetailsSortColumn::Handles => a.handle_count.cmp(&b.handle_count),
            DetailsSortColumn::Threads => a.thread_count.cmp(&b.thread_count),
            DetailsSortColumn::Arch => {
                let aa = arch_rank(
                    cold_cache
                        .get_process(a.key)
                        .map_or(ProcessArch::Unknown, |m| m.arch),
                );
                let ab = arch_rank(
                    cold_cache
                        .get_process(b.key)
                        .map_or(ProcessArch::Unknown, |m| m.arch),
                );
                aa.cmp(&ab)
            }
            DetailsSortColumn::UacVirtualization => {
                let va = uac_rank(
                    cold_cache
                        .get_process(a.key)
                        .map_or(UacVirtualization::Unknown, |m| m.uac_virtualization),
                );
                let vb = uac_rank(
                    cold_cache
                        .get_process(b.key)
                        .map_or(UacVirtualization::Unknown, |m| m.uac_virtualization),
                );
                va.cmp(&vb)
            }
            DetailsSortColumn::Elevated => effective_status(a, cold_cache)
                .contains(ProcessStatusFlags::ELEVATED)
                .cmp(&effective_status(b, cold_cache).contains(ProcessStatusFlags::ELEVATED)),
            DetailsSortColumn::GpuMemory => a.gpu_dedicated_bytes.cmp(&b.gpu_dedicated_bytes),
            DetailsSortColumn::Publisher => {
                let pa = cold_cache
                    .get_process(a.key)
                    .and_then(|m| m.asset.as_ref())
                    .and_then(|asset| asset.publisher.as_deref())
                    .unwrap_or("");
                let pb = cold_cache
                    .get_process(b.key)
                    .and_then(|m| m.asset.as_ref())
                    .and_then(|asset| asset.publisher.as_deref())
                    .unwrap_or("");
                cmp_str_ascii_ci(pa, pb)
            }
            DetailsSortColumn::Description => {
                let da = cold_cache
                    .get_process(a.key)
                    .and_then(|m| m.asset.as_ref())
                    .and_then(|asset| asset.file_description.as_deref())
                    .unwrap_or("");
                let db = cold_cache
                    .get_process(b.key)
                    .and_then(|m| m.asset.as_ref())
                    .and_then(|asset| asset.file_description.as_deref())
                    .unwrap_or("");
                cmp_str_ascii_ci(da, db)
            }
            DetailsSortColumn::ExePath => {
                let ea = cold_cache
                    .get_process(a.key)
                    .and_then(|m| m.exe_path.as_deref())
                    .unwrap_or("");
                let eb = cold_cache
                    .get_process(b.key)
                    .and_then(|m| m.exe_path.as_deref())
                    .unwrap_or("");
                cmp_str_ascii_ci(ea, eb)
            }
            DetailsSortColumn::CommandLine => {
                let cmd_a = cold_cache
                    .get_process(a.key)
                    .and_then(|m| m.command_line.as_deref())
                    .unwrap_or("");
                let cmd_b = cold_cache
                    .get_process(b.key)
                    .and_then(|m| m.command_line.as_deref())
                    .unwrap_or("");
                cmp_str_ascii_ci(cmd_a, cmd_b)
            }
        };
        let directed = match direction {
            SortDirection::Ascending => ord,
            SortDirection::Descending => ord.reverse(),
        };
        directed
            .then_with(|| {
                if let Some(db) = deadband_ctx {
                    db.dense_prev[ia]
                        .prev_rank
                        .cmp(&db.dense_prev[ib].prev_rank)
                } else {
                    Ordering::Equal
                }
            })
            .then_with(|| cmp_str_ascii_ci(&a.name, &b.name))
            .then_with(|| a.key.cmp(&b.key))
    });
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn cmp_process_sort_column(
    ia: usize,
    ib: usize,
    a: &ComputedProcess,
    b: &ComputedProcess,
    agg_a: Option<&GroupAggregate>,
    agg_b: Option<&GroupAggregate>,
    cold_cache: &ColdMetaCache,
    column: ProcessSortColumn,
    deadband_ctx: Option<DeadbandContext<'_>>,
) -> Ordering {
    match column {
        ProcessSortColumn::Name => {
            let da = process_display_name_ref(a, cold_cache);
            let db = process_display_name_ref(b, cold_cache);
            cmp_str_ascii_ci(da, db)
        }
        ProcessSortColumn::Category => category_index(a.category).cmp(&category_index(b.category)),
        ProcessSortColumn::Pid => a.key.pid.cmp(&b.key.pid),
        ProcessSortColumn::ProcessName => cmp_str_ascii_ci(&a.name, &b.name),
        ProcessSortColumn::Status => {
            let sa = agg_a.map_or_else(|| effective_status(a, cold_cache), |g| g.status);
            let sb = agg_b.map_or_else(|| effective_status(b, cold_cache), |g| g.status);
            sa.bits().cmp(&sb.bits())
        }
        ProcessSortColumn::Cpu => {
            let va = u64::from(agg_a.map_or(a.cpu_permille, |g| g.cpu_permille));
            let vb = u64::from(agg_b.map_or(b.cpu_permille, |g| g.cpu_permille));
            if let Some(db) = deadband_ctx {
                let sa = resolve_sticky_metric_dense(db.dense_prev[ia], va, db.deadband);
                let sb = resolve_sticky_metric_dense(db.dense_prev[ib], vb, db.deadband);
                sa.cmp(&sb)
            } else {
                va.cmp(&vb)
            }
        }
        ProcessSortColumn::Memory => agg_a
            .map_or(a.private_working_set_bytes, |g| g.memory_bytes)
            .cmp(&agg_b.map_or(b.private_working_set_bytes, |g| g.memory_bytes)),
        ProcessSortColumn::Disk => {
            let va = agg_a.map_or(a.disk_bytes_per_sec, |g| g.disk_bytes_per_sec);
            let vb = agg_b.map_or(b.disk_bytes_per_sec, |g| g.disk_bytes_per_sec);
            if let Some(db) = deadband_ctx {
                let sa = resolve_sticky_metric_dense(db.dense_prev[ia], va, db.deadband);
                let sb = resolve_sticky_metric_dense(db.dense_prev[ib], vb, db.deadband);
                sa.cmp(&sb)
            } else {
                va.cmp(&vb)
            }
        }
        ProcessSortColumn::Network => {
            let va = agg_a.map_or(a.net_bytes_per_sec, |g| g.net_bytes_per_sec);
            let vb = agg_b.map_or(b.net_bytes_per_sec, |g| g.net_bytes_per_sec);
            if let Some(db) = deadband_ctx {
                let sa = resolve_sticky_metric_dense(db.dense_prev[ia], va, db.deadband);
                let sb = resolve_sticky_metric_dense(db.dense_prev[ib], vb, db.deadband);
                sa.cmp(&sb)
            } else {
                va.cmp(&vb)
            }
        }
        ProcessSortColumn::Gpu => {
            let (ua, vram_a) = agg_a.map_or((a.gpu_util_permille, a.gpu_dedicated_bytes), |g| {
                (g.gpu_util_permille, g.gpu_dedicated_bytes)
            });
            let (ub, vram_b) = agg_b.map_or((b.gpu_util_permille, b.gpu_dedicated_bytes), |g| {
                (g.gpu_util_permille, g.gpu_dedicated_bytes)
            });
            if let Some(db) = deadband_ctx {
                let sa = resolve_sticky_metric_dense(db.dense_prev[ia], u64::from(ua), db.deadband);
                let sb = resolve_sticky_metric_dense(db.dense_prev[ib], u64::from(ub), db.deadband);
                (sa, vram_a).cmp(&(sb, vram_b))
            } else {
                (ua, vram_a).cmp(&(ub, vram_b))
            }
        }
        ProcessSortColumn::Publisher => {
            let pa = cold_cache
                .get_process(a.key)
                .and_then(|m| m.asset.as_ref())
                .and_then(|asset| asset.publisher.as_deref())
                .unwrap_or("");
            let pb = cold_cache
                .get_process(b.key)
                .and_then(|m| m.asset.as_ref())
                .and_then(|asset| asset.publisher.as_deref())
                .unwrap_or("");
            cmp_str_ascii_ci(pa, pb)
        }
        ProcessSortColumn::CommandLine => {
            let ca = cold_cache
                .get_process(a.key)
                .and_then(|m| m.command_line.as_deref())
                .unwrap_or("");
            let cb = cold_cache
                .get_process(b.key)
                .and_then(|m| m.command_line.as_deref())
                .unwrap_or("");
            cmp_str_ascii_ci(ca, cb)
        }
    }
}
