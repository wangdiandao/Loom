//! Pure state helpers, action target snapshot, formatting, and selection predicates
//! for [`super::DetailsView`].

use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::SharedString;
use loom_types::{
    CapabilityFlags, DetailsRowProjection, DetailsSortColumn, ProcessKey, ProcessPriority,
    ProcessStatusFlags, SortDirection, UacVirtualization,
};
use loom_ui_kit::{HeatmapLevel, format_bytes, format_permille};

use crate::features::table_columns::format_process_name_and_pid;

/// Scheduling priority classes displayed in the `Set priority >` submenu (highest to lowest).
pub(super) const PRIORITY_MENU_ORDER: [ProcessPriority; 6] = [
    ProcessPriority::Realtime,
    ProcessPriority::High,
    ProcessPriority::AboveNormal,
    ProcessPriority::Normal,
    ProcessPriority::BelowNormal,
    ProcessPriority::Idle,
];

/// Fallback total physical memory (16 GiB) used only before the first performance sample arrives.
const FALLBACK_TOTAL_RAM_BYTES: u64 = 16 * 1024 * 1024 * 1024;

/// Maximum cached domain-stripped user names retained per Details table instance.
const MAX_CACHED_USER_NAMES: usize = 128;

/// Snapshot of a process row needed by dialogs and right-click context menu handlers.
#[derive(Debug, Clone)]
pub(super) struct ProcessActionTarget {
    pub(super) key: ProcessKey,
    pub(super) name: Arc<str>,
    pub(super) status: ProcessStatusFlags,
    pub(super) priority: ProcessPriority,
    pub(super) uac_virtualization: UacVirtualization,
    pub(super) exe_path: Option<Arc<str>>,
    pub(super) has_leak_warning: bool,
}

impl ProcessActionTarget {
    pub(super) fn from_row(row: &DetailsRowProjection) -> Self {
        Self {
            key: row.key,
            name: Arc::clone(&row.name),
            status: row.status,
            priority: row.priority,
            uac_virtualization: row.uac_virtualization,
            exe_path: row.exe_path.clone(),
            has_leak_warning: row.leak_warning.is_some(),
        }
    }
}

/// Formats the short `Ctrl+C` clipboard text (`"<name> (<pid>)"`) for a Details row.
pub(super) fn format_details_copy_short(row: &DetailsRowProjection) -> String {
    format_process_name_and_pid(row.name.trim(), row.key.pid)
}

/// Formats the structured multi-field `Ctrl+Shift+C` summary for a Details row.
pub(super) fn format_details_row_summary(
    row: &DetailsRowProjection,
    status_text: &str,
    priority_text: &str,
    arch_text: &str,
) -> String {
    let user = display_user_name(row.user_name.as_deref());
    let cpu = format_permille(row.cpu_permille);
    let mem = format_bytes(row.private_working_set_bytes);
    let gpu = format_bytes(row.gpu_dedicated_bytes);
    let publisher = row
        .publisher
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("—");
    let exe_path = row
        .exe_path
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
        "Name: {}\nPID: {}\nStatus: {status_text}\nUser: {user}\nCPU: {cpu}\nMemory: {mem}\nPriority: {priority_text}\nHandles: {}\nThreads: {}\nPlatform: {arch_text}\nGPU Memory: {gpu}\nPublisher: {publisher}\nPath: {exe_path}\nCommand Line: {cmdline}",
        row.name, row.key.pid, row.handle_count, row.thread_count
    )
}

/// Finds the current display row index of `target` by composite [`ProcessKey`].
///
/// Immune to live row re-sorting and OS PID reuse because both `pid` and
/// `create_time_100ns` must match.
#[inline]
pub(super) fn find_row_index_by_key(
    rows: &[DetailsRowProjection],
    target: ProcessKey,
) -> Option<usize> {
    rows.iter().position(|row| row.key == target)
}

/// Fast-path variant of [`find_row_index_by_key`] checking `hint_idx` in `O(1)` before scanning.
#[inline]
pub(super) fn find_row_index_by_key_fast(
    rows: &[DetailsRowProjection],
    target: ProcessKey,
    hint_idx: Option<usize>,
) -> Option<usize> {
    if let Some(idx) = hint_idx
        && let Some(row) = rows.get(idx)
        && row.key == target
    {
        return Some(idx);
    }
    find_row_index_by_key(rows, target)
}

/// Returns `true` when a process can be targeted by End Task / End Process Tree
/// (excludes OS idle pseudo-processes and kernel pseudo-processes).
#[inline]
pub(super) fn can_terminate_details_row(status: ProcessStatusFlags) -> bool {
    !status.intersects(ProcessStatusFlags::SYSTEM_IDLE | ProcessStatusFlags::KERNEL)
}

/// Returns `true` when `status` represents a critical, kernel, idle, or OS-protected process
/// that should show an explicit confirmation warning before termination.
#[inline]
pub(super) fn is_critical_or_system_process(status: ProcessStatusFlags) -> bool {
    status.intersects(
        ProcessStatusFlags::CRITICAL
            | ProcessStatusFlags::PROTECTED
            | ProcessStatusFlags::KERNEL
            | ProcessStatusFlags::SYSTEM_IDLE,
    )
}

