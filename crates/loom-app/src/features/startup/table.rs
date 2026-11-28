//! 2D-virtualized `gpui-kit` [`TableDelegate`] implementation for [`super::StartupView`].

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Size, StyledExt as _, h_flex, v_flex};
use gpui_kit::{
    App, Context, Empty, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, img,
    prelude::FluentBuilder as _, px,
};
use loom_types::{CapabilityFlags, StartupEntry, StartupImpact, StartupSlice, StartupSortColumn};
use loom_ui_kit::UNAVAILABLE_LABEL;

use crate::bridge::{UiCommands, UiStore};
use crate::features::table_columns::{BudgetedIconLookup, IconRenderCache};
use crate::i18n;

use super::state::{
    STARTUP_COLUMNS, StartupLocaleCache, StartupToolbarState, column_default_direction,
    column_min_width, column_slug, column_width, to_column_sort,
};

/// 2D-virtualized `gpui-kit` [`TableDelegate`] backing [`super::StartupView`].
pub(super) struct StartupTableDelegate {
    store: Entity<UiStore>,
    pub(super) slice: Arc<StartupSlice>,
    pub(super) capabilities: CapabilityFlags,
    pub(super) last_rev: u64,
    pub(super) selected_id: Option<Arc<str>>,
    pub(super) reanchored_selection: Option<Option<usize>>,
    pub(super) visible_rows: Range<usize>,
    pub(super) last_scroll_at: Option<Instant>,
    pub(super) last_viewport_fp: Option<u64>,
    pub(super) user_action_pending: bool,
    pub(super) locale: StartupLocaleCache,
    icon_cache: IconRenderCache,
    icon_catchup_scheduled: bool,
}

impl StartupTableDelegate {
    pub(super) fn new(store: Entity<UiStore>, cx: &App) -> Self {
        let store_ref = store.read(cx);
        let snap = store_ref.snapshot();
        let slice = Arc::clone(&snap.startup);
        let capabilities = snap.capabilities;

        Self {
            store: store.clone(),
            last_rev: snap.revisions.startup,
            slice,
            capabilities,
            selected_id: None,
            reanchored_selection: None,
            visible_rows: 0..0,
            last_scroll_at: None,
            last_viewport_fp: None,
            user_action_pending: false,
            locale: StartupLocaleCache::build(),
            icon_cache: IconRenderCache::new(),
            icon_catchup_scheduled: false,
        }
    }

    #[inline]
    pub(super) fn row_at(&self, row_ix: usize) -> Option<&StartupEntry> {
        let item_idx = *self.slice.visible_indices.get(row_ix)?;
        self.slice.items.get(item_idx)
    }

    /// Locates `id` in `slice.visible_indices`, checking `hint_row` in O(1) before scanning.
    #[inline]
    pub(super) fn find_visible_index_by_id(
        &self,
        id: &str,
        hint_row: Option<usize>,
    ) -> Option<usize> {
        if let Some(hint) = hint_row
            && self
                .row_at(hint)
                .is_some_and(|entry| entry.id.as_ref() == id)
        {
            return Some(hint);
        }
        self.slice.visible_indices.iter().position(|&idx| {
            self.slice
                .items
                .get(idx)
                .is_some_and(|entry| entry.id.as_ref() == id)
        })
    }

    pub(super) fn selected_entry(&self, hint_row: Option<usize>) -> Option<&StartupEntry> {
        let id = self.selected_id.as_deref()?;
        let row_ix = self.find_visible_index_by_id(id, hint_row)?;
        self.row_at(row_ix)
    }

    pub(super) fn toolbar_state(&self, hint_row: Option<usize>) -> StartupToolbarState {
        if !self.capabilities.contains(CapabilityFlags::STARTUP_APPS) {
            return StartupToolbarState::default();
        }
        let Some(entry) = self.selected_entry(hint_row) else {
            return StartupToolbarState::default();
        };
        StartupToolbarState {
            can_enable: !entry.enabled,
            can_disable: entry.enabled,
        }
    }

    pub(super) fn trigger_sort(&mut self, col_ix: usize, cx: &mut Context<TableState<Self>>) {
        let Some(&target) = STARTUP_COLUMNS.get(col_ix) else {
            return;
        };
        let next_dir = if self.slice.sort_column == target {
            self.slice.sort_direction.toggle()
        } else {
            column_default_direction(target)
        };
        self.user_action_pending = true;
        let store = self.store.clone();
        UiCommands::set_startup_sort(&store, target, next_dir, cx);
    }
}

