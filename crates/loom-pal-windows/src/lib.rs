//! Windows native Platform Abstraction Layer (PAL) backend for Loom.
//!
//! Strictly observes the Loom Windows anti-hang red lines:
//! - No WMI or PDH dependencies.
//! - No cross-process `GetWindowTextW` or blocking `SendMessageW` (uses exclusively
//!   `InternalGetWindowText`, `IsHungAppWindow`, and `DwmGetWindowAttribute`).
//! - No cross-process `ReadProcessMemory` for PEB inspection (command lines use
//!   `NtQueryInformationProcess(ProcessCommandLineInformation)`).
//! - Slow disk/Shell/PE/IOCTL queries isolated behind a `300 ms` [`CircuitBreaker`]
//!   with mandatory UNC / `DRIVE_REMOTE` network drive bypass.
//! - Process handles are never retained across ticks; every handle is scoped to an
//!   RAII guard and validated against [`loom_types::ProcessKey::create_time_100ns`].

#![warn(missing_docs)]

pub mod circuit_breaker;
pub mod fast_sampler;
pub(crate) mod handle;
pub mod operator;
pub mod slow_prober;

pub use circuit_breaker::CircuitBreaker;
pub use fast_sampler::WindowsFastSampler;
pub use operator::WindowsSystemOperator;
pub use slow_prober::WindowsSlowProber;

use loom_pal_api::{
    AppHistoryReader, PalError, PlatformBackend, ServiceManager, SessionManager, StartupManager,
};
use loom_types::{
    AppHistoryRecord, CapabilityFlags, ServiceRecord, StartupEntry, UserSessionRecord,
};

/// Windows native PAL backend combining dual-lane sampling with core process control.
#[derive(Debug, Default)]
pub struct WindowsBackend {
    service_mgr: WindowsServiceManager,
    startup_mgr: WindowsStartupManager,
    session_mgr: WindowsSessionManager,
    history_reader: WindowsAppHistoryReader,
    operator: WindowsSystemOperator,
}

/// Windows SCM service manager (scheduled for `0.0.5`).
#[derive(Debug, Default)]
pub struct WindowsServiceManager;

/// Windows startup application manager (scheduled for `0.0.5`).
#[derive(Debug, Default)]
pub struct WindowsStartupManager;

/// Windows WTS user session manager (scheduled for `0.0.6`).
#[derive(Debug, Default)]
pub struct WindowsSessionManager;

/// Windows application usage history reader (scheduled for `0.0.6`).
#[derive(Debug, Default)]
pub struct WindowsAppHistoryReader;

impl PlatformBackend for WindowsBackend {
    type ProcessExt = ();
    type FastSampler = WindowsFastSampler;
    type SlowProber = WindowsSlowProber;
    type ServiceMgr = WindowsServiceManager;
    type StartupMgr = WindowsStartupManager;
    type SessionMgr = WindowsSessionManager;
    type HistoryReader = WindowsAppHistoryReader;
    type Operator = WindowsSystemOperator;

    const CAPABILITIES: CapabilityFlags = CapabilityFlags::ECO_MODE
        .union(CapabilityFlags::CPU_AFFINITY)
        .union(CapabilityFlags::PROCESS_PRIORITY)
        .union(CapabilityFlags::ELEVATION_RELAUNCH);

    fn boost_self_priority() {
        fast_sampler::boost_self_priority();
    }

    fn initialize() -> Result<Self, PalError> {
        Ok(Self::default())
    }

    fn create_fast_sampler(&self) -> Result<Self::FastSampler, PalError> {
        Ok(WindowsFastSampler::new())
    }

    fn create_slow_prober(&self) -> Result<Self::SlowProber, PalError> {
        Ok(WindowsSlowProber::new())
    }

    fn service_manager(&self) -> &Self::ServiceMgr {
        &self.service_mgr
    }

    fn startup_manager(&self) -> &Self::StartupMgr {
        &self.startup_mgr
    }

    fn session_manager(&self) -> &Self::SessionMgr {
        &self.session_mgr
    }

    fn history_reader(&self) -> &Self::HistoryReader {
        &self.history_reader
    }

    fn operator(&self) -> &Self::Operator {
        &self.operator
    }
}