/// Returns `true` if Efficiency Mode (`EcoQoS`) can be toggled for this process.
///
/// Disabled for idle, kernel, protected, or critical processes, and for backends
/// that do not advertise [`CapabilityFlags::ECO_MODE`].
#[inline]
pub(super) fn is_eco_mode_eligible(
    status: ProcessStatusFlags,
    capabilities: CapabilityFlags,
) -> bool {
    capabilities.contains(CapabilityFlags::ECO_MODE)
        && !status.intersects(
            ProcessStatusFlags::SYSTEM_IDLE
                | ProcessStatusFlags::KERNEL
                | ProcessStatusFlags::PROTECTED
                | ProcessStatusFlags::CRITICAL,
        )
}

/// Returns `true` if scheduling priority can be changed for this process.
#[inline]
pub(super) fn can_set_priority(status: ProcessStatusFlags, capabilities: CapabilityFlags) -> bool {
    capabilities.contains(CapabilityFlags::PROCESS_PRIORITY)
        && !status.intersects(ProcessStatusFlags::SYSTEM_IDLE | ProcessStatusFlags::KERNEL)
}

/// Returns `true` if CPU affinity can be changed for this process.
#[inline]
pub(super) fn can_set_affinity(status: ProcessStatusFlags, capabilities: CapabilityFlags) -> bool {
    capabilities.contains(CapabilityFlags::CPU_AFFINITY)
        && !status.intersects(ProcessStatusFlags::SYSTEM_IDLE | ProcessStatusFlags::KERNEL)
}

/// Returns the default initial sort direction when switching to a different column.
#[inline]
pub(super) const fn default_sort_direction(column: DetailsSortColumn) -> SortDirection {
    match column {
        DetailsSortColumn::Cpu
        | DetailsSortColumn::CpuTime
        | DetailsSortColumn::Memory
        | DetailsSortColumn::WorkingSet
        | DetailsSortColumn::WorkingSetDelta
        | DetailsSortColumn::CommitSize
        | DetailsSortColumn::Priority
        | DetailsSortColumn::Handles
        | DetailsSortColumn::Threads
        | DetailsSortColumn::Elevated
        | DetailsSortColumn::GpuMemory
        | DetailsSortColumn::Status => SortDirection::Descending,
        DetailsSortColumn::Name
        | DetailsSortColumn::Pid
        | DetailsSortColumn::UserName
        | DetailsSortColumn::SessionId
        | DetailsSortColumn::Arch
        | DetailsSortColumn::UacVirtualization
        | DetailsSortColumn::Publisher
        | DetailsSortColumn::Description
        | DetailsSortColumn::ExePath
        | DetailsSortColumn::CommandLine => SortDirection::Ascending,
    }
}

/// Computes the next sort direction when clicking `clicked` given the active column/direction.
#[inline]
pub(super) fn next_sort_direction(
    active_column: DetailsSortColumn,
    active_direction: SortDirection,
    clicked: DetailsSortColumn,
) -> SortDirection {
    if active_column == clicked {
        active_direction.toggle()
    } else {
        default_sort_direction(clicked)
    }
}

/// Strips a leading Windows computer/domain prefix (`HOST\User` -> `User`) for compact display.
#[inline]
pub(super) fn display_user_name(raw: Option<&str>) -> &str {
    let Some(name) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return "—";
    };
    match name.rsplit_once('\\') {
        Some((_, account)) if !account.is_empty() => account,
        _ => name,
    }
}

/// Computes a stable element ID hash from composite [`ProcessKey`].
#[inline]
pub(super) fn process_key_element_id(key: ProcessKey) -> usize {
    let mixed = (u64::from(key.pid) << 32) ^ key.create_time_100ns.rotate_left(13);
    mixed as usize
}

/// Computes a heatmap level for private working set memory relative to total system RAM.
#[inline]
pub(super) fn memory_heatmap_level(
    private_working_set_bytes: u64,
    total_ram_bytes: u64,
) -> HeatmapLevel {
    let denom = if total_ram_bytes > 0 {
        total_ram_bytes
    } else {
        FALLBACK_TOTAL_RAM_BYTES
    };
    let permille =
        ((u128::from(private_working_set_bytes) * 1_000) / u128::from(denom)).min(1_000) as u16;
    HeatmapLevel::from_permille(permille)
}

/// Returns a display [`SharedString`] for a process user name without allocating in steady state.
///
/// Clean single-segment names reuse `Arc::clone` directly; domain-prefixed Windows names
/// (e.g. `HOST\User`) are memoized in `cache` (typically < 10 distinct accounts per machine).
pub(super) fn cached_user_name(
    cache: &mut HashMap<String, SharedString>,
    raw: Option<&Arc<str>>,
) -> SharedString {
    let Some(arc) = raw else {
        return SharedString::new_static("—");
    };
    let trimmed = arc.trim();
    if trimmed.is_empty() {
        return SharedString::new_static("—");
    }
    if trimmed.len() == arc.len() && !trimmed.contains('\\') {
        return SharedString::from(Arc::clone(arc));
    }
    if let Some(cached) = cache.get(arc.as_ref()) {
        return cached.clone();
    }
    let formatted: SharedString = display_user_name(Some(arc.as_ref())).to_owned().into();
    if cache.len() >= MAX_CACHED_USER_NAMES {
        cache.clear();
    }
    cache.insert(arc.to_string(), formatted.clone());
    formatted
}
