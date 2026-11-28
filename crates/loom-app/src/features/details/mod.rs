//! On-demand Details vertical feature slice gated by an RAII [`ScopedMonitorLease`].
//!
//! Splits the static toolbar + highlight banner ([`DetailsView`]) from the
//! high-frequency 2D-virtualized `gpui-kit` `DataTable` (`Entity<TableState<DetailsTableDelegate>>`)
//! so live snapshot ticks update only the table entity while `gpui-fast` reuses
//! the surrounding shell and toolbar as retained subtrees.

mod affinity_dialog;
mod wait_chain_dialog;

use std::cell::Cell;
use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName as AssetIconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableEvent, TableState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, ElementExt as _, Icon, IconName, Sizable as _, Size,
    StyledExt as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Bounds, ClipboardItem, Context, Empty, Entity, EntityInputHandler,
    FocusHandle, Focusable, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, ParentElement as _, Pixels, Point, Render, ScrollStrategy, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, UTF16Selection, WeakEntity,
    Window, div, img, prelude::FluentBuilder as _, px,
};
use loom_engine::{EngineCommand, EngineCoordinator};
use loom_pal::NativeBackend;
use loom_types::{
    CapabilityFlags, DetailsRowProjection, DetailsSortColumn, DetailsTableSlice, DetailsViewMode,
    LanguagePreference, MonitorDomain, PlatformProfile, ProcessArch, ProcessKey, ProcessPriority,
    ProcessStatusFlags, SortDirection, UacVirtualization, WebView2Role,
};
use loom_ui_kit::{
    CachedSemanticColors, DEFAULT_CELL_PREVIEW_CHARS, HeatmapLevel, MetricDisplay, RowTextCache,
    format_bytes, format_cpu_time, format_permille, format_permille_metric, format_signed_bytes,
    heatmap_cell_metric, quantize_bytes_display, quantize_cpu_time_secs,
    quantize_signed_bytes_display,
};

use crate::bridge::{ScopedMonitorLease, UiCommands, UiStore, UiStoreEvent};
use crate::features::table_columns::{
    ColumnKey, ColumnSpec, ColumnToggleItem, CopyRowFields, DataTablePane, FnvHasher64,
    HeaderMenuAction, IconRenderCache, OrderFreezeState, ScrollBurstCoalescer, SortPausedBadge,
    TableColumnLayout, TableInputBridge, TypeToSearchState, build_column_header_menu,
    build_copy_submenu, can_suspend_or_resume_process, dispatch_ignore_leak_path,
    dispatch_restart_explorer, dispatch_suspend_or_resume, expand_chevron_button,
    format_process_name_and_pid, is_explorer_image, leak_ignore_target, leak_warning_badge,
    open_select_columns_dialog, should_attach_cell_tooltip, spawn_csv_export, webview2_role_badge,
    webview2_role_label,
};
use crate::i18n;
use crate::shell::{ShellHandle, dialogs};

const DETAILS_HEADER_HEIGHT: f32 = 32.0;

// Default column widths (px) kept wide enough so horizontal scrolling prevents cramping.
const COL_NAME_W: f32 = 240.0;
const COL_PID_W: f32 = 76.0;
const COL_STATUS_W: f32 = 96.0;
const COL_USER_W: f32 = 140.0;
const COL_SESSION_W: f32 = 84.0;
const COL_CPU_W: f32 = 82.0;
const COL_CPU_TIME_W: f32 = 96.0;
const COL_MEM_W: f32 = 184.0;
const COL_WORKING_SET_W: f32 = 128.0;
const COL_WORKING_SET_DELTA_W: f32 = 168.0;
const COL_COMMIT_W: f32 = 112.0;
const COL_PRIORITY_W: f32 = 112.0;
const COL_HANDLES_W: f32 = 84.0;
const COL_THREADS_W: f32 = 76.0;
const COL_PLATFORM_W: f32 = 86.0;
const COL_UAC_W: f32 = 132.0;
const COL_ELEVATED_W: f32 = 76.0;
const COL_GPU_MEM_W: f32 = 144.0;
const COL_PUBLISHER_W: f32 = 164.0;
const COL_DESCRIPTION_W: f32 = 200.0;
const COL_EXE_PATH_W: f32 = 300.0;
const COL_CMDLINE_W: f32 = 360.0;

#[cfg(test)]
const TABLE_HORIZONTAL_PADDING: f32 = 16.0;

/// Logical columns supported by [`DetailsView`] (distinct diagnostic column set from Processes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailsColumn {
    Name,
    Pid,
    Status,
    UserName,
    SessionId,
    Cpu,
    CpuTime,
    Memory,
    WorkingSet,
    WorkingSetDelta,
    CommitSize,
    Priority,
    Handles,
    Threads,
    Platform,
    UacVirtualization,
    Elevated,
    GpuMemory,
    Publisher,
    Description,
    ExePath,
    CommandLine,
}

impl ColumnKey for DetailsColumn {
    fn slug(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Pid => "pid",
            Self::Status => "status",
            Self::UserName => "user",
            Self::SessionId => "session_id",
            Self::Cpu => "cpu",
            Self::CpuTime => "cpu_time",
            Self::Memory => "memory",
            Self::WorkingSet => "working_set",
            Self::WorkingSetDelta => "working_set_delta",
            Self::CommitSize => "commit_size",
            Self::Priority => "priority",
            Self::Handles => "handles",
            Self::Threads => "threads",
            Self::Platform => "platform",
            Self::UacVirtualization => "uac",
            Self::Elevated => "elevated",
            Self::GpuMemory => "gpu_memory",
            Self::Publisher => "publisher",
            Self::Description => "description",
            Self::ExePath => "exe_path",
            Self::CommandLine => "command_line",
        }
    }
}

impl DetailsColumn {
    fn default_specs(capabilities: CapabilityFlags) -> Vec<ColumnSpec<Self>> {
        let mut cols = vec![
            ColumnSpec::pinned(Self::Name, COL_NAME_W, 140.0, 640.0),
            ColumnSpec::new(Self::Pid, COL_PID_W, 56.0, 160.0),
            ColumnSpec::new(Self::Status, COL_STATUS_W, 68.0, 200.0),
            ColumnSpec::new(Self::UserName, COL_USER_W, 80.0, 360.0),
            ColumnSpec::hidden(Self::SessionId, COL_SESSION_W, 64.0, 160.0),
            ColumnSpec::new(Self::Cpu, COL_CPU_W, 60.0, 180.0),
            ColumnSpec::hidden(Self::CpuTime, COL_CPU_TIME_W, 76.0, 200.0),
            ColumnSpec::new(Self::Memory, COL_MEM_W, 96.0, 320.0),
            ColumnSpec::hidden(Self::WorkingSet, COL_WORKING_SET_W, 88.0, 280.0),
            ColumnSpec::hidden(Self::WorkingSetDelta, COL_WORKING_SET_DELTA_W, 96.0, 320.0),
            ColumnSpec::hidden(Self::CommitSize, COL_COMMIT_W, 84.0, 260.0),
            ColumnSpec::new(Self::Priority, COL_PRIORITY_W, 72.0, 240.0),
            ColumnSpec::hidden(Self::Handles, COL_HANDLES_W, 64.0, 180.0),
            ColumnSpec::hidden(Self::Threads, COL_THREADS_W, 60.0, 160.0),
            ColumnSpec::new(Self::Platform, COL_PLATFORM_W, 64.0, 180.0),
        ];
        if capabilities.contains(CapabilityFlags::UAC_VIRTUALIZATION) {
            cols.push(ColumnSpec::new(
                Self::UacVirtualization,
                COL_UAC_W,
                84.0,
                260.0,
            ));
        }
        cols.extend([
            ColumnSpec::hidden(Self::Elevated, COL_ELEVATED_W, 60.0, 160.0),
            ColumnSpec::new(Self::GpuMemory, COL_GPU_MEM_W, 88.0, 280.0),
            ColumnSpec::new(Self::Publisher, COL_PUBLISHER_W, 88.0, 420.0),
            ColumnSpec::hidden(Self::Description, COL_DESCRIPTION_W, 100.0, 480.0),
            ColumnSpec::hidden(Self::ExePath, COL_EXE_PATH_W, 120.0, 900.0),
            ColumnSpec::new(Self::CommandLine, COL_CMDLINE_W, 140.0, 1200.0),
        ]);
        cols
    }

    fn selectable_order(capabilities: CapabilityFlags) -> Vec<Self> {
        let mut order = vec![
            Self::Pid,
            Self::Status,
            Self::UserName,
            Self::SessionId,
            Self::Cpu,
            Self::CpuTime,
            Self::Memory,
            Self::WorkingSet,
            Self::WorkingSetDelta,
            Self::CommitSize,
            Self::Priority,
            Self::Handles,
            Self::Threads,
            Self::Platform,
        ];
        if capabilities.contains(CapabilityFlags::UAC_VIRTUALIZATION) {
            order.push(Self::UacVirtualization);
        }
        order.extend([
            Self::Elevated,
            Self::GpuMemory,
            Self::Publisher,
            Self::Description,
            Self::ExePath,
            Self::CommandLine,
        ]);
        order
    }

    fn label(self, platform: PlatformProfile) -> String {
        match self {
            Self::Name => i18n::tr("details.col_name"),
            Self::Pid => i18n::tr("details.col_pid"),
            Self::Status => i18n::tr("details.col_status"),
            Self::UserName => i18n::tr("details.col_user"),
            Self::SessionId => i18n::tr("details.col_session_id"),
            Self::Cpu => i18n::tr("details.col_cpu"),
            Self::CpuTime => i18n::tr("details.col_cpu_time"),
            Self::Memory => i18n::tr("details.col_memory"),
            Self::WorkingSet => i18n::tr("details.col_working_set"),
            Self::WorkingSetDelta => i18n::tr("details.col_working_set_delta"),
            Self::CommitSize => i18n::tr("details.col_commit"),
            Self::Priority => i18n::tr("details.col_priority"),
            Self::Handles => i18n::tr("details.col_handles"),
            Self::Threads => i18n::tr("details.col_threads"),
            Self::Platform => i18n::tr("details.col_platform"),
            Self::UacVirtualization => i18n::virtualization_column_label(platform),
            Self::Elevated => i18n::tr("details.col_elevated"),
            Self::GpuMemory => i18n::tr("details.col_gpu_memory"),
            Self::Publisher => i18n::tr("details.col_publisher"),
            Self::Description => i18n::tr("details.col_description"),
            Self::ExePath => i18n::tr("details.col_exe_path"),
            Self::CommandLine => i18n::tr("details.col_command_line"),
        }
    }

    const fn sort_column(self) -> Option<DetailsSortColumn> {
        match self {
            Self::Name => Some(DetailsSortColumn::Name),
            Self::Pid => Some(DetailsSortColumn::Pid),
            Self::Status => Some(DetailsSortColumn::Status),
            Self::UserName => Some(DetailsSortColumn::UserName),
            Self::SessionId => Some(DetailsSortColumn::SessionId),
            Self::Cpu => Some(DetailsSortColumn::Cpu),
            Self::CpuTime => Some(DetailsSortColumn::CpuTime),
            Self::Memory => Some(DetailsSortColumn::Memory),
            Self::WorkingSet => Some(DetailsSortColumn::WorkingSet),
            Self::WorkingSetDelta => Some(DetailsSortColumn::WorkingSetDelta),
            Self::CommitSize => Some(DetailsSortColumn::CommitSize),
            Self::Priority => Some(DetailsSortColumn::Priority),
            Self::Handles => Some(DetailsSortColumn::Handles),
            Self::Threads => Some(DetailsSortColumn::Threads),
            Self::Platform => Some(DetailsSortColumn::Arch),
            Self::UacVirtualization => Some(DetailsSortColumn::UacVirtualization),
            Self::Elevated => Some(DetailsSortColumn::Elevated),
            Self::GpuMemory => Some(DetailsSortColumn::GpuMemory),
            Self::Publisher => Some(DetailsSortColumn::Publisher),
            Self::Description => Some(DetailsSortColumn::Description),
            Self::ExePath => Some(DetailsSortColumn::ExePath),
            Self::CommandLine => Some(DetailsSortColumn::CommandLine),
        }
    }

    const fn right_aligned(self) -> bool {
        matches!(
            self,
            Self::Pid
                | Self::SessionId
                | Self::Cpu
                | Self::CpuTime
                | Self::Memory
                | Self::WorkingSet
                | Self::WorkingSetDelta
                | Self::CommitSize
                | Self::Handles
                | Self::Threads
                | Self::GpuMemory
        )
    }

    const fn is_heatmap(self) -> bool {
        matches!(self, Self::Cpu | Self::Memory)
    }
}

/// Total minimum default width of the Details table, adapting to advertised [`CapabilityFlags`].
#[cfg(test)]
pub(super) fn table_min_width(capabilities: CapabilityFlags) -> f32 {
    TableColumnLayout::new(DetailsColumn::default_specs(capabilities))
        .visible_columns()
        .iter()
        .map(|c| c.width)
        .sum::<f32>()
        + TABLE_HORIZONTAL_PADDING
}

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

#[inline]
const fn to_column_sort(dir: SortDirection) -> ColumnSort {
    match dir {
        SortDirection::Ascending => ColumnSort::Ascending,
        SortDirection::Descending => ColumnSort::Descending,
    }
}

/// Snapshot of a process row needed by dialogs and right-click context menu handlers.
#[derive(Debug, Clone)]
struct ProcessActionTarget {
    key: ProcessKey,
    name: Arc<str>,
    status: ProcessStatusFlags,
    priority: ProcessPriority,
    uac_virtualization: UacVirtualization,
    exe_path: Option<Arc<str>>,
    has_leak_warning: bool,
}

