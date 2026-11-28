//! Clean-room elevated-only per-process network byte collector backed by a private
//! real-time Windows ETW (`Microsoft-Windows-Kernel-Network`) session.
//!
//! Design invariants:
//! - **Strictly elevated-only**: [`EtwProcessNetCollector::start`] immediately returns
//!   [`PalError::ElevationRequired`] unless the caller confirms `is_elevated == true`.
//! - **Bounded thread budget**: runs `ProcessTrace` on at most **1** dedicated worker
//!   thread (`"loom-etw-net"`).
//! - **Zero session / handle leaks**: [`EtwProcessNetCollector::stop`] (and `Drop`)
//!   stops the ETW controller session via `ControlTraceW(EVENT_TRACE_CONTROL_STOP)`,
//!   joins the consumer thread, and releases the session cleanly.
//! - **Zero external crate additions**: dynamically loads `advapi32.dll` (falling back
//!   to `sechost.dll`) via `LoadLibraryExW(..., LOAD_LIBRARY_SEARCH_SYSTEM32)` / `GetProcAddress`.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{size_of, transmute, zeroed};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use loom_pal_api::PalError;
use parking_lot::Mutex;
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HANDLE,
};

use crate::handle::{OwnedModule, map_win32_error, to_wide_null};

/// `Microsoft-Windows-Kernel-Network` ETW provider GUID:
/// `{7dd42a49-5329-4832-8dfd-43d979153a88}`.
const KERNEL_NETWORK_PROVIDER_GUID: Guid = Guid {
    data1: 0x7dd4_2a49,
    data2: 0x5329,
    data3: 0x4832,
    data4: [0x8d, 0xfd, 0x43, 0xd9, 0x79, 0x15, 0x3a, 0x88],
};

/// `WNODE_FLAG_TRACED_GUID` (`wmistr.h`).
const WNODE_FLAG_TRACED_GUID: u32 = 0x0002_0000;
/// `EVENT_TRACE_REAL_TIME_MODE` (`evntrace.h`).
const EVENT_TRACE_REAL_TIME_MODE: u32 = 0x0000_0100;
/// `EVENT_TRACE_CONTROL_STOP` (`evntrace.h`).
const EVENT_TRACE_CONTROL_STOP: u32 = 1;
/// `EVENT_CONTROL_CODE_ENABLE_PROVIDER` (`evntrace.h`).
const EVENT_CONTROL_CODE_ENABLE_PROVIDER: u32 = 1;
/// `TRACE_LEVEL_INFORMATION` (`evntrace.h`).
const TRACE_LEVEL_INFORMATION: u8 = 4;
/// `PROCESS_TRACE_MODE_REAL_TIME` (`evntrace.h`).
const PROCESS_TRACE_MODE_REAL_TIME: u32 = 0x0000_0100;
/// `PROCESS_TRACE_MODE_EVENT_RECORD` (`evntrace.h`).
const PROCESS_TRACE_MODE_EVENT_RECORD: u32 = 0x1000_0000;

/// Invalid handle sentinel returned by `OpenTraceW` (`INVALID_PROCESSTRACE_HANDLE`).
#[cfg(target_pointer_width = "64")]
const INVALID_PROCESSTRACE_HANDLE: u64 = u64::MAX;
#[cfg(target_pointer_width = "32")]
const INVALID_PROCESSTRACE_HANDLE: u64 = u32::MAX as u64;

/// Maximum UTF-16 code units reserved after `EVENT_TRACE_PROPERTIES` for the session name.
const MAX_SESSION_NAME_CHARS: usize = 128;

/// Standard 16-byte Windows `GUID`.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// `WNODE_HEADER` (`wmistr.h`, 48 bytes).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct WnodeHeader {
    buffer_size: u32,
    provider_id: u32,
    historical_context: u64,
    timestamp: i64,
    guid: Guid,
    client_context: u32,
    flags: u32,
}

