//! Reusable column catalog, visibility/order/width persistence, header context menu,
//! and **Select columns** dialog shared by the Processes and Details `DataTable` slices.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::table::{DataTable, TableDelegate, TableState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, Size, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Bounds, ClipboardItem, Context, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, GlobalElementId, Hsla, InspectorElementId,
    InteractiveElement as _, IntoElement, LayoutId, ParentElement as _, Pixels, Position, Render,
    RenderImage, SharedString, StatefulInteractiveElement as _, Style, Styled as _, Window, div,
    px,
};
use loom_config::TableColumnSetting;
use loom_engine::EngineCommand;
use loom_types::{
    CapabilityFlags, ExeIconRgba, LeakWarning, ProcessKey, ProcessStatusFlags, WebView2Role,
};
use loom_ui_kit::{build_csv_document, rgba_icon_to_render_image};

use crate::bridge::{UiCommands, UiStore};
use crate::i18n;

/// Maximum number of cached GPUI [`RenderImage`] icons retained by [`IconRenderCache`].
pub(crate) const MAX_CACHED_ICONS: usize = 512;

/// Unique key identifying an [`ExeIconRgba`] buffer by pointer address and dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct IconCacheKey {
    ptr: usize,
    width: u16,
    height: u16,
    len: usize,
}

impl IconCacheKey {
    #[inline]
    fn from_icon(icon: &ExeIconRgba) -> Self {
        Self {
            ptr: icon.rgba.as_ptr() as usize,
            width: icon.width,
            height: icon.height,
            len: icon.rgba.len(),
        }
    }
}

struct IconCacheEntry {
    /// Weak reference to the originating `Arc<[u8]>` to guard against ABA address reuse.
    source: Weak<[u8]>,
    image: Arc<RenderImage>,
}

/// Bounded local cache mapping unique [`ExeIconRgba`] buffers to GPUI [`RenderImage`] handles.
///
/// Prevents re-swapping RGBA -> BGRA pixels and re-uploading textures on every frame.
#[derive(Default)]
pub(crate) struct IconRenderCache {
    entries: HashMap<IconCacheKey, IconCacheEntry>,
    max_entries: usize,
}

impl IconRenderCache {
    /// Creates a new bounded [`IconRenderCache`] capped at [`MAX_CACHED_ICONS`].
    pub(crate) fn new() -> Self {
        Self::with_capacity(MAX_CACHED_ICONS)
    }

    /// Creates a new bounded [`IconRenderCache`] with a custom capacity ceiling.
    pub(crate) fn with_capacity(max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_entries: max_entries.max(1),
        }
    }

    /// Returns a cached [`Arc<RenderImage>`] for `icon`, converting it once on cache miss.
    pub(crate) fn get_or_insert(&mut self, icon: &ExeIconRgba) -> Option<Arc<RenderImage>> {
        let key = IconCacheKey::from_icon(icon);
        if let Some(entry) = self.entries.get(&key)
            && entry
                .source
                .upgrade()
                .is_some_and(|existing| Arc::ptr_eq(&existing, &icon.rgba))
        {
            return Some(Arc::clone(&entry.image));
        }

        let rendered = rgba_icon_to_render_image(icon)?;

        if self.entries.len() >= self.max_entries {
            self.prune();
        }

        self.entries.insert(
            key,
            IconCacheEntry {
                source: Arc::downgrade(&icon.rgba),
                image: Arc::clone(&rendered),
            },
        );
        Some(rendered)
    }

    /// Reclaims entries whose underlying `ExeIconRgba` buffer was dropped, or clears
    /// the cache if all live entries exceed `max_entries`.
    fn prune(&mut self) {
        self.entries
            .retain(|_, entry| entry.source.strong_count() > 0);
        if self.entries.len() >= self.max_entries {
            self.entries.clear();
        }
    }

    /// Returns the number of cached entries currently retained.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Identifier trait for columns managed by [`TableColumnLayout`].
pub(crate) trait ColumnKey: Copy + Eq + 'static {
    /// Stable lowercase key used to persist column order, widths, and visibility in `settings.toml`.
    fn slug(self) -> &'static str;
}

/// Static + runtime geometry and visibility specification for a single table column.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ColumnSpec<C: ColumnKey> {
    /// Logical column identifier.
    pub(crate) id: C,
    /// Current rendered width in logical pixels.
    pub(crate) width: f32,
    /// Minimum allowed width in logical pixels during resize.
    pub(crate) min_width: f32,
    /// Maximum allowed width in logical pixels during resize.
    pub(crate) max_width: f32,
    /// Whether this column is currently visible in the table.
    pub(crate) visible: bool,
    /// Whether this column is pinned on the left and cannot be hidden or moved.
    pub(crate) locked: bool,
}

impl<C: ColumnKey> ColumnSpec<C> {
    /// Creates a visible, hideable [`ColumnSpec`] with the given width and bounds.
    #[inline]
    pub(crate) const fn new(id: C, width: f32, min_width: f32, max_width: f32) -> Self {
        Self {
            id,
            width,
            min_width,
            max_width,
            visible: true,
            locked: false,
        }
    }

    /// Creates a visible, left-pinned [`ColumnSpec`] that cannot be hidden or reordered.
    #[inline]
    pub(crate) const fn pinned(id: C, width: f32, min_width: f32, max_width: f32) -> Self {
        Self {
            id,
            width,
            min_width,
            max_width,
            visible: true,
            locked: true,
        }
    }

    /// Creates a hidden-by-default, toggleable [`ColumnSpec`] with the given width and bounds.
    #[inline]
    pub(crate) const fn hidden(id: C, width: f32, min_width: f32, max_width: f32) -> Self {
        Self {
            id,
            width,
            min_width,
            max_width,
            visible: false,
            locked: false,
        }
    }
}

/// Pure column order, visibility, and width state backing a `gpui-kit` `DataTable`.
///
/// Maintains a pre-filtered `visible` cache so hot-path `TableDelegate::render_td`
/// lookups by visible column index run in `O(1)` with zero allocations, while hidden
/// columns retain their relative order and last saved widths inside `columns`.
///
/// Locked (pinned) columns are always normalized to the leading positions so they
/// satisfy `DataTable`'s `Column::fixed_left()` ordering invariant even if loaded
/// from an older or hand-edited `settings.toml`.
#[derive(Debug, Clone)]
pub(crate) struct TableColumnLayout<C: ColumnKey> {
    columns: Vec<ColumnSpec<C>>,
    visible: Vec<ColumnSpec<C>>,
}

