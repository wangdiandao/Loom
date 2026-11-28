//! Per-group idle-cycle and performance-counter CPU utilization accounting.

use std::ffi::c_void;
use std::mem::{size_of, transmute};
use std::sync::OnceLock;

use windows_sys::Wdk::System::SystemInformation::{
    NtQuerySystemInformation, SystemProcessorPerformanceInformation,
};
use windows_sys::Win32::Foundation::{STATUS_INFO_LENGTH_MISMATCH, STATUS_SUCCESS};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Threading::GetActiveProcessorCount;
use windows_sys::Win32::System::WindowsProgramming::SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION;
use windows_sys::core::BOOL;

use crate::handle::to_wide_null;

/// NT `SYSTEM_INFORMATION_CLASS::SystemProcessorIdleCycleTimeInformation` (`83`).
const SYSTEM_PROCESSOR_IDLE_CYCLE_TIME_INFORMATION_CLASS: i32 = 83;
/// Minimum plausible processor cycle frequency for QPC-validated idle-cycle accounting (10 MHz).
const MIN_VALID_CYCLE_RATE_HZ: u64 = 10_000_000;
/// Maximum plausible processor cycle frequency for QPC-validated idle-cycle accounting (20 GHz).
const MAX_VALID_CYCLE_RATE_HZ: u64 = 20_000_000_000;

type FarProc = unsafe extern "system" fn() -> isize;
type FnNtQuerySystemInformationEx = unsafe extern "system" fn(
    system_information_class: i32,
    input_buffer: *const c_void,
    input_buffer_length: u32,
    system_information: *mut c_void,
    system_information_length: u32,
    return_length: *mut u32,
) -> i32;
type FnQueryPerformanceCounter = unsafe extern "system" fn(lp_performance_count: *mut i64) -> BOOL;
type FnQueryPerformanceFrequency = unsafe extern "system" fn(lp_frequency: *mut i64) -> BOOL;
type FnGetActiveProcessorGroupCount = unsafe extern "system" fn() -> u16;

/// Dynamically resolved NT/Kernel32 timing and processor-group helpers for idle-cycle accounting.
struct IdleCycleApi {
    nt_query_system_information_ex: Option<FnNtQuerySystemInformationEx>,
    query_performance_counter: Option<FnQueryPerformanceCounter>,
    qpc_frequency: u64,
    get_active_processor_group_count: Option<FnGetActiveProcessorGroupCount>,
}

unsafe impl Send for IdleCycleApi {}
unsafe impl Sync for IdleCycleApi {}

fn idle_cycle_api() -> &'static IdleCycleApi {
    static API: OnceLock<IdleCycleApi> = OnceLock::new();
    API.get_or_init(|| {
        let ntdll_name = to_wide_null("ntdll.dll");
        // SAFETY: `ntdll.dll` is permanently mapped in every Windows user-mode process.
        let ntdll = unsafe { GetModuleHandleW(ntdll_name.as_ptr()) };
        let nt_query_ex = if !ntdll.is_null() {
            unsafe { GetProcAddress(ntdll, c"NtQuerySystemInformationEx".as_ptr().cast()) }
                .map(|p| unsafe { transmute::<FarProc, FnNtQuerySystemInformationEx>(p) })
        } else {
            None
        };

        let k32_name = to_wide_null("kernel32.dll");
        // SAFETY: `kernel32.dll` is permanently mapped in every Windows user-mode process.
        let k32 = unsafe { GetModuleHandleW(k32_name.as_ptr()) };
        let qpc = if !k32.is_null() {
            unsafe { GetProcAddress(k32, c"QueryPerformanceCounter".as_ptr().cast()) }
                .map(|p| unsafe { transmute::<FarProc, FnQueryPerformanceCounter>(p) })
        } else {
            None
        };
        let qpf = if !k32.is_null() {
            unsafe { GetProcAddress(k32, c"QueryPerformanceFrequency".as_ptr().cast()) }
                .map(|p| unsafe { transmute::<FarProc, FnQueryPerformanceFrequency>(p) })
        } else {
            None
        };
        let group_count_fn = if !k32.is_null() {
            unsafe { GetProcAddress(k32, c"GetActiveProcessorGroupCount".as_ptr().cast()) }
                .map(|p| unsafe { transmute::<FarProc, FnGetActiveProcessorGroupCount>(p) })
        } else {
            None
        };

        let mut freq_i64: i64 = 0;
        let qpc_frequency = if let Some(qpf_fn) = qpf
            && unsafe { qpf_fn(&mut freq_i64) } != 0
            && freq_i64 > 0
        {
            freq_i64 as u64
        } else {
            0
        };

        IdleCycleApi {
            nt_query_system_information_ex: nt_query_ex,
            query_performance_counter: qpc,
            qpc_frequency,
            get_active_processor_group_count: group_count_fn,
        }
    })
}

