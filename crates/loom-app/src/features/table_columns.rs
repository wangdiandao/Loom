//! Reusable column catalog, visibility/order/width persistence, header context menu,
//! and **Select columns** dialog shared by the Processes and Details `DataTable` slices.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::table::{DataTable, TableDelegate, TableState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, Size, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Bounds, ClickEvent, ClipboardItem, Context, Element, ElementId,
    ElementInputHandler, Empty, Entity, EntityInputHandler, FocusHandle, GlobalElementId, Hsla,
    InspectorElementId, InteractiveElement as _, IntoElement, LayoutId, MouseButton,
    ParentElement as _, Pixels, Position, Render, RenderImage, SharedString,
    StatefulInteractiveElement as _, Style, Styled as _, Window, div, px,
};
use loom_config::TableColumnSetting;
use loom_engine::EngineCommand;
use loom_types::{
    CapabilityFlags, ExeIconRgba, LeakWarning, ProcessKey, ProcessStatusFlags, WebView2Role,
};
use loom_ui_kit::{
    CachedSemanticColors, DEFAULT_CELL_PREVIEW_CHARS, build_csv_document, rgba_icon_to_render_image,
};

use crate::bridge::{UiCommands, UiStore};
use crate::i18n;

/// Maximum number of cached GPUI [`RenderImage`] icons retained by [`IconRenderCache`].
pub(crate) const MAX_CACHED_ICONS: usize = 512;

/// Maximum number of newly decoded icons converted into GPU [`RenderImage`] handles within
/// a single UI frame before remaining uncached icons defer to the next frame.
///
/// Prevents frame-time spikes when rapidly scrolling past dozens of never-seen processes;
/// cache hits consume zero budget.
pub(crate) const MAX_ICON_UPLOADS_PER_FRAME: usize = 6;

/// Outcome of a budgeted [`IconRenderCache::get_or_insert_budgeted`] lookup.
#[derive(Clone)]
pub(crate) enum BudgetedIconLookup {
    /// Icon was already cached (consumed zero frame upload budget).
    Hit(Arc<RenderImage>),
    /// Icon was converted and uploaded within the current frame's budget.
    Uploaded(Arc<RenderImage>),
    /// Per-frame upload budget was exhausted; caller should render a lightweight placeholder
    /// and schedule a next-frame catch-up pass.
    Deferred,
    /// Underlying RGBA payload was invalid and could not be converted.
    Invalid,
}

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

/// Bounded local cache mapping unique [`ExeIconRgba`] buffers to GPUI [`RenderImage`] handles
/// with optional per-frame upload budgeting to avoid GPU texture upload spikes during fast scrolling.
///
/// Prevents re-swapping RGBA -> BGRA pixels and re-uploading textures on every frame.
pub(crate) struct IconRenderCache {
    entries: HashMap<IconCacheKey, IconCacheEntry>,
    max_entries: usize,
    max_uploads_per_frame: usize,
    uploads_this_frame: usize,
    deferred_this_frame: bool,
}

impl Default for IconRenderCache {
    fn default() -> Self {
        Self::new()
    }
}

impl IconRenderCache {
    /// Creates a new bounded [`IconRenderCache`] capped at [`MAX_CACHED_ICONS`] and
    /// [`MAX_ICON_UPLOADS_PER_FRAME`] new uploads per frame.
    pub(crate) fn new() -> Self {
        Self::with_limits(MAX_CACHED_ICONS, MAX_ICON_UPLOADS_PER_FRAME)
    }

    /// Creates a new bounded [`IconRenderCache`] with a custom capacity ceiling.
    #[cfg(test)]
    pub(crate) fn with_capacity(max_entries: usize) -> Self {
        Self::with_limits(max_entries, MAX_ICON_UPLOADS_PER_FRAME)
    }