/// `EVENT_TRACE_PROPERTIES` (`evntrace.h`, 120 bytes on 64-bit).
#[repr(C)]
#[derive(Clone, Copy)]
struct EventTraceProperties {
    wnode: WnodeHeader,
    buffer_size: u32,
    minimum_buffers: u32,
    maximum_buffers: u32,
    maximum_file_size: u32,
    log_file_mode: u32,
    flush_timer: u32,
    enable_flags: u32,
    age_limit: i32,
    number_of_buffers: u32,
    free_buffers: u32,
    events_lost: u32,
    buffers_written: u32,
    log_buffers_lost: u32,
    real_time_buffers_lost: u32,
    logger_thread_id: HANDLE,
    log_file_name_offset: u32,
    logger_name_offset: u32,
}

/// `EVENT_TRACE_PROPERTIES` followed immediately by inline UTF-16 session name storage.
#[repr(C)]
struct EventTracePropertiesBuf {
    props: EventTraceProperties,
    logger_name: [u16; MAX_SESSION_NAME_CHARS],
}

impl EventTracePropertiesBuf {
    fn new(session_name: &[u16]) -> Self {
        // SAFETY: Plain C POD zero initialization.
        let mut buf: Self = unsafe { zeroed() };
        buf.props.wnode.buffer_size = size_of::<Self>() as u32;
        buf.props.wnode.client_context = 1; // QPC timestamps
        buf.props.wnode.flags = WNODE_FLAG_TRACED_GUID;
        buf.props.log_file_mode = EVENT_TRACE_REAL_TIME_MODE;
        buf.props.buffer_size = 64; // 64 KB ETW buffers
        buf.props.minimum_buffers = 2;
        buf.props.maximum_buffers = 8;
        buf.props.flush_timer = 1; // flush every 1 second
        buf.props.logger_name_offset = size_of::<EventTraceProperties>() as u32;

        let copy_len = session_name.len().min(MAX_SESSION_NAME_CHARS - 1);
        buf.logger_name[..copy_len].copy_from_slice(&session_name[..copy_len]);
        buf.logger_name[copy_len] = 0;
        buf
    }
}

/// `EVENT_DESCRIPTOR` (`evntprov.h`, 16 bytes).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct EventDescriptor {
    id: u16,
    version: u8,
    channel: u8,
    level: u8,
    opcode: u8,
    task: u16,
    keyword: u64,
}

/// `EVENT_HEADER` (`evntcons.h`, 80 bytes).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct EventHeader {
    size: u16,
    header_type: u16,
    flags: u16,
    event_property: u16,
    thread_id: u32,
    process_id: u32,
    time_stamp: i64,
    provider_id: Guid,
    event_descriptor: EventDescriptor,
    kernel_time: u32,
    user_time: u32,
    activity_id: Guid,
}

/// `ETW_BUFFER_CONTEXT` (`evntrace.h`, 4 bytes).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct EtwBufferContext {
    processor_number: u8,
    alignment: u8,
    logger_id: u16,
}

/// `EVENT_RECORD` (`evntcons.h`, 112 bytes on 64-bit).
#[repr(C)]
#[derive(Clone, Copy)]
struct EventRecord {
    event_header: EventHeader,
    buffer_context: EtwBufferContext,
    extended_data_count: u16,
    user_data_length: u16,
    extended_data: *mut c_void,
    user_data: *mut c_void,
    user_context: *mut c_void,
}

/// `EVENT_TRACE_HEADER` (`evntrace.h`, 48 bytes).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct EventTraceHeader {
    size: u16,
    field_type_flags: u16,
    version: u32,
    thread_id: u32,
    process_id: u32,
    time_stamp: i64,
    guid: Guid,
    kernel_time: u32,
    user_time: u32,
}

/// `EVENT_TRACE` (`evntrace.h`, 88 bytes on 64-bit).
#[repr(C)]
#[derive(Clone, Copy)]
struct EventTrace {
    header: EventTraceHeader,
    instance_id: u32,
    parent_instance_id: u32,
    parent_guid: Guid,
    mof_data: *mut c_void,
    mof_length: u32,
    buffer_context: EtwBufferContext,
}

