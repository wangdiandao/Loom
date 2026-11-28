//! Windows fast-lane pure-memory kernel sampler (`ntdll` + non-blocking window metadata).
//!
//! Strictly observes Loom's Windows anti-hang red lines:
//! - No WMI or PDH dependencies.
//! - No cross-process `GetWindowTextW` or blocking `SendMessageW` — uses exclusively
//!   `InternalGetWindowText`, `IsHungAppWindow`, and `DwmGetWindowAttribute(DWMWA_CLOAKED)`.
//! - Reuses aligned internal buffers (`raw_proc_buf`, `raw_cpu_buf`, window maps) and
//!   caller-owned [`ProcessSampleScratch`] / [`PerfSampleScratch`] for zero per-tick
//!   heap allocations in steady state.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{size_of, transmute, zeroed};
use std::ptr;
use std::sync::OnceLock;

use loom_pal_api::{
    FastSystemSampler, PalError, PerfSampleScratch, ProcessSampleScratch, SystemOperator,
};
use loom_types::{
    ProcessCategory, ProcessHotSample, ProcessKey, ProcessPriority, ProcessStatusFlags,
    SystemPerfSample, Timestamp100Ns,
};
use windows_sys::Wdk::System::SystemInformation::{
    NtQuerySystemInformation, SystemProcessInformation, SystemProcessorPerformanceInformation,
};
use windows_sys::Win32::Foundation::{
    FILETIME, HANDLE, HWND, LPARAM, STATUS_INFO_LENGTH_MISMATCH, STATUS_SUCCESS, TRUE,
    UNICODE_STRING,
};
use windows_sys::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Power::PROCESSOR_POWER_INFORMATION;
use windows_sys::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
use windows_sys::Win32::System::SystemInformation::{
    GetSystemTimeAsFileTime, GetTickCount64, GlobalMemoryStatusEx, MEMORYSTATUSEX,
};
use windows_sys::Win32::System::Threading::{
    ALL_PROCESSOR_GROUPS, GetActiveProcessorCount, GetCurrentProcess, HIGH_PRIORITY_CLASS,
    SetPriorityClass,
};
use windows_sys::Win32::System::WindowsProgramming::{
    SYSTEM_PROCESS_INFORMATION, SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION, SYSTEM_THREAD_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongW, GetWindowThreadProcessId,
    InternalGetWindowText, IsHungAppWindow, IsWindowVisible, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};
use windows_sys::core::BOOL;

use crate::etw_net::EtwProcessNetCollector;
use crate::handle::{filetime_to_u64, to_wide_null};
use crate::operator::WindowsSystemOperator;
use crate::perf::cpu::{populate_memory_list_composition, sample_cpu_frequencies};
use crate::perf::gpu::GpuPerfSampler;
use crate::perf::net::NetworkPerfSampler;

