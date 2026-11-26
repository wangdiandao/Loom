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

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
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
    UniformListScrollHandle, Window, div, img, prelude::FluentBuilder as _, px, uniform_list,
};
use loom_types::{
    CapabilityFlags, DetailsRowProjection, DetailsSortColumn, DetailsViewMode, MonitorDomain,
    ProcessKey, ProcessPriority, ProcessStatusFlags, SortDirection,
};
use loom_ui_kit::{
    HeatmapLevel, LoomSemanticColors, StatusLabels, format_bytes, format_permille, heatmap_cell,
    rgba_icon_to_render_image, status_badges,
};

use crate::bridge::{ScopedMonitorLease, UiCommands, UiStore, UiStoreEvent};
use crate::i18n;

const DETAILS_ROW_HEIGHT: f32 = 30.0;
const ICON_CACHE_MAX_ENTRIES: usize = 512;

// Column widths (px) kept wide enough so horizontal scrolling prevents cramping.
const COL_NAME_W: f32 = 240.0;
const COL_PID_W: f32 = 72.0;
const COL_STATUS_W: f32 = 156.0;
const COL_USER_W: f32 = 152.0;
const COL_CPU_W: f32 = 78.0;
const COL_MEM_W: f32 = 136.0;
const COL_PRIORITY_W: f32 = 112.0;
const COL_PLATFORM_W: f32 = 82.0;
const COL_UAC_W: f32 = 132.0;
const COL_GPU_MEM_W: f32 = 144.0;
const COL_PUBLISHER_W: f32 = 164.0;
const COL_CMDLINE_MIN_W: f32 = 360.0;
/// Total minimum width of the full-field Details table (columns + 11 gaps + horizontal padding).
const TABLE_MIN_WIDTH: f32 = COL_NAME_W
    + COL_PID_W
    + COL_STATUS_W
    + COL_USER_W
    + COL_CPU_W
    + COL_MEM_W
    + COL_PRIORITY_W
    + COL_PLATFORM_W
    + COL_UAC_W
    + COL_GPU_MEM_W
    + COL_PUBLISHER_W
    + COL_CMDLINE_MIN_W
    + 11.0 * 8.0
    + 32.0;

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

/// Returns `true` when `key`/`status` represents a critical or OS-protected process
/// that should show an explicit confirmation warning before termination.
#[inline]
pub(super) fn is_critical_or_system_process(key: ProcessKey, status: ProcessStatusFlags) -> bool {
    key.pid <= 4
        || status.contains(ProcessStatusFlags::CRITICAL)
        || status.contains(ProcessStatusFlags::PROTECTED)
        || status.contains(ProcessStatusFlags::SYSTEM_IDLE)
}

/// Returns `true` if Efficiency Mode (`EcoQoS`) can be toggled for this process.
///
/// Disabled for PID 0 (System Idle), PID 4 (System), protected/critical system processes,
/// and backends that do not advertise [`CapabilityFlags::ECO_MODE`].
#[inline]
pub(super) fn is_eco_mode_eligible(
    key: ProcessKey,
    status: ProcessStatusFlags,
    capabilities: CapabilityFlags,
) -> bool {
    capabilities.contains(CapabilityFlags::ECO_MODE)
        && key.pid != 0
        && key.pid != 4
        && !status.contains(ProcessStatusFlags::SYSTEM_IDLE)
        && !status.contains(ProcessStatusFlags::PROTECTED)
        && !status.contains(ProcessStatusFlags::CRITICAL)
}