/// `SYSTEMTIME` (`minwinbase.h`, 16 bytes).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SystemTime {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}

/// `TIME_ZONE_INFORMATION` (`timezoneapi.h`, 172 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
struct TimeZoneInformation {
    bias: i32,
    standard_name: [u16; 32],
    standard_date: SystemTime,
    standard_bias: i32,
    daylight_name: [u16; 32],
    daylight_date: SystemTime,
    daylight_bias: i32,
}

/// `TRACE_LOGFILE_HEADER` (`evntrace.h`, 280 bytes on 64-bit).
#[repr(C)]
#[derive(Clone, Copy)]
struct TraceLogfileHeader {
    buffer_size: u32,
    version: u32,
    provider_version: u32,
    number_of_processors: u32,
    end_time: i64,
    timer_resolution: u32,
    maximum_file_size: u32,
    log_file_mode: u32,
    buffers_written: u32,
    log_instance_guid: Guid,
    logger_name: *mut u16,
    log_file_name: *mut u16,
    time_zone: TimeZoneInformation,
    boot_time: i64,
    perf_freq: i64,
    start_time: i64,
    reserved_flags: u32,
    buffers_lost: u32,
}

type BufferCallbackFn = unsafe extern "system" fn(logfile: *mut EventTraceLogfileW) -> u32;
type EventRecordCallbackFn = unsafe extern "system" fn(record: *mut EventRecord);

/// `EVENT_TRACE_LOGFILEW` (`evntrace.h`, 448 bytes on 64-bit).
#[repr(C)]
struct EventTraceLogfileW {
    log_file_name: *mut u16,
    logger_name: *mut u16,
    current_time: i64,
    buffers_read: u32,
    process_trace_mode: u32,
    current_event: EventTrace,
    logfile_header: TraceLogfileHeader,
    buffer_callback: Option<BufferCallbackFn>,
    buffer_size: u32,
    filled: u32,
    events_lost: u32,
    event_record_callback: Option<EventRecordCallbackFn>,
    is_kernel_trace: u32,
    context: *mut c_void,
}

type StartTraceWFn = unsafe extern "system" fn(
    trace_handle: *mut u64,
    instance_name: *const u16,
    properties: *mut EventTraceProperties,
) -> u32;

type ControlTraceWFn = unsafe extern "system" fn(
    trace_handle: u64,
    instance_name: *const u16,
    properties: *mut EventTraceProperties,
    control_code: u32,
) -> u32;

type EnableTraceEx2Fn = unsafe extern "system" fn(
    trace_handle: u64,
    provider_id: *const Guid,
    control_code: u32,
    level: u8,
    match_any_keyword: u64,
    match_all_keyword: u64,
    timeout: u32,
    enable_parameters: *const c_void,
) -> u32;

type OpenTraceWFn = unsafe extern "system" fn(logfile: *mut EventTraceLogfileW) -> u64;

type ProcessTraceFn = unsafe extern "system" fn(
    handle_array: *const u64,
    handle_count: u32,
    start_time: *const c_void,
    end_time: *const c_void,
) -> u32;

type CloseTraceFn = unsafe extern "system" fn(trace_handle: u64) -> u32;

/// Dynamically resolved ETW API table.
struct EtwApi {
    _module: OwnedModule,
    start_trace: StartTraceWFn,
    control_trace: ControlTraceWFn,
    enable_trace_ex2: EnableTraceEx2Fn,
    open_trace: OpenTraceWFn,
    process_trace: ProcessTraceFn,
    close_trace: CloseTraceFn,
}

// SAFETY: Resolved Win32 DLL function pointers and `HMODULE` are thread-safe to share.
unsafe impl Send for EtwApi {}
unsafe impl Sync for EtwApi {}