/// Queries the active logical processor count for each Windows processor group (`0..N`).
pub(super) fn query_processor_group_counts(total_logical: u32) -> Vec<u32> {
    let api = idle_cycle_api();
    let group_count = api
        .get_active_processor_group_count
        .map(|f| unsafe { f() })
        .unwrap_or(1)
        .max(1);
    let mut counts = Vec::with_capacity(usize::from(group_count));
    for g in 0..group_count {
        // SAFETY: `GetActiveProcessorCount` safely accepts any `u16` group index.
        let cnt = unsafe { GetActiveProcessorCount(g) };
        if cnt > 0 {
            counts.push(cnt);
        }
    }
    if counts.is_empty() {
        counts.push(total_logical.max(1));
    }
    counts
}

/// Stateful per-stream CPU utilization accountant preferring
/// `NtQuerySystemInformationEx(SystemProcessorIdleCycleTimeInformation = 83)` with
/// QPC-validated per-group cycle baselines and clean fallback to
/// `SystemProcessorPerformanceInformation`.
#[derive(Default)]
pub(super) struct CpuCycleAccountant {
    prev_perf: Vec<SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION>,
    prev_idle_cycles: Vec<u64>,
    prev_qpc: Option<i64>,
    group_cycles_per_sec: Vec<Option<u64>>,
}

impl CpuCycleAccountant {
    pub(super) fn with_capacity(cpu_cap: usize, group_count: usize) -> Self {
        Self {
            prev_perf: Vec::with_capacity(cpu_cap),
            prev_idle_cycles: Vec::with_capacity(cpu_cap),
            prev_qpc: None,
            group_cycles_per_sec: vec![None; group_count.max(1)],
        }
    }