impl<C: ColumnKey> TableColumnLayout<C> {
    /// Constructs a layout initialized from default ordered [`ColumnSpec`] definitions.
    pub(crate) fn new(mut columns: Vec<ColumnSpec<C>>) -> Self {
        normalize_columns(&mut columns);
        let visible = collect_visible(&columns);
        Self { columns, visible }
    }

    /// Applies previously persisted [`TableColumnSetting`] entries, preserving order, clamped
    /// widths, and visibility while keeping locked columns leading and appending any newly
    /// added columns at their default positions.
    pub(crate) fn apply_saved(&mut self, saved: &[TableColumnSetting]) {
        if saved.is_empty() {
            return;
        }
        let mut remaining = self.columns.clone();
        let mut reordered = Vec::with_capacity(remaining.len());

        for entry in saved {
            if let Some(pos) = remaining
                .iter()
                .position(|spec| spec.id.slug() == entry.id.as_str())
            {
                let mut spec = remaining.remove(pos);
                spec.width = (entry.width as f32).clamp(spec.min_width, spec.max_width);
                spec.visible = if spec.locked { true } else { entry.visible };
                reordered.push(spec);
            }
        }
        reordered.extend(remaining);
        normalize_columns(&mut reordered);

        self.columns = reordered;
        self.rebuild_visible_cache();
    }

    /// Exports the current column order, widths, and visibility for persistence in `settings.toml`.
    pub(crate) fn snapshot(&self) -> Vec<TableColumnSetting> {
        self.columns
            .iter()
            .map(|spec| TableColumnSetting {
                id: spec.id.slug().to_owned(),
                width: spec.width.round().max(1.0) as u32,
                visible: spec.visible,
            })
            .collect()
    }

    /// Resets the layout to fresh default specifications.
    pub(crate) fn reset(&mut self, mut defaults: Vec<ColumnSpec<C>>) {
        normalize_columns(&mut defaults);
        self.columns = defaults;
        self.rebuild_visible_cache();
    }

    /// Returns all columns (both visible and hidden) in their stored order.
    #[cfg(test)]
    #[inline]
    pub(crate) fn all_columns(&self) -> &[ColumnSpec<C>] {
        &self.columns
    }

    /// Returns the cached slice of currently visible columns in display order (`O(1)`).
    #[inline]
    pub(crate) fn visible_columns(&self) -> &[ColumnSpec<C>] {
        &self.visible
    }

    /// Returns the visible column at display index `vis_ix` in `O(1)`.
    #[inline]
    pub(crate) fn visible_column(&self, vis_ix: usize) -> Option<ColumnSpec<C>> {
        self.visible.get(vis_ix).copied()
    }

    /// Returns the number of currently visible columns in `O(1)`.
    #[inline]
    pub(crate) fn visible_count(&self) -> usize {
        self.visible.len()
    }

    /// Returns `true` if column `id` is currently visible.
    #[inline]
    pub(crate) fn is_visible(&self, id: C) -> bool {
        self.visible.iter().any(|spec| spec.id == id)
    }

    /// Hides column `id` if it is visible, unlocked, and not the last visible column.
    ///
    /// Returns `true` if visibility changed.
    pub(crate) fn hide_column(&mut self, id: C) -> bool {
        if self.visible.len() <= 1 {
            return false;
        }
        let Some(spec) = self.columns.iter_mut().find(|spec| spec.id == id) else {
            return false;
        };
        if spec.locked || !spec.visible {
            return false;
        }
        spec.visible = false;
        self.rebuild_visible_cache();
        true
    }