impl EtwApi {
    fn load() -> Result<Self, PalError> {
        let module =
            OwnedModule::load("advapi32.dll").or_else(|_| OwnedModule::load("sechost.dll"))?;

        let start_addr = module
            .proc_addr(b"StartTraceW\0")
            .ok_or(PalError::Unsupported("StartTraceW is unavailable"))?;
        let control_addr = module
            .proc_addr(b"ControlTraceW\0")
            .ok_or(PalError::Unsupported("ControlTraceW is unavailable"))?;
        let enable_addr = module
            .proc_addr(b"EnableTraceEx2\0")
            .ok_or(PalError::Unsupported("EnableTraceEx2 is unavailable"))?;
        let open_addr = module
            .proc_addr(b"OpenTraceW\0")
            .ok_or(PalError::Unsupported("OpenTraceW is unavailable"))?;
        let process_addr = module
            .proc_addr(b"ProcessTrace\0")
            .ok_or(PalError::Unsupported("ProcessTrace is unavailable"))?;
        let close_addr = module
            .proc_addr(b"CloseTrace\0")
            .ok_or(PalError::Unsupported("CloseTrace is unavailable"))?;

        // SAFETY: All symbol signatures match `evntrace.h` / `evntcons.h`.
        unsafe {
            Ok(Self {
                _module: module,
                start_trace: transmute::<*const c_void, StartTraceWFn>(start_addr),
                control_trace: transmute::<*const c_void, ControlTraceWFn>(control_addr),
                enable_trace_ex2: transmute::<*const c_void, EnableTraceEx2Fn>(enable_addr),
                open_trace: transmute::<*const c_void, OpenTraceWFn>(open_addr),
                process_trace: transmute::<*const c_void, ProcessTraceFn>(process_addr),
                close_trace: transmute::<*const c_void, CloseTraceFn>(close_addr),
            })
        }
    }
}

/// Direction of a parsed `Microsoft-Windows-Kernel-Network` event payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NetDirection {
    Recv,
    Send,
}

/// Shared state updated by the ETW consumer thread and read by the fast sampler.
#[derive(Default)]
struct EtwSharedState {
    stop_requested: AtomicBool,
    /// Cumulative `(recv_bytes, sent_bytes)` per PID.
    totals: Mutex<HashMap<u32, (u64, u64)>>,
}

struct ActiveEtwSession {
    api: Arc<EtwApi>,
    session_handle: u64,
    wide_session_name: Vec<u16>,
    worker: Option<JoinHandle<()>>,
}

/// Elevated-only private real-time ETW collector tracking per-PID network throughput.
///
/// Tracks cumulative `(recv_bytes, sent_bytes)` per PID from `Microsoft-Windows-Kernel-Network`
/// and computes overflow-safe delta rates via [`Self::take_delta_rates`].
pub(crate) struct EtwProcessNetCollector {
    shared: Arc<EtwSharedState>,
    active: Option<ActiveEtwSession>,
    prev_totals: HashMap<u32, (u64, u64)>,
}

impl Default for EtwProcessNetCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl EtwProcessNetCollector {
    /// Creates an idle collector (starts no ETW session and spawns zero threads).
    pub(crate) fn new() -> Self {
        Self {
            shared: Arc::new(EtwSharedState::default()),
            active: None,
            prev_totals: HashMap::new(),
        }
    }

    /// Returns `true` when the private real-time ETW session is currently running.
    #[allow(dead_code)]
    pub(crate) fn is_running(&self) -> bool {
        self.active.is_some()
    }

