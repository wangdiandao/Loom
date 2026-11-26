//! Reusable interactive table column layout (visibility, header context menu,
//! "Select columns" dialog, drag-to-resize, and drag-to-reorder) shared by the
//! Processes and Details feature views.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    ActiveTheme as _, ElementExt as _, Icon, IconName, Sizable as _, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Bounds, Context, DragMoveEvent, Empty, EntityId, InteractiveElement as _,
    IntoElement, MouseButton, MouseDownEvent, ParentElement as _, Pixels, Render, Role,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use loom_config::TableColumnSetting;
use loom_types::SortDirection;

use crate::i18n;

/// Width in logical pixels on each side of a column divider that acts as a resize grab zone.
const RESIZE_HANDLE_HALF_WIDTH: f32 = 4.0;

/// Extra vertical tolerance (px) above/below the header bar when dragging a column left/right
/// so slight pointer drift does not drop the insertion indicator unexpectedly.
const HEADER_DRAG_VERTICAL_SLACK: f32 = 36.0;

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
    /// Whether this column is pinned and cannot be hidden (e.g., primary Name column).
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

    /// Creates a visible, locked [`ColumnSpec`] that cannot be hidden.
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

#[derive(Debug, Clone, Copy)]
struct ResizeDragOrigin {
    col_ix: usize,
    origin_x: Pixels,
    origin_width: f32,
}

/// Pure column order, visibility, width, bounds, resize, and drag-reorder state for a virtualized table.
///
/// All `col_ix` indices passed to resize, bounds, and drag-reorder methods operate in
/// **visible-column** coordinates (`0..visible_count()`), while hidden columns retain their
/// relative order and last saved widths inside `columns`.
#[derive(Debug, Clone)]
pub(crate) struct TableColumnLayout<C: ColumnKey> {
    columns: Vec<ColumnSpec<C>>,
    bounds: Vec<Bounds<Pixels>>,
    resize_origin: Option<ResizeDragOrigin>,
    resizing_col: Option<usize>,
    drag_gap: Option<usize>,
}

impl<C: ColumnKey> TableColumnLayout<C> {
    /// Constructs a layout initialized from default ordered [`ColumnSpec`] definitions.
    pub(crate) fn new(mut columns: Vec<ColumnSpec<C>>) -> Self {
        ensure_at_least_one_visible(&mut columns);
        let vis_len = columns.iter().filter(|c| c.visible).count();
        Self {
            columns,
            bounds: vec![Bounds::default(); vis_len],
            resize_origin: None,
            resizing_col: None,
            drag_gap: None,
        }
    }

    /// Applies previously persisted [`TableColumnSetting`] entries, preserving order, clamped
    /// widths, and visibility while appending any newly added columns at their default positions.
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
        ensure_at_least_one_visible(&mut reordered);

