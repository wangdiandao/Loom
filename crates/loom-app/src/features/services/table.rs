//! 2D-virtualized `gpui-kit` [`TableDelegate`] implementation for [`super::ServicesView`].

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::time::Instant;

use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Size, StyledExt as _, h_flex, v_flex};
use gpui_kit::{
    App, Context, Empty, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use loom_types::{CapabilityFlags, ServiceRecord, ServiceState, ServicesSlice, ServicesSortColumn};
use loom_ui_kit::{SMALL_U32_TABLE_MAX, UNAVAILABLE_LABEL, format_u32_shared};

use crate::bridge::{UiCommands, UiStore};
use crate::i18n;
use crate::shell::ShellHandle;

use super::state::{
    PID_TEXT_CACHE_LIMIT, SERVICES_COLUMNS, ServicesLocaleCache, ServicesToolbarState,
    can_start_service, can_stop_or_restart_service, column_default_direction, column_min_width,
    column_right_aligned, column_slug, column_width, resolve_process_key_by_pid,
    service_description_shared, to_column_sort,
};

/// 2D-virtualized `gpui-kit` [`TableDelegate`] backing [`super::ServicesView`].
pub(super) struct ServicesTableDelegate {
    store: Entity<UiStore>,
    shell_handle: ShellHandle,
    pub(super) slice: Arc<ServicesSlice>,
    pub(super) capabilities: CapabilityFlags,
    pub(super) last_rev: u64,
    pub(super) selected_name: Option<Arc<str>>,
    pub(super) reanchored_selection: Option<Option<usize>>,
    pub(super) visible_rows: Range<usize>,
    pub(super) last_scroll_at: Option<Instant>,
    pub(super) last_viewport_fp: Option<u64>,
    pub(super) user_action_pending: bool,
    pid_text_cache: HashMap<u32, SharedString>,
    pub(super) locale: ServicesLocaleCache,
}

impl ServicesTableDelegate {
    pub(super) fn new(store: Entity<UiStore>, shell_handle: ShellHandle, cx: &App) -> Self {
        let store_ref = store.read(cx);
        let snap = store_ref.snapshot();
        let slice = Arc::clone(&snap.services);
        let capabilities = snap.capabilities;

        Self {
            store: store.clone(),
            shell_handle,
            last_rev: snap.revisions.services,
            slice,
            capabilities,
            selected_name: None,
            reanchored_selection: None,
            visible_rows: 0..0,
            last_scroll_at: None,
            last_viewport_fp: None,
            user_action_pending: false,
            pid_text_cache: HashMap::new(),
            locale: ServicesLocaleCache::build(),
        }
    }

    #[inline]
    pub(super) fn row_at(&self, row_ix: usize) -> Option<&ServiceRecord> {
        let item_idx = *self.slice.visible_indices.get(row_ix)?;
        self.slice.items.get(item_idx)
    }

    /// Locates `name` in `slice.visible_indices`, checking `hint_row` in O(1) before scanning.
    #[inline]
    pub(super) fn find_visible_index_by_name(
        &self,
        name: &str,
        hint_row: Option<usize>,
    ) -> Option<usize> {
        if let Some(hint) = hint_row
            && self
                .row_at(hint)
                .is_some_and(|record| record.name.as_ref() == name)
        {
            return Some(hint);
        }
        self.slice.visible_indices.iter().position(|&idx| {
            self.slice
                .items
                .get(idx)
                .is_some_and(|record| record.name.as_ref() == name)
        })
    }

    pub(super) fn selected_record(&self, hint_row: Option<usize>) -> Option<&ServiceRecord> {
        let sel = self.selected_name.as_deref()?;
        let row_ix = self.find_visible_index_by_name(sel, hint_row)?;
        self.row_at(row_ix)
    }

    pub(super) fn toolbar_state(&self, hint_row: Option<usize>) -> ServicesToolbarState {
        if !self.capabilities.contains(CapabilityFlags::SERVICES) {
            return ServicesToolbarState::default();
        }
        let Some(record) = self.selected_record(hint_row) else {
            return ServicesToolbarState::default();
        };
        ServicesToolbarState {
            can_start: can_start_service(record.state),
            can_stop: can_stop_or_restart_service(record.state),
            can_restart: can_stop_or_restart_service(record.state),
        }
    }

    #[inline]
    fn pid_shared(&mut self, pid: u32) -> SharedString {
        if pid <= SMALL_U32_TABLE_MAX {
            return format_u32_shared(pid);
        }
        if self.pid_text_cache.len() >= PID_TEXT_CACHE_LIMIT
            && !self.pid_text_cache.contains_key(&pid)
        {
            self.pid_text_cache.clear();
        }
        self.pid_text_cache
            .entry(pid)
            .or_insert_with(|| format_u32_shared(pid))
            .clone()
    }

    pub(super) fn trigger_sort(&mut self, col_ix: usize, cx: &mut Context<TableState<Self>>) {
        let Some(&target) = SERVICES_COLUMNS.get(col_ix) else {
            return;
        };
        let next_dir = if self.slice.sort_column == target {
            self.slice.sort_direction.toggle()
        } else {
            column_default_direction(target)
        };
        self.user_action_pending = true;
        let store = self.store.clone();
        UiCommands::set_services_sort(&store, target, next_dir, cx);
    }
}

impl TableDelegate for ServicesTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        SERVICES_COLUMNS.len()
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
        let Some(&col) = SERVICES_COLUMNS.get(col_ix) else {
            return Column::default();
        };
        let mut column = Column::new(column_slug(col), self.locale.column_label(col))
            .width(px(column_width(col)))
            .min_width(px(column_min_width(col)))
            .selectable(false)
            .movable(false);

        if col == ServicesSortColumn::Name {
            column = column.fixed_left();
        }
        if column_right_aligned(col) {
            column = column.text_right();
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
        div()
            .id("services-column-header")
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
        let Some(&col) = SERVICES_COLUMNS.get(col_ix) else {
            return Empty.into_any_element();
        };
        let is_active = col == self.slice.sort_column;
        let fg = cx.theme().foreground;
        let muted_fg = cx.theme().muted_foreground;
        let label = self.locale.column_label(col);

        h_flex()
            .id(("services-th", col_ix))
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .when(column_right_aligned(col), |d| d.justify_end())
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
        div().id(("services-row", row_ix)).text_xs()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(&col) = SERVICES_COLUMNS.get(col_ix) else {
            return Empty.into_any_element();
        };
        let Some(&item_idx) = self.slice.visible_indices.get(row_ix) else {
            return Empty.into_any_element();
        };
        if item_idx >= self.slice.items.len() {
            return Empty.into_any_element();
        }

        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let primary = theme.primary;
        let mono_font = theme.mono_font_family.clone();

        match col {
            ServicesSortColumn::Name => {
                let text = SharedString::from(Arc::clone(&self.slice.items[item_idx].name));
                h_flex()
                    .size_full()
                    .items_center()
                    .truncate()
                    .text_xs()
                    .font_medium()
                    .text_color(fg)
                    .child(text)
                    .into_any_element()
            }
            ServicesSortColumn::Pid => {
                let pid = self.slice.items[item_idx].pid;
                let text = match pid {
                    Some(pid) => self.pid_shared(pid),
                    None => SharedString::new_static(UNAVAILABLE_LABEL),
                };
                h_flex()
                    .size_full()
                    .items_center()
                    .justify_end()
                    .truncate()
                    .text_xs()
                    .font_family(mono_font)
                    .text_color(muted_fg)
                    .child(text)
                    .into_any_element()
            }
            ServicesSortColumn::Description => {
                let text = service_description_shared(&self.slice.items[item_idx]);
                h_flex()
                    .size_full()
                    .items_center()
                    .truncate()
                    .text_xs()
                    .text_color(muted_fg)
                    .child(text)
                    .into_any_element()
            }
            ServicesSortColumn::Status => {
                let state = self.slice.items[item_idx].state;
                let color = if state == ServiceState::Running {
                    primary
                } else {
                    muted_fg
                };
                h_flex()
                    .size_full()
                    .items_center()
                    .truncate()
                    .text_xs()
                    .text_color(color)
                    .child(self.locale.state_label(state))
                    .into_any_element()
            }
            ServicesSortColumn::Group => {
                let text = self.slice.items[item_idx]
                    .group
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
        }
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(record) = self.row_at(row_ix).cloned() else {
            return menu;
        };

        let supported = self.capabilities.contains(CapabilityFlags::SERVICES);
        let can_start = supported && can_start_service(record.state);
        let can_stop = supported && can_stop_or_restart_service(record.state);
        let can_restart = supported && can_stop_or_restart_service(record.state);
        let resolved_key = record
            .pid
            .and_then(|pid| resolve_process_key_by_pid(self.store.read(cx).snapshot(), pid));

        let store_start = self.store.clone();
        let name_start = record.name.to_string();

        let store_stop = self.store.clone();
        let name_stop = record.name.to_string();

        let store_restart = self.store.clone();
        let name_restart = record.name.to_string();

        let store_console = self.store.clone();
        let shell_details = self.shell_handle.clone();

        let store_search = self.store.clone();
        let search_query = format!("{} {}", record.name, record.display_name);

        menu.item(
            PopupMenuItem::new(i18n::tr("services.menu_start"))
                .icon(IconName::Play)
                .disabled(!can_start)
                .on_click(move |_, _, cx| {
                    UiCommands::start_service(&store_start, name_start.clone(), cx);
                }),
        )
        .item(
            PopupMenuItem::new(i18n::tr("services.menu_stop"))
                .icon(IconName::Square)
                .disabled(!can_stop)
                .on_click(move |_, _, cx| {
                    UiCommands::stop_service(&store_stop, name_stop.clone(), cx);
                }),
        )
        .item(
            PopupMenuItem::new(i18n::tr("services.menu_restart"))
                .icon(IconName::RotateCw)
                .disabled(!can_restart)
                .on_click(move |_, _, cx| {
                    UiCommands::restart_service(&store_restart, name_restart.clone(), cx);
                }),
        )
        .separator()
        .item(
            PopupMenuItem::new(i18n::tr("services.menu_open_services"))
                .icon(IconName::ExternalLink)
                .disabled(!supported)
                .on_click(move |_, _, cx| {
                    UiCommands::open_services_console(&store_console, cx);
                }),
        )
        .item(
            PopupMenuItem::new(i18n::tr("services.menu_go_to_details"))
                .icon(IconName::ArrowRight)
                .disabled(resolved_key.is_none())
                .on_click(move |_, window, cx| {
                    if let Some(key) = resolved_key {
                        shell_details.jump_to_details(key, window, cx);
                    }
                }),
        )
        .separator()
        .item(
            PopupMenuItem::new(i18n::tr("services.menu_search_online"))
                .icon(IconName::Globe)
                .on_click(move |_, _, cx| {
                    UiCommands::search_online(&store_search, search_query.clone(), cx);
                }),
        )
    }

    fn row_label(&self, row_ix: usize, _cx: &App) -> Option<SharedString> {
        let record = self.row_at(row_ix)?;
        let status = self.locale.state_label(record.state);
        if let Some(pid) = record.pid {
            Some(format!("{} (PID {}, {})", record.name, pid, status).into())
        } else {
            Some(format!("{} ({})", record.name, status).into())
        }
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
