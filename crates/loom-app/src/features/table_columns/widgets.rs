//! Shared GPUI table chrome, isolated reactive entities, cell renderers, and badges
//! used across Processes, Details, and sibling table feature slices.

use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu};
use gpui_kit::component::table::{DataTable, TableDelegate, TableState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, Size, StyledExt as _, h_flex,
    v_flex,
};
use gpui_kit::{
    AnyElement, App, ClickEvent, Context, Div, Empty, Entity, Hsla, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, Pixels, Render, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use loom_types::{LeakWarning, ProcessKey, WebView2Role};
use loom_ui_kit::{CachedSemanticColors, DEFAULT_CELL_PREVIEW_CHARS, MetricDisplay, RowTextCache};

use crate::i18n;

/// Thin GPUI entity isolating [`DataTable`] (`RenderOnce`) from parent page toolbars.
///
/// `DataTable::render` calls `TableState::update` to apply visual options and query row/column
/// counts. Hosting it inside its own `Entity` keeps that dependency out of the parent page's
/// `Render` pass so `gpui-fast` can retain the outer toolbar & shell and splice only the table
/// subtree on live metric updates.
pub(crate) struct DataTablePane<D: TableDelegate> {
    state: Entity<TableState<D>>,
    size: Size,
    header_row_height: Pixels,
    bordered: bool,
    accessibility_label: SharedString,
}

impl<D: TableDelegate> DataTablePane<D> {
    /// Creates a new [`DataTablePane`] hosting `state`.
    pub(crate) fn new(
        state: Entity<TableState<D>>,
        size: Size,
        header_row_height: Pixels,
        bordered: bool,
        accessibility_label: impl Into<SharedString>,
    ) -> Self {
        Self {
            state,
            size,
            header_row_height,
            bordered,
            accessibility_label: accessibility_label.into(),
        }
    }

    /// Updates the localized accessibility label (e.g. after a live language switch).
    pub(crate) fn set_accessibility_label(
        &mut self,
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.accessibility_label = label.into();
        cx.notify();
    }
}

impl<D: TableDelegate> Render for DataTablePane<D> {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        DataTable::new(&self.state)
            .with_size(self.size)
            .header_row_height(self.header_row_height)
            .bordered(self.bordered)
            .accessibility_label(self.accessibility_label.clone())
    }
}

// ---------------------------------------------------------------------------
// Pause-sort-on-hover state machine and header indicator
// ---------------------------------------------------------------------------

/// Pure state machine coordinating row-order freezing across table body hover,
/// column header hover, and open row context menus.
///
/// Row order freezes while hovering the table body (unless hovering the column
/// header to click-sort) or while any row context menu is open.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OrderFreezeState {
    table_hovered: bool,
    header_hovered: bool,
    context_menu_open: bool,
    frozen: bool,
}

impl OrderFreezeState {
    /// Returns whether row order is currently frozen.
    #[cfg(test)]
    #[inline]
    pub(crate) const fn is_frozen(&self) -> bool {
        self.frozen
    }

    #[inline]
    const fn desired(&self) -> bool {
        (self.table_hovered && !self.header_hovered) || self.context_menu_open
    }

    #[inline]
    fn reconcile(&mut self) -> Option<bool> {
        let next = self.desired();
        if next != self.frozen {
            self.frozen = next;
            Some(next)
        } else {
            None
        }
    }

    /// Updates table hover state and returns `Some(new_frozen)` when freeze transitions.
    pub(crate) fn set_table_hovered(&mut self, hovered: bool) -> Option<bool> {
        self.table_hovered = hovered;
        if !hovered {
            self.header_hovered = false;
        }
        self.reconcile()
    }

    /// Updates column-header hover state and returns `Some(new_frozen)` when freeze transitions.
    pub(crate) fn set_header_hovered(&mut self, hovered: bool) -> Option<bool> {
        self.header_hovered = hovered;
        self.reconcile()
    }

    /// Updates row context-menu open state and returns `Some(new_frozen)` when freeze transitions.
    pub(crate) fn set_context_menu_open(&mut self, open: bool) -> Option<bool> {
        self.context_menu_open = open;
        self.reconcile()
    }
}

/// Isolated GPUI Entity rendering the active sort header's amber paused-sort indicator.
///
/// Hosting the 16 px badge in its own entity lets hover-freeze transitions update only
/// this tiny header badge without notifying the enclosing `TableState` or invalidating
/// the table body's viewport fingerprint.
pub(crate) struct SortPausedBadge {
    id_prefix: &'static str,
    col_ix: usize,
    frozen: bool,
    tooltip: SharedString,
    semantic_colors: CachedSemanticColors,
}