    /// Computes `(cpu_total_permille, cpu_kernel_permille)` and optional `per_core_out`.
    pub(super) fn compute_delta(
        &mut self,
        current_perf: &[SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION],
        current_idle_cycles: Option<&[u64]>,
        group_counts: &[u32],
        qpc_now: Option<i64>,
        qpc_freq: u64,
        mut per_core_out: Option<&mut Vec<u16>>,
    ) -> (u16, u16) {
        let count = current_perf.len();
        if count == 0 {
            return (0, 0);
        }

        // Cold baseline sample: store initial counters and report 0 until next delta.
        if self.prev_perf.len() != count {
            self.prev_perf.clear();
            self.prev_perf.extend_from_slice(current_perf);
            self.prev_idle_cycles.clear();
            if let Some(cycles) = current_idle_cycles
                && cycles.len() == count
            {
                self.prev_idle_cycles.extend_from_slice(cycles);
            }
            self.prev_qpc = qpc_now;
            if let Some(out) = per_core_out.as_deref_mut() {
                out.clear();
                out.resize(count, 0);
            }
            return (0, 0);
        }

        // Compute `SystemProcessorPerformanceInformation` totals (needed for kernel-time split
        // and as our guaranteed fallback path).
        let mut perf_sum_busy: u128 = 0;
        let mut perf_sum_kernel: u128 = 0;
        let mut perf_sum_total: u128 = 0;
        for (curr, old) in current_perf.iter().zip(self.prev_perf.iter()) {
            let d_idle = curr.IdleTime.checked_sub(old.IdleTime).unwrap_or(0).max(0) as u64;
            let d_kernel_incl_idle = curr
                .KernelTime
                .checked_sub(old.KernelTime)
                .unwrap_or(0)
                .max(0) as u64;
            let d_user = curr.UserTime.checked_sub(old.UserTime).unwrap_or(0).max(0) as u64;
            let d_kernel_only = d_kernel_incl_idle.saturating_sub(d_idle);
            let d_total = d_kernel_incl_idle.saturating_add(d_user);
            let d_busy = d_kernel_only.saturating_add(d_user);

            perf_sum_busy += u128::from(d_busy);
            perf_sum_kernel += u128::from(d_kernel_only);
            perf_sum_total += u128::from(d_total);
        }

        let idle_result = self.try_compute_idle_cycle_delta(
            current_perf,
            current_idle_cycles,
            group_counts,
            qpc_now,
            qpc_freq,
            perf_sum_busy,
            perf_sum_kernel,
            per_core_out.as_deref_mut(),
        );

        let result = if let Some(pair) = idle_result {
            pair
        } else {
            // Fallback: compute per-core and aggregate utilization from
            // `SystemProcessorPerformanceInformation`.
            if let Some(out) = per_core_out {
                out.clear();
                if out.capacity() < count {
                    out.reserve(count - out.capacity());
                }
                for (curr, old) in current_perf.iter().zip(self.prev_perf.iter()) {
                    let d_idle = curr.IdleTime.checked_sub(old.IdleTime).unwrap_or(0).max(0) as u64;
                    let d_kernel_incl_idle = curr
                        .KernelTime
                        .checked_sub(old.KernelTime)
                        .unwrap_or(0)
                        .max(0) as u64;
                    let d_user = curr.UserTime.checked_sub(old.UserTime).unwrap_or(0).max(0) as u64;
                    let d_kernel_only = d_kernel_incl_idle.saturating_sub(d_idle);
                    let d_total = d_kernel_incl_idle.saturating_add(d_user);
                    let d_busy = d_kernel_only.saturating_add(d_user);
                    let core_permille = (u128::from(d_busy) * 1000)
                        .checked_div(u128::from(d_total))
                        .map_or(0, |v| v.min(1000) as u16);
                    out.push(core_permille);
                }
            }

            let cpu_total = (perf_sum_busy * 1000)
                .checked_div(perf_sum_total)
                .map_or(0, |v| v.min(1000) as u16);
            let cpu_kernel = (perf_sum_kernel * 1000)
                .checked_div(perf_sum_total)
                .map_or(0, |v| v.min(u128::from(cpu_total)) as u16);
            (cpu_total, cpu_kernel)
        };

        // Always refresh baseline buffers for the next tick.
        self.prev_perf.clear();
        self.prev_perf.extend_from_slice(current_perf);
        self.prev_idle_cycles.clear();
        if let Some(cycles) = current_idle_cycles
            && cycles.len() == count
        {
            self.prev_idle_cycles.extend_from_slice(cycles);
        }
        self.prev_qpc = qpc_now;

        result
    }

