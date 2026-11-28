//! Plain-text hardware summary formatter for Performance pane clipboard copy.

use loom_types::{
    CpuCoreKind, DiskDeviceSlice, GpuAdapterSlice, NetAdapterSlice, PerformanceSlice,
};
use loom_ui_kit::{
    format_bits_per_sec, format_bytes, format_freq_ghz, format_latency_us,
    format_network_rate_mbps, format_opt_clock_mhz, format_opt_fan_percent,
    format_opt_percent_permille, format_opt_power_mw, format_opt_temp_celsius, format_permille,
    format_rate_bytes_per_sec, format_uptime,
};

use crate::features::performance::state::SelectedDevice;
use crate::i18n;

/// Formats a clean plain-text readout of the currently selected hardware pane
/// suitable for copying to the system clipboard.
pub(crate) fn format_performance_summary(
    perf: &PerformanceSlice,
    selected: SelectedDevice,
) -> String {
    match selected.resolve(perf) {
        SelectedDevice::Cpu => format_cpu_summary(perf),
        SelectedDevice::Memory => format_memory_summary(perf),
        SelectedDevice::Disk(index) => perf
            .disks
            .iter()
            .find(|d| d.index == index)
            .map(format_disk_summary)
            .unwrap_or_else(|| format_cpu_summary(perf)),
        SelectedDevice::Network(luid) => perf
            .networks
            .iter()
            .find(|n| n.luid == luid)
            .map(format_network_summary)
            .unwrap_or_else(|| format_cpu_summary(perf)),
        SelectedDevice::Gpu(luid) => perf
            .gpus
            .iter()
            .find(|g| g.luid == luid)
            .map(format_gpu_summary)
            .unwrap_or_else(|| format_cpu_summary(perf)),
    }
}

