//! Reusable interactive table column layout (drag-to-resize and drag-to-reorder)
//! shared by the Processes and Details feature views.

use gpui_kit::component::{
    ActiveTheme as _, ElementExt as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
};
use gpui_kit::{
    AppContext as _, Bounds, Context, DragMoveEvent, Empty, EntityId, InteractiveElement as _,
    IntoElement, MouseButton, MouseDownEvent, ParentElement as _, Pixels, Render, Role,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use loom_types::SortDirection;

/// Width in logical pixels on each side of a column divider that acts as a resize grab zone.
const RESIZE_HANDLE_HALF_WIDTH: f32 = 4.0;

/// Extra vertical tolerance (px) above/below the header bar when dragging a column left/right
/// so slight pointer drift does not drop the insertion indicator unexpectedly.
const HEADER_DRAG_VERTICAL_SLACK: f32 = 36.0;

/// Identifier trait for columns managed by [`TableColumnLayout`].
pub(crate) trait ColumnKey: Copy + Eq + 'static {
    /// Stable lowercase key used to persist session column order and widths in `UiStore`.
    fn slug(self) -> &'static str;
}

/// Static + runtime geometry specification for a single table column.
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
}

impl<C: ColumnKey> ColumnSpec<C> {
    /// Creates a new [`ColumnSpec`] with the given initial width and `[min_width, max_width]` bounds.
    #[inline]
    pub(crate) const fn new(id: C, width: f32, min_width: f32, max_width: f32) -> Self {
        Self {
            id,
            width,
            min_width,
            max_width,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ResizeDragOrigin {
    col_ix: usize,
    origin_x: Pixels,
    origin_width: f32,
}

/// Pure column order, width, bounds, resize, and drag-reorder state for a virtualized table.
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
    pub(crate) fn new(columns: Vec<ColumnSpec<C>>) -> Self {
        let len = columns.len();
        Self {
            columns,
            bounds: vec![Bounds::default(); len],
            resize_origin: None,
            resizing_col: None,
            drag_gap: None,
        }
    }

    /// Applies previously saved `(slug, width)` entries, preserving order and clamped widths
    /// while keeping any new columns appended at their default positions.
    pub(crate) fn apply_saved(&mut self, saved: &[(&'static str, f32)]) {
        if saved.is_empty() {
            return;
        }
        let mut remaining = self.columns.clone();
        let mut reordered = Vec::with_capacity(remaining.len());

        for &(slug, width) in saved {
            if let Some(pos) = remaining.iter().position(|spec| spec.id.slug() == slug) {
                let mut spec = remaining.remove(pos);
                spec.width = width.clamp(spec.min_width, spec.max_width);
                reordered.push(spec);
            }
        }
        reordered.extend(remaining);

        let len = reordered.len();
        self.columns = reordered;
        self.bounds = vec![Bounds::default(); len];
        self.resize_origin = None;
        self.resizing_col = None;
        self.drag_gap = None;
    }

    /// Exports the current column order and widths as `(slug, width)` pairs for session storage.
    pub(crate) fn snapshot(&self) -> Vec<(&'static str, f32)> {
        self.columns
            .iter()
            .map(|spec| (spec.id.slug(), spec.width))
            .collect()
    }

    /// Returns the ordered slice of active columns.
    #[inline]
    pub(crate) fn columns(&self) -> &[ColumnSpec<C>] {
        &self.columns
    }

    /// Returns total table width across all columns plus `horizontal_padding` (left + right).
    #[inline]
    pub(crate) fn total_width(&self, horizontal_padding: f32) -> f32 {
        let cols_sum: f32 = self.columns.iter().map(|c| c.width).sum();
        cols_sum + horizontal_padding
    }

    /// Records the rendered window-space bounds of column header `col_ix`.
    #[inline]
    pub(crate) fn set_bounds(&mut self, col_ix: usize, bounds: Bounds<Pixels>) {
        if let Some(slot) = self.bounds.get_mut(col_ix) {
            *slot = bounds;
        }
    }

    /// Starts a column-resize interaction at pointer x-coordinate `pointer_x`.
    pub(crate) fn begin_resize(&mut self, col_ix: usize, pointer_x: Pixels) {
        if let Some(spec) = self.columns.get(col_ix) {
            self.resize_origin = Some(ResizeDragOrigin {
                col_ix,
                origin_x: pointer_x,
                origin_width: spec.width,
            });
            self.resizing_col = Some(col_ix);
        }
    }

    /// Updates column `col_ix` width from a drag-move at `pointer_x`. Returns `true` if changed.
    pub(crate) fn update_resize(&mut self, col_ix: usize, pointer_x: Pixels) -> bool {
        let origin = match self.resize_origin {
            Some(origin) if origin.col_ix == col_ix => origin,
            _ => {
                self.begin_resize(col_ix, pointer_x);
                return false;
            }
        };
        self.resizing_col = Some(col_ix);
        let delta = f32::from(pointer_x - origin.origin_x);
        self.resize_column(col_ix, origin.origin_width + delta)
    }

    /// Sets column `col_ix` width clamped to `[min_width, max_width]`. Returns `true` if changed.
    pub(crate) fn resize_column(&mut self, col_ix: usize, new_width: f32) -> bool {
        let Some(spec) = self.columns.get_mut(col_ix) else {
            return false;
        };
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

    /// Returns `true` while column `col_ix` is actively being resized.
    #[inline]
    pub(crate) fn is_resizing(&self, col_ix: usize) -> bool {
        self.resizing_col == Some(col_ix)
    }

    /// Returns the currently highlighted drop insertion gap (`0..=columns.len()`).
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

    /// Computes the drop insertion gap (`0..=columns.len()`) for a column header dragged to `x`.
    ///
    /// Returns `None` when dropping at `x` would leave `drag_col_ix` in its current position.
    pub(crate) fn drag_gap_at(&self, x: Pixels, drag_col_ix: usize) -> Option<usize> {
        if drag_col_ix >= self.columns.len() || self.bounds.len() != self.columns.len() {
            return None;
        }

        let mut gap = 0usize;
        for (ix, col_bounds) in self.bounds.iter().enumerate() {
            if x < col_bounds.center().x {
                break;
            }
            gap = ix + 1;
        }

        if gap == drag_col_ix || gap == drag_col_ix + 1 {
            None
        } else {
            Some(gap)
        }
    }

    /// Commits a column reorder by moving `from_ix` into the active `drag_gap`.
    ///
    /// Returns `true` if the column order changed.
    pub(crate) fn commit_drag_drop(&mut self, from_ix: usize) -> bool {
        let Some(gap) = self.drag_gap.take() else {
            return false;
        };
        let to_ix = if from_ix < gap { gap - 1 } else { gap };
        self.move_column(from_ix, to_ix)
    }

    /// Moves the column at `from_ix` to `to_ix`, keeping bounds synchronized.
    pub(crate) fn move_column(&mut self, from_ix: usize, to_ix: usize) -> bool {
        let len = self.columns.len();
        if from_ix >= len || to_ix >= len || from_ix == to_ix {
            return false;
        }
        let col = self.columns.remove(from_ix);
        self.columns.insert(to_ix, col);
        if self.bounds.len() == len {
            let b = self.bounds.remove(from_ix);
            self.bounds.insert(to_ix, b);
        }
        true
    }
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

/// Renders one interactive column header cell with:
/// - Click-to-sort (when `desc.sortable`)
/// - Drag-to-reorder (`DragColumnHeader`) + vertical drop insertion bar
/// - Symmetric leading & trailing `cursor_col_resize` handles (`ResizeColumnDrag`)
/// - Prepaint bounds capture for accurate live drop-gap hit testing
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_interactive_header_cell<V, C, FLayout, FSort, FChanged>(
    col_ix: usize,
    total_cols: usize,
    desc: HeaderColumnDescriptor,
    layout: &TableColumnLayout<C>,
    cx: &mut Context<V>,
    layout_mut: FLayout,
    on_sort: FSort,
    on_layout_committed: FChanged,
) -> impl IntoElement
where
    V: 'static,
    C: ColumnKey,
    FLayout: Fn(&mut V) -> &mut TableColumnLayout<C> + Copy + 'static,
    FSort: Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    FChanged: Fn(&mut V, &mut Context<V>) + Copy + 'static,
{
    let table_id = cx.entity_id();
    let theme = cx.theme();
    let fg = theme.foreground;
    let muted_fg = theme.muted_foreground;
    let border = theme.border;
    let primary = theme.primary;
    let hover_bg = theme.accent.opacity(0.4);

    let width_px = px(desc.width);
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

#[cfg(test)]
mod tests {
    use gpui_kit::{point, size};

    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TestCol {
        Name,
        Cpu,
        Memory,
        Publisher,
    }

    impl ColumnKey for TestCol {
        fn slug(self) -> &'static str {
            match self {
                Self::Name => "name",
                Self::Cpu => "cpu",
                Self::Memory => "memory",
                Self::Publisher => "publisher",
            }
        }
    }

    fn sample_layout() -> TableColumnLayout<TestCol> {
        let mut layout = TableColumnLayout::new(vec![
            ColumnSpec::new(TestCol::Name, 200.0, 100.0, 400.0),
            ColumnSpec::new(TestCol::Cpu, 80.0, 50.0, 200.0),
            ColumnSpec::new(TestCol::Memory, 100.0, 60.0, 240.0),
            ColumnSpec::new(TestCol::Publisher, 140.0, 80.0, 320.0),
        ]);
        // Assign contiguous horizontal bounds [0..200], [200..280], [280..380], [380..520].
        let mut x = 0.0;
        for (ix, spec) in layout.columns().to_vec().into_iter().enumerate() {
            layout.set_bounds(
                ix,
                Bounds::new(point(px(x), px(0.)), size(px(spec.width), px(32.))),
            );
            x += spec.width;
        }
        layout
    }

    #[test]
    fn resize_clamps_to_min_and_max_bounds() {
        let mut layout = sample_layout();

        layout.begin_resize(1, px(280.));
        assert!(layout.is_resizing(1));
        assert!(layout.update_resize(1, px(320.)));
        assert_eq!(layout.columns()[1].width, 120.0);

        // Shrink below min_width (50.0) clamps to 50.0.
        assert!(layout.update_resize(1, px(100.)));
        assert_eq!(layout.columns()[1].width, 50.0);

        // Expand past max_width (200.0) clamps to 200.0.
        assert!(layout.update_resize(1, px(900.)));
        assert_eq!(layout.columns()[1].width, 200.0);

        assert!(layout.end_resize());
        assert!(!layout.is_resizing(1));
        assert!(!layout.end_resize());
    }

    #[test]
    fn drag_gap_and_commit_reorder_forward_and_backward() {
        let mut layout = sample_layout();

        // Dropping Cpu (ix=1) near its own edges is a no-op.
        assert_eq!(layout.drag_gap_at(px(210.), 1), None);
        assert_eq!(layout.drag_gap_at(px(270.), 1), None);

        // Dragging Cpu (ix=1) to the left of Name's center (100) targets gap 0.
        assert_eq!(layout.drag_gap_at(px(40.), 1), Some(0));
        assert!(layout.set_drag_gap(Some(0)));
        assert!(layout.commit_drag_drop(1));
        assert_eq!(
            layout.columns().iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![
                TestCol::Cpu,
                TestCol::Name,
                TestCol::Memory,
                TestCol::Publisher
            ]
        );

        // Rebuild bounds for the new order and drag Cpu (now ix=0) past Publisher's center to gap 4.
        let mut x = 0.0;
        for (ix, spec) in layout.columns().to_vec().into_iter().enumerate() {
            layout.set_bounds(
                ix,
                Bounds::new(point(px(x), px(0.)), size(px(spec.width), px(32.))),
            );
            x += spec.width;
        }
        assert_eq!(layout.drag_gap_at(px(500.), 0), Some(4));
        assert!(layout.set_drag_gap(Some(4)));
        assert!(layout.commit_drag_drop(0));
        assert_eq!(
            layout.columns().iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![
                TestCol::Name,
                TestCol::Memory,
                TestCol::Publisher,
                TestCol::Cpu
            ]
        );
    }

    #[test]
    fn saved_snapshot_roundtrip_handles_missing_and_extra_columns() {
        let mut layout = sample_layout();
        layout.resize_column(0, 260.0);
        layout.move_column(3, 1);
        let saved = layout.snapshot();

        // Simulate restoring into a fresh layout (plus an unknown legacy slug and out-of-range width).
        let mut extended_saved = saved;
        extended_saved.push(("legacy_removed_col", 999.0));
        extended_saved[0].1 = 9999.0; // should clamp to Name max_width (400.0)

        let mut restored = sample_layout();
        restored.apply_saved(&extended_saved);

        assert_eq!(
            restored.columns().iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![
                TestCol::Name,
                TestCol::Publisher,
                TestCol::Cpu,
                TestCol::Memory
            ]
        );
        assert_eq!(restored.columns()[0].width, 400.0);
    }

    struct InteractiveHeaderHarness {
        layout: TableColumnLayout<TestCol>,
        sorts: Vec<TestCol>,
        commits: usize,
    }

    impl Render for InteractiveHeaderHarness {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let cols = self.layout.columns().to_vec();
            let total = cols.len();
            let snap = self.layout.clone();

            let header_cells = cols.iter().enumerate().map(|(ix, spec)| {
                let col = spec.id;
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
    fn interactive_header_supports_sort_click_column_resize_and_column_reorder(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::{Modifiers, VisualTestContext};

        cx.update(gpui_kit::init);
        let view = cx.new(|_| InteractiveHeaderHarness {
            layout: TableColumnLayout::new(vec![
                ColumnSpec::new(TestCol::Name, 200.0, 100.0, 400.0),
                ColumnSpec::new(TestCol::Cpu, 80.0, 50.0, 200.0),
                ColumnSpec::new(TestCol::Memory, 100.0, 60.0, 240.0),
                ColumnSpec::new(TestCol::Publisher, 140.0, 80.0, 320.0),
            ]),
            sorts: Vec::new(),
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
                h.layout.columns().iter().map(|c| c.id).collect::<Vec<_>>(),
                vec![
                    TestCol::Name,
                    TestCol::Cpu,
                    TestCol::Memory,
                    TestCol::Publisher
                ]
            );
        });

        // 2. Drag the resize divider at Name's right border (x = 199, y = 16) +50px to x = 249.
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
                h.layout.columns()[0].width,
                250.0,
                "dragging Name divider +50px must resize Name from 200px to 250px"
            );
            assert!(h.commits >= 1, "resize completion must commit layout");
        });

        // 3. Drag Cpu header (now at x = 250..330, center 290) past Memory (330..430) to x = 420.
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
                h.layout.columns().iter().map(|c| c.id).collect::<Vec<_>>(),
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
