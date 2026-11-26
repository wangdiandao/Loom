//! On-demand Details vertical feature slice gated by an RAII [`ScopedMonitorLease`].
//!
//! Acquires `MonitorDomain::Details` upon construction and automatically releases it
//! when unmounted so hidden views incur zero projection overhead. Provides the full-field
//! sortable Flat/Tree process table, `ProcessKey`-anchored selection & `Delete` hotkey,
//! cross-page `JumpToDetails(ProcessKey)` scroll-to-row & highlight, multi-level right-click
//! context menu, and feature-local **Set Affinity** & confirmation dialogs.

mod affinity_dialog;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::assets::IconName as AssetIconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement, KeyDownEvent,
    MouseButton, ParentElement as _, Render, RenderImage, ScrollHandle, ScrollStrategy,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    UniformListScrollHandle, WeakEntity, Window, div, img, prelude::FluentBuilder as _, px,
    uniform_list,
};
use loom_types::{
    CapabilityFlags, DetailsRowProjection, DetailsSortColumn, DetailsViewMode, MonitorDomain,
    PlatformProfile, ProcessKey, ProcessPriority, ProcessStatusFlags, SortDirection,
};
use loom_ui_kit::{
    HeatmapLevel, LoomSemanticColors, format_bytes, format_cpu_time, format_permille, heatmap_cell,
    rgba_icon_to_render_image,
};

use crate::bridge::{ScopedMonitorLease, UiCommands, UiStore, UiStoreEvent};
use crate::features::table_columns::{
    ColumnKey, ColumnSpec, ColumnToggleItem, HeaderColumnDescriptor, HeaderMenuAction,
    TableColumnLayout, attach_header_reorder_handlers, attach_table_column_drag_handlers,
    build_column_header_menu, open_select_columns_dialog, render_header_trailing_spacer,
    render_interactive_header_cell,
};
use crate::i18n;

const DETAILS_ROW_HEIGHT: f32 = 30.0;
const ICON_CACHE_MAX_ENTRIES: usize = 512;
const TABLE_HORIZONTAL_PADDING: f32 = 16.0;

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
}