fn format_cpu_summary(perf: &PerformanceSlice) -> String {
    let latest = &perf.latest;
    let info = &perf.cpu_info;
    let unavailable = i18n::tr("perf.unavailable");

    let brand = if info.brand_name.trim().is_empty() {
        unavailable.as_str()
    } else {
        info.brand_name.trim()
    };
    let speed = if latest.cpu_freq_mhz > 0 {
        format_freq_ghz(latest.cpu_freq_mhz)
    } else {
        unavailable.clone()
    };
    let base_mhz = if info.base_freq_mhz > 0 {
        info.base_freq_mhz
    } else {
        latest.cpu_base_freq_mhz
    };
    let base_speed = if base_mhz > 0 {
        format_freq_ghz(base_mhz)
    } else {
        unavailable.clone()
    };
    let cores = if info.physical_cores > 0 {
        info.physical_cores
    } else {
        latest.physical_core_count
    };
    let logical = if info.logical_processors > 0 {
        info.logical_processors
    } else {
        latest.logical_cpu_count
    };
    let virt = match info.virtualization_enabled {
        Some(true) => i18n::tr("perf.enabled"),
        Some(false) => i18n::tr("perf.disabled"),
        None => unavailable.clone(),
    };
    let fmt_cache = |bytes: u64| {
        if bytes > 0 {
            format_bytes(bytes)
        } else {
            unavailable.clone()
        }
    };

    let mut lines = vec![
        format!("{} — {brand}", i18n::tr("perf.cpu")),
        format!(
            "{}: {}",
            i18n::tr("perf.cpu_utilization"),
            format_permille(latest.cpu_total_permille)
        ),
        format!("{}: {speed}", i18n::tr("perf.cpu_speed")),
        format!("{}: {base_speed}", i18n::tr("perf.cpu_base_speed")),
        format!(
            "{}: {}",
            i18n::tr("perf.cpu_processes"),
            latest.process_count
        ),
        format!("{}: {}", i18n::tr("perf.cpu_threads"), latest.thread_count),
        format!("{}: {}", i18n::tr("perf.cpu_handles"), latest.handle_count),
        format!(
            "{}: {}",
            i18n::tr("perf.cpu_uptime"),
            format_uptime(latest.uptime_secs)
        ),
        format!(
            "{}: {}",
            i18n::tr("perf.cpu_sockets"),
            if info.sockets > 0 {
                info.sockets.to_string()
            } else {
                unavailable.clone()
            }
        ),
        format!(
            "{}: {}",
            i18n::tr("perf.cpu_cores"),
            if cores > 0 {
                cores.to_string()
            } else {
                unavailable.clone()
            }
        ),
        format!(
            "{}: {}",
            i18n::tr("perf.cpu_logical_processors"),
            if logical > 0 {
                logical.to_string()
            } else {
                unavailable.clone()
            }
        ),
        format!("{}: {virt}", i18n::tr("perf.cpu_virtualization")),
        format!(
            "{}: {}",
            i18n::tr("perf.cpu_l1_cache"),
            fmt_cache(info.l1_cache_bytes)
        ),
        format!(
            "{}: {}",
            i18n::tr("perf.cpu_l2_cache"),
            fmt_cache(info.l2_cache_bytes)
        ),
        format!(
            "{}: {}",
            i18n::tr("perf.cpu_l3_cache"),
            fmt_cache(info.l3_cache_bytes)
        ),
    ];

    let p_count = info
        .core_kinds
        .iter()
        .filter(|&&k| k == CpuCoreKind::Performance)
        .count();
    let e_count = info
        .core_kinds
        .iter()
        .filter(|&&k| k == CpuCoreKind::Efficiency)
        .count();
    let lpe_count = info
        .core_kinds
        .iter()
        .filter(|&&k| k == CpuCoreKind::LowPowerEfficiency)
        .count();
    let distinct = usize::from(p_count > 0) + usize::from(e_count > 0) + usize::from(lpe_count > 0);
    if distinct >= 2 {
        let mut parts = Vec::with_capacity(3);
        if p_count > 0 {
            parts.push(format!("{} {p_count}", i18n::tr("perf.perf_polish_core_p")));
        }
        if e_count > 0 {
            parts.push(format!("{} {e_count}", i18n::tr("perf.perf_polish_core_e")));
        }
        if lpe_count > 0 {
            parts.push(format!(
                "{} {lpe_count}",
                i18n::tr("perf.perf_polish_core_lp_e")
            ));
        }
        lines.push(format!(
            "{}: {}",
            i18n::tr("perf.cpu_logical"),
            parts.join(" / ")
        ));
    }

    lines.join("\n")
}

fn format_memory_summary(perf: &PerformanceSlice) -> String {
    let latest = &perf.latest;
    let info = &perf.memory_info;
    let unavailable = i18n::tr("perf.unavailable");

    let cached_bytes = latest
        .mem_standby_bytes
        .saturating_add(latest.mem_modified_bytes);
    let speed = match info.speed_mt_s {
        Some(speed) if speed > 0 => format!("{speed} MT/s"),
        _ => unavailable.clone(),
    };
    let slots = match (info.slots_used, info.slots_total) {
        (Some(used), Some(total)) if total > 0 => format!("{used} / {total}"),
        (Some(used), _) => used.to_string(),
        _ => unavailable.clone(),
    };
    let form_factor = info
        .form_factor
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| unavailable.clone());

    [
        format!(
            "{} — {}",
            i18n::tr("perf.memory"),
            format_bytes(latest.mem_total_bytes)
        ),
        format!(
            "{}: {} ({})",
            i18n::tr("perf.mem_in_use"),
            format_bytes(latest.mem_used_bytes),
            format_permille(latest.mem_util_permille())
        ),
        format!(
            "{}: {}",
            i18n::tr("perf.mem_available"),
            format_bytes(latest.mem_available_bytes)
        ),
        format!(
            "{}: {} / {}",
            i18n::tr("perf.mem_committed"),
            format_bytes(latest.mem_committed_bytes),
            format_bytes(latest.mem_commit_limit_bytes)
        ),
        format!(
            "{}: {}",
            i18n::tr("perf.mem_cached"),
            format_bytes(cached_bytes)
        ),
        format!(
            "{}: {}",
            i18n::tr("perf.mem_paged_pool"),
            format_bytes(latest.mem_paged_pool_bytes)
        ),
        format!(
            "{}: {}",
            i18n::tr("perf.mem_nonpaged_pool"),
            format_bytes(latest.mem_nonpaged_pool_bytes)
        ),
        format!("{}: {speed}", i18n::tr("perf.mem_speed")),
        format!("{}: {slots}", i18n::tr("perf.mem_slots_used")),
        format!("{}: {form_factor}", i18n::tr("perf.mem_form_factor")),
        format!(
            "{}: {}",
            i18n::tr("perf.mem_hardware_reserved"),
            format_bytes(info.hardware_reserved_bytes)
        ),
    ]
    .join("\n")
}