    /// Starts the private real-time ETW network session if and only if `is_elevated == true`.
    ///
    /// Calling `start` while already running is an idempotent no-op.
    #[allow(dead_code)]
    pub(crate) fn start(&mut self, is_elevated: bool) -> Result<(), PalError> {
        if !is_elevated {
            return Err(PalError::ElevationRequired);
        }
        if self.active.is_some() {
            return Ok(());
        }

        let api = Arc::new(EtwApi::load()?);
        let session_name = format!("LoomNetTrace-{}", std::process::id());
        let mut wide_session_name = to_wide_null(&session_name);

        // Stop any stale session left behind by an earlier crash of the same PID.
        let mut cleanup_props = EventTracePropertiesBuf::new(&wide_session_name);
        // SAFETY: `wide_session_name` and `cleanup_props` are valid initialized buffers.
        unsafe {
            (api.control_trace)(
                0,
                wide_session_name.as_ptr(),
                &mut cleanup_props.props,
                EVENT_TRACE_CONTROL_STOP,
            );
        }

        let mut start_props = EventTracePropertiesBuf::new(&wide_session_name);
        let mut session_handle: u64 = 0;
        // SAFETY: `wide_session_name` and `start_props` match `EVENT_TRACE_PROPERTIES` layout.
        let mut rc = unsafe {
            (api.start_trace)(
                &mut session_handle,
                wide_session_name.as_ptr(),
                &mut start_props.props,
            )
        };
        if rc == ERROR_ALREADY_EXISTS {
            let mut retry_cleanup = EventTracePropertiesBuf::new(&wide_session_name);
            // SAFETY: Best-effort cleanup of colliding session before retrying once.
            unsafe {
                (api.control_trace)(
                    0,
                    wide_session_name.as_ptr(),
                    &mut retry_cleanup.props,
                    EVENT_TRACE_CONTROL_STOP,
                );
            }
            start_props = EventTracePropertiesBuf::new(&wide_session_name);
            // SAFETY: Retry `StartTraceW` with fresh properties.
            rc = unsafe {
                (api.start_trace)(
                    &mut session_handle,
                    wide_session_name.as_ptr(),
                    &mut start_props.props,
                )
            };
        }
        if rc != ERROR_SUCCESS {
            if rc == ERROR_ACCESS_DENIED {
                return Err(PalError::ElevationRequired);
            }
            return Err(map_win32_error(rc, None, "StartTraceW"));
        }

        // Enable `Microsoft-Windows-Kernel-Network` provider on our private session.
        // SAFETY: `session_handle` is active and `KERNEL_NETWORK_PROVIDER_GUID` is valid.
        let enable_rc = unsafe {
            (api.enable_trace_ex2)(
                session_handle,
                &KERNEL_NETWORK_PROVIDER_GUID,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER,
                TRACE_LEVEL_INFORMATION,
                u64::MAX,
                0,
                0,
                ptr::null(),
            )
        };
        if enable_rc != ERROR_SUCCESS {
            stop_etw_controller(&api, session_handle, &wide_session_name);
            return Err(map_win32_error(enable_rc, None, "EnableTraceEx2"));
        }

        self.shared.stop_requested.store(false, Ordering::Release);
        self.shared.totals.lock().clear();
        self.prev_totals.clear();

        // SAFETY: `EventTraceLogfileW` is plain C POD zero-initialized before populating fields.
        let mut logfile: EventTraceLogfileW = unsafe { zeroed() };
        logfile.logger_name = wide_session_name.as_mut_ptr();
        logfile.process_trace_mode = PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
        logfile.buffer_callback = Some(etw_buffer_callback);
        logfile.event_record_callback = Some(etw_event_record_callback);
        // `self.shared` is cloned into the consumer thread and strictly outlives `ProcessTrace`.
        logfile.context = Arc::as_ptr(&self.shared) as *mut c_void;

        // SAFETY: `logfile` references live `wide_session_name` and `self.shared`.
        let trace_handle = unsafe { (api.open_trace)(&mut logfile) };
        if trace_handle == INVALID_PROCESSTRACE_HANDLE {
            stop_etw_controller(&api, session_handle, &wide_session_name);
            return Err(PalError::OsError {
                code: -1,
                message: "OpenTraceW failed for Loom real-time network session".into(),
            });
        }

        let worker_api = Arc::clone(&api);
        let worker_shared = Arc::clone(&self.shared);
        let worker = thread::Builder::new()
            .name("loom-etw-net".to_owned())
            .spawn(move || {
                // Keep `worker_shared` alive for the full duration of `ProcessTrace` so the raw
                // `logfile.context` pointer remains valid in callbacks.
                let _keep_alive = worker_shared;
                let handles = [trace_handle];
                // SAFETY: `trace_handle` was returned by `OpenTraceW` and callbacks reference `_keep_alive`.
                unsafe {
                    (worker_api.process_trace)(handles.as_ptr(), 1, ptr::null(), ptr::null());
                    (worker_api.close_trace)(trace_handle);
                }
            })
            .map_err(|err| {
                // SAFETY: Close trace handle and stop ETW session if thread spawn fails.
                unsafe {
                    (api.close_trace)(trace_handle);
                }
                stop_etw_controller(&api, session_handle, &wide_session_name);
                PalError::Io(err)
            })?;

        self.active = Some(ActiveEtwSession {
            api,
            session_handle,
            wide_session_name,
            worker: Some(worker),
        });
        Ok(())
    }