impl ProcessActionTarget {
    fn from_row(row: &DetailsRowProjection) -> Self {
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
fn find_row_index_by_key_fast(
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
fn process_key_element_id(key: ProcessKey) -> usize {
    let mixed = (u64::from(key.pid) << 32) ^ key.create_time_100ns.rotate_left(13);
    mixed as usize
}

/// Computes a heatmap level for private working set memory relative to total system RAM.
#[inline]
fn memory_heatmap_level(private_working_set_bytes: u64, total_ram_bytes: u64) -> HeatmapLevel {
    let denom = if total_ram_bytes > 0 {
        total_ram_bytes
    } else {
        FALLBACK_TOTAL_RAM_BYTES
    };
    let permille =
        ((u128::from(private_working_set_bytes) * 1_000) / u128::from(denom)).min(1_000) as u16;
    HeatmapLevel::from_permille(permille)
}

/// Maximum cached domain-stripped user names retained per Details table instance.
const MAX_CACHED_USER_NAMES: usize = 128;

/// Returns a display [`SharedString`] for a process user name without allocating in steady state.
///
/// Clean single-segment names reuse `Arc::clone` directly; domain-prefixed Windows names
/// (e.g. `HOST\User`) are memoized in `cache` (typically < 10 distinct accounts per machine).
fn cached_user_name(
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

/// Renders a right-aligned monospace numeric cell from a split [`MetricDisplay`] so
/// `metric.value` hits `gpui-fast::NumberShaping` and `metric.unit` hits `LineLayoutCache`.
#[inline]
fn render_metric_cell(
    metric: MetricDisplay,
    mono_font: SharedString,
    text_color: gpui_kit::Hsla,
) -> gpui_kit::AnyElement {
    let mut cell = h_flex()
        .size_full()
        .items_center()
        .justify_end()
        .text_xs()
        .font_family(mono_font)
        .text_color(text_color)
        .child(metric.value);
    if !metric.unit.is_empty() {
        cell = cell.child(SharedString::new_static(metric.unit));
    }
    cell.into_any_element()
}

/// Cached localized strings for [`DetailsTableDelegate`] to avoid per-frame `i18n::tr` allocations.
struct DetailsLocaleCache {
    col_labels: [SharedString; 22],
    sort_paused_tooltip: SharedString,
    running: SharedString,
    suspended: SharedString,
    elevated_yes: SharedString,
    elevated_no: SharedString,
    loading: SharedString,
    empty_search: SharedString,
    priority_idle: SharedString,
    priority_below_normal: SharedString,
    priority_normal: SharedString,
    priority_above_normal: SharedString,
    priority_high: SharedString,
    priority_realtime: SharedString,
    arch_x64: SharedString,
    arch_x86: SharedString,
    arch_arm64: SharedString,
    arch_unknown: SharedString,
    uac_enabled: SharedString,
    uac_disabled: SharedString,
    uac_not_allowed: SharedString,
    uac_unknown: SharedString,
    wv2_browser: SharedString,
    wv2_renderer: SharedString,
    wv2_gpu: SharedString,
    wv2_network: SharedString,
    wv2_storage: SharedString,
    wv2_audio: SharedString,
    wv2_utility: SharedString,
    wv2_crashpad: SharedString,
}

impl DetailsLocaleCache {
    fn build(platform: PlatformProfile) -> Self {
        Self {
            col_labels: [
                DetailsColumn::Name.label(platform).into(),
                DetailsColumn::Pid.label(platform).into(),
                DetailsColumn::Status.label(platform).into(),
                DetailsColumn::UserName.label(platform).into(),
                DetailsColumn::SessionId.label(platform).into(),
                DetailsColumn::Cpu.label(platform).into(),
                DetailsColumn::CpuTime.label(platform).into(),
                DetailsColumn::Memory.label(platform).into(),
                DetailsColumn::WorkingSet.label(platform).into(),
                DetailsColumn::WorkingSetDelta.label(platform).into(),
                DetailsColumn::CommitSize.label(platform).into(),
                DetailsColumn::Priority.label(platform).into(),
                DetailsColumn::Handles.label(platform).into(),
                DetailsColumn::Threads.label(platform).into(),
                DetailsColumn::Platform.label(platform).into(),
                DetailsColumn::UacVirtualization.label(platform).into(),
                DetailsColumn::Elevated.label(platform).into(),
                DetailsColumn::GpuMemory.label(platform).into(),
                DetailsColumn::Publisher.label(platform).into(),
                DetailsColumn::Description.label(platform).into(),
                DetailsColumn::ExePath.label(platform).into(),
                DetailsColumn::CommandLine.label(platform).into(),
            ],
            sort_paused_tooltip: i18n::tr("proc_ux_sort_paused_tooltip").into(),
            running: i18n::tr("details.status_running").into(),
            suspended: i18n::tr("details.status_suspended").into(),
            elevated_yes: i18n::tr("details.elevated_yes").into(),
            elevated_no: i18n::tr("details.elevated_no").into(),
            loading: i18n::tr("details.loading").into(),
            empty_search: i18n::tr("details.empty_search").into(),
            priority_idle: i18n::process_priority_label(ProcessPriority::Idle).into(),
            priority_below_normal: i18n::process_priority_label(ProcessPriority::BelowNormal)
                .into(),
            priority_normal: i18n::process_priority_label(ProcessPriority::Normal).into(),
            priority_above_normal: i18n::process_priority_label(ProcessPriority::AboveNormal)
                .into(),
            priority_high: i18n::process_priority_label(ProcessPriority::High).into(),
            priority_realtime: i18n::process_priority_label(ProcessPriority::Realtime).into(),
            arch_x64: i18n::process_arch_label(ProcessArch::X64).into(),
            arch_x86: i18n::process_arch_label(ProcessArch::X86).into(),
            arch_arm64: i18n::process_arch_label(ProcessArch::Arm64).into(),
            arch_unknown: i18n::process_arch_label(ProcessArch::Unknown).into(),
            uac_enabled: i18n::uac_virtualization_label(UacVirtualization::Enabled).into(),
            uac_disabled: i18n::uac_virtualization_label(UacVirtualization::Disabled).into(),
            uac_not_allowed: i18n::uac_virtualization_label(UacVirtualization::NotAllowed).into(),
            uac_unknown: i18n::uac_virtualization_label(UacVirtualization::Unknown).into(),
            wv2_browser: webview2_role_label(WebView2Role::Browser).into(),
            wv2_renderer: webview2_role_label(WebView2Role::Renderer).into(),
            wv2_gpu: webview2_role_label(WebView2Role::Gpu).into(),
            wv2_network: webview2_role_label(WebView2Role::Network).into(),
            wv2_storage: webview2_role_label(WebView2Role::Storage).into(),
            wv2_audio: webview2_role_label(WebView2Role::Audio).into(),
            wv2_utility: webview2_role_label(WebView2Role::Utility).into(),
            wv2_crashpad: webview2_role_label(WebView2Role::Crashpad).into(),
        }
    }

    #[inline]
    fn column_label(&self, col: DetailsColumn) -> SharedString {
        self.col_labels[col as usize].clone()
    }

    #[inline]
    fn webview2_label(&self, role: WebView2Role) -> SharedString {
        match role {
            WebView2Role::Browser => self.wv2_browser.clone(),
            WebView2Role::Renderer => self.wv2_renderer.clone(),
            WebView2Role::Gpu => self.wv2_gpu.clone(),
            WebView2Role::Network => self.wv2_network.clone(),
            WebView2Role::Storage => self.wv2_storage.clone(),
            WebView2Role::Audio => self.wv2_audio.clone(),
            WebView2Role::Utility => self.wv2_utility.clone(),
            WebView2Role::Crashpad => self.wv2_crashpad.clone(),
        }
    }

    #[inline]
    fn priority_label(&self, p: ProcessPriority) -> SharedString {
        match p {
            ProcessPriority::Idle => self.priority_idle.clone(),
            ProcessPriority::BelowNormal => self.priority_below_normal.clone(),
            ProcessPriority::Normal => self.priority_normal.clone(),
            ProcessPriority::AboveNormal => self.priority_above_normal.clone(),
            ProcessPriority::High => self.priority_high.clone(),
            ProcessPriority::Realtime => self.priority_realtime.clone(),
        }
    }

    #[inline]
    fn arch_label(&self, arch: ProcessArch) -> SharedString {
        match arch {
            ProcessArch::X64 => self.arch_x64.clone(),
            ProcessArch::X86 => self.arch_x86.clone(),
            ProcessArch::Arm64 => self.arch_arm64.clone(),
            ProcessArch::Unknown => self.arch_unknown.clone(),
        }
    }

    #[inline]
    fn uac_label(&self, uac: UacVirtualization) -> SharedString {
        match uac {
            UacVirtualization::Enabled => self.uac_enabled.clone(),
            UacVirtualization::Disabled => self.uac_disabled.clone(),
            UacVirtualization::NotAllowed => self.uac_not_allowed.clone(),
            UacVirtualization::Unknown => self.uac_unknown.clone(),
        }
    }
}

/// Terminates a process immediately for standard user processes, or opens a feature-local
/// confirmation dialog for critical/system processes.
fn request_end_task(
    store: &Entity<UiStore>,
    target: &ProcessActionTarget,
    window: &mut Window,
    cx: &mut App,
) {
    if !is_critical_or_system_process(target.status) {
        UiCommands::terminate_process(store, target.key, cx);
        return;
    }

    let store = store.clone();
    let key = target.key;
    let name = target.name.to_string();
    window.open_dialog(cx, move |dialog, _window, cx| {
        let store_confirm = store.clone();
        let warning = cx.theme().warning;
        let danger = cx.theme().danger;
        let muted_fg = cx.theme().muted_foreground;
        let radius = cx.theme().radius;
        let message = rust_i18n::t!(
            "processes.confirm_end_message",
            name = name.as_str(),
            pid = key.pid
        )
        .into_owned();

        dialog
            .w(px(440.))
            .title(i18n::tr("processes.confirm_end_title"))
            .child(
                v_flex()
                    .w_full()
                    .gap_3()
                    .child(div().text_sm().text_color(muted_fg).child(message))
                    .child(
                        h_flex()
                            .w_full()
                            .items_start()
                            .gap_2()
                            .p_2p5()
                            .rounded(radius)
                            .bg(warning.opacity(0.14))
                            .border_1()
                            .border_color(warning.opacity(0.4))
                            .child(
                                Icon::new(IconName::TriangleAlert)
                                    .small()
                                    .text_color(danger),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_xs()
                                    .text_color(danger)
                                    .child(i18n::tr("processes.confirm_end_critical")),
                            ),
                    ),
            )
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("details-confirm-end-cancel")
                            .outline()
                            .label(i18n::tr("common.cancel"))
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("details-confirm-end-ok")
                            .danger()
                            .label(i18n::tr("processes.confirm_end_action"))
                            .on_click(move |_, window, cx| {
                                UiCommands::terminate_process(&store_confirm, key, cx);
                                window.close_dialog(cx);
                            }),
                    ),
            )
    });
}

/// Opens a feature-local confirmation dialog before terminating an entire process tree
/// in bottom-up (leaves-to-root) order.
fn open_end_process_tree_dialog(
    store: &Entity<UiStore>,
    target: &ProcessActionTarget,
    window: &mut Window,
    cx: &mut App,
) {
    let store = store.clone();
    let key = target.key;
    let name = target.name.to_string();
    let is_critical = is_critical_or_system_process(target.status);

    window.open_dialog(cx, move |dialog, _window, cx| {
        let store_confirm = store.clone();
        let warning = cx.theme().warning;
        let danger = cx.theme().danger;
        let muted_fg = cx.theme().muted_foreground;
        let radius = cx.theme().radius;
        let message = rust_i18n::t!(
            "details.confirm_tree_message",
            name = name.as_str(),
            pid = key.pid
        )
        .into_owned();

        dialog
            .w(px(460.))
            .title(i18n::tr("details.confirm_tree_title"))
            .child(
                v_flex()
                    .w_full()
                    .gap_3()
                    .child(div().text_sm().text_color(muted_fg).child(message))
                    .when(is_critical, |container| {
                        container.child(
                            h_flex()
                                .w_full()
                                .items_start()
                                .gap_2()
                                .p_2p5()
                                .rounded(radius)
                                .bg(warning.opacity(0.14))
                                .border_1()
                                .border_color(warning.opacity(0.4))
                                .child(
                                    Icon::new(IconName::TriangleAlert)
                                        .small()
                                        .text_color(danger),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_xs()
                                        .text_color(danger)
                                        .child(i18n::tr("processes.confirm_end_critical")),
                                ),
                        )
                    }),
            )
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("details-confirm-tree-cancel")
                            .outline()
                            .label(i18n::tr("common.cancel"))
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("details-confirm-tree-ok")
                            .danger()
                            .label(i18n::tr("details.confirm_tree_action"))
                            .on_click(move |_, window, cx| {
                                UiCommands::terminate_process_tree(&store_confirm, key, cx);
                                window.close_dialog(cx);
                            }),
                    ),
            )
    });
}

/// Computes a fast 64-bit viewport fingerprint over the currently visible rows and columns
/// of a [`DetailsTableSlice`] so unchanged live ticks can skip `cx.notify()`.
#[allow(clippy::too_many_arguments)]
fn compute_details_viewport_fingerprint(
    details: &DetailsTableSlice,
    column_layout: &TableColumnLayout<DetailsColumn>,
    visible_rows: &Range<usize>,
    visible_cols: &Range<usize>,
    selected_idx: Option<usize>,
    highlighted_key: Option<ProcessKey>,
    total_ram_bytes: u64,
) -> u64 {
    let mut h = FnvHasher64::new();
    let rows_len = details.rows.len();
    let start = visible_rows.start.min(rows_len);
    let end = visible_rows.end.min(rows_len).max(start);
    let cols_mask = column_layout.visible_cols_mask(visible_cols);

    h.write_bool(details.revision == 0);
    h.write_usize(rows_len);
    h.write_u8(details.view_mode as u8);
    h.write_u8(details.sort_column as u8);
    h.write_u8(details.sort_direction as u8);
    h.write_usize(selected_idx.unwrap_or(usize::MAX));
    if let Some(hk) = highlighted_key {
        h.write_u8(1);
        h.write_u32(hk.pid);
        h.write_u64(hk.create_time_100ns);
    } else {
        h.write_u8(0);
    }
    h.write_u64(total_ram_bytes);
    h.write_usize(start);
    h.write_usize(end);
    h.write_u64(cols_mask);

    let visible_specs = column_layout.visible_columns();
    for row in &details.rows[start..end] {
        h.write_u32(row.key.pid);
        h.write_u64(row.key.create_time_100ns);
        h.write_u16(row.depth);
        h.write_bool(row.has_children);
        h.write_bool(row.is_expanded);

        for (col_ix, spec) in visible_specs.iter().enumerate() {
            if col_ix < 64 && (cols_mask & (1u64 << col_ix)) == 0 {
                continue;
            }
            match spec.id {
                DetailsColumn::Name => {
                    h.write_str(&row.name);
                    h.write_u8(row.webview2_role.map_or(u8::MAX, |r| r as u8));
                    if let Some(icon) = row.icon.as_ref() {
                        h.write_usize(icon.rgba.as_ptr() as usize);
                        h.write_usize(icon.rgba.len());
                    } else {
                        h.write_u8(0);
                    }
                }
                DetailsColumn::Pid => {}
                DetailsColumn::Status => {
                    h.write_bool(row.status.contains(ProcessStatusFlags::SUSPENDED));
                    if let Some(w) = row.leak_warning.as_ref() {
                        h.write_u8(1);
                        h.write_u8(w.kind as u8);
                        h.write_u64(w.current_value);
                        h.write_u64(w.baseline_value);
                        h.write_str(&w.summary);
                    } else {
                        h.write_u8(0);
                    }
                }
                DetailsColumn::UserName => {
                    if let Some(u) = row.user_name.as_deref() {
                        h.write_u8(1);
                        h.write_str(u);
                    } else {
                        h.write_u8(0);
                    }
                }
                DetailsColumn::SessionId => {
                    h.write_u32(row.session_id);
                }
                DetailsColumn::Cpu => {
                    h.write_u16(row.cpu_permille);
                }
                DetailsColumn::CpuTime => {
                    h.write_u64(quantize_cpu_time_secs(row.cpu_time_100ns));
                }
                DetailsColumn::Memory => {
                    h.write_u64(quantize_bytes_display(row.private_working_set_bytes));
                    h.write_u8(
                        memory_heatmap_level(row.private_working_set_bytes, total_ram_bytes) as u8,
                    );
                }
                DetailsColumn::WorkingSet => {
                    h.write_u64(quantize_bytes_display(row.working_set_bytes));
                }
                DetailsColumn::WorkingSetDelta => {
                    h.write_u64(quantize_signed_bytes_display(row.working_set_delta_bytes));
                }
                DetailsColumn::CommitSize => {
                    h.write_u64(quantize_bytes_display(row.commit_bytes));
                }
                DetailsColumn::Priority => {
                    h.write_u8(row.priority as u8);
                }
                DetailsColumn::Handles => {
                    h.write_u32(row.handle_count);
                }
                DetailsColumn::Threads => {
                    h.write_u32(row.thread_count);
                }
                DetailsColumn::Platform => {
                    h.write_u8(row.arch as u8);
                }
                DetailsColumn::UacVirtualization => {
                    h.write_u8(row.uac_virtualization as u8);
                }
                DetailsColumn::Elevated => {
                    h.write_bool(row.status.contains(ProcessStatusFlags::ELEVATED));
                }
                DetailsColumn::GpuMemory => {
                    h.write_u64(quantize_bytes_display(row.gpu_dedicated_bytes));
                }
                DetailsColumn::Publisher => {
                    if let Some(p) = row.publisher.as_deref() {
                        h.write_u8(1);
                        h.write_str(p);
                    } else {
                        h.write_u8(0);
                    }
                }
                DetailsColumn::Description => {
                    if let Some(d) = row.description.as_deref() {
                        h.write_u8(1);
                        h.write_str(d);
                    } else {
                        h.write_u8(0);
                    }
                }
                DetailsColumn::ExePath => {
                    if let Some(p) = row.exe_path.as_deref() {
                        h.write_u8(1);
                        h.write_preview_str(p);
                    } else {
                        h.write_u8(0);
                    }
                }
                DetailsColumn::CommandLine => {
                    if let Some(c) = row.command_line.as_deref() {
                        h.write_u8(1);
                        h.write_preview_str(c);
                    } else {
                        h.write_u8(0);
                    }
                }
            }
        }
    }

    h.finish()
}