    /// Creates a new bounded [`IconRenderCache`] with custom capacity and per-frame upload budget.
    pub(crate) fn with_limits(max_entries: usize, max_uploads_per_frame: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_entries: max_entries.max(1),
            max_uploads_per_frame: max_uploads_per_frame.max(1),
            uploads_this_frame: 0,
            deferred_this_frame: false,
        }
    }

    /// Resets the per-frame GPU icon upload budget at the start of a new frame.
    #[inline]
    pub(crate) fn begin_frame(&mut self) {
        self.uploads_this_frame = 0;
        self.deferred_this_frame = false;
    }

    /// Returns whether at least one uncached icon was deferred during the current frame.
    #[cfg(test)]
    #[inline]
    pub(crate) const fn has_deferred_uploads(&self) -> bool {
        self.deferred_this_frame
    }

    /// Returns a cached [`Arc<RenderImage>`] for `icon`, converting it unconditionally on miss.
    #[cfg(test)]
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

    /// Returns a cached [`Arc<RenderImage>`] for `icon` while enforcing the per-frame upload budget.
    ///
    /// Cache hits always return [`BudgetedIconLookup::Hit`] without consuming budget. Cache misses
    /// beyond `max_uploads_per_frame` return [`BudgetedIconLookup::Deferred`] so the caller can
    /// render a lightweight placeholder icon and schedule a next-frame catch-up pass.
    pub(crate) fn get_or_insert_budgeted(&mut self, icon: &ExeIconRgba) -> BudgetedIconLookup {
        let key = IconCacheKey::from_icon(icon);
        if let Some(entry) = self.entries.get(&key)
            && entry
                .source
                .upgrade()
                .is_some_and(|existing| Arc::ptr_eq(&existing, &icon.rgba))
        {
            return BudgetedIconLookup::Hit(Arc::clone(&entry.image));
        }

        if self.uploads_this_frame >= self.max_uploads_per_frame {
            self.deferred_this_frame = true;
            return BudgetedIconLookup::Deferred;
        }

        let Some(rendered) = rgba_icon_to_render_image(icon) else {
            return BudgetedIconLookup::Invalid;
        };

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
        self.uploads_this_frame += 1;
        BudgetedIconLookup::Uploaded(rendered)
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

    /// Returns the number of leading left-pinned (`locked == true`) visible columns.
    #[inline]
    pub(crate) fn pinned_count(&self) -> usize {
        self.visible.iter().take_while(|c| c.locked).count()
    }

    /// Computes a 64-bit bitmask of visible column display indices currently inside the viewport.
    ///
    /// Leading pinned columns (`0..pinned_count`) are always marked visible. When `scrollable_range`
    /// is empty (before the first horizontal virtual-list measurement), all visible columns are
    /// included so viewport fingerprinting stays conservative on initial paint.
    pub(crate) fn visible_cols_mask(&self, scrollable_range: &Range<usize>) -> u64 {
        let total = self.visible.len().min(64);
        if total == 0 {
            return 0;
        }
        if scrollable_range.is_empty() {
            return if total == 64 {
                u64::MAX
            } else {
                (1u64 << total) - 1
            };
        }
        let pinned = self.pinned_count().min(total);
        let mut mask = if pinned == 64 {
            u64::MAX
        } else if pinned == 0 {
            0
        } else {
            (1u64 << pinned) - 1
        };
        let start = pinned.saturating_add(scrollable_range.start).min(total);
        let end = pinned.saturating_add(scrollable_range.end).min(total);
        for ix in start..end {
            mask |= 1u64 << ix;
        }
        mask
    }

    /// Resolves the visible column display index hit by a pointer click at `local_x`
    /// relative to the table's left edge, accounting for leading left-pinned columns
    /// and horizontal scroll distance `scroll_x_positive` (`(-offset.x).max(0.0)`).
    ///
    /// Returns `None` if `local_x` is negative, non-finite, or past the right edge of
    /// the last visible column.
    pub(crate) fn column_index_at_x(&self, local_x: f32, scroll_x_positive: f32) -> Option<usize> {
        if !local_x.is_finite() || local_x < 0.0 || self.visible.is_empty() {
            return None;
        }

        let mut pinned_width = 0.0_f32;
        let mut pinned_count = 0usize;
        for (ix, spec) in self.visible.iter().enumerate() {
            if !spec.locked {
                break;
            }
            let next = pinned_width + spec.width;
            if local_x < next {
                return Some(ix);
            }
            pinned_width = next;
            pinned_count = ix + 1;
        }

        let scroll_x = if scroll_x_positive.is_finite() {
            scroll_x_positive.max(0.0)
        } else {
            0.0
        };
        let content_x = (local_x - pinned_width) + scroll_x;
        let mut cursor = 0.0_f32;
        for (ix, spec) in self.visible.iter().enumerate().skip(pinned_count) {
            let next = cursor + spec.width;
            if content_x < next {
                return Some(ix);
            }
            cursor = next;
        }
        None
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
        let border = cx.theme().border;

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

// ---------------------------------------------------------------------------
// Zero-allocation atomic list-rendering telemetry (120Hz frame-budget diagnostics)
// ---------------------------------------------------------------------------

/// High-frequency virtualized table views tracked by [`ListTelemetryRegistry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum ListViewTelemetryKind {
    Processes = 0,
    Details = 1,
}

impl ListViewTelemetryKind {
    #[inline]
    const fn index(self) -> usize {
        self as usize
    }

    #[inline]
    const fn label(self) -> &'static str {
        match self {
            Self::Processes => "Processes",
            Self::Details => "Details",
        }
    }
}