impl ServiceManager for WindowsServiceManager {
    fn list_services(&self) -> Result<Vec<ServiceRecord>, PalError> {
        Err(PalError::Unsupported(
            "windows service enumeration is scheduled for v0.0.5",
        ))
    }

    fn start_service(&self, _name: &str) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "windows service control is scheduled for v0.0.5",
        ))
    }

    fn stop_service(&self, _name: &str) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "windows service control is scheduled for v0.0.5",
        ))
    }

    fn restart_service(&self, _name: &str) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "windows service control is scheduled for v0.0.5",
        ))
    }

    fn open_services_console(&self) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "windows services console is scheduled for v0.0.5",
        ))
    }
}

impl StartupManager for WindowsStartupManager {
    fn list_startup_entries(&self) -> Result<(Option<u64>, Vec<StartupEntry>), PalError> {
        Err(PalError::Unsupported(
            "windows startup enumeration is scheduled for v0.0.5",
        ))
    }

    fn set_startup_enabled(&self, _id: &str, _enabled: bool) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "windows startup toggle is scheduled for v0.0.5",
        ))
    }
}

impl SessionManager for WindowsSessionManager {
    fn list_sessions(&self) -> Result<Vec<UserSessionRecord>, PalError> {
        Err(PalError::Unsupported(
            "windows session enumeration is scheduled for v0.0.6",
        ))
    }

    fn disconnect_session(&self, _session_id: u32) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "windows session disconnect is scheduled for v0.0.6",
        ))
    }
}

impl AppHistoryReader for WindowsAppHistoryReader {
    fn read_history(&self) -> Result<(Option<u64>, Vec<AppHistoryRecord>), PalError> {
        Err(PalError::Unsupported(
            "windows app history reader is scheduled for v0.0.6",
        ))
    }