    /// Stops the private ETW session and joins the dedicated consumer thread.
    pub(crate) fn stop(&mut self) {
        self.shared.stop_requested.store(true, Ordering::Release);
        if let Some(mut session) = self.active.take() {
            stop_etw_controller(
                &session.api,
                session.session_handle,
                &session.wide_session_name,
            );
            if let Some(worker) = session.worker.take() {
                let _ = worker.join();
            }
        }
        self.shared.totals.lock().clear();
        self.prev_totals.clear();
    }

    /// Copies cumulative `(recv_bytes, sent_bytes)` counters per PID into caller-owned `dst`
    /// so steady-state fast ticks perform zero `HashMap` allocations.
    pub(crate) fn snapshot_totals_into(&self, dst: &mut HashMap<u32, (u64, u64)>) {
        dst.clear();
        let guard = self.shared.totals.lock();
        dst.extend(guard.iter().map(|(&k, &v)| (k, v)));
    }

    /// Computes per-PID `(recv_bytes_per_sec, sent_bytes_per_sec)` deltas since the previous call
    /// using overflow/underflow-safe saturating subtraction so counter resets never wrap.
    #[allow(dead_code)]
    pub(crate) fn take_delta_rates(&mut self, elapsed_secs: f64) -> HashMap<u32, (u64, u64)> {
        let current = self.shared.totals.lock().clone();
        compute_delta_rates(&mut self.prev_totals, &current, elapsed_secs)
    }
}

impl Drop for EtwProcessNetCollector {
    fn drop(&mut self) {
        self.stop();
    }
}

fn stop_etw_controller(api: &EtwApi, session_handle: u64, wide_session_name: &[u16]) {
    let mut stop_props = EventTracePropertiesBuf::new(wide_session_name);
    // SAFETY: Stops the private ETW session cleanly via `ControlTraceW`.
    unsafe {
        (api.control_trace)(
            session_handle,
            wide_session_name.as_ptr(),
            &mut stop_props.props,
            EVENT_TRACE_CONTROL_STOP,
        );
    }
}

/// Computes per-PID `(recv_bps, sent_bps)` rates from cumulative snapshots without underflow.
fn compute_delta_rates(
    prev_totals: &mut HashMap<u32, (u64, u64)>,
    current: &HashMap<u32, (u64, u64)>,
    elapsed_secs: f64,
) -> HashMap<u32, (u64, u64)> {
    let mut rates = HashMap::new();
    let valid_elapsed = elapsed_secs.is_finite() && elapsed_secs > 0.0;

    for (&pid, &(cur_recv, cur_sent)) in current {
        let (prev_recv, prev_sent) = prev_totals.get(&pid).copied().unwrap_or((0, 0));
        if valid_elapsed {
            let d_recv = cur_recv.saturating_sub(prev_recv);
            let d_sent = cur_sent.saturating_sub(prev_sent);
            if d_recv > 0 || d_sent > 0 {
                let recv_bps = (d_recv as f64 / elapsed_secs).round() as u64;
                let sent_bps = (d_sent as f64 / elapsed_secs).round() as u64;
                rates.insert(pid, (recv_bps, sent_bps));
            }
        }
    }

    prev_totals.clear();
    prev_totals.extend(current.iter().map(|(&k, &v)| (k, v)));
    rates
}