/// Immutable point-in-time snapshot of one table's rendering telemetry counters.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ListTelemetrySnapshot {
    pub(crate) commits: u64,
    pub(crate) viewport_fp_skips: u64,
    pub(crate) scroll_defers: u64,
    pub(crate) icon_uploads: u64,
    pub(crate) icon_defers: u64,
    pub(crate) last_commit_us: u64,
    pub(crate) max_commit_us: u64,
    pub(crate) render_td_calls: u64,
}

struct ListViewTelemetryCounters {
    commits: AtomicU64,
    viewport_fp_skips: AtomicU64,
    scroll_defers: AtomicU64,
    icon_uploads: AtomicU64,
    icon_defers: AtomicU64,
    last_commit_us: AtomicU64,
    max_commit_us: AtomicU64,
    render_td_calls: AtomicU64,
}

impl ListViewTelemetryCounters {
    const fn new() -> Self {
        Self {
            commits: AtomicU64::new(0),
            viewport_fp_skips: AtomicU64::new(0),
            scroll_defers: AtomicU64::new(0),
            icon_uploads: AtomicU64::new(0),
            icon_defers: AtomicU64::new(0),
            last_commit_us: AtomicU64::new(0),
            max_commit_us: AtomicU64::new(0),
            render_td_calls: AtomicU64::new(0),
        }
    }

    #[inline]
    fn snapshot(&self) -> ListTelemetrySnapshot {
        ListTelemetrySnapshot {
            commits: self.commits.load(Ordering::Relaxed),
            viewport_fp_skips: self.viewport_fp_skips.load(Ordering::Relaxed),
            scroll_defers: self.scroll_defers.load(Ordering::Relaxed),
            icon_uploads: self.icon_uploads.load(Ordering::Relaxed),
            icon_defers: self.icon_defers.load(Ordering::Relaxed),
            last_commit_us: self.last_commit_us.load(Ordering::Relaxed),
            max_commit_us: self.max_commit_us.load(Ordering::Relaxed),
            render_td_calls: self.render_td_calls.load(Ordering::Relaxed),
        }
    }
}

/// Lock-free, zero-allocation atomic counter registry tracking UI frame-budget efficiency
/// for high-frequency virtualized tables.
///
/// Contains strictly aggregate numeric counters (no PIDs, names, or paths) and is safe to
/// surface in `tracing::debug!` logs and the privacy-safe Diagnostic Summary.
pub(crate) struct ListTelemetryRegistry {
    views: [ListViewTelemetryCounters; 2],
}

static LIST_TELEMETRY: ListTelemetryRegistry = ListTelemetryRegistry::new();

/// Returns the process-wide [`ListTelemetryRegistry`].
#[inline]
pub(crate) fn list_telemetry() -> &'static ListTelemetryRegistry {
    &LIST_TELEMETRY
}

impl ListTelemetryRegistry {
    /// Creates a zeroed [`ListTelemetryRegistry`].
    pub(crate) const fn new() -> Self {
        Self {
            views: [
                ListViewTelemetryCounters::new(),
                ListViewTelemetryCounters::new(),
            ],
        }
    }