/// 2D-virtualized `gpui-kit` [`TableDelegate`] backing the live Details table.
struct DetailsTableDelegate {
    store: Entity<UiStore>,
    shell_handle: ShellHandle,
    details: Arc<DetailsTableSlice>,
    capabilities: CapabilityFlags,
    platform: PlatformProfile,
    total_ram_bytes: u64,
    last_rev: u64,
    selected_key: Option<ProcessKey>,
    reanchored_selection: Option<Option<usize>>,
    highlighted_key: Option<ProcessKey>,
    pending_scroll_key: Option<ProcessKey>,
    last_right_clicked_col: Option<usize>,
    order_freeze: OrderFreezeState,
    sort_paused_badge: Entity<SortPausedBadge>,
    menu_focus_subs: Vec<Subscription>,
    column_layout: TableColumnLayout<DetailsColumn>,
    icon_cache: IconRenderCache,
    semantic_colors: CachedSemanticColors,
    row_cache: RowTextCache,
    user_name_cache: HashMap<String, SharedString>,
    locale: DetailsLocaleCache,
    visible_rows: Range<usize>,
    visible_cols: Range<usize>,
    last_rows_len: usize,
    last_viewport_fingerprint: Option<u64>,
    scroll_coalescer: ScrollBurstCoalescer,
    user_initiated_pending: bool,
}

impl DetailsTableDelegate {
    fn new(
        store: Entity<UiStore>,
        highlighted_key: Option<ProcessKey>,
        shell_handle: ShellHandle,
        cx: &mut App,
    ) -> Self {
        let (details, capabilities, platform, total_ram_bytes, last_rev, saved_cols) = {
            let store_ref = store.read(cx);
            let snap = store_ref.snapshot();
            (
                Arc::clone(&snap.details),
                snap.capabilities,
                snap.platform,
                snap.performance.latest.mem_total_bytes,
                snap.revisions.details,
                store_ref.details_columns().to_vec(),
            )
        };

        let mut column_layout = TableColumnLayout::new(DetailsColumn::default_specs(capabilities));
        if !saved_cols.is_empty() {
            column_layout.apply_saved(&saved_cols);
        }
        let initial_rows_len = details.rows.len();
        let locale = DetailsLocaleCache::build(platform);
        let sort_paused_tip = locale.sort_paused_tooltip.clone();
        let sort_paused_badge =
            cx.new(|_| SortPausedBadge::new("details-sort-paused", sort_paused_tip));

        Self {
            store: store.clone(),
            shell_handle,
            details,
            capabilities,
            platform,
            total_ram_bytes,
            last_rev,
            selected_key: highlighted_key,
            reanchored_selection: None,
            highlighted_key,
            pending_scroll_key: highlighted_key,
            last_right_clicked_col: None,
            order_freeze: OrderFreezeState::default(),
            sort_paused_badge,
            menu_focus_subs: Vec::new(),
            column_layout,
            icon_cache: IconRenderCache::new(),
            semantic_colors: CachedSemanticColors::new(),
            row_cache: RowTextCache::new(),
            user_name_cache: HashMap::new(),
            locale,
            visible_rows: 0..0,
            visible_cols: 0..0,
            last_rows_len: initial_rows_len,
            last_viewport_fingerprint: None,
            scroll_coalescer: ScrollBurstCoalescer::new(),
            user_initiated_pending: false,
        }
    }

    #[inline]
    fn mark_user_initiated(&mut self) {
        self.user_initiated_pending = true;
    }

    #[inline]
    fn take_user_initiated(&mut self) -> bool {
        std::mem::take(&mut self.user_initiated_pending)
    }

    #[inline]
    fn invalidate_viewport_fingerprint(&mut self) {
        self.last_viewport_fingerprint = None;
    }

    #[inline]
    fn compute_viewport_fingerprint(&self, selected_idx: Option<usize>) -> u64 {
        compute_details_viewport_fingerprint(
            &self.details,
            &self.column_layout,
            &self.visible_rows,
            &self.visible_cols,
            selected_idx,
            self.highlighted_key,
            self.total_ram_bytes,
        )
    }

    fn apply_order_freeze_transition(
        &mut self,
        transition: Option<bool>,
        cx: &mut Context<TableState<Self>>,
    ) {
        if let Some(frozen) = transition {
            let _ = self
                .store
                .read(cx)
                .dispatch_command(EngineCommand::SetDetailsOrderFreeze(frozen));
            self.sort_paused_badge.update(cx, |badge, cx| {
                badge.set_frozen(frozen, cx);
            });
        }
    }

    fn set_table_hovered(&mut self, hovered: bool, cx: &mut Context<TableState<Self>>) {
        let transition = self.order_freeze.set_table_hovered(hovered);
        self.apply_order_freeze_transition(transition, cx);
    }

    fn set_header_hovered(&mut self, hovered: bool, cx: &mut Context<TableState<Self>>) {
        let transition = self.order_freeze.set_header_hovered(hovered);
        self.apply_order_freeze_transition(transition, cx);
    }

    fn set_context_menu_open(&mut self, open: bool, cx: &mut Context<TableState<Self>>) {
        let transition = self.order_freeze.set_context_menu_open(open);
        self.apply_order_freeze_transition(transition, cx);
    }

    fn format_cell_text(&self, row: &DetailsRowProjection, col: DetailsColumn) -> String {
        match col {
            DetailsColumn::Name => row.name.to_string(),
            DetailsColumn::Pid => row.key.pid.to_string(),
            DetailsColumn::Status => {
                if row.status.contains(ProcessStatusFlags::SUSPENDED) {
                    self.locale.suspended.to_string()
                } else {
                    self.locale.running.to_string()
                }
            }
            DetailsColumn::UserName => display_user_name(row.user_name.as_deref()).to_owned(),
            DetailsColumn::SessionId => row.session_id.to_string(),
            DetailsColumn::Cpu => format_permille(row.cpu_permille),
            DetailsColumn::CpuTime => format_cpu_time(row.cpu_time_100ns),
            DetailsColumn::Memory => format_bytes(row.private_working_set_bytes),
            DetailsColumn::WorkingSet => format_bytes(row.working_set_bytes),
            DetailsColumn::WorkingSetDelta => format_signed_bytes(row.working_set_delta_bytes),
            DetailsColumn::CommitSize => format_bytes(row.commit_bytes),
            DetailsColumn::Priority => self.locale.priority_label(row.priority).to_string(),
            DetailsColumn::Handles => row.handle_count.to_string(),
            DetailsColumn::Threads => row.thread_count.to_string(),
            DetailsColumn::Platform => self.locale.arch_label(row.arch).to_string(),
            DetailsColumn::UacVirtualization => {
                self.locale.uac_label(row.uac_virtualization).to_string()
            }
            DetailsColumn::Elevated => {
                if row.status.contains(ProcessStatusFlags::ELEVATED) {
                    self.locale.elevated_yes.to_string()
                } else {
                    self.locale.elevated_no.to_string()
                }
            }
            DetailsColumn::GpuMemory => format_bytes(row.gpu_dedicated_bytes),
            DetailsColumn::Publisher => row
                .publisher
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("")
                .to_owned(),
            DetailsColumn::Description => row
                .description
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("")
                .to_owned(),
            DetailsColumn::ExePath => row
                .exe_path
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("")
                .to_owned(),
            DetailsColumn::CommandLine => row
                .command_line
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("")
                .to_owned(),
        }
    }

    fn format_row_summary(&self, row: &DetailsRowProjection) -> String {
        let status_text = self.format_cell_text(row, DetailsColumn::Status);
        let priority_text = self.locale.priority_label(row.priority);
        let arch_text = self.locale.arch_label(row.arch);
        format_details_row_summary(
            row,
            &status_text,
            priority_text.as_ref(),
            arch_text.as_ref(),
        )
    }

    fn selected_row(&self) -> Option<&DetailsRowProjection> {
        let sel = self.selected_key?;
        find_row_index_by_key(&self.details.rows, sel).and_then(|idx| self.details.rows.get(idx))
    }

    fn selected_copy_short(&self) -> Option<String> {
        self.selected_row().map(format_details_copy_short)
    }

    fn selected_copy_summary(&self) -> Option<String> {
        self.selected_row().map(|row| self.format_row_summary(row))
    }

    fn export_visible_csv_payload(&self) -> (Vec<String>, Vec<Vec<String>>) {
        let visible_cols = self.column_layout.visible_columns();
        let headers: Vec<String> = visible_cols
            .iter()
            .map(|spec| spec.id.label(self.platform))
            .collect();
        let rows: Vec<Vec<String>> = self
            .details
            .rows
            .iter()
            .map(|row| {
                visible_cols
                    .iter()
                    .map(|spec| self.format_cell_text(row, spec.id))
                    .collect()
            })
            .collect();
        (headers, rows)
    }

    fn persist_column_layout(&self, cx: &mut App) {
        let snapshot = self.column_layout.snapshot();
        UiCommands::set_details_columns(&self.store, snapshot, cx);
    }

    fn trigger_sort_col(&mut self, col_ix: usize, cx: &mut App) {
        let Some(spec) = self.column_layout.visible_column(col_ix) else {
            return;
        };
        let Some(target) = spec.id.sort_column() else {
            return;
        };
        self.mark_user_initiated();
        let next_dir = next_sort_direction(
            self.details.sort_column,
            self.details.sort_direction,
            target,
        );
        UiCommands::set_details_sort(&self.store, target, next_dir, cx);
    }

    fn hide_column(&mut self, col: DetailsColumn, cx: &mut App) -> bool {
        if self.column_layout.hide_column(col) {
            self.invalidate_viewport_fingerprint();
            self.persist_column_layout(cx);
            true
        } else {
            false
        }
    }

    fn reset_columns(&mut self, cx: &mut App) {
        self.column_layout
            .reset(DetailsColumn::default_specs(self.capabilities));
        self.invalidate_viewport_fingerprint();
        UiCommands::set_details_columns(&self.store, Vec::new(), cx);
    }

    fn open_select_columns_modal(
        &self,
        table_weak: WeakEntity<TableState<Self>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let items = DetailsColumn::selectable_order(self.capabilities)
            .into_iter()
            .map(|col| ColumnToggleItem {
                slug: col.slug(),
                label: col.label(self.platform),
                visible: self.column_layout.is_visible(col),
            })
            .collect();
        open_select_columns_dialog(
            items,
            Rc::new(move |selected_slugs, cx| {
                let _ = table_weak.update(cx, |table, cx| {
                    let changed = table
                        .delegate_mut()
                        .column_layout
                        .apply_visible_slugs(&selected_slugs);
                    if changed {
                        table.delegate_mut().invalidate_viewport_fingerprint();
                        table.delegate().persist_column_layout(cx);
                        table.refresh(cx);
                    }
                });
            }),
            window,
            cx,
        );
    }

    fn build_header_context_menu(
        menu: PopupMenu,
        clicked_col: Option<DetailsColumn>,
        table_weak: WeakEntity<TableState<Self>>,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let hide_action: Option<HeaderMenuAction> = clicked_col
            .filter(|&col| col != DetailsColumn::Name)
            .map(|col| {
                let weak = table_weak.clone();
                Rc::new(move |_window: &mut Window, cx: &mut App| {
                    let _ = weak.update(cx, |table, cx| {
                        if table.delegate_mut().hide_column(col, cx) {
                            table.refresh(cx);
                        }
                    });
                }) as HeaderMenuAction
            });

        let weak_select = table_weak.clone();
        let on_select_columns: HeaderMenuAction =
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let _ = weak_select.update(cx, |table, cx| {
                    table
                        .delegate()
                        .open_select_columns_modal(weak_select.clone(), window, cx);
                });
            });

        let weak_reset = table_weak;
        let on_reset_defaults: HeaderMenuAction =
            Rc::new(move |_window: &mut Window, cx: &mut App| {
                let _ = weak_reset.update(cx, |table, cx| {
                    table.delegate_mut().reset_columns(cx);
                    table.refresh(cx);
                });
            });

        build_column_header_menu(
            menu,
            hide_action,
            on_select_columns,
            |sub, _window, _cx| sub,
            on_reset_defaults,
            window,
            cx,
        )
    }

    fn selected_target(&self) -> Option<ProcessActionTarget> {
        let sel = self.selected_key?;
        find_row_index_by_key(&self.details.rows, sel)
            .and_then(|idx| self.details.rows.get(idx))
            .map(ProcessActionTarget::from_row)
    }

    fn has_valid_selection(&self) -> bool {
        self.selected_key
            .and_then(|sel| find_row_index_by_key(&self.details.rows, sel))
            .is_some()
    }
}

