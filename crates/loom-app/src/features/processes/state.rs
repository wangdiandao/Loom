//! Pure state helpers and selection predicates for Processes view.

use loom_types::{
    CapabilityFlags, ProcessCategory, ProcessKey, ProcessRowKind, ProcessRowProjection,
    ProcessSortColumn, ProcessStatusFlags, ResourceDisplayMode, SortDirection,
};
use loom_ui_kit::{
    StatusLabels, format_disk_cell, format_memory_cell, format_network_cell, format_permille,
};

use crate::features::table_columns::format_process_name_and_pid;

/// Resource columns whose formatting can be toggled between `Values` and `Percent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResourceColumn {
    Memory,
    Disk,
    Network,
}

/// Updates one resource column's [`ResourceDisplayMode`] while leaving the other two unchanged.
#[inline]
pub(crate) const fn apply_resource_display_mode(
    memory: ResourceDisplayMode,
    disk: ResourceDisplayMode,
    network: ResourceDisplayMode,
    column: ResourceColumn,
    mode: ResourceDisplayMode,
) -> (
    ResourceDisplayMode,
    ResourceDisplayMode,
    ResourceDisplayMode,
) {
    match column {
        ResourceColumn::Memory => (mode, disk, network),
        ResourceColumn::Disk => (memory, mode, network),
        ResourceColumn::Network => (memory, disk, mode),
    }
}

/// Computes the next [`SortDirection`] when clicking a sortable Processes table column header.
#[inline]
pub(crate) fn next_sort_direction(
    active_column: ProcessSortColumn,
    active_direction: SortDirection,
    clicked_column: ProcessSortColumn,
) -> SortDirection {
    if active_column == clicked_column {
        active_direction.toggle()
    } else {
        match clicked_column {
            ProcessSortColumn::Name
            | ProcessSortColumn::Category
            | ProcessSortColumn::Pid
            | ProcessSortColumn::ProcessName
            | ProcessSortColumn::Publisher
            | ProcessSortColumn::CommandLine => SortDirection::Ascending,
            ProcessSortColumn::Status
            | ProcessSortColumn::Cpu
            | ProcessSortColumn::Memory
            | ProcessSortColumn::Disk
            | ProcessSortColumn::Network
            | ProcessSortColumn::Gpu => SortDirection::Descending,
        }
    }
}

/// Looks up the currently selected process row strictly by [`ProcessKey`] (never row index).
pub(crate) fn find_selected_row(
    rows: &[ProcessRowProjection],
    selected_key: Option<ProcessKey>,
) -> Option<&ProcessRowProjection> {
    find_selected_row_fast(rows, selected_key, None)
}

/// Fast-path variant of [`find_selected_row`] checking `hint_idx` in `O(1)` before scanning.
#[inline]
pub(crate) fn find_selected_row_fast(
    rows: &[ProcessRowProjection],
    selected_key: Option<ProcessKey>,
    hint_idx: Option<usize>,
) -> Option<&ProcessRowProjection> {
    let key = selected_key?;
    if let Some(idx) = hint_idx
        && let Some(row) = rows.get(idx)
        && row.kind == ProcessRowKind::Process
        && row.key == key
    {
        return Some(row);
    }
    rows.iter()
        .find(|row| row.kind == ProcessRowKind::Process && row.key == key)
}

/// Returns `true` when `row` can be targeted by End Task (excludes idle and kernel processes).
#[inline]
pub(crate) fn can_terminate_process(row: &ProcessRowProjection) -> bool {
    row.kind == ProcessRowKind::Process
        && !row
            .status
            .intersects(ProcessStatusFlags::SYSTEM_IDLE | ProcessStatusFlags::KERNEL)
}

/// Returns `true` when terminating `row` requires a confirmation modal dialog.
///
/// System processes, critical processes, kernel processes, idle pseudo-processes,
/// and OS-protected processes require explicit confirmation before termination.
#[inline]
pub(crate) fn requires_end_task_confirmation(row: &ProcessRowProjection) -> bool {
    row.category == ProcessCategory::System
        || row.status.intersects(
            ProcessStatusFlags::CRITICAL
                | ProcessStatusFlags::PROTECTED
                | ProcessStatusFlags::KERNEL
                | ProcessStatusFlags::SYSTEM_IDLE,
        )
}