/// Initial size of the reusable `SystemProcessInformation` buffer (512 KiB).
const INITIAL_PROC_BUF_BYTES: usize = 512 * 1024;
/// Maximum number of retry attempts when growing the kernel buffer on `STATUS_INFO_LENGTH_MISMATCH`.
const MAX_QUERY_RETRIES: usize = 8;
/// NT `KTHREAD_STATE::Waiting` value.
const THREAD_STATE_WAITING: u32 = 5;
/// NT `KWAIT_REASON::Suspended` value.
const WAIT_REASON_SUSPENDED: u32 = 5;
/// Maximum UTF-16 code units captured per top-level window title.
const MAX_WINDOW_TITLE_CHARS: usize = 256;
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
fn query_processor_group_counts(total_logical: u32) -> Vec<u32> {
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
struct CpuCycleAccountant {
    prev_perf: Vec<SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION>,
    prev_idle_cycles: Vec<u64>,
    prev_qpc: Option<i64>,
    group_cycles_per_sec: Vec<Option<u64>>,
}

impl CpuCycleAccountant {
    fn with_capacity(cpu_cap: usize, group_count: usize) -> Self {
        Self {
            prev_perf: Vec::with_capacity(cpu_cap),
            prev_idle_cycles: Vec::with_capacity(cpu_cap),
            prev_qpc: None,
            group_cycles_per_sec: vec![None; group_count.max(1)],
        }
    }

    /// Computes `(cpu_total_permille, cpu_kernel_permille)` and optional `per_core_out`.
    ///
    /// Prefers idle-cycle accounting whenever `current_idle_cycles` and a monotonic QPC delta
    /// are available and pass sanity checks; falls back seamlessly to
    /// `SystemProcessorPerformanceInformation` on cold ticks, older/restricted environments, or
    /// counter/clock anomalies while preserving accurate kernel-time breakdown.
    fn compute_delta(
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

/// Complete Windows NT `SYSTEM_PROCESS_INFORMATION` ABI layout (Vista / Windows 10 / 11).
///
/// The `winternl.h` definition exported by `windows-sys` collapses several documented
/// fields (`WorkingSetPrivateSize`, `CreateTime`, `UserTime`, `KernelTime`,
/// `InheritedFromUniqueProcessId`, I/O transfer counters) into opaque `Reserved*` arrays.
/// We mirror the exact NT kernel ABI and verify layout size equivalence at compile time.
#[repr(C)]
#[derive(Clone, Copy)]
struct NtSystemProcessInformation {
    next_entry_offset: u32,
    number_of_threads: u32,
    working_set_private_size: i64,
    hard_fault_count: u32,
    number_of_threads_high_watermark: u32,
    cycle_time: u64,
    create_time: i64,
    user_time: i64,
    kernel_time: i64,
    image_name: UNICODE_STRING,
    base_priority: i32,
    unique_process_id: HANDLE,
    inherited_from_unique_process_id: HANDLE,
    handle_count: u32,
    session_id: u32,
    unique_process_key: usize,
    peak_virtual_size: usize,
    virtual_size: usize,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_page_count: usize,
    read_operation_count: i64,
    write_operation_count: i64,
    other_operation_count: i64,
    read_transfer_count: i64,
    write_transfer_count: i64,
    other_transfer_count: i64,
}

const _: () = {
    assert!(
        size_of::<NtSystemProcessInformation>() == size_of::<SYSTEM_PROCESS_INFORMATION>(),
        "NtSystemProcessInformation size must match SYSTEM_PROCESS_INFORMATION"
    );
};

/// Compact per-PID top-level window summary stored in a reusable map.
#[derive(Debug, Clone, Copy, Default)]
struct WindowSummary {
    title_offset: u32,
    title_len: u16,
    is_hung: bool,
}

/// Reusable state passed into the `EnumWindows` callback without heap allocations.
struct WindowEnumState<'a> {
    map: &'a mut HashMap<u32, WindowSummary>,
    titles: &'a mut String,
    utf16_buf: &'a mut Vec<u16>,
}

/// Boosts the current Loom process priority to `HIGH_PRIORITY_CLASS` early during startup
/// so sampling and UI responsiveness survive 100% host CPU contention.
pub fn boost_self_priority() {
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle valid for the current process,
    // and `SetPriorityClass` safely adjusts our own process priority class.
    unsafe {
        let _ = SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
    }
}

/// Windows fast-lane pure-memory kernel sampler.
///
/// Owns pre-allocated, 8-byte aligned buffers reused across every tick to achieve
/// sub-millisecond scans with zero steady-state heap allocations.
pub struct WindowsFastSampler {
    /// 8-byte aligned backing storage for `NtQuerySystemInformation(SystemProcessInformation)`.
    raw_proc_buf: Vec<u64>,
    /// Reusable buffer for `NtQuerySystemInformation(SystemProcessorPerformanceInformation)`.
    raw_cpu_buf: Vec<SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION>,
    /// Reusable buffer for `NtQuerySystemInformationEx(SystemProcessorIdleCycleTimeInformation)`.
    raw_idle_cycles_buf: Vec<u64>,
    /// Active logical processor counts per Windows processor group (`0..N`).
    group_counts: Vec<u32>,
    /// Idle-cycle + perf CPU accountant for `sample_processes`.
    proc_cpu_accountant: CpuCycleAccountant,
    /// Idle-cycle + perf CPU accountant for `sample_performance`.
    perf_cpu_accountant: CpuCycleAccountant,
    /// Reusable PID -> top-level visible window summary map.
    window_map: HashMap<u32, WindowSummary>,
    /// Reusable UTF-8 arena storing window titles for the current tick.
    window_titles: String,
    /// Reusable UTF-16 buffer for `InternalGetWindowText`.
    utf16_title_buf: Vec<u16>,
    /// Reusable buffer for `CallNtPowerInformation(ProcessorInformation)`.
    power_info_buf: Vec<PROCESSOR_POWER_INFORMATION>,
    /// Fast-lane `iphlpapi` network adapter throughput sampler.
    net_sampler: NetworkPerfSampler,
    /// Fast-lane `DXGI` + `D3DKMT` GPU telemetry sampler.
    gpu_sampler: GpuPerfSampler,
    /// Optional elevated-only ETW per-process network byte collector (active only when leased).
    etw_net: Option<EtwProcessNetCollector>,
    /// Whether per-process `D3DKMT` GPU telemetry runs during [`Self::sample_processes`].
    per_process_gpu_enabled: bool,
    /// Cached total logical CPU count across all processor groups.
    logical_cpu_count: u32,
}

impl Drop for WindowsFastSampler {
    fn drop(&mut self) {
        self.etw_net = None;
    }
}

impl std::fmt::Debug for WindowsFastSampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsFastSampler")
            .field(
                "raw_proc_buf_bytes",
                &(self.raw_proc_buf.len() * size_of::<u64>()),
            )
            .field("logical_cpu_count", &self.logical_cpu_count)
            .field("window_map_len", &self.window_map.len())
            .finish()
    }
}

