//! Windows fast-lane pure-memory kernel sampler (`ntdll` + non-blocking window metadata).
//!
//! Strictly observes Loom's Windows anti-hang red lines:
//! - No WMI or PDH dependencies.
//! - No cross-process `GetWindowTextW` or blocking `SendMessageW` — uses exclusively
//!   `InternalGetWindowText`, `IsHungAppWindow`, and `DwmGetWindowAttribute(DWMWA_CLOAKED)`.
//! - Reuses aligned internal buffers (`raw_proc_buf`, `raw_cpu_buf`, window maps, ETW scratch)
//!   and caller-owned [`ProcessSampleScratch`] / [`PerfSampleScratch`] for zero per-tick
//!   heap allocations in steady state.

mod cpu_cycles;
#[cfg(test)]
mod tests;
mod windows_enum;

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr;

use loom_pal_api::{
    FastSystemSampler, PalError, PerfSampleScratch, ProcessSampleScratch, SystemOperator,
};
use loom_types::{
    ProcessCategory, ProcessHotSample, ProcessKey, ProcessPriority, ProcessStatusFlags,
    SystemPerfSample, Timestamp100Ns,
};
use windows_sys::Wdk::System::SystemInformation::{
    NtQuerySystemInformation, SystemProcessInformation,
};
use windows_sys::Win32::Foundation::{
    FILETIME, HANDLE, STATUS_INFO_LENGTH_MISMATCH, STATUS_SUCCESS, UNICODE_STRING,
};
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

use crate::etw_net::EtwProcessNetCollector;
use crate::handle::filetime_to_u64;
use crate::operator::WindowsSystemOperator;
use crate::perf::cpu::{populate_memory_list_composition, sample_cpu_frequencies};
use crate::perf::gpu::GpuPerfSampler;
use crate::perf::net::NetworkPerfSampler;

use self::cpu_cycles::{CpuCycleAccountant, query_processor_group_counts, sample_cpu_counters};
use self::windows_enum::{
    MAX_WINDOW_TITLE_CHARS, WindowSummary, collect_visible_windows, push_utf16_into_scratch,
    slice_str,
};

/// Initial size of the reusable `SystemProcessInformation` buffer (512 KiB).
const INITIAL_PROC_BUF_BYTES: usize = 512 * 1024;
/// Maximum number of retry attempts when growing the kernel buffer on `STATUS_INFO_LENGTH_MISMATCH`.
const MAX_QUERY_RETRIES: usize = 8;
/// NT `KTHREAD_STATE::Waiting` value.
const THREAD_STATE_WAITING: u32 = 5;
/// NT `KWAIT_REASON::Suspended` value.
const WAIT_REASON_SUSPENDED: u32 = 5;

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
    /// Lazily-sized reusable buffer for `CallNtPowerInformation(ProcessorInformation)`.
    power_info_buf: Vec<PROCESSOR_POWER_INFORMATION>,
    /// Fast-lane `iphlpapi` network adapter throughput sampler.
    net_sampler: NetworkPerfSampler,
    /// Fast-lane `DXGI` + `D3DKMT` GPU telemetry sampler (loads `dxgi.dll` lazily).
    gpu_sampler: GpuPerfSampler,
    /// Optional elevated-only ETW per-process network byte collector (active only when leased).
    etw_net: Option<EtwProcessNetCollector>,
    /// Reusable per-PID cumulative ETW byte snapshot buffer (avoids per-tick `HashMap` clones).
    etw_totals_scratch: HashMap<u32, (u64, u64)>,
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
    ///
    /// Non-essential subsystems (`dxgi.dll`, `GetAdaptersAddresses` buffer, CPU power-info buffer)
    /// initialize lazily on first use so cold-start Tick 0 stays sub-millisecond.
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
            power_info_buf: Vec::new(),
            net_sampler: NetworkPerfSampler::new(),
            gpu_sampler: GpuPerfSampler::new(),
            etw_net: None,
            etw_totals_scratch: HashMap::new(),
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
        collect_visible_windows(
            &mut self.window_map,
            &mut self.window_titles,
            &mut self.utf16_title_buf,
        );

        // Snapshot cumulative ETW per-PID network bytes into reusable scratch if active.
        let has_etw = if let Some(ref etw) = self.etw_net {
            etw.snapshot_totals_into(&mut self.etw_totals_scratch);
            true
        } else {
            false
        };

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

            // Classify process and copy primary window title if present (single hash lookup).
            let (category, window_title_offset, window_title_len) = if pid != 0
                && pid != 4
                && let Some(&win) = self.window_map.get(&pid)
            {
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

            let (net_recv_bytes, net_sent_bytes) = if has_etw {
                self.etw_totals_scratch.get(&pid).copied().unwrap_or((0, 0))
            } else {
                (0, 0)
            };

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
        // using strictly short-lived verified handles.
        // Skipped on Tick 0 when disabled by the coordinator so cold startup publishes
        // immediately before DXGI/D3DKMT loading & adapter discovery run on Tick 1.
        if self.per_process_gpu_enabled {
            self.gpu_sampler
                .sample_process_gpu_metrics(&mut scratch.samples);
        }

        // Populate system-wide performance summary alongside the process snapshot.
        let (cpu_total_permille, cpu_kernel_permille) = sample_cpu_counters(
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
        let (cpu_total_permille, cpu_kernel_permille) = sample_cpu_counters(
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
            self.etw_totals_scratch.clear();
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