/// Returns `true` when `row` is eligible for Efficiency Mode (`EcoQoS`) toggling.
///
/// Disabled when the active backend does not advertise [`CapabilityFlags::ECO_MODE`]
/// or when `row` carries `SYSTEM_IDLE`, `KERNEL`, `PROTECTED`, or `CRITICAL` flags.
#[inline]
pub(crate) fn is_efficiency_mode_eligible(
    row: &ProcessRowProjection,
    capabilities: CapabilityFlags,
) -> bool {
    capabilities.contains(CapabilityFlags::ECO_MODE)
        && row.kind == ProcessRowKind::Process
        && !row.status.intersects(
            ProcessStatusFlags::SYSTEM_IDLE
                | ProcessStatusFlags::KERNEL
                | ProcessStatusFlags::PROTECTED
                | ProcessStatusFlags::CRITICAL,
        )
}

/// Returns the best query string for "Search online" on a process row.
pub(crate) fn search_online_query(row: &ProcessRowProjection) -> String {
    let image = row.name.trim();
    if !image.is_empty() {
        image.to_owned()
    } else {
        row.display_name.trim().to_owned()
    }
}

/// Computes a deterministic, allocation-free element ID discriminator from [`ProcessKey`].
#[inline]
pub(crate) fn stable_row_id(key: ProcessKey) -> usize {
    let mixed = u64::from(key.pid).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ key.create_time_100ns.rotate_left(17);
    mixed as usize
}

/// Formats the short `Ctrl+C` clipboard text (`"<name> (<pid>)"`) for a process row.
pub(crate) fn format_process_copy_short(row: &ProcessRowProjection) -> String {
    let primary = if !row.name.trim().is_empty() {
        row.name.trim()
    } else {
        row.display_name.trim()
    };
    format_process_name_and_pid(primary, row.key.pid)
}

/// Formats a human-readable status summary string for clipboard copy and CSV export.
pub(crate) fn format_process_status_text(
    status: ProcessStatusFlags,
    labels: &StatusLabels,
    elevated_label: &str,
    running_label: &str,
) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(4);
    if status.contains(ProcessStatusFlags::NOT_RESPONDING) {
        parts.push(labels.not_responding.as_ref());
    }
    if status.contains(ProcessStatusFlags::SUSPENDED) {
        parts.push(labels.suspended.as_ref());
    }
    if status.contains(ProcessStatusFlags::EFFICIENCY_MODE) {
        parts.push(labels.eco_mode.as_ref());
    }
    if status.contains(ProcessStatusFlags::ELEVATED) {
        parts.push(elevated_label);
    }
    if parts.is_empty() {
        running_label.to_owned()
    } else {
        parts.join(", ")
    }
}