impl Default for WindowsFastSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsFastSampler {
    /// Creates a new fast-lane sampler with pre-allocated reusable buffers.
    pub fn new() -> Self {
        let logical_cpu_count = query_logical_cpu_count();
        let cpu_cap = (logical_cpu_count as usize).max(1);
        let group_counts = query_processor_group_counts(logical_cpu_count);
        let group_len = group_counts.len();
        Self {
            raw_proc_buf: vec![0u64; INITIAL_PROC_BUF_BYTES / size_of::<u64>()],
            raw_cpu_buf: vec![SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION::default(); cpu_cap],
            raw_idle_cycles_buf: vec![0u64; cpu_cap],
            group_counts,
            proc_cpu_accountant: CpuCycleAccountant::with_capacity(cpu_cap, group_len),
            perf_cpu_accountant: CpuCycleAccountant::with_capacity(cpu_cap, group_len),
            window_map: HashMap::with_capacity(128),
            window_titles: String::with_capacity(4096),
            utf16_title_buf: vec![0u16; MAX_WINDOW_TITLE_CHARS],
            power_info_buf: vec![PROCESSOR_POWER_INFORMATION::default(); cpu_cap],
            net_sampler: NetworkPerfSampler::new(),
            gpu_sampler: GpuPerfSampler::new(),
            etw_net: None,
            per_process_gpu_enabled: true,
            logical_cpu_count,
        }
    }

    /// Queries `SystemProcessInformation` into `self.raw_proc_buf`, growing the buffer
    /// only when needed, and returns the exact byte length written by the kernel.
    fn query_raw_processes(&mut self) -> Result<usize, PalError> {
        for _ in 0..MAX_QUERY_RETRIES {
            let buf_bytes = self.raw_proc_buf.len() * size_of::<u64>();
            let mut return_len: u32 = 0;

            // SAFETY: `self.raw_proc_buf` is 8-byte aligned and writable for `buf_bytes`.
            let status = unsafe {
                NtQuerySystemInformation(
                    SystemProcessInformation,
                    self.raw_proc_buf.as_mut_ptr().cast::<c_void>(),
                    buf_bytes as u32,
                    &mut return_len,
                )
            };

            if status == STATUS_SUCCESS {
                let used = if return_len == 0 {
                    buf_bytes
                } else {
                    (return_len as usize).min(buf_bytes)
                };
                return Ok(used);
            }

            if status == STATUS_INFO_LENGTH_MISMATCH {
                let needed_bytes = (return_len as usize)
                    .saturating_add(64 * 1024)
                    .max(buf_bytes.saturating_mul(2));
                let needed_u64s = needed_bytes.div_ceil(size_of::<u64>());
                self.raw_proc_buf.resize(needed_u64s, 0);
                continue;
            }

            return Err(PalError::OsError {
                code: status,
                message: format!(
                    "NtQuerySystemInformation(SystemProcessInformation) failed: 0x{:08X}",
                    status as u32
                ),
            });
        }

        Err(PalError::OsError {
            code: STATUS_INFO_LENGTH_MISMATCH,
            message:
                "NtQuerySystemInformation(SystemProcessInformation) exceeded buffer growth retries"
                    .into(),
        })
    }

