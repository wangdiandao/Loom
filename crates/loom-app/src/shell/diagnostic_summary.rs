//! Privacy-safe plain-text diagnostic summary builder for bug reports.
//!
//! Intentionally restricts output to non-identifying hardware, platform, and
//! Loom configuration fields — strictly omitting PIDs, process lists, usernames,
//! command lines, file paths, IP addresses, DNS suffixes, and hardware serials.

use std::fmt::Write as _;

use loom_config::AppSettings;
use loom_types::{AppSnapshot, LanguagePreference, RefreshSpeed, ThemePreference};
use loom_ui_kit::format_bytes;

/// Builds a concise, privacy-safe plain-text system and configuration summary
/// suitable for pasting into GitHub issues or bug reports.
///
/// # Privacy guarantees
///
/// Strictly omits:
/// - Process IDs (PIDs), process names, window titles, and command lines
/// - Usernames, account domains, and session identifiers
/// - Executable paths, configuration paths, MRU task history, and ignored leak paths
/// - IPv4/IPv6 addresses, DNS suffixes, and network interface LUIDs
/// - PCI bus locations, GPU LUIDs, and hardware serial numbers
pub(crate) fn build_diagnostic_summary(snapshot: &AppSnapshot, settings: &AppSettings) -> String {
    let version = env!("CARGO_PKG_VERSION");
    let os_name = if snapshot.platform.os_name.is_empty() {
        std::env::consts::OS
    } else {
        snapshot.platform.os_name
    };
    let arch = std::env::consts::ARCH;

    let perf = &snapshot.performance;
    let logical_cpus = if perf.cpu_info.logical_processors > 0 {
        perf.cpu_info.logical_processors
    } else if perf.latest.logical_cpu_count > 0 {
        perf.latest.logical_cpu_count
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(0)
    };

    let physical_cores = if perf.cpu_info.physical_cores > 0 {
        perf.cpu_info.physical_cores
    } else {
        perf.latest.physical_core_count
    };

    let total_ram = if perf.latest.mem_total_bytes > 0 {
        format_bytes(perf.latest.mem_total_bytes)
    } else {
        "Unknown".to_owned()
    };

    let gpu_names: Vec<&str> = perf
        .gpus
        .iter()
        .map(|gpu| gpu.name.trim())
        .filter(|name| !name.is_empty())
        .collect();
    let gpus_line = if gpu_names.is_empty() {
        "Not probed yet".to_owned()
    } else {
        gpu_names.join(", ")
    };

    let refresh_label = match settings.refresh_speed {
        RefreshSpeed::High500Ms => "High (0.5s)",
        RefreshSpeed::Normal1S => "Normal (1s)",
        RefreshSpeed::Low2S => "Low (2s)",
        RefreshSpeed::Paused => "Paused",
    };

    let theme_label = match settings.theme {
        ThemePreference::System => "System",
        ThemePreference::Light => "Light",
        ThemePreference::Dark => "Dark",
    };
    let accent_label = if settings.accent_color.is_system() {
        "System"
    } else {
        "Custom"
    };

    let language_label = match settings.language {
        LanguagePreference::EnUs => "en-US",
        LanguagePreference::ZhCn => "zh-CN",
    };

    let elevated_label = if snapshot.is_elevated { "Yes" } else { "No" };

    let mut out = String::with_capacity(320);
    let _ = writeln!(out, "Loom Diagnostic Summary");
    let _ = writeln!(out, "Loom Version: {version}");
    let _ = writeln!(out, "Platform: {os_name} ({arch})");
    let _ = writeln!(out, "Elevated: {elevated_label}");
    if physical_cores > 0 {
        let _ = writeln!(
            out,
            "Logical CPUs: {logical_cpus} ({physical_cores} physical cores)"
        );
    } else {
        let _ = writeln!(out, "Logical CPUs: {logical_cpus}");
    }
    let _ = writeln!(out, "Total RAM: {total_ram}");
    let _ = writeln!(out, "GPU Adapters: {gpus_line}");
    let _ = writeln!(out, "Refresh Speed: {refresh_label}");
    let _ = writeln!(out, "Theme: {theme_label} (Accent: {accent_label})");
    let _ = write!(out, "Language: {language_label}");
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use loom_config::AppSettings;
    use loom_types::{
        AppSnapshot, CpuStaticInfo, DetailsRowProjection, DetailsTableSlice, GpuAdapterSlice,
        LanguagePreference, NetAdapterSlice, PerformanceSlice, PlatformProfile, ProcessKey,
        ProcessRowKind, ProcessRowProjection, ProcessTableSlice, RefreshSpeed, SystemPerfSample,
        ThemePreference, UserSessionRecord, UsersSlice,
    };

    use super::build_diagnostic_summary;

    #[test]
    fn diagnostic_summary_includes_safe_metadata_and_omits_sensitive_fields() {
        let snapshot = AppSnapshot {
            platform: PlatformProfile::WINDOWS,
            is_elevated: true,
            processes: Arc::new(ProcessTableSlice {
                rows: vec![ProcessRowProjection {
                    kind: ProcessRowKind::Process,
                    key: ProcessKey::new(98765, 123456),
                    depth: 0,
                    has_children: false,
                    is_expanded: false,
                    child_count: 0,
                    name: Arc::from("secret_app.exe"),
                    display_name: Arc::from("Secret Internal App"),
                    window_title: Some(Arc::from("Confidential Roadmap - Browser")),
                    publisher: Some(Arc::from("Private Corp")),
                    exe_path: Some(Arc::from(r"C:\Users\alice.smith\secret_app.exe")),
                    command_line: Some(Arc::from("secret_app.exe --token=super-secret-123")),
                    icon: None,
                    category: Default::default(),
                    status: Default::default(),
                    cpu_permille: 120,
                    memory_bytes: 1024 * 1024,
                    memory_permille: 10,
                    disk_bytes_per_sec: 0,
                    disk_permille: 0,
                    net_bytes_per_sec: 0,
                    net_permille: 0,
                    gpu_util_permille: 0,
                    gpu_dedicated_bytes: 0,
                    parent_key: None,
                    leak_warning: None,
                    webview2_role: None,
                }],
                ..ProcessTableSlice::default()
            }),
            details: Arc::new(DetailsTableSlice {
                rows: vec![DetailsRowProjection {
                    key: ProcessKey::new(98765, 123456),
                    parent_key: None,
                    depth: 0,
                    has_children: false,
                    is_expanded: false,
                    name: Arc::from("secret_app.exe"),
                    description: None,
                    publisher: None,
                    icon: None,
                    status: Default::default(),
                    user_name: Some(Arc::from(r"CORP\alice.smith")),
                    session_id: 2,
                    cpu_permille: 0,
                    cpu_time_100ns: 0,
                    private_working_set_bytes: 0,
                    working_set_bytes: 0,
                    working_set_delta_bytes: 0,
                    commit_bytes: 0,
                    thread_count: 1,
                    handle_count: 1,
                    priority: Default::default(),
                    arch: Default::default(),
                    uac_virtualization: Default::default(),
                    gpu_dedicated_bytes: 0,
                    gpu_shared_bytes: 0,
                    exe_path: Some(Arc::from(r"C:\Users\alice.smith\secret_app.exe")),
                    command_line: Some(Arc::from("--password=do-not-leak")),
                    leak_warning: None,
                    webview2_role: None,
                }],
                ..DetailsTableSlice::default()
            }),
            users: Arc::new(UsersSlice {
                sessions: vec![UserSessionRecord {
                    session_id: 2,
                    user_name: Arc::from("alice.smith"),
                    state: Default::default(),
                    process_count: 1,
                    cpu_permille: 0,
                    memory_bytes: 0,
                    disk_bytes_per_sec: 0,
                    net_bytes_per_sec: 0,
                    processes: Vec::new(),
                }],
                ..UsersSlice::default()
            }),
            performance: Arc::new(PerformanceSlice {
                latest: SystemPerfSample {
                    logical_cpu_count: 16,
                    physical_core_count: 8,
                    mem_total_bytes: 32 * 1024 * 1024 * 1024,
                    ..SystemPerfSample::default()
                },
                cpu_info: CpuStaticInfo {
                    logical_processors: 16,
                    physical_cores: 8,
                    ..CpuStaticInfo::default()
                },
                networks: vec![NetAdapterSlice {
                    luid: 0xDEAD_BEEF,
                    name: Arc::from("Ethernet"),
                    description: Arc::from("Intel I225-V"),
                    ipv4: Some(Arc::from("192.168.42.99")),
                    ipv6: Some(Arc::from("fe80::1234:5678")),
                    dns_suffix: Some(Arc::from("corp.internal")),
                    ..NetAdapterSlice::default()
                }],
                gpus: vec![
                    GpuAdapterSlice {
                        index: 0,
                        luid: 0x1122_3344_5566,
                        name: Arc::from("NVIDIA GeForce RTX 4080"),
                        pci_location: Some(Arc::from("PCI bus 1, device 0, function 0")),
                        ..GpuAdapterSlice::default()
                    },
                    GpuAdapterSlice {
                        index: 1,
                        luid: 0xAABB_CCDD,
                        name: Arc::from("AMD Radeon(TM) Graphics"),
                        pci_location: Some(Arc::from("PCI bus 12, device 0, function 0")),
                        ..GpuAdapterSlice::default()
                    },
                ],
                ..PerformanceSlice::default()
            }),
            ..AppSnapshot::default()
        };

        let settings = AppSettings {
            refresh_speed: RefreshSpeed::High500Ms,
            theme: ThemePreference::Dark,
            language: LanguagePreference::ZhCn,
            run_task_history: vec![r"C:\Private\internal_tool.exe --api-key=xyz".to_owned()],
            leak_ignored_paths: vec![r"D:\SecretRepo\daemon.exe".to_owned()],
            ..AppSettings::default()
        };

        let summary = build_diagnostic_summary(&snapshot, &settings);

        // Required safe fields are present.
        assert!(summary.contains("Loom Version: 0.1.0"));
        assert!(summary.contains("Platform: Windows"));
        assert!(summary.contains("Elevated: Yes"));
        assert!(summary.contains("Logical CPUs: 16 (8 physical cores)"));
        assert!(summary.contains("Total RAM: 32.0 GB"));
        assert!(summary.contains("GPU Adapters: NVIDIA GeForce RTX 4080, AMD Radeon(TM) Graphics"));
        assert!(summary.contains("Refresh Speed: High (0.5s)"));
        assert!(summary.contains("Theme: Dark"));
        assert!(summary.contains("Language: zh-CN"));

        // Sensitive fields must NEVER appear anywhere in the summary.
        let forbidden = [
            "98765",
            "alice.smith",
            "CORP\\",
            "secret_app",
            "Confidential Roadmap",
            "super-secret-123",
            "do-not-leak",
            "192.168.42.99",
            "fe80::1234",
            "corp.internal",
            "PCI bus",
            "internal_tool",
            "api-key",
            "SecretRepo",
            "1122",
            "DEAD",
        ];
        for secret in forbidden {
            assert!(
                !summary.contains(secret),
                "diagnostic summary must not leak sensitive token `{secret}`:\n{summary}"
            );
        }
    }
}
