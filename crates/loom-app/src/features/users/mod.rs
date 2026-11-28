//! On-demand Users vertical feature slice gated by an RAII [`ScopedMonitorLease`].
//!
//! Splits the static toolbar ([`UsersView`]) from the expandable session-tree
//! 2D-virtualized `gpui-kit` `DataTable` (`Entity<TableState<UsersTableDelegate>>`)
//! so hidden routes incur zero WTS session polling or per-session aggregation overhead.
//! Sorting, filtering, and session-tree flattening are offloaded to `loom-engine`.

mod dialogs;
mod state;
mod table;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Instant;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::table::{TableEvent, TableState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, Size, StyledExt as _, h_flex,
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, Styled as _, Subscription, Task, Window, div, px,
};
use loom_types::{LanguagePreference, MonitorDomain};

use crate::bridge::{ScopedMonitorLease, UiStore, UiStoreEvent};
use crate::features::table_columns::DataTablePane;
use crate::i18n;
use crate::shell::ShellHandle;

use dialogs::{launch_manage_user_accounts, open_disconnect_session_dialog};
use state::{
    CoalesceDecision, ScrollCoalescer, USERS_HEADER_HEIGHT, UsersLocaleCache,
    compute_users_viewport_fingerprint, row_key,
};
use table::UsersTableDelegate;

/// On-demand logged-in user sessions view holding an RAII `MonitorDomain::Users` lease.
pub(crate) struct UsersView {
    store: Entity<UiStore>,
    _lease: ScopedMonitorLease,
    table: Entity<TableState<UsersTableDelegate>>,
    table_pane: Entity<DataTablePane<UsersTableDelegate>>,
    can_disconnect: bool,
    last_locale: LanguagePreference,
    scroll_coalescer: ScrollCoalescer,
    coalesce_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for UsersView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.table.focus_handle(cx)
    }
}