    /// Records a `commit_snapshot` completion, its wall duration in microseconds, and
    /// whether the viewport fingerprint check triggered a repaint (`table_notified`).
    #[inline]
    pub(crate) fn record_commit(
        &self,
        kind: ListViewTelemetryKind,
        duration_us: u64,
        table_notified: bool,
    ) {
        let c = &self.views[kind.index()];
        let commits = c.commits.fetch_add(1, Ordering::Relaxed) + 1;
        if !table_notified {
            c.viewport_fp_skips.fetch_add(1, Ordering::Relaxed);
        }
        c.last_commit_us.store(duration_us, Ordering::Relaxed);
        c.max_commit_us.fetch_max(duration_us, Ordering::Relaxed);

        if commits.is_multiple_of(64) {
            let snap = c.snapshot();
            tracing::debug!(
                view = kind.label(),
                commits = snap.commits,
                fp_skips = snap.viewport_fp_skips,
                scroll_defers = snap.scroll_defers,
                icon_uploads = snap.icon_uploads,
                icon_defers = snap.icon_defers,
                last_commit_us = snap.last_commit_us,
                max_commit_us = snap.max_commit_us,
                render_td_calls = snap.render_td_calls,
                "list render telemetry sample"
            );
        }
    }

    /// Records a snapshot deferred during an active scroll burst.
    #[inline]
    pub(crate) fn record_scroll_defer(&self, kind: ListViewTelemetryKind) {
        self.views[kind.index()]
            .scroll_defers
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records a newly converted GPU `RenderImage` icon upload.
    #[inline]
    pub(crate) fn record_icon_upload(&self, kind: ListViewTelemetryKind) {
        self.views[kind.index()]
            .icon_uploads
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records an uncached icon deferred to the next frame by the per-frame upload cap.
    #[inline]
    pub(crate) fn record_icon_defer(&self, kind: ListViewTelemetryKind) {
        self.views[kind.index()]
            .icon_defers
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records a single `TableDelegate::render_td` cell evaluation.
    #[inline]
    pub(crate) fn record_render_td(&self, kind: ListViewTelemetryKind) {
        self.views[kind.index()]
            .render_td_calls
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Reads an atomic snapshot of counters for `kind`.
    #[inline]
    pub(crate) fn snapshot(&self, kind: ListViewTelemetryKind) -> ListTelemetrySnapshot {
        self.views[kind.index()].snapshot()
    }

    /// Formats a privacy-safe single-line diagnostic summary across views that recorded
    /// at least one snapshot commit, or `None` if no list commits have occurred yet.
    pub(crate) fn format_diagnostic_line(&self) -> Option<String> {
        use std::fmt::Write as _;

        let mut parts: Vec<String> = Vec::with_capacity(2);
        for kind in [
            ListViewTelemetryKind::Processes,
            ListViewTelemetryKind::Details,
        ] {
            let s = self.snapshot(kind);
            if s.commits == 0 {
                continue;
            }
            let mut seg = String::with_capacity(96);
            let _ = write!(
                seg,
                "{}(commits={}, fp_skips={}, scroll_defers={}, icon_uploads={}, icon_defers={}, max_commit={}us)",
                kind.label(),
                s.commits,
                s.viewport_fp_skips,
                s.scroll_defers,
                s.icon_uploads,
                s.icon_defers,
                s.max_commit_us,
            );
            parts.push(seg);
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("; "))
        }
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
        .cursor_pointer()
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

// ---------------------------------------------------------------------------
// Scroll-burst coalescer & deterministic viewport fingerprint hasher
// ---------------------------------------------------------------------------

/// Default quiet period after the latest scroll tick before deferred snapshots flush (`150 ms`).
pub(crate) const DEFAULT_SCROLL_QUIET_WINDOW: Duration = Duration::from_millis(150);

/// Default upper bound on snapshot deferral during continuous scrolling (`500 ms`).
pub(crate) const DEFAULT_MAX_DEFER_WINDOW: Duration = Duration::from_millis(500);

/// Pure, deterministic state machine that defers periodic live snapshot notifications
/// during active table scroll bursts so `gpui-fast`'s GPU scroll layers remain intact.
///
/// User-initiated updates (search filter, sort change, view mode change, expand/collapse,
/// or jump-to-row navigation) always bypass deferral immediately.
#[derive(Debug, Clone)]
pub(crate) struct ScrollBurstCoalescer {
    scroll_quiet_window: Duration,
    max_defer_window: Duration,
    last_scroll_at: Option<Instant>,
    first_deferred_at: Option<Instant>,
    has_pending: bool,
}

impl Default for ScrollBurstCoalescer {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl ScrollBurstCoalescer {
    /// Creates a [`ScrollBurstCoalescer`] with the default `150 ms` quiet window and
    /// `500 ms` maximum deferral cap.
    #[inline]
    pub(crate) const fn new() -> Self {
        Self::with_windows(DEFAULT_SCROLL_QUIET_WINDOW, DEFAULT_MAX_DEFER_WINDOW)
    }

    /// Creates a [`ScrollBurstCoalescer`] with custom quiet and maximum deferral windows.
    #[inline]
    pub(crate) const fn with_windows(
        scroll_quiet_window: Duration,
        max_defer_window: Duration,
    ) -> Self {
        Self {
            scroll_quiet_window,
            max_defer_window,
            last_scroll_at: None,
            first_deferred_at: None,
            has_pending: false,
        }
    }

    /// Records a scroll event at `now`.
    #[inline]
    pub(crate) fn on_scroll(&mut self, now: Instant) {
        self.last_scroll_at = Some(now);
    }

    /// Returns `true` if a live snapshot arriving at `now` should be deferred until scrolling
    /// settles or the maximum deferral window elapses.
    ///
    /// User-initiated updates (`is_user_initiated == true`) clear any pending deferred state
    /// and always return `false`.
    pub(crate) fn should_defer_snapshot(&mut self, now: Instant, is_user_initiated: bool) -> bool {
        if is_user_initiated {
            self.has_pending = false;
            self.first_deferred_at = None;
            return false;
        }

        if let Some(last_scroll) = self.last_scroll_at
            && now.saturating_duration_since(last_scroll) < self.scroll_quiet_window
        {
            if let Some(first_deferred) = self.first_deferred_at
                && now.saturating_duration_since(first_deferred) >= self.max_defer_window
            {
                self.has_pending = false;
                self.first_deferred_at = None;
                return false;
            }
            self.has_pending = true;
            self.first_deferred_at.get_or_insert(now);
            return true;
        }

        self.has_pending = false;
        self.first_deferred_at = None;
        false
    }

    /// Returns how long a flush timer should wait before applying the pending snapshot,
    /// bounded by both the scroll quiet window and the maximum deferral cap.
    ///
    /// Returns `None` if no snapshot is currently pending.
    pub(crate) fn remaining_quiet_delay(&self, now: Instant) -> Option<Duration> {
        if !self.has_pending {
            return None;
        }
        let quiet_rem = self
            .last_scroll_at
            .map(|last| {
                self.scroll_quiet_window
                    .saturating_sub(now.saturating_duration_since(last))
            })
            .unwrap_or(Duration::ZERO);
        let cap_rem = self
            .first_deferred_at
            .map(|first| {
                self.max_defer_window
                    .saturating_sub(now.saturating_duration_since(first))
            })
            .unwrap_or(quiet_rem);
        Some(quiet_rem.min(cap_rem))
    }

    /// Clears deferred state and returns whether a pending snapshot existed.
    #[inline]
    pub(crate) fn take_pending(&mut self) -> bool {
        let had = self.has_pending;
        self.has_pending = false;
        self.first_deferred_at = None;
        had
    }
}

/// Fast, deterministic 64-bit FNV-1a hasher for computing table viewport fingerprints
/// without `RandomState` initialization or heap allocations.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FnvHasher64(u64);

impl Default for FnvHasher64 {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl FnvHasher64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    /// Creates a new [`FnvHasher64`] initialized with the 64-bit FNV-1a offset basis.
    #[inline]
    pub(crate) const fn new() -> Self {
        Self(Self::OFFSET_BASIS)
    }

    /// Mixes a byte slice into the hash state.
    #[inline]
    pub(crate) fn write_bytes(&mut self, bytes: &[u8]) {
        let mut hash = self.0;
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(Self::PRIME);
        }
        self.0 = hash;
    }

    /// Mixes a single `u8` into the hash state.
    #[inline]
    pub(crate) fn write_u8(&mut self, v: u8) {
        self.0 ^= u64::from(v);
        self.0 = self.0.wrapping_mul(Self::PRIME);
    }

    /// Mixes a `bool` into the hash state.
    #[inline]
    pub(crate) fn write_bool(&mut self, v: bool) {
        self.write_u8(u8::from(v));
    }

    /// Mixes a `u16` into the hash state.
    #[inline]
    pub(crate) fn write_u16(&mut self, v: u16) {
        self.write_bytes(&v.to_le_bytes());
    }

    /// Mixes a `u32` into the hash state.
    #[inline]
    pub(crate) fn write_u32(&mut self, v: u32) {
        self.write_bytes(&v.to_le_bytes());
    }

    /// Mixes a `u64` into the hash state.
    #[inline]
    pub(crate) fn write_u64(&mut self, v: u64) {
        self.write_bytes(&v.to_le_bytes());
    }

    /// Mixes a `usize` into the hash state.
    #[inline]
    pub(crate) fn write_usize(&mut self, v: usize) {
        self.write_u64(v as u64);
    }

    /// Mixes a length-delimited string slice into the hash state.
    #[inline]
    pub(crate) fn write_str(&mut self, s: &str) {
        self.write_usize(s.len());
        self.write_bytes(s.as_bytes());
    }

    /// Mixes a bounded head+tail sample of a potentially multi-kilobyte string (e.g. command line)
    /// into the hash state in `O(1)` time.
    #[inline]
    pub(crate) fn write_preview_str(&mut self, s: &str) {
        let bytes = s.as_bytes();
        self.write_usize(bytes.len());
        if bytes.len() <= 320 {
            self.write_bytes(bytes);
        } else {
            self.write_bytes(&bytes[..256]);
            self.write_bytes(&bytes[bytes.len() - 64..]);
        }
    }

    /// Returns the final 64-bit fingerprint.
    #[inline]
    pub(crate) const fn finish(self) -> u64 {
        self.0
    }
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

    #[test]
    fn column_index_at_x_handles_pinned_scroll_bounds_and_layout_mutations() {
        let mut layout = sample_layout();
        // Visible initially: [Name (pinned, 200), Cpu (80), Memory (100), Publisher (140)].
        // Total width = 520.

        // Negative or non-finite coordinates never match.
        assert_eq!(layout.column_index_at_x(-1.0, 0.0), None);
        assert_eq!(layout.column_index_at_x(f32::NAN, 0.0), None);
        assert_eq!(layout.column_index_at_x(f32::INFINITY, 0.0), None);

        // Left-pinned Name column [0, 200) hits index 0 regardless of horizontal scroll.
        assert_eq!(layout.column_index_at_x(0.0, 0.0), Some(0));
        assert_eq!(layout.column_index_at_x(199.5, 0.0), Some(0));
        assert_eq!(layout.column_index_at_x(50.0, 120.0), Some(0));
        assert_eq!(layout.column_index_at_x(199.9, 250.0), Some(0));

        // Scrollable columns at scroll_x = 0:
        // Cpu: [200, 280), Memory: [280, 380), Publisher: [380, 520), past end: >= 520.
        assert_eq!(layout.column_index_at_x(200.0, 0.0), Some(1));
        assert_eq!(layout.column_index_at_x(279.9, 0.0), Some(1));
        assert_eq!(layout.column_index_at_x(280.0, 0.0), Some(2));
        assert_eq!(layout.column_index_at_x(379.9, 0.0), Some(2));
        assert_eq!(layout.column_index_at_x(380.0, 0.0), Some(3));
        assert_eq!(layout.column_index_at_x(519.9, 0.0), Some(3));
        assert_eq!(layout.column_index_at_x(520.0, 0.0), None);
        assert_eq!(layout.column_index_at_x(900.0, 0.0), None);

        // After scrolling right by 80 px, Cpu is scrolled under Name, so local_x = 210 hits Memory (2).
        assert_eq!(layout.column_index_at_x(210.0, 80.0), Some(2));
        // Scrolling right by 180 px brings Publisher to local_x = 200.
        assert_eq!(layout.column_index_at_x(200.0, 180.0), Some(3));
        // Past Publisher when scrolled right by 180 px: content_x = (340 - 200) + 180 = 320 -> None.
        assert_eq!(layout.column_index_at_x(340.0, 180.0), None);

        // After hiding Cpu, reordering Publisher before Memory, and resizing Name to 150 px:
        assert!(layout.hide_column(TestCol::Cpu));
        // Visible: [Name (200), Memory (100), Publisher (140)]
        assert!(layout.move_visible_column(2, 1));
        // Visible: [Name (200), Publisher (140), Memory (100)]
        assert!(layout.apply_visible_widths(&[px(150.0), px(140.0), px(100.0)]));
        // Pinned Name is now [0, 150), Publisher [150, 290), Memory [290, 390).
        assert_eq!(layout.column_index_at_x(149.0, 0.0), Some(0));
        assert_eq!(layout.column_index_at_x(150.0, 0.0), Some(1));
        assert_eq!(layout.column_index_at_x(290.0, 0.0), Some(2));
        assert_eq!(layout.column_index_at_x(390.0, 0.0), None);
        // With scroll_x = 140, local_x = 155 lands on Memory (index 2).
        assert_eq!(layout.column_index_at_x(155.0, 140.0), Some(2));
    }

    #[test]
    fn visible_cols_mask_always_includes_pinned_and_handles_empty_range() {
        let layout = sample_layout();
        // 4 visible columns: Name (pinned), Cpu, Memory, Publisher.
        assert_eq!(layout.pinned_count(), 1);
        // Empty range (initial paint) conservatively marks all 4 columns visible.
        assert_eq!(layout.visible_cols_mask(&(0..0)), 0b1111);
        // Scrollable range 0..1 marks Name (bit 0) and Cpu (bit 1).
        assert_eq!(layout.visible_cols_mask(&(0..1)), 0b0011);
        // Scrollable range 1..3 marks Name (bit 0), Memory (bit 2), Publisher (bit 3).
        assert_eq!(layout.visible_cols_mask(&(1..3)), 0b1101);
    }

    #[test]
    fn scroll_burst_coalescer_defers_during_scroll_flushes_on_quiet_cap_and_user_action() {
        let quiet = Duration::from_millis(150);
        let cap = Duration::from_millis(500);
        let mut coalescer = ScrollBurstCoalescer::with_windows(quiet, cap);
        let t0 = Instant::now();

        // 1. No prior scroll -> apply immediately.
        assert!(!coalescer.should_defer_snapshot(t0, false));
        assert_eq!(coalescer.remaining_quiet_delay(t0), None);
        assert!(!coalescer.take_pending());

        // 2. During active scroll -> defer and report remaining quiet delay.
        coalescer.on_scroll(t0);
        let t_50 = t0 + Duration::from_millis(50);
        assert!(coalescer.should_defer_snapshot(t_50, false));
        assert_eq!(
            coalescer.remaining_quiet_delay(t_50),
            Some(Duration::from_millis(100))
        );

        // 3. Quiet window elapsed (>= 150ms since last scroll) -> flush immediately.
        let t_150 = t0 + Duration::from_millis(150);
        assert_eq!(coalescer.remaining_quiet_delay(t_150), Some(Duration::ZERO));
        assert!(!coalescer.should_defer_snapshot(t_150, false));
        assert!(!coalescer.take_pending());

        // 4. Continuous scroll past max_defer_window (500ms) -> cap reached, force flush!
        let base = t0 + Duration::from_secs(2);
        coalescer.on_scroll(base);
        assert!(coalescer.should_defer_snapshot(base + Duration::from_millis(10), false));
        // Keep scrolling every 100ms (never quiet for 150ms) until 510ms elapsed since first defer.
        for step in 1..=4 {
            let now = base + Duration::from_millis(step * 100);
            coalescer.on_scroll(now);
            assert!(
                coalescer.should_defer_snapshot(now + Duration::from_millis(10), false),
                "step {step} should still defer before 500ms cap"
            );
        }
        // At 490ms since first_deferred (base + 10ms), remaining_quiet_delay is clamped to 10ms.
        let t_near_cap = base + Duration::from_millis(500);
        coalescer.on_scroll(t_near_cap);
        assert_eq!(
            coalescer.remaining_quiet_delay(t_near_cap),
            Some(Duration::from_millis(10))
        );
        // At base + 510ms (500ms after first defer at base + 10ms), cap triggers flush.
        let t_cap = base + Duration::from_millis(510);
        assert!(!coalescer.should_defer_snapshot(t_cap, false));
        assert!(!coalescer.take_pending());

        // 5. User-initiated snapshot always bypasses deferral immediately, even mid-scroll.
        let t_user = base + Duration::from_secs(2);
        coalescer.on_scroll(t_user);
        assert!(coalescer.should_defer_snapshot(t_user + Duration::from_millis(20), false));
        assert!(!coalescer.should_defer_snapshot(t_user + Duration::from_millis(30), true));
        assert!(!coalescer.take_pending());
    }

    #[test]
    fn should_attach_cell_tooltip_only_when_text_can_overflow_column_width() {
        // Short ASCII path fits comfortably in a 300px column -> skip stateful tooltip.
        assert!(!should_attach_cell_tooltip(r"C:\app.exe", 300.0));
        // Long ASCII command line exceeds 300px column -> attach tooltip.
        assert!(should_attach_cell_tooltip(
            r#""C:\Program Files\Example\app.exe" --profile-directory=Default --enable-features=Fast"#,
            300.0
        ));
        // Wide CJK path counts double width and triggers tooltip on narrow columns.
        assert!(should_attach_cell_tooltip(
            "C:\\用户\\测试目录\\应用程序主程序.exe",
            180.0
        ));
        // Texts exceeding DEFAULT_CELL_PREVIEW_CHARS always attach tooltip regardless of width.
        let huge = "a".repeat(DEFAULT_CELL_PREVIEW_CHARS + 10);
        assert!(should_attach_cell_tooltip(&huge, 5000.0));
    }

    #[test]
    fn icon_render_cache_enforces_per_frame_upload_budget_and_zero_cost_hits() {
        let mut cache = IconRenderCache::with_limits(16, 2);
        let mk_icon = |seed: u8| ExeIconRgba {
            width: 2,
            height: 2,
            rgba: Arc::from([seed; 16]),
        };

        let icon1 = mk_icon(1);
        let icon2 = mk_icon(2);
        let icon3 = mk_icon(3);

        // Frame 1: first two uncached icons upload, cache hits consume zero budget,
        // third uncached icon defers.
        cache.begin_frame();
        assert!(matches!(
            cache.get_or_insert_budgeted(&icon1),
            BudgetedIconLookup::Uploaded(_)
        ));
        assert!(matches!(
            cache.get_or_insert_budgeted(&icon1),
            BudgetedIconLookup::Hit(_)
        ));
        assert!(matches!(
            cache.get_or_insert_budgeted(&icon2),
            BudgetedIconLookup::Uploaded(_)
        ));
        assert!(!cache.has_deferred_uploads());
        assert!(matches!(
            cache.get_or_insert_budgeted(&icon3),
            BudgetedIconLookup::Deferred
        ));
        assert!(cache.has_deferred_uploads());
        assert_eq!(cache.len(), 2);

        // Frame 2: budget resets, icon1/icon2 hit cache, icon3 uploads cleanly with no defer.
        cache.begin_frame();
        assert!(!cache.has_deferred_uploads());
        assert!(matches!(
            cache.get_or_insert_budgeted(&icon1),
            BudgetedIconLookup::Hit(_)
        ));
        assert!(matches!(
            cache.get_or_insert_budgeted(&icon2),
            BudgetedIconLookup::Hit(_)
        ));
        assert!(matches!(
            cache.get_or_insert_budgeted(&icon3),
            BudgetedIconLookup::Uploaded(_)
        ));
        assert!(!cache.has_deferred_uploads());
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn list_telemetry_registry_records_counters_and_formats_privacy_safe_line() {
        let reg = ListTelemetryRegistry::new();
        assert!(reg.format_diagnostic_line().is_none());

        reg.record_commit(ListViewTelemetryKind::Processes, 180, true);
        reg.record_commit(ListViewTelemetryKind::Processes, 95, false);
        reg.record_scroll_defer(ListViewTelemetryKind::Processes);
        reg.record_icon_upload(ListViewTelemetryKind::Processes);
        reg.record_icon_defer(ListViewTelemetryKind::Processes);
        reg.record_render_td(ListViewTelemetryKind::Processes);

        let snap = reg.snapshot(ListViewTelemetryKind::Processes);
        assert_eq!(snap.commits, 2);
        assert_eq!(snap.viewport_fp_skips, 1);
        assert_eq!(snap.scroll_defers, 1);
        assert_eq!(snap.icon_uploads, 1);
        assert_eq!(snap.icon_defers, 1);
        assert_eq!(snap.last_commit_us, 95);
        assert_eq!(snap.max_commit_us, 180);
        assert_eq!(snap.render_td_calls, 1);

        let line = reg
            .format_diagnostic_line()
            .expect("non-zero commits produce telemetry line");
        assert!(line.contains("Processes(commits=2, fp_skips=1, scroll_defers=1, icon_uploads=1, icon_defers=1, max_commit=180us)"));
        assert!(!line.contains("Details("));
    }
}