impl SortPausedBadge {
    /// Creates a new hidden [`SortPausedBadge`] bound to `id_prefix`.
    pub(crate) fn new(id_prefix: &'static str, tooltip: SharedString) -> Self {
        Self {
            id_prefix,
            col_ix: 0,
            frozen: false,
            tooltip,
            semantic_colors: CachedSemanticColors::new(),
        }
    }

    /// Returns the currently tracked active sort column index.
    #[inline]
    pub(crate) const fn col_ix(&self) -> usize {
        self.col_ix
    }

    /// Updates whether the badge should be displayed, notifying only on state transitions.
    pub(crate) fn set_frozen(&mut self, frozen: bool, cx: &mut Context<Self>) {
        if self.frozen != frozen {
            self.frozen = frozen;
            cx.notify();
        }
    }

    /// Updates the localized tooltip text (e.g. after a live language switch).
    pub(crate) fn set_tooltip(&mut self, tooltip: SharedString, cx: &mut Context<Self>) {
        if self.tooltip != tooltip {
            self.tooltip = tooltip;
            if self.frozen {
                cx.notify();
            }
        }
    }

    /// Updates the active sort column index used for the badge's element ID.
    #[inline]
    pub(crate) fn set_col_ix(&mut self, col_ix: usize) {
        self.col_ix = col_ix;
    }
}

impl Render for SortPausedBadge {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.frozen {
            return Empty.into_any_element();
        }
        let color = self.semantic_colors.get_copy(cx.theme()).sort_paused;
        sort_paused_indicator((self.id_prefix, self.col_ix), color, self.tooltip.clone())
            .into_any_element()
    }
}

/// Visual styling mode for an isolated [`ToolbarActionButton`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolbarActionStyle {
    /// Solid danger button when enabled, outline when disabled (Processes toolbar).
    DangerWhenEnabled,
    /// Outlined danger button in both states (Details toolbar).
    DangerOutline,
}

/// Callback triggered when an isolated [`ToolbarActionButton`] is clicked.
pub(crate) type ToolbarActionClick = Rc<dyn Fn(&mut Window, &mut App)>;

/// Isolated GPUI Entity rendering a single selection-sensitive toolbar button
/// (such as **End task** in `ProcessesView` and `DetailsView`).
///
/// Hosting selection-dependent enablement in its own small entity mirrors Zed's
/// fine-grained status/action views: moving selection up/down or clicking a row
/// notifies only this button instead of re-rendering the enclosing page toolbar.
pub(crate) struct ToolbarActionButton {
    id: SharedString,
    icon: IconName,
    label: SharedString,
    style: ToolbarActionStyle,
    enabled: bool,
    on_click: ToolbarActionClick,
}

impl ToolbarActionButton {
    /// Creates a new [`ToolbarActionButton`] with initial enablement and click handler.
    pub(crate) fn new(
        id: impl Into<SharedString>,
        icon: IconName,
        label: impl Into<SharedString>,
        style: ToolbarActionStyle,
        enabled: bool,
        on_click: ToolbarActionClick,
    ) -> Self {
        Self {
            id: id.into(),
            icon,
            label: label.into(),
            style,
            enabled,
            on_click,
        }
    }

    /// Updates enablement, notifying GPUI only when state actually changes.
    pub(crate) fn set_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.enabled != enabled {
            self.enabled = enabled;
            cx.notify();
        }
    }

    /// Updates the localized button label (e.g. after a live language switch).
    pub(crate) fn set_label(&mut self, label: impl Into<SharedString>, cx: &mut Context<Self>) {
        let next = label.into();
        if self.label != next {
            self.label = next;
            cx.notify();
        }
    }

    /// Returns whether this action button is currently enabled.
    #[cfg(test)]
    #[inline]
    pub(crate) const fn is_enabled(&self) -> bool {
        self.enabled
    }
}

impl Render for ToolbarActionButton {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let on_click = Rc::clone(&self.on_click);
        let button = Button::new(self.id.clone()).small();
        let styled = match self.style {
            ToolbarActionStyle::DangerWhenEnabled => {
                if self.enabled {
                    button.danger()
                } else {
                    button.outline()
                }
            }
            ToolbarActionStyle::DangerOutline => button.danger().outline(),
        };
        styled
            .icon(self.icon.clone())
            .label(self.label.clone())
            .disabled(!self.enabled)
            .on_click(move |_, window, cx| {
                on_click(window, cx);
            })
    }
}