impl UsersView {
    /// Creates a new [`UsersView`], immediately acquiring `MonitorDomain::Users`.
    pub(crate) fn new(
        store: Entity<UiStore>,
        shell_handle: ShellHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let lease = store.read(cx).acquire_lease(MonitorDomain::Users);
        let last_locale = store.read(cx).settings().language;

        let table = cx.new(|cx| {
            let delegate = UsersTableDelegate::new(store.clone(), shell_handle, cx);
            TableState::new(delegate, window, cx)
                .row_selectable(true)
                .col_selectable(false)
                .col_resizable(true)
                .col_movable(false)
                .sortable(true)
        });
        let table_pane = cx.new(|_| {
            DataTablePane::new(
                table.clone(),
                Size::Small,
                px(USERS_HEADER_HEIGHT),
                false,
                i18n::tr("users.title"),
            )
        });
        let can_disconnect = {
            let t = table.read(cx);
            t.delegate().can_disconnect_selected(t.selected_row())
        };

        let mut subscriptions = Vec::with_capacity(2);

        let table_for_store = table.clone();
        subscriptions.push(
            cx.subscribe(&store, move |this, store, event, cx| match event {
                UiStoreEvent::SnapshotUpdated(revs) => {
                    let current_rev = table_for_store.read(cx).delegate().last_rev;
                    if revs.users == current_rev {
                        return;
                    }
                    this.handle_snapshot_updated(cx);
                }
                UiStoreEvent::SearchQueryChanged => {
                    table_for_store.update(cx, |table, _| {
                        table.delegate_mut().user_action_pending = true;
                    });
                }
                UiStoreEvent::SettingsChanged => {
                    let next_locale = store.read(cx).settings().language;
                    if this.last_locale != next_locale {
                        this.last_locale = next_locale;
                        table_for_store.update(cx, |table, cx| {
                            let d = table.delegate_mut();
                            d.locale = UsersLocaleCache::build();
                            d.last_viewport_fp = None;
                            table.refresh(cx);
                            cx.notify();
                        });
                        this.table_pane.update(cx, |pane, cx| {
                            pane.set_accessibility_label(i18n::tr("users.title"), cx);
                        });
                        cx.notify();
                    }
                }
                UiStoreEvent::ElevationRequired { .. } | UiStoreEvent::CommandFailed { .. } => {}
            }),
        );

        subscriptions.push(cx.subscribe(
            &table,
            |this, table, event: &TableEvent, cx| match event {
                TableEvent::SelectRow(row_ix) => {
                    let next_can_disconnect = table.update(cx, |table, _| {
                        let d = table.delegate_mut();
                        if d.reanchored_selection == Some(Some(*row_ix)) {
                            d.reanchored_selection = None;
                            return d.can_disconnect_selected(Some(*row_ix));
                        }
                        d.reanchored_selection = None;
                        let key = d.slice.rows.get(*row_ix).map(row_key);
                        d.selected_key = key;
                        d.last_viewport_fp = None;
                        d.can_disconnect_selected(Some(*row_ix))
                    });
                    if this.can_disconnect != next_can_disconnect {
                        this.can_disconnect = next_can_disconnect;
                        cx.notify();
                    }
                }
                TableEvent::RightClickedRow(Some(row_ix)) => {
                    let next_can_disconnect = table.update(cx, |table, cx| {
                        let key = table.delegate().slice.rows.get(*row_ix).map(row_key);
                        {
                            let d = table.delegate_mut();
                            d.selected_key = key;
                            d.reanchored_selection = None;
                            d.last_viewport_fp = None;
                        }
                        if key.is_some() && table.selected_row() != Some(*row_ix) {
                            table.set_selected_row(*row_ix, cx);
                        }
                        table
                            .delegate()
                            .can_disconnect_selected(table.selected_row())
                    });
                    if this.can_disconnect != next_can_disconnect {
                        this.can_disconnect = next_can_disconnect;
                        cx.notify();
                    }
                }
                TableEvent::ClearSelection => {
                    let (was_reanchor, next_can_disconnect) = table.update(cx, |table, _| {
                        let d = table.delegate_mut();
                        if d.reanchored_selection == Some(None) {
                            d.reanchored_selection = None;
                            return (true, d.can_disconnect_selected(None));
                        }
                        d.reanchored_selection = None;
                        d.selected_key = None;
                        d.last_viewport_fp = None;
                        (false, d.can_disconnect_selected(None))
                    });
                    if !was_reanchor && this.can_disconnect != next_can_disconnect {
                        this.can_disconnect = next_can_disconnect;
                        cx.notify();
                    }
                }
                _ => {}
            },
        ));

        Self {
            store,
            _lease: lease,
            table,
            table_pane,
            can_disconnect,
            last_locale,
            scroll_coalescer: ScrollCoalescer::default(),
            coalesce_task: None,
            _subscriptions: subscriptions,
        }
    }