/// Parses a `Microsoft-Windows-Kernel-Network` event descriptor ID and payload into
/// `(pid, direction, byte_count)`.
///
/// Event IDs in `Microsoft-Windows-Kernel-Network`:
/// - `10` (TCPv4 Send), `26` (TCPv6 Send), `42` (UDPv4 Send), `58` (UDPv6 Send)
/// - `11` (TCPv4 Recv), `27` (TCPv6 Recv), `43` (UDPv4 Recv), `59` (UDPv6 Recv)
///
/// Each event payload starts with `{ uint32 PID; uint32 size; ... }`.
fn parse_network_event_payload(
    event_id: u16,
    header_pid: u32,
    user_data: &[u8],
) -> Option<(u32, NetDirection, u64)> {
    let direction = match event_id {
        10 | 26 | 42 | 58 => NetDirection::Send,
        11 | 27 | 43 | 59 => NetDirection::Recv,
        _ => return None,
    };

    if user_data.len() < 8 {
        return None;
    }

    let payload_pid = u32::from_le_bytes([user_data[0], user_data[1], user_data[2], user_data[3]]);
    let size = u32::from_le_bytes([user_data[4], user_data[5], user_data[6], user_data[7]]);
    let pid = if payload_pid != 0 {
        payload_pid
    } else {
        header_pid
    };

    if pid == 0 || size == 0 {
        return None;
    }

    Some((pid, direction, u64::from(size)))
}

unsafe extern "system" fn etw_buffer_callback(logfile: *mut EventTraceLogfileW) -> u32 {
    if logfile.is_null() {
        return 1;
    }
    // SAFETY: `logfile` is provided by ETW `ProcessTrace` and `context` points to `EtwSharedState`.
    let context = unsafe { (*logfile).context };
    if context.is_null() {
        return 1;
    }
    // SAFETY: `context` points to `EtwSharedState` kept alive by the worker thread.
    let shared = unsafe { &*(context as *const EtwSharedState) };
    if shared.stop_requested.load(Ordering::Acquire) {
        0 // Signal `ProcessTrace` to exit cleanly.
    } else {
        1
    }
}

unsafe extern "system" fn etw_event_record_callback(record: *mut EventRecord) {
    if record.is_null() {
        return;
    }
    // SAFETY: `record` is valid for the duration of this ETW callback.
    let rec = unsafe { &*record };
    if rec.user_context.is_null() || rec.user_data.is_null() || rec.user_data_length < 8 {
        return;
    }
    // SAFETY: `user_data` has `user_data_length` readable bytes per `EVENT_RECORD` contract.
    let payload = unsafe {
        std::slice::from_raw_parts(
            rec.user_data.cast::<u8>(),
            usize::from(rec.user_data_length),
        )
    };
    let Some((pid, dir, bytes)) = parse_network_event_payload(
        rec.event_header.event_descriptor.id,
        rec.event_header.process_id,
        payload,
    ) else {
        return;
    };

    // SAFETY: `user_context` points to `EtwSharedState` kept alive for the lifetime of `ProcessTrace`.
    let shared = unsafe { &*(rec.user_context as *const EtwSharedState) };
    if shared.stop_requested.load(Ordering::Relaxed) {
        return;
    }
    let mut guard = shared.totals.lock();
    let entry = guard.entry(pid).or_insert((0, 0));
    match dir {
        NetDirection::Recv => entry.0 = entry.0.saturating_add(bytes),
        NetDirection::Send => entry.1 = entry.1.saturating_add(bytes),
    }
}

#[cfg(test)]
mod tests {
    use std::mem::{offset_of, size_of};

    use super::*;

