//! Static Platform Abstraction Layer (PAL) contracts for Loom.
//!
//! This crate defines OS-independent traits, reusable zero-allocation scratch buffers,
//! and structured [`PalError`] types. It depends exclusively on `loom-types` and
//! `thiserror`, exposing no OS handles or UI toolkit types.
//!
//! # Frozen `0.1.0` Cross-Platform Static Contract
//!
//! Starting in Loom `0.1.0` ([`PAL_API_CONTRACT_VERSION`] `= 1`), the 8 core PAL
//! traits ([`PlatformBackend`], [`FastSystemSampler`], [`SlowMetadataProber`],
//! [`ServiceManager`], [`StartupManager`], [`SessionManager`], [`AppHistoryReader`],
//! and [`SystemOperator`]) together with caller-reused [`ProcessSampleScratch`] /
//! [`PerfSampleScratch`] arenas form the frozen compile-time static-dispatch
//! contract for `loom-pal-windows` and upcoming `0.2.0` (`loom-pal-linux`) and
//! `0.3.0` (`loom-pal-macos`) backends. Hot paths use associated types and `Copy`
//! sample structs without `dyn Trait` vtables or per-tick heap allocations.

#![warn(missing_docs)]

pub mod error;
pub mod scratch;
pub mod traits;

/// Frozen cross-platform PAL trait contract version established in Loom `0.1.0`.
///
/// Bumped only on breaking changes to the static PAL trait suite or scratch-buffer
/// contracts across Windows, Linux (`0.2.0`), and macOS (`0.3.0`) backends.
pub const PAL_API_CONTRACT_VERSION: u32 = 1;

