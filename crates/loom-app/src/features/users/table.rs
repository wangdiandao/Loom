//! 2D-virtualized `gpui-kit` [`TableDelegate`] implementation for [`super::UsersView`].

use std::ops::Range;
use std::sync::Arc;
use std::time::Instant;

use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableState};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, Size, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, App, Context, Empty, Entity, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use loom_types::{
    CapabilityFlags, ProcessKey, ProcessStatusFlags, ResourceDisplayMode, UserSessionState,
    UsersRowProjection, UsersSlice, UsersSortColumn,
};
use loom_ui_kit::{
    CachedSemanticColors, HeatmapLevel, LoomSemanticColors, RowTextCache,
    format_child_count_shared, format_permille_metric, heatmap_cell_metric,
};

use crate::bridge::{UiCommands, UiStore};
use crate::i18n;
use crate::shell::ShellHandle;

use super::dialogs::{
    launch_manage_user_accounts, open_disconnect_session_dialog, request_end_user_process,
};
use super::state::{
    SESSION_CACHE_CREATE_TIME_SENTINEL, USERS_COLUMNS, UsersLocaleCache, UsersRowKey,
    column_default_direction, column_is_heatmap, column_min_width, column_right_aligned,
    column_slug, column_width, memory_heatmap_level, row_key, to_column_sort,
};

/// 2D-virtualized `gpui-kit` [`TableDelegate`] backing [`super::UsersView`].
pub(super) struct UsersTableDelegate {
    store: Entity<UiStore>,
    shell_handle: ShellHandle,
    pub(super) slice: Arc<UsersSlice>,
    pub(super) capabilities: CapabilityFlags,
    pub(super) total_ram_bytes: u64,
    pub(super) last_rev: u64,
    pub(super) selected_key: Option<UsersRowKey>,
    pub(super) reanchored_selection: Option<Option<usize>>,
    pub(super) visible_rows: Range<usize>,
    pub(super) last_scroll_at: Option<Instant>,
    pub(super) last_viewport_fp: Option<u64>,
    pub(super) user_action_pending: bool,
    semantic_colors: CachedSemanticColors,
    pub(super) row_text_cache: RowTextCache,
    pub(super) locale: UsersLocaleCache,
}

impl UsersTableDelegate {
    pub(super) fn new(store: Entity<UiStore>, shell_handle: ShellHandle, cx: &App) -> Self {
        let store_ref = store.read(cx);
        let snap = store_ref.snapshot();
        let slice = Arc::clone(&snap.users);
        let capabilities = snap.capabilities;
        let total_ram_bytes = snap.performance.latest.mem_total_bytes;

        Self {
            store: store.clone(),
            shell_handle,
            last_rev: snap.revisions.users,
            slice,
            capabilities,
            total_ram_bytes,
            selected_key: None,
            reanchored_selection: None,
            visible_rows: 0..0,
            last_scroll_at: None,
            last_viewport_fp: None,
            user_action_pending: false,
            semantic_colors: CachedSemanticColors::new(),
            row_text_cache: RowTextCache::new(),
            locale: UsersLocaleCache::build(),
        }
    }

    /// Locates `key` in `slice.rows`, checking `hint_row` in O(1) before scanning.
    #[inline]
    pub(super) fn find_row_index(
        &self,
        key: UsersRowKey,
        hint_row: Option<usize>,
    ) -> Option<usize> {
        if let Some(hint) = hint_row
            && self
                .slice
                .rows
                .get(hint)
                .is_some_and(|row| row_key(row) == key)
        {
            return Some(hint);
        }
        self.slice.rows.iter().position(|row| row_key(row) == key)
    }

    pub(super) fn selected_session(&self, hint_row: Option<usize>) -> Option<(u32, String)> {
        let sel = self.selected_key?;
        let row_ix = self.find_row_index(sel, hint_row)?;
        match self.slice.rows.get(row_ix)? {
            UsersRowProjection::Session {
                session_id,
                user_name,
                ..
            } => Some((*session_id, user_name.to_string())),
            UsersRowProjection::Process { .. } => None,
        }
    }