    /// Updates visibility for all unlocked columns based on the selected `visible_slugs`.
    ///
    /// Locked columns always remain visible, and at least one column is guaranteed to stay visible.
    /// Returns `true` if any column's visibility changed.
    pub(crate) fn apply_visible_slugs(&mut self, visible_slugs: &[&'static str]) -> bool {
        let mut changed = false;
        for spec in &mut self.columns {
            let next_visible = if spec.locked {
                true
            } else {
                visible_slugs.contains(&spec.id.slug())
            };
            if spec.visible != next_visible {
                spec.visible = next_visible;
                changed = true;
            }
        }
        if ensure_at_least_one_visible(&mut self.columns) {
            changed = true;
        }
        if changed {
            self.rebuild_visible_cache();
        }
        changed
    }

    /// Synchronizes resized visible column widths reported by `TableEvent::ColumnWidthsChanged`.
    ///
    /// Returns `true` if any column's clamped width changed.
    pub(crate) fn apply_visible_widths(&mut self, widths: &[Pixels]) -> bool {
        if widths.len() != self.visible.len() {
            return false;
        }
        let mut changed = false;
        let mut vis_ix = 0;
        for spec in &mut self.columns {
            if !spec.visible {
                continue;
            }
            let clamped = f32::from(widths[vis_ix])
                .clamp(spec.min_width, spec.max_width)
                .round();
            if (spec.width - clamped).abs() >= 0.5 {
                spec.width = clamped;
                changed = true;
            }
            vis_ix += 1;
        }
        if changed {
            self.rebuild_visible_cache();
        }
        changed
    }

    /// Moves the visible unlocked column at `from_vis_ix` to visible slot `to_vis_ix`.
    ///
    /// Locked (left-pinned) columns cannot be moved or displaced from the leading slots.
    pub(crate) fn move_visible_column(&mut self, from_vis_ix: usize, to_vis_ix: usize) -> bool {
        let vis_len = self.visible.len();
        if from_vis_ix >= vis_len || to_vis_ix >= vis_len || from_vis_ix == to_vis_ix {
            return false;
        }
        let pinned_count = self.visible.iter().take_while(|c| c.locked).count();
        if from_vis_ix < pinned_count || to_vis_ix < pinned_count {
            return false;
        }
        let Some(from_all_ix) = self.visible_to_all_index(from_vis_ix) else {
            return false;
        };
        let Some(target_all_ix) = self.visible_to_all_index(to_vis_ix) else {
            return false;
        };

        let col = self.columns.remove(from_all_ix);
        self.columns.insert(target_all_ix, col);
        self.rebuild_visible_cache();
        true
    }

    fn visible_to_all_index(&self, vis_ix: usize) -> Option<usize> {
        self.columns
            .iter()
            .enumerate()
            .filter(|(_, spec)| spec.visible)
            .nth(vis_ix)
            .map(|(all_ix, _)| all_ix)
    }

    fn rebuild_visible_cache(&mut self) {
        self.visible.clear();
        self.visible
            .extend(self.columns.iter().copied().filter(|spec| spec.visible));
    }
}

fn collect_visible<C: ColumnKey>(columns: &[ColumnSpec<C>]) -> Vec<ColumnSpec<C>> {
    columns.iter().copied().filter(|c| c.visible).collect()
}

fn normalize_columns<C: ColumnKey>(columns: &mut Vec<ColumnSpec<C>>) {
    // Partition locked (left-pinned) columns to the front while preserving relative order.
    let mut locked = Vec::new();
    let mut unlocked = Vec::with_capacity(columns.len());
    for mut col in columns.drain(..) {
        if col.locked {
            col.visible = true;
            locked.push(col);
        } else {
            unlocked.push(col);
        }
    }
    columns.extend(locked);
    columns.extend(unlocked);
    ensure_at_least_one_visible(columns);
}

fn ensure_at_least_one_visible<C: ColumnKey>(columns: &mut [ColumnSpec<C>]) -> bool {
    if columns.is_empty() || columns.iter().any(|c| c.visible) {
        return false;
    }
    if let Some(locked) = columns.iter_mut().find(|c| c.locked) {
        locked.visible = true;
    } else {
        columns[0].visible = true;
    }
    true
}

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

/// Callback executed by a table column header context menu item.
pub(crate) type HeaderMenuAction = Rc<dyn Fn(&mut Window, &mut App)>;

/// Builds the standard table column header right-click [`PopupMenu`].
///
/// Includes `Hide column` (disabled when `hide_action` is `None`, e.g., on the pinned Name
/// column or empty header space), `Select columns...`, optional view-specific `extra_items`,
/// and `Reset default columns`.
pub(crate) fn build_column_header_menu<FExtra>(
    mut menu: PopupMenu,
    hide_action: Option<HeaderMenuAction>,
    on_select_columns: HeaderMenuAction,
    extra_items: FExtra,
    on_reset_defaults: HeaderMenuAction,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu
where
    FExtra: FnOnce(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu,
{
    let can_hide = hide_action.is_some();
    menu = menu
        .item(
            PopupMenuItem::new(i18n::tr("table.menu_hide_column"))
                .disabled(!can_hide)
                .on_click(move |_, window, cx| {
                    if let Some(ref action) = hide_action {
                        action(window, cx);
                    }
                }),
        )
        .item(
            PopupMenuItem::new(i18n::tr("table.menu_select_columns"))
                .icon(IconName::Settings2)
                .on_click(move |_, window, cx| {
                    on_select_columns(window, cx);
                }),
        );

    menu = extra_items(menu, window, cx);

    menu.separator().item(
        PopupMenuItem::new(i18n::tr("table.menu_reset_columns")).on_click(move |_, window, cx| {
            on_reset_defaults(window, cx);
        }),
    )
}

/// Single checkbox entry displayed inside the **Select columns** dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnToggleItem {
    /// Stable column slug matching [`ColumnKey::slug`].
    pub(crate) slug: &'static str,
    /// Localized display label shown next to the checkbox.
    pub(crate) label: String,
    /// Current visibility toggle state.
    pub(crate) visible: bool,
}

/// Pure draft state for the **Select columns** checklist dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectColumnsDraft {
    items: Vec<ColumnToggleItem>,
}

impl SelectColumnsDraft {
    /// Creates a new draft from ordered [`ColumnToggleItem`] entries.
    pub(crate) fn new(items: Vec<ColumnToggleItem>) -> Self {
        Self { items }
    }

    /// Returns the ordered checklist items.
    #[inline]
    pub(crate) fn items(&self) -> &[ColumnToggleItem] {
        &self.items
    }

    /// Updates the checked state of item `index`.
    pub(crate) fn set_checked(&mut self, index: usize, checked: bool) {
        if let Some(item) = self.items.get_mut(index) {
            item.visible = checked;
        }
    }

    /// Returns the slugs of all currently checked items.
    pub(crate) fn selected_slugs(&self) -> Vec<&'static str> {
        self.items
            .iter()
            .filter(|item| item.visible)
            .map(|item| item.slug)
            .collect()
    }
}

type ConfirmColumnsCallback = Rc<dyn Fn(Vec<&'static str>, &mut App)>;

struct SelectColumnsDialogView {
    draft: SelectColumnsDraft,
    on_confirm: ConfirmColumnsCallback,
}

impl SelectColumnsDialogView {
    fn new(items: Vec<ColumnToggleItem>, on_confirm: ConfirmColumnsCallback) -> Self {
        Self {
            draft: SelectColumnsDraft::new(items),
            on_confirm,
        }
    }
}

impl Render for SelectColumnsDialogView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted_fg = theme.muted_foreground;
        let border = theme.border;

        let checkboxes = self
            .draft
            .items()
            .iter()
            .enumerate()
            .map(|(idx, item)| {
                let label = item.label.clone();
                let checked = item.visible;
                div().w_full().py_0p5().child(
                    Checkbox::new(("select-column-checkbox", idx))
                        .label(label)
                        .checked(checked)
                        .on_change(cx.listener(move |this, next: &bool, _, cx| {
                            this.draft.set_checked(idx, *next);
                            cx.notify();
                        })),
                )
            })
            .collect::<Vec<_>>();

        v_flex()
            .w_full()
            .gap_3()
            .child(
                div()
                    .text_xs()
                    .text_color(muted_fg)
                    .child(i18n::tr("table.select_columns_subtitle")),
            )
            .child(
                v_flex()
                    .id("select-columns-scroll")
                    .w_full()
                    .max_h(px(340.))
                    .overflow_y_scrollbar()
                    .gap_2()
                    .py_1()
                    .pr_2()
                    .border_t_1()
                    .border_b_1()
                    .border_color(border.opacity(0.6))
                    .children(checkboxes),
            )
            .child(
                DialogFooter::new()
                    .pt_1()
                    .child(
                        Button::new("select-columns-ok")
                            .primary()
                            .label(i18n::tr("common.ok"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                let selected = this.draft.selected_slugs();
                                (this.on_confirm)(selected, cx);
                                window.close_dialog(cx);
                            })),
                    )
                    .child(
                        Button::new("select-columns-cancel")
                            .outline()
                            .label(i18n::tr("common.cancel"))
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    ),
            )
    }
}

