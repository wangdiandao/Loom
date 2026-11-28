//! On-demand App History vertical feature slice gated by an RAII [`ScopedMonitorLease`].
//!
//! Splits the static toolbar + collection start date indicator ([`AppHistoryView`])
//! from the 2D-virtualized `gpui-kit` `DataTable` (`Entity<TableState<AppHistoryTableDelegate>>`)
//! so hidden routes incur zero background polling or history aggregation overhead.
//! Filtering and sorting are offloaded completely to `loom-engine`.

mod state;
mod table;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Instant;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::table::{TableEvent, TableState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, Size, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, Styled as _, Subscription, Task, Window, div,
    prelude::FluentBuilder as _, px,
};
use loom_types::{CapabilityFlags, LanguagePreference, MonitorDomain};

use crate::bridge::{ScopedMonitorLease, UiCommands, UiStore, UiStoreEvent};
use crate::features::table_columns::DataTablePane;
use crate::i18n;

use state::{
    APP_HISTORY_HEADER_HEIGHT, AppHistoryLocaleCache, CoalesceDecision, ScrollCoalescer,
    compute_app_history_viewport_fingerprint, format_epoch_datetime,
};
use table::AppHistoryTableDelegate;

/// Opens a confirmation dialog before clearing cumulative application usage history.
fn open_clear_history_dialog(store: &Entity<UiStore>, window: &mut Window, cx: &mut App) {
    let store = store.clone();
    window.open_dialog(cx, move |dialog, _window, cx| {
        let store_confirm = store.clone();
        let muted_fg = cx.theme().muted_foreground;
        let warning = cx.theme().warning;
        let danger = cx.theme().danger;
        let radius = cx.theme().radius;

        dialog
            .w(px(440.))
            .title(i18n::tr("app_history.confirm_delete_title"))
            .child(
                h_flex()
                    .w_full()
                    .items_start()
                    .gap_2p5()
                    .p_3()
                    .rounded(radius)
                    .bg(warning.opacity(0.12))
                    .border_1()
                    .border_color(warning.opacity(0.35))
                    .child(
                        Icon::new(IconName::TriangleAlert)
                            .small()
                            .text_color(danger),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(muted_fg)
                            .child(i18n::tr("app_history.confirm_delete_message")),
                    ),
            )
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("app-history-clear-cancel")
                            .outline()
                            .label(i18n::tr("common.cancel"))
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("app-history-clear-ok")
                            .danger()
                            .label(i18n::tr("app_history.confirm_delete_action"))
                            .on_click(move |_, window, cx| {
                                UiCommands::clear_app_history(&store_confirm, cx);
                                window.close_dialog(cx);
                            }),
                    ),
            )
    });
}

/// On-demand cumulative App History view holding an RAII `MonitorDomain::AppHistory` lease.
pub(crate) struct AppHistoryView {
    store: Entity<UiStore>,
    _lease: ScopedMonitorLease,
    table: Entity<TableState<AppHistoryTableDelegate>>,
    table_pane: Entity<DataTablePane<AppHistoryTableDelegate>>,
    since_epoch_secs: Option<u64>,
    history_supported: bool,
    last_locale: LanguagePreference,
    scroll_coalescer: ScrollCoalescer,
    coalesce_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for AppHistoryView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.table.focus_handle(cx)
    }
}

impl AppHistoryView {
    /// Creates a new [`AppHistoryView`], immediately acquiring `MonitorDomain::AppHistory`.
    pub(crate) fn new(store: Entity<UiStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let lease = store.read(cx).acquire_lease(MonitorDomain::AppHistory);
        let (since_epoch_secs, history_supported, last_locale) = {
            let s = store.read(cx);
            let snap = s.snapshot();
            (
                snap.app_history.since_epoch_secs,
                snap.capabilities.contains(CapabilityFlags::APP_HISTORY),
                s.settings().language,
            )
        };

        let table = cx.new(|cx| {
            let delegate = AppHistoryTableDelegate::new(store.clone(), cx);
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
                px(APP_HISTORY_HEADER_HEIGHT),
                false,
                i18n::tr("app_history.title"),
            )
        });

        let mut subscriptions = Vec::with_capacity(2);