    /// Runs a non-blocking `EnumWindows` pass using exclusively `InternalGetWindowText`,
    /// `IsHungAppWindow`, and `DwmGetWindowAttribute(DWMWA_CLOAKED)`.
    fn collect_visible_windows(&mut self) {
        self.window_map.clear();
        self.window_titles.clear();

        let mut state = WindowEnumState {
            map: &mut self.window_map,
            titles: &mut self.window_titles,
            utf16_buf: &mut self.utf16_title_buf,
        };

        // SAFETY: `state` lives for the entire synchronous duration of `EnumWindows`.
        unsafe {
            let _ = EnumWindows(
                Some(enum_window_proc),
                (&mut state as *mut WindowEnumState<'_>) as LPARAM,
            );
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
    fn sample_cpu_counters(
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
        let idle_cycles = Self::query_idle_cycles(raw_idle_cycles_buf, group_counts, count);

        accountant.compute_delta(
            &raw_cpu_buf[..count],
            idle_cycles,
            group_counts,
            qpc_now,
            api.qpc_frequency,
            per_core_out,
        )
    }
}

impl FastSystemSampler for WindowsFastSampler {
    type Ext = ();

    fn sample_processes(
        &mut self,
        scratch: &mut ProcessSampleScratch<Self::Ext>,
    ) -> Result<Timestamp100Ns, PalError> {
        scratch.clear();

        let used_bytes = self.query_raw_processes()?;
        let timestamp = current_filetime_100ns();

        // Collect non-blocking top-level window classification & hung status.
        self.collect_visible_windows();

        // Snapshot cumulative ETW per-PID network bytes if elevated ETW collector is active.
        let etw_totals = self
            .etw_net
            .as_ref()
            .map(EtwProcessNetCollector::snapshot_totals);

        let base_ptr = self.raw_proc_buf.as_ptr().cast::<u8>();
        let base_addr = base_ptr as usize;
        let end_addr = base_addr.saturating_add(used_bytes);
        let proc_hdr_size = size_of::<NtSystemProcessInformation>();
        let thread_size = size_of::<SYSTEM_THREAD_INFORMATION>();

        let mut offset: usize = 0;
        let mut total_processes: u32 = 0;
        let mut total_threads: u32 = 0;
        let mut total_handles: u32 = 0;

        while offset.saturating_add(proc_hdr_size) <= used_bytes {
            // SAFETY: `offset + proc_hdr_size <= used_bytes` and buffer is valid for `used_bytes`.
            let entry = unsafe {
                ptr::read_unaligned(base_ptr.add(offset).cast::<NtSystemProcessInformation>())
            };

            let entry_end = if entry.next_entry_offset == 0 {
                used_bytes
            } else {
                offset
                    .saturating_add(entry.next_entry_offset as usize)
                    .min(used_bytes)
            };

            let pid = entry.unique_process_id as usize as u32;
            let parent_pid = entry.inherited_from_unique_process_id as usize as u32;
            let create_time_100ns = entry.create_time.max(0) as u64;

            let mut status = ProcessStatusFlags::empty();
            if pid == 0 {
                status |= ProcessStatusFlags::SYSTEM_IDLE
                    | ProcessStatusFlags::KERNEL
                    | ProcessStatusFlags::PROTECTED
                    | ProcessStatusFlags::CRITICAL;
            } else if pid == 4 {
                status |= ProcessStatusFlags::KERNEL
                    | ProcessStatusFlags::PROTECTED
                    | ProcessStatusFlags::CRITICAL;
            }

            // Push UTF-16 image name directly into `scratch.string_buf` without intermediate allocation.
            let (name_offset, name_len) = if pid == 0 {
                scratch.push_str("System Idle Process")
            } else if entry.image_name.Length > 0 && !entry.image_name.Buffer.is_null() {
                let name_ptr = entry.image_name.Buffer as usize;
                let byte_len = usize::from(entry.image_name.Length);
                if name_ptr >= base_addr && name_ptr.saturating_add(byte_len) <= end_addr {
                    let code_units = byte_len / size_of::<u16>();
                    // SAFETY: Verified `name_ptr..name_ptr + byte_len` lies strictly inside `raw_proc_buf`.
                    let wide_slice =
                        unsafe { std::slice::from_raw_parts(entry.image_name.Buffer, code_units) };
                    push_utf16_into_scratch(scratch, wide_slice)
                } else {
                    (0, 0)
                }
            } else if pid == 4 {
                scratch.push_str("System")
            } else {
                (0, 0)
            };

            // Detect SUSPENDED state from inline `SYSTEM_THREAD_INFORMATION` array.
            if pid != 0 && entry.number_of_threads > 0 {
                let threads_offset = offset.saturating_add(proc_hdr_size);
                let threads_bytes = (entry.number_of_threads as usize).saturating_mul(thread_size);
                if threads_offset.saturating_add(threads_bytes) <= entry_end {
                    let mut all_suspended = true;
                    for i in 0..(entry.number_of_threads as usize) {
                        let t_off = threads_offset + i * thread_size;
                        // SAFETY: Bounds verified above (`threads_offset + threads_bytes <= entry_end <= used_bytes`).
                        let thread = unsafe {
                            ptr::read_unaligned(
                                base_ptr.add(t_off).cast::<SYSTEM_THREAD_INFORMATION>(),
                            )
                        };
                        if thread.ThreadState != THREAD_STATE_WAITING
                            || thread.WaitReason != WAIT_REASON_SUSPENDED
                        {
                            all_suspended = false;
                            break;
                        }
                    }
                    if all_suspended {
                        status |= ProcessStatusFlags::SUSPENDED;
                    }
                }
            }

            // Classify process and copy primary window title if present.
            let (category, window_title_offset, window_title_len) =
                if pid != 0 && pid != 4 && self.window_map.contains_key(&pid) {
                    let win = self.window_map[&pid];
                    if win.is_hung {
                        status |= ProcessStatusFlags::NOT_RESPONDING;
                    }
                    let title = slice_str(&self.window_titles, win.title_offset, win.title_len);
                    let (t_off, t_len) = scratch.push_str(title);
                    (ProcessCategory::App, t_off, t_len)
                } else {
                    let image_name = scratch.str_slice(name_offset, name_len);
                    let cat = if is_windows_system_process(pid, entry.session_id, image_name) {
                        ProcessCategory::System
                    } else {
                        ProcessCategory::Background
                    };
                    (cat, 0, 0)
                };

            let (net_recv_bytes, net_sent_bytes) = etw_totals
                .as_ref()
                .and_then(|totals| totals.get(&pid).copied())
                .unwrap_or((0, 0));

            let sample = ProcessHotSample {
                key: ProcessKey::new(pid, create_time_100ns),
                parent_pid,
                session_id: entry.session_id,
                thread_count: entry.number_of_threads,
                handle_count: entry.handle_count,
                kernel_time_100ns: entry.kernel_time.max(0) as u64,
                user_time_100ns: entry.user_time.max(0) as u64,
                private_working_set_bytes: entry.working_set_private_size.max(0) as u64,
                working_set_bytes: entry.working_set_size as u64,
                commit_bytes: entry.private_page_count as u64,
                disk_read_bytes: entry.read_transfer_count.max(0) as u64,
                disk_write_bytes: entry.write_transfer_count.max(0) as u64,
                net_recv_bytes,
                net_sent_bytes,
                gpu_dedicated_bytes: 0,
                gpu_shared_bytes: 0,
                gpu_util_permille: 0,
                priority: map_base_priority(entry.base_priority),
                category,
                status,
                name_offset,
                name_len,
                window_title_offset,
                window_title_len,
                ext: (),
            };

            scratch.samples.push(sample);

            total_processes = total_processes.saturating_add(1);
            total_threads = total_threads.saturating_add(entry.number_of_threads);
            total_handles = total_handles.saturating_add(entry.handle_count);

            if entry.next_entry_offset == 0 {
                break;
            }
            let next = offset.saturating_add(entry.next_entry_offset as usize);
            if next <= offset {
                break;
            }
            offset = next;
        }

        // Populate pure kernel-mode D3DKMT per-process GPU telemetry (VRAM & utilization)
        // using strictly short-lived `PROCESS_QUERY_LIMITED_INFORMATION` handles.
        // Skipped on Tick 0 when disabled by the coordinator so cold startup publishes
        // immediately before DXGI/D3DKMT adapter discovery runs on Tick 1.
        if self.per_process_gpu_enabled {
            self.gpu_sampler
                .sample_process_gpu_metrics(&mut scratch.samples);
        }

        // Populate system-wide performance summary alongside the process snapshot.
        let (cpu_total_permille, cpu_kernel_permille) = Self::sample_cpu_counters(
            &mut self.raw_cpu_buf,
            &mut self.raw_idle_cycles_buf,
            &self.group_counts,
            &mut self.proc_cpu_accountant,
            None,
        );

        let mut perf = SystemPerfSample {
            timestamp,
            cpu_total_permille,
            cpu_kernel_permille,
            logical_cpu_count: self.logical_cpu_count,
            process_count: total_processes,
            thread_count: total_threads,
            handle_count: total_handles,
            uptime_secs: current_uptime_secs(),
            ..SystemPerfSample::default()
        };
        populate_memory_counters(&mut perf, false);
        scratch.perf = perf;

        Ok(timestamp)
    }

    fn sample_performance(
        &mut self,
        scratch: &mut PerfSampleScratch,
    ) -> Result<Timestamp100Ns, PalError> {
        scratch.clear();

        let timestamp = current_filetime_100ns();
        let (cpu_total_permille, cpu_kernel_permille) = Self::sample_cpu_counters(
            &mut self.raw_cpu_buf,
            &mut self.raw_idle_cycles_buf,
            &self.group_counts,
            &mut self.perf_cpu_accountant,
            Some(&mut scratch.per_core_permille),
        );
        let (cpu_freq_mhz, cpu_base_freq_mhz) =
            sample_cpu_frequencies(&mut self.power_info_buf, self.logical_cpu_count);
        let (net_recv_bytes_per_sec, net_sent_bytes_per_sec) =
            self.net_sampler.sample(&mut scratch.networks);
        let gpu_summary = self.gpu_sampler.sample(&mut scratch.gpus);

        let mut sample = SystemPerfSample {
            timestamp,
            cpu_total_permille,
            cpu_kernel_permille,
            cpu_freq_mhz,
            cpu_base_freq_mhz,
            logical_cpu_count: self.logical_cpu_count,
            uptime_secs: current_uptime_secs(),
            net_recv_bytes_per_sec,
            net_sent_bytes_per_sec,
            gpu_util_permille: gpu_summary.util_permille,
            gpu_dedicated_used_bytes: gpu_summary.dedicated_used_bytes,
            gpu_dedicated_total_bytes: gpu_summary.dedicated_total_bytes,
            gpu_shared_used_bytes: gpu_summary.shared_used_bytes,
            ..SystemPerfSample::default()
        };
        populate_memory_counters(&mut sample, true);
        scratch.sample = sample;

        Ok(timestamp)
    }

    fn set_per_process_network_enabled(&mut self, enabled: bool) {
        if !enabled {
            self.etw_net = None;
            return;
        }
        if self.etw_net.is_some() {
            return;
        }
        let is_elevated = WindowsSystemOperator.is_elevated();
        if !is_elevated {
            return;
        }
        let mut collector = EtwProcessNetCollector::new();
        match collector.start(true) {
            Ok(()) => {
                self.etw_net = Some(collector);
            }
            Err(err) => {
                tracing::debug!(
                    %err,
                    "per-process ETW network collector unavailable; leaving disabled"
                );
            }
        }
    }

    fn set_per_process_gpu_enabled(&mut self, enabled: bool) {
        self.per_process_gpu_enabled = enabled;
    }
}

/// Non-blocking `EnumWindows` callback.
///
/// Strictly avoids `GetWindowTextW` and `SendMessageW`; uses only `InternalGetWindowText`,
/// `IsHungAppWindow`, and `DwmGetWindowAttribute(DWMWA_CLOAKED)`.
unsafe extern "system" fn enum_window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    if hwnd.is_null() || lparam == 0 {
        return TRUE;
    }

    // SAFETY: All Win32 queries below are non-blocking kernel/DWM reads and `lparam`
    // points to our stack-resident `WindowEnumState` for the synchronous `EnumWindows` call.
    unsafe {
        if IsWindowVisible(hwnd) == 0 {
            return TRUE;
        }

        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if (ex_style & WS_EX_TOOLWINDOW) != 0 {
            return TRUE;
        }

        let owner = GetWindow(hwnd, GW_OWNER);
        if !owner.is_null() && (ex_style & WS_EX_APPWINDOW) == 0 {
            return TRUE;
        }

        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 {
            return TRUE;
        }

        let state = &mut *(lparam as *mut WindowEnumState<'_>);

        // Read title directly from kernel window struct (non-blocking) BEFORE DWM IPC so
        // titleless helper windows never incur a DwmGetWindowAttribute roundtrip.
        let written = InternalGetWindowText(
            hwnd,
            state.utf16_buf.as_mut_ptr(),
            state.utf16_buf.len() as i32,
        );
        if written <= 0 {
            return TRUE;
        }
        let wide_len = (written as usize).min(state.utf16_buf.len());

        // Check DWM cloaking to ignore hidden UWP / virtual-desktop windows.
        let mut cloaked: u32 = 0;
        let hr = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED as u32,
            (&mut cloaked as *mut u32).cast::<c_void>(),
            size_of::<u32>() as u32,
        );
        if hr == 0 && cloaked != 0 {
            return TRUE;
        }

        let is_hung = IsHungAppWindow(hwnd) != 0;

        if let Some(existing) = state.map.get_mut(&pid) {
            existing.is_hung |= is_hung;
            if existing.title_len == 0 && wide_len > 0 {
                let (off, len) = push_utf16_into_string(state.titles, &state.utf16_buf[..wide_len]);
                existing.title_offset = off;
                existing.title_len = len;
            }
        } else {
            let (title_offset, title_len) =
                push_utf16_into_string(state.titles, &state.utf16_buf[..wide_len]);
            state.map.insert(
                pid,
                WindowSummary {
                    title_offset,
                    title_len,
                    is_hung,
                },
            );
        }
    }

    TRUE
}

/// Decodes a UTF-16 slice directly into `scratch.string_buf` without intermediate allocation.
fn push_utf16_into_scratch<Ext>(
    scratch: &mut ProcessSampleScratch<Ext>,
    wide: &[u16],
) -> (u32, u16) {
    push_utf16_into_string(&mut scratch.string_buf, wide)
}

/// Decodes a UTF-16 slice directly into a reusable UTF-8 `String` arena.
fn push_utf16_into_string(arena: &mut String, wide: &[u16]) -> (u32, u16) {
    if wide.is_empty() {
        return (0, 0);
    }
    let Ok(start) = u32::try_from(arena.len()) else {
        return (0, 0);
    };
    let start_usize = start as usize;
    let max_bytes = usize::from(u16::MAX);

    // Fast path for ASCII image names (avoids UTF-16 surrogate decoding overhead).
    let mut idx = 0;
    while idx < wide.len() {
        let code = wide[idx];
        if code == 0 {
            let written = arena.len() - start_usize;
            return if written == 0 {
                (0, 0)
            } else {
                (start, written as u16)
            };
        }
        if code >= 0x80 {
            break;
        }
        if arena.len() - start_usize >= max_bytes {
            return (start, (arena.len() - start_usize) as u16);
        }
        arena.push(code as u8 as char);
        idx += 1;
    }

    if idx < wide.len() {
        for decoded in char::decode_utf16(wide[idx..].iter().copied()) {
            let ch = decoded.unwrap_or(char::REPLACEMENT_CHARACTER);
            if ch == '\0' {
                break;
            }
            let next_len = arena.len() - start_usize + ch.len_utf8();
            if next_len > max_bytes {
                break;
            }
            arena.push(ch);
        }
    }

    let written = arena.len() - start_usize;
    if written == 0 {
        (0, 0)
    } else {
        (start, written as u16)
    }
}

/// Resolves an `(offset, len)` slice inside a string arena.
#[inline]
fn slice_str(arena: &str, offset: u32, len: u16) -> &str {
    if len == 0 {
        return "";
    }
    let start = offset as usize;
    let Some(end) = start.checked_add(usize::from(len)) else {
        return "";
    };
    arena.get(start..end).unwrap_or("")
}

/// Returns the current system timestamp in 100-nanosecond FILETIME ticks.
#[inline]
fn current_filetime_100ns() -> Timestamp100Ns {
    let mut ft = FILETIME::default();
    // SAFETY: `GetSystemTimeAsFileTime` writes directly into initialized stack memory.
    unsafe {
        GetSystemTimeAsFileTime(&mut ft);
    }
    Timestamp100Ns::new(filetime_to_u64(ft))
}

/// Returns system uptime in whole seconds via `GetTickCount64`.
#[inline]
fn current_uptime_secs() -> u64 {
    // SAFETY: `GetTickCount64` has no preconditions.
    unsafe { GetTickCount64() / 1000 }
}

/// Queries the total number of active logical processors across all processor groups.
pub(crate) fn query_logical_cpu_count() -> u32 {
    // SAFETY: `GetActiveProcessorCount` accepts `ALL_PROCESSOR_GROUPS` safely.
    let count = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) };
    if count > 0 {
        count
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1)
    }
}