/// Opens the reusable **Select columns** modal dialog inside `gpui-kit`'s `Root` dialog layer.
pub(crate) fn open_select_columns_dialog(
    items: Vec<ColumnToggleItem>,
    on_confirm: ConfirmColumnsCallback,
    window: &mut Window,
    cx: &mut App,
) {
    let dialog_view = cx.new(|_| SelectColumnsDialogView::new(items, on_confirm));
    window.open_dialog(cx, move |dialog, _window, _cx| {
        dialog
            .w(px(360.))
            .title(i18n::tr("table.select_columns_title"))
            .child(dialog_view.clone())
    });
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

/// Renders a subtle amber paused-sort indicator icon on the active sort column header.
pub(crate) fn sort_paused_indicator(
    id: (&'static str, usize),
    accent_color: Hsla,
) -> impl IntoElement {
    let tip = SharedString::from(i18n::tr("proc_ux_sort_paused_tooltip"));
    h_flex()
        .id(id)
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .size(px(16.0))
        .rounded_full()
        .bg(accent_color.opacity(0.16))
        .aria_label(tip.clone())
        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
        .child(
            Icon::new(IconName::Pause)
                .size(px(10.0))
                .text_color(accent_color),
        )
}

// ---------------------------------------------------------------------------
// Direct type-to-search with CJK IME composition state machine & input bridge
// ---------------------------------------------------------------------------

/// Pure UTF-8 / UTF-16 state machine for direct table type-to-search with CJK IME composition.
///
/// Uncommitted IME preedit text is tracked in `marked_range` without mutating `committed`,
/// so cancelling composition (via Escape or empty commit) leaves the previous query intact,
/// while committing text immediately updates the global search query.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct TypeToSearchState {
    committed: String,
    buffer: String,
    marked_range: Option<Range<usize>>,
}

impl TypeToSearchState {
    /// Synchronizes committed text from the global store when no IME composition is active.
    pub(crate) fn sync_committed(&mut self, query: &str) {
        if self.marked_range.is_none() && self.committed != query {
            self.committed.clear();
            self.committed.push_str(query);
            self.buffer.clear();
            self.buffer.push_str(query);
        }
    }

    /// Returns `true` when an IME preedit composition is currently active.
    #[inline]
    pub(crate) fn is_composing(&self) -> bool {
        self.marked_range.is_some()
    }

    /// Returns the currently active IME preedit text, if any.
    pub(crate) fn preedit_text(&self) -> Option<&str> {
        let range = self.marked_range.as_ref()?;
        self.buffer
            .get(range.start..range.end)
            .filter(|s| !s.is_empty())
    }

    /// Returns the UTF-16 length of the current text buffer.
    pub(crate) fn utf16_len(&self) -> usize {
        self.offset_to_utf16(self.buffer.len())
    }

    /// Returns the current caret range (collapsed at the end of the buffer) in UTF-16 units.
    pub(crate) fn selected_utf16_range(&self) -> Range<usize> {
        let end = self.utf16_len();
        end..end
    }

    /// Returns the active IME marked text range in UTF-16 units, if composing.
    pub(crate) fn marked_utf16_range(&self) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|r| self.range_to_utf16(r.clone()))
    }

    /// Extracts text for a UTF-16 range and returns `(adjusted_utf16_range, text)`.
    pub(crate) fn text_for_utf16_range(&self, range_utf16: Range<usize>) -> (Range<usize>, String) {
        let utf8 = self.range_from_utf16(range_utf16);
        let adjusted = self.range_to_utf16(utf8.clone());
        let text = self.buffer.get(utf8).unwrap_or_default().to_owned();
        (adjusted, text)
    }

    /// Updates or cancels uncommitted IME preedit text without altering `committed`.
    pub(crate) fn replace_and_mark(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        store_query: &str,
    ) {
        if self.marked_range.is_none() {
            self.sync_committed(store_query);
        }

        let target = range_utf16
            .map(|r| self.range_from_utf16(r))
            .or_else(|| self.marked_range.clone())
            .unwrap_or(self.buffer.len()..self.buffer.len());

        let sanitized = sanitize_search_input(new_text);
        if sanitized.is_empty() {
            self.buffer.clear();
            self.buffer.push_str(&self.committed);
            self.marked_range = None;
            return;
        }

        self.buffer.replace_range(target.clone(), &sanitized);
        self.marked_range = Some(target.start..target.start + sanitized.len());
    }

    /// Commits typed or IME-composed text into the search query.
    ///
    /// Returns `Some(next_query)` when the committed search query changed and should
    /// be dispatched via `UiCommands::set_search_query`. Cancelling an active composition
    /// with an empty string restores the previous `committed` query and returns `None`.
    pub(crate) fn replace_text(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        store_query: &str,
    ) -> Option<String> {
        let was_composing = self.marked_range.is_some();
        if !was_composing {
            self.sync_committed(store_query);
        }

        let sanitized = sanitize_search_input(text);
        if was_composing && sanitized.is_empty() && range_utf16.is_none() {
            // IME cancelled composition (e.g., Windows WM_IME_COMPOSITION lparam == 0):
            // restore previous committed query cleanly.
            self.buffer.clear();
            self.buffer.push_str(&self.committed);
            self.marked_range = None;
            return None;
        }

        if sanitized.is_empty() && range_utf16.is_none() {
            return None;
        }

        let target = range_utf16
            .map(|r| self.range_from_utf16(r))
            .or_else(|| self.marked_range.take())
            .unwrap_or(self.buffer.len()..self.buffer.len());

        self.buffer.replace_range(target, &sanitized);
        self.marked_range = None;

        if self.buffer != self.committed {
            self.committed.clear();
            self.committed.push_str(&self.buffer);
            Some(self.committed.clone())
        } else {
            None
        }
    }

    /// Cancels/unmarks any active IME preedit without altering the committed query.
    pub(crate) fn unmark_text(&mut self) {
        if self.marked_range.take().is_some() {
            self.buffer.clear();
            self.buffer.push_str(&self.committed);
        }
    }

    /// Removes the last Unicode scalar from the committed query when not composing.
    pub(crate) fn handle_backspace(&mut self, store_query: &str) -> Option<String> {
        if self.marked_range.is_some() {
            return None;
        }
        self.sync_committed(store_query);
        self.committed.pop()?;
        self.buffer.clear();
        self.buffer.push_str(&self.committed);
        Some(self.committed.clone())
    }

    /// Cancels active IME composition (preserving previous query) or clears a non-empty query.
    pub(crate) fn handle_escape(&mut self, store_query: &str) -> Option<String> {
        if self.marked_range.take().is_some() {
            self.buffer.clear();
            self.buffer.push_str(&self.committed);
            return None;
        }
        self.sync_committed(store_query);
        if self.committed.is_empty() {
            return None;
        }
        self.committed.clear();
        self.buffer.clear();
        Some(String::new())
    }

    fn offset_to_utf16(&self, byte_offset: usize) -> usize {
        let clamped = byte_offset.min(self.buffer.len());
        let mut utf16 = 0usize;
        for (idx, ch) in self.buffer.char_indices() {
            if idx >= clamped {
                break;
            }
            utf16 += ch.len_utf16();
        }
        utf16
    }

    fn offset_from_utf16(&self, utf16_offset: usize) -> usize {
        let mut utf16 = 0usize;
        for (idx, ch) in self.buffer.char_indices() {
            if utf16 >= utf16_offset {
                return idx;
            }
            utf16 += ch.len_utf16();
        }
        self.buffer.len()
    }

    fn range_to_utf16(&self, range: Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: Range<usize>) -> Range<usize> {
        let start = self.offset_from_utf16(range.start);
        let end = self.offset_from_utf16(range.end);
        start.min(end)..start.max(end)
    }
}