        let table_for_store = table.clone();
        subscriptions.push(
            cx.subscribe(&store, move |this, store, event, cx| match event {
                UiStoreEvent::SnapshotUpdated(revs) => {
                    let current_rev = table_for_store.read(cx).delegate().last_rev;
                    if revs.app_history == current_rev {
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
                            d.locale = AppHistoryLocaleCache::build();
                            d.last_viewport_fp = None;
                            table.refresh(cx);
                            cx.notify();
                        });
                        this.table_pane.update(cx, |pane, cx| {
                            pane.set_accessibility_label(i18n::tr("app_history.title"), cx);
                        });
                        cx.notify();
                    }
                }
                UiStoreEvent::ElevationRequired { .. } | UiStoreEvent::CommandFailed { .. } => {}
            }),
        );

        subscriptions.push(cx.subscribe(
            &table,
            |_this, table, event: &TableEvent, cx| match event {
                TableEvent::SelectRow(row_ix) => {
                    table.update(cx, |table, _| {
                        let d = table.delegate_mut();
                        if d.reanchored_selection == Some(Some(*row_ix)) {
                            d.reanchored_selection = None;
                            return;
                        }
                        d.reanchored_selection = None;
                        let id = d.row_at(*row_ix).map(|r| Arc::clone(&r.app_id));
                        d.selected_app_id = id;
                        d.last_viewport_fp = None;
                    });
                }
                TableEvent::RightClickedRow(Some(row_ix)) => {
                    table.update(cx, |table, cx| {
                        let id = table
                            .delegate()
                            .row_at(*row_ix)
                            .map(|r| Arc::clone(&r.app_id));
                        {
                            let d = table.delegate_mut();
                            d.selected_app_id = id.clone();
                            d.reanchored_selection = None;
                            d.last_viewport_fp = None;
                        }
                        if id.is_some() && table.selected_row() != Some(*row_ix) {
                            table.set_selected_row(*row_ix, cx);
                        }
                    });
                }
                TableEvent::ClearSelection => {
                    table.update(cx, |table, _| {
                        let d = table.delegate_mut();
                        if d.reanchored_selection == Some(None) {
                            d.reanchored_selection = None;
                            return;
                        }
                        d.reanchored_selection = None;
                        d.selected_app_id = None;
                        d.last_viewport_fp = None;
                    });
                }
                _ => {}
            },
        ));

        Self {
            store,
            _lease: lease,
            table,
            table_pane,
            since_epoch_secs,
            history_supported,
            last_locale,
            scroll_coalescer: ScrollCoalescer::default(),
            coalesce_task: None,
            _subscriptions: subscriptions,
        }
    }

    fn handle_snapshot_updated(&mut self, cx: &mut Context<Self>) {
        let (next_rev, next_slice, next_supported) = {
            let snap = self.store.read(cx).snapshot();
            (
                snap.revisions.app_history,
                Arc::clone(&snap.app_history),
                snap.capabilities.contains(CapabilityFlags::APP_HISTORY),
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
                let next_since = next_slice.since_epoch_secs;
                self.table.update(cx, |table, cx| {
                    let prev_fp = table.delegate().last_viewport_fp;
                    let sort_changed = {
                        let d = table.delegate_mut();
                        let changed = d.slice.sort_column != next_slice.sort_column
                            || d.slice.sort_direction != next_slice.sort_direction;
                        d.slice = next_slice;
                        d.last_rev = next_rev;
                        d.user_action_pending = false;
                        changed
                    };

                    Self::sync_table_selection(table, cx);

                    let selected_row = table.selected_row();
                    let next_fp = {
                        let d = table.delegate();
                        compute_app_history_viewport_fingerprint(
                            &d.slice,
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
                });
                if self.since_epoch_secs != next_since || self.history_supported != next_supported {
                    self.since_epoch_secs = next_since;
                    self.history_supported = next_supported;
                    cx.notify();
                }
            }
        }
    }

    fn sync_table_selection(
        table: &mut TableState<AppHistoryTableDelegate>,
        cx: &mut Context<TableState<AppHistoryTableDelegate>>,
    ) {
        let hint_row = table.selected_row();
        let selected_idx = table
            .delegate()
            .selected_app_id
            .as_deref()
            .and_then(|id| table.delegate().find_visible_index_by_id(id, hint_row));
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

impl Render for AppHistoryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        debug_assert_eq!(self._lease.domain(), MonitorDomain::AppHistory);

        let theme = cx.theme();
        let border = theme.border;
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let supported = self.history_supported;
        let since_label = self
            .since_epoch_secs
            .map(|secs| i18n::app_history_since(&format_epoch_datetime(secs)));
        let store_clear = self.store.clone();

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
                h_flex()
                    .items_center()
                    .flex_wrap()
                    .gap_3()
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .text_color(fg)
                            .child(i18n::tr("app_history.title")),
                    )
                    .when_some(since_label, |row, label| {
                        row.child(div().text_xs().text_color(muted_fg).child(label))
                    }),
            )
            .child(
                Button::new("app-history-delete")
                    .small()
                    .danger()
                    .outline()
                    .icon(IconName::Delete)
                    .label(i18n::tr("app_history.delete_history"))
                    .disabled(!supported)
                    .on_click(move |_, window, cx| {
                        open_clear_history_dialog(&store_clear, window, cx);
                    }),
            );

        v_flex()
            .id("app-history-view-root")
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