impl TableDelegate for DetailsTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.column_layout.visible_count()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.details.rows.len()
    }

    fn loading(&self, _cx: &App) -> bool {
        self.details.revision == 0
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        let Some(spec) = self.column_layout.visible_column(col_ix) else {
            return Column::default();
        };
        let col = spec.id;
        let mut column = Column::new(col.slug(), self.locale.column_label(col))
            .width(px(spec.width))
            .min_width(px(spec.min_width))
            .max_width(px(spec.max_width))
            .selectable(false);

        if spec.locked {
            column = column.fixed_left().movable(false);
        }
        if col.right_aligned() {
            column = column.text_right();
        }
        if col.is_heatmap() {
            column = column.p_0();
        }
        if let Some(sort_col) = col.sort_column() {
            if sort_col == self.details.sort_column {
                column = column.sort(to_column_sort(self.details.sort_direction));
            } else {
                column = column.sortable();
            }
        }
        column
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        _sort: ColumnSort,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        self.trigger_sort_col(col_ix, cx);
    }

    fn move_column(
        &mut self,
        col_ix: usize,
        to_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        if self.column_layout.move_visible_column(col_ix, to_ix) {
            self.invalidate_viewport_fingerprint();
            self.persist_column_layout(cx);
        }
    }

    fn visible_rows_changed(
        &mut self,
        visible_range: Range<usize>,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) {
        let rows_len = self.details.rows.len();
        if !self.visible_rows.is_empty()
            && self.visible_rows != visible_range
            && rows_len == self.last_rows_len
        {
            self.scroll_coalescer.on_scroll(Instant::now());
        }
        self.visible_rows = visible_range;
        self.last_rows_len = rows_len;
        self.last_viewport_fingerprint = None;
    }

    fn visible_columns_changed(
        &mut self,
        visible_range: Range<usize>,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) {
        if !self.visible_cols.is_empty() && self.visible_cols != visible_range {
            self.scroll_coalescer.on_scroll(Instant::now());
        }
        self.visible_cols = visible_range;
        self.last_viewport_fingerprint = None;
    }

    fn render_header(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        let table_weak = cx.entity().downgrade();
        let bg = cx.theme().secondary.opacity(0.45);
        div()
            .id("details-column-header")
            .relative()
            .bg(bg)
            .text_xs()
            .font_medium()
            .on_hover(cx.listener(|table, hovered: &bool, _, cx| {
                table.delegate_mut().set_header_hovered(*hovered, cx);
            }))
            .child(
                div()
                    .id("details-header-bg-menu")
                    .absolute()
                    .inset_0()
                    .context_menu(move |menu, window, cx| {
                        Self::build_header_context_menu(menu, None, table_weak.clone(), window, cx)
                    }),
            )
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(spec) = self.column_layout.visible_column(col_ix) else {
            return Empty.into_any_element();
        };
        let col = spec.id;
        let is_active_sort = col.sort_column() == Some(self.details.sort_column);
        if is_active_sort {
            self.sort_paused_badge.update(cx, |badge, _| {
                badge.set_col_ix(col_ix);
            });
        }
        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let col_label = self.locale.column_label(col);
        let sort_paused_badge = self.sort_paused_badge.clone();
        let table_weak = cx.entity().downgrade();

        h_flex()
            .id(("details-th", col_ix))
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .gap_1()
            .when(col.is_heatmap(), |d| d.px_2())
            .when(col.right_aligned(), |d| d.justify_end())
            .text_xs()
            .text_color(if is_active_sort { fg } else { muted_fg })
            .when(is_active_sort, |d| d.font_semibold())
            .cursor_pointer()
            .on_click(cx.listener(move |table, _, _, cx| {
                table.delegate_mut().trigger_sort_col(col_ix, cx);
            }))
            .when(is_active_sort && col.right_aligned(), |d| {
                d.child(sort_paused_badge.clone())
            })
            .child(div().min_w_0().truncate().child(col_label))
            .when(is_active_sort && !col.right_aligned(), |d| {
                d.child(sort_paused_badge)
            })
            .context_menu(move |menu, window, cx| {
                Self::build_header_context_menu(menu, Some(col), table_weak.clone(), window, cx)
            })
            .into_any_element()
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        let Some(row) = self.details.rows.get(row_ix) else {
            return div().id(("details-empty", row_ix));
        };
        let is_highlighted = self.highlighted_key == Some(row.key);
        let primary = cx.theme().primary;

        div()
            .id(("details-row", process_key_element_id(row.key)))
            .text_xs()
            .when(is_highlighted, |s| {
                s.bg(primary.opacity(0.24))
                    .border_l_2()
                    .border_color(primary)
            })
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(row) = self.details.rows.get(row_ix) else {
            return Empty.into_any_element();
        };
        let Some(spec) = self.column_layout.visible_column(col_ix) else {
            return Empty.into_any_element();
        };

        let theme = cx.theme();
        let muted_fg = theme.muted_foreground;
        let muted_bg = theme.muted;
        let hover_bg = theme.list_hover;
        let radius = theme.radius;
        let semantic = self.semantic_colors.get_copy(theme);
        let key = row.key;
        let row_id = process_key_element_id(key);

        match spec.id {
            DetailsColumn::Name => {
                let is_tree_mode = self.details.view_mode == DetailsViewMode::Tree;
                let indent = if is_tree_mode {
                    f32::from(row.depth.min(10)) * 16.0
                } else {
                    0.0
                };
                let has_children = is_tree_mode && row.has_children;
                let is_expanded = row.is_expanded;
                let icon_image = row
                    .icon
                    .as_ref()
                    .and_then(|icon| self.icon_cache.get_or_insert(icon));
                let toggle_store = self.store.clone();
                let name = SharedString::from(Arc::clone(&row.name));
                let wv2_label = row
                    .webview2_role
                    .map(|role| self.locale.webview2_label(role));

                let name_el = if let Some(role_label) = wv2_label {
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .items_center()
                        .gap_1p5()
                        .overflow_hidden()
                        .child(div().min_w_0().truncate().text_xs().child(name))
                        .child(webview2_role_badge(role_label, muted_bg, muted_fg, radius))
                        .into_any_element()
                } else {
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .child(name)
                        .into_any_element()
                };

                h_flex()
                    .size_full()
                    .items_center()
                    .gap_1p5()
                    .pl(px(indent))
                    .overflow_hidden()
                    .when(is_tree_mode, |el| {
                        el.child(if has_children {
                            expand_chevron_button(
                                ("details-expand", row_id),
                                is_expanded,
                                hover_bg,
                                muted_fg,
                                cx.listener(move |table, _, _, cx| {
                                    table.delegate_mut().mark_user_initiated();
                                    UiCommands::toggle_details_expanded(&toggle_store, key, cx);
                                }),
                            )
                            .into_any_element()
                        } else {
                            div().w(px(20.)).flex_shrink_0().into_any_element()
                        })
                    })
                    .child(if let Some(image) = icon_image {
                        img(image).size(px(16.)).flex_shrink_0().into_any_element()
                    } else {
                        Icon::new(IconName::File)
                            .size(px(15.))
                            .flex_shrink_0()
                            .text_color(muted_fg)
                            .into_any_element()
                    })
                    .child(name_el)
                    .into_any_element()
            }
            DetailsColumn::Pid => h_flex()
                .size_full()
                .items_center()
                .justify_end()
                .text_xs()
                .font_family(theme.mono_font_family.clone())
                .text_color(muted_fg)
                .child(self.row_cache.pid_text(key))
                .into_any_element(),
            DetailsColumn::Status => {
                let status_text = if row.status.contains(ProcessStatusFlags::SUSPENDED) {
                    self.locale.suspended.clone()
                } else {
                    self.locale.running.clone()
                };
                h_flex()
                    .size_full()
                    .items_center()
                    .gap_1()
                    .overflow_hidden()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(muted_fg)
                            .child(status_text),
                    )
                    .when_some(row.leak_warning.as_ref(), |el, warning| {
                        el.child(leak_warning_badge(
                            ("details-leak", row_id),
                            warning,
                            semantic.leak_warning,
                        ))
                    })
                    .into_any_element()
            }
            DetailsColumn::UserName => {
                let user_text = cached_user_name(&mut self.user_name_cache, row.user_name.as_ref());
                h_flex()
                    .size_full()
                    .items_center()
                    .overflow_hidden()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(muted_fg)
                            .child(user_text),
                    )
                    .into_any_element()
            }
            DetailsColumn::SessionId => h_flex()
                .size_full()
                .items_center()
                .justify_end()
                .text_xs()
                .font_family(theme.mono_font_family.clone())
                .text_color(muted_fg)
                .child(self.row_cache.u32_slot(key, 0, row.session_id))
                .into_any_element(),
            DetailsColumn::Cpu => heatmap_cell_metric(
                format_permille_metric(row.cpu_permille),
                HeatmapLevel::from_permille(row.cpu_permille),
                &semantic,
                theme.mono_font_family.clone(),
            )
            .into_any_element(),
            DetailsColumn::CpuTime => h_flex()
                .size_full()
                .items_center()
                .justify_end()
                .text_xs()
                .font_family(theme.mono_font_family.clone())
                .text_color(muted_fg)
                .child(self.row_cache.cpu_time_text(key, row.cpu_time_100ns))
                .into_any_element(),
            DetailsColumn::Memory => heatmap_cell_metric(
                self.row_cache
                    .bytes_metric_slot(key, 0, row.private_working_set_bytes),
                memory_heatmap_level(row.private_working_set_bytes, self.total_ram_bytes),
                &semantic,
                theme.mono_font_family.clone(),
            )
            .into_any_element(),
            DetailsColumn::WorkingSet => render_metric_cell(
                self.row_cache
                    .bytes_metric_slot(key, 1, row.working_set_bytes),
                theme.mono_font_family.clone(),
                muted_fg,
            ),
            DetailsColumn::WorkingSetDelta => render_metric_cell(
                self.row_cache
                    .signed_bytes_metric_slot(key, 0, row.working_set_delta_bytes),
                theme.mono_font_family.clone(),
                muted_fg,
            ),
            DetailsColumn::CommitSize => render_metric_cell(
                self.row_cache.bytes_metric_slot(key, 2, row.commit_bytes),
                theme.mono_font_family.clone(),
                muted_fg,
            ),
            DetailsColumn::Priority => h_flex()
                .size_full()
                .items_center()
                .overflow_hidden()
                .text_xs()
                .text_color(muted_fg)
                .child(self.locale.priority_label(row.priority))
                .into_any_element(),
            DetailsColumn::Handles => h_flex()
                .size_full()
                .items_center()
                .justify_end()
                .text_xs()
                .font_family(theme.mono_font_family.clone())
                .text_color(muted_fg)
                .child(self.row_cache.u32_slot(key, 1, row.handle_count))
                .into_any_element(),
            DetailsColumn::Threads => h_flex()
                .size_full()
                .items_center()
                .justify_end()
                .text_xs()
                .font_family(theme.mono_font_family.clone())
                .text_color(muted_fg)
                .child(self.row_cache.u32_slot(key, 2, row.thread_count))
                .into_any_element(),
            DetailsColumn::Platform => h_flex()
                .size_full()
                .items_center()
                .overflow_hidden()
                .text_xs()
                .text_color(muted_fg)
                .child(self.locale.arch_label(row.arch))
                .into_any_element(),
            DetailsColumn::UacVirtualization => h_flex()
                .size_full()
                .items_center()
                .overflow_hidden()
                .text_xs()
                .text_color(muted_fg)
                .child(self.locale.uac_label(row.uac_virtualization))
                .into_any_element(),
            DetailsColumn::Elevated => {
                let text = if row.status.contains(ProcessStatusFlags::ELEVATED) {
                    self.locale.elevated_yes.clone()
                } else {
                    self.locale.elevated_no.clone()
                };
                h_flex()
                    .size_full()
                    .items_center()
                    .overflow_hidden()
                    .text_xs()
                    .text_color(muted_fg)
                    .child(text)
                    .into_any_element()
            }
            DetailsColumn::GpuMemory => render_metric_cell(
                self.row_cache
                    .bytes_metric_slot(key, 3, row.gpu_dedicated_bytes),
                theme.mono_font_family.clone(),
                muted_fg,
            ),
            DetailsColumn::Publisher => {
                let text = row
                    .publisher
                    .as_ref()
                    .map(|p| SharedString::from(Arc::clone(p)))
                    .unwrap_or_else(|| SharedString::new_static("—"));
                h_flex()
                    .size_full()
                    .items_center()
                    .overflow_hidden()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(muted_fg)
                            .child(text),
                    )
                    .into_any_element()
            }
            DetailsColumn::Description => {
                let text = row
                    .description
                    .as_ref()
                    .filter(|s| !s.is_empty())
                    .map(|s| SharedString::from(Arc::clone(s)))
                    .unwrap_or_else(|| SharedString::new_static("—"));
                h_flex()
                    .size_full()
                    .items_center()
                    .overflow_hidden()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(muted_fg)
                            .child(text),
                    )
                    .into_any_element()
            }
            DetailsColumn::ExePath => {
                let mono_font = theme.mono_font_family.clone();
                if let Some(path_arc) = row.exe_path.as_ref().filter(|s| !s.is_empty()) {
                    let preview =
                        self.row_cache
                            .preview_slot(key, 0, path_arc, DEFAULT_CELL_PREVIEW_CHARS);
                    if should_attach_cell_tooltip(path_arc, spec.width) {
                        let tip = SharedString::from(Arc::clone(path_arc));
                        h_flex()
                            .id(("details-exe", row_id))
                            .size_full()
                            .items_center()
                            .overflow_hidden()
                            .text_xs()
                            .font_family(mono_font)
                            .text_color(muted_fg)
                            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                            .child(div().min_w_0().truncate().child(preview))
                            .into_any_element()
                    } else {
                        h_flex()
                            .size_full()
                            .items_center()
                            .overflow_hidden()
                            .text_xs()
                            .font_family(mono_font)
                            .text_color(muted_fg)
                            .child(div().min_w_0().truncate().child(preview))
                            .into_any_element()
                    }
                } else {
                    h_flex()
                        .size_full()
                        .items_center()
                        .overflow_hidden()
                        .text_xs()
                        .font_family(mono_font)
                        .text_color(muted_fg)
                        .child(SharedString::new_static("—"))
                        .into_any_element()
                }
            }
            DetailsColumn::CommandLine => {
                let mono_font = theme.mono_font_family.clone();
                if let Some(cmd_arc) = row.command_line.as_ref().filter(|s| !s.is_empty()) {
                    let preview =
                        self.row_cache
                            .preview_slot(key, 1, cmd_arc, DEFAULT_CELL_PREVIEW_CHARS);
                    if should_attach_cell_tooltip(cmd_arc, spec.width) {
                        let tip = SharedString::from(Arc::clone(cmd_arc));
                        h_flex()
                            .id(("details-cmd", row_id))
                            .size_full()
                            .items_center()
                            .overflow_hidden()
                            .text_xs()
                            .font_family(mono_font)
                            .text_color(muted_fg)
                            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                            .child(div().min_w_0().truncate().child(preview))
                            .into_any_element()
                    } else {
                        h_flex()
                            .size_full()
                            .items_center()
                            .overflow_hidden()
                            .text_xs()
                            .font_family(mono_font)
                            .text_color(muted_fg)
                            .child(div().min_w_0().truncate().child(preview))
                            .into_any_element()
                    }
                } else {
                    h_flex()
                        .size_full()
                        .items_center()
                        .overflow_hidden()
                        .text_xs()
                        .font_family(mono_font)
                        .text_color(muted_fg)
                        .child(SharedString::new_static("—"))
                        .into_any_element()
                }
            }
        }
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(row) = self.details.rows.get(row_ix).cloned() else {
            return menu;
        };
        let target = ProcessActionTarget::from_row(&row);
        let clicked_column = self
            .last_right_clicked_col
            .and_then(|col_ix| self.column_layout.visible_column(col_ix))
            .map(|spec| {
                (
                    spec.id.label(self.platform),
                    self.format_cell_text(&row, spec.id),
                )
            });
        let copy_fields = CopyRowFields {
            name: row.name.to_string(),
            pid: row.key.pid,
            exe_path: row.exe_path.as_deref().map(str::to_owned),
            command_line: row.command_line.as_deref().map(str::to_owned),
            row_summary: self.format_row_summary(&row),
            clicked_column,
        };

        self.set_context_menu_open(true, cx);

        let root_focus = menu.focus_handle(cx);
        let (built_menu, submenu_handles) = build_row_context_menu(
            menu,
            &self.store,
            &self.shell_handle,
            &target,
            copy_fields,
            self.capabilities,
            window,
            cx,
        );

        let mut tracked_handles = Vec::with_capacity(1 + submenu_handles.len());
        tracked_handles.push(root_focus);
        tracked_handles.extend(submenu_handles);
        let tracked = Rc::new(tracked_handles);

        self.menu_focus_subs.clear();
        for handle in tracked.iter() {
            let all_handles = Rc::clone(&tracked);
            let sub = cx.on_focus_out(handle, window, move |table, _, window, cx| {
                let still_in_menu = all_handles
                    .iter()
                    .any(|h| h.is_focused(window) || h.contains_focused(window, cx));
                if !still_in_menu {
                    table.delegate_mut().set_context_menu_open(false, cx);
                }
            });
            self.menu_focus_subs.push(sub);
        }

        built_menu
    }

    fn row_label(&self, row_ix: usize, _cx: &App) -> Option<SharedString> {
        let row = self.details.rows.get(row_ix)?;
        Some(format!("{} (PID {})", row.name, row.key.pid).into())
    }

    fn render_loading(
        &mut self,
        _size: Size,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let muted_fg = cx.theme().muted_foreground;
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .p_8()
            .child(
                div()
                    .text_sm()
                    .text_color(muted_fg)
                    .child(self.locale.loading.clone()),
            )
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let muted_fg = cx.theme().muted_foreground;
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .p_8()
            .child(
                Icon::new(IconName::Search)
                    .size(px(24.))
                    .text_color(muted_fg),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(muted_fg)
                    .child(self.locale.empty_search.clone()),
            )
    }
}

struct PendingDetailsSnapshot {
    rev: u64,
    details: Arc<DetailsTableSlice>,
    capabilities: CapabilityFlags,
    platform: PlatformProfile,
    total_ram_bytes: u64,
}

/// Live full-field Details table view holding an RAII `MonitorDomain::Details` lease.
///
/// Splits the static toolbar/banner from the 2D-virtualized `DataTable` entity so
/// live snapshot updates refresh only `self.table` without re-rendering the outer shell.
pub(crate) struct DetailsView {
    store: Entity<UiStore>,
    coordinator: Arc<EngineCoordinator<NativeBackend>>,
    _details_lease: ScopedMonitorLease,
    table: Entity<TableState<DetailsTableDelegate>>,
    table_pane: Entity<DataTablePane<DetailsTableDelegate>>,
    table_left_x: Rc<Cell<Pixels>>,
    view_mode: DetailsViewMode,
    highlighted_key: Option<ProcessKey>,
    has_selection: bool,
    last_locale: LanguagePreference,
    type_to_search: TypeToSearchState,
    pending_snapshot: Option<PendingDetailsSnapshot>,
    scroll_flush_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Drop for DetailsView {
    fn drop(&mut self) {
        let _ = self
            .coordinator
            .send_command(EngineCommand::SetDetailsOrderFreeze(false));
    }
}

impl Focusable for DetailsView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.table.focus_handle(cx)
    }
}