/// Populates physical memory, commit, kernel pool, and optionally global object counts.
fn populate_memory_counters(sample: &mut SystemPerfSample, include_counts: bool) {
    // SAFETY: `MEMORYSTATUSEX` is POD; `dwLength` is initialized before the Win32 call.
    let mut mem_status: MEMORYSTATUSEX = unsafe { zeroed() };
    mem_status.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut mem_status) } != 0 {
        sample.mem_total_bytes = mem_status.ullTotalPhys;
        sample.mem_available_bytes = mem_status.ullAvailPhys;
        sample.mem_used_bytes = mem_status
            .ullTotalPhys
            .saturating_sub(mem_status.ullAvailPhys);
    }

    // SAFETY: `PERFORMANCE_INFORMATION` is POD; `cb` is initialized before the Win32 call.
    let mut perf_info: PERFORMANCE_INFORMATION = unsafe { zeroed() };
    perf_info.cb = size_of::<PERFORMANCE_INFORMATION>() as u32;
    if unsafe { GetPerformanceInfo(&mut perf_info, perf_info.cb) } != 0 {
        let page = perf_info.PageSize as u64;
        sample.mem_committed_bytes = (perf_info.CommitTotal as u64).saturating_mul(page);
        sample.mem_commit_limit_bytes = (perf_info.CommitLimit as u64).saturating_mul(page);
        sample.mem_paged_pool_bytes = (perf_info.KernelPaged as u64).saturating_mul(page);
        sample.mem_nonpaged_pool_bytes = (perf_info.KernelNonpaged as u64).saturating_mul(page);
        if include_counts {
            sample.process_count = perf_info.ProcessCount;
            sample.thread_count = perf_info.ThreadCount;
            sample.handle_count = perf_info.HandleCount;
            populate_memory_list_composition(sample, page, perf_info.SystemCache);
        }
    } else if include_counts {
        populate_memory_list_composition(sample, 4096, 0);
    }
}