fn sanitize_search_input(raw: &str) -> String {
    raw.chars().filter(|c| !c.is_control()).collect()
}

/// Zero-size, non-drawing GPUI element that binds an [`EntityInputHandler`] to a table's
/// [`FocusHandle`] during paint so Windows/macOS/Linux IME composition and direct typing
/// work seamlessly whenever the table is focused.
pub(crate) struct TableInputBridge<V: EntityInputHandler> {
    focus_handle: FocusHandle,
    entity: Entity<V>,
}

impl<V: EntityInputHandler> TableInputBridge<V> {
    /// Creates a new [`TableInputBridge`] binding `entity` to `focus_handle`.
    pub(crate) fn new(focus_handle: FocusHandle, entity: Entity<V>) -> Self {
        Self {
            focus_handle,
            entity,
        }
    }
}

impl<V: EntityInputHandler> Element for TableInputBridge<V> {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = Style {
            position: Position::Absolute,
            ..Style::default()
        };
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.handle_input(
            &self.focus_handle,
            ElementInputHandler::new(bounds, self.entity.clone()),
            cx,
        );
    }
}

// ---------------------------------------------------------------------------
// Smart context-aware copying
// ---------------------------------------------------------------------------

/// Formats the default `Ctrl+C` clipboard representation (`"<name> (<pid>)"`).
#[inline]
pub(crate) fn format_process_name_and_pid(name: &str, pid: u32) -> String {
    format!("{name} ({pid})")
}

/// Snapshot of copyable fields for a right-clicked process row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CopyRowFields {
    /// Primary process image/display name.
    pub(crate) name: String,
    /// Process numeric PID.
    pub(crate) pid: u32,
    /// Resolved executable path, when known.
    pub(crate) exe_path: Option<String>,
    /// Resolved command line, when known.
    pub(crate) command_line: Option<String>,
    /// Pre-formatted multi-field row summary (matching `Ctrl+Shift+C`).
    pub(crate) row_summary: String,
    /// Optional `(column_label, cell_text)` for the specific column that was right-clicked.
    pub(crate) clicked_column: Option<(String, String)>,
}

/// Builds the `"Copy >"` row context submenu shared by Processes and Details.
pub(crate) fn build_copy_submenu(
    fields: CopyRowFields,
    window: &mut Window,
    cx: &mut App,
) -> Entity<PopupMenu> {
    PopupMenu::build(window, cx, move |mut sub, _window, _cx| {
        let copy_name = fields.name.clone();
        let copy_pid = fields.pid.to_string();
        let copy_path = fields
            .exe_path
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let copy_cmdline = fields
            .command_line
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let copy_summary = fields.row_summary.clone();
        let clicked_col = fields
            .clicked_column
            .clone()
            .filter(|(_, val)| !val.trim().is_empty() && val.trim() != "—");

        sub = sub
            .item(
                PopupMenuItem::new(i18n::tr("proc_ux_copy_name"))
                    .disabled(copy_name.is_empty())
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_name.clone()));
                    }),
            )
            .item(
                PopupMenuItem::new(i18n::tr("proc_ux_copy_pid")).on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_pid.clone()));
                }),
            )
            .item(
                PopupMenuItem::new(i18n::tr("proc_ux_copy_path"))
                    .disabled(copy_path.is_none())
                    .on_click(move |_, _, cx| {
                        if let Some(ref p) = copy_path {
                            cx.write_to_clipboard(ClipboardItem::new_string(p.clone()));
                        }
                    }),
            )
            .item(
                PopupMenuItem::new(i18n::tr("proc_ux_copy_cmdline"))
                    .disabled(copy_cmdline.is_none())
                    .on_click(move |_, _, cx| {
                        if let Some(ref c) = copy_cmdline {
                            cx.write_to_clipboard(ClipboardItem::new_string(c.clone()));
                        }
                    }),
            )
            .item(
                PopupMenuItem::new(i18n::tr("proc_ux_copy_summary")).on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_summary.clone()));
                }),
            );

        if let Some((col_label, cell_text)) = clicked_col {
            let label =
                rust_i18n::t!("proc_ux_copy_column", column = col_label.as_str()).into_owned();
            sub = sub
                .separator()
                .item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(cell_text.clone()));
                }));
        }

        sub
    })
}

// ---------------------------------------------------------------------------
// Formula-safe CSV export helpers
// ---------------------------------------------------------------------------

/// Formats a UTC timestamped CSV filename (`"<prefix>-YYYYMMDD-HHMMSS.csv"`).
pub(crate) fn format_csv_export_filename(prefix: &str, epoch_secs: u64) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let rem_secs = epoch_secs % 86_400;
    let hour = rem_secs / 3_600;
    let minute = (rem_secs % 3_600) / 60;
    let second = rem_secs % 60;

    // Howard Hinnant's proleptic Gregorian `civil_from_days` algorithm.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(month <= 2);

    format!("{prefix}-{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}.csv")
}

/// Resolves a writable user export directory (`Downloads` -> `Desktop` -> `Documents` -> temp).
pub(crate) fn default_csv_export_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    if let Some(base) = home {
        for sub in ["Downloads", "Desktop", "Documents"] {
            let candidate = base.join(sub);
            if candidate.is_dir() {
                return candidate;
            }
        }
        if base.is_dir() {
            return base;
        }
    }
    std::env::temp_dir()
}

/// Serializes `headers` and `rows` via `loom_ui_kit::build_csv_document` (UTF-8 BOM +
/// RFC 4180 + formula-injection neutralization) and writes the file to `dir.join(filename)`.
pub(crate) fn write_csv_export_file(
    dir: &Path,
    filename: &str,
    headers: &[String],
    rows: &[Vec<String>],
) -> std::io::Result<PathBuf> {
    let header_refs: Vec<&str> = headers.iter().map(String::as_str).collect();
    let content = build_csv_document(&header_refs, rows);
    let dest = dir.join(filename);
    std::fs::write(&dest, content.as_bytes())?;
    Ok(dest)
}

