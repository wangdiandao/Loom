//! On-demand Startup Applications vertical feature slice gated by an RAII [`ScopedMonitorLease`].
//!
//! Splits the static toolbar + Last BIOS time indicator ([`StartupView`]) from the
//! 2D-virtualized `gpui-kit` `DataTable` (`Entity<TableState<StartupTableDelegate>>`)
//! so hidden routes incur zero registry/folder/UWP polling overhead. Filtering and
//! sorting are offloaded completely to `loom-engine`.

mod state;
mod table;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Instant;

use gpui_kit::component::button::Button;
use gpui_kit::component::table::{TableEvent, TableState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, Size, StyledExt as _, h_flex,
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, Styled as _, Subscription, Task, Window, div,
    prelude::FluentBuilder as _, px,
};
use loom_types::{LanguagePreference, MonitorDomain};

use crate::bridge::{ScopedMonitorLease, UiCommands, UiStore, UiStoreEvent};
use crate::features::table_columns::DataTablePane;
use crate::i18n;

use state::{
    CoalesceDecision, STARTUP_HEADER_HEIGHT, ScrollCoalescer, StartupLocaleCache,
    StartupToolbarState, compute_startup_viewport_fingerprint, format_bios_time_seconds,
};
use table::StartupTableDelegate;

/// On-demand startup applications manager holding an RAII `MonitorDomain::Startup` lease.
pub(crate) struct StartupView {
    store: Entity<UiStore>,
    _lease: ScopedMonitorLease,
    table: Entity<TableState<StartupTableDelegate>>,
    table_pane: Entity<DataTablePane<StartupTableDelegate>>,
    last_bios_time_ms: Option<u64>,
    toolbar_state: StartupToolbarState,
    last_locale: LanguagePreference,
    scroll_coalescer: ScrollCoalescer,
    coalesce_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for StartupView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.table.focus_handle(cx)
    }
}

impl StartupView {
    /// Creates a new [`StartupView`], immediately acquiring `MonitorDomain::Startup`.
    pub(crate) fn new(store: Entity<UiStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let lease = store.read(cx).acquire_lease(MonitorDomain::Startup);
        let (last_bios_time_ms, last_locale) = {
            let s = store.read(cx);
            (
                s.snapshot().startup.last_bios_time_ms,
                s.settings().language,
            )
        };

        let table = cx.new(|cx| {
            let delegate = StartupTableDelegate::new(store.clone(), cx);
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
                px(STARTUP_HEADER_HEIGHT),
                false,
                i18n::tr("startup.title"),
            )
        });
        let toolbar_state = {
            let t = table.read(cx);
            t.delegate().toolbar_state(t.selected_row())
        };

        let mut subscriptions = Vec::with_capacity(2);