/// Returns the default initial sort direction when switching to a different column.
#[inline]
pub(super) const fn default_sort_direction(column: DetailsSortColumn) -> SortDirection {
    match column {
        DetailsSortColumn::Cpu
        | DetailsSortColumn::Memory
        | DetailsSortColumn::GpuMemory
        | DetailsSortColumn::Status => SortDirection::Descending,
        DetailsSortColumn::Name
        | DetailsSortColumn::Pid
        | DetailsSortColumn::UserName
        | DetailsSortColumn::Arch
        | DetailsSortColumn::UacVirtualization
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
    if !is_critical_or_system_process(target.key, target.status) {
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
    let is_critical = is_critical_or_system_process(target.key, target.status);

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
        let initial_rev = store.read(cx).snapshot().revisions.details;
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
            icon_cache: Rc::new(RefCell::new(HashMap::new())),
            _subscriptions: vec![sub],
        };
        view.try_scroll_to_pending(cx);
        view
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

/// Renders a clickable sortable column header button with active sort chevron indicator.
#[allow(clippy::too_many_arguments)]
fn sortable_header(
    id: &'static str,
    label: String,
    column: DetailsSortColumn,
    active_column: DetailsSortColumn,
    active_direction: SortDirection,
    width: f32,
    flex_grow: bool,
    align_right: bool,
    store: Entity<UiStore>,
    fg: gpui_kit::Hsla,
    muted_fg: gpui_kit::Hsla,
    hover_bg: gpui_kit::Hsla,
) -> impl IntoElement {
    let is_active = active_column == column;
    let next_dir = next_sort_direction(active_column, active_direction, column);
    let text_color = if is_active { fg } else { muted_fg };

    h_flex()
        .id(id)
        .h_full()
        .items_center()
        .gap_1()
        .px_1()
        .rounded_sm()
        .cursor_pointer()
        .text_color(text_color)
        .hover(move |s| s.bg(hover_bg).text_color(fg))
        .when(flex_grow, |el| el.flex_1().min_w(px(width)))
        .when(!flex_grow, |el| el.w(px(width)).flex_shrink_0())
        .when(align_right, |el| el.justify_end())
        .on_click(move |_, _, cx| {
            UiCommands::set_details_sort(&store, column, next_dir, cx);
        })
        .child(div().truncate().child(label))
        .when(is_active, |el| {
            let chevron = match active_direction {
                SortDirection::Ascending => IconName::ChevronUp,
                SortDirection::Descending => IconName::ChevronDown,
            };
            el.child(Icon::new(chevron).xsmall().text_color(fg))
        })
}

impl Render for DetailsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        debug_assert_eq!(self._details_lease.domain(), MonitorDomain::Details);

        let snapshot = self.store.read(cx).snapshot().clone();
        let details = snapshot.details.clone();
        let capabilities = snapshot.capabilities;
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
        let success = theme.success;
        let accent = theme.accent;
        let radius = theme.radius;
        let mono_font = theme.mono_font_family.clone();
        let semantic = LoomSemanticColors::from_theme(theme);

        let store_flat = self.store.clone();
        let store_tree = self.store.clone();
        let store_refresh = self.store.clone();
        let store_end_task = self.store.clone();
        let store_end_tree = self.store.clone();
        let end_task_target = selected_target.clone();
        let end_tree_target = selected_target.clone();
        let has_selection = selected_target.is_some();

        let toolbar = h_flex()
            .w_full()
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
                            .child(
                                Button::new("details-mode-flat")
                                    .small()
                                    .label(i18n::tr("details.view_flat"))
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
                                    .small()
                                    .label(i18n::tr("details.view_tree"))
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
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1p5()
                            .px_2p5()
                            .py_1()
                            .rounded(radius)
                            .bg(success.opacity(0.14))
                            .text_xs()
                            .text_color(success)
                            .child(Icon::new(IconName::CircleCheck).xsmall())
                            .child(i18n::tr("details.lease_active")),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1p5()
                            .px_2p5()
                            .py_1()
                            .rounded(radius)
                            .bg(muted_bg.opacity(0.65))
                            .text_xs()
                            .child(
                                div()
                                    .text_color(muted_fg)
                                    .child(i18n::tr("details.rows_label")),
                            )
                            .child(
                                div()
                                    .font_semibold()
                                    .text_color(fg)
                                    .child(row_count.to_string()),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
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
                    )
                    .child(
                        Button::new("details-end-process-tree")
                            .small()
                            .danger()
                            .outline()
                            .icon(IconName::Ban)
                            .label(i18n::tr("details.end_process_tree"))
                            .disabled(!has_selection)
                            .on_click(move |_, window, cx| {
                                if let Some(ref target) = end_tree_target {
                                    open_end_process_tree_dialog(
                                        &store_end_tree,
                                        target,
                                        window,
                                        cx,
                                    );
                                }
                            }),
                    )
                    .child(
                        Button::new("details-refresh")
                            .small()
                            .ghost()
                            .icon(IconName::RefreshCw)
                            .label(i18n::tr("processes.refresh"))
                            .on_click(move |_, _, cx| {
                                UiCommands::force_refresh(&store_refresh, cx);
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

        let header_hover = accent.opacity(0.5);
        let column_header = h_flex()
            .w_full()
            .h(px(32.))
            .flex_shrink_0()
            .items_center()
            .px_4()
            .gap_2()
            .bg(secondary_bg.opacity(0.45))
            .border_b_1()
            .border_color(border)
            .text_xs()
            .font_medium()
            .text_color(muted_fg)
            .child(sortable_header(
                "details-sort-name",
                i18n::tr("details.col_name"),
                DetailsSortColumn::Name,
                sort_column,
                sort_direction,
                COL_NAME_W,
                false,
                false,
                self.store.clone(),
                fg,
                muted_fg,
                header_hover,
            ))
            .child(sortable_header(
                "details-sort-pid",
                i18n::tr("details.col_pid"),
                DetailsSortColumn::Pid,
                sort_column,
                sort_direction,
                COL_PID_W,
                false,
                true,
                self.store.clone(),
                fg,
                muted_fg,
                header_hover,
            ))
            .child(sortable_header(
                "details-sort-status",
                i18n::tr("details.col_status"),
                DetailsSortColumn::Status,
                sort_column,
                sort_direction,
                COL_STATUS_W,
                false,
                false,
                self.store.clone(),
                fg,
                muted_fg,
                header_hover,
            ))
            .child(sortable_header(
                "details-sort-user",
                i18n::tr("details.col_user"),
                DetailsSortColumn::UserName,
                sort_column,
                sort_direction,
                COL_USER_W,
                false,
                false,
                self.store.clone(),
                fg,
                muted_fg,
                header_hover,
            ))
            .child(sortable_header(
                "details-sort-cpu",
                i18n::tr("details.col_cpu"),
                DetailsSortColumn::Cpu,
                sort_column,
                sort_direction,
                COL_CPU_W,
                false,
                true,
                self.store.clone(),
                fg,
                muted_fg,
                header_hover,
            ))
            .child(sortable_header(
                "details-sort-memory",
                i18n::tr("details.col_memory"),
                DetailsSortColumn::Memory,
                sort_column,
                sort_direction,
                COL_MEM_W,
                false,
                true,
                self.store.clone(),
                fg,
                muted_fg,
                header_hover,
            ))
            .child(
                div()
                    .w(px(COL_PRIORITY_W))
                    .flex_shrink_0()
                    .px_1()
                    .truncate()
                    .child(i18n::tr("details.col_priority")),
            )
            .child(sortable_header(
                "details-sort-arch",
                i18n::tr("details.col_platform"),
                DetailsSortColumn::Arch,
                sort_column,
                sort_direction,
                COL_PLATFORM_W,
                false,
                false,
                self.store.clone(),
                fg,
                muted_fg,
                header_hover,
            ))
            .child(sortable_header(
                "details-sort-uac",
                i18n::tr("details.col_uac"),
                DetailsSortColumn::UacVirtualization,
                sort_column,
                sort_direction,
                COL_UAC_W,
                false,
                false,
                self.store.clone(),
                fg,
                muted_fg,
                header_hover,
            ))
            .child(sortable_header(
                "details-sort-gpu-mem",
                i18n::tr("details.col_gpu_memory"),
                DetailsSortColumn::GpuMemory,
                sort_column,
                sort_direction,
                COL_GPU_MEM_W,
                false,
                true,
                self.store.clone(),
                fg,
                muted_fg,
                header_hover,
            ))
            .child(
                div()
                    .w(px(COL_PUBLISHER_W))
                    .flex_shrink_0()
                    .px_1()
                    .truncate()
                    .child(i18n::tr("details.col_publisher")),
            )
            .child(sortable_header(
                "details-sort-cmdline",
                i18n::tr("details.col_command_line"),
                DetailsSortColumn::CommandLine,
                sort_column,
                sort_direction,
                COL_CMDLINE_MIN_W,
                true,
                false,
                self.store.clone(),
                fg,
                muted_fg,
                header_hover,
            ));

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
            let status_labels = StatusLabels::new(
                i18n::tr("processes.status_efficiency"),
                i18n::tr("processes.status_suspended"),
                i18n::tr("processes.status_not_responding"),
            );
            let running_label = i18n::tr("details.status_running");

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
                                .min_w(px(TABLE_MIN_WIDTH))
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

                                                        let has_badge_flags = row.status.intersects(
                                                            ProcessStatusFlags::NOT_RESPONDING
                                                                | ProcessStatusFlags::SUSPENDED
                                                                | ProcessStatusFlags::EFFICIENCY_MODE,
                                                        );
                                                        let user_text = row
                                                            .user_name
                                                            .as_deref()
                                                            .unwrap_or("—")
                                                            .to_owned();
                                                        let publisher_text = row
                                                            .publisher
                                                            .as_deref()
                                                            .unwrap_or("—")
                                                            .to_owned();
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

                                                        let row_element = h_flex()
                                                            .id(("details-row", row_id))
                                                            .w_full()
                                                            .h(px(DETAILS_ROW_HEIGHT))
                                                            .items_center()
                                                            .px_4()
                                                            .gap_2()
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
                                                            .child(
                                                                h_flex()
                                                                    .w(px(COL_NAME_W))
                                                                    .flex_shrink_0()
                                                                    .items_center()
                                                                    .gap_1p5()
                                                                    .pl(px(indent))
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
                                                                        icon_image
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
                                                                    ),
                                                            )
                                                            .child(
                                                                div()
                                                                    .w(px(COL_PID_W))
                                                                    .flex_shrink_0()
                                                                    .text_right()
                                                                    .font_family(mono_font.clone())
                                                                    .text_color(muted_fg)
                                                                    .child(key.pid.to_string()),
                                                            )
                                                            .child(
                                                                div()
                                                                    .w(px(COL_STATUS_W))
                                                                    .flex_shrink_0()
                                                                    .overflow_hidden()
                                                                    .child(if has_badge_flags {
                                                                        status_badges(
                                                                            row.status,
                                                                            &status_labels,
                                                                            &semantic,
                                                                        )
                                                                        .into_any_element()
                                                                    } else {
                                                                        div()
                                                                            .truncate()
                                                                            .text_color(muted_fg)
                                                                            .child(
                                                                                running_label
                                                                                    .clone(),
                                                                            )
                                                                            .into_any_element()
                                                                    }),
                                                            )
                                                            .child(
                                                                div()
                                                                    .w(px(COL_USER_W))
                                                                    .flex_shrink_0()
                                                                    .truncate()
                                                                    .text_color(muted_fg)
                                                                    .child(user_text),
                                                            )
                                                            .child(
                                                                div()
                                                                    .w(px(COL_CPU_W))
                                                                    .flex_shrink_0()
                                                                    .child(heatmap_cell(
                                                                        format_permille(
                                                                            row.cpu_permille,
                                                                        ),
                                                                        HeatmapLevel::from_permille(
                                                                            row.cpu_permille,
                                                                        ),
                                                                        &semantic,
                                                                    )),
                                                            )
                                                            .child(
                                                                div()
                                                                    .w(px(COL_MEM_W))
                                                                    .flex_shrink_0()
                                                                    .child(heatmap_cell(
                                                                        format_bytes(
                                                                            row.private_working_set_bytes,
                                                                        ),
                                                                        memory_heatmap_level(
                                                                            row.private_working_set_bytes,
                                                                            total_ram_bytes,
                                                                        ),
                                                                        &semantic,
                                                                    )),
                                                            )
                                                            .child(
                                                                div()
                                                                    .w(px(COL_PRIORITY_W))
                                                                    .flex_shrink_0()
                                                                    .truncate()
                                                                    .text_color(muted_fg)
                                                                    .child(
                                                                        i18n::process_priority_label(
                                                                            row.priority,
                                                                        ),
                                                                    ),
                                                            )
                                                            .child(
                                                                div()
                                                                    .w(px(COL_PLATFORM_W))
                                                                    .flex_shrink_0()
                                                                    .truncate()
                                                                    .text_color(muted_fg)
                                                                    .child(
                                                                        i18n::process_arch_label(
                                                                            row.arch,
                                                                        ),
                                                                    ),
                                                            )
                                                            .child(
                                                                div()
                                                                    .w(px(COL_UAC_W))
                                                                    .flex_shrink_0()
                                                                    .truncate()
                                                                    .text_color(muted_fg)
                                                                    .child(
                                                                        i18n::uac_virtualization_label(
                                                                            row.uac_virtualization,
                                                                        ),
                                                                    ),
                                                            )
                                                            .child(
                                                                div()
                                                                    .w(px(COL_GPU_MEM_W))
                                                                    .flex_shrink_0()
                                                                    .text_right()
                                                                    .font_family(mono_font.clone())
                                                                    .text_color(muted_fg)
                                                                    .child(format_bytes(
                                                                        row.gpu_dedicated_bytes,
                                                                    )),
                                                            )
                                                            .child(
                                                                div()
                                                                    .w(px(COL_PUBLISHER_W))
                                                                    .flex_shrink_0()
                                                                    .truncate()
                                                                    .text_color(muted_fg)
                                                                    .child(publisher_text),
                                                            )
                                                            .child(
                                                                div()
                                                                    .id(("details-cmd", row_id))
                                                                    .flex_1()
                                                                    .min_w(px(COL_CMDLINE_MIN_W))
                                                                    .truncate()
                                                                    .font_family(mono_font.clone())
                                                                    .text_color(muted_fg)
                                                                    .when_some(
                                                                        cmdline_tooltip,
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
                                                                    .child(cmdline_text),
                                                            );

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

        v_flex()
            .id("details-view-root")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .size_full()
            .min_h_0()
            .child(toolbar)
            .when_some(highlight_banner, |this, banner| this.child(banner))
            .child(table_area)
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
    let eco_enabled = target.status.contains(ProcessStatusFlags::EFFICIENCY_MODE);
    let eco_eligible = is_eco_mode_eligible(key, target.status, capabilities);
    let can_set_priority = capabilities.contains(CapabilityFlags::PROCESS_PRIORITY) && key.pid != 0;
    let can_set_affinity = capabilities.contains(CapabilityFlags::CPU_AFFINITY) && key.pid != 0;
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

    menu.item(
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
                        .disabled(!can_set_priority)
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
            .disabled(!can_set_affinity)
            .on_click(move |_, window, cx| {
                affinity_dialog::open_set_affinity_dialog(
                    store_affinity.clone(),
                    key,
                    affinity_name.clone(),
                    window,
                    cx,
                );
            }),
    )
    .item(
        PopupMenuItem::new(i18n::tr("details.menu_efficiency_mode"))
            .checked(eco_enabled)
            .disabled(!eco_eligible)
            .on_click(move |_, _, cx| {
                UiCommands::set_efficiency_mode(&store_eco, key, !eco_enabled, cx);
            }),
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

        // Standard user process is eligible when capability is present.
        let user_key = ProcessKey::new(1234, 500);
        assert!(is_eco_mode_eligible(
            user_key,
            ProcessStatusFlags::empty(),
            with_eco
        ));
        assert!(is_eco_mode_eligible(
            user_key,
            ProcessStatusFlags::EFFICIENCY_MODE,
            with_eco
        ));
        assert!(!is_eco_mode_eligible(
            user_key,
            ProcessStatusFlags::empty(),
            without_eco
        ));
        assert!(!is_critical_or_system_process(
            user_key,
            ProcessStatusFlags::empty()
        ));

        // PID 0, PID 4, protected, critical, and system-idle processes are ineligible.
        assert!(!is_eco_mode_eligible(
            ProcessKey::new(0, 0),
            ProcessStatusFlags::SYSTEM_IDLE,
            with_eco
        ));
        assert!(!is_eco_mode_eligible(
            ProcessKey::new(4, 0),
            ProcessStatusFlags::empty(),
            with_eco
        ));
        assert!(!is_eco_mode_eligible(
            user_key,
            ProcessStatusFlags::PROTECTED,
            with_eco
        ));
        assert!(!is_eco_mode_eligible(
            user_key,
            ProcessStatusFlags::CRITICAL,
            with_eco
        ));

        assert!(is_critical_or_system_process(
            ProcessKey::new(4, 0),
            ProcessStatusFlags::empty()
        ));
        assert!(is_critical_or_system_process(
            user_key,
            ProcessStatusFlags::CRITICAL
        ));
        assert!(is_critical_or_system_process(
            user_key,
            ProcessStatusFlags::PROTECTED
        ));
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
}