impl TableDelegate for StartupTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        STARTUP_COLUMNS.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.slice.visible_indices.len()
    }

    fn loading(&self, _cx: &App) -> bool {
        self.slice.revision == 0
    }

    fn visible_rows_changed(
        &mut self,
        visible_range: Range<usize>,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) {
        if self.visible_rows != visible_range {
            let scrolled = !self.visible_rows.is_empty();
            self.visible_rows = visible_range;
            self.last_viewport_fp = None;
            if scrolled {
                self.last_scroll_at = Some(Instant::now());
            }
        }
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        let Some(&col) = STARTUP_COLUMNS.get(col_ix) else {
            return Column::default();
        };
        let mut column = Column::new(column_slug(col), self.locale.column_label(col))
            .width(px(column_width(col)))
            .min_width(px(column_min_width(col)))
            .selectable(false)
            .movable(false);

        if col == StartupSortColumn::Name {
            column = column.fixed_left();
        }
        if col == self.slice.sort_column {
            column = column.sort(to_column_sort(self.slice.sort_direction));
        } else {
            column = column.sortable();
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
        self.trigger_sort(col_ix, cx);
    }

    fn render_header(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        self.icon_cache.begin_frame();
        div()
            .id("startup-column-header")
            .bg(cx.theme().secondary.opacity(0.45))
            .text_xs()
            .font_medium()
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(&col) = STARTUP_COLUMNS.get(col_ix) else {
            return Empty.into_any_element();
        };
        let is_active = col == self.slice.sort_column;
        let fg = cx.theme().foreground;
        let muted_fg = cx.theme().muted_foreground;
        let label = self.locale.column_label(col);

        h_flex()
            .id(("startup-th", col_ix))
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .text_xs()
            .text_color(if is_active { fg } else { muted_fg })
            .when(is_active, |d| d.font_semibold())
            .cursor_pointer()
            .on_click(cx.listener(move |table, _, _, cx| {
                table.delegate_mut().trigger_sort(col_ix, cx);
            }))
            .child(div().min_w_0().truncate().child(label))
            .into_any_element()
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        div().id(("startup-row", row_ix)).text_xs()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(&item_idx) = self.slice.visible_indices.get(row_ix) else {
            return Empty.into_any_element();
        };
        let Some(entry) = self.slice.items.get(item_idx) else {
            return Empty.into_any_element();
        };
        let Some(&col) = STARTUP_COLUMNS.get(col_ix) else {
            return Empty.into_any_element();
        };

        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let primary = theme.primary;
        let warning = theme.warning;
        let danger = theme.danger;

        match col {
            StartupSortColumn::Name => {
                let name = SharedString::from(Arc::clone(&entry.name));
                let rendered_icon = entry.icon.as_ref().and_then(|icon| {
                    match self.icon_cache.get_or_insert_budgeted(icon) {
                        BudgetedIconLookup::Hit(img) | BudgetedIconLookup::Uploaded(img) => {
                            Some(img)
                        }
                        BudgetedIconLookup::Deferred => {
                            if !self.icon_catchup_scheduled {
                                self.icon_catchup_scheduled = true;
                                let table_weak = cx.entity().downgrade();
                                window.on_next_frame(move |_window, cx| {
                                    let _ = table_weak.update(cx, |table, cx| {
                                        let d = table.delegate_mut();
                                        d.icon_catchup_scheduled = false;
                                        d.icon_cache.begin_frame();
                                        cx.notify();
                                    });
                                });
                            }
                            None
                        }
                        BudgetedIconLookup::Invalid => None,
                    }
                });
                let icon_el = if let Some(render_img) = rendered_icon {
                    img(render_img)
                        .size(px(16.0))
                        .flex_shrink_0()
                        .into_any_element()
                } else {
                    Icon::new(IconName::Frame)
                        .size(px(15.0))
                        .text_color(muted_fg.opacity(0.6))
                        .into_any_element()
                };
                h_flex()
                    .size_full()
                    .items_center()
                    .gap_2()
                    .overflow_hidden()
                    .child(icon_el)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .font_medium()
                            .text_color(fg)
                            .child(name),
                    )
                    .into_any_element()
            }
            StartupSortColumn::Publisher => {
                let text = entry
                    .publisher
                    .as_ref()
                    .filter(|s| !s.is_empty())
                    .map(|s| SharedString::from(Arc::clone(s)))
                    .unwrap_or_else(|| SharedString::new_static(UNAVAILABLE_LABEL));
                h_flex()
                    .size_full()
                    .items_center()
                    .truncate()
                    .text_xs()
                    .text_color(muted_fg)
                    .child(text)
                    .into_any_element()
            }
            StartupSortColumn::Status => {
                let (label, color) = if entry.enabled {
                    (self.locale.enabled.clone(), primary)
                } else {
                    (self.locale.disabled.clone(), muted_fg)
                };
                h_flex()
                    .size_full()
                    .items_center()
                    .truncate()
                    .text_xs()
                    .text_color(color)
                    .child(label)
                    .into_any_element()
            }
            StartupSortColumn::Impact => {
                let color = match entry.impact {
                    StartupImpact::High => danger,
                    StartupImpact::Medium => warning,
                    StartupImpact::Low | StartupImpact::NotMeasured | StartupImpact::None => {
                        muted_fg
                    }
                };
                h_flex()
                    .size_full()
                    .items_center()
                    .truncate()
                    .text_xs()
                    .text_color(color)
                    .child(self.locale.impact_label(entry.impact))
                    .into_any_element()
            }
        }
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(entry) = self.row_at(row_ix).cloned() else {
            return menu;
        };

        let supported = self.capabilities.contains(CapabilityFlags::STARTUP_APPS);
        let has_file_path = entry
            .file_path
            .as_deref()
            .is_some_and(|p| !p.trim().is_empty());

        let store_enable = self.store.clone();
        let id_enable = entry.id.to_string();

        let store_disable = self.store.clone();
        let id_disable = entry.id.to_string();

        let store_open_loc = self.store.clone();
        let path_open_loc = entry.file_path.clone();

        let store_props = self.store.clone();
        let path_props = entry.file_path.clone();

        let store_search = self.store.clone();
        let search_query = entry.name.to_string();

        menu.item(
            PopupMenuItem::new(i18n::tr("startup.menu_enable"))
                .icon(IconName::CircleCheck)
                .disabled(!supported || entry.enabled)
                .on_click(move |_, _, cx| {
                    UiCommands::set_startup_enabled(&store_enable, id_enable.clone(), true, cx);
                }),
        )
        .item(
            PopupMenuItem::new(i18n::tr("startup.menu_disable"))
                .icon(IconName::Ban)
                .disabled(!supported || !entry.enabled)
                .on_click(move |_, _, cx| {
                    UiCommands::set_startup_enabled(&store_disable, id_disable.clone(), false, cx);
                }),
        )
        .separator()
        .item(
            PopupMenuItem::new(i18n::tr("startup.menu_open_file_location"))
                .icon(IconName::FolderOpen)
                .disabled(!has_file_path)
                .on_click(move |_, _, cx| {
                    if let Some(ref path) = path_open_loc {
                        UiCommands::open_file_location(
                            &store_open_loc,
                            PathBuf::from(path.as_ref()),
                            cx,
                        );
                    }
                }),
        )
        .item(
            PopupMenuItem::new(i18n::tr("startup.menu_properties"))
                .icon(IconName::Info)
                .disabled(!has_file_path)
                .on_click(move |_, _, cx| {
                    if let Some(ref path) = path_props {
                        UiCommands::show_file_properties(
                            &store_props,
                            PathBuf::from(path.as_ref()),
                            cx,
                        );
                    }
                }),
        )
        .separator()
        .item(
            PopupMenuItem::new(i18n::tr("startup.menu_search_online"))
                .icon(IconName::Globe)
                .on_click(move |_, _, cx| {
                    UiCommands::search_online(&store_search, search_query.clone(), cx);
                }),
        )
    }

    fn row_label(&self, row_ix: usize, _cx: &App) -> Option<SharedString> {
        let entry = self.row_at(row_ix)?;
        let status = if entry.enabled {
            &self.locale.enabled
        } else {
            &self.locale.disabled
        };
        Some(format!("{} ({})", entry.name, status).into())
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
                    .child(self.locale.empty.clone()),
            )
    }
}