/// Maps Windows NT `BasePriority` values to cross-platform [`ProcessPriority`].
#[inline]
fn map_base_priority(base_priority: i32) -> ProcessPriority {
    match base_priority {
        ..=4 => ProcessPriority::Idle,
        5..=6 => ProcessPriority::BelowNormal,
        7..=8 => ProcessPriority::Normal,
        9..=10 => ProcessPriority::AboveNormal,
        11..=15 => ProcessPriority::High,
        16.. => ProcessPriority::Realtime,
    }
}

/// Classifies core Windows operating-system processes when they do not host a visible app window.
fn is_windows_system_process(pid: u32, session_id: u32, image_name: &str) -> bool {
    if pid == 0 || pid == 4 || session_id == 0 {
        return true;
    }

    // Well-known interactive-session Windows shell and subsystem processes.
    const WINDOWS_IMAGES: &[&str] = &[
        "explorer.exe",
        "csrss.exe",
        "dwm.exe",
        "sihost.exe",
        "ctfmon.exe",
        "taskhostw.exe",
        "fontdrvhost.exe",
        "conhost.exe",
        "runtimebroker.exe",
        "startmenuexperiencehost.exe",
        "shellexperiencehost.exe",
        "searchhost.exe",
        "textinputhost.exe",
        "dllhost.exe",
        "svchost.exe",
        "winlogon.exe",
        "wininit.exe",
        "services.exe",
        "lsass.exe",
        "smss.exe",
        "registry",
        "memory compression",
        "secure system",
        "system interrupts",
    ];

    WINDOWS_IMAGES
        .iter()
        .any(|known| image_name.eq_ignore_ascii_case(known))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perf_entry(
        idle: i64,
        kernel_only: i64,
        user: i64,
    ) -> SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION {
        SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION {
            IdleTime: idle,
            KernelTime: idle + kernel_only,
            UserTime: user,
            ..SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION::default()
        }
    }

    #[test]
    fn cpu_cycle_accountant_computes_idle_cycle_busy_and_kernel_split() {
        let mut acc = CpuCycleAccountant::with_capacity(2, 1);
        let qpc_freq = 10_000_000u64; // 10 MHz QPC
        let rate_hz = 2_000_000_000u64; // 2.0 GHz TSC/cycle rate
        let mut per_core = Vec::new();

        // Cold sample establishes baseline and returns (0, 0).
        let p0 = [perf_entry(0, 0, 0), perf_entry(0, 0, 0)];
        let c0 = [1_000_000_000u64, 1_000_000_000u64];
        assert_eq!(
            acc.compute_delta(&p0, Some(&c0), &[2], Some(0), qpc_freq, Some(&mut per_core)),
            (0, 0)
        );
        assert_eq!(per_core, vec![0, 0]);

        // Tick 1 (1.0s): core 0 is 100% idle (calibrates 2.0 GHz exact rate),
        // core 1 is 50% busy (50% idle cycles = 1.0e9), with half of busy time in kernel.
        let p1 = [
            perf_entry(10_000_000, 0, 0),
            perf_entry(5_000_000, 2_500_000, 2_500_000),
        ];
        let c1 = [c0[0] + rate_hz, c0[1] + rate_hz / 2];
        let (total, kernel) = acc.compute_delta(
            &p1,
            Some(&c1),
            &[2],
            Some(qpc_freq as i64),
            qpc_freq,
            Some(&mut per_core),
        );
        assert_eq!(per_core, vec![0, 500]);
        assert_eq!(total, 250);
        assert_eq!(kernel, 125);
    }

    #[test]
    fn cpu_cycle_accountant_falls_back_cleanly_on_anomaly_or_missing_cycles() {
        let mut acc = CpuCycleAccountant::with_capacity(2, 1);
        let qpc_freq = 10_000_000u64;
        let mut per_core = Vec::new();

        let p0 = [perf_entry(0, 0, 0), perf_entry(0, 0, 0)];
        let c0 = [5_000_000_000u64, 5_000_000_000u64];
        assert_eq!(
            acc.compute_delta(&p0, Some(&c0), &[2], Some(0), qpc_freq, Some(&mut per_core)),
            (0, 0)
        );

        // Counter underflow (idle cycles drop below previous): must fall back to
        // SystemProcessorPerformanceInformation (core 0 = 20% busy, core 1 = 60% busy => 40% total, 10% kernel).
        let p1 = [
            perf_entry(8_000_000, 1_000_000, 1_000_000),
            perf_entry(4_000_000, 1_000_000, 5_000_000),
        ];
        let c1_underflow = [100u64, 100u64];
        let (total, kernel) = acc.compute_delta(
            &p1,
            Some(&c1_underflow),
            &[2],
            Some(qpc_freq as i64),
            qpc_freq,
            Some(&mut per_core),
        );
        assert_eq!(per_core, vec![200, 600]);
        assert_eq!(total, 400);
        assert_eq!(kernel, 100);

        // Missing idle cycles (`None`) also falls back cleanly (cumulative counters:
        // core 0: +10M idle, 0 busy -> 0%; core 1: +5M idle, +2M kernel, +3M user -> 50% busy, 20% kernel).
        let p2 = [
            perf_entry(18_000_000, 1_000_000, 1_000_000),
            perf_entry(9_000_000, 3_000_000, 8_000_000),
        ];
        let (total2, kernel2) = acc.compute_delta(
            &p2,
            None,
            &[2],
            Some((qpc_freq * 2) as i64),
            qpc_freq,
            Some(&mut per_core),
        );
        assert_eq!(per_core, vec![0, 500]);
        assert_eq!(total2, 250);
        assert_eq!(kernel2, 100);
    }

    #[test]
    fn cpu_cycle_accountant_supports_multiple_processor_groups() {
        let mut acc = CpuCycleAccountant::with_capacity(2, 2);
        let qpc_freq = 10_000_000u64;
        let mut per_core = Vec::new();

        let p0 = [perf_entry(0, 0, 0), perf_entry(0, 0, 0)];
        let c0 = [0u64, 0u64];
        acc.compute_delta(
            &p0,
            Some(&c0),
            &[1, 1],
            Some(0),
            qpc_freq,
            Some(&mut per_core),
        );

        // Group 0 runs at 3.0 GHz (idle), Group 1 runs at 2.0 GHz (idle) -> calibrate both groups.
        let p1 = [perf_entry(10_000_000, 0, 0), perf_entry(10_000_000, 0, 0)];
        let c1 = [3_000_000_000u64, 2_000_000_000u64];
        let (t1, _) = acc.compute_delta(
            &p1,
            Some(&c1),
            &[1, 1],
            Some(qpc_freq as i64),
            qpc_freq,
            Some(&mut per_core),
        );
        assert_eq!(t1, 0);
        assert_eq!(per_core, vec![0, 0]);

        // Tick 2: Group 0 is 40% busy (1.8e9 idle), Group 1 is 20% busy (1.6e9 idle).
        let p2 = [
            perf_entry(16_000_000, 1_000_000, 3_000_000),
            perf_entry(18_000_000, 500_000, 1_500_000),
        ];
        let c2 = [c1[0] + 1_800_000_000, c1[1] + 1_600_000_000];
        let (t2, k2) = acc.compute_delta(
            &p2,
            Some(&c2),
            &[1, 1],
            Some((qpc_freq * 2) as i64),
            qpc_freq,
            Some(&mut per_core),
        );
        assert_eq!(per_core, vec![400, 200]);
        assert_eq!(t2, 320); // (1.2e9 + 0.4e9) / (3.0e9 + 2.0e9) = 1.6 / 5.0 = 320 permille
        assert!(k2 <= t2);
    }
}