    fn clear_history(&self) -> Result<(), PalError> {
        Err(PalError::Unsupported(
            "windows app history clear is scheduled for v0.0.6",
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use loom_pal_api::{
        FastSystemSampler, PerfSampleScratch, ProcessSampleScratch, SlowMetadataProber,
        SystemOperator,
    };
    use loom_types::{ProcessArch, ProcessKey};

    use super::*;

    #[test]
    fn fast_sampler_captures_current_process_and_reuses_scratch_within_budget() {
        let backend = WindowsBackend::initialize().expect("WindowsBackend::initialize");
        let mut sampler = backend.create_fast_sampler().expect("create_fast_sampler");

        // Pre-size scratch generously so warm ticks perform zero scratch reallocations.
        let mut scratch = ProcessSampleScratch::<()>::with_capacity(1024, 64 * 1024);

        // Cold sample warms internal kernel buffers and establishes CPU counter baseline.
        let ts1 = sampler
            .sample_processes(&mut scratch)
            .expect("first sample_processes");
        assert!(ts1.as_raw() > 0);
        assert!(!scratch.samples.is_empty());

        let samples_cap_after_first = scratch.samples.capacity();
        let strings_cap_after_first = scratch.string_buf.capacity();

        // Warm sample: measure wall-clock latency and assert zero scratch reallocation.
        let start = Instant::now();
        let ts2 = sampler
            .sample_processes(&mut scratch)
            .expect("warm sample_processes");
        let elapsed = start.elapsed();
        eprintln!(
            "[loom-pal-windows] warm sample_processes: {:.3} ms ({} processes)",
            elapsed.as_secs_f64() * 1000.0,
            scratch.samples.len()
        );

        #[cfg(debug_assertions)]
        const WARM_BUDGET: Duration = Duration::from_millis(25);
        #[cfg(not(debug_assertions))]
        const WARM_BUDGET: Duration = Duration::from_millis(10);

        assert!(ts2 >= ts1);
        assert_eq!(
            scratch.samples.capacity(),
            samples_cap_after_first,
            "scratch.samples unexpectedly reallocated on warm tick"
        );
        assert_eq!(
            scratch.string_buf.capacity(),
            strings_cap_after_first,
            "scratch.string_buf unexpectedly reallocated on warm tick"
        );
        assert!(
            elapsed < WARM_BUDGET,
            "warm fast-lane scan exceeded {:?} budget: {elapsed:?}",
            WARM_BUDGET
        );

        let my_pid = std::process::id();
        let me = scratch
            .samples
            .iter()
            .find(|s| s.key.pid == my_pid)
            .expect("current test process must appear in snapshot");

        let name = scratch.str_slice(me.name_offset, me.name_len);
        assert!(
            !name.is_empty(),
            "current process image name must be non-empty"
        );
        assert!(
            me.key.create_time_100ns > 0,
            "current process must have non-zero create_time_100ns"
        );
        assert!(
            me.working_set_bytes > 0,
            "current process must have non-zero working_set_bytes"
        );
        assert!(
            me.commit_bytes > 0,
            "current process must have non-zero commit_bytes"
        );
        assert!(scratch.perf.mem_total_bytes > 0);
        assert!(scratch.perf.logical_cpu_count >= 1);
        assert!(scratch.perf.uptime_secs > 0);
    }

    #[test]
    fn fast_sampler_performance_captures_per_core_and_memory_counters() {
        let mut sampler = WindowsFastSampler::new();
        let mut perf = PerfSampleScratch::new();

        let ts1 = sampler
            .sample_performance(&mut perf)
            .expect("baseline sample_performance");
        assert!(ts1.as_raw() > 0);
        assert!(perf.sample.mem_total_bytes > 0);
        assert!(perf.sample.mem_used_bytes > 0);
        assert!(perf.sample.logical_cpu_count >= 1);
        assert_eq!(
            perf.per_core_permille.len(),
            perf.sample.logical_cpu_count as usize
        );

        let cap = perf.per_core_permille.capacity();
        std::thread::sleep(Duration::from_millis(15));

        let ts2 = sampler
            .sample_performance(&mut perf)
            .expect("delta sample_performance");
        assert!(ts2 >= ts1);
        assert_eq!(perf.per_core_permille.capacity(), cap);
        assert!(perf.sample.cpu_total_permille <= 1000);
        assert!(perf.sample.cpu_kernel_permille <= 1000);
        assert!(perf.per_core_permille.iter().all(|&v| v <= 1000));
    }

    #[test]
    fn slow_prober_probes_current_process_and_enforces_identity_check() {
        let mut fast = WindowsFastSampler::new();
        let mut scratch = ProcessSampleScratch::<()>::with_capacity(512, 32 * 1024);
        fast.sample_processes(&mut scratch)
            .expect("sample_processes");

        let my_pid = std::process::id();
        let my_key = scratch
            .samples
            .iter()
            .find(|s| s.key.pid == my_pid)
            .expect("current process in fast sample")
            .key;

        let prober = WindowsSlowProber::new();
        let meta = prober
            .probe_process(my_key)
            .expect("probe_process on current process must succeed");

        assert_eq!(meta.key, my_key);
        let exe_path = meta
            .exe_path
            .as_deref()
            .expect("current process exe_path must be present");
        assert!(!exe_path.is_empty());
        assert!(
            meta.command_line.is_some(),
            "current process command_line should be readable via ProcessCommandLineInformation"
        );
        assert_ne!(
            meta.arch,
            ProcessArch::Unknown,
            "current process arch must resolve via IsWow64Process2"
        );
        assert!(
            meta.user_name.as_deref().is_some_and(|u| !u.is_empty()),
            "current process user_name must resolve via LookupAccountSidW"
        );

        // Passing a mismatched create_time_100ns MUST return ProcessIdentityMismatch.
        let wrong_key = ProcessKey::new(my_pid, my_key.create_time_100ns.wrapping_add(12345));
        let err = prober
            .probe_process(wrong_key)
            .expect_err("wrong create_time_100ns must fail identity check");
        assert!(
            matches!(err, PalError::ProcessIdentityMismatch(k) if k == wrong_key),
            "expected ProcessIdentityMismatch, got {err:?}"
        );
    }

    #[test]
    fn slow_prober_extracts_current_exe_asset_and_bypasses_unc() {
        let prober = WindowsSlowProber::new();
        let current_exe = std::env::current_exe().expect("std::env::current_exe");
        let exe_str = current_exe.to_string_lossy();

        let asset = prober
            .probe_exe_asset(&exe_str)
            .expect("probe_exe_asset on current_exe must succeed");
        assert_eq!(asset.normalized_path.as_ref(), exe_str.as_ref());
        assert!(!asset.is_network_path);
        if let Some(icon) = &asset.icon {
            assert_eq!(icon.width, 20);
            assert_eq!(icon.height, 20);
            assert_eq!(icon.rgba.len(), 20 * 20 * 4);
        }

        // Also verify PE VersionInfo + 20x20 RGBA8 icon extraction against a standard Windows binary.
        let system_cmd = std::path::Path::new(r"C:\Windows\System32\cmd.exe");
        if system_cmd.exists() {
            let cmd_asset = prober
                .probe_exe_asset(r"C:\Windows\System32\cmd.exe")
                .expect("probe_exe_asset on cmd.exe must succeed");
            assert!(!cmd_asset.is_network_path);
            assert!(
                cmd_asset.publisher.is_some(),
                "cmd.exe should expose PE CompanyName"
            );
            assert!(
                cmd_asset.file_description.is_some(),
                "cmd.exe should expose PE FileDescription"
            );
            let icon = cmd_asset
                .icon
                .as_ref()
                .expect("cmd.exe should produce a 20x20 RGBA8 icon");
            assert_eq!(icon.width, 20);
            assert_eq!(icon.height, 20);
            assert_eq!(icon.rgba.len(), 20 * 20 * 4);
        }

        // UNC path must bypass disk reads immediately and report is_network_path = true.
        let unc_asset = prober
            .probe_exe_asset(r"\\nonexistent-server\nonexistent-share\demo.exe")
            .expect("UNC probe must bypass network I/O cleanly");
        assert!(unc_asset.is_network_path);
        assert!(unc_asset.publisher.is_none());
        assert!(unc_asset.file_description.is_none());
    }

    #[test]
    fn system_operator_queries_host_info_and_verifies_process_identity() {
        let op = WindowsSystemOperator::new();
        assert!(op.logical_cpu_count() >= 1);
        // Just verify `is_elevated` executes cleanly without panicking.
        let _elevated = op.is_elevated();

        let mut fast = WindowsFastSampler::new();
        let mut scratch = ProcessSampleScratch::<()>::with_capacity(512, 32 * 1024);
        fast.sample_processes(&mut scratch)
            .expect("sample_processes");

        let my_pid = std::process::id();
        let my_key = scratch
            .samples
            .iter()
            .find(|s| s.key.pid == my_pid)
            .expect("current process")
            .key;

        let affinity = op.get_affinity(my_key).expect("get_affinity on self");
        assert!(!affinity.is_empty());
        assert_eq!(affinity.logical_cpu_count, op.logical_cpu_count());

        // Attempting to terminate self with a wrong create_time MUST fail with ProcessIdentityMismatch
        // BEFORE calling TerminateProcess!
        let bad_key = ProcessKey::new(my_pid, my_key.create_time_100ns.wrapping_add(999));
        let err = op
            .terminate_process(bad_key)
            .expect_err("terminate_process with wrong create_time must fail");
        assert!(matches!(err, PalError::ProcessIdentityMismatch(k) if k == bad_key));
    }

    #[test]
    fn slow_prober_reads_efficiency_mode_and_elevation_on_current_process() {
        let op = WindowsSystemOperator::new();
        let mut fast = WindowsFastSampler::new();
        let mut scratch = ProcessSampleScratch::<()>::with_capacity(512, 32 * 1024);
        fast.sample_processes(&mut scratch)
            .expect("sample_processes");

        let my_pid = std::process::id();
        let my_key = scratch
            .samples
            .iter()
            .find(|s| s.key.pid == my_pid)
            .expect("current process in fast sample")
            .key;

        let prober = WindowsSlowProber::new();
        let meta = prober
            .probe_process(my_key)
            .expect("probe_process on current process");

        assert_eq!(
            meta.is_elevated,
            op.is_elevated(),
            "SlowProber TokenElevation must match SystemOperator::is_elevated for self"
        );
        // Verify efficiency_mode field is cleanly populated without panicking.
        let _eco = meta.efficiency_mode;
    }

    #[test]
    fn system_operator_rejects_empty_affinity_and_protects_kernel_processes() {
        let op = WindowsSystemOperator::new();
        let mut fast = WindowsFastSampler::new();
        let mut scratch = ProcessSampleScratch::<()>::with_capacity(512, 32 * 1024);
        fast.sample_processes(&mut scratch)
            .expect("sample_processes");

        let my_pid = std::process::id();
        let my_key = scratch
            .samples
            .iter()
            .find(|s| s.key.pid == my_pid)
            .expect("current process")
            .key;

        let empty_mask = loom_types::CpuAffinityMask::empty(op.logical_cpu_count());
        let err = op
            .set_affinity(my_key, &empty_mask)
            .expect_err("empty CPU affinity mask must be rejected");
        assert!(
            matches!(err, PalError::InvalidArgument(_)),
            "expected PalError::InvalidArgument, got {err:?}"
        );

        // PID 0 (System Idle) and PID 4 (System) must be gracefully rejected without panicking.
        for kernel_pid in [0u32, 4u32] {
            let key = scratch
                .samples
                .iter()
                .find(|s| s.key.pid == kernel_pid)
                .map_or_else(|| ProcessKey::new(kernel_pid, 0), |s| s.key);
            let eco_err = op
                .set_efficiency_mode(key, true)
                .expect_err("kernel process EcoQoS toggle must be rejected");
            assert!(
                matches!(
                    eco_err,
                    PalError::Unsupported(_) | PalError::ElevationRequired
                ),
                "unexpected error for PID {kernel_pid}: {eco_err:?}"
            );
        }
    }

    #[test]
    fn child_process_priority_efficiency_mode_and_terminate_lifecycle() {
        use std::process::{Child, Command, Stdio};

        struct ChildGuard(Option<Child>);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                if let Some(mut child) = self.0.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }

        // Spawn a harmless, silent child process that waits ~30s without requiring stdin.
        let child = Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn harmless ping child process");
        let child_pid = child.id();
        let mut guard = ChildGuard(Some(child));

        let mut fast = WindowsFastSampler::new();
        let mut scratch = ProcessSampleScratch::<()>::with_capacity(512, 32 * 1024);

        // Locate child ProcessKey from WindowsFastSampler.
        let deadline = Instant::now() + Duration::from_secs(2);
        let child_key = loop {
            fast.sample_processes(&mut scratch)
                .expect("sample_processes");
            if let Some(sample) = scratch.samples.iter().find(|s| s.key.pid == child_pid) {
                break sample.key;
            }
            assert!(
                Instant::now() < deadline,
                "spawned child PID {child_pid} did not appear in fast sample"
            );
            std::thread::sleep(Duration::from_millis(10));
        };

        let op = WindowsSystemOperator::new();
        let prober = WindowsSlowProber::new();

        // 1. Verify set_priority succeeds and is reflected in fast sample.
        op.set_priority(child_key, loom_types::ProcessPriority::BelowNormal)
            .expect("set_priority(BelowNormal) on child");
        fast.sample_processes(&mut scratch)
            .expect("sample_processes after set_priority");
        let after_prio = scratch
            .samples
            .iter()
            .find(|s| s.key == child_key)
            .expect("child process still alive after set_priority");
        assert_eq!(
            after_prio.priority,
            loom_types::ProcessPriority::BelowNormal
        );

        // 2. Verify set_efficiency_mode(true) -> probe_process sees efficiency_mode == true
        //    -> set_efficiency_mode(false) -> probe_process sees efficiency_mode == false.
        op.set_efficiency_mode(child_key, true)
            .expect("set_efficiency_mode(true) on child");
        let meta_on = prober
            .probe_process(child_key)
            .expect("probe_process after enabling EcoQoS");
        assert!(
            meta_on.efficiency_mode,
            "probe_process must report efficiency_mode == true after set_efficiency_mode(true)"
        );

        op.set_efficiency_mode(child_key, false)
            .expect("set_efficiency_mode(false) on child");
        let meta_off = prober
            .probe_process(child_key)
            .expect("probe_process after disabling EcoQoS");
        assert!(
            !meta_off.efficiency_mode,
            "probe_process must report efficiency_mode == false after set_efficiency_mode(false)"
        );

        // 3. Verify terminate_process kills the child cleanly.
        op.terminate_process(child_key)
            .expect("terminate_process on child");
        let status = guard
            .0
            .as_mut()
            .expect("child handle")
            .wait()
            .expect("wait on terminated child");
        guard.0 = None;
        assert!(!status.success(), "terminated child should exit non-zero");
    }
}