        self.columns = reordered;
        self.reset_interaction_state();
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
        ensure_at_least_one_visible(&mut defaults);
        self.columns = defaults;
        self.reset_interaction_state();
    }

    /// Returns all columns (both visible and hidden) in their stored order.
    #[cfg(test)]
    #[inline]
    pub(crate) fn all_columns(&self) -> &[ColumnSpec<C>] {
        &self.columns
    }

    /// Returns the ordered list of currently visible columns.
    pub(crate) fn visible_columns(&self) -> Vec<ColumnSpec<C>> {
        self.columns
            .iter()
            .copied()
            .filter(|spec| spec.visible)
            .collect()
    }

    /// Returns the number of currently visible columns.
    #[inline]
    pub(crate) fn visible_count(&self) -> usize {
        self.columns.iter().filter(|spec| spec.visible).count()
    }

    /// Returns `true` if column `id` is currently visible.
    #[inline]
    pub(crate) fn is_visible(&self, id: C) -> bool {
        self.columns
            .iter()
            .find(|spec| spec.id == id)
            .is_some_and(|spec| spec.visible)
    }

    /// Hides column `id` if it is visible, unlocked, and not the last visible column.
    ///
    /// Returns `true` if visibility changed.
    pub(crate) fn hide_column(&mut self, id: C) -> bool {
        let visible_total = self.visible_count();
        if visible_total <= 1 {
            return false;
        }
        let Some(spec) = self.columns.iter_mut().find(|spec| spec.id == id) else {
            return false;
        };
        if spec.locked || !spec.visible {
            return false;
        }
        spec.visible = false;
        self.reset_interaction_state();
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
            self.reset_interaction_state();
        }
        changed
    }

    /// Returns total table width across all visible columns plus `horizontal_padding` (left + right).
    #[inline]
    pub(crate) fn total_width(&self, horizontal_padding: f32) -> f32 {
        let cols_sum: f32 = self
            .columns
            .iter()
            .filter(|c| c.visible)
            .map(|c| c.width)
            .sum();
        cols_sum + horizontal_padding
    }

    /// Records the rendered window-space bounds of visible column header `vis_ix`.
    #[inline]
    pub(crate) fn set_bounds(&mut self, vis_ix: usize, bounds: Bounds<Pixels>) {
        if let Some(slot) = self.bounds.get_mut(vis_ix) {
            *slot = bounds;
        }
    }

    /// Starts a column-resize interaction for visible column `vis_ix` at pointer `pointer_x`.
    pub(crate) fn begin_resize(&mut self, vis_ix: usize, pointer_x: Pixels) {
        if let Some(all_ix) = self.visible_to_all_index(vis_ix) {
            let width = self.columns[all_ix].width;
            self.resize_origin = Some(ResizeDragOrigin {
                col_ix: vis_ix,
                origin_x: pointer_x,
                origin_width: width,
            });
            self.resizing_col = Some(vis_ix);
        }
    }

    /// Updates visible column `vis_ix` width from a drag-move at `pointer_x`. Returns `true` if changed.
    pub(crate) fn update_resize(&mut self, vis_ix: usize, pointer_x: Pixels) -> bool {
        let origin = match self.resize_origin {
            Some(origin) if origin.col_ix == vis_ix => origin,
            _ => {
                self.begin_resize(vis_ix, pointer_x);
                return false;
            }
        };
        self.resizing_col = Some(vis_ix);
        let delta = f32::from(pointer_x - origin.origin_x);
        self.resize_column(vis_ix, origin.origin_width + delta)
    }

    /// Sets visible column `vis_ix` width clamped to `[min_width, max_width]`. Returns `true` if changed.
    pub(crate) fn resize_column(&mut self, vis_ix: usize, new_width: f32) -> bool {
        let Some(all_ix) = self.visible_to_all_index(vis_ix) else {
            return false;
        };
        let spec = &mut self.columns[all_ix];
        let clamped = new_width.clamp(spec.min_width, spec.max_width).round();
        if (spec.width - clamped).abs() < 0.5 {
            return false;
        }
        spec.width = clamped;
        true
    }

    /// Ends any active column resize interaction. Returns `true` if a resize was active.
    pub(crate) fn end_resize(&mut self) -> bool {
        let was_active = self.resize_origin.is_some() || self.resizing_col.is_some();
        self.resize_origin = None;
        self.resizing_col = None;
        was_active
    }

    /// Returns `true` while visible column `vis_ix` is actively being resized.
    #[inline]
    pub(crate) fn is_resizing(&self, vis_ix: usize) -> bool {
        self.resizing_col == Some(vis_ix)
    }

    /// Returns the currently highlighted drop insertion gap (`0..=visible_count()`).
    #[inline]
    pub(crate) fn drag_gap(&self) -> Option<usize> {
        self.drag_gap
    }

    /// Updates the highlighted drop gap. Returns `true` if it changed.
    #[inline]
    pub(crate) fn set_drag_gap(&mut self, gap: Option<usize>) -> bool {
        if self.drag_gap != gap {
            self.drag_gap = gap;
            true
        } else {
            false
        }
    }

    /// Computes the drop insertion gap (`0..=visible_count()`) for a visible column header dragged to `x`.
    ///
    /// Returns `None` when dropping at `x` would leave `drag_vis_ix` in its current position.
    pub(crate) fn drag_gap_at(&self, x: Pixels, drag_vis_ix: usize) -> Option<usize> {
        let vis_len = self.visible_count();
        if drag_vis_ix >= vis_len || self.bounds.len() != vis_len {
            return None;
        }

        let mut gap = 0usize;
        for (ix, col_bounds) in self.bounds.iter().enumerate() {
            if x < col_bounds.center().x {
                break;
            }
            gap = ix + 1;
        }

        if gap == drag_vis_ix || gap == drag_vis_ix + 1 {
            None
        } else {
            Some(gap)
        }
    }

    /// Commits a column reorder by moving visible column `from_vis_ix` into the active `drag_gap`.
    ///
    /// Returns `true` if the column order changed.
    pub(crate) fn commit_drag_drop(&mut self, from_vis_ix: usize) -> bool {
        let Some(gap) = self.drag_gap.take() else {
            return false;
        };
        let to_vis_ix = if from_vis_ix < gap { gap - 1 } else { gap };
        self.move_visible_column(from_vis_ix, to_vis_ix)
    }

    /// Moves the visible column at `from_vis_ix` to visible slot `to_vis_ix`, keeping bounds synchronized.
    pub(crate) fn move_visible_column(&mut self, from_vis_ix: usize, to_vis_ix: usize) -> bool {
        let vis_len = self.visible_count();
        if from_vis_ix >= vis_len || to_vis_ix >= vis_len || from_vis_ix == to_vis_ix {
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
        if self.bounds.len() == vis_len {
            let b = self.bounds.remove(from_vis_ix);
            self.bounds.insert(to_vis_ix, b);
        }
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

    fn reset_interaction_state(&mut self) {
        let vis_len = self.visible_count();
        self.bounds = vec![Bounds::default(); vis_len];
        self.resize_origin = None;
        self.resizing_col = None;
        self.drag_gap = None;
    }
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

/// Floating ghost preview payload used while dragging a column header to reorder it.
#[derive(Clone)]
pub(crate) struct DragColumnHeader {
    pub(crate) table_id: EntityId,
    pub(crate) col_ix: usize,
    pub(crate) label: SharedString,
    pub(crate) width: Pixels,
}

impl Render for DragColumnHeader {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .px_2p5()
            .py_1()
            .bg(theme.secondary)
            .text_xs()
            .font_medium()
            .text_color(theme.foreground)
            .opacity(0.92)
            .border_1()
            .border_color(theme.primary.opacity(0.6))
            .rounded(theme.radius)
            .shadow_md()
            .w(self.width.clamp(px(80.), px(240.)))
            .truncate()
            .child(self.label.clone())
    }
}

/// Invisible drag payload used while dragging a column border handle to resize it.
#[derive(Clone)]
pub(crate) struct ResizeColumnDrag {
    pub(crate) table_id: EntityId,
    pub(crate) col_ix: usize,
}

impl Render for ResizeColumnDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// Visual & sorting descriptor for a single interactive column header cell.
pub(crate) struct HeaderColumnDescriptor {
    pub(crate) id: SharedString,
    pub(crate) label: SharedString,
    pub(crate) width: f32,
    pub(crate) right_aligned: bool,
    pub(crate) sortable: bool,
    pub(crate) is_active_sort: bool,
    pub(crate) sort_direction: SortDirection,
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

/// Renders one interactive column header cell with:
/// - Click-to-sort (when `desc.sortable`)
/// - Right-click header context menu (`on_context_menu`)
/// - Drag-to-reorder (`DragColumnHeader`) + vertical drop insertion bar
/// - Symmetric leading & trailing `cursor_col_resize` handles (`ResizeColumnDrag`)
/// - Prepaint bounds capture for accurate live drop-gap hit testing
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_interactive_header_cell<V, C, FLayout, FSort, FChanged, FMenu>(
    col_ix: usize,
    total_cols: usize,
    desc: HeaderColumnDescriptor,
    layout: &TableColumnLayout<C>,
    cx: &mut Context<V>,
    layout_mut: FLayout,
    on_sort: FSort,
    on_layout_committed: FChanged,
    on_context_menu: FMenu,
) -> impl IntoElement
where
    V: 'static,
    C: ColumnKey,
    FLayout: Fn(&mut V) -> &mut TableColumnLayout<C> + Copy + 'static,
    FSort: Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    FChanged: Fn(&mut V, &mut Context<V>) + Copy + 'static,
    FMenu: Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
{
    let table_id = cx.entity_id();
    let theme = cx.theme();
    let fg = theme.foreground;
    let muted_fg = theme.muted_foreground;
    let border = theme.border;
    let primary = theme.primary;
    let hover_bg = theme.accent.opacity(0.4);

    let width_px = px(desc.width);
    let wrapper_id: SharedString = format!("{}-cell", desc.id).into();
    let label_for_drag = desc.label.clone();
    let label_for_a11y = desc.label.clone();
    let is_resizing = layout.is_resizing(col_ix);
    let drag_gap = layout.drag_gap();
    let show_left_drop = cx.has_active_drag() && drag_gap == Some(col_ix);
    let show_right_drop =
        cx.has_active_drag() && col_ix + 1 == total_cols && drag_gap == Some(total_cols);

    let chevron = match desc.sort_direction {
        SortDirection::Ascending => IconName::ChevronUp,
        SortDirection::Descending => IconName::ChevronDown,
    };

    let cell_body = h_flex()
        .id(desc.id)
        .role(Role::ColumnHeader)
        .aria_label(label_for_a11y)
        .flex_1()
        .min_w_0()
        .h_full()
        .items_center()
        .gap_1()
        .px_2()
        .when(desc.right_aligned, |this| this.justify_end())
        .text_color(if desc.is_active_sort { fg } else { muted_fg })
        .when(desc.is_active_sort, |this| this.font_semibold())
        .when(desc.sortable, |this| {
            this.cursor_pointer()
                .hover(move |s| s.bg(hover_bg).text_color(fg))
                .on_click(cx.listener(move |view, _, window, cx| {
                    on_sort(view, window, cx);
                }))
        })
        .when(!desc.sortable, |this| {
            this.cursor_grab().hover(move |s| s.bg(hover_bg))
        })
        .on_drag(
            DragColumnHeader {
                table_id,
                col_ix,
                label: label_for_drag,
                width: width_px,
            },
            |drag, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| drag.clone())
            },
        )
        .child(div().min_w_0().truncate().child(desc.label))
        .when(desc.is_active_sort, |this| {
            this.child(Icon::new(chevron).xsmall().text_color(primary))
        });

    let right_group: SharedString = format!("col-resize-{}-{}", table_id.as_u64(), col_ix).into();
    let trailing_handle = make_resize_handle(
        ("col-resize-right", col_ix),
        col_ix,
        table_id,
        cx,
        layout_mut,
        on_layout_committed,
    )
    .group(right_group.clone())
    .w(px(RESIZE_HANDLE_HALF_WIDTH))
    .ml(px(-RESIZE_HANDLE_HALF_WIDTH))
    .h_full()
    .flex_shrink_0()
    .justify_end()
    .items_center()
    .child(
        div()
            .w(px(1.))
            .h(px(16.))
            .bg(if is_resizing {
                primary
            } else {
                border.opacity(0.65)
            })
            .group_hover(&right_group, move |s| s.h_full().bg(primary)),
    );

    let leading_handle = col_ix.checked_sub(1).map(|prev_ix| {
        make_resize_handle(
            ("col-resize-left", col_ix),
            prev_ix,
            table_id,
            cx,
            layout_mut,
            on_layout_committed,
        )
        .absolute()
        .top_0()
        .left_0()
        .w(px(RESIZE_HANDLE_HALF_WIDTH))
        .h_full()
    });

    h_flex()
        .id(wrapper_id)
        .relative()
        .w(width_px)
        .h_full()
        .flex_shrink_0()
        .child(cell_body)
        .child(trailing_handle)
        .when_some(leading_handle, |this, handle| this.child(handle))
        .when(show_left_drop, |this| {
            this.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(px(2.))
                    .bg(primary),
            )
        })
        .when(show_right_drop, |this| {
            this.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right_0()
                    .w(px(2.))
                    .bg(primary),
            )
        })
        .on_prepaint({
            let view = cx.entity();
            move |bounds, _, cx| {
                view.update(cx, |v, _| {
                    layout_mut(v).set_bounds(col_ix, bounds);
                });
            }
        })
        .context_menu(move |menu, window, cx| on_context_menu(menu, window, cx))
}