fn format_disk_summary(disk: &DiskDeviceSlice) -> String {
    let unavailable = i18n::tr("perf.unavailable");
    let title = i18n::perf_disk_title(disk.index, disk.drive_letters.as_ref());
    let model = if disk.model.trim().is_empty() {
        unavailable.as_str()
    } else {
        disk.model.trim()
    };
    let capacity = if disk.capacity_bytes > 0 {
        format_bytes(disk.capacity_bytes)
    } else {
        unavailable.clone()
    };
    let drives = if disk.drive_letters.trim().is_empty() {
        unavailable.clone()
    } else {
        disk.drive_letters.trim().to_owned()
    };

    [
        format!("{title} — {model}"),
        format!(
            "{}: {}",
            i18n::tr("perf.disk_active_time"),
            format_permille(disk.active_time_permille)
        ),
        format!(
            "{}: {}",
            i18n::tr("perf.disk_avg_response"),
            format_latency_us(disk.avg_response_us)
        ),
        format!(
            "{}: {}",
            i18n::tr("perf.disk_read_speed"),
            format_rate_bytes_per_sec(disk.read_bytes_per_sec)
        ),
        format!(
            "{}: {}",
            i18n::tr("perf.disk_write_speed"),
            format_rate_bytes_per_sec(disk.write_bytes_per_sec)
        ),
        format!("{}: {capacity}", i18n::tr("perf.disk_capacity")),
        format!("{}: {drives}", i18n::tr("perf.disk_drive_letters")),
        format!(
            "{}: {}",
            i18n::tr("perf.disk_type"),
            i18n::disk_media_type_label(disk.media_type)
        ),
    ]
    .join("\n")
}

fn format_network_summary(adapter: &NetAdapterSlice) -> String {
    let unavailable = i18n::tr("perf.unavailable");
    let conn = i18n::net_connection_type_label(adapter.connection_type);
    let desc = if !adapter.description.trim().is_empty() {
        adapter.description.trim()
    } else if !adapter.name.trim().is_empty() {
        adapter.name.trim()
    } else {
        unavailable.as_str()
    };
    let link_speed = if adapter.link_speed_bps > 0 {
        format_bits_per_sec(adapter.link_speed_bps)
    } else {
        unavailable.clone()
    };
    let opt_str = |opt: &Option<std::sync::Arc<str>>| {
        opt.as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| unavailable.clone())
    };

    [
        format!("{conn} — {desc}"),
        format!(
            "{}: {}",
            i18n::tr("perf.net_send"),
            format_network_rate_mbps(adapter.sent_bytes_per_sec)
        ),
        format!(
            "{}: {}",
            i18n::tr("perf.net_receive"),
            format_network_rate_mbps(adapter.recv_bytes_per_sec)
        ),
        format!(
            "{}: {}",
            i18n::tr("perf.net_adapter_name"),
            if adapter.name.trim().is_empty() {
                unavailable.as_str()
            } else {
                adapter.name.trim()
            }
        ),
        format!("{}: {conn}", i18n::tr("perf.net_connection_type")),
        format!("{}: {link_speed}", i18n::tr("perf.net_link_speed")),
        format!("{}: {}", i18n::tr("perf.net_ipv4"), opt_str(&adapter.ipv4)),
        format!("{}: {}", i18n::tr("perf.net_ipv6"), opt_str(&adapter.ipv6)),
        format!(
            "{}: {}",
            i18n::tr("perf.net_dns_suffix"),
            opt_str(&adapter.dns_suffix)
        ),
    ]
    .join("\n")
}