    #[test]
    fn abi_etw_struct_layouts_match_windows_sdk() {
        assert_eq!(size_of::<Guid>(), 16);
        assert_eq!(size_of::<WnodeHeader>(), 48);
        assert_eq!(size_of::<EventDescriptor>(), 16);
        assert_eq!(size_of::<EventHeader>(), 80);
        assert_eq!(size_of::<EtwBufferContext>(), 4);
        assert_eq!(size_of::<EventTraceHeader>(), 48);
        assert_eq!(size_of::<TimeZoneInformation>(), 172);

        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(size_of::<EventTraceProperties>(), 120);
            assert_eq!(offset_of!(EventTraceProperties, logger_name_offset), 116);
            assert_eq!(size_of::<EventRecord>(), 112);
            assert_eq!(offset_of!(EventRecord, user_data), 96);
            assert_eq!(offset_of!(EventRecord, user_context), 104);
            assert_eq!(size_of::<EventTrace>(), 88);
            assert_eq!(size_of::<TraceLogfileHeader>(), 280);
            assert_eq!(size_of::<EventTraceLogfileW>(), 448);
            assert_eq!(offset_of!(EventTraceLogfileW, buffer_callback), 400);
            assert_eq!(offset_of!(EventTraceLogfileW, event_record_callback), 424);
            assert_eq!(offset_of!(EventTraceLogfileW, context), 440);
        }
    }

    #[test]
    fn start_refuses_non_elevated_caller_without_spawning_threads() {
        let mut collector = EtwProcessNetCollector::new();
        assert!(!collector.is_running());
        let err = collector
            .start(false)
            .expect_err("non-elevated start must fail");
        assert!(matches!(err, PalError::ElevationRequired));
        assert!(!collector.is_running());
    }

    #[test]
    fn parse_network_event_payload_decodes_tcp_udp_send_recv_and_rejects_short() {
        let mut payload = [0u8; 8];
        payload[0..4].copy_from_slice(&4242u32.to_le_bytes());
        payload[4..8].copy_from_slice(&1500u32.to_le_bytes());

        assert_eq!(
            parse_network_event_payload(10, 99, &payload),
            Some((4242, NetDirection::Send, 1500))
        );
        assert_eq!(
            parse_network_event_payload(11, 99, &payload),
            Some((4242, NetDirection::Recv, 1500))
        );
        assert_eq!(
            parse_network_event_payload(26, 99, &payload),
            Some((4242, NetDirection::Send, 1500))
        );
        assert_eq!(
            parse_network_event_payload(59, 99, &payload),
            Some((4242, NetDirection::Recv, 1500))
        );

        // Falls back to header_pid when payload_pid is 0.
        payload[0..4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            parse_network_event_payload(43, 777, &payload),
            Some((777, NetDirection::Recv, 1500))
        );

        // Unknown event ID or truncated payload is ignored cleanly.
        assert_eq!(parse_network_event_payload(999, 777, &payload), None);
        assert_eq!(parse_network_event_payload(10, 777, &payload[..4]), None);
    }

    #[test]
    fn compute_delta_rates_uses_checked_sub_and_handles_resets_and_zero_elapsed() {
        let mut prev = HashMap::new();
        prev.insert(100, (1_000, 500));
        prev.insert(200, (5_000, 5_000));

        let mut cur = HashMap::new();
        cur.insert(100, (3_000, 1_500)); // +2000 recv, +1000 sent
        cur.insert(200, (100, 200)); // counter reset -> checked_sub -> 0
        cur.insert(300, (400, 0)); // brand-new PID -> +400 recv

        // Zero or non-finite elapsed returns empty rates without panicking, still updates prev.
        let mut prev_zero = prev.clone();
        assert!(compute_delta_rates(&mut prev_zero, &cur, 0.0).is_empty());
        assert_eq!(prev_zero.get(&100), Some(&(3_000, 1_500)));

        // Valid 2.0s elapsed yields exact bytes/sec rates and skips reset PID 200.
        let rates = compute_delta_rates(&mut prev, &cur, 2.0);
        assert_eq!(rates.get(&100), Some(&(1_000, 500)));
        assert_eq!(rates.get(&200), None);
        assert_eq!(rates.get(&300), Some(&(200, 0)));
    }
}