/// Exports visible table headers & rows to a timestamped CSV file off the UI thread and
/// posts a confirmation (or error) [`Notification`] upon completion.
pub(crate) fn spawn_csv_export(
    store: Entity<UiStore>,
    prefix: &'static str,
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    window: &mut Window,
    cx: &mut App,
) {
    let epoch_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let filename = format_csv_export_filename(prefix, epoch_secs);

    window
        .spawn(cx, async move |cx| {
            let write_result = cx
                .background_executor()
                .spawn(async move {
                    let dir = default_csv_export_dir();
                    write_csv_export_file(&dir, &filename, &headers, &rows)
                })
                .await;

            let _ = cx.update(|window, cx| match write_result {
                Ok(saved_path) => {
                    let display_path = saved_path.display().to_string();
                    let msg =
                        rust_i18n::t!("csv_export_success_message", path = display_path.as_str())
                            .into_owned();
                    let open_label = i18n::tr("csv_export_open_location");
                    let store_open = store.clone();
                    let path_open = saved_path.clone();
                    let notice = Notification::success(msg)
                        .title(i18n::tr("csv_export_success_title"))
                        .action(move |_, _, cx| {
                            let store_click = store_open.clone();
                            let path_click = path_open.clone();
                            Button::new("csv-export-open-location")
                                .outline()
                                .xsmall()
                                .label(open_label.clone())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    UiCommands::open_file_location(
                                        &store_click,
                                        path_click.clone(),
                                        cx,
                                    );
                                    this.dismiss(window, cx);
                                }))
                        });
                    window.push_notification(notice, cx);
                }
                Err(err) => {
                    let notice = Notification::error(err.to_string())
                        .title(i18n::tr("csv_export_error_title"));
                    window.push_notification(notice, cx);
                }
            });
        })
        .detach();
}

// ---------------------------------------------------------------------------
// Process controls, WebView2 role tags, and resource leak warning badges
// ---------------------------------------------------------------------------

/// Returns `true` when `image_name` refers to `explorer.exe` (case-insensitive).
#[inline]
pub(crate) fn is_explorer_image(image_name: &str) -> bool {
    image_name.trim().eq_ignore_ascii_case("explorer.exe")
}

/// Returns `true` when a process can safely be suspended or resumed.
#[inline]
pub(crate) fn can_suspend_or_resume_process(
    pid: u32,
    image_name: &str,
    status: ProcessStatusFlags,
    capabilities: CapabilityFlags,
) -> bool {
    capabilities.contains(CapabilityFlags::SUSPEND_RESUME)
        && pid != 0
        && pid != std::process::id()
        && !image_name.trim().eq_ignore_ascii_case("dwm.exe")
        && !status.intersects(
            ProcessStatusFlags::SYSTEM_IDLE
                | ProcessStatusFlags::KERNEL
                | ProcessStatusFlags::PROTECTED
                | ProcessStatusFlags::CRITICAL,
        )
}

