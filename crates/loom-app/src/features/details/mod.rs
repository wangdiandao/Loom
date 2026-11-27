//! On-demand Details vertical feature slice gated by an RAII [`ScopedMonitorLease`].
//!
//! Splits the static toolbar + highlight banner ([`DetailsView`]) from the
//! high-frequency 2D-virtualized `gpui-kit` `DataTable` (`Entity<TableState<DetailsTableDelegate>>`)
//! so live snapshot ticks update only the table entity while `gpui-fast` reuses
//! the surrounding shell and toolbar as retained subtrees.

mod affinity_dialog;
mod wait_chain_dialog;

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::assets::IconName as AssetIconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableEvent, TableState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, Size, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Empty, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, KeyDownEvent, MouseButton, ParentElement as _, Render, ScrollStrategy,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window,
    div, img, prelude::FluentBuilder as _, px,
};
use loom_types::{
    CapabilityFlags, DetailsRowProjection, DetailsSortColumn, DetailsTableSlice, DetailsViewMode,
    LanguagePreference, MonitorDomain, PlatformProfile, ProcessArch, ProcessKey, ProcessPriority,
    ProcessStatusFlags, SortDirection, UacVirtualization,
};
use loom_ui_kit::{
    HeatmapLevel, LoomSemanticColors, format_bytes_shared, format_cpu_time_shared,
    format_permille_shared, heatmap_cell,
};

use crate::bridge::{ScopedMonitorLease, UiCommands, UiStore, UiStoreEvent};
use crate::features::table_columns::{
    ColumnKey, ColumnSpec, ColumnToggleItem, DataTablePane, HeaderMenuAction, IconRenderCache,
    TableColumnLayout, build_column_header_menu, open_select_columns_dialog,
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
const COL_MEM_W: f32 = 168.0;
const COL_WORKING_SET_W: f32 = 128.0;
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
        }
    }
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

/// Cached localized strings for [`DetailsTableDelegate`] to avoid per-frame `i18n::tr` allocations.
struct DetailsLocaleCache {
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
}

