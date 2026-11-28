//! Pure column order, visibility, width, hit-testing, and viewport-mask state
//! shared by `DataTable` views.

use std::ops::Range;

use gpui_kit::Pixels;
use gpui_kit::component::table::ColumnSort;
use loom_config::TableColumnSetting;
use loom_types::SortDirection;

/// Maps Loom's [`SortDirection`] to `gpui-kit`'s [`ColumnSort`].
#[inline]
pub(crate) const fn to_column_sort(dir: SortDirection) -> ColumnSort {
    match dir {
        SortDirection::Ascending => ColumnSort::Ascending,
        SortDirection::Descending => ColumnSort::Descending,
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