/// Renders an invisible flex-1 trailing spacer inside the column header row so right-clicking
/// blank header space to the right of the last visible column still opens the header menu.
pub(crate) fn render_header_trailing_spacer<FMenu>(
    id: &'static str,
    on_context_menu: FMenu,
) -> impl IntoElement
where
    FMenu: Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
{
    div()
        .id(id)
        .flex_1()
        .min_w_0()
        .h_full()
        .context_menu(move |menu, window, cx| on_context_menu(menu, window, cx))
}

fn make_resize_handle<V, C, FLayout, FChanged>(
    id: (&'static str, usize),
    target_col_ix: usize,
    table_id: EntityId,
    cx: &mut Context<V>,
    layout_mut: FLayout,
    on_layout_committed: FChanged,
) -> Stateful<gpui_kit::Div>
where
    V: 'static,
    C: ColumnKey,
    FLayout: Fn(&mut V) -> &mut TableColumnLayout<C> + Copy + 'static,
    FChanged: Fn(&mut V, &mut Context<V>) + Copy + 'static,
{
    h_flex()
        .id(id)
        .occlude()
        .cursor_col_resize()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |view, event: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                layout_mut(view).begin_resize(target_col_ix, event.position.x);
                cx.notify();
            }),
        )
        .on_drag(
            ResizeColumnDrag {
                table_id,
                col_ix: target_col_ix,
            },
            |drag, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| drag.clone())
            },
        )
        .on_mouse_up_out(
            MouseButton::Left,
            cx.listener(move |view, _, _, cx| {
                if layout_mut(view).end_resize() {
                    on_layout_committed(view, cx);
                    cx.notify();
                }
            }),
        )
}