/// Total minimum default width of the Details table, adapting to advertised [`CapabilityFlags`].
#[cfg(test)]
pub(super) fn table_min_width(capabilities: CapabilityFlags) -> f32 {
    TableColumnLayout::new(DetailsColumn::default_specs(capabilities))
        .total_width(TABLE_HORIZONTAL_PADDING)
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

type IconCache = Rc<RefCell<HashMap<Arc<str>, Option<Arc<RenderImage>>>>>;

/// Snapshot of a process row needed by dialogs and right-click context menu handlers.
#[derive(Debug, Clone)]
struct ProcessActionTarget {
    key: ProcessKey,
    name: Arc<str>,
    status: ProcessStatusFlags,
    priority: ProcessPriority,
    exe_path: Option<Arc<str>>,
}

impl ProcessActionTarget {
    fn from_row(row: &DetailsRowProjection) -> Self {
        Self {
            key: row.key,
            name: Arc::clone(&row.name),
            status: row.status,
            priority: row.priority,
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

/// Resolves or lazily caches a `20×20` GPUI [`RenderImage`] from a row's RGBA icon.
fn resolve_cached_icon(cache: &IconCache, row: &DetailsRowProjection) -> Option<Arc<RenderImage>> {
    let rgba = row.icon.as_ref()?;
    let cache_key = row
        .exe_path
        .as_ref()
        .map(Arc::clone)
        .unwrap_or_else(|| Arc::clone(&row.name));

    let mut guard = cache.borrow_mut();
    if let Some(existing) = guard.get(&cache_key) {
        return existing.clone();
    }
    if guard.len() >= ICON_CACHE_MAX_ENTRIES {
        guard.clear();
    }
    let converted = rgba_icon_to_render_image(rgba);
    guard.insert(cache_key, converted.clone());
    converted
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

/// Live full-field Details table view holding an RAII `MonitorDomain::Details` lease.
pub(crate) struct DetailsView {
    store: Entity<UiStore>,
    _details_lease: ScopedMonitorLease,
    last_details_rev: u64,
    selected_key: Option<ProcessKey>,
    highlighted_key: Option<ProcessKey>,
    pending_scroll_key: Option<ProcessKey>,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    h_scroll_handle: ScrollHandle,
    column_layout: TableColumnLayout<DetailsColumn>,
    icon_cache: IconCache,
    _subscriptions: Vec<Subscription>,
}

impl DetailsView {
    /// Creates a new [`DetailsView`], acquiring `MonitorDomain::Details` immediately.
    pub(crate) fn new(
        store: Entity<UiStore>,
        highlighted_key: Option<ProcessKey>,
        cx: &mut Context<Self>,
    ) -> Self {
        let lease = store.read(cx).acquire_lease(MonitorDomain::Details);
        let (initial_rev, capabilities) = {
            let snap = store.read(cx).snapshot();
            (snap.revisions.details, snap.capabilities)
        };
        let mut column_layout = TableColumnLayout::new(DetailsColumn::default_specs(capabilities));
        let saved_cols = store.read(cx).details_columns().to_vec();
        if !saved_cols.is_empty() {
            column_layout.apply_saved(&saved_cols);
        }
        let sub = cx.subscribe(&store, |this, _store, event, cx| match event {
            UiStoreEvent::SnapshotUpdated(revs) => {
                if revs.details != this.last_details_rev {
                    this.last_details_rev = revs.details;
                    this.try_scroll_to_pending(cx);
                    cx.notify();
                }
            }
            UiStoreEvent::SettingsChanged => cx.notify(),
            UiStoreEvent::ElevationRequired { .. } | UiStoreEvent::CommandFailed { .. } => {}
        });

        let mut view = Self {
            store,
            _details_lease: lease,
            last_details_rev: initial_rev,
            selected_key: highlighted_key,
            highlighted_key,
            pending_scroll_key: highlighted_key,
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            h_scroll_handle: ScrollHandle::new(),
            column_layout,
            icon_cache: Rc::new(RefCell::new(HashMap::new())),
            _subscriptions: vec![sub],
        };
        view.try_scroll_to_pending(cx);
        view
    }

    fn persist_column_layout(&self, cx: &mut Context<Self>) {
        let snapshot = self.column_layout.snapshot();
        UiCommands::set_details_columns(&self.store, snapshot, cx);
    }

    fn hide_column(&mut self, col: DetailsColumn, cx: &mut Context<Self>) {
        if self.column_layout.hide_column(col) {
            self.persist_column_layout(cx);
            cx.notify();
        }
    }

    fn reset_columns_to_default(&mut self, cx: &mut Context<Self>) {
        let capabilities = self.store.read(cx).snapshot().capabilities;
        self.column_layout
            .reset(DetailsColumn::default_specs(capabilities));
        UiCommands::set_details_columns(&self.store, Vec::new(), cx);
        cx.notify();
    }

    fn open_select_columns_modal(&self, window: &mut Window, cx: &mut Context<Self>) {
        let (capabilities, platform) = {
            let snap = self.store.read(cx).snapshot();
            (snap.capabilities, snap.platform)
        };
        let items = DetailsColumn::selectable_order(capabilities)
            .into_iter()
            .map(|col| ColumnToggleItem {
                slug: col.slug(),
                label: col.label(platform),
                visible: self.column_layout.is_visible(col),
            })
            .collect();
        let view_weak = cx.entity().downgrade();
        open_select_columns_dialog(
            items,
            Rc::new(move |selected_slugs, cx| {
                let _ = view_weak.update(cx, |this, cx| {
                    if this.column_layout.apply_visible_slugs(&selected_slugs) {
                        this.persist_column_layout(cx);
                        cx.notify();
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
        view_weak: WeakEntity<Self>,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let hide_action: Option<HeaderMenuAction> = clicked_col
            .filter(|&col| col != DetailsColumn::Name)
            .map(|col| {
                let view_hide = view_weak.clone();
                Rc::new(move |_window: &mut Window, cx: &mut App| {
                    let _ = view_hide.update(cx, |this, cx| this.hide_column(col, cx));
                }) as HeaderMenuAction
            });

        let view_select = view_weak.clone();
        let on_select_columns: HeaderMenuAction =
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let _ = view_select.update(cx, |this, cx| {
                    this.open_select_columns_modal(window, cx);
                });
            });

        let view_reset = view_weak;
        let on_reset_defaults: HeaderMenuAction =
            Rc::new(move |_window: &mut Window, cx: &mut App| {
                let _ = view_reset.update(cx, |this, cx| this.reset_columns_to_default(cx));
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

    /// Updates the highlighted target [`ProcessKey`] (e.g., from `NavigationIntent::JumpToDetails`).
    pub(crate) fn set_highlighted_key(&mut self, key: Option<ProcessKey>, cx: &mut Context<Self>) {
        self.highlighted_key = key;
        if let Some(target) = key {
            self.selected_key = Some(target);
            self.pending_scroll_key = Some(target);
            self.try_scroll_to_pending(cx);
        } else {
            self.pending_scroll_key = None;
        }
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

    /// Attempts to scroll the virtualized table to `pending_scroll_key` once present in rows.
    fn try_scroll_to_pending(&mut self, cx: &Context<Self>) {
        let Some(target) = self.pending_scroll_key else {
            return;
        };
        let rows = &self.store.read(cx).snapshot().details.rows;
        if let Some(idx) = find_row_index_by_key(rows, target) {
            self.scroll_handle
                .scroll_to_item(idx, ScrollStrategy::Center);
            self.pending_scroll_key = None;
        }
    }

    fn select_row(&mut self, key: ProcessKey, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_key = Some(key);
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let modifiers = &event.keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
            return;
        }

        let key_name = event.keystroke.key.as_str();
        match key_name {
            "delete" => {
                let Some(selected) = self.selected_key else {
                    return;
                };
                let target = {
                    let rows = &self.store.read(cx).snapshot().details.rows;
                    find_row_index_by_key(rows, selected)
                        .and_then(|idx| rows.get(idx))
                        .map(ProcessActionTarget::from_row)
                };
                if let Some(target) = target {
                    request_end_task(&self.store, &target, window, cx);
                }
            }
            "up" | "down" => {
                let next = {
                    let rows = &self.store.read(cx).snapshot().details.rows;
                    if rows.is_empty() {
                        None
                    } else {
                        let current = self
                            .selected_key
                            .and_then(|sel| find_row_index_by_key(rows, sel));
                        let next_idx = match (current, key_name) {
                            (Some(idx), "up") => idx.saturating_sub(1),
                            (Some(idx), "down") => (idx + 1).min(rows.len() - 1),
                            (None, _) => 0,
                            _ => 0,
                        };
                        rows.get(next_idx).map(|row| (next_idx, row.key))
                    }
                };
                if let Some((next_idx, next_key)) = next {
                    self.selected_key = Some(next_key);
                    self.scroll_handle
                        .scroll_to_item(next_idx, ScrollStrategy::Nearest);
                    cx.notify();
                }
            }
            _ => {}
        }
    }
}

impl Render for DetailsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        debug_assert_eq!(self._details_lease.domain(), MonitorDomain::Details);

        let snapshot = self.store.read(cx).snapshot().clone();
        let details = snapshot.details.clone();
        let capabilities = snapshot.capabilities;
        let platform = snapshot.platform;
        let total_ram_bytes = snapshot.performance.latest.mem_total_bytes;
        let view_mode = details.view_mode;
        let sort_column = details.sort_column;
        let sort_direction = details.sort_direction;
        let row_count = details.rows.len();

        let selected_key = self.selected_key;
        let highlighted_key = self.highlighted_key;
        let selected_target = selected_key
            .and_then(|sel| find_row_index_by_key(&details.rows, sel))
            .and_then(|idx| details.rows.get(idx))
            .map(ProcessActionTarget::from_row);

        let theme = cx.theme();
        let border = theme.border;
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let muted_bg = theme.muted;
        let secondary_bg = theme.secondary;
        let primary = theme.primary;
        let radius = theme.radius;
        let mono_font = theme.mono_font_family.clone();
        let semantic = LoomSemanticColors::from_theme(theme);

        let store_flat = self.store.clone();
        let store_tree = self.store.clone();
        let store_run_task = self.store.clone();
        let store_end_task = self.store.clone();
        let end_task_target = selected_target.clone();
        let has_selection = selected_target.is_some();

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
                            .on_click(move |_, window, cx| {
                                if let Some(ref target) = end_task_target {
                                    request_end_task(&store_end_task, target, window, cx);
                                }
                            }),
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

        let columns: Vec<ColumnSpec<DetailsColumn>> = self.column_layout.visible_columns();
        let total_cols = columns.len();
        let table_width = self.column_layout.total_width(TABLE_HORIZONTAL_PADDING);
        let column_layout_snapshot = self.column_layout.clone();
        let header_view_weak = cx.entity().downgrade();

        let header_cells = columns.iter().enumerate().map(|(col_ix, spec)| {
            let col = spec.id;
            let sort_target = col.sort_column();
            let is_active_sort = sort_target == Some(sort_column);
            let next_dir = sort_target
                .map(|target| next_sort_direction(sort_column, sort_direction, target))
                .unwrap_or(SortDirection::Descending);
            let store_for_sort = self.store.clone();
            let menu_view = header_view_weak.clone();

            render_interactive_header_cell(
                col_ix,
                total_cols,
                HeaderColumnDescriptor {
                    id: format!("details-col-{}", col.slug()).into(),
                    label: col.label(platform).into(),
                    width: spec.width,
                    right_aligned: col.right_aligned(),
                    sortable: sort_target.is_some(),
                    is_active_sort,
                    sort_direction,
                },
                &column_layout_snapshot,
                cx,
                |view: &mut Self| &mut view.column_layout,
                move |_view, _window, cx| {
                    if let Some(target) = sort_target {
                        UiCommands::set_details_sort(&store_for_sort, target, next_dir, cx);
                    }
                },
                |view: &mut Self, cx| view.persist_column_layout(cx),
                move |menu, window, cx| {
                    Self::build_header_context_menu(menu, Some(col), menu_view.clone(), window, cx)
                },
            )
            .into_any_element()
        });

        let spacer_menu_view = header_view_weak.clone();
        let header_trailing = render_header_trailing_spacer(
            "details-column-header-spacer",
            move |menu, window, cx| {
                Self::build_header_context_menu(menu, None, spacer_menu_view.clone(), window, cx)
            },
        );

        let column_header = attach_header_reorder_handlers(
            h_flex()
                .id("details-column-header")
                .w_full()
                .h(px(32.))
                .flex_shrink_0()
                .items_center()
                .px_2()
                .bg(secondary_bg.opacity(0.45))
                .border_b_1()
                .border_color(border)
                .text_xs()
                .font_medium()
                .text_color(muted_fg)
                .children(header_cells)
                .child(header_trailing),
            cx,
            |view: &mut Self| &mut view.column_layout,
            |view: &mut Self, cx| view.persist_column_layout(cx),
        );

        let table_area = if details.revision == 0 {
            v_flex()
                .flex_1()
                .min_h_0()
                .child(column_header)
                .child(
                    v_flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .p_8()
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted_fg)
                                .child(i18n::tr("details.loading")),
                        ),
                )
                .into_any_element()
        } else if row_count == 0 {
            v_flex()
                .flex_1()
                .min_h_0()
                .child(column_header)
                .child(
                    v_flex()
                        .flex_1()
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
                                .child(i18n::tr("details.empty_search")),
                        ),
                )
                .into_any_element()
        } else {
            let store_for_rows = self.store.clone();
            let view_weak = cx.entity().downgrade();
            let icon_cache = Rc::clone(&self.icon_cache);
            let is_tree_mode = view_mode == DetailsViewMode::Tree;
            let running_label: SharedString = i18n::tr("details.status_running").into();
            let suspended_label: SharedString = i18n::tr("details.status_suspended").into();
            let elevated_yes_label: SharedString = i18n::tr("details.elevated_yes").into();
            let elevated_no_label: SharedString = i18n::tr("details.elevated_no").into();

            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .relative()
                .child(
                    div()
                        .id("details-table-hscroll")
                        .size_full()
                        .overflow_x_scroll()
                        .track_scroll(&self.h_scroll_handle)
                        .child(
                            v_flex()
                                .min_w(px(table_width))
                                .w_full()
                                .h_full()
                                .child(column_header)
                                .child(
                                    div().flex_1().min_h_0().w_full().child(
                                        uniform_list(
                                            "details-virtual-list",
                                            row_count,
                                            move |range, _window, cx| {
                                                let theme = cx.theme();
                                                let row_border = theme.border.opacity(0.45);
                                                let row_hover = theme.accent.opacity(0.45);
                                                let selected_bg = theme.primary.opacity(0.16);
                                                let highlight_bg = theme.primary.opacity(0.24);
                                                let highlight_border = theme.primary;

                                                range
                                                    .filter_map(|ix| {
                                                        let row = details.rows.get(ix)?;
                                                        let key = row.key;
                                                        let row_id = process_key_element_id(key);
                                                        let is_selected = selected_key == Some(key);
                                                        let is_highlighted =
                                                            highlighted_key == Some(key);
                                                        let target =
                                                            ProcessActionTarget::from_row(row);

                                                        let icon_image =
                                                            resolve_cached_icon(&icon_cache, row);
                                                        let indent = if is_tree_mode {
                                                            f32::from(row.depth.min(10)) * 16.0
                                                        } else {
                                                            0.0
                                                        };
                                                        let has_children =
                                                            is_tree_mode && row.has_children;
                                                        let is_expanded = row.is_expanded;

                                                        let status_text = if row
                                                            .status
                                                            .contains(ProcessStatusFlags::SUSPENDED)
                                                        {
                                                            suspended_label.clone()
                                                        } else {
                                                            running_label.clone()
                                                        };
                                                        let user_text = display_user_name(
                                                            row.user_name.as_deref(),
                                                        )
                                                        .to_owned();
                                                        let publisher_text = row
                                                            .publisher
                                                            .as_deref()
                                                            .unwrap_or("—")
                                                            .to_owned();
                                                        let description_text = row
                                                            .description
                                                            .as_deref()
                                                            .filter(|s| !s.is_empty())
                                                            .unwrap_or("—")
                                                            .to_owned();
                                                        let exe_path_text: SharedString = row
                                                            .exe_path
                                                            .as_deref()
                                                            .filter(|s| !s.is_empty())
                                                            .unwrap_or("—")
                                                            .to_owned()
                                                            .into();
                                                        let exe_path_tooltip = row
                                                            .exe_path
                                                            .as_deref()
                                                            .filter(|s| !s.is_empty())
                                                            .map(str::to_owned);
                                                        let elevated_text = if row
                                                            .status
                                                            .contains(ProcessStatusFlags::ELEVATED)
                                                        {
                                                            elevated_yes_label.clone()
                                                        } else {
                                                            elevated_no_label.clone()
                                                        };
                                                        let cmdline_text: SharedString = row
                                                            .command_line
                                                            .as_deref()
                                                            .filter(|s| !s.is_empty())
                                                            .unwrap_or("—")
                                                            .to_owned()
                                                            .into();
                                                        let cmdline_tooltip = row
                                                            .command_line
                                                            .as_deref()
                                                            .filter(|s| !s.is_empty())
                                                            .map(str::to_owned);

                                                        let toggle_store = store_for_rows.clone();
                                                        let click_view = view_weak.clone();
                                                        let rclick_view = view_weak.clone();
                                                        let menu_store = store_for_rows.clone();
                                                        let menu_target = target.clone();

                                                        let row_a11y_label: SharedString =
                                                            format!("{} (PID {})", row.name, key.pid)
                                                                .into();

                                                        let cells = columns.iter().map(|spec| {
                                                            let w = px(spec.width);
                                                            match spec.id {
                                                                DetailsColumn::Name => {
                                                                    let toggle_store = toggle_store.clone();
                                                                    h_flex()
                                                                        .w(w)
                                                                        .h_full()
                                                                        .flex_shrink_0()
                                                                        .items_center()
                                                                        .gap_1p5()
                                                                        .px_2()
                                                                        .pl(px(8.0 + indent))
                                                                        .overflow_hidden()
                                                                        .when(is_tree_mode, |el| {
                                                                            el.child(if has_children {
                                                                                Button::new((
                                                                                    "details-expand",
                                                                                    row_id,
                                                                                ))
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
                                                                                })
                                                                                .into_any_element()
                                                                            } else {
                                                                                div()
                                                                                    .w(px(18.))
                                                                                    .flex_shrink_0()
                                                                                    .into_any_element()
                                                                            })
                                                                        })
                                                                        .child(if let Some(image) =
                                                                            icon_image.clone()
                                                                        {
                                                                            img(image)
                                                                                .size(px(20.))
                                                                                .flex_shrink_0()
                                                                                .into_any_element()
                                                                        } else {
                                                                            h_flex()
                                                                                .size(px(20.))
                                                                                .flex_shrink_0()
                                                                                .items_center()
                                                                                .justify_center()
                                                                                .child(
                                                                                    Icon::new(
                                                                                        IconName::File,
                                                                                    )
                                                                                    .small()
                                                                                    .text_color(
                                                                                        muted_fg,
                                                                                    ),
                                                                                )
                                                                                .into_any_element()
                                                                        })
                                                                        .child(
                                                                            div()
                                                                                .flex_1()
                                                                                .min_w_0()
                                                                                .truncate()
                                                                                .child(
                                                                                    row.name
                                                                                        .to_string(),
                                                                                ),
                                                                        )
                                                                        .into_any_element()
                                                                }
                                                                DetailsColumn::Pid => h_flex()
                                                                    .w(w)
                                                                    .h_full()
                                                                    .flex_shrink_0()
                                                                    .items_center()
                                                                    .justify_end()
                                                                    .px_2()
                                                                    .font_family(mono_font.clone())
                                                                    .text_color(muted_fg)
                                                                    .child(key.pid.to_string())
                                                                    .into_any_element(),
                                                                DetailsColumn::Status => h_flex()
                                                                    .w(w)
                                                                    .h_full()
                                                                    .flex_shrink_0()
                                                                    .items_center()
                                                                    .px_2()
                                                                    .overflow_hidden()
                                                                    .child(
                                                                        div()
                                                                            .min_w_0()
                                                                            .truncate()
                                                                            .text_color(muted_fg)
                                                                            .child(status_text.clone()),
                                                                    )
                                                                    .into_any_element(),
                                                                DetailsColumn::UserName => h_flex()
                                                                    .w(w)
                                                                    .h_full()
                                                                    .flex_shrink_0()
                                                                    .items_center()
                                                                    .px_2()
                                                                    .overflow_hidden()
                                                                    .child(
                                                                        div()
                                                                            .min_w_0()
                                                                            .truncate()
                                                                            .text_color(muted_fg)
                                                                            .child(user_text.clone()),
                                                                    )
                                                                    .into_any_element(),
                                                                DetailsColumn::SessionId => h_flex()
                                                                    .w(w)
                                                                    .h_full()
                                                                    .flex_shrink_0()
                                                                    .items_center()
                                                                    .justify_end()
                                                                    .px_2()
                                                                    .font_family(mono_font.clone())
                                                                    .text_color(muted_fg)
                                                                    .child(row.session_id.to_string())
                                                                    .into_any_element(),
                                                                DetailsColumn::Cpu => div()
                                                                    .w(w)
                                                                    .h_full()
                                                                    .flex_shrink_0()
                                                                    .px_1()
                                                                    .flex()
                                                                    .items_center()
                                                                    .child(heatmap_cell(
                                                                        format_permille(
                                                                            row.cpu_permille,
                                                                        ),
                                                                        HeatmapLevel::from_permille(
                                                                            row.cpu_permille,
                                                                        ),
                                                                        &semantic,
                                                                    ))
                                                                    .into_any_element(),
                                                                DetailsColumn::CpuTime => h_flex()
                                                                    .w(w)
                                                                    .h_full()
                                                                    .flex_shrink_0()
                                                                    .items_center()
                                                                    .justify_end()
                                                                    .px_2()
                                                                    .font_family(mono_font.clone())
                                                                    .text_color(muted_fg)
                                                                    .child(format_cpu_time(
                                                                        row.cpu_time_100ns,
                                                                    ))
                                                                    .into_any_element(),
                                                                DetailsColumn::Memory => div()
                                                                    .w(w)
                                                                    .h_full()
                                                                    .flex_shrink_0()
                                                                    .px_1()
                                                                    .flex()
                                                                    .items_center()
                                                                    .child(heatmap_cell(
                                                                        format_bytes(
                                                                            row.private_working_set_bytes,
                                                                        ),
                                                                        memory_heatmap_level(
                                                                            row.private_working_set_bytes,
                                                                            total_ram_bytes,
                                                                        ),
                                                                        &semantic,
                                                                    ))
                                                                    .into_any_element(),
                                                                DetailsColumn::WorkingSet => h_flex()
                                                                    .w(w)
                                                                    .h_full()
                                                                    .flex_shrink_0()
                                                                    .items_center()
                                                                    .justify_end()
                                                                    .px_2()
                                                                    .font_family(mono_font.clone())
                                                                    .text_color(muted_fg)
                                                                    .child(format_bytes(
                                                                        row.working_set_bytes,
                                                                    ))
                                                                    .into_any_element(),
                                                                DetailsColumn::CommitSize => h_flex()
                                                                    .w(w)
                                                                    .h_full()
                                                                    .flex_shrink_0()
                                                                    .items_center()
                                                                    .justify_end()
                                                                    .px_2()
                                                                    .font_family(mono_font.clone())
                                                                    .text_color(muted_fg)
                                                                    .child(format_bytes(
                                                                        row.commit_bytes,
                                                                    ))
                                                                    .into_any_element(),
                                                                DetailsColumn::Priority => h_flex()
                                                                    .w(w)
                                                                    .h_full()
                                                                    .flex_shrink_0()
                                                                    .items_center()
                                                                    .px_2()
                                                                    .overflow_hidden()
                                                                    .child(
                                                                        div()
                                                                            .min_w_0()
                                                                            .truncate()
                                                                            .text_color(muted_fg)
                                                                            .child(
                                                                                i18n::process_priority_label(
                                                                                    row.priority,
                                                                                ),
                                                                            ),
                                                                    )
                                                                    .into_any_element(),
                                                                DetailsColumn::Handles => h_flex()
                                                                    .w(w)
                                                                    .h_full()
                                                                    .flex_shrink_0()
                                                                    .items_center()
                                                                    .justify_end()
                                                                    .px_2()
                                                                    .font_family(mono_font.clone())
                                                                    .text_color(muted_fg)
                                                                    .child(
                                                                        row.handle_count.to_string(),
                                                                    )
                                                                    .into_any_element(),
                                                                DetailsColumn::Threads => h_flex()
                                                                    .w(w)
                                                                    .h_full()
                                                                    .flex_shrink_0()
                                                                    .items_center()
                                                                    .justify_end()
                                                                    .px_2()
                                                                    .font_family(mono_font.clone())
                                                                    .text_color(muted_fg)
                                                                    .child(
                                                                        row.thread_count.to_string(),
                                                                    )
                                                                    .into_any_element(),
                                                                DetailsColumn::Platform => h_flex()
                                                                    .w(w)
                                                                    .h_full()
                                                                    .flex_shrink_0()
                                                                    .items_center()
                                                                    .px_2()
                                                                    .overflow_hidden()
                                                                    .child(
                                                                        div()
                                                                            .min_w_0()
                                                                            .truncate()
                                                                            .text_color(muted_fg)
                                                                            .child(
                                                                                i18n::process_arch_label(
                                                                                    row.arch,
                                                                                ),
                                                                            ),
                                                                    )
                                                                    .into_any_element(),
                                                                DetailsColumn::UacVirtualization => {
                                                                    h_flex()
                                                                        .w(w)
                                                                        .h_full()
                                                                        .flex_shrink_0()
                                                                        .items_center()
                                                                        .px_2()
                                                                        .overflow_hidden()
                                                                        .child(
                                                                            div()
                                                                                .min_w_0()
                                                                                .truncate()
                                                                                .text_color(muted_fg)
                                                                                .child(
                                                                                    i18n::uac_virtualization_label(
                                                                                        row.uac_virtualization,
                                                                                    ),
                                                                                ),
                                                                        )
                                                                        .into_any_element()
                                                                }
                                                                DetailsColumn::Elevated => h_flex()
                                                                    .w(w)
                                                                    .h_full()
                                                                    .flex_shrink_0()
                                                                    .items_center()
                                                                    .px_2()
                                                                    .overflow_hidden()
                                                                    .child(
                                                                        div()
                                                                            .min_w_0()
                                                                            .truncate()
                                                                            .text_color(muted_fg)
                                                                            .child(
                                                                                elevated_text.clone(),
                                                                            ),
                                                                    )
                                                                    .into_any_element(),
                                                                DetailsColumn::GpuMemory => h_flex()
                                                                    .w(w)
                                                                    .h_full()
                                                                    .flex_shrink_0()
                                                                    .items_center()
                                                                    .justify_end()
                                                                    .px_2()
                                                                    .font_family(mono_font.clone())
                                                                    .text_color(muted_fg)
                                                                    .child(format_bytes(
                                                                        row.gpu_dedicated_bytes,
                                                                    ))
                                                                    .into_any_element(),
                                                                DetailsColumn::Publisher => h_flex()
                                                                    .w(w)
                                                                    .h_full()
                                                                    .flex_shrink_0()
                                                                    .items_center()
                                                                    .px_2()
                                                                    .overflow_hidden()
                                                                    .child(
                                                                        div()
                                                                            .min_w_0()
                                                                            .truncate()
                                                                            .text_color(muted_fg)
                                                                            .child(publisher_text.clone()),
                                                                    )
                                                                    .into_any_element(),
                                                                DetailsColumn::Description => h_flex()
                                                                    .w(w)
                                                                    .h_full()
                                                                    .flex_shrink_0()
                                                                    .items_center()
                                                                    .px_2()
                                                                    .overflow_hidden()
                                                                    .child(
                                                                        div()
                                                                            .min_w_0()
                                                                            .truncate()
                                                                            .text_color(muted_fg)
                                                                            .child(
                                                                                description_text.clone(),
                                                                            ),
                                                                    )
                                                                    .into_any_element(),
                                                                DetailsColumn::ExePath => {
                                                                    h_flex()
                                                                        .id(("details-exe", row_id))
                                                                        .w(w)
                                                                        .h_full()
                                                                        .flex_shrink_0()
                                                                        .items_center()
                                                                        .px_2()
                                                                        .overflow_hidden()
                                                                        .font_family(mono_font.clone())
                                                                        .text_color(muted_fg)
                                                                        .when_some(
                                                                            exe_path_tooltip.clone(),
                                                                            |el, full_path| {
                                                                                el.tooltip(
                                                                                    move |window, cx| {
                                                                                        Tooltip::new(
                                                                                            full_path
                                                                                                .clone(),
                                                                                        )
                                                                                        .build(
                                                                                            window, cx,
                                                                                        )
                                                                                    },
                                                                                )
                                                                            },
                                                                        )
                                                                        .child(
                                                                            div()
                                                                                .min_w_0()
                                                                                .truncate()
                                                                                .child(
                                                                                    exe_path_text
                                                                                        .clone(),
                                                                                ),
                                                                        )
                                                                        .into_any_element()
                                                                }
                                                                DetailsColumn::CommandLine => {
                                                                    h_flex()
                                                                        .id(("details-cmd", row_id))
                                                                        .w(w)
                                                                        .h_full()
                                                                        .flex_shrink_0()
                                                                        .items_center()
                                                                        .px_2()
                                                                        .overflow_hidden()
                                                                        .font_family(mono_font.clone())
                                                                        .text_color(muted_fg)
                                                                        .when_some(
                                                                            cmdline_tooltip.clone(),
                                                                            |el, full_cmd| {
                                                                                el.tooltip(
                                                                                    move |window, cx| {
                                                                                        Tooltip::new(
                                                                                            full_cmd
                                                                                                .clone(),
                                                                                        )
                                                                                        .build(
                                                                                            window, cx,
                                                                                        )
                                                                                    },
                                                                                )
                                                                            },
                                                                        )
                                                                        .child(
                                                                            div()
                                                                                .min_w_0()
                                                                                .truncate()
                                                                                .child(cmdline_text.clone()),
                                                                        )
                                                                        .into_any_element()
                                                                }
                                                            }
                                                        });

                                                        let row_element = h_flex()
                                                            .id(("details-row", row_id))
                                                            .role(gpui_kit::Role::ListItem)
                                                            .aria_label(row_a11y_label)
                                                            .aria_selected(is_selected)
                                                            .w_full()
                                                            .h(px(DETAILS_ROW_HEIGHT))
                                                            .items_center()
                                                            .px_2()
                                                            .border_b_1()
                                                            .border_color(row_border)
                                                            .text_xs()
                                                            .text_color(fg)
                                                            .when(is_highlighted, |s| {
                                                                s.bg(highlight_bg)
                                                                    .border_l_2()
                                                                    .border_color(highlight_border)
                                                            })
                                                            .when(
                                                                is_selected && !is_highlighted,
                                                                |s| s.bg(selected_bg),
                                                            )
                                                            .hover(move |s| s.bg(row_hover))
                                                            .on_click(move |_, window, cx| {
                                                                let _ = click_view.update(
                                                                    cx,
                                                                    |this, cx| {
                                                                        this.select_row(
                                                                            key, window, cx,
                                                                        );
                                                                    },
                                                                );
                                                            })
                                                            .on_mouse_down(
                                                                MouseButton::Right,
                                                                move |_, window, cx| {
                                                                    let _ = rclick_view.update(
                                                                        cx,
                                                                        |this, cx| {
                                                                            this.select_row(
                                                                                key, window, cx,
                                                                            );
                                                                        },
                                                                    );
                                                                },
                                                            )
                                                            .children(cells);

                                                        Some(
                                                            row_element
                                                                .context_menu(
                                                                    move |menu, window, cx| {
                                                                        build_row_context_menu(
                                                                            menu,
                                                                            &menu_store,
                                                                            &menu_target,
                                                                            capabilities,
                                                                            window,
                                                                            cx,
                                                                        )
                                                                    },
                                                                )
                                                                .into_any_element(),
                                                        )
                                                    })
                                                    .collect()
                                            },
                                        )
                                        .size_full()
                                        .track_scroll(&self.scroll_handle),
                                    ),
                                ),
                        ),
                )
                .vertical_scrollbar(&self.scroll_handle)
                .horizontal_scrollbar(&self.h_scroll_handle)
                .into_any_element()
        };

        attach_table_column_drag_handlers(
            v_flex()
                .id("details-view-root")
                .track_focus(&self.focus_handle)
                .on_key_down(cx.listener(Self::handle_key_down))
                .size_full()
                .min_h_0()
                .child(toolbar)
                .when_some(highlight_banner, |this, banner| this.child(banner))
                .child(table_area),
            cx,
            |view: &mut Self| &mut view.column_layout,
            |view: &mut Self, cx| view.persist_column_layout(cx),
        )
    }
}

/// Builds the multi-level right-click [`gpui_kit::component::menu::PopupMenu`]
/// anchored strictly to `target.key`.
fn build_row_context_menu(
    menu: gpui_kit::component::menu::PopupMenu,
    store: &Entity<UiStore>,
    target: &ProcessActionTarget,
    capabilities: CapabilityFlags,
    window: &mut Window,
    cx: &mut Context<gpui_kit::component::menu::PopupMenu>,
) -> gpui_kit::component::menu::PopupMenu {
    let key = target.key;
    let current_priority = target.priority;
    let eco_supported = capabilities.contains(CapabilityFlags::ECO_MODE);
    let eco_enabled = target.status.contains(ProcessStatusFlags::EFFICIENCY_MODE);
    let eco_eligible = is_eco_mode_eligible(target.status, capabilities);
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
        .submenu_with_icon(
            Some(Icon::new(IconName::ChevronsUpDown)),
            i18n::tr("details.menu_set_priority"),
            window,
            cx,
            move |mut submenu, _window, _cx| {
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
            },
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
