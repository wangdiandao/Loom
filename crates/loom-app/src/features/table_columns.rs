//! Reusable column catalog, visibility/order/width persistence, header context menu,
//! and **Select columns** dialog shared by the Processes and Details `DataTable` slices.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Weak};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::table::{DataTable, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, Size, WindowExt as _, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Render, RenderImage, SharedString, Styled as _, Window, div, px,
};
use loom_config::TableColumnSetting;
use loom_types::ExeIconRgba;
use loom_ui_kit::rgba_icon_to_render_image;

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
    #[cfg(test)]
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
}