        let table_for_store = table.clone();
        subscriptions.push(
            cx.subscribe(&store, move |this, store, event, cx| match event {
                UiStoreEvent::SnapshotUpdated(revs) => {
                    let current_rev = table_for_store.read(cx).delegate().last_rev;
                    if revs.startup == current_rev {
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
                            d.locale = StartupLocaleCache::build();
                            d.last_viewport_fp = None;
                            table.refresh(cx);
                            cx.notify();
                        });
                        this.table_pane.update(cx, |pane, cx| {
                            pane.set_accessibility_label(i18n::tr("startup.title"), cx);
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
                    let next_toolbar = table.update(cx, |table, _| {
                        let d = table.delegate_mut();
                        if d.reanchored_selection == Some(Some(*row_ix)) {
                            d.reanchored_selection = None;
                            return d.toolbar_state(Some(*row_ix));
                        }
                        d.reanchored_selection = None;
                        let id = d.row_at(*row_ix).map(|e| Arc::clone(&e.id));
                        d.selected_id = id;
                        d.last_viewport_fp = None;
                        d.toolbar_state(Some(*row_ix))
                    });
                    if this.toolbar_state != next_toolbar {
                        this.toolbar_state = next_toolbar;
                        cx.notify();
                    }
                }
                TableEvent::RightClickedRow(Some(row_ix)) => {
                    let next_toolbar = table.update(cx, |table, cx| {
                        let id = table.delegate().row_at(*row_ix).map(|e| Arc::clone(&e.id));
                        {
                            let d = table.delegate_mut();
                            d.selected_id = id.clone();
                            d.reanchored_selection = None;
                            d.last_viewport_fp = None;
                        }
                        if id.is_some() && table.selected_row() != Some(*row_ix) {
                            table.set_selected_row(*row_ix, cx);
                        }
                        table.delegate().toolbar_state(table.selected_row())
                    });
                    if this.toolbar_state != next_toolbar {
                        this.toolbar_state = next_toolbar;
                        cx.notify();
                    }
                }
                TableEvent::ClearSelection => {
                    let (was_reanchor, next_toolbar) = table.update(cx, |table, _| {
                        let d = table.delegate_mut();
                        if d.reanchored_selection == Some(None) {
                            d.reanchored_selection = None;
                            return (true, d.toolbar_state(None));
                        }
                        d.reanchored_selection = None;
                        d.selected_id = None;
                        d.last_viewport_fp = None;
                        (false, d.toolbar_state(None))
                    });
                    if !was_reanchor && this.toolbar_state != next_toolbar {
                        this.toolbar_state = next_toolbar;
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
            last_bios_time_ms,
            toolbar_state,
            last_locale,
            scroll_coalescer: ScrollCoalescer::default(),
            coalesce_task: None,
            _subscriptions: subscriptions,
        }
    }

    fn handle_snapshot_updated(&mut self, cx: &mut Context<Self>) {
        let (next_rev, next_slice, next_caps) = {
            let snap = self.store.read(cx).snapshot();
            (
                snap.revisions.startup,
                Arc::clone(&snap.startup),
                snap.capabilities,
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
                let next_bios = next_slice.last_bios_time_ms;
                let next_toolbar = self.table.update(cx, |table, cx| {
                    let prev_fp = table.delegate().last_viewport_fp;
                    let sort_changed = {
                        let d = table.delegate_mut();
                        let changed = d.slice.sort_column != next_slice.sort_column
                            || d.slice.sort_direction != next_slice.sort_direction;
                        d.slice = next_slice;
                        d.capabilities = next_caps;
                        d.last_rev = next_rev;
                        d.user_action_pending = false;
                        changed
                    };

                    Self::sync_table_selection(table, cx);

                    let selected_row = table.selected_row();
                    let next_fp = {
                        let d = table.delegate();
                        compute_startup_viewport_fingerprint(
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
                    table.delegate().toolbar_state(selected_row)
                });
                if self.toolbar_state != next_toolbar || self.last_bios_time_ms != next_bios {
                    self.toolbar_state = next_toolbar;
                    self.last_bios_time_ms = next_bios;
                    cx.notify();
                }
            }
        }
    }

    fn sync_table_selection(
        table: &mut TableState<StartupTableDelegate>,
        cx: &mut Context<TableState<StartupTableDelegate>>,
    ) {
        let hint_row = table.selected_row();
        let selected_idx = table
            .delegate()
            .selected_id
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

    fn selected_entry_id(&self, cx: &App) -> Option<String> {
        let table = self.table.read(cx);
        table
            .delegate()
            .selected_entry(table.selected_row())
            .map(|e| e.id.to_string())
    }
}

impl Render for StartupView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        debug_assert_eq!(self._lease.domain(), MonitorDomain::Startup);

        let theme = cx.theme();
        let border = theme.border;
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let toolbar_state = self.toolbar_state;
        let bios_label = self
            .last_bios_time_ms
            .map(|ms| i18n::startup_last_bios_time(&format_bios_time_seconds(ms)));

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
                    .child(i18n::tr("startup.title")),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2p5()
                    .flex_shrink_0()
                    .when_some(bios_label, |row, label| {
                        row.child(div().text_xs().text_color(muted_fg).child(label))
                    })
                    .child(
                        Button::new("startup-enable")
                            .small()
                            .outline()
                            .icon(IconName::CircleCheck)
                            .label(i18n::tr("startup.enable"))
                            .disabled(!toolbar_state.can_enable)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(id) = this.selected_entry_id(cx) {
                                    UiCommands::set_startup_enabled(&this.store, id, true, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("startup-disable")
                            .small()
                            .outline()
                            .icon(IconName::Ban)
                            .label(i18n::tr("startup.disable"))
                            .disabled(!toolbar_state.can_disable)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(id) = this.selected_entry_id(cx) {
                                    UiCommands::set_startup_enabled(&this.store, id, false, cx);
                                }
                            })),
                    ),
            );

        v_flex()
            .id("startup-view-root")
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