fn format_gpu_summary(gpu: &GpuAdapterSlice) -> String {
    let unavailable = i18n::tr("perf.unavailable");
    let title = i18n::perf_gpu_title(gpu.index);
    let name = if gpu.name.trim().is_empty() {
        unavailable.as_str()
    } else {
        gpu.name.trim()
    };
    let opt_str = |opt: &Option<std::sync::Arc<str>>| {
        opt.as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| unavailable.clone())
    };

    let util_str = if gpu.d3_asleep {
        i18n::tr("perf.perf_polish_gpu_sleeping")
    } else {
        format_opt_percent_permille(Some(gpu.util_permille)).to_string()
    };
    let temp_str = format_opt_temp_celsius(gpu.temperature_celsius, gpu.d3_asleep);

    let mut lines = vec![
        format!("{title} — {name}"),
        format!("{}: {util_str}", i18n::tr("perf.gpu_utilization")),
        format!(
            "{}: {} / {}",
            i18n::tr("perf.gpu_dedicated_memory"),
            format_bytes(gpu.dedicated_used_bytes),
            format_bytes(gpu.dedicated_total_bytes)
        ),
        format!(
            "{}: {} / {}",
            i18n::tr("perf.gpu_shared_memory"),
            format_bytes(gpu.shared_used_bytes),
            format_bytes(gpu.shared_total_bytes)
        ),
        format!("{}: {temp_str}", i18n::tr("perf.gpu_temperature")),
    ];
    if !gpu.d3_asleep {
        if gpu.power_draw_mw.is_some() {
            lines.push(format!(
                "{}: {}",
                i18n::tr("perf.gpu_power_draw"),
                format_opt_power_mw(gpu.power_draw_mw, gpu.d3_asleep)
            ));
        }
        if gpu.fan_speed_percent.is_some() {
            lines.push(format!(
                "{}: {}",
                i18n::tr("perf.gpu_fan_speed"),
                format_opt_fan_percent(gpu.fan_speed_percent, gpu.d3_asleep)
            ));
        }
        if gpu.core_clock_mhz.is_some() {
            lines.push(format!(
                "{}: {}",
                i18n::tr("perf.gpu_core_clock"),
                format_opt_clock_mhz(gpu.core_clock_mhz, gpu.d3_asleep)
            ));
        }
        if gpu.memory_clock_mhz.is_some() {
            lines.push(format!(
                "{}: {}",
                i18n::tr("perf.gpu_memory_clock"),
                format_opt_clock_mhz(gpu.memory_clock_mhz, gpu.d3_asleep)
            ));
        }
    }
    lines.extend([
        format!(
            "{}: {}",
            i18n::tr("perf.gpu_driver_version"),
            opt_str(&gpu.driver_version)
        ),
        format!(
            "{}: {}",
            i18n::tr("perf.gpu_directx_version"),
            opt_str(&gpu.directx_version)
        ),
        format!(
            "{}: {}",
            i18n::tr("perf.gpu_pci_location"),
            opt_str(&gpu.pci_location)
        ),
    ]);
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use loom_types::{
        CpuCoreKind, CpuStaticInfo, DiskDeviceSlice, DiskMediaType, GpuAdapterSlice,
        LanguagePreference, NetAdapterSlice, NetConnectionType, PerformanceSlice, SystemPerfSample,
    };

    use super::format_performance_summary;
    use crate::features::performance::state::SelectedDevice;
    use crate::i18n;

    #[test]
    fn performance_summary_formats_all_devices_and_honest_d3_sleep() {
        i18n::apply_locale(LanguagePreference::EnUs);

        let mut perf = PerformanceSlice {
            latest: SystemPerfSample {
                cpu_total_permille: 245,
                cpu_freq_mhz: 4250,
                process_count: 140,
                thread_count: 1900,
                handle_count: 54_000,
                uptime_secs: 3661,
                mem_total_bytes: 32 * 1024 * 1024 * 1024,
                mem_used_bytes: 16 * 1024 * 1024 * 1024,
                mem_available_bytes: 16 * 1024 * 1024 * 1024,
                ..Default::default()
            },
            cpu_info: CpuStaticInfo {
                brand_name: Arc::from("Intel Core Ultra 9"),
                base_freq_mhz: 3200,
                sockets: 1,
                physical_cores: 16,
                logical_processors: 22,
                virtualization_enabled: Some(true),
                core_kinds: vec![
                    CpuCoreKind::Performance,
                    CpuCoreKind::Performance,
                    CpuCoreKind::Efficiency,
                    CpuCoreKind::LowPowerEfficiency,
                ],
                ..Default::default()
            },
            ..Default::default()
        };
        perf.disks.push(DiskDeviceSlice {
            index: 0,
            model: Arc::from("Samsung 990 PRO"),
            media_type: DiskMediaType::Nvme,
            drive_letters: Arc::from("C:"),
            capacity_bytes: 1024 * 1024 * 1024 * 1024,
            active_time_permille: 120,
            read_bytes_per_sec: 10 * 1024 * 1024,
            write_bytes_per_sec: 5 * 1024 * 1024,
            avg_response_us: 1500,
            ..Default::default()
        });
        perf.networks.push(NetAdapterSlice {
            luid: 42,
            name: Arc::from("Ethernet"),
            description: Arc::from("Intel I226-V"),
            connection_type: NetConnectionType::Ethernet,
            link_speed_bps: 2_500_000_000,
            ipv4: Some(Arc::from("192.168.1.10")),
            ..Default::default()
        });
        perf.gpus.push(GpuAdapterSlice {
            index: 1,
            luid: 99,
            name: Arc::from("NVIDIA GeForce RTX 4070 Laptop"),
            temperature_celsius: Some(0),
            util_permille: 0,
            d3_asleep: true,
            ..Default::default()
        });

        let cpu_text = format_performance_summary(&perf, SelectedDevice::Cpu);
        assert!(cpu_text.contains("Intel Core Ultra 9"));
        assert!(cpu_text.contains("24.5%"));
        assert!(cpu_text.contains("P 2 / E 1 / LP-E 1"));

        let mem_text = format_performance_summary(&perf, SelectedDevice::Memory);
        assert!(mem_text.contains("32.0 GB"));
        assert!(mem_text.contains("50.0%"));

        let disk_text = format_performance_summary(&perf, SelectedDevice::Disk(0));
        assert!(disk_text.contains("Samsung 990 PRO"));
        assert!(disk_text.contains("12.0%"));

        let net_text = format_performance_summary(&perf, SelectedDevice::Network(42));
        assert!(net_text.contains("Intel I226-V"));
        assert!(net_text.contains("192.168.1.10"));

        let gpu_text = format_performance_summary(&perf, SelectedDevice::Gpu(99));
        assert!(gpu_text.contains("NVIDIA GeForce RTX 4070 Laptop"));
        assert!(gpu_text.contains("Sleeping (D3)"));
        assert!(gpu_text.contains("GPU Temperature: —"));
        assert!(!gpu_text.contains("0°C"));
        assert!(!gpu_text.contains("Power draw:"));

        perf.gpus.push(GpuAdapterSlice {
            index: 0,
            luid: 100,
            name: Arc::from("NVIDIA GeForce RTX 4080"),
            temperature_celsius: Some(58),
            power_draw_mw: Some(184_500),
            fan_speed_percent: Some(42),
            core_clock_mhz: Some(2520),
            memory_clock_mhz: Some(11200),
            util_permille: 650,
            d3_asleep: false,
            ..Default::default()
        });
        let active_gpu_text = format_performance_summary(&perf, SelectedDevice::Gpu(100));
        assert!(active_gpu_text.contains("Power draw: 184.5 W"));
        assert!(active_gpu_text.contains("Fan speed: 42%"));
        assert!(active_gpu_text.contains("GPU clock: 2520 MHz"));
        assert!(active_gpu_text.contains("Memory clock: 11200 MHz"));
    }
}