/// Estimates whether a single-line monospace `text_xs` cell string (`CommandLine` / `ExePath`)
/// may overflow `col_width_px` or exceed [`DEFAULT_CELL_PREVIEW_CHARS`], warranting a
/// stateful hover [`Tooltip`].
///
/// Short strings that comfortably fit inside the column skip `.id(...)` and `.tooltip(...)`
/// registration on every visible row.
#[inline]
pub(crate) fn should_attach_cell_tooltip(text: &str, col_width_px: f32) -> bool {
    if text.len() > DEFAULT_CELL_PREVIEW_CHARS {
        return true;
    }
    let usable_px = (col_width_px - 20.0).max(0.0);
    // Conservative monospace `text_xs` glyph width (~7.2 px per ASCII cell).
    const CHAR_WIDTH_PX: f32 = 7.2;
    if text.is_ascii() {
        return (text.len() as f32) * CHAR_WIDTH_PX > usable_px;
    }
    let mut cols = 0usize;
    for ch in text.chars() {
        cols += if ch.is_ascii() { 1 } else { 2 };
    }
    (cols as f32) * CHAR_WIDTH_PX > usable_px
}

/// Renders a subtle amber paused-sort indicator icon on the active sort column header.
///
/// Accepts a pre-cached `tooltip` string from the table's locale cache to avoid
/// per-frame `i18n::tr` allocations during header rendering.
pub(crate) fn sort_paused_indicator(
    id: (&'static str, usize),
    accent_color: Hsla,
    tooltip: SharedString,
) -> impl IntoElement {
    h_flex()
        .id(id)
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .size(px(16.0))
        .rounded_full()
        .bg(accent_color.opacity(0.16))
        .aria_label(tooltip.clone())
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        .child(
            Icon::new(IconName::Pause)
                .size(px(10.0))
                .text_color(accent_color),
        )
}

/// Lightweight interactive expand/collapse chevron replacing the full `Button` widget
/// inside high-density tree table rows.
pub(crate) fn expand_chevron_button(
    id: (&'static str, usize),
    is_expanded: bool,
    hover_bg: Hsla,
    muted_fg: Hsla,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .size(px(20.0))
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_sm()
        .hover(move |s| s.bg(hover_bg))
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .on_click(on_click)
        .child(
            Icon::new(if is_expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .xsmall()
            .text_color(muted_fg),
        )
}

/// Returns the localized role label for a [`WebView2Role`].
pub(crate) fn webview2_role_label(role: WebView2Role) -> String {
    match role {
        WebView2Role::Browser => i18n::tr("webview2_role_browser"),
        WebView2Role::Renderer => i18n::tr("webview2_role_renderer"),
        WebView2Role::Gpu => i18n::tr("webview2_role_gpu"),
        WebView2Role::Network => i18n::tr("webview2_role_network"),
        WebView2Role::Storage => i18n::tr("webview2_role_storage"),
        WebView2Role::Audio => i18n::tr("webview2_role_audio"),
        WebView2Role::Utility => i18n::tr("webview2_role_utility"),
        WebView2Role::Crashpad => i18n::tr("webview2_role_crashpad"),
    }
}

/// Pre-cached localized labels for all [`WebView2Role`] variants, shared by
/// Processes and Details locale caches to avoid per-frame `i18n::tr` calls.
pub(crate) struct WebView2RoleLabels {
    browser: SharedString,
    renderer: SharedString,
    gpu: SharedString,
    network: SharedString,
    storage: SharedString,
    audio: SharedString,
    utility: SharedString,
    crashpad: SharedString,
}

impl WebView2RoleLabels {
    /// Builds a fresh cache for the currently active locale.
    pub(crate) fn build() -> Self {
        Self {
            browser: webview2_role_label(WebView2Role::Browser).into(),
            renderer: webview2_role_label(WebView2Role::Renderer).into(),
            gpu: webview2_role_label(WebView2Role::Gpu).into(),
            network: webview2_role_label(WebView2Role::Network).into(),
            storage: webview2_role_label(WebView2Role::Storage).into(),
            audio: webview2_role_label(WebView2Role::Audio).into(),
            utility: webview2_role_label(WebView2Role::Utility).into(),
            crashpad: webview2_role_label(WebView2Role::Crashpad).into(),
        }
    }

    /// Returns the cached [`SharedString`] label for `role`.
    #[inline]
    pub(crate) fn get(&self, role: WebView2Role) -> SharedString {
        match role {
            WebView2Role::Browser => self.browser.clone(),
            WebView2Role::Renderer => self.renderer.clone(),
            WebView2Role::Gpu => self.gpu.clone(),
            WebView2Role::Network => self.network.clone(),
            WebView2Role::Storage => self.storage.clone(),
            WebView2Role::Audio => self.audio.clone(),
            WebView2Role::Utility => self.utility.clone(),
            WebView2Role::Crashpad => self.crashpad.clone(),
        }
    }
}

/// Renders a subtle compact tag identifying a `msedgewebview2.exe` subprocess role.
pub(crate) fn webview2_role_badge(
    label: SharedString,
    muted_bg: Hsla,
    muted_fg: Hsla,
    radius: Pixels,
) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .px_1p5()
        .py_0p5()
        .rounded(radius)
        .bg(muted_bg.opacity(0.75))
        .text_size(px(10.0))
        .text_color(muted_fg)
        .child(label)
}

/// Renders an amber resource-leak warning icon badge with an informative hover tooltip.
#[allow(dead_code)]
pub(crate) fn leak_warning_badge(
    id: (&'static str, usize),
    warning: &LeakWarning,
    leak_color: Hsla,
) -> impl IntoElement + use<> {
    let prefix = i18n::tr("leak_warning_badge_prefix");
    leak_warning_badge_with_prefix(id, &prefix, warning, leak_color)
}

/// Hot-path variant of [`leak_warning_badge`] accepting a pre-cached localized prefix
/// so `TableDelegate::render_td` avoids per-frame `i18n::tr` lookups.
pub(crate) fn leak_warning_badge_with_prefix(
    id: (&'static str, usize),
    prefix: &str,
    warning: &LeakWarning,
    leak_color: Hsla,
) -> impl IntoElement + use<> {
    let tooltip_text = SharedString::from(format!("{prefix}: {}", warning.summary));
    h_flex()
        .id(id)
        .size(px(22.0))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(leak_color.opacity(0.16))
        .aria_label(tooltip_text.clone())
        .tooltip(move |window, cx| Tooltip::new(tooltip_text.clone()).build(window, cx))
        .child(
            Icon::new(IconName::TriangleAlert)
                .xsmall()
                .text_color(leak_color),
        )
}

// ---------------------------------------------------------------------------
// Shared table header, cell, empty-state, and toolbar badge helpers
// ---------------------------------------------------------------------------

/// Builds the shared table header container with hover-freeze tracking and an empty-space
/// right-click header context menu.
pub(crate) fn render_table_header_row<D: TableDelegate>(
    header_id: &'static str,
    bg_menu_id: &'static str,
    cx: &mut Context<TableState<D>>,
    on_hover: impl Fn(&mut TableState<D>, bool, &mut Context<TableState<D>>) + 'static,
    build_menu: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
) -> Stateful<Div> {
    let bg = cx.theme().secondary.opacity(0.45);
    div()
        .id(header_id)
        .relative()
        .bg(bg)
        .text_xs()
        .font_medium()
        .on_hover(cx.listener(move |table, hovered: &bool, _, cx| {
            on_hover(table, *hovered, cx);
        }))
        .child(
            div()
                .id(bg_menu_id)
                .absolute()
                .inset_0()
                .context_menu(build_menu),
        )
}

/// Builds a shared sortable column header cell (`render_th`) with active-sort emphasis,
/// properly positioned [`SortPausedBadge`], click-to-sort, and column header context menu.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_table_th_cell<D: TableDelegate>(
    id_prefix: &'static str,
    col_ix: usize,
    label: SharedString,
    is_active_sort: bool,
    is_heatmap: bool,
    right_aligned: bool,
    sort_paused_badge: &Entity<SortPausedBadge>,
    cx: &mut Context<TableState<D>>,
    on_click_sort: impl Fn(&mut TableState<D>, &mut Context<TableState<D>>) + 'static,
    build_menu: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
) -> AnyElement {
    if is_active_sort && sort_paused_badge.read(cx).col_ix() != col_ix {
        sort_paused_badge.update(cx, |badge, _| {
            badge.set_col_ix(col_ix);
        });
    }
    let theme = cx.theme();
    let fg = theme.foreground;
    let muted_fg = theme.muted_foreground;
    let badge = sort_paused_badge.clone();

    h_flex()
        .id((id_prefix, col_ix))
        .flex_1()
        .min_w_0()
        .h_full()
        .items_center()
        .gap_1()
        .when(is_heatmap, |d| d.px_2())
        .when(right_aligned, |d| d.justify_end())
        .text_xs()
        .text_color(if is_active_sort { fg } else { muted_fg })
        .when(is_active_sort, |d| d.font_semibold())
        .on_click(cx.listener(move |table, _, _, cx| {
            on_click_sort(table, cx);
        }))
        .when(is_active_sort && right_aligned, |d| d.child(badge.clone()))
        .child(div().min_w_0().truncate().child(label))
        .when(is_active_sort && !right_aligned, |d| d.child(badge))
        .context_menu(build_menu)
        .into_any_element()
}

/// Renders a standard left-aligned truncating text table cell.
#[inline]
pub(crate) fn render_text_cell(text: SharedString, text_color: Hsla) -> AnyElement {
    h_flex()
        .size_full()
        .items_center()
        .overflow_hidden()
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_xs()
                .text_color(text_color)
                .child(text),
        )
        .into_any_element()
}

/// Renders a left-aligned truncating monospace text table cell.
#[inline]
pub(crate) fn render_mono_text_cell(
    text: SharedString,
    mono_font: SharedString,
    text_color: Hsla,
) -> AnyElement {
    h_flex()
        .size_full()
        .items_center()
        .overflow_hidden()
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_xs()
                .font_family(mono_font)
                .text_color(text_color)
                .child(text),
        )
        .into_any_element()
}

/// Renders a right-aligned monospace numeric table cell.
#[inline]
pub(crate) fn render_mono_numeric_cell(
    text: SharedString,
    mono_font: SharedString,
    text_color: Hsla,
) -> AnyElement {
    h_flex()
        .size_full()
        .items_center()
        .justify_end()
        .text_xs()
        .font_family(mono_font)
        .text_color(text_color)
        .child(text)
        .into_any_element()
}

/// Renders a right-aligned monospace numeric cell from a split [`MetricDisplay`] so
/// `metric.value` hits `gpui-fast::NumberShaping` and `metric.unit` hits `LineLayoutCache`.
#[inline]
pub(crate) fn render_metric_cell(
    metric: MetricDisplay,
    mono_font: SharedString,
    text_color: Hsla,
) -> AnyElement {
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

/// Renders a potentially multi-kilobyte monospace cell (`CommandLine` / `ExePath`) capped via
/// [`RowTextCache::preview_slot`], attaching a stateful [`Tooltip`] only when the full string
/// can overflow `col_width_px`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_mono_preview_cell(
    id_prefix: &'static str,
    row_id: usize,
    key: ProcessKey,
    preview_slot: u8,
    value: Option<&Arc<str>>,
    col_width_px: f32,
    row_cache: &mut RowTextCache,
    mono_font: SharedString,
    muted_fg: Hsla,
) -> AnyElement {
    let Some(arc) = value.filter(|s| !s.is_empty()) else {
        return h_flex()
            .size_full()
            .items_center()
            .overflow_hidden()
            .text_xs()
            .font_family(mono_font)
            .text_color(muted_fg)
            .child(SharedString::new_static("—"))
            .into_any_element();
    };

    let preview = row_cache.preview_slot(key, preview_slot, arc, DEFAULT_CELL_PREVIEW_CHARS);
    if should_attach_cell_tooltip(arc, col_width_px) {
        let tip = SharedString::from(Arc::clone(arc));
        h_flex()
            .id((id_prefix, row_id))
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
}

/// Renders the standard centered empty/no-search-matches state inside a `DataTable`.
pub(crate) fn render_table_empty_state(message: SharedString, muted_fg: Hsla) -> impl IntoElement {
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
        .child(div().text_sm().text_color(muted_fg).child(message))
}

/// Renders the compact IME composition preedit pill displayed in the page toolbar while
/// direct type-to-search has active CJK preedit text.
pub(crate) fn render_ime_preedit_badge(
    text: String,
    primary: Hsla,
    radius: Pixels,
) -> impl IntoElement {
    div()
        .px_2()
        .py_0p5()
        .rounded(radius)
        .bg(primary.opacity(0.14))
        .text_xs()
        .text_color(primary)
        .child(text)
}