    #[allow(clippy::too_many_arguments)]
    fn try_compute_idle_cycle_delta(
        &mut self,
        current_perf: &[SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION],
        current_idle_cycles: Option<&[u64]>,
        group_counts: &[u32],
        qpc_now: Option<i64>,
        qpc_freq: u64,
        perf_sum_busy: u128,
        perf_sum_kernel: u128,
        mut per_core_out: Option<&mut Vec<u16>>,
    ) -> Option<(u16, u16)> {
        let count = current_perf.len();
        let curr_cycles = current_idle_cycles?;
        if curr_cycles.len() != count || self.prev_idle_cycles.len() != count || qpc_freq == 0 {
            return None;
        }
        let d_qpc = qpc_now?.checked_sub(self.prev_qpc?)?;
        if d_qpc <= 0 {
            return None;
        }
        let d_qpc_u128 = d_qpc as u128;
        let qpc_freq_u128 = u128::from(qpc_freq);

        // Determine processor-group slice layout; fall back to a single group if counts differ.
        let single_group = [count as u32];
        let groups_total: usize = group_counts.iter().map(|&c| c as usize).sum();
        let groups = if !group_counts.is_empty() && groups_total == count {
            group_counts
        } else {
            &single_group
        };
        if self.group_cycles_per_sec.len() != groups.len() {
            self.group_cycles_per_sec.resize(groups.len(), None);
        }

        if let Some(out) = per_core_out.as_deref_mut() {
            out.clear();
            if out.capacity() < count {
                out.reserve(count - out.capacity());
            }
        }

        let mut sum_busy_cycles: u128 = 0;
        let mut sum_expected_cycles: u128 = 0;
        let mut cpu_offset: usize = 0;

        for (group_idx, &g_count_u32) in groups.iter().enumerate() {
            let g_count = g_count_u32 as usize;
            let range = cpu_offset..cpu_offset + g_count;
            cpu_offset += g_count;

            let mut max_core_rate_hz: u64 = 0;
            let mut has_nearly_idle_core = false;
            let mut group_idle_cycles: u128 = 0;
            let mut group_idle_100ns: u128 = 0;

            for i in range.clone() {
                let d_idle_cycles = curr_cycles[i].checked_sub(self.prev_idle_cycles[i])?;
                let rate_u128 = (u128::from(d_idle_cycles) * qpc_freq_u128) / d_qpc_u128;
                let rate_hz = u64::try_from(rate_u128).ok()?;
                max_core_rate_hz = max_core_rate_hz.max(rate_hz);

                let curr_p = &current_perf[i];
                let old_p = &self.prev_perf[i];
                let d_idle_100ns = curr_p
                    .IdleTime
                    .checked_sub(old_p.IdleTime)
                    .unwrap_or(0)
                    .max(0) as u64;
                let d_kernel_incl = curr_p
                    .KernelTime
                    .checked_sub(old_p.KernelTime)
                    .unwrap_or(0)
                    .max(0) as u64;
                let d_user = curr_p
                    .UserTime
                    .checked_sub(old_p.UserTime)
                    .unwrap_or(0)
                    .max(0) as u64;
                let d_total = d_kernel_incl.saturating_add(d_user);
                let d_busy = d_kernel_incl
                    .saturating_sub(d_idle_100ns)
                    .saturating_add(d_user);

                if d_total > 0 && d_busy.saturating_mul(10) <= d_total {
                    has_nearly_idle_core = true;
                }
                group_idle_cycles += u128::from(d_idle_cycles);
                group_idle_100ns += u128::from(d_idle_100ns);
            }

            let perf_implied_hz = if group_idle_100ns >= 10_000 && group_idle_cycles > 0 {
                u64::try_from((group_idle_cycles * 10_000_000) / group_idle_100ns).ok()
            } else {
                None
            };

            let base_hz = match self.group_cycles_per_sec[group_idx] {
                Some(existing) => {
                    if max_core_rate_hz > existing.saturating_mul(2) {
                        // Clock jump or VM suspend/resume anomaly: recalibrate via fallback.
                        self.group_cycles_per_sec[group_idx] = None;
                        return None;
                    }
                    if max_core_rate_hz > existing && max_core_rate_hz <= MAX_VALID_CYCLE_RATE_HZ {
                        self.group_cycles_per_sec[group_idx] = Some(max_core_rate_hz);
                        max_core_rate_hz
                    } else {
                        existing
                    }
                }
                None => {
                    let candidate = if has_nearly_idle_core
                        && (MIN_VALID_CYCLE_RATE_HZ..=MAX_VALID_CYCLE_RATE_HZ)
                            .contains(&max_core_rate_hz)
                    {
                        Some(max_core_rate_hz)
                    } else if let Some(implied) = perf_implied_hz
                        && (MIN_VALID_CYCLE_RATE_HZ..=MAX_VALID_CYCLE_RATE_HZ).contains(&implied)
                    {
                        Some(implied.max(max_core_rate_hz).min(MAX_VALID_CYCLE_RATE_HZ))
                    } else {
                        None
                    };
                    let calibrated = candidate?;
                    self.group_cycles_per_sec[group_idx] = Some(calibrated);
                    calibrated
                }
            };

            let expected_core_cycles = (u128::from(base_hz) * d_qpc_u128) / qpc_freq_u128;
            if expected_core_cycles == 0 {
                return None;
            }
            // Allow modest 15% headroom for cycle-rate jitter; reject larger anomalies cleanly.
            let max_sane_idle = (expected_core_cycles * 115) / 100;

            for i in range {
                let d_idle_cycles = u128::from(curr_cycles[i] - self.prev_idle_cycles[i]);
                if d_idle_cycles > max_sane_idle {
                    return None;
                }
                let busy_cycles = expected_core_cycles.saturating_sub(d_idle_cycles);
                sum_busy_cycles += busy_cycles;
                sum_expected_cycles += expected_core_cycles;

                if let Some(out) = per_core_out.as_deref_mut() {
                    let core_permille =
                        ((busy_cycles * 1_000) / expected_core_cycles).min(1_000) as u16;
                    out.push(core_permille);
                }
            }
        }

        let cpu_total = (sum_busy_cycles * 1_000)
            .checked_div(sum_expected_cycles)
            .map_or(0, |v| v.min(1_000) as u16);
        let cpu_kernel = (u128::from(cpu_total) * perf_sum_kernel)
            .checked_div(perf_sum_busy)
            .map_or(0, |v| v.min(u128::from(cpu_total)) as u16);
        Some((cpu_total, cpu_kernel))
    }
}