/// Attaches view-wide resize drag-move/drop and column-reorder drop handlers so fast mouse
/// motion or slight vertical drift outside the 32px header row still resizes/drops smoothly.
pub(crate) fn attach_table_column_drag_handlers<V, C, FLayout, FChanged>(
    root: Stateful<gpui_kit::Div>,
    cx: &mut Context<V>,
    layout_mut: FLayout,
    on_layout_committed: FChanged,
) -> Stateful<gpui_kit::Div>
where
    V: 'static,
    C: ColumnKey,
    FLayout: Fn(&mut V) -> &mut TableColumnLayout<C> + Copy + 'static,
    FChanged: Fn(&mut V, &mut Context<V>) + Copy + 'static,
{
    let table_id = cx.entity_id();
    root.on_drag_move(cx.listener(
        move |view, event: &DragMoveEvent<ResizeColumnDrag>, _, cx| {
            let drag = event.drag(cx);
            if drag.table_id != table_id {
                return;
            }
            if layout_mut(view).update_resize(drag.col_ix, event.event.position.x) {
                cx.notify();
            }
        },
    ))
    .on_drop(cx.listener(move |view, drag: &ResizeColumnDrag, _, cx| {
        if drag.table_id != table_id {
            return;
        }
        if layout_mut(view).end_resize() {
            on_layout_committed(view, cx);
            cx.notify();
        }
    }))
    .on_drop(cx.listener(move |view, drag: &DragColumnHeader, _, cx| {
        if drag.table_id != table_id {
            return;
        }
        let layout = layout_mut(view);
        let moved = layout.commit_drag_drop(drag.col_ix);
        if moved {
            on_layout_committed(view, cx);
        }
        cx.notify();
    }))
    .on_mouse_up(
        MouseButton::Left,
        cx.listener(move |view, _, _, cx| {
            let layout = layout_mut(view);
            let ended_resize = layout.end_resize();
            let cleared_gap = layout.set_drag_gap(None);
            if ended_resize {
                on_layout_committed(view, cx);
            }
            if ended_resize || cleared_gap {
                cx.notify();
            }
        }),
    )
}