    fn handle_snapshot_updated(&mut self, cx: &mut Context<Self>) {
        let (next_rev, next_slice, next_caps, next_ram) = {
            let snap = self.store.read(cx).snapshot();
            (
                snap.revisions.users,
                Arc::clone(&snap.users),
                snap.capabilities,
                snap.performance.latest.mem_total_bytes,
            )
        };

        let (bypass, last_scroll_at) = {
            let d = self.table.read(cx).delegate();
            if next_rev == d.last_rev {
                return;
            }
            let bypass = d.slice.revision == 0
                || d.user_action_pending
                || d.slice.sort_column != next_slice.sort_column
                || d.slice.sort_direction != next_slice.sort_direction;
            (bypass, d.last_scroll_at)
        };

        match self
            .scroll_coalescer
            .decide(Instant::now(), last_scroll_at, bypass)
        {
            CoalesceDecision::Defer(wait) => {
                if self.coalesce_task.is_none() {
                    self.coalesce_task = Some(cx.spawn(async move |weak, cx| {
                        cx.background_executor().timer(wait).await;
                        let _ = weak.update(cx, |this, cx| {
                            this.coalesce_task = None;
                            this.handle_snapshot_updated(cx);
                        });
                    }));
                }
            }
            CoalesceDecision::ApplyNow => {
                self.coalesce_task = None;
                let next_can_disconnect = self.table.update(cx, |table, cx| {
                    let prev_fp = table.delegate().last_viewport_fp;
                    let sort_changed = {
                        let d = table.delegate_mut();
                        let changed = d.slice.sort_column != next_slice.sort_column
                            || d.slice.sort_direction != next_slice.sort_direction;
                        d.slice = next_slice;
                        d.capabilities = next_caps;
                        d.total_ram_bytes = next_ram;
                        d.last_rev = next_rev;
                        d.user_action_pending = false;
                        d.row_text_cache.begin_epoch();
                        d.row_text_cache.prune_if_needed();
                        changed
                    };

                    Self::sync_table_selection(table, cx);

                    let selected_row = table.selected_row();
                    let next_fp = {
                        let d = table.delegate();
                        compute_users_viewport_fingerprint(
                            &d.slice,
                            d.total_ram_bytes,
                            &d.visible_rows,
                            selected_row,
                        )
                    };
                    table.delegate_mut().last_viewport_fp = Some(next_fp);

                    if sort_changed {
                        table.refresh(cx);
                    }
                    if sort_changed || prev_fp != Some(next_fp) {
                        cx.notify();
                    }
                    table.delegate().can_disconnect_selected(selected_row)
                });
                if self.can_disconnect != next_can_disconnect {
                    self.can_disconnect = next_can_disconnect;
                    cx.notify();
                }
            }
        }
    }

    fn sync_table_selection(
        table: &mut TableState<UsersTableDelegate>,
        cx: &mut Context<TableState<UsersTableDelegate>>,
    ) {
        let hint_row = table.selected_row();
        let selected_idx = table
            .delegate()
            .selected_key
            .and_then(|key| table.delegate().find_row_index(key, hint_row));
        if let Some(idx) = selected_idx {
            if hint_row != Some(idx) {
                table.delegate_mut().reanchored_selection = Some(Some(idx));
                table.set_selected_row(idx, cx);
            }
        } else if hint_row.is_some() {
            table.delegate_mut().reanchored_selection = Some(None);
            table.clear_selection(cx);
        }
    }
}

impl Render for UsersView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        debug_assert_eq!(self._lease.domain(), MonitorDomain::Users);

        let theme = cx.theme();
        let border = theme.border;
        let fg = theme.foreground;
        let can_disconnect = self.can_disconnect;
        let store_manage = self.store.clone();

        let toolbar = h_flex()
            .w_full()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_3()
            .px_4()
            .py_2p5()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .text_lg()
                    .font_semibold()
                    .text_color(fg)
                    .child(i18n::tr("users.title")),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        Button::new("users-disconnect")
                            .small()
                            .danger()
                            .outline()
                            .icon(IconName::CircleX)
                            .label(i18n::tr("users.disconnect"))
                            .disabled(!can_disconnect)
                            .on_click(cx.listener(|this, _, window, cx| {
                                let t = this.table.read(cx);
                                if let Some((session_id, user_name)) =
                                    t.delegate().selected_session(t.selected_row())
                                {
                                    open_disconnect_session_dialog(
                                        &this.store,
                                        session_id,
                                        user_name,
                                        window,
                                        cx,
                                    );
                                }
                            })),
                    )
                    .child(
                        Button::new("users-manage-accounts")
                            .small()
                            .outline()
                            .icon(IconName::User)
                            .label(i18n::tr("users.manage_accounts"))
                            .on_click(move |_, _, cx| {
                                launch_manage_user_accounts(&store_manage, cx);
                            }),
                    ),
            );

        v_flex()
            .id("users-view-root")
            .size_full()
            .min_h_0()
            .child(toolbar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.table_pane.clone()),
            )
    }
}