/// Returns the best path/name identifier for `EngineCommand::IgnoreLeakPath`.
pub(crate) fn leak_ignore_target(exe_path: Option<&str>, image_name: &str) -> Option<String> {
    if let Some(path) = exe_path.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(path.to_owned());
    }
    let name = image_name.trim();
    (!name.is_empty()).then(|| name.to_owned())
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
pub(crate) fn leak_warning_badge(
    id: (&'static str, usize),
    warning: &LeakWarning,
    leak_color: Hsla,
) -> impl IntoElement {
    let tooltip_text = SharedString::from(format!(
        "{}: {}",
        i18n::tr("leak_warning_badge_prefix"),
        warning.summary
    ));
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

/// Dispatches `EngineCommand::SuspendProcess` or `EngineCommand::ResumeProcess`.
pub(crate) fn dispatch_suspend_or_resume(
    store: &Entity<UiStore>,
    key: ProcessKey,
    currently_suspended: bool,
    cx: &mut App,
) {
    let cmd = if currently_suspended {
        EngineCommand::ResumeProcess(key)
    } else {
        EngineCommand::SuspendProcess(key)
    };
    let _ = store.read(cx).dispatch_command(cmd);
}

/// Dispatches `EngineCommand::RestartExplorer`.
pub(crate) fn dispatch_restart_explorer(store: &Entity<UiStore>, cx: &mut App) {
    let _ = store
        .read(cx)
        .dispatch_command(EngineCommand::RestartExplorer);
}

/// Dispatches `EngineCommand::IgnoreLeakPath` and persists the ignored path in `settings.toml`.
pub(crate) fn dispatch_ignore_leak_path(store: &Entity<UiStore>, target: String, cx: &mut App) {
    let trimmed = target.trim().to_owned();
    if trimmed.is_empty() {
        return;
    }
    store.update(cx, |s, cx| {
        let _ = s.dispatch_command(EngineCommand::IgnoreLeakPath(trimmed.clone()));
        let _ = s.update_settings(
            |cfg| {
                if !cfg
                    .leak_ignored_paths
                    .iter()
                    .any(|p| p.eq_ignore_ascii_case(&trimmed))
                {
                    cfg.leak_ignored_paths.push(trimmed.clone());
                }
            },
            cx,
        );
    });
}

/// Dispatches `EngineCommand::SetHideSystemProcesses` and persists it in `settings.toml`.
pub(crate) fn dispatch_hide_system_processes(store: &Entity<UiStore>, hidden: bool, cx: &mut App) {
    store.update(cx, |s, cx| {
        let _ = s.dispatch_command(EngineCommand::SetHideSystemProcesses(hidden));
        let _ = s.update_settings(|cfg| cfg.hide_windows_system_processes = hidden, cx);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TestCol {
        Name,
        Pid,
        Cpu,
        Memory,
        Publisher,
    }

    impl ColumnKey for TestCol {
        fn slug(self) -> &'static str {
            match self {
                Self::Name => "name",
                Self::Pid => "pid",
                Self::Cpu => "cpu",
                Self::Memory => "memory",
                Self::Publisher => "publisher",
            }
        }
    }

    fn sample_layout() -> TableColumnLayout<TestCol> {
        TableColumnLayout::new(vec![
            ColumnSpec::pinned(TestCol::Name, 200.0, 100.0, 400.0),
            ColumnSpec::hidden(TestCol::Pid, 76.0, 56.0, 160.0),
            ColumnSpec::new(TestCol::Cpu, 80.0, 50.0, 200.0),
            ColumnSpec::new(TestCol::Memory, 100.0, 60.0, 240.0),
            ColumnSpec::new(TestCol::Publisher, 140.0, 80.0, 320.0),
        ])
    }

    #[test]
    fn apply_visible_widths_clamps_to_min_and_max_bounds() {
        let mut layout = sample_layout();

        assert!(layout.apply_visible_widths(&[px(240.), px(120.), px(100.), px(140.)]));
        assert_eq!(layout.visible_column(0).unwrap().width, 240.0);
        assert_eq!(layout.visible_column(1).unwrap().width, 120.0);

        // No-op when widths match current values.
        assert!(!layout.apply_visible_widths(&[px(240.), px(120.), px(100.), px(140.)]));

        // Clamps out-of-range widths to [min_width, max_width].
        assert!(layout.apply_visible_widths(&[px(240.), px(10.), px(999.), px(140.)]));
        assert_eq!(layout.visible_column(1).unwrap().width, 50.0);
        assert_eq!(layout.visible_column(2).unwrap().width, 240.0);
    }

    #[test]
    fn move_visible_column_protects_pinned_columns_and_preserves_hidden() {
        let mut layout = sample_layout();
        // Visible: [Name (pinned), Cpu, Memory, Publisher]; Hidden: Pid.

        // Cannot move pinned Name or move another column before it.
        assert!(!layout.move_visible_column(0, 2));
        assert!(!layout.move_visible_column(1, 0));

        // Move Publisher (vis 3) to vis 1 (immediately after Name).
        assert!(layout.move_visible_column(3, 1));
        assert_eq!(
            layout
                .visible_columns()
                .iter()
                .map(|c| c.id)
                .collect::<Vec<_>>(),
            vec![
                TestCol::Name,
                TestCol::Publisher,
                TestCol::Cpu,
                TestCol::Memory
            ]
        );
        // Hidden Pid column remains intact in all_columns().
        assert!(!layout.is_visible(TestCol::Pid));
        assert_eq!(layout.all_columns().len(), 5);
    }

    #[test]
    fn visibility_hiding_locked_guard_and_select_columns_apply() {
        let mut layout = sample_layout();

        // Pinned Name column cannot be hidden.
        assert!(!layout.hide_column(TestCol::Name));
        assert!(layout.is_visible(TestCol::Name));

        // Hide Publisher.
        assert!(layout.hide_column(TestCol::Publisher));
        assert!(!layout.is_visible(TestCol::Publisher));
        assert_eq!(layout.visible_count(), 3);

        // Re-applying visibility with Pid enabled and Cpu disabled keeps Name visible.
        assert!(layout.apply_visible_slugs(&["pid", "memory"]));
        assert_eq!(
            layout
                .visible_columns()
                .iter()
                .map(|c| c.id)
                .collect::<Vec<_>>(),
            vec![TestCol::Name, TestCol::Pid, TestCol::Memory]
        );

        // Empty selection still preserves the pinned Name column.
        assert!(layout.apply_visible_slugs(&[]));
        assert_eq!(
            layout
                .visible_columns()
                .iter()
                .map(|c| c.id)
                .collect::<Vec<_>>(),
            vec![TestCol::Name]
        );
    }

    #[test]
    fn saved_snapshot_roundtrip_normalizes_pinned_columns_and_guards_corrupt_data() {
        let mut layout = sample_layout();
        assert!(layout.apply_visible_widths(&[px(260.), px(80.), px(100.), px(140.)]));
        assert!(layout.apply_visible_slugs(&["pid", "cpu", "publisher"]));
        assert!(layout.move_visible_column(3, 1)); // move Publisher after Name
        let mut saved = layout.snapshot();

        // Simulate legacy config where Name was moved non-leading, hidden, and oversized.
        saved.swap(0, 1);
        saved.push(TableColumnSetting {
            id: "legacy_removed_col".to_owned(),
            width: 999,
            visible: true,
        });
        if let Some(name_entry) = saved.iter_mut().find(|e| e.id == "name") {
            name_entry.width = 9999; // clamps to max_width (400.0)
            name_entry.visible = false; // locked Name stays visible
        }

        let mut restored = sample_layout();
        restored.apply_saved(&saved);

        assert_eq!(
            restored
                .visible_columns()
                .iter()
                .map(|c| c.id)
                .collect::<Vec<_>>(),
            vec![
                TestCol::Name,
                TestCol::Publisher,
                TestCol::Pid,
                TestCol::Cpu
            ]
        );
        assert_eq!(restored.visible_column(0).unwrap().width, 400.0);
        assert!(!restored.is_visible(TestCol::Memory));
    }

    #[test]
    fn select_columns_draft_tracks_checkbox_toggles() {
        let mut draft = SelectColumnsDraft::new(vec![
            ColumnToggleItem {
                slug: "pid",
                label: "PID".to_owned(),
                visible: false,
            },
            ColumnToggleItem {
                slug: "cpu",
                label: "CPU".to_owned(),
                visible: true,
            },
        ]);
        assert_eq!(draft.selected_slugs(), vec!["cpu"]);
        draft.set_checked(0, true);
        draft.set_checked(1, false);
        assert_eq!(draft.selected_slugs(), vec!["pid"]);
    }

    #[test]
    fn order_freeze_state_coordinates_hover_header_and_context_menu() {
        let mut state = OrderFreezeState::default();
        assert!(!state.is_frozen());

        // Hovering table body freezes order.
        assert_eq!(state.set_table_hovered(true), Some(true));
        assert!(state.is_frozen());
        // Duplicate hover emits no transition.
        assert_eq!(state.set_table_hovered(true), None);

        // Moving onto the column header temporarily unfreezes so clicking sort works immediately.
        assert_eq!(state.set_header_hovered(true), Some(false));
        assert!(!state.is_frozen());
        assert_eq!(state.set_header_hovered(false), Some(true));
        assert!(state.is_frozen());

        // Opening context menu keeps row order frozen even after mouse leaves table.
        assert_eq!(state.set_context_menu_open(true), None);
        assert_eq!(state.set_table_hovered(false), None);
        assert!(state.is_frozen());

        // Dismissing context menu while mouse is outside unfreezes immediately.
        assert_eq!(state.set_context_menu_open(false), Some(false));
        assert!(!state.is_frozen());
    }

    #[test]
    fn type_to_search_state_handles_direct_typing_and_cjk_ime_cancel_commit() {
        let mut search = TypeToSearchState::default();

        // Direct ASCII typing appends without losing the first keystroke.
        assert_eq!(search.replace_text(None, "c", ""), Some("c".to_owned()));
        assert_eq!(search.replace_text(None, "h", "c"), Some("ch".to_owned()));

        // Start CJK IME composition on top of existing query "ch":
        // committed query stays untouched while composing.
        search.replace_and_mark(None, "wei", "ch");
        assert!(search.is_composing());
        assert_eq!(search.preedit_text(), Some("wei"));
        assert_eq!(search.marked_utf16_range(), Some(2..5));

        // Cancelling composition (Escape or empty Windows WM_IME_COMPOSITION commit)
        // leaves previous committed query "ch" intact.
        assert_eq!(search.replace_text(None, "", "ch"), None);
        assert!(!search.is_composing());
        assert_eq!(search.preedit_text(), None);
        assert_eq!(search.text_for_utf16_range(0..2).1, "ch");

        // Second CJK composition -> commit "微信" appends cleanly to "ch".
        search.replace_and_mark(None, "wx", "ch");
        assert_eq!(search.preedit_text(), Some("wx"));
        assert_eq!(
            search.replace_text(None, "微信", "ch"),
            Some("ch微信".to_owned())
        );
        assert!(!search.is_composing());
        assert_eq!(search.utf16_len(), 4);

        // Backspace pops one full Unicode scalar ("信").
        assert_eq!(search.handle_backspace("ch微信"), Some("ch微".to_owned()));

        // Escape cancels composition if active, otherwise clears query.
        search.replace_and_mark(None, "pin", "ch微");
        assert_eq!(search.handle_escape("ch微"), None);
        assert!(!search.is_composing());
        assert_eq!(search.handle_escape("ch微"), Some(String::new()));
    }

    #[test]
    fn csv_export_filename_and_formula_safe_write_output() {
        // 2026-11-28 02:16:52 UTC = 1795832212? Let's test Unix epoch 0 and a known date.
        assert_eq!(
            format_csv_export_filename("loom-processes", 0),
            "loom-processes-19700101-000000.csv"
        );
        // 2026-01-02 03:04:05 UTC = 1767323045
        assert_eq!(
            format_csv_export_filename("loom-details", 1_767_323_045),
            "loom-details-20260102-030405.csv"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let headers = vec![
            "Name".to_owned(),
            "PID".to_owned(),
            "Command line".to_owned(),
        ];
        let rows = vec![vec![
            "app.exe".to_owned(),
            "1234".to_owned(),
            "=CMD|'/C calc'!A0".to_owned(),
        ]];
        let written =
            write_csv_export_file(dir.path(), "export.csv", &headers, &rows).expect("write csv");
        let content = std::fs::read_to_string(&written).expect("read csv");
        assert!(content.starts_with(loom_ui_kit::CSV_UTF8_BOM));
        assert!(content.contains("Name,PID,Command line\r\n"));
        assert!(content.contains("app.exe,1234,'=CMD|'/C calc'!A0\r\n"));
    }

    #[test]
    fn suspend_resume_explorer_and_leak_target_gating() {
        let with_cap = CapabilityFlags::SUSPEND_RESUME;
        let no_cap = CapabilityFlags::empty();

        assert!(can_suspend_or_resume_process(
            4242,
            "notepad.exe",
            ProcessStatusFlags::empty(),
            with_cap
        ));
        assert!(!can_suspend_or_resume_process(
            4242,
            "notepad.exe",
            ProcessStatusFlags::empty(),
            no_cap
        ));
        assert!(!can_suspend_or_resume_process(
            0,
            "System Idle Process",
            ProcessStatusFlags::SYSTEM_IDLE,
            with_cap
        ));
        assert!(!can_suspend_or_resume_process(
            std::process::id(),
            "loom-app.exe",
            ProcessStatusFlags::empty(),
            with_cap
        ));
        assert!(!can_suspend_or_resume_process(
            512,
            "DWM.EXE",
            ProcessStatusFlags::empty(),
            with_cap
        ));
        assert!(!can_suspend_or_resume_process(
            600,
            "csrss.exe",
            ProcessStatusFlags::CRITICAL,
            with_cap
        ));

        assert!(is_explorer_image("explorer.exe"));
        assert!(is_explorer_image("EXPLORER.EXE"));
        assert!(!is_explorer_image("iexplore.exe"));

        assert_eq!(
            leak_ignore_target(Some(r" C:\Apps\leak.exe "), "leak.exe"),
            Some(r"C:\Apps\leak.exe".to_owned())
        );
        assert_eq!(
            leak_ignore_target(None, "leak.exe"),
            Some("leak.exe".to_owned())
        );
        assert_eq!(leak_ignore_target(Some("  "), "   "), None);
    }

    #[test]
    fn processes_and_details_ux_locale_keys_exist_in_en_and_zh() {
        let keys = [
            "proc_ux_sort_paused_tooltip",
            "proc_ux_hide_system_tooltip",
            "proc_ux_copy_submenu",
            "proc_ux_copy_name",
            "proc_ux_copy_pid",
            "proc_ux_copy_path",
            "proc_ux_copy_cmdline",
            "proc_ux_copy_summary",
            "proc_ux_menu_suspend",
            "proc_ux_menu_resume",
            "proc_ux_menu_restart_explorer",
            "proc_ux_ime_searching",
            "csv_export_button",
            "csv_export_success_title",
            "csv_export_error_title",
            "csv_export_open_location",
            "leak_warning_ignore_menu",
            "leak_warning_badge_prefix",
            "webview2_role_browser",
            "webview2_role_renderer",
            "webview2_role_gpu",
            "webview2_role_network",
            "webview2_role_storage",
            "webview2_role_audio",
            "webview2_role_utility",
            "webview2_role_crashpad",
        ];
        for key in keys {
            let en = rust_i18n::t!(key, locale = "en-US");
            let zh = rust_i18n::t!(key, locale = "zh-CN");
            assert!(!en.is_empty() && en != key, "missing en-US for {key}");
            assert!(!zh.is_empty() && zh != key, "missing zh-CN for {key}");
        }
        assert_eq!(
            rust_i18n::t!(
                "proc_ux_copy_column",
                locale = "en-US",
                column = "Publisher"
            ),
            "Copy \"Publisher\""
        );
        assert_eq!(
            rust_i18n::t!("proc_ux_copy_column", locale = "zh-CN", column = "发布者"),
            "复制“发布者”"
        );
    }
}