/// Attaches header-bar `DragColumnHeader` drag-move and drop handlers that compute the live
/// insertion gap from the pointer position.
pub(crate) fn attach_header_reorder_handlers<V, C, FLayout, FChanged>(
    header: Stateful<gpui_kit::Div>,
    cx: &mut Context<V>,
    layout_mut: FLayout,
    on_layout_committed: FChanged,
) -> Stateful<gpui_kit::Div>
where
    V: 'static,
    C: ColumnKey,
    FLayout: Fn(&mut V) -> &mut TableColumnLayout<C> + Copy + 'static,
    FChanged: Fn(&mut V, &mut Context<V>) + Copy + 'static,
{
    let table_id = cx.entity_id();
    header
        .on_drag_move(cx.listener(
            move |view, event: &DragMoveEvent<DragColumnHeader>, _, cx| {
                let drag = event.drag(cx);
                if drag.table_id != table_id {
                    return;
                }
                let pos = event.event.position;
                let within_band = pos.y >= event.bounds.top() - px(HEADER_DRAG_VERTICAL_SLACK)
                    && pos.y <= event.bounds.bottom() + px(HEADER_DRAG_VERTICAL_SLACK)
                    && pos.x >= event.bounds.left() - px(16.)
                    && pos.x <= event.bounds.right() + px(16.);
                let layout = layout_mut(view);
                let next_gap = if within_band {
                    layout.drag_gap_at(pos.x, drag.col_ix)
                } else {
                    None
                };
                if layout.set_drag_gap(next_gap) {
                    cx.notify();
                }
            },
        ))
        .on_drop(cx.listener(move |view, drag: &DragColumnHeader, _, cx| {
            if drag.table_id != table_id {
                return;
            }
            cx.stop_propagation();
            let moved = layout_mut(view).commit_drag_drop(drag.col_ix);
            if moved {
                on_layout_committed(view, cx);
            }
            cx.notify();
        }))
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
    use gpui_kit::{point, size};

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

    fn assign_contiguous_bounds(layout: &mut TableColumnLayout<TestCol>) {
        let mut x = 0.0;
        for (ix, spec) in layout.visible_columns().into_iter().enumerate() {
            layout.set_bounds(
                ix,
                Bounds::new(point(px(x), px(0.)), size(px(spec.width), px(32.))),
            );
            x += spec.width;
        }
    }

    fn sample_layout() -> TableColumnLayout<TestCol> {
        let mut layout = TableColumnLayout::new(vec![
            ColumnSpec::pinned(TestCol::Name, 200.0, 100.0, 400.0),
            ColumnSpec::hidden(TestCol::Pid, 76.0, 56.0, 160.0),
            ColumnSpec::new(TestCol::Cpu, 80.0, 50.0, 200.0),
            ColumnSpec::new(TestCol::Memory, 100.0, 60.0, 240.0),
            ColumnSpec::new(TestCol::Publisher, 140.0, 80.0, 320.0),
        ]);
        // Visible columns: Name[0..200], Cpu[200..280], Memory[280..380], Publisher[380..520].
        assign_contiguous_bounds(&mut layout);
        layout
    }

    #[test]
    fn resize_clamps_to_min_and_max_bounds_on_visible_columns() {
        let mut layout = sample_layout();

        layout.begin_resize(1, px(280.));
        assert!(layout.is_resizing(1));
        assert!(layout.update_resize(1, px(320.)));
        assert_eq!(layout.visible_columns()[1].id, TestCol::Cpu);
        assert_eq!(layout.visible_columns()[1].width, 120.0);

        // Shrink below min_width (50.0) clamps to 50.0.
        assert!(layout.update_resize(1, px(100.)));
        assert_eq!(layout.visible_columns()[1].width, 50.0);

        // Expand past max_width (200.0) clamps to 200.0.
        assert!(layout.update_resize(1, px(900.)));
        assert_eq!(layout.visible_columns()[1].width, 200.0);

        assert!(layout.end_resize());
        assert!(!layout.is_resizing(1));
        assert!(!layout.end_resize());
    }

    #[test]
    fn drag_gap_and_commit_reorder_preserves_hidden_columns() {
        let mut layout = sample_layout();

        // Dropping Cpu (vis_ix=1) near its own edges is a no-op.
        assert_eq!(layout.drag_gap_at(px(210.), 1), None);
        assert_eq!(layout.drag_gap_at(px(270.), 1), None);

        // Dragging Cpu (vis_ix=1) to the left of Name's center (100) targets gap 0.
        assert_eq!(layout.drag_gap_at(px(40.), 1), Some(0));
        assert!(layout.set_drag_gap(Some(0)));
        assert!(layout.commit_drag_drop(1));
        assert_eq!(
            layout
                .visible_columns()
                .iter()
                .map(|c| c.id)
                .collect::<Vec<_>>(),
            vec![
                TestCol::Cpu,
                TestCol::Name,
                TestCol::Memory,
                TestCol::Publisher
            ]
        );
        // Hidden Pid column remains intact in all_columns().
        assert!(!layout.is_visible(TestCol::Pid));
        assert_eq!(layout.all_columns().len(), 5);

        // Rebuild bounds for the new order and drag Cpu (now vis_ix=0) past Publisher's center to gap 4.
        assign_contiguous_bounds(&mut layout);
        assert_eq!(layout.drag_gap_at(px(500.), 0), Some(4));
        assert!(layout.set_drag_gap(Some(4)));
        assert!(layout.commit_drag_drop(0));
        assert_eq!(
            layout
                .visible_columns()
                .iter()
                .map(|c| c.id)
                .collect::<Vec<_>>(),
            vec![
                TestCol::Name,
                TestCol::Memory,
                TestCol::Publisher,
                TestCol::Cpu
            ]
        );
    }

    #[test]
    fn visibility_hiding_locked_guard_and_select_columns_apply() {
        let mut layout = sample_layout();
        assert_eq!(
            layout.total_width(16.0),
            200.0 + 80.0 + 100.0 + 140.0 + 16.0
        );

        // Pinned Name column cannot be hidden.
        assert!(!layout.hide_column(TestCol::Name));
        assert!(layout.is_visible(TestCol::Name));

        // Hide Publisher; width drops by 140px.
        assert!(layout.hide_column(TestCol::Publisher));
        assert!(!layout.is_visible(TestCol::Publisher));
        assert_eq!(layout.total_width(16.0), 200.0 + 80.0 + 100.0 + 16.0);

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
    fn saved_snapshot_roundtrip_preserves_visibility_widths_and_guards_corrupt_data() {
        let mut layout = sample_layout();
        layout.resize_column(0, 260.0);
        assert!(layout.apply_visible_slugs(&["pid", "cpu", "publisher"]));
        assert!(layout.move_visible_column(3, 1)); // move Publisher after Name
        let mut saved = layout.snapshot();

        // Add an unknown legacy slug, corrupt Name visibility to false, and out-of-range width.
        saved.push(TableColumnSetting {
            id: "legacy_removed_col".to_owned(),
            width: 999,
            visible: true,
        });
        saved[0].width = 9999; // Name clamps to max_width (400.0)
        saved[0].visible = false; // Locked Name stays visible

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
        assert_eq!(restored.visible_columns()[0].width, 400.0);
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

    struct InteractiveHeaderHarness {
        layout: TableColumnLayout<TestCol>,
        sorts: Vec<TestCol>,
        context_menu_opens: Vec<TestCol>,
        commits: usize,
    }

    impl Render for InteractiveHeaderHarness {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let cols = self.layout.visible_columns();
            let total = cols.len();
            let snap = self.layout.clone();

            let header_cells = cols.iter().enumerate().map(|(ix, spec)| {
                let col = spec.id;
                let view_for_menu = cx.entity().downgrade();
                render_interactive_header_cell(
                    ix,
                    total,
                    HeaderColumnDescriptor {
                        id: format!("harness-col-{}", col.slug()).into(),
                        label: col.slug().into(),
                        width: spec.width,
                        right_aligned: false,
                        sortable: col != TestCol::Publisher,
                        is_active_sort: false,
                        sort_direction: SortDirection::Descending,
                    },
                    &snap,
                    cx,
                    |view: &mut Self| &mut view.layout,
                    move |view, _, _| view.sorts.push(col),
                    |view, _| view.commits += 1,
                    move |menu, _window, cx| {
                        let _ = view_for_menu.update(cx, |v, _| v.context_menu_opens.push(col));
                        menu.item(PopupMenuItem::new("Hide column"))
                    },
                )
                .into_any_element()
            });

            let header = attach_header_reorder_handlers(
                h_flex()
                    .id("harness-header")
                    .w_full()
                    .h(px(32.))
                    .items_center()
                    .children(header_cells),
                cx,
                |view: &mut Self| &mut view.layout,
                |view, _| view.commits += 1,
            );

            attach_table_column_drag_handlers(
                div().id("harness-root").size_full().child(header),
                cx,
                |view: &mut Self| &mut view.layout,
                |view, _| view.commits += 1,
            )
        }
    }

    #[gpui_kit::test]
    fn interactive_header_supports_sort_click_resize_reorder_and_right_click_menu(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::{Modifiers, VisualTestContext};

        cx.update(gpui_kit::init);
        let view = cx.new(|_| InteractiveHeaderHarness {
            layout: TableColumnLayout::new(vec![
                ColumnSpec::pinned(TestCol::Name, 200.0, 100.0, 400.0),
                ColumnSpec::hidden(TestCol::Pid, 76.0, 56.0, 160.0),
                ColumnSpec::new(TestCol::Cpu, 80.0, 50.0, 200.0),
                ColumnSpec::new(TestCol::Memory, 100.0, 60.0, 240.0),
                ColumnSpec::new(TestCol::Publisher, 140.0, 80.0, 320.0),
            ]),
            sorts: Vec::new(),
            context_menu_opens: Vec::new(),
            commits: 0,
        });

        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                cx.new(|cx| gpui_kit::base::Root::new(view.clone(), window, cx))
            })
            .unwrap()
        });
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.update(|window, cx| window.draw(cx).clear(cx));

        // 1. Plain click on Cpu column (x = 200..280, center 240) triggers sort without reordering.
        let cpu_center = point(px(240.), px(16.));
        vcx.simulate_mouse_down(cpu_center, MouseButton::Left, Modifiers::default());
        vcx.simulate_mouse_up(cpu_center, MouseButton::Left, Modifiers::default());
        vcx.update(|window, cx| window.draw(cx).clear(cx));

        view.read_with(&vcx, |h, _| {
            assert_eq!(h.sorts, vec![TestCol::Cpu]);
            assert_eq!(
                h.layout
                    .visible_columns()
                    .iter()
                    .map(|c| c.id)
                    .collect::<Vec<_>>(),
                vec![
                    TestCol::Name,
                    TestCol::Cpu,
                    TestCol::Memory,
                    TestCol::Publisher
                ]
            );
        });

        // 2. Right-click on Cpu header opens the column header context menu for Cpu.
        vcx.simulate_mouse_down(cpu_center, MouseButton::Right, Modifiers::default());
        vcx.simulate_mouse_up(cpu_center, MouseButton::Right, Modifiers::default());
        vcx.update(|window, cx| window.draw(cx).clear(cx));

        view.read_with(&vcx, |h, _| {
            assert_eq!(
                h.context_menu_opens,
                vec![TestCol::Cpu],
                "right-clicking Cpu header must open the header context menu for Cpu"
            );
        });

        // Dismiss the open context menu before continuing drag tests.
        vcx.simulate_keystrokes("escape");
        vcx.update(|window, cx| window.draw(cx).clear(cx));

        // 3. Drag the resize divider at Name's right border (x = 199, y = 16) +50px to x = 249.
        let divider_start = point(px(199.), px(16.));
        let divider_mid = point(px(225.), px(16.));
        let divider_end = point(px(249.), px(16.));
        vcx.simulate_mouse_down(divider_start, MouseButton::Left, Modifiers::default());
        vcx.simulate_mouse_move(divider_mid, Some(MouseButton::Left), Modifiers::default());
        vcx.simulate_mouse_move(divider_end, Some(MouseButton::Left), Modifiers::default());
        vcx.simulate_mouse_up(divider_end, MouseButton::Left, Modifiers::default());
        vcx.update(|window, cx| window.draw(cx).clear(cx));

        view.read_with(&vcx, |h, _| {
            assert_eq!(
                h.layout.visible_columns()[0].width,
                250.0,
                "dragging Name divider +50px must resize Name from 200px to 250px"
            );
            assert!(h.commits >= 1, "resize completion must commit layout");
        });

        // 4. Drag Cpu header (now at x = 250..330, center 290) past Memory (330..430) to x = 420.
        let drag_start = point(px(290.), px(16.));
        let drag_step = point(px(360.), px(16.));
        let drag_drop = point(px(420.), px(16.));
        vcx.simulate_mouse_down(drag_start, MouseButton::Left, Modifiers::default());
        vcx.simulate_mouse_move(drag_step, Some(MouseButton::Left), Modifiers::default());
        vcx.simulate_mouse_move(drag_drop, Some(MouseButton::Left), Modifiers::default());
        vcx.simulate_mouse_up(drag_drop, MouseButton::Left, Modifiers::default());
        vcx.update(|window, cx| window.draw(cx).clear(cx));

        view.read_with(&vcx, |h, _| {
            assert_eq!(
                h.layout
                    .visible_columns()
                    .iter()
                    .map(|c| c.id)
                    .collect::<Vec<_>>(),
                vec![
                    TestCol::Name,
                    TestCol::Memory,
                    TestCol::Cpu,
                    TestCol::Publisher
                ],
                "dragging Cpu past Memory's midpoint must move Cpu after Memory"
            );
        });
    }
}