impl DetailsLocaleCache {
    fn build() -> Self {
        Self {
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
    highlighted_key: Option<ProcessKey>,
    pending_scroll_key: Option<ProcessKey>,
    column_layout: TableColumnLayout<DetailsColumn>,
    icon_cache: IconRenderCache,
    locale: DetailsLocaleCache,
}

impl DetailsTableDelegate {
    fn new(
        store: Entity<UiStore>,
        highlighted_key: Option<ProcessKey>,
        shell_handle: ShellHandle,
        cx: &App,
    ) -> Self {
        let store_ref = store.read(cx);
        let snap = store_ref.snapshot();
        let capabilities = snap.capabilities;
        let platform = snap.platform;
        let total_ram_bytes = snap.performance.latest.mem_total_bytes;

        let mut column_layout = TableColumnLayout::new(DetailsColumn::default_specs(capabilities));
        let saved_cols = store_ref.details_columns();
        if !saved_cols.is_empty() {
            column_layout.apply_saved(saved_cols);
        }

        Self {
            store: store.clone(),
            shell_handle,
            details: Arc::clone(&snap.details),
            capabilities,
            platform,
            total_ram_bytes,
            last_rev: snap.revisions.details,
            selected_key: highlighted_key,
            highlighted_key,
            pending_scroll_key: highlighted_key,
            column_layout,
            icon_cache: IconRenderCache::new(),
            locale: DetailsLocaleCache::build(),
        }
    }

    fn persist_column_layout(&self, cx: &mut App) {
        let snapshot = self.column_layout.snapshot();
        UiCommands::set_details_columns(&self.store, snapshot, cx);
    }

    fn trigger_sort_col(&self, col_ix: usize, cx: &mut App) {
        let Some(spec) = self.column_layout.visible_column(col_ix) else {
            return;
        };
        let Some(target) = spec.id.sort_column() else {
            return;
        };
        let next_dir = next_sort_direction(
            self.details.sort_column,
            self.details.sort_direction,
            target,
        );
        UiCommands::set_details_sort(&self.store, target, next_dir, cx);
    }

    fn hide_column(&mut self, col: DetailsColumn, cx: &mut App) -> bool {
        if self.column_layout.hide_column(col) {
            self.persist_column_layout(cx);
            true
        } else {
            false
        }
    }

    fn reset_columns(&mut self, cx: &mut App) {
        self.column_layout
            .reset(DetailsColumn::default_specs(self.capabilities));
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
        let mut column = Column::new(col.slug(), col.label(self.platform))
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
            self.persist_column_layout(cx);
        }
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
        let fg = cx.theme().foreground;
        let muted_fg = cx.theme().muted_foreground;
        let table_weak = cx.entity().downgrade();

        h_flex()
            .id(("details-th", col_ix))
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .when(col.is_heatmap(), |d| d.px_2())
            .when(col.right_aligned(), |d| d.justify_end())
            .text_xs()
            .text_color(if is_active_sort { fg } else { muted_fg })
            .when(is_active_sort, |d| d.font_semibold())
            .cursor_pointer()
            .on_click(cx.listener(move |table, _, _, cx| {
                table.delegate().trigger_sort_col(col_ix, cx);
            }))
            .child(div().min_w_0().truncate().child(col.label(self.platform)))
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

                h_flex()
                    .size_full()
                    .items_center()
                    .gap_1p5()
                    .pl(px(indent))
                    .overflow_hidden()
                    .when(is_tree_mode, |el| {
                        el.child(if has_children {
                            div()
                                .flex_shrink_0()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation();
                                })
                                .child(
                                    Button::new(("details-expand", row_id))
                                        .xsmall()
                                        .ghost()
                                        .icon(if is_expanded {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronRight
                                        })
                                        .on_click(move |_, _, cx| {
                                            UiCommands::toggle_details_expanded(
                                                &toggle_store,
                                                key,
                                                cx,
                                            );
                                        }),
                                )
                                .into_any_element()
                        } else {
                            div().w(px(18.)).flex_shrink_0().into_any_element()
                        })
                    })
                    .child(if let Some(image) = icon_image {
                        img(image).size(px(18.)).flex_shrink_0().into_any_element()
                    } else {
                        h_flex()
                            .size(px(18.))
                            .flex_shrink_0()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(IconName::File).xsmall().text_color(muted_fg))
                            .into_any_element()
                    })
                    .child(div().flex_1().min_w_0().truncate().text_xs().child(name))
                    .into_any_element()
            }
            DetailsColumn::Pid => h_flex()
                .size_full()
                .items_center()
                .justify_end()
                .text_xs()
                .font_family(theme.mono_font_family.clone())
                .text_color(muted_fg)
                .child(key.pid.to_string())
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
                    .overflow_hidden()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(muted_fg)
                            .child(status_text),
                    )
                    .into_any_element()
            }
            DetailsColumn::UserName => {
                let user_text = display_user_name(row.user_name.as_deref()).to_owned();
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
                .child(row.session_id.to_string())
                .into_any_element(),
            DetailsColumn::Cpu => heatmap_cell(
                format_permille_shared(row.cpu_permille),
                HeatmapLevel::from_permille(row.cpu_permille),
                &LoomSemanticColors::from_theme(theme),
            )
            .into_any_element(),
            DetailsColumn::CpuTime => h_flex()
                .size_full()
                .items_center()
                .justify_end()
                .text_xs()
                .font_family(theme.mono_font_family.clone())
                .text_color(muted_fg)
                .child(format_cpu_time_shared(row.cpu_time_100ns))
                .into_any_element(),
            DetailsColumn::Memory => heatmap_cell(
                format_bytes_shared(row.private_working_set_bytes),
                memory_heatmap_level(row.private_working_set_bytes, self.total_ram_bytes),
                &LoomSemanticColors::from_theme(theme),
            )
            .into_any_element(),
            DetailsColumn::WorkingSet => h_flex()
                .size_full()
                .items_center()
                .justify_end()
                .text_xs()
                .font_family(theme.mono_font_family.clone())
                .text_color(muted_fg)
                .child(format_bytes_shared(row.working_set_bytes))
                .into_any_element(),
            DetailsColumn::CommitSize => h_flex()
                .size_full()
                .items_center()
                .justify_end()
                .text_xs()
                .font_family(theme.mono_font_family.clone())
                .text_color(muted_fg)
                .child(format_bytes_shared(row.commit_bytes))
                .into_any_element(),
            DetailsColumn::Priority => h_flex()
                .size_full()
                .items_center()
                .overflow_hidden()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(muted_fg)
                        .child(self.locale.priority_label(row.priority)),
                )
                .into_any_element(),
            DetailsColumn::Handles => h_flex()
                .size_full()
                .items_center()
                .justify_end()
                .text_xs()
                .font_family(theme.mono_font_family.clone())
                .text_color(muted_fg)
                .child(row.handle_count.to_string())
                .into_any_element(),
            DetailsColumn::Threads => h_flex()
                .size_full()
                .items_center()
                .justify_end()
                .text_xs()
                .font_family(theme.mono_font_family.clone())
                .text_color(muted_fg)
                .child(row.thread_count.to_string())
                .into_any_element(),
            DetailsColumn::Platform => h_flex()
                .size_full()
                .items_center()
                .overflow_hidden()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(muted_fg)
                        .child(self.locale.arch_label(row.arch)),
                )
                .into_any_element(),
            DetailsColumn::UacVirtualization => h_flex()
                .size_full()
                .items_center()
                .overflow_hidden()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(muted_fg)
                        .child(self.locale.uac_label(row.uac_virtualization)),
                )
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
            DetailsColumn::GpuMemory => h_flex()
                .size_full()
                .items_center()
                .justify_end()
                .text_xs()
                .font_family(theme.mono_font_family.clone())
                .text_color(muted_fg)
                .child(format_bytes_shared(row.gpu_dedicated_bytes))
                .into_any_element(),
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
                let path_arc = row
                    .exe_path
                    .as_ref()
                    .filter(|s| !s.is_empty())
                    .map(Arc::clone);
                let text = path_arc
                    .as_ref()
                    .map(|s| SharedString::from(Arc::clone(s)))
                    .unwrap_or_else(|| SharedString::new_static("—"));

                h_flex()
                    .id(("details-exe", row_id))
                    .size_full()
                    .items_center()
                    .overflow_hidden()
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .text_color(muted_fg)
                    .when_some(path_arc, |el, full_path| {
                        let tip = SharedString::from(full_path);
                        el.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    })
                    .child(div().min_w_0().truncate().child(text))
                    .into_any_element()
            }
            DetailsColumn::CommandLine => {
                let cmd_arc = row
                    .command_line
                    .as_ref()
                    .filter(|s| !s.is_empty())
                    .map(Arc::clone);
                let text = cmd_arc
                    .as_ref()
                    .map(|s| SharedString::from(Arc::clone(s)))
                    .unwrap_or_else(|| SharedString::new_static("—"));

                h_flex()
                    .id(("details-cmd", row_id))
                    .size_full()
                    .items_center()
                    .overflow_hidden()
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .text_color(muted_fg)
                    .when_some(cmd_arc, |el, full_cmd| {
                        let tip = SharedString::from(full_cmd);
                        el.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    })
                    .child(div().min_w_0().truncate().child(text))
                    .into_any_element()
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
        let Some(row) = self.details.rows.get(row_ix) else {
            return menu;
        };
        let target = ProcessActionTarget::from_row(row);
        build_row_context_menu(
            menu,
            &self.store,
            &self.shell_handle,
            &target,
            self.capabilities,
            window,
            cx,
        )
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

/// Live full-field Details table view holding an RAII `MonitorDomain::Details` lease.
///
/// Splits the static toolbar/banner from the 2D-virtualized `DataTable` entity so
/// live snapshot updates refresh only `self.table` without re-rendering the outer shell.
pub(crate) struct DetailsView {
    store: Entity<UiStore>,
    _details_lease: ScopedMonitorLease,
    table: Entity<TableState<DetailsTableDelegate>>,
    table_pane: Entity<DataTablePane<DetailsTableDelegate>>,
    view_mode: DetailsViewMode,
    highlighted_key: Option<ProcessKey>,
    has_selection: bool,
    last_locale: LanguagePreference,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for DetailsView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.table.focus_handle(cx)
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
        let (view_mode, last_locale) = {
            let s = store.read(cx);
            (s.snapshot().details.view_mode, s.settings().language)
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
                    let current_rev = table_for_store.read(cx).delegate().last_rev;
                    if revs.details == current_rev {
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
                    let (next_view_mode, next_has_sel) = table_for_store.update(cx, |table, cx| {
                        let d = table.delegate_mut();
                        let sort_changed = d.details.sort_column != next_details.sort_column
                            || d.details.sort_direction != next_details.sort_direction;
                        d.details = next_details;
                        d.capabilities = next_caps;
                        d.platform = next_platform;
                        d.total_ram_bytes = next_ram;
                        d.last_rev = revs.details;

                        let view_mode = d.details.view_mode;
                        let selected_idx = d
                            .selected_key
                            .and_then(|sel| find_row_index_by_key(&d.details.rows, sel));

                        if let Some(idx) = selected_idx {
                            if table.selected_row() != Some(idx) {
                                table.set_selected_row(idx, cx);
                            }
                        } else if table.selected_row().is_some() {
                            table.clear_selection(cx);
                        }

                        Self::try_scroll_to_pending_in_table(table, cx);
                        if sort_changed {
                            table.refresh(cx);
                        }
                        cx.notify();
                        (view_mode, selected_idx.is_some())
                    });

                    if this.view_mode != next_view_mode || this.has_selection != next_has_sel {
                        this.view_mode = next_view_mode;
                        this.has_selection = next_has_sel;
                        cx.notify();
                    }
                }
                UiStoreEvent::SettingsChanged => {
                    let next_locale = store.read(cx).settings().language;
                    if this.last_locale != next_locale {
                        this.last_locale = next_locale;
                        table_for_store.update(cx, |table, cx| {
                            table.delegate_mut().locale = DetailsLocaleCache::build();
                            table.refresh(cx);
                            cx.notify();
                        });
                        this.table_pane.update(cx, |pane, cx| {
                            pane.set_accessibility_label(i18n::tr("details.title"), cx);
                        });
                        cx.notify();
                    }
                }
                UiStoreEvent::SearchQueryChanged
                | UiStoreEvent::ElevationRequired { .. }
                | UiStoreEvent::CommandFailed { .. } => {}
            }),
        );

        // Subscribe to DataTable events: ProcessKey selection & column width persistence.
        subscriptions.push(cx.subscribe(
            &table,
            |this, table, event: &TableEvent, cx| match event {
                TableEvent::SelectRow(row_ix) => {
                    let next_has_sel = table.update(cx, |table, _| {
                        let d = table.delegate_mut();
                        d.selected_key = d.details.rows.get(*row_ix).map(|r| r.key);
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
                        table.delegate_mut().selected_key = key_opt;
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
                    table.update(cx, |table, _| {
                        table.delegate_mut().selected_key = None;
                    });
                    if this.has_selection {
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
                            table.delegate().persist_column_layout(cx);
                        }
                    });
                }
                _ => {}
            },
        ));

        Self {
            store,
            _details_lease: lease,
            table,
            table_pane,
            view_mode,
            highlighted_key,
            has_selection,
            last_locale,
            _subscriptions: subscriptions,
        }
    }

    fn try_scroll_to_pending_in_table(
        table: &mut TableState<DetailsTableDelegate>,
        cx: &mut Context<TableState<DetailsTableDelegate>>,
    ) {
        let Some(target) = table.delegate().pending_scroll_key else {
            return;
        };
        let found = find_row_index_by_key(&table.delegate().details.rows, target);
        if let Some(idx) = found {
            table
                .vertical_scroll_handle
                .scroll_to_item(idx, ScrollStrategy::Center);
            table.set_selected_row(idx, cx);
            table.delegate_mut().pending_scroll_key = None;
        }
    }

    /// Updates the highlighted target [`ProcessKey`] (e.g., from `NavigationIntent::JumpToDetails`).
    pub(crate) fn set_highlighted_key(&mut self, key: Option<ProcessKey>, cx: &mut Context<Self>) {
        self.highlighted_key = key;
        let next_has_sel = self.table.update(cx, |table, cx| {
            let d = table.delegate_mut();
            d.highlighted_key = key;
            if let Some(target) = key {
                d.selected_key = Some(target);
                d.pending_scroll_key = Some(target);
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
}

impl Render for DetailsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        debug_assert_eq!(self._details_lease.domain(), MonitorDomain::Details);

        let view_mode = self.view_mode;
        let highlighted_key = self.highlighted_key;
        let has_selection = self.has_selection;

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
                    ),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
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

        v_flex()
            .id("details-view-root")
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let modifiers = &event.keystroke.modifiers;
                if !modifiers.control
                    && !modifiers.alt
                    && !modifiers.platform
                    && !modifiers.function
                    && !modifiers.shift
                    && event.keystroke.key == "delete"
                    && this.has_selection
                {
                    this.trigger_end_task_for_selected(window, cx);
                    cx.stop_propagation();
                }
            }))
            .size_full()
            .min_h_0()
            .child(toolbar)
            .when_some(highlight_banner, |this, banner| this.child(banner))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.table_pane.clone()),
            )
    }
}

