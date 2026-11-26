//! Loom desktop task manager binary entry point (composition root skeleton).

use loom_config::AppSettings;
use loom_engine::EngineCoordinator;
use loom_pal::{NativeBackend, PlatformBackend};
use loom_types::ResourceDisplayMode;
use loom_ui_kit::{HeatmapLevel, format_memory_cell};

fn main() {
    NativeBackend::boost_self_priority();

    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let settings = AppSettings::default();
    match NativeBackend::initialize().and_then(|backend| EngineCoordinator::new(backend, settings))
    {
        Ok(coordinator) => {
            let snap = coordinator.snapshot();
            let cpu_level = HeatmapLevel::from_permille(snap.processes.total_cpu_permille);
            let mem_label = format_memory_cell(
                snap.performance.latest.mem_used_bytes,
                snap.processes.total_memory_permille,
                ResourceDisplayMode::Values,
            );
            tracing::info!(
                elevated = snap.is_elevated,
                capabilities = ?snap.capabilities,
                ?cpu_level,
                %mem_label,
                "Loom workspace skeleton initialized"
            );
        }
        Err(err) => {
            tracing::error!(%err, "failed to initialize Loom backend");
            std::process::exit(1);
        }
    }
}