/// Queries per-logical-CPU idle cycles via
/// `NtQuerySystemInformationEx(SystemProcessorIdleCycleTimeInformation = 83)` across all
/// active processor groups into `raw_idle_cycles_buf`, returning the filled slice on success.
fn query_idle_cycles<'a>(
    raw_idle_cycles_buf: &'a mut Vec<u64>,
    group_counts: &[u32],
    expected_total: usize,
) -> Option<&'a [u64]> {
    let api = idle_cycle_api();
    let query_ex = api.nt_query_system_information_ex?;
    if expected_total == 0 {
        return None;
    }
    if raw_idle_cycles_buf.len() < expected_total {
        raw_idle_cycles_buf.resize(expected_total, 0);
    }

    let mut offset: usize = 0;
    for (group_idx, &g_count_u32) in group_counts.iter().enumerate() {
        let g_count = g_count_u32 as usize;
        if g_count == 0 || offset + g_count > expected_total {
            return None;
        }
        let group_id = group_idx as u16;
        let dst = &mut raw_idle_cycles_buf[offset..offset + g_count];
        let dst_bytes = (g_count * size_of::<u64>()) as u32;
        let mut return_len: u32 = 0;

        // SAFETY: `group_id` is a valid `u16` processor group input, and `dst` is 8-byte
        // aligned and writable for `dst_bytes`.
        let status = unsafe {
            query_ex(
                SYSTEM_PROCESSOR_IDLE_CYCLE_TIME_INFORMATION_CLASS,
                (&group_id as *const u16).cast::<c_void>(),
                size_of::<u16>() as u32,
                dst.as_mut_ptr().cast::<c_void>(),
                dst_bytes,
                &mut return_len,
            )
        };
        if status != STATUS_SUCCESS || (return_len as usize) < g_count * size_of::<u64>() {
            return None;
        }
        offset += g_count;
    }

    if offset == expected_total {
        Some(&raw_idle_cycles_buf[..expected_total])
    } else {
        None
    }
}

/// Queries per-logical-CPU performance and idle-cycle counters and computes aggregate
/// total/kernel CPU permille (and optional per-core permille) against `accountant`.
pub(super) fn sample_cpu_counters(
    raw_cpu_buf: &mut Vec<SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION>,
    raw_idle_cycles_buf: &mut Vec<u64>,
    group_counts: &[u32],
    accountant: &mut CpuCycleAccountant,
    per_core_out: Option<&mut Vec<u16>>,
) -> (u16, u16) {
    if raw_cpu_buf.is_empty() {
        raw_cpu_buf.resize(1, SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION::default());
    }

    let mut return_len: u32 = 0;
    let entry_size = size_of::<SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION>();

    for _ in 0..4 {
        let buf_bytes = raw_cpu_buf.len() * entry_size;
        // SAFETY: `raw_cpu_buf` is validly aligned and writable for `buf_bytes`.
        let status = unsafe {
            NtQuerySystemInformation(
                SystemProcessorPerformanceInformation,
                raw_cpu_buf.as_mut_ptr().cast::<c_void>(),
                buf_bytes as u32,
                &mut return_len,
            )
        };

        if status == STATUS_SUCCESS {
            break;
        }
        if status == STATUS_INFO_LENGTH_MISMATCH {
            let needed = (return_len as usize)
                .div_ceil(entry_size)
                .max(raw_cpu_buf.len() * 2);
            raw_cpu_buf.resize(needed, SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION::default());
            continue;
        }
        return (0, 0);
    }

    let count = ((return_len as usize) / entry_size).min(raw_cpu_buf.len());
    if count == 0 {
        return (0, 0);
    }

    let api = idle_cycle_api();
    let qpc_now = api.query_performance_counter.and_then(|qpc| {
        let mut val: i64 = 0;
        // SAFETY: `val` is an initialized stack `i64`.
        (unsafe { qpc(&mut val) } != 0).then_some(val)
    });
    let idle_cycles = query_idle_cycles(raw_idle_cycles_buf, group_counts, count);

    accountant.compute_delta(
        &raw_cpu_buf[..count],
        idle_cycles,
        group_counts,
        qpc_now,
        api.qpc_frequency,
        per_core_out,
    )
}