/// Builds the multi-level right-click [`PopupMenu`] anchored strictly to `target.key`.
fn build_row_context_menu(
    menu: PopupMenu,
    store: &Entity<UiStore>,
    shell_handle: &ShellHandle,
    target: &ProcessActionTarget,
    capabilities: CapabilityFlags,
    window: &mut Window,
    cx: &mut Context<TableState<DetailsTableDelegate>>,
) -> PopupMenu {
    let key = target.key;
    let current_priority = target.priority;
    let is_kernel_or_idle = target
        .status
        .intersects(ProcessStatusFlags::SYSTEM_IDLE | ProcessStatusFlags::KERNEL);
    let eco_supported = capabilities.contains(CapabilityFlags::ECO_MODE);
    let wait_chain_supported = capabilities.contains(CapabilityFlags::WAIT_CHAIN);
    let minidump_supported = capabilities.contains(CapabilityFlags::MINIDUMP);
    let uac_supported = capabilities.contains(CapabilityFlags::UAC_VIRTUALIZATION);
    let debugger_supported = capabilities.contains(CapabilityFlags::DEBUGGER_ATTACH);
    let eco_enabled = target.status.contains(ProcessStatusFlags::EFFICIENCY_MODE);
    let eco_eligible = is_eco_mode_eligible(target.status, capabilities);
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
        )
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

    menu.separator()
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
        )
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
}