    pub(super) fn can_disconnect_selected(&self, hint_row: Option<usize>) -> bool {
        self.capabilities.contains(CapabilityFlags::USER_SESSIONS)
            && self.selected_session(hint_row).is_some()
    }

    pub(super) fn trigger_sort(&mut self, col_ix: usize, cx: &mut Context<TableState<Self>>) {
        let Some(&target) = USERS_COLUMNS.get(col_ix) else {
            return;
        };
        let next_dir = if self.slice.sort_column == target {
            self.slice.sort_direction.toggle()
        } else {
            column_default_direction(target)
        };
        self.user_action_pending = true;
        let store = self.store.clone();
        UiCommands::set_users_sort(&store, target, next_dir, cx);
    }

    pub(super) fn trigger_toggle_session(
        &mut self,
        session_id: u32,
        cx: &mut Context<TableState<Self>>,
    ) {
        self.user_action_pending = true;
        let store = self.store.clone();
        UiCommands::toggle_user_session_expanded(&store, session_id, cx);
    }

    #[allow(clippy::too_many_arguments)]
    fn render_metric_cell(
        &mut self,
        col: UsersSortColumn,
        cache_key: ProcessKey,
        cpu_permille: u16,
        memory_bytes: u64,
        disk_bytes_per_sec: u64,
        net_bytes_per_sec: u64,
        colors: &LoomSemanticColors,
        mono_font: SharedString,
    ) -> AnyElement {
        match col {
            UsersSortColumn::Cpu => heatmap_cell_metric(
                format_permille_metric(cpu_permille),
                HeatmapLevel::from_permille(cpu_permille),
                colors,
                mono_font,
            )
            .into_any_element(),
            UsersSortColumn::Memory => {
                let metric = self.row_text_cache.memory_metric(
                    cache_key,
                    memory_bytes,
                    0,
                    ResourceDisplayMode::Values,
                );
                heatmap_cell_metric(
                    metric,
                    memory_heatmap_level(memory_bytes, self.total_ram_bytes),
                    colors,
                    mono_font,
                )
                .into_any_element()
            }
            UsersSortColumn::Disk => {
                let metric = self.row_text_cache.disk_metric(
                    cache_key,
                    disk_bytes_per_sec,
                    0,
                    ResourceDisplayMode::Values,
                );
                heatmap_cell_metric(
                    metric,
                    HeatmapLevel::from_bytes_per_sec(disk_bytes_per_sec),
                    colors,
                    mono_font,
                )
                .into_any_element()
            }
            UsersSortColumn::Network => {
                let metric = self.row_text_cache.network_metric(
                    cache_key,
                    net_bytes_per_sec,
                    0,
                    ResourceDisplayMode::Values,
                );
                heatmap_cell_metric(
                    metric,
                    HeatmapLevel::from_bytes_per_sec(net_bytes_per_sec),
                    colors,
                    mono_font,
                )
                .into_any_element()
            }
            UsersSortColumn::User | UsersSortColumn::Status => Empty.into_any_element(),
        }
    }
}