/// Formats the structured multi-field `Ctrl+Shift+C` summary for a Processes row.
pub(crate) fn format_process_row_summary(
    row: &ProcessRowProjection,
    mem_mode: ResourceDisplayMode,
    disk_mode: ResourceDisplayMode,
    net_mode: ResourceDisplayMode,
    status_text: &str,
) -> String {
    let display = row.display_name.trim();
    let image = row.name.trim();
    let name_line =
        if !display.is_empty() && !image.is_empty() && !display.eq_ignore_ascii_case(image) {
            format!("{display} ({image})")
        } else if !display.is_empty() {
            display.to_owned()
        } else {
            image.to_owned()
        };
    let cpu = format_permille(row.cpu_permille);
    let mem = format_memory_cell(row.memory_bytes, row.memory_permille, mem_mode);
    let disk = format_disk_cell(row.disk_bytes_per_sec, row.disk_permille, disk_mode);
    let net = format_network_cell(row.net_bytes_per_sec, row.net_permille, net_mode);
    let gpu = format_permille(row.gpu_util_permille);
    let publisher = row
        .publisher
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("—");
    let cmdline = row
        .command_line
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("—");

    format!(
        "Name: {name_line}\nPID: {}\nStatus: {status_text}\nCPU: {cpu}\nMemory: {mem}\nDisk: {disk}\nNetwork: {net}\nGPU: {gpu}\nPublisher: {publisher}\nCommand Line: {cmdline}",
        row.key.pid
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use loom_types::{
        ExeIconRgba, ProcessCategory, ProcessKey, ProcessRowKind, ProcessRowProjection,
        ProcessSortColumn, ProcessStatusFlags, ResourceDisplayMode, SortDirection,
    };

    use crate::features::table_columns::IconRenderCache;

    use super::*;

    fn sample_process_row(
        pid: u32,
        create_time: u64,
        name: &str,
        display_name: &str,
        category: ProcessCategory,
        status: ProcessStatusFlags,
    ) -> ProcessRowProjection {
        ProcessRowProjection {
            kind: ProcessRowKind::Process,
            key: ProcessKey::new(pid, create_time),
            depth: 0,
            has_children: false,
            is_expanded: false,
            child_count: 0,
            name: Arc::from(name),
            display_name: Arc::from(display_name),
            window_title: None,
            publisher: None,
            exe_path: None,
            command_line: None,
            icon: None,
            category,
            status,
            cpu_permille: 0,
            memory_bytes: 0,
            memory_permille: 0,
            disk_bytes_per_sec: 0,
            disk_permille: 0,
            net_bytes_per_sec: 0,
            net_permille: 0,
            gpu_util_permille: 0,
            gpu_dedicated_bytes: 0,
            parent_key: None,
            leak_warning: None,
            webview2_role: None,
        }
    }

    fn sample_section_header(category: ProcessCategory, count: usize) -> ProcessRowProjection {
        let mut row = sample_process_row(0, 0, "", "", category, ProcessStatusFlags::empty());
        row.kind = ProcessRowKind::SectionHeader { category, count };
        row
    }

    #[test]
    fn process_key_selection_stays_anchored_when_rows_reorder_or_pid_reused() {
        let target_key = ProcessKey::new(4242, 100_000);
        let other_key = ProcessKey::new(1337, 90_000);
        let recycled_pid_key = ProcessKey::new(4242, 200_000);

        let initial_rows = vec![
            sample_section_header(ProcessCategory::App, 2),
            sample_process_row(
                other_key.pid,
                other_key.create_time_100ns,
                "alpha.exe",
                "Alpha",
                ProcessCategory::App,
                ProcessStatusFlags::empty(),
            ),
            sample_process_row(
                target_key.pid,
                target_key.create_time_100ns,
                "target.exe",
                "Target App",
                ProcessCategory::App,
                ProcessStatusFlags::empty(),
            ),
        ];

        // Initially at index 2.
        let found_initial = find_selected_row_fast(&initial_rows, Some(target_key), Some(2))
            .expect("target should be found at initial position via O(1) hint");
        assert_eq!(found_initial.key, target_key);
        assert_eq!(found_initial.display_name.as_ref(), "Target App");

        // After live re-sort, target moves to index 0 (and another process takes index 2).
        let reordered_rows = vec![
            sample_process_row(
                target_key.pid,
                target_key.create_time_100ns,
                "target.exe",
                "Target App",
                ProcessCategory::App,
                ProcessStatusFlags::empty(),
            ),
            sample_section_header(ProcessCategory::Background, 1),
            sample_process_row(
                other_key.pid,
                other_key.create_time_100ns,
                "alpha.exe",
                "Alpha",
                ProcessCategory::Background,
                ProcessStatusFlags::empty(),
            ),
        ];

        // Stale hint (index 2 now holds other_key) falls back cleanly to scanning by ProcessKey.
        let found_reordered = find_selected_row_fast(&reordered_rows, Some(target_key), Some(2))
            .expect("target must stay anchored to ProcessKey after re-sort");
        assert_eq!(found_reordered.key, target_key);
        assert_eq!(found_reordered.display_name.as_ref(), "Target App");

        // If the original process exits and the OS recycles PID 4242 with a newer create_time,
        // selection MUST NOT silently attach to the recycled PID.
        let recycled_rows = vec![sample_process_row(
            recycled_pid_key.pid,
            recycled_pid_key.create_time_100ns,
            "new_process.exe",
            "New Process",
            ProcessCategory::App,
            ProcessStatusFlags::empty(),
        )];
        assert!(find_selected_row(&recycled_rows, Some(target_key)).is_none());
    }

    #[test]
    fn semantic_status_flags_govern_termination_and_eco_mode_without_hardcoded_pids() {
        let with_eco = CapabilityFlags::ECO_MODE;
        let without_eco = CapabilityFlags::empty();

        let idle = sample_process_row(
            0,
            0,
            "System Idle Process",
            "System Idle Process",
            ProcessCategory::System,
            ProcessStatusFlags::SYSTEM_IDLE | ProcessStatusFlags::KERNEL,
        );
        let kernel = sample_process_row(
            2,
            1,
            "kthreadd",
            "kthreadd",
            ProcessCategory::System,
            ProcessStatusFlags::KERNEL,
        );
        let protected = sample_process_row(
            812,
            500,
            "csrss.exe",
            "Client Server Runtime Process",
            ProcessCategory::System,
            ProcessStatusFlags::PROTECTED | ProcessStatusFlags::CRITICAL,
        );
        let system_idle_flagged = sample_process_row(
            999,
            500,
            "idle_stub",
            "Idle Stub",
            ProcessCategory::System,
            ProcessStatusFlags::SYSTEM_IDLE,
        );
        // Normal user process with PID 4 (e.g. on Linux/macOS without KERNEL/PROTECTED flags)
        // must NOT be blocked by Windows-specific PID heuristics.
        let non_kernel_pid4 = sample_process_row(
            4,
            10_000,
            "my-worker",
            "My Worker",
            ProcessCategory::Background,
            ProcessStatusFlags::empty(),
        );
        let normal_app = sample_process_row(
            5120,
            80_000,
            "editor",
            "Editor",
            ProcessCategory::App,
            ProcessStatusFlags::empty(),
        );
        let already_eco_bg = sample_process_row(
            6120,
            90_000,
            "worker",
            "Worker",
            ProcessCategory::Background,
            ProcessStatusFlags::EFFICIENCY_MODE,
        );

        assert!(!is_efficiency_mode_eligible(&idle, with_eco));
        assert!(!is_efficiency_mode_eligible(&kernel, with_eco));
        assert!(!is_efficiency_mode_eligible(&protected, with_eco));
        assert!(!is_efficiency_mode_eligible(&system_idle_flagged, with_eco));
        assert!(is_efficiency_mode_eligible(&non_kernel_pid4, with_eco));
        assert!(is_efficiency_mode_eligible(&normal_app, with_eco));
        assert!(is_efficiency_mode_eligible(&already_eco_bg, with_eco));
        assert!(!is_efficiency_mode_eligible(&normal_app, without_eco));

        assert!(!can_terminate_process(&idle));
        assert!(!can_terminate_process(&kernel));
        assert!(!can_terminate_process(&system_idle_flagged));
        assert!(can_terminate_process(&non_kernel_pid4));
        assert!(can_terminate_process(&normal_app));

        assert!(requires_end_task_confirmation(&idle));
        assert!(requires_end_task_confirmation(&kernel));
        assert!(requires_end_task_confirmation(&protected));
        assert!(!requires_end_task_confirmation(&non_kernel_pid4));
        assert!(!requires_end_task_confirmation(&normal_app));
        assert!(!requires_end_task_confirmation(&already_eco_bg));
    }

    #[test]
    fn icon_render_cache_deduplicates_same_arc_and_respects_capacity() {
        let mut cache = IconRenderCache::with_capacity(2);

        let icon_a = ExeIconRgba {
            width: 2,
            height: 2,
            rgba: Arc::from([0x10u8; 16]),
        };
        let icon_a_clone = icon_a.clone();
        let icon_b = ExeIconRgba {
            width: 2,
            height: 2,
            rgba: Arc::from([0x20u8; 16]),
        };
        let icon_c = ExeIconRgba {
            width: 2,
            height: 2,
            rgba: Arc::from([0x30u8; 16]),
        };

        let first = cache
            .get_or_insert(&icon_a)
            .expect("valid RGBA icon renders");
        let second = cache
            .get_or_insert(&icon_a_clone)
            .expect("cloned Arc hits cache");
        assert!(
            Arc::ptr_eq(&first, &second),
            "identical icon Arc must reuse the exact same RenderImage without re-converting"
        );
        assert_eq!(cache.len(), 1);

        let img_b = cache.get_or_insert(&icon_b).expect("second unique icon");
        assert!(!Arc::ptr_eq(&first, &img_b));
        assert_eq!(cache.len(), 2);

        // Inserting a third live icon triggers bounded eviction so cache never grows unbounded.
        let _img_c = cache.get_or_insert(&icon_c).expect("third icon");
        assert!(cache.len() <= 2);

        // Invalid icons return None and do not pollute the cache.
        let invalid = ExeIconRgba {
            width: 0,
            height: 0,
            rgba: Arc::from([]),
        };
        assert!(cache.get_or_insert(&invalid).is_none());
    }

    #[test]
    fn resource_display_mode_toggle_updates_only_selected_column() {
        let (m, d, n) = apply_resource_display_mode(
            ResourceDisplayMode::Values,
            ResourceDisplayMode::Values,
            ResourceDisplayMode::Values,
            ResourceColumn::Memory,
            ResourceDisplayMode::Percent,
        );
        assert_eq!(
            (m, d, n),
            (
                ResourceDisplayMode::Percent,
                ResourceDisplayMode::Values,
                ResourceDisplayMode::Values
            )
        );

        let (m, d, n) = apply_resource_display_mode(
            m,
            d,
            n,
            ResourceColumn::Disk,
            ResourceDisplayMode::Percent,
        );
        assert_eq!(
            (m, d, n),
            (
                ResourceDisplayMode::Percent,
                ResourceDisplayMode::Percent,
                ResourceDisplayMode::Values
            )
        );

        let (m, d, n) = apply_resource_display_mode(
            m,
            d,
            n,
            ResourceColumn::Network,
            ResourceDisplayMode::Percent,
        );
        assert_eq!(
            (m, d, n),
            (
                ResourceDisplayMode::Percent,
                ResourceDisplayMode::Percent,
                ResourceDisplayMode::Percent
            )
        );

        let (m, d, n) = apply_resource_display_mode(
            m,
            d,
            n,
            ResourceColumn::Memory,
            ResourceDisplayMode::Values,
        );
        assert_eq!(
            (m, d, n),
            (
                ResourceDisplayMode::Values,
                ResourceDisplayMode::Percent,
                ResourceDisplayMode::Percent
            )
        );
    }

    #[test]
    fn next_sort_direction_toggles_active_and_defaults_appropriately() {
        // Clicking the currently active column flips its direction.
        assert_eq!(
            next_sort_direction(
                ProcessSortColumn::Cpu,
                SortDirection::Descending,
                ProcessSortColumn::Cpu
            ),
            SortDirection::Ascending
        );
        assert_eq!(
            next_sort_direction(
                ProcessSortColumn::Cpu,
                SortDirection::Ascending,
                ProcessSortColumn::Cpu
            ),
            SortDirection::Descending
        );

        // Switching to Name defaults to Ascending; switching to metric columns defaults to Descending.
        assert_eq!(
            next_sort_direction(
                ProcessSortColumn::Cpu,
                SortDirection::Descending,
                ProcessSortColumn::Name
            ),
            SortDirection::Ascending
        );
        assert_eq!(
            next_sort_direction(
                ProcessSortColumn::Name,
                SortDirection::Ascending,
                ProcessSortColumn::Memory
            ),
            SortDirection::Descending
        );
    }

    #[test]
    fn smart_copy_formats_short_and_structured_process_summary() {
        let mut row = sample_process_row(
            1234,
            99_000,
            "chrome.exe",
            "Google Chrome",
            ProcessCategory::App,
            ProcessStatusFlags::EFFICIENCY_MODE,
        );
        row.cpu_permille = 125;
        row.memory_bytes = 512 * 1024 * 1024;
        row.disk_bytes_per_sec = 1024 * 1024;
        row.net_bytes_per_sec = 125_000;
        row.gpu_util_permille = 40;
        row.publisher = Some(Arc::from("Google LLC"));
        row.command_line = Some(Arc::from(r#""C:\chrome.exe" --profile"#));

        assert_eq!(format_process_copy_short(&row), "chrome.exe (1234)");

        let labels = StatusLabels::new("Efficiency mode", "Suspended", "Not responding");
        let status = format_process_status_text(row.status, &labels, "Elevated", "Running");
        assert_eq!(status, "Efficiency mode");

        let summary = format_process_row_summary(
            &row,
            ResourceDisplayMode::Values,
            ResourceDisplayMode::Values,
            ResourceDisplayMode::Values,
            &status,
        );
        assert!(summary.contains("Name: Google Chrome (chrome.exe)"));
        assert!(summary.contains("PID: 1234"));
        assert!(summary.contains("Status: Efficiency mode"));
        assert!(summary.contains("CPU: 12.5%"));
        assert!(summary.contains("Memory: 512.0 MB"));
        assert!(summary.contains("GPU: 4.0%"));
        assert!(summary.contains("Publisher: Google LLC"));
        assert!(summary.contains(r#"Command Line: "C:\chrome.exe" --profile"#));
    }
}
