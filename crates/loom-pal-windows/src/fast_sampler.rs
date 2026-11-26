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
use std::mem::{size_of, zeroed};
use std::ptr;

use loom_pal_api::{FastSystemSampler, PalError, PerfSampleScratch, ProcessSampleScratch};
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

use crate::handle::filetime_to_u64;

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
    /// Previous CPU tick counters for `sample_processes` delta calculation.
    prev_proc_cpu: Vec<SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION>,
    /// Previous CPU tick counters for `sample_performance` delta calculation.
    prev_perf_cpu: Vec<SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION>,
    /// Reusable PID -> top-level visible window summary map.
    window_map: HashMap<u32, WindowSummary>,
    /// Reusable UTF-8 arena storing window titles for the current tick.
    window_titles: String,
    /// Reusable UTF-16 buffer for `InternalGetWindowText`.
    utf16_title_buf: Vec<u16>,
    /// Cached total logical CPU count across all processor groups.
    logical_cpu_count: u32,
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
        Self {
            raw_proc_buf: vec![0u64; INITIAL_PROC_BUF_BYTES / size_of::<u64>()],
            raw_cpu_buf: vec![SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION::default(); cpu_cap],
            prev_proc_cpu: Vec::with_capacity(cpu_cap),
            prev_perf_cpu: Vec::with_capacity(cpu_cap),
            window_map: HashMap::with_capacity(128),
            window_titles: String::with_capacity(4096),
            utf16_title_buf: vec![0u16; MAX_WINDOW_TITLE_CHARS],
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

    /// Queries per-logical-CPU performance counters into `self.raw_cpu_buf` and computes
    /// aggregate total/kernel CPU permille (and optional per-core permille) against `prev`.
    fn sample_cpu_counters(
        raw_cpu_buf: &mut Vec<SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION>,
        prev: &mut Vec<SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION>,
        mut per_core_out: Option<&mut Vec<u16>>,
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

        let current = &raw_cpu_buf[..count];

        if prev.len() != count {
            // Cold baseline sample: store initial counters and report 0 until next delta.
            prev.clear();
            prev.extend_from_slice(current);
            if let Some(out) = per_core_out.as_deref_mut() {
                out.clear();
                out.resize(count, 0);
            }
            return (0, 0);
        }

        let mut sum_busy: u128 = 0;
        let mut sum_kernel: u128 = 0;
        let mut sum_total: u128 = 0;

        if let Some(out) = per_core_out.as_deref_mut() {
            out.clear();
            if out.capacity() < count {
                out.reserve(count - out.capacity());
            }
        }

        for (curr, old) in current.iter().zip(prev.iter()) {
            // Note: On Windows, `KernelTime` INCLUDES `IdleTime`.
            let d_idle = (curr.IdleTime - old.IdleTime).max(0) as u64;
            let d_kernel_incl_idle = (curr.KernelTime - old.KernelTime).max(0) as u64;
            let d_user = (curr.UserTime - old.UserTime).max(0) as u64;

            let d_kernel_only = d_kernel_incl_idle.saturating_sub(d_idle);
            let d_total = d_kernel_incl_idle.saturating_add(d_user);
            let d_busy = d_kernel_only.saturating_add(d_user);

            sum_busy += u128::from(d_busy);
            sum_kernel += u128::from(d_kernel_only);
            sum_total += u128::from(d_total);

            if let Some(out) = per_core_out.as_deref_mut() {
                let core_permille = if d_total == 0 {
                    0
                } else {
                    ((u128::from(d_busy) * 1000) / u128::from(d_total)).min(1000) as u16
                };
                out.push(core_permille);
            }
        }

        prev.clear();
        prev.extend_from_slice(current);

        if sum_total == 0 {
            return (0, 0);
        }

        let cpu_total = ((sum_busy * 1000) / sum_total).min(1000) as u16;
        let cpu_kernel = ((sum_kernel * 1000) / sum_total).min(1000) as u16;
        (cpu_total, cpu_kernel)
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
                net_recv_bytes: 0,
                net_sent_bytes: 0,
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

        // Populate system-wide performance summary alongside the process snapshot.
        let (cpu_total_permille, cpu_kernel_permille) =
            Self::sample_cpu_counters(&mut self.raw_cpu_buf, &mut self.prev_proc_cpu, None);

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
            &mut self.prev_perf_cpu,
            Some(&mut scratch.per_core_permille),
        );

        let mut sample = SystemPerfSample {
            timestamp,
            cpu_total_permille,
            cpu_kernel_permille,
            logical_cpu_count: self.logical_cpu_count,
            uptime_secs: current_uptime_secs(),
            ..SystemPerfSample::default()
        };
        populate_memory_counters(&mut sample, true);
        scratch.sample = sample;

        Ok(timestamp)
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
        }
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