impl TableDelegate for UsersTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        USERS_COLUMNS.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.slice.rows.len()
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
        let Some(&col) = USERS_COLUMNS.get(col_ix) else {
            return Column::default();
        };
        let mut column = Column::new(column_slug(col), self.locale.column_label(col))
            .width(px(column_width(col)))
            .min_width(px(column_min_width(col)))
            .selectable(false)
            .movable(false);

        if col == UsersSortColumn::User {
            column = column.fixed_left();
        }
        if column_right_aligned(col) {
            column = column.text_right();
        }
        if column_is_heatmap(col) {
            column = column.p_0();
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
            .id("users-column-header")
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
        let Some(&col) = USERS_COLUMNS.get(col_ix) else {
            return Empty.into_any_element();
        };
        let is_active = col == self.slice.sort_column;
        let fg = cx.theme().foreground;
        let muted_fg = cx.theme().muted_foreground;
        let label = self.locale.column_label(col);

        h_flex()
            .id(("users-th", col_ix))
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .when(column_is_heatmap(col), |d| d.px_2())
            .when(column_right_aligned(col), |d| d.justify_end())
            .text_xs()
            .text_color(if is_active { fg } else { muted_fg })
            .when(is_active, |d| d.font_semibold())
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
        div().id(("users-row", row_ix)).text_xs()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(&col) = USERS_COLUMNS.get(col_ix) else {
            return Empty.into_any_element();
        };
        if row_ix >= self.slice.rows.len() {
            return Empty.into_any_element();
        }

        let theme = cx.theme();
        let colors = self.semantic_colors.get_copy(theme);
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let primary = theme.primary;
        let list_hover = theme.list_hover;
        let mono_font = theme.mono_font_family.clone();

        match &self.slice.rows[row_ix] {
            UsersRowProjection::Session {
                session_id,
                user_name,
                state,
                process_count,
                cpu_permille,
                memory_bytes,
                disk_bytes_per_sec,
                net_bytes_per_sec,
                has_children,
                is_expanded,
            } => {
                let sid = *session_id;
                let state = *state;
                let process_count = *process_count;
                let cpu_permille = *cpu_permille;
                let memory_bytes = *memory_bytes;
                let disk_bytes_per_sec = *disk_bytes_per_sec;
                let net_bytes_per_sec = *net_bytes_per_sec;
                let has_children = *has_children;
                let is_expanded = *is_expanded;
                let cache_key = ProcessKey::new(sid, SESSION_CACHE_CREATE_TIME_SENTINEL);

                match col {
                    UsersSortColumn::User => {
                        let user_label = SharedString::from(Arc::clone(user_name));
                        let count_label = format_child_count_shared(process_count);
                        h_flex()
                            .size_full()
                            .items_center()
                            .gap_1p5()
                            .overflow_hidden()
                            .child(if has_children {
                                div()
                                    .id(("users-expand", sid as usize))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .size(px(18.))
                                    .flex_shrink_0()
                                    .rounded_sm()
                                    .hover(move |s| s.bg(list_hover))
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation();
                                    })
                                    .on_click(cx.listener(move |table, _, _, cx| {
                                        table.delegate_mut().trigger_toggle_session(sid, cx);
                                    }))
                                    .child(
                                        Icon::new(if is_expanded {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronRight
                                        })
                                        .xsmall()
                                        .text_color(muted_fg),
                                    )
                                    .into_any_element()
                            } else {
                                div().w(px(18.)).flex_shrink_0().into_any_element()
                            })
                            .child(
                                Icon::new(IconName::User)
                                    .xsmall()
                                    .text_color(primary)
                                    .flex_shrink_0(),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .font_medium()
                                    .text_color(fg)
                                    .child(user_label),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_xs()
                                    .text_color(muted_fg)
                                    .child(count_label),
                            )
                            .into_any_element()
                    }
                    UsersSortColumn::Status => {
                        let color = if state == UserSessionState::Active {
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
                    _ => self.render_metric_cell(
                        col,
                        cache_key,
                        cpu_permille,
                        memory_bytes,
                        disk_bytes_per_sec,
                        net_bytes_per_sec,
                        &colors,
                        mono_font,
                    ),
                }
            }
            UsersRowProjection::Process { process, .. } => {
                let proc_key = process.key;
                let status = process.status;
                let cpu_permille = process.cpu_permille;
                let memory_bytes = process.memory_bytes;
                let disk_bytes_per_sec = process.disk_bytes_per_sec;
                let net_bytes_per_sec = process.net_bytes_per_sec;

                match col {
                    UsersSortColumn::User => {
                        let display = if process.display_name.trim().is_empty() {
                            SharedString::from(Arc::clone(&process.name))
                        } else {
                            SharedString::from(Arc::clone(&process.display_name))
                        };
                        h_flex()
                            .size_full()
                            .items_center()
                            .gap_1p5()
                            .pl(px(28.))
                            .overflow_hidden()
                            .child(
                                Icon::new(IconName::File)
                                    .xsmall()
                                    .text_color(muted_fg)
                                    .flex_shrink_0(),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(fg)
                                    .child(display),
                            )
                            .into_any_element()
                    }
                    UsersSortColumn::Status => {
                        let text = if status.contains(ProcessStatusFlags::SUSPENDED) {
                            self.locale.suspended.clone()
                        } else {
                            self.locale.running.clone()
                        };
                        h_flex()
                            .size_full()
                            .items_center()
                            .truncate()
                            .text_xs()
                            .text_color(muted_fg)
                            .child(text)
                            .into_any_element()
                    }
                    _ => self.render_metric_cell(
                        col,
                        proc_key,
                        cpu_permille,
                        memory_bytes,
                        disk_bytes_per_sec,
                        net_bytes_per_sec,
                        &colors,
                        mono_font,
                    ),
                }
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
        let Some(row) = self.slice.rows.get(row_ix).cloned() else {
            return menu;
        };

        match row {
            UsersRowProjection::Session {
                session_id,
                user_name,
                has_children,
                is_expanded,
                ..
            } => {
                let supported = self.capabilities.contains(CapabilityFlags::USER_SESSIONS);
                let store_disc = self.store.clone();
                let user_disc = user_name.to_string();
                let store_manage = self.store.clone();
                let table_weak = cx.entity().downgrade();

                let mut menu = menu;
                if has_children {
                    let toggle_label = if is_expanded {
                        i18n::tr("users.menu_collapse")
                    } else {
                        i18n::tr("users.menu_expand")
                    };
                    let toggle_icon = if is_expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    };
                    menu = menu
                        .item(PopupMenuItem::new(toggle_label).icon(toggle_icon).on_click(
                            move |_, _, cx| {
                                let _ = table_weak.update(cx, |table, cx| {
                                    table.delegate_mut().trigger_toggle_session(session_id, cx);
                                });
                            },
                        ))
                        .separator();
                }

                menu.item(
                    PopupMenuItem::new(i18n::tr("users.menu_disconnect"))
                        .icon(IconName::CircleX)
                        .disabled(!supported)
                        .on_click(move |_, window, cx| {
                            open_disconnect_session_dialog(
                                &store_disc,
                                session_id,
                                user_disc.clone(),
                                window,
                                cx,
                            );
                        }),
                )
                .item(
                    PopupMenuItem::new(i18n::tr("users.menu_manage_accounts"))
                        .icon(IconName::User)
                        .on_click(move |_, _, cx| {
                            launch_manage_user_accounts(&store_manage, cx);
                        }),
                )
            }
            UsersRowProjection::Process { process, .. } => {
                let store_end = self.store.clone();
                let proc_end = process.clone();
                let shell_details = self.shell_handle.clone();
                let key = process.key;
                let store_search = self.store.clone();
                let query = process.name.to_string();

                menu.item(
                    PopupMenuItem::new(i18n::tr("users.menu_end_task"))
                        .icon(IconName::CircleX)
                        .on_click(move |_, window, cx| {
                            request_end_user_process(&store_end, &proc_end, window, cx);
                        }),
                )
                .item(
                    PopupMenuItem::new(i18n::tr("users.menu_go_to_details"))
                        .icon(IconName::ArrowRight)
                        .on_click(move |_, window, cx| {
                            shell_details.jump_to_details(key, window, cx);
                        }),
                )
                .separator()
                .item(
                    PopupMenuItem::new(i18n::tr("users.menu_search_online"))
                        .icon(IconName::Globe)
                        .on_click(move |_, _, cx| {
                            UiCommands::search_online(&store_search, query.clone(), cx);
                        }),
                )
            }
        }
    }

    fn row_label(&self, row_ix: usize, _cx: &App) -> Option<SharedString> {
        match self.slice.rows.get(row_ix)? {
            UsersRowProjection::Session {
                user_name,
                process_count,
                state,
                ..
            } => Some(
                format!(
                    "{} ({}) - {}",
                    user_name,
                    process_count,
                    self.locale.state_label(*state)
                )
                .into(),
            ),
            UsersRowProjection::Process { process, .. } => {
                Some(format!("{} (PID {})", process.display_name, process.key.pid).into())
            }
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