pub use error::PalError;
pub use scratch::{PerfSampleScratch, ProcessSampleScratch};
pub use traits::{
    AppHistoryReader, FastSystemSampler, PlatformBackend, ServiceManager, SessionManager,
    SlowMetadataProber, StartupManager, SystemOperator,
};

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;

    use loom_types::{
        AppHistoryRecord, CapabilityFlags, CpuAffinityMask, DiskDeviceSample, ExeStaticAsset,
        HardwareStaticInfo, PlatformKind, PlatformProfile, ProcessColdMeta, ProcessHotSample,
        ProcessKey, ProcessPriority, RunTaskRequest, ServiceRecord, StartupEntry, SystemPerfSample,
        Timestamp100Ns, TreeKillReport, UserSessionRecord, WaitChainNode,
    };

    use super::*;

    #[test]
    fn process_sample_scratch_reuses_capacity_and_slices_strings() {
        let mut scratch = ProcessSampleScratch::<()>::with_capacity(16, 128);
        let (off1, len1) = scratch.push_str("explorer.exe");
        let (off2, len2) = scratch.push_str("Loom 任务管理器");
        let (off3, len3) = scratch.push_str("");

        assert_eq!(scratch.str_slice(off1, len1), "explorer.exe");
        assert_eq!(scratch.str_slice(off2, len2), "Loom 任务管理器");
        assert_eq!(scratch.str_slice(off3, len3), "");
        assert_eq!(scratch.str_slice(9999, 10), "");

        let prev_cap = scratch.string_buf.capacity();
        scratch.clear();
        assert!(scratch.samples.is_empty());
        assert!(scratch.string_buf.is_empty());
        assert_eq!(scratch.string_buf.capacity(), prev_cap);

        // Multi-byte UTF-8 strings exceeding u16::MAX truncate cleanly on a char boundary.
        let oversized = "汉".repeat(25_000); // 75_000 bytes (> u16::MAX = 65_535)
        let (off_big, len_big) = scratch.push_str(&oversized);
        let slice = scratch.str_slice(off_big, len_big);
        assert_eq!(usize::from(len_big), slice.len());
        assert!(slice.len() <= usize::from(u16::MAX));
        assert_eq!(slice.len() % 3, 0);
        assert!(slice.chars().all(|ch| ch == '汉'));
    }

    #[test]
    fn perf_sample_scratch_clear_retains_capacity() {
        let mut scratch = PerfSampleScratch::new();
        scratch
            .per_core_permille
            .extend_from_slice(&[100, 200, 300]);
        scratch
            .networks
            .push(loom_types::NetAdapterSample::default());
        scratch.gpus.push(loom_types::GpuAdapterSample::default());
        let cap = scratch.per_core_permille.capacity();
        let net_cap = scratch.networks.capacity();
        let gpu_cap = scratch.gpus.capacity();
        scratch.clear();
        assert!(scratch.per_core_permille.is_empty());
        assert!(scratch.networks.is_empty());
        assert!(scratch.gpus.is_empty());
        assert_eq!(scratch.per_core_permille.capacity(), cap);
        assert_eq!(scratch.networks.capacity(), net_cap);
        assert_eq!(scratch.gpus.capacity(), gpu_cap);
    }

    #[test]
    fn pal_api_v1_static_contract_freeze() {
        assert_eq!(PAL_API_CONTRACT_VERSION, 1);

        // Compile-time Copy + thread-safety bounds on hot sample structs.
        const fn assert_hot_copy<T: Copy + Send + Sync + 'static>() {}
        const fn assert_hot_copy_default<T: Copy + Default + Send + Sync + 'static>() {}

        assert_hot_copy::<ProcessKey>();
        assert_hot_copy_default::<ProcessHotSample<()>>();
        assert_hot_copy_default::<SystemPerfSample>();

        // Compile-time bounds across all 8 frozen PAL traits and associated types.
        fn assert_frozen_backend_contract<B>()
        where
            B: PlatformBackend + Send + Sync + 'static,
            B::ProcessExt: Copy + Default + Send + Sync + 'static,
            B::FastSampler: FastSystemSampler<Ext = B::ProcessExt> + Send,
            B::SlowProber: SlowMetadataProber + Send + Sync,
            B::ServiceMgr: ServiceManager + Send + Sync,
            B::StartupMgr: StartupManager + Send + Sync,
            B::SessionMgr: SessionManager + Send + Sync,
            B::HistoryReader: AppHistoryReader + Send + Sync,
            B::Operator: SystemOperator + Send + Sync,
        {
            let _profile: PlatformProfile = B::PLATFORM;
            let _caps: CapabilityFlags = B::CAPABILITIES;
            B::boost_self_priority();
            B::set_main_window_always_on_top(false);
            let _minimized: bool = B::is_main_window_minimized();
            B::restore_and_focus_main_window();
        }

        // Zero-sized witness backend verifying every trait method signature stays intact.
        struct WitnessFastSampler;
        struct WitnessSlowProber;
        struct WitnessServices;
        struct WitnessStartup;
        struct WitnessSessions;
        struct WitnessHistory;
        struct WitnessOperator;

        impl FastSystemSampler for WitnessFastSampler {
            type Ext = ();

            fn sample_processes(
                &mut self,
                _scratch: &mut ProcessSampleScratch<Self::Ext>,
            ) -> Result<Timestamp100Ns, PalError> {
                Ok(Timestamp100Ns::new(0))
            }

            fn sample_performance(
                &mut self,
                _scratch: &mut PerfSampleScratch,
            ) -> Result<Timestamp100Ns, PalError> {
                Ok(Timestamp100Ns::new(0))
            }
        }

        impl SlowMetadataProber for WitnessSlowProber {
            fn probe_process(&self, _key: ProcessKey) -> Result<ProcessColdMeta, PalError> {
                Err(PalError::Unsupported("contract witness"))
            }

            fn probe_exe_asset(&self, _exe_path: &str) -> Result<ExeStaticAsset, PalError> {
                Err(PalError::Unsupported("contract witness"))
            }

            fn probe_hardware_static_info(&self) -> Result<HardwareStaticInfo, PalError> {
                Ok(HardwareStaticInfo::default())
            }

            fn probe_disk_performance(&self) -> Result<Vec<DiskDeviceSample>, PalError> {
                Ok(Vec::new())
            }
        }

        impl ServiceManager for WitnessServices {
            fn list_services(&self) -> Result<Vec<ServiceRecord>, PalError> {
                Ok(Vec::new())
            }
            fn start_service(&self, _name: &str) -> Result<(), PalError> {
                Ok(())
            }
            fn stop_service(&self, _name: &str) -> Result<(), PalError> {
                Ok(())
            }
            fn restart_service(&self, _name: &str) -> Result<(), PalError> {
                Ok(())
            }
            fn open_services_console(&self) -> Result<(), PalError> {
                Ok(())
            }
        }

        impl StartupManager for WitnessStartup {
            fn list_startup_entries(&self) -> Result<(Option<u64>, Vec<StartupEntry>), PalError> {
                Ok((None, Vec::new()))
            }
            fn set_startup_enabled(&self, _id: &str, _enabled: bool) -> Result<(), PalError> {
                Ok(())
            }
        }

        impl SessionManager for WitnessSessions {
            fn list_sessions(&self) -> Result<Vec<UserSessionRecord>, PalError> {
                Ok(Vec::new())
            }
            fn disconnect_session(&self, _session_id: u32) -> Result<(), PalError> {
                Ok(())
            }
        }

        impl AppHistoryReader for WitnessHistory {
            fn read_history(&self) -> Result<(Option<u64>, Vec<AppHistoryRecord>), PalError> {
                Ok((None, Vec::new()))
            }
            fn clear_history(&self) -> Result<(), PalError> {
                Ok(())
            }
        }

        impl SystemOperator for WitnessOperator {
            fn run_new_task(&self, _req: &RunTaskRequest) -> Result<Option<u32>, PalError> {
                Ok(None)
            }
            fn terminate_process(&self, _key: ProcessKey) -> Result<(), PalError> {
                Ok(())
            }
            fn terminate_process_tree(
                &self,
                _targets: &[ProcessKey],
            ) -> Result<TreeKillReport, PalError> {
                Err(PalError::Unsupported("contract witness"))
            }
            fn set_efficiency_mode(
                &self,
                _key: ProcessKey,
                _enabled: bool,
            ) -> Result<(), PalError> {
                Ok(())
            }
            fn set_priority(
                &self,
                _key: ProcessKey,
                _priority: ProcessPriority,
            ) -> Result<(), PalError> {
                Ok(())
            }
            fn get_affinity(&self, _key: ProcessKey) -> Result<CpuAffinityMask, PalError> {
                Ok(CpuAffinityMask::all(1))
            }
            fn set_affinity(
                &self,
                _key: ProcessKey,
                _mask: &CpuAffinityMask,
            ) -> Result<(), PalError> {
                Ok(())
            }
            fn set_uac_virtualization(
                &self,
                _key: ProcessKey,
                _enabled: bool,
            ) -> Result<(), PalError> {
                Ok(())
            }
            fn analyze_wait_chain(&self, _key: ProcessKey) -> Result<Vec<WaitChainNode>, PalError> {
                Ok(Vec::new())
            }
            fn write_minidump(
                &self,
                _key: ProcessKey,
                _dest: &Path,
                _cancel: &AtomicBool,
            ) -> Result<(), PalError> {
                Ok(())
            }
            fn attach_debugger(&self, _key: ProcessKey) -> Result<(), PalError> {
                Ok(())
            }
            fn relaunch_self_elevated(&self, _restore_route: &str) -> Result<(), PalError> {
                Ok(())
            }
            fn open_file_location(&self, _path: &Path) -> Result<(), PalError> {
                Ok(())
            }
            fn show_file_properties(&self, _path: &Path) -> Result<(), PalError> {
                Ok(())
            }
            fn search_online(&self, _query: &str) -> Result<(), PalError> {
                Ok(())
            }
            fn browse_executable(&self) -> Result<Option<PathBuf>, PalError> {
                Ok(None)
            }
            fn is_elevated(&self) -> bool {
                false
            }
            fn logical_cpu_count(&self) -> u32 {
                1
            }
        }

        struct WitnessBackend {
            services: WitnessServices,
            startup: WitnessStartup,
            sessions: WitnessSessions,
            history: WitnessHistory,
            operator: WitnessOperator,
        }

        impl PlatformBackend for WitnessBackend {
            type ProcessExt = ();
            type FastSampler = WitnessFastSampler;
            type SlowProber = WitnessSlowProber;
            type ServiceMgr = WitnessServices;
            type StartupMgr = WitnessStartup;
            type SessionMgr = WitnessSessions;
            type HistoryReader = WitnessHistory;
            type Operator = WitnessOperator;

            const PLATFORM: PlatformProfile =
                PlatformProfile::new(PlatformKind::Unknown, "ContractWitness");
            const CAPABILITIES: CapabilityFlags = CapabilityFlags::empty();

            fn boost_self_priority() {}

            fn initialize() -> Result<Self, PalError> {
                Ok(Self {
                    services: WitnessServices,
                    startup: WitnessStartup,
                    sessions: WitnessSessions,
                    history: WitnessHistory,
                    operator: WitnessOperator,
                })
            }

            fn create_fast_sampler(&self) -> Result<Self::FastSampler, PalError> {
                Ok(WitnessFastSampler)
            }

            fn create_slow_prober(&self) -> Result<Self::SlowProber, PalError> {
                Ok(WitnessSlowProber)
            }

            fn service_manager(&self) -> &Self::ServiceMgr {
                &self.services
            }

            fn startup_manager(&self) -> &Self::StartupMgr {
                &self.startup
            }

            fn session_manager(&self) -> &Self::SessionMgr {
                &self.sessions
            }

            fn history_reader(&self) -> &Self::HistoryReader {
                &self.history
            }

            fn operator(&self) -> &Self::Operator {
                &self.operator
            }
        }

        assert_frozen_backend_contract::<WitnessBackend>();
        let backend = WitnessBackend::initialize().expect("witness backend init");
        let mut fast = backend.create_fast_sampler().expect("witness fast sampler");
        let slow = backend.create_slow_prober().expect("witness slow prober");
        let mut proc_scratch = ProcessSampleScratch::<()>::new();
        let mut perf_scratch = PerfSampleScratch::new();
        assert_eq!(
            fast.sample_processes(&mut proc_scratch).unwrap(),
            Timestamp100Ns::new(0)
        );
        assert_eq!(
            fast.sample_performance(&mut perf_scratch).unwrap(),
            Timestamp100Ns::new(0)
        );
        let _ = slow.probe_hardware_static_info().unwrap();
        let _ = slow.probe_disk_performance().unwrap();
        let _ = backend.service_manager().list_services().unwrap();
        let _ = backend.startup_manager().list_startup_entries().unwrap();
        let _ = backend.session_manager().list_sessions().unwrap();
        let _ = backend.history_reader().read_history().unwrap();
        assert_eq!(backend.operator().logical_cpu_count(), 1);
    }
}