impl EntityInputHandler for DetailsView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let query = self.store.read(cx).search_query().to_owned();
        self.type_to_search.sync_committed(&query);
        let (adj, text) = self.type_to_search.text_for_utf16_range(range);
        adjusted_range.replace(adj);
        Some(text)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let query = self.store.read(cx).search_query().to_owned();
        self.type_to_search.sync_committed(&query);
        Some(UTF16Selection {
            range: self.type_to_search.selected_utf16_range(),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.type_to_search.marked_utf16_range()
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.type_to_search.is_composing() {
            self.type_to_search.unmark_text();
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_composing = self.type_to_search.is_composing();
        let current = self.store.read(cx).search_query().to_owned();
        if let Some(next_query) = self.type_to_search.replace_text(range, text, &current) {
            UiCommands::set_search_query(&self.store, next_query, cx);
        }
        if was_composing {
            cx.notify();
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.store.read(cx).search_query().to_owned();
        self.type_to_search
            .replace_and_mark(range, new_text, &current);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(Bounds::new(
            element_bounds.origin,
            gpui_kit::size(px(1.0), px(20.0)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.type_to_search.utf16_len())
    }

    fn text_length_utf16(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Option<usize> {
        let query = self.store.read(cx).search_query().to_owned();
        self.type_to_search.sync_committed(&query);
        Some(self.type_to_search.utf16_len())
    }

    fn accepts_text_input(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.table.focus_handle(cx).is_focused(window)
    }
}

impl DetailsView {
    /// Creates a new [`DetailsView`], acquiring `MonitorDomain::Details` immediately.
    pub(crate) fn new(
        store: Entity<UiStore>,
        highlighted_key: Option<ProcessKey>,
        shell_handle: ShellHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let lease = store.read(cx).acquire_lease(MonitorDomain::Details);
        let (coordinator, view_mode, last_locale, initial_query) = {
            let s = store.read(cx);
            (
                Arc::clone(s.coordinator()),
                s.snapshot().details.view_mode,
                s.settings().language,
                s.search_query().to_owned(),
            )
        };

        let table = cx.new(|cx| {
            let delegate =
                DetailsTableDelegate::new(store.clone(), highlighted_key, shell_handle, cx);
            let mut state = TableState::new(delegate, window, cx)
                .row_selectable(true)
                .col_selectable(false)
                .col_resizable(true)
                .col_movable(true)
                .sortable(true);
            Self::try_scroll_to_pending_in_table(&mut state, cx);
            state
        });
        let table_pane = cx.new(|_| {
            DataTablePane::new(
                table.clone(),
                Size::Small,
                px(DETAILS_HEADER_HEIGHT),
                false,
                i18n::tr("details.title"),
            )
        });
        let has_selection = table.read(cx).delegate().has_valid_selection();

        let mut subscriptions = Vec::with_capacity(2);

        // Subscribe to UiStore: route live ticks directly to `self.table`.
        let table_for_store = table.clone();
        subscriptions.push(
            cx.subscribe(&store, move |this, store, event, cx| match event {
                UiStoreEvent::SnapshotUpdated(revs) => {
                    let latest_rev = this
                        .pending_snapshot
                        .as_ref()
                        .map_or_else(|| table_for_store.read(cx).delegate().last_rev, |p| p.rev);
                    if revs.details == latest_rev {
                        return;
                    }
                    let (next_details, next_caps, next_platform, next_ram) = {
                        let snap = store.read(cx).snapshot();
                        (
                            Arc::clone(&snap.details),
                            snap.capabilities,
                            snap.platform,
                            snap.performance.latest.mem_total_bytes,
                        )
                    };

                    let should_defer = table_for_store.update(cx, |table, _| {
                        let d = table.delegate_mut();
                        let is_user_initiated = d.last_rev == 0
                            || d.details.revision == 0
                            || d.pending_scroll_key.is_some()
                            || d.take_user_initiated()
                            || d.details.sort_column != next_details.sort_column
                            || d.details.sort_direction != next_details.sort_direction
                            || d.details.view_mode != next_details.view_mode
                            || d.details.rows.is_empty() != next_details.rows.is_empty();
                        d.scroll_coalescer
                            .should_defer_snapshot(Instant::now(), is_user_initiated)
                    });

                    if should_defer {
                        this.pending_snapshot = Some(PendingDetailsSnapshot {
                            rev: revs.details,
                            details: next_details,
                            capabilities: next_caps,
                            platform: next_platform,
                            total_ram_bytes: next_ram,
                        });
                        this.ensure_scroll_flush_scheduled(cx);
                    } else {
                        this.pending_snapshot = None;
                        this.scroll_flush_task = None;
                        this.commit_snapshot(
                            revs.details,
                            next_details,
                            next_caps,
                            next_platform,
                            next_ram,
                            cx,
                        );
                    }
                }
                UiStoreEvent::SettingsChanged => {
                    let next_locale = store.read(cx).settings().language;
                    if this.last_locale != next_locale {
                        this.last_locale = next_locale;
                        table_for_store.update(cx, |table, cx| {
                            let platform = table.delegate().platform;
                            let (badge, next_tip) = {
                                let d = table.delegate_mut();
                                d.locale = DetailsLocaleCache::build(platform);
                                d.invalidate_viewport_fingerprint();
                                (
                                    d.sort_paused_badge.clone(),
                                    d.locale.sort_paused_tooltip.clone(),
                                )
                            };
                            badge.update(cx, |b, cx| b.set_tooltip(next_tip, cx));
                            table.refresh(cx);
                            cx.notify();
                        });
                        this.table_pane.update(cx, |pane, cx| {
                            pane.set_accessibility_label(i18n::tr("details.title"), cx);
                        });
                        cx.notify();
                    }
                }
                UiStoreEvent::SearchQueryChanged => {
                    let query = store.read(cx).search_query().to_owned();
                    this.type_to_search.sync_committed(&query);
                    table_for_store.update(cx, |table, _| {
                        table.delegate_mut().mark_user_initiated();
                    });
                }
                UiStoreEvent::ElevationRequired { .. } | UiStoreEvent::CommandFailed { .. } => {}
            }),
        );

        // Subscribe to DataTable events: ProcessKey selection & column width persistence.
        subscriptions.push(cx.subscribe(
            &table,
            |this, table, event: &TableEvent, cx| match event {
                TableEvent::SelectRow(row_ix) => {
                    let next_has_sel = table.update(cx, |table, _| {
                        let d = table.delegate_mut();
                        if d.reanchored_selection == Some(Some(*row_ix)) {
                            d.reanchored_selection = None;
                            return d.selected_key.is_some();
                        }
                        d.reanchored_selection = None;
                        d.selected_key = d.details.rows.get(*row_ix).map(|r| r.key);
                        d.invalidate_viewport_fingerprint();
                        d.selected_key.is_some()
                    });
                    if this.has_selection != next_has_sel {
                        this.has_selection = next_has_sel;
                        cx.notify();
                    }
                }
                TableEvent::RightClickedRow(Some(row_ix)) => {
                    let next_has_sel = table.update(cx, |table, cx| {
                        let key_opt = table.delegate().details.rows.get(*row_ix).map(|r| r.key);
                        {
                            let d = table.delegate_mut();
                            d.selected_key = key_opt;
                            d.reanchored_selection = None;
                            d.invalidate_viewport_fingerprint();
                        }
                        if key_opt.is_some() && table.selected_row() != Some(*row_ix) {
                            table.set_selected_row(*row_ix, cx);
                        }
                        key_opt.is_some()
                    });
                    if this.has_selection != next_has_sel {
                        this.has_selection = next_has_sel;
                        cx.notify();
                    }
                }
                TableEvent::ClearSelection => {
                    let was_reanchor = table.update(cx, |table, _| {
                        let d = table.delegate_mut();
                        if d.reanchored_selection == Some(None) {
                            d.reanchored_selection = None;
                            return true;
                        }
                        d.reanchored_selection = None;
                        d.selected_key = None;
                        d.invalidate_viewport_fingerprint();
                        false
                    });
                    if !was_reanchor && this.has_selection {
                        this.has_selection = false;
                        cx.notify();
                    }
                }
                TableEvent::ColumnWidthsChanged(widths) => {
                    table.update(cx, |table, cx| {
                        if table
                            .delegate_mut()
                            .column_layout
                            .apply_visible_widths(widths)
                        {
                            table.delegate_mut().invalidate_viewport_fingerprint();
                            table.delegate().persist_column_layout(cx);
                        }
                    });
                }
                _ => {}
            },
        ));

        let mut type_to_search = TypeToSearchState::default();
        type_to_search.sync_committed(&initial_query);

        Self {
            store,
            coordinator,
            _details_lease: lease,
            table,
            table_pane,
            table_left_x: Rc::new(Cell::new(px(0.0))),
            view_mode,
            highlighted_key,
            has_selection,
            last_locale,
            type_to_search,
            pending_snapshot: None,
            scroll_flush_task: None,
            _subscriptions: subscriptions,
        }
    }

    /// Commits a live [`DetailsTableSlice`] to `self.table`, re-anchoring selection in `O(1)`
    /// steady state and notifying `TableState` only when the visible viewport fingerprint changed.
    ///
    /// Returns `true` if `TableState` was notified to repaint.
    fn commit_snapshot(
        &mut self,
        rev: u64,
        next_details: Arc<DetailsTableSlice>,
        next_caps: CapabilityFlags,
        next_platform: PlatformProfile,
        next_ram: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        let (next_view_mode, next_has_sel, table_notified) = self.table.update(cx, |table, cx| {
            let prev_selected_row = table.selected_row();
            let (sort_changed, prev_fp, had_visible_rows, had_pending_scroll) = {
                let d = table.delegate_mut();
                let sort_changed = d.details.sort_column != next_details.sort_column
                    || d.details.sort_direction != next_details.sort_direction;
                let had_visible = !d.visible_rows.is_empty();
                let had_pending_scroll = d.pending_scroll_key.is_some();
                let prev_fp = d
                    .last_viewport_fingerprint
                    .unwrap_or_else(|| d.compute_viewport_fingerprint(prev_selected_row));

                d.row_cache.begin_epoch();
                d.row_cache.prune_if_needed();
                let platform_changed = d.platform != next_platform;
                d.details = next_details;
                d.capabilities = next_caps;
                d.platform = next_platform;
                d.total_ram_bytes = next_ram;
                d.last_rev = rev;
                if platform_changed {
                    d.locale = DetailsLocaleCache::build(next_platform);
                }
                (sort_changed, prev_fp, had_visible, had_pending_scroll)
            };

            let (view_mode, selected_idx) = {
                let d = table.delegate();
                let selected_idx = d.selected_key.and_then(|sel| {
                    find_row_index_by_key_fast(&d.details.rows, sel, prev_selected_row)
                });
                (d.details.view_mode, selected_idx)
            };

            let mut selection_changed = false;
            if let Some(idx) = selected_idx {
                if prev_selected_row != Some(idx) {
                    table.delegate_mut().reanchored_selection = Some(Some(idx));
                    table.set_selected_row(idx, cx);
                    selection_changed = true;
                }
            } else if prev_selected_row.is_some() {
                table.delegate_mut().reanchored_selection = Some(None);
                table.clear_selection(cx);
                selection_changed = true;
            }

            Self::try_scroll_to_pending_in_table(table, cx);
            let scrolled_to_pending =
                had_pending_scroll && table.delegate().pending_scroll_key.is_none();

            let effective_selected_idx = table.selected_row();
            let next_fp = {
                let d = table.delegate_mut();
                let fp = d.compute_viewport_fingerprint(effective_selected_idx);
                d.last_viewport_fingerprint = Some(fp);
                fp
            };

            if sort_changed {
                table.refresh(cx);
            }

            let should_notify = !had_visible_rows
                || sort_changed
                || selection_changed
                || scrolled_to_pending
                || prev_fp != next_fp;
            if should_notify {
                cx.notify();
            }
            (
                view_mode,
                selected_idx.is_some() || scrolled_to_pending,
                should_notify,
            )
        });

        if self.view_mode != next_view_mode || self.has_selection != next_has_sel {
            self.view_mode = next_view_mode;
            self.has_selection = next_has_sel;
            cx.notify();
        }

        table_notified
    }

    fn ensure_scroll_flush_scheduled(&mut self, cx: &mut Context<Self>) {
        if self.scroll_flush_task.is_some() {
            return;
        }
        let table = self.table.clone();
        self.scroll_flush_task = Some(cx.spawn(async move |weak, cx| {
            loop {
                let delay = match cx.update(|cx| {
                    table
                        .read(cx)
                        .delegate()
                        .scroll_coalescer
                        .remaining_quiet_delay(Instant::now())
                }) {
                    Some(d) if !d.is_zero() => d,
                    Some(_) => break,
                    None => return,
                };
                cx.background_executor()
                    .timer(delay.max(Duration::from_millis(1)))
                    .await;
            }

            let _ = weak.update(cx, |this, cx| {
                this.scroll_flush_task = None;
                let had_pending = this.table.update(cx, |table, _| {
                    table.delegate_mut().scroll_coalescer.take_pending()
                });
                if had_pending && let Some(pending) = this.pending_snapshot.take() {
                    this.commit_snapshot(
                        pending.rev,
                        pending.details,
                        pending.capabilities,
                        pending.platform,
                        pending.total_ram_bytes,
                        cx,
                    );
                }
            });
        }));
    }

    fn try_scroll_to_pending_in_table(
        table: &mut TableState<DetailsTableDelegate>,
        cx: &mut Context<TableState<DetailsTableDelegate>>,
    ) {
        let Some(target) = table.delegate().pending_scroll_key else {
            return;
        };
        let found = find_row_index_by_key(&table.delegate().details.rows, target).or_else(|| {
            (target.create_time_100ns == 0)
                .then(|| {
                    table
                        .delegate()
                        .details
                        .rows
                        .iter()
                        .position(|row| row.key.pid == target.pid)
                })
                .flatten()
        });
        if let Some(idx) = found {
            let resolved_key = table.delegate().details.rows[idx].key;
            table
                .vertical_scroll_handle
                .scroll_to_item(idx, ScrollStrategy::Center);
            {
                let d = table.delegate_mut();
                d.selected_key = Some(resolved_key);
                if d.highlighted_key == Some(target) {
                    d.highlighted_key = Some(resolved_key);
                }
                d.reanchored_selection = Some(Some(idx));
                d.pending_scroll_key = None;
            }
            table.set_selected_row(idx, cx);
        }
    }

    /// Updates the highlighted target [`ProcessKey`] (e.g., from `NavigationIntent::JumpToDetails`).
    pub(crate) fn set_highlighted_key(&mut self, key: Option<ProcessKey>, cx: &mut Context<Self>) {
        self.highlighted_key = key;
        let next_has_sel = self.table.update(cx, |table, cx| {
            let d = table.delegate_mut();
            d.highlighted_key = key;
            d.invalidate_viewport_fingerprint();
            if let Some(target) = key {
                d.selected_key = Some(target);
                d.pending_scroll_key = Some(target);
                d.mark_user_initiated();
                Self::try_scroll_to_pending_in_table(table, cx);
            } else {
                d.pending_scroll_key = None;
            }
            cx.notify();
            table.delegate().has_valid_selection()
        });
        self.has_selection = next_has_sel;
        cx.notify();
    }

    /// Selects, highlights, and scrolls to `key` (alias for `JumpToDetails` navigation).
    #[allow(dead_code)]
    pub(crate) fn highlight_process(
        &mut self,
        key: ProcessKey,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_highlighted_key(Some(key), cx);
    }

    fn trigger_end_task_for_selected(&self, window: &mut Window, cx: &mut Context<Self>) {
        let target = self.table.read(cx).delegate().selected_target();
        if let Some(target) = target {
            request_end_task(&self.store, &target, window, cx);
        }
    }

    fn trigger_export_csv(&self, window: &mut Window, cx: &mut Context<Self>) {
        let (headers, rows) = self.table.read(cx).delegate().export_visible_csv_payload();
        spawn_csv_export(
            self.store.clone(),
            "loom-details",
            headers,
            rows,
            window,
            cx,
        );
    }

    fn copy_selected_short(&self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.table.read(cx).delegate().selected_copy_short() else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    fn copy_selected_summary(&self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.table.read(cx).delegate().selected_copy_summary() else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }
}

impl Render for DetailsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        debug_assert_eq!(self._details_lease.domain(), MonitorDomain::Details);

        let view_mode = self.view_mode;
        let highlighted_key = self.highlighted_key;
        let has_selection = self.has_selection;
        let preedit_badge: Option<String> = self
            .type_to_search
            .preedit_text()
            .map(|pre| format!("{}: {pre}", i18n::tr("proc_ux_ime_searching")));

        let theme = cx.theme();
        let border = theme.border;
        let fg = theme.foreground;
        let muted_bg = theme.muted;
        let primary = theme.primary;
        let radius = theme.radius;

        let store_flat = self.store.clone();
        let store_tree = self.store.clone();
        let store_run_task = self.store.clone();

        let flat_label = i18n::tr("details.view_flat");
        let tree_label = i18n::tr("details.view_tree");
        let export_csv_label = i18n::tr("csv_export_button");

        let toolbar = h_flex()
            .w_full()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_3()
            .px_4()
            .py_2p5()
            .border_b_1()
            .border_color(border)
            .child(
                h_flex()
                    .items_center()
                    .gap_2p5()
                    .flex_wrap()
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .text_color(fg)
                            .child(i18n::tr("details.title")),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .p_0p5()
                            .rounded(radius)
                            .bg(muted_bg.opacity(0.65))
                            .child(
                                Button::new("details-mode-flat")
                                    .xsmall()
                                    .icon(Icon::new(AssetIconName::List))
                                    .tooltip(flat_label.clone())
                                    .accessibility_label(flat_label)
                                    .map(|btn| {
                                        if view_mode == DetailsViewMode::Flat {
                                            btn.primary()
                                        } else {
                                            btn.ghost()
                                        }
                                    })
                                    .on_click(move |_, _, cx| {
                                        UiCommands::set_details_view_mode(
                                            &store_flat,
                                            DetailsViewMode::Flat,
                                            cx,
                                        );
                                    }),
                            )
                            .child(
                                Button::new("details-mode-tree")
                                    .xsmall()
                                    .icon(Icon::new(AssetIconName::ListTree))
                                    .tooltip(tree_label.clone())
                                    .accessibility_label(tree_label)
                                    .map(|btn| {
                                        if view_mode == DetailsViewMode::Tree {
                                            btn.primary()
                                        } else {
                                            btn.ghost()
                                        }
                                    })
                                    .on_click(move |_, _, cx| {
                                        UiCommands::set_details_view_mode(
                                            &store_tree,
                                            DetailsViewMode::Tree,
                                            cx,
                                        );
                                    }),
                            ),
                    )
                    .when_some(preedit_badge, |el, text| {
                        el.child(
                            div()
                                .px_2()
                                .py_0p5()
                                .rounded(radius)
                                .bg(primary.opacity(0.14))
                                .text_xs()
                                .text_color(primary)
                                .child(text),
                        )
                    }),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        Button::new("details-export-csv")
                            .small()
                            .outline()
                            .icon(Icon::new(AssetIconName::Download))
                            .tooltip(export_csv_label.clone())
                            .accessibility_label(export_csv_label)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.trigger_export_csv(window, cx);
                            })),
                    )
                    .child(
                        Button::new("details-run-new-task")
                            .small()
                            .primary()
                            .icon(IconName::Play)
                            .label(i18n::tr("details.run_new_task"))
                            .on_click(move |_, window, cx| {
                                crate::shell::dialogs::open_run_new_task_dialog(
                                    store_run_task.clone(),
                                    window,
                                    cx,
                                );
                            }),
                    )
                    .child(
                        Button::new("details-end-task")
                            .small()
                            .danger()
                            .outline()
                            .icon(IconName::CircleX)
                            .label(i18n::tr("details.end_task"))
                            .disabled(!has_selection)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.trigger_end_task_for_selected(window, cx);
                            })),
                    ),
            );

        let highlight_banner = highlighted_key.map(|key| {
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .px_4()
                .py_1p5()
                .bg(primary.opacity(0.10))
                .border_b_1()
                .border_color(primary.opacity(0.25))
                .child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .font_medium()
                        .text_color(primary)
                        .child(Icon::new(IconName::Search).xsmall().text_color(primary))
                        .child(format!(
                            "{}: {}",
                            i18n::tr("details.highlighted_pid"),
                            key.pid
                        )),
                )
                .child(
                    Button::new("details-clear-highlight")
                        .xsmall()
                        .ghost()
                        .label(i18n::tr("details.clear_highlight"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.set_highlighted_key(None, cx);
                        })),
                )
        });

        let table_focus = self.table.focus_handle(cx);
        let view_entity = cx.entity();
        let table_left_x_click = Rc::clone(&self.table_left_x);
        let table_left_x_prepaint = Rc::clone(&self.table_left_x);

        v_flex()
            .id("details-view-root")
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let m = &event.keystroke.modifiers;
                let primary_mod = m.control || m.platform;
                let no_modifiers = !m.control && !m.alt && !m.platform && !m.function && !m.shift;

                if primary_mod && !m.alt && event.keystroke.key.eq_ignore_ascii_case("c") {
                    let copied = if m.shift {
                        this.copy_selected_summary(cx)
                    } else {
                        this.copy_selected_short(cx)
                    };
                    if copied {
                        cx.stop_propagation();
                    }
                    return;
                }

                if no_modifiers && event.keystroke.key == "delete" && this.has_selection {
                    this.trigger_end_task_for_selected(window, cx);
                    cx.stop_propagation();
                    return;
                }

                if !m.control && !m.alt && !m.platform && event.keystroke.key == "backspace" {
                    let current = this.store.read(cx).search_query().to_owned();
                    if let Some(next) = this.type_to_search.handle_backspace(&current) {
                        UiCommands::set_search_query(&this.store, next, cx);
                        cx.stop_propagation();
                    }
                    return;
                }

                if no_modifiers && event.keystroke.key == "escape" {
                    let was_composing = this.type_to_search.is_composing();
                    let current = this.store.read(cx).search_query().to_owned();
                    if let Some(next) = this.type_to_search.handle_escape(&current) {
                        UiCommands::set_search_query(&this.store, next, cx);
                        cx.stop_propagation();
                    } else if was_composing {
                        cx.notify();
                        cx.stop_propagation();
                    }
                }
            }))
            .size_full()
            .min_h_0()
            .child(toolbar)
            .when_some(highlight_banner, |this, banner| this.child(banner))
            .child(
                div()
                    .id("details-table-hover-zone")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        this.table.update(cx, |table, cx| {
                            table.delegate_mut().set_table_hovered(*hovered, cx);
                        });
                    }))
                    .capture_any_mouse_down(cx.listener(
                        move |this, event: &MouseDownEvent, _, cx| {
                            if event.button != MouseButton::Right {
                                return;
                            }
                            let local_x = f32::from(event.position.x - table_left_x_click.get());
                            this.table.update(cx, |table, _| {
                                let scroll_x = f32::from(
                                    (-table.horizontal_scroll_handle.offset().x).max(px(0.0)),
                                );
                                let col_ix = table
                                    .delegate()
                                    .column_layout
                                    .column_index_at_x(local_x, scroll_x);
                                table.delegate_mut().last_right_clicked_col = col_ix;
                            });
                        },
                    ))
                    .child(TableInputBridge::new(table_focus, view_entity))
                    .child(self.table_pane.clone())
                    .on_prepaint(move |bounds, _, _| {
                        table_left_x_prepaint.set(bounds.left());
                    }),
            )
    }
}

/// Builds the multi-level right-click [`PopupMenu`] anchored strictly to `target.key`.
#[allow(clippy::too_many_arguments)]
fn build_row_context_menu(
    menu: PopupMenu,
    store: &Entity<UiStore>,
    shell_handle: &ShellHandle,
    target: &ProcessActionTarget,
    copy_fields: CopyRowFields,
    capabilities: CapabilityFlags,
    window: &mut Window,
    cx: &mut Context<TableState<DetailsTableDelegate>>,
) -> (PopupMenu, Vec<FocusHandle>) {
    let key = target.key;
    let current_priority = target.priority;
    let is_kernel_or_idle = target
        .status
        .intersects(ProcessStatusFlags::SYSTEM_IDLE | ProcessStatusFlags::KERNEL);
    let eco_supported = capabilities.contains(CapabilityFlags::ECO_MODE);
    let suspend_supported = capabilities.contains(CapabilityFlags::SUSPEND_RESUME);
    let wait_chain_supported = capabilities.contains(CapabilityFlags::WAIT_CHAIN);
    let minidump_supported = capabilities.contains(CapabilityFlags::MINIDUMP);
    let uac_supported = capabilities.contains(CapabilityFlags::UAC_VIRTUALIZATION);
    let debugger_supported = capabilities.contains(CapabilityFlags::DEBUGGER_ATTACH);
    let eco_enabled = target.status.contains(ProcessStatusFlags::EFFICIENCY_MODE);
    let eco_eligible = is_eco_mode_eligible(target.status, capabilities);
    let is_suspended = target.status.contains(ProcessStatusFlags::SUSPENDED);
    let suspend_eligible =
        can_suspend_or_resume_process(key.pid, &target.name, target.status, capabilities);
    let is_explorer = is_explorer_image(&target.name);
    let leak_ignore = target
        .has_leak_warning
        .then(|| leak_ignore_target(target.exe_path.as_deref(), &target.name))
        .flatten();
    let uac_enabled = target.uac_virtualization == UacVirtualization::Enabled;
    let uac_toggleable = matches!(
        target.uac_virtualization,
        UacVirtualization::Enabled | UacVirtualization::Disabled
    );
    let priority_allowed = can_set_priority(target.status, capabilities);
    let affinity_allowed = can_set_affinity(target.status, capabilities);
    let has_exe_path = target.exe_path.is_some();

    let store_end = store.clone();
    let target_end = target.clone();

    let store_tree = store.clone();
    let target_tree = target.clone();

    let store_priority = store.clone();

    let store_affinity = store.clone();
    let affinity_name = target.name.to_string();

    let store_eco = store.clone();

    let store_open_loc = store.clone();
    let open_loc_path = target.exe_path.clone();

    let store_props = store.clone();
    let props_path = target.exe_path.clone();

    let store_search = store.clone();
    let search_query = target.name.to_string();
    let shell_services = shell_handle.clone();
    let can_jump_services = key.pid > 0 && capabilities.contains(CapabilityFlags::SERVICES);
    let mut submenu_handles = Vec::with_capacity(2);

    let priority_submenu = PopupMenu::build(window, cx, move |mut submenu, _window, _cx| {
        for level in PRIORITY_MENU_ORDER {
            let store_level = store_priority.clone();
            submenu = submenu.item(
                PopupMenuItem::new(i18n::process_priority_label(level))
                    .checked(current_priority == level)
                    .disabled(!priority_allowed)
                    .on_click(move |_, _, cx| {
                        UiCommands::set_priority(&store_level, key, level, cx);
                    }),
            );
        }
        submenu
    });
    submenu_handles.push(priority_submenu.focus_handle(cx));

    let copy_submenu = build_copy_submenu(copy_fields, window, cx);
    submenu_handles.push(copy_submenu.focus_handle(cx));

    let mut menu = menu
        .item(
            PopupMenuItem::new(i18n::tr("details.menu_end_task"))
                .icon(IconName::CircleX)
                .on_click(move |_, window, cx| {
                    request_end_task(&store_end, &target_end, window, cx);
                }),
        )
        .item(
            PopupMenuItem::new(i18n::tr("details.menu_end_process_tree"))
                .icon(IconName::Ban)
                .on_click(move |_, window, cx| {
                    open_end_process_tree_dialog(&store_tree, &target_tree, window, cx);
                }),
        );

    if suspend_supported {
        let store_suspend = store.clone();
        let (label, icon) = if is_suspended {
            (i18n::tr("proc_ux_menu_resume"), IconName::Play)
        } else {
            (i18n::tr("proc_ux_menu_suspend"), IconName::Pause)
        };
        menu = menu.item(
            PopupMenuItem::new(label)
                .icon(icon)
                .disabled(!suspend_eligible)
                .on_click(move |_, _, cx| {
                    dispatch_suspend_or_resume(&store_suspend, key, is_suspended, cx);
                }),
        );

        if is_explorer {
            let store_restart = store.clone();
            menu = menu.item(
                PopupMenuItem::new(i18n::tr("proc_ux_menu_restart_explorer"))
                    .icon(Icon::new(AssetIconName::RotateCw))
                    .on_click(move |_, _, cx| {
                        dispatch_restart_explorer(&store_restart, cx);
                    }),
            );
        }
    }

    menu = menu
        .separator()
        .item(
            PopupMenuItem::submenu(i18n::tr("details.menu_set_priority"), priority_submenu)
                .icon(IconName::ChevronsUpDown),
        )
        .item(
            PopupMenuItem::new(i18n::tr("details.menu_set_affinity"))
                .icon(IconName::Cpu)
                .disabled(!affinity_allowed)
                .on_click(move |_, window, cx| {
                    affinity_dialog::open_set_affinity_dialog(
                        store_affinity.clone(),
                        key,
                        affinity_name.clone(),
                        window,
                        cx,
                    );
                }),
        );

    if eco_supported {
        menu = menu.item(
            PopupMenuItem::new(i18n::tr("details.menu_efficiency_mode"))
                .checked(eco_enabled)
                .disabled(!eco_eligible)
                .on_click(move |_, _, cx| {
                    UiCommands::set_efficiency_mode(&store_eco, key, !eco_enabled, cx);
                }),
        );
    }

    if let Some(ignore_target) = leak_ignore {
        let store_leak = store.clone();
        menu = menu.item(
            PopupMenuItem::new(i18n::tr("leak_warning_ignore_menu"))
                .icon(IconName::TriangleAlert)
                .on_click(move |_, _, cx| {
                    dispatch_ignore_leak_path(&store_leak, ignore_target.clone(), cx);
                }),
        );
    }

    if wait_chain_supported || minidump_supported || uac_supported || debugger_supported {
        menu = menu.separator();
        if wait_chain_supported {
            let store_wct = store.clone();
            let wct_name = target.name.to_string();
            menu = menu.item(
                PopupMenuItem::new(i18n::tr("details.menu_analyze_wait_chain"))
                    .icon(IconName::Search)
                    .disabled(is_kernel_or_idle)
                    .on_click(move |_, window, cx| {
                        wait_chain_dialog::open_wait_chain_dialog(
                            store_wct.clone(),
                            key,
                            wct_name.clone(),
                            window,
                            cx,
                        );
                    }),
            );
        }
        if minidump_supported {
            let store_dump = store.clone();
            let dump_name = target.name.to_string();
            menu = menu.item(
                PopupMenuItem::new(i18n::tr("details.menu_create_dump"))
                    .icon(IconName::File)
                    .disabled(is_kernel_or_idle)
                    .on_click(move |_, window, cx| {
                        dialogs::open_minidump_dialog(
                            store_dump.clone(),
                            key,
                            dump_name.clone(),
                            window,
                            cx,
                        );
                    }),
            );
        }
        if uac_supported {
            let store_uac = store.clone();
            menu = menu.item(
                PopupMenuItem::new(i18n::tr("details.menu_uac_virtualization"))
                    .checked(uac_enabled)
                    .disabled(!uac_toggleable || is_kernel_or_idle)
                    .on_click(move |_, _, cx| {
                        UiCommands::set_uac_virtualization(&store_uac, key, !uac_enabled, cx);
                    }),
            );
        }
        if debugger_supported {
            let store_dbg = store.clone();
            menu = menu.item(
                PopupMenuItem::new(i18n::tr("details.menu_attach_debugger"))
                    .icon(IconName::SquareTerminal)
                    .disabled(is_kernel_or_idle)
                    .on_click(move |_, _, cx| {
                        UiCommands::attach_debugger(&store_dbg, key, cx);
                    }),
            );
        }
    }

    let built = menu
        .separator()
        .item(
            PopupMenuItem::submenu(i18n::tr("proc_ux_copy_submenu"), copy_submenu)
                .icon(Icon::new(AssetIconName::Copy)),
        )
        .separator()
        .item(
            PopupMenuItem::new(i18n::tr("details.menu_open_file_location"))
                .icon(IconName::FolderOpen)
                .disabled(!has_exe_path)
                .on_click(move |_, _, cx| {
                    if let Some(ref path) = open_loc_path {
                        UiCommands::open_file_location(
                            &store_open_loc,
                            PathBuf::from(path.as_ref()),
                            cx,
                        );
                    }
                }),
        )
        .item(
            PopupMenuItem::new(i18n::tr("details.menu_properties"))
                .icon(IconName::Info)
                .disabled(!has_exe_path)
                .on_click(move |_, _, cx| {
                    if let Some(ref path) = props_path {
                        UiCommands::show_file_properties(
                            &store_props,
                            PathBuf::from(path.as_ref()),
                            cx,
                        );
                    }
                }),
        )
        .item(
            PopupMenuItem::new(i18n::tr("details.menu_search_online"))
                .icon(IconName::Globe)
                .on_click(move |_, _, cx| {
                    UiCommands::search_online(&store_search, search_query.clone(), cx);
                }),
        )
        .separator()
        .item(
            PopupMenuItem::new(i18n::tr("details.menu_go_to_services"))
                .icon(IconName::Settings2)
                .disabled(!can_jump_services)
                .on_click(move |_, window, cx| {
                    shell_services.jump_to_services(key.pid, window, cx);
                }),
        );
    (built, submenu_handles)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use loom_types::{
        CapabilityFlags, CpuAffinityMask, DetailsRowProjection, DetailsSortColumn, ProcessArch,
        ProcessKey, ProcessPriority, ProcessStatusFlags, SortDirection, UacVirtualization,
    };

    use super::affinity_dialog::{AffinitySelectionState, CpuGroupRange, compute_cpu_groups};
    use super::*;

    fn sample_row(pid: u32, create_time_100ns: u64, name: &str) -> DetailsRowProjection {
        DetailsRowProjection {
            key: ProcessKey::new(pid, create_time_100ns),
            parent_key: None,
            depth: 0,
            has_children: false,
            is_expanded: false,
            name: Arc::from(name),
            description: None,
            publisher: None,
            icon: None,
            status: ProcessStatusFlags::empty(),
            user_name: None,
            session_id: 1,
            cpu_permille: 0,
            cpu_time_100ns: 0,
            private_working_set_bytes: 0,
            working_set_bytes: 0,
            working_set_delta_bytes: 0,
            commit_bytes: 0,
            thread_count: 1,
            handle_count: 10,
            priority: ProcessPriority::Normal,
            arch: ProcessArch::X64,
            uac_virtualization: UacVirtualization::NotAllowed,
            gpu_dedicated_bytes: 0,
            gpu_shared_bytes: 0,
            exe_path: None,
            command_line: None,
            leak_warning: None,
            webview2_role: None,
        }
    }

    #[test]
    fn jump_to_details_lookup_tracks_process_key_across_resorts_and_pid_reuse() {
        let target = ProcessKey::new(4242, 9_000_000);
        let reused_pid_old = ProcessKey::new(4242, 1_000_000);

        let initial_rows = vec![
            sample_row(100, 100, "alpha.exe"),
            sample_row(4242, 1_000_000, "old-worker.exe"),
            sample_row(4242, 9_000_000, "target.exe"),
            sample_row(8888, 500, "zeta.exe"),
        ];
        assert_eq!(find_row_index_by_key(&initial_rows, target), Some(2));
        assert_eq!(
            find_row_index_by_key(&initial_rows, reused_pid_old),
            Some(1)
        );

        // Simulate live re-sorting by CPU/Memory shifting `target` to index 0.
        let resorted_rows = vec![
            sample_row(4242, 9_000_000, "target.exe"),
            sample_row(8888, 500, "zeta.exe"),
            sample_row(100, 100, "alpha.exe"),
        ];
        assert_eq!(find_row_index_by_key(&resorted_rows, target), Some(0));
        assert_eq!(find_row_index_by_key(&resorted_rows, reused_pid_old), None);
    }

    #[test]
    fn affinity_group_slicing_supports_single_and_multi_group_topologies() {
        assert!(compute_cpu_groups(0).is_empty());

        assert_eq!(
            compute_cpu_groups(16),
            vec![CpuGroupRange {
                group_index: 0,
                start_cpu: 0,
                end_cpu_exclusive: 16,
            }]
        );

        assert_eq!(
            compute_cpu_groups(64),
            vec![CpuGroupRange {
                group_index: 0,
                start_cpu: 0,
                end_cpu_exclusive: 64,
            }]
        );

        assert_eq!(
            compute_cpu_groups(96),
            vec![
                CpuGroupRange {
                    group_index: 0,
                    start_cpu: 0,
                    end_cpu_exclusive: 64,
                },
                CpuGroupRange {
                    group_index: 1,
                    start_cpu: 64,
                    end_cpu_exclusive: 96,
                },
            ]
        );

        assert_eq!(
            compute_cpu_groups(144),
            vec![
                CpuGroupRange {
                    group_index: 0,
                    start_cpu: 0,
                    end_cpu_exclusive: 64,
                },
                CpuGroupRange {
                    group_index: 1,
                    start_cpu: 64,
                    end_cpu_exclusive: 128,
                },
                CpuGroupRange {
                    group_index: 2,
                    start_cpu: 128,
                    end_cpu_exclusive: 144,
                },
            ]
        );
    }

    #[test]
    fn affinity_all_processors_toggle_and_indeterminate_state_logic() {
        let mut sel = AffinitySelectionState::new(CpuAffinityMask::all(96));
        assert!(sel.has_multiple_groups());
        assert!(sel.all_checked());
        assert!(!sel.is_indeterminate());
        assert!(!sel.is_empty());

        // Clear one CPU in group 1 -> indeterminate state.
        assert!(sel.set_cpu(70, false));
        assert!(!sel.all_checked());
        assert!(sel.is_indeterminate());
        assert!(!sel.is_empty());
        assert_eq!(sel.mask().enabled_count(), 95);

        // Master checkbox unchecked -> empty mask (validation error active).
        sel.toggle_all(false);
        assert!(!sel.all_checked());
        assert!(!sel.is_indeterminate());
        assert!(sel.is_empty());
        assert_eq!(sel.mask().enabled_count(), 0);

        // Master checkbox checked -> restores all 96 CPUs across both groups.
        sel.toggle_all(true);
        assert!(sel.all_checked());
        assert!(!sel.is_indeterminate());
        assert!(!sel.is_empty());
        assert!(sel.mask().is_enabled(0));
        assert!(sel.mask().is_enabled(63));
        assert!(sel.mask().is_enabled(64));
        assert!(sel.mask().is_enabled(95));
        assert!(!sel.mask().is_enabled(96));
    }

    #[test]
    fn priority_menu_ordering_and_localized_labels() {
        // Strictly descending from Realtime down to Idle.
        for pair in PRIORITY_MENU_ORDER.windows(2) {
            assert!(
                pair[0] > pair[1],
                "expected {:?} > {:?} in menu order",
                pair[0],
                pair[1]
            );
        }

        // Verify en-US and zh-CN translations without mutating global locale across parallel tests.
        let expected = [
            (
                ProcessPriority::Realtime,
                "details.priority_realtime",
                "Realtime",
                "实时",
            ),
            (ProcessPriority::High, "details.priority_high", "High", "高"),
            (
                ProcessPriority::AboveNormal,
                "details.priority_above_normal",
                "Above normal",
                "高于正常",
            ),
            (
                ProcessPriority::Normal,
                "details.priority_normal",
                "Normal",
                "正常",
            ),
            (
                ProcessPriority::BelowNormal,
                "details.priority_below_normal",
                "Below normal",
                "低于正常",
            ),
            (ProcessPriority::Idle, "details.priority_idle", "Low", "低"),
        ];

        for (priority, key, en, zh) in expected {
            assert_eq!(rust_i18n::t!(key, locale = "en-US"), en);
            assert_eq!(rust_i18n::t!(key, locale = "zh-CN"), zh);
            let live_label = i18n::process_priority_label(priority);
            assert!(
                live_label == en || live_label == zh,
                "unexpected priority label '{live_label}' for {priority:?}"
            );
        }
    }

    #[test]
    fn eco_mode_eligibility_and_critical_process_guards() {
        let with_eco = CapabilityFlags::ECO_MODE;
        let without_eco = CapabilityFlags::empty();
        let prio_and_affinity = CapabilityFlags::PROCESS_PRIORITY | CapabilityFlags::CPU_AFFINITY;

        // Standard user process (including a non-kernel PID 4 on Linux/macOS) is eligible
        // when the backend advertises the capability and no kernel/system flags are set.
        assert!(is_eco_mode_eligible(ProcessStatusFlags::empty(), with_eco));
        assert!(is_eco_mode_eligible(
            ProcessStatusFlags::EFFICIENCY_MODE,
            with_eco
        ));
        assert!(!is_eco_mode_eligible(
            ProcessStatusFlags::empty(),
            without_eco
        ));
        assert!(!is_critical_or_system_process(ProcessStatusFlags::empty()));
        assert!(can_set_priority(
            ProcessStatusFlags::empty(),
            prio_and_affinity
        ));
        assert!(can_set_affinity(
            ProcessStatusFlags::empty(),
            prio_and_affinity
        ));
        assert!(!can_set_priority(
            ProcessStatusFlags::empty(),
            CapabilityFlags::empty()
        ));
        assert!(!can_set_affinity(
            ProcessStatusFlags::empty(),
            CapabilityFlags::empty()
        ));

        // Semantic SYSTEM_IDLE, KERNEL, PROTECTED, and CRITICAL flags block EcoQoS & guard actions
        // regardless of numeric PID (e.g. Linux PID 1/2 kernel/init or Windows PID 0/4).
        for blocked in [
            ProcessStatusFlags::SYSTEM_IDLE,
            ProcessStatusFlags::KERNEL,
            ProcessStatusFlags::PROTECTED,
            ProcessStatusFlags::CRITICAL,
        ] {
            assert!(!is_eco_mode_eligible(blocked, with_eco));
            assert!(is_critical_or_system_process(blocked));
        }

        // Priority and affinity changes are disabled for SYSTEM_IDLE and KERNEL processes.
        for non_schedulable in [ProcessStatusFlags::SYSTEM_IDLE, ProcessStatusFlags::KERNEL] {
            assert!(!can_set_priority(non_schedulable, prio_and_affinity));
            assert!(!can_set_affinity(non_schedulable, prio_and_affinity));
        }
    }

    #[test]
    fn table_min_width_includes_virtualization_column_only_when_supported() {
        let without_uac = table_min_width(CapabilityFlags::empty());
        let with_uac = table_min_width(CapabilityFlags::UAC_VIRTUALIZATION);
        assert!(
            (with_uac - without_uac - COL_UAC_W).abs() < f32::EPSILON,
            "expected UAC column width ({}) delta, got {}",
            COL_UAC_W,
            with_uac - without_uac
        );
    }

    #[test]
    fn sort_direction_toggles_active_column_and_defaults_on_switch() {
        assert_eq!(
            next_sort_direction(
                DetailsSortColumn::Name,
                SortDirection::Ascending,
                DetailsSortColumn::Name
            ),
            SortDirection::Descending
        );
        assert_eq!(
            next_sort_direction(
                DetailsSortColumn::Name,
                SortDirection::Ascending,
                DetailsSortColumn::Cpu
            ),
            SortDirection::Descending
        );
        assert_eq!(
            next_sort_direction(
                DetailsSortColumn::Cpu,
                SortDirection::Descending,
                DetailsSortColumn::Pid
            ),
            SortDirection::Ascending
        );
    }

    #[test]
    fn display_user_name_strips_computer_and_domain_prefix() {
        assert_eq!(display_user_name(Some(r"LAPTOP-M850NT2C\Lenovo")), "Lenovo");
        assert_eq!(display_user_name(Some(r"NT AUTHORITY\SYSTEM")), "SYSTEM");
        assert_eq!(display_user_name(Some("Lenovo")), "Lenovo");
        assert_eq!(display_user_name(Some("")), "—");
        assert_eq!(display_user_name(Some("   ")), "—");
        assert_eq!(display_user_name(None), "—");
        assert_eq!(display_user_name(Some(r"DOMAIN\")), r"DOMAIN\");
    }

    #[test]
    fn details_smart_copy_formats_short_and_structured_summary() {
        let mut row = sample_row(7777, 123_456, "explorer.exe");
        row.user_name = Some(Arc::from(r"WORKSTATION\Alice"));
        row.cpu_permille = 55;
        row.private_working_set_bytes = 128 * 1024 * 1024;
        row.handle_count = 900;
        row.thread_count = 32;
        row.publisher = Some(Arc::from("Microsoft Windows"));
        row.exe_path = Some(Arc::from(r"C:\Windows\explorer.exe"));
        row.command_line = Some(Arc::from(r"C:\Windows\explorer.exe"));

        assert_eq!(format_details_copy_short(&row), "explorer.exe (7777)");
        let summary = format_details_row_summary(&row, "Running", "Normal", "64-bit");
        assert!(summary.contains("Name: explorer.exe"));
        assert!(summary.contains("PID: 7777"));
        assert!(summary.contains("User: Alice"));
        assert!(summary.contains("CPU: 5.5%"));
        assert!(summary.contains("Memory: 128.0 MB"));
        assert!(summary.contains("Handles: 900"));
        assert!(summary.contains("Threads: 32"));
        assert!(summary.contains("Publisher: Microsoft Windows"));
        assert!(summary.contains(r"Path: C:\Windows\explorer.exe"));
    }

    #[gpui_kit::test]
    fn details_view_hover_freeze_ime_search_copy_and_csv_payload(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::{AppContext as _, EntityInputHandler as _, VisualTestContext, px, size};
        use loom_config::{AppSettings, ConfigStore};
        use loom_engine::EngineCoordinator;
        use loom_pal::{NativeBackend, PlatformBackend as _};
        use loom_types::{LeakKind, LeakWarning, WebView2Role};

        use crate::bridge::UiStore;
        use crate::shell::ShellHandle;

        cx.update(gpui_kit::init);

        let dir = tempfile::tempdir().expect("tempdir");
        let config_store = ConfigStore::new(dir.path().join("settings.toml"));
        let backend = NativeBackend::initialize().expect("native backend");
        let coordinator =
            Arc::new(EngineCoordinator::new(backend, AppSettings::default()).expect("coordinator"));
        let coordinator_for_shutdown = Arc::clone(&coordinator);

        let store =
            cx.new(|cx| UiStore::new(coordinator, config_store, AppSettings::default(), cx));
        let shell = ShellHandle::new(|_, _, _| {});

        let store_for_window = store.clone();
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), move |window, cx| {
                let view = cx.new(|cx| DetailsView::new(store_for_window, None, shell, window, cx));
                cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
            })
            .expect("open window")
        });
        cx.simulate_window_resize(window.into(), size(px(1200.), px(720.)));
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.update(|window, cx| window.draw(cx).clear(cx));

        let view = window
            .read_with(&vcx, |root, _| {
                root.view()
                    .clone()
                    .downcast::<DetailsView>()
                    .expect("root wraps DetailsView")
            })
            .expect("read root");

        let mut row = sample_row(8080, 555_000, "msedgewebview2.exe");
        row.publisher = Some(Arc::from("Microsoft Corporation"));
        row.webview2_role = Some(WebView2Role::Gpu);
        row.leak_warning = Some(LeakWarning {
            kind: LeakKind::Children,
            summary: Arc::from("Sustained child process growth (18 live children, baseline 2)"),
            current_value: 18,
            baseline_value: 2,
        });
        let key = row.key;

        let table = view.read_with(&vcx, |v, _| v.table.clone());
        table.update(&mut vcx, |table, cx| {
            let d = table.delegate_mut();
            d.details = Arc::new(DetailsTableSlice {
                revision: 42,
                rows: vec![row],
                ..Default::default()
            });
            d.selected_key = Some(key);
            cx.notify();
        });
        vcx.update(|window, cx| window.draw(cx).clear(cx));

        // Hover pause & header unfreeze transitions.
        table.update(&mut vcx, |table, cx| {
            table.delegate_mut().set_table_hovered(true, cx);
            assert!(table.delegate().order_freeze.is_frozen());
            table.delegate_mut().set_header_hovered(true, cx);
            assert!(!table.delegate().order_freeze.is_frozen());
            table.delegate_mut().set_header_hovered(false, cx);
            assert!(table.delegate().order_freeze.is_frozen());
            table.delegate_mut().set_table_hovered(false, cx);
            assert!(!table.delegate().order_freeze.is_frozen());
        });

        // Direct type-to-search with CJK IME.
        view.update_in(&mut vcx, |this, window, cx| {
            this.replace_text_in_range(None, "gpu", window, cx);
            this.replace_and_mark_text_in_range(None, "ce", None, window, cx);
            this.replace_text_in_range(None, "", window, cx);
        });
        assert_eq!(
            store.read_with(&vcx, |s, _| s.search_query().to_owned()),
            "gpu"
        );

        // Smart copy short & summary.
        view.update(&mut vcx, |this, cx| {
            assert!(this.copy_selected_short(cx));
        });
        assert_eq!(
            vcx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("msedgewebview2.exe (8080)")
        );
        view.update(&mut vcx, |this, cx| {
            assert!(this.copy_selected_summary(cx));
        });
        assert!(
            vcx.read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default()
                .contains("PID: 8080")
        );

        // Visible-column CSV payload.
        let (headers, rows) =
            table.read_with(&vcx, |t, _| t.delegate().export_visible_csv_payload());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), headers.len());
        assert_eq!(rows[0][0], "msedgewebview2.exe");

        coordinator_for_shutdown.shutdown();
    }

    #[test]
    fn details_viewport_fingerprint_detects_visible_changes_and_ignores_offscreen_and_jitter() {
        let layout = TableColumnLayout::new(DetailsColumn::default_specs(CapabilityFlags::empty()));
        let visible_rows = 0..10;
        let visible_cols = 0..8;
        let ram = 16 * 1024 * 1024 * 1024u64;

        let mut slice = DetailsTableSlice {
            revision: 1,
            rows: (0..60u32)
                .map(|i| {
                    let mut r = sample_row(1000 + i, 50_000 + u64::from(i), &format!("p-{i}.exe"));
                    r.cpu_permille = 30;
                    r.cpu_time_100ns = 10_000_000; // 1.0s
                    r.private_working_set_bytes = 128 * 1024 * 1024;
                    r
                })
                .collect(),
            ..Default::default()
        };

        let fp_base = compute_details_viewport_fingerprint(
            &slice,
            &layout,
            &visible_rows,
            &visible_cols,
            None,
            None,
            ram,
        );

        // Mutating an off-screen row (index 50) keeps the same fingerprint.
        slice.rows[50].cpu_permille = 880;
        slice.rows[50].private_working_set_bytes = 2 * 1024 * 1024 * 1024;
        assert_eq!(
            fp_base,
            compute_details_viewport_fingerprint(
                &slice,
                &layout,
                &visible_rows,
                &visible_cols,
                None,
                None,
                ram,
            )
        );

        // Sub-display-tenth private WS jitter (+512 B on 128.0 MB) and sub-second CPU time jitter
        // (+500ms within 1s) render identically -> fingerprint stays identical.
        slice.rows[1].private_working_set_bytes += 512;
        slice.rows[1].cpu_time_100ns = 15_000_000;
        assert_eq!(
            fp_base,
            compute_details_viewport_fingerprint(
                &slice,
                &layout,
                &visible_rows,
                &visible_cols,
                None,
                None,
                ram,
            )
        );

        // Visible CPU change invalidates the fingerprint.
        slice.rows[1].cpu_permille = 150;
        let fp_cpu = compute_details_viewport_fingerprint(
            &slice,
            &layout,
            &visible_rows,
            &visible_cols,
            None,
            None,
            ram,
        );
        assert_ne!(fp_base, fp_cpu);

        // Loading transition (revision == 0 vs > 0) always changes the fingerprint.
        let mut loading_slice = slice.clone();
        loading_slice.revision = 0;
        assert_ne!(
            fp_cpu,
            compute_details_viewport_fingerprint(
                &loading_slice,
                &layout,
                &visible_rows,
                &visible_cols,
                None,
                None,
                ram,
            )
        );
    }

    #[gpui_kit::test]
    fn details_view_right_click_column_coord_long_paths_and_viewport_dirty_skip(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::{
            AppContext as _, Modifiers, MouseButton, VisualTestContext, point, px, size,
        };
        use loom_config::{AppSettings, ConfigStore};
        use loom_engine::EngineCoordinator;
        use loom_pal::{NativeBackend, PlatformBackend as _};
        use loom_ui_kit::DEFAULT_CELL_PREVIEW_CHARS;

        use crate::bridge::UiStore;
        use crate::shell::ShellHandle;

        cx.update(gpui_kit::init);

        let dir = tempfile::tempdir().expect("tempdir");
        let config_store = ConfigStore::new(dir.path().join("settings.toml"));
        let backend = NativeBackend::initialize().expect("native backend");
        let coordinator =
            Arc::new(EngineCoordinator::new(backend, AppSettings::default()).expect("coordinator"));
        let coordinator_for_shutdown = Arc::clone(&coordinator);

        let store =
            cx.new(|cx| UiStore::new(coordinator, config_store, AppSettings::default(), cx));
        let shell = ShellHandle::new(|_, _, _| {});

        let store_for_window = store.clone();
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), move |window, cx| {
                let view = cx.new(|cx| DetailsView::new(store_for_window, None, shell, window, cx));
                cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
            })
            .expect("open window")
        });
        cx.simulate_window_resize(window.into(), size(px(1200.), px(720.)));
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.update(|window, cx| window.draw(cx).clear(cx));

        let view = window
            .read_with(&vcx, |root, _| {
                root.view()
                    .clone()
                    .downcast::<DetailsView>()
                    .expect("root wraps DetailsView")
            })
            .expect("read root");
        let table = view.read_with(&vcx, |v, _| v.table.clone());

        // Stop the live OS sampler so background ticks never overwrite synthetic test slices.
        coordinator_for_shutdown.shutdown();
        vcx.run_until_parked();

        let long_path: Arc<str> = Arc::from(format!(
            "C:\\Program Files\\{}\\{}\\app.exe",
            "deepdir\\".repeat(500),
            "segment".repeat(500)
        ));
        let long_cmd: Arc<str> = Arc::from(format!("\"{}\" --arg={}", long_path, "z".repeat(4096)));

        let make_rows = |visible_cpu: u16, offscreen_cpu: u16| -> Vec<DetailsRowProjection> {
            (0..60u32)
                .map(|i| {
                    let mut r = sample_row(3000 + i, 80_000 + u64::from(i), &format!("d-{i}.exe"));
                    r.user_name = Some(Arc::from(r"WORKSTATION\Alice"));
                    r.private_working_set_bytes = 128 * 1024 * 1024;
                    r.working_set_bytes = 256 * 1024 * 1024;
                    r.commit_bytes = 512 * 1024 * 1024;
                    r.cpu_permille = if i == 0 {
                        visible_cpu
                    } else if i == 55 {
                        offscreen_cpu
                    } else {
                        5
                    };
                    if i == 0 {
                        r.exe_path = Some(Arc::clone(&long_path));
                        r.command_line = Some(Arc::clone(&long_cmd));
                    }
                    r
                })
                .collect()
        };

        let platform = store.read_with(&vcx, |s, _| s.snapshot().platform);
        let slice_v1 = Arc::new(DetailsTableSlice {
            revision: 10,
            rows: make_rows(15, 15),
            ..Default::default()
        });
        let ram = 16 * 1024 * 1024 * 1024;
        let notified_v1 = view.update(&mut vcx, |this, cx| {
            this.commit_snapshot(
                10,
                Arc::clone(&slice_v1),
                CapabilityFlags::empty(),
                platform,
                ram,
                cx,
            )
        });
        assert!(notified_v1, "initial commit must notify TableState");
        vcx.update(|window, cx| window.draw(cx).clear(cx));

        // 1. Table-level right-click column hit-testing:
        // Default visible columns: Name [0..240), Pid [240..316), Status [316..412), User [412..552).
        for (x_pos, expected_col) in [(120.0, Some(0)), (260.0, Some(1)), (350.0, Some(2))] {
            vcx.simulate_mouse_down(
                point(px(x_pos), px(100.)),
                MouseButton::Right,
                Modifiers::default(),
            );
            vcx.simulate_mouse_up(
                point(px(x_pos), px(100.)),
                MouseButton::Right,
                Modifiers::default(),
            );
            let detected = table.read_with(&vcx, |t, _| t.delegate().last_right_clicked_col);
            assert_eq!(
                detected, expected_col,
                "right-click at x={x_pos} should map to column {expected_col:?}"
            );
            // Dismiss the opened context menu overlay so the next right-click hits the table.
            vcx.simulate_keystrokes("escape");
            vcx.update(|window, cx| window.draw(cx).clear(cx));
        }

        // 2. Long ExePath & CommandLine are capped in RowTextCache previews while Copy & CSV retain full text.
        let key0 = slice_v1.rows[0].key;
        let (path_preview, cmd_preview) = table.update(&mut vcx, |t, cx| {
            let previews = {
                let d = t.delegate_mut();
                d.selected_key = Some(key0);
                d.column_layout
                    .apply_visible_slugs(&["pid", "cpu", "exe_path", "command_line"]);
                d.invalidate_viewport_fingerprint();
                let p = d
                    .row_cache
                    .preview_slot(key0, 0, &long_path, DEFAULT_CELL_PREVIEW_CHARS);
                let c = d
                    .row_cache
                    .preview_slot(key0, 1, &long_cmd, DEFAULT_CELL_PREVIEW_CHARS);
                (p, c)
            };
            t.refresh(cx);
            previews
        });
        assert!(path_preview.chars().count() <= DEFAULT_CELL_PREVIEW_CHARS + 1);
        assert!(path_preview.ends_with('…'));
        assert!(cmd_preview.chars().count() <= DEFAULT_CELL_PREVIEW_CHARS + 1);
        assert!(cmd_preview.ends_with('…'));

        let (summary_text, csv_rows) = table.read_with(&vcx, |t, _| {
            (
                t.delegate().selected_copy_summary().expect("row 0 summary"),
                t.delegate().export_visible_csv_payload().1,
            )
        });
        assert!(summary_text.contains(long_path.as_ref()));
        assert!(summary_text.contains(long_cmd.as_ref()));
        assert!(
            csv_rows[0]
                .iter()
                .any(|cell| cell.as_str() == long_path.as_ref())
        );
        assert!(
            csv_rows[0]
                .iter()
                .any(|cell| cell.as_str() == long_cmd.as_ref())
        );

        vcx.update(|window, cx| window.draw(cx).clear(cx));

        // 3. Off-screen change skips TableState notification.
        let slice_v2 = Arc::new(DetailsTableSlice {
            revision: 11,
            rows: make_rows(15, 777),
            ..Default::default()
        });
        let notified_v2 = view.update(&mut vcx, |this, cx| {
            this.commit_snapshot(11, slice_v2, CapabilityFlags::empty(), platform, ram, cx)
        });
        assert!(
            !notified_v2,
            "off-screen Details row change must skip TableState cx.notify()"
        );

        // 4. Visible row change triggers TableState notification.
        let slice_v3 = Arc::new(DetailsTableSlice {
            revision: 12,
            rows: make_rows(320, 777),
            ..Default::default()
        });
        let notified_v3 = view.update(&mut vcx, |this, cx| {
            this.commit_snapshot(12, slice_v3, CapabilityFlags::empty(), platform, ram, cx)
        });
        assert!(
            notified_v3,
            "visible Details row CPU change must notify TableState"
        );

        coordinator_for_shutdown.shutdown();
    }

    #[test]
    fn details_locale_cache_and_working_set_delta_column_alignment() {
        let all_cols = [
            (DetailsColumn::Name, "details.col_name"),
            (DetailsColumn::Pid, "details.col_pid"),
            (DetailsColumn::Status, "details.col_status"),
            (DetailsColumn::UserName, "details.col_user"),
            (DetailsColumn::SessionId, "details.col_session_id"),
            (DetailsColumn::Cpu, "details.col_cpu"),
            (DetailsColumn::CpuTime, "details.col_cpu_time"),
            (DetailsColumn::Memory, "details.col_memory"),
            (DetailsColumn::WorkingSet, "details.col_working_set"),
            (
                DetailsColumn::WorkingSetDelta,
                "details.col_working_set_delta",
            ),
            (DetailsColumn::CommitSize, "details.col_commit"),
            (DetailsColumn::Priority, "details.col_priority"),
            (DetailsColumn::Handles, "details.col_handles"),
            (DetailsColumn::Threads, "details.col_threads"),
            (DetailsColumn::Platform, "details.col_platform"),
            (DetailsColumn::UacVirtualization, "details.col_uac"),
            (DetailsColumn::Elevated, "details.col_elevated"),
            (DetailsColumn::GpuMemory, "details.col_gpu_memory"),
            (DetailsColumn::Publisher, "details.col_publisher"),
            (DetailsColumn::Description, "details.col_description"),
            (DetailsColumn::ExePath, "details.col_exe_path"),
            (DetailsColumn::CommandLine, "details.col_command_line"),
        ];
        let cache = DetailsLocaleCache::build(PlatformProfile::WINDOWS);
        assert_eq!(cache.col_labels.len(), all_cols.len());
        for (idx, (col, key)) in all_cols.into_iter().enumerate() {
            assert_eq!(
                col as usize, idx,
                "DetailsColumn::{col:?} discriminant must match slot {idx}"
            );
            let cached = cache.column_label(col);
            let en = rust_i18n::t!(key, locale = "en-US");
            let zh = rust_i18n::t!(key, locale = "zh-CN");
            assert!(
                cached.as_ref() == en.as_ref() || cached.as_ref() == zh.as_ref(),
                "DetailsLocaleCache slot for {col:?} returned {cached:?}, expected {en:?} or {zh:?}"
            );
        }

        assert_eq!(
            rust_i18n::t!("details.col_memory", locale = "en-US").as_ref(),
            "Memory (private working set)"
        );
        assert_eq!(
            rust_i18n::t!("details.col_working_set_delta", locale = "en-US").as_ref(),
            "Working set delta (memory)"
        );
        assert_eq!(
            rust_i18n::t!("details.col_memory", locale = "zh-CN").as_ref(),
            "内存（专用工作集）"
        );
        assert_eq!(
            rust_i18n::t!("details.col_working_set_delta", locale = "zh-CN").as_ref(),
            "工作集增量（内存）"
        );

        let mut layout =
            TableColumnLayout::new(DetailsColumn::default_specs(CapabilityFlags::empty()));
        assert!(layout.is_visible(DetailsColumn::Memory));
        assert!(!layout.is_visible(DetailsColumn::WorkingSetDelta));
        assert!(
            DetailsColumn::selectable_order(CapabilityFlags::empty())
                .contains(&DetailsColumn::WorkingSetDelta)
        );
        assert!(DetailsColumn::WorkingSetDelta.right_aligned());
        assert_eq!(
            DetailsColumn::WorkingSetDelta.sort_column(),
            Some(DetailsSortColumn::WorkingSetDelta)
        );
        assert_eq!(
            default_sort_direction(DetailsSortColumn::WorkingSetDelta),
            SortDirection::Descending
        );

        // Make WorkingSetDelta visible and verify fingerprint detects sign & bucket changes.
        assert!(layout.apply_visible_slugs(&[DetailsColumn::WorkingSetDelta.slug()]));
        assert!(layout.is_visible(DetailsColumn::WorkingSetDelta));
        let ram = 16 * 1024 * 1024 * 1024u64;
        let mut slice = DetailsTableSlice {
            revision: 1,
            rows: vec![sample_row(1000, 1, "worker.exe")],
            ..Default::default()
        };
        let visible_rows = 0..1;
        let visible_cols = 0..layout.visible_count();

        let fp_zero = compute_details_viewport_fingerprint(
            &slice,
            &layout,
            &visible_rows,
            &visible_cols,
            None,
            None,
            ram,
        );
        slice.rows[0].working_set_delta_bytes = 2 * 1024 * 1024;
        let fp_pos = compute_details_viewport_fingerprint(
            &slice,
            &layout,
            &visible_rows,
            &visible_cols,
            None,
            None,
            ram,
        );
        assert_ne!(fp_zero, fp_pos);

        // Sub-tenth jitter (+64 B on +2.0 MB) keeps identical fingerprint.
        slice.rows[0].working_set_delta_bytes += 64;
        let fp_pos_jitter = compute_details_viewport_fingerprint(
            &slice,
            &layout,
            &visible_rows,
            &visible_cols,
            None,
            None,
            ram,
        );
        assert_eq!(fp_pos, fp_pos_jitter);

        // Flipping to negative -2.0 MB must change the fingerprint.
        slice.rows[0].working_set_delta_bytes = -(2 * 1024 * 1024);
        let fp_neg = compute_details_viewport_fingerprint(
            &slice,
            &layout,
            &visible_rows,
            &visible_cols,
            None,
            None,
            ram,
        );
        assert_ne!(fp_pos, fp_neg);
    }
}
