//! On-demand Services vertical feature slice gated by an RAII [`ScopedMonitorLease`].
//!
//! Splits the static toolbar + PID filter banner ([`ServicesView`]) from the
//! 2D-virtualized `gpui-kit` `DataTable` (`Entity<TableState<ServicesTableDelegate>>`)
//! so live snapshot ticks refresh only the table entity while hidden routes incur
//! zero SCM polling overhead. Filtering and sorting are offloaded completely to
//! `loom-engine`, keeping the GPUI UI thread focused on O(1) rendering.

mod state;
mod table;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Instant;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::table::{TableEvent, TableState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, Size, StyledExt as _, h_flex,
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, Styled as _, Subscription, Task, Window, div,
    prelude::FluentBuilder as _, px,
};
use loom_engine::{EngineCommand, EngineCoordinator};
use loom_pal::NativeBackend;
use loom_types::{CapabilityFlags, LanguagePreference, MonitorDomain};

use crate::bridge::{ScopedMonitorLease, UiCommands, UiStore, UiStoreEvent};
use crate::features::table_columns::DataTablePane;
use crate::i18n;
use crate::shell::ShellHandle;

use state::{
    CoalesceDecision, SERVICES_HEADER_HEIGHT, ScrollCoalescer, ServicesLocaleCache,
    ServicesToolbarState, compute_services_viewport_fingerprint,
};
use table::ServicesTableDelegate;

/// On-demand system services manager holding an RAII `MonitorDomain::Services` lease.
pub(crate) struct ServicesView {
    store: Entity<UiStore>,
    coordinator: Arc<EngineCoordinator<NativeBackend>>,
    _lease: ScopedMonitorLease,
    table: Entity<TableState<ServicesTableDelegate>>,
    table_pane: Entity<DataTablePane<ServicesTableDelegate>>,
    filter_pid: Option<u32>,
    toolbar_state: ServicesToolbarState,
    services_supported: bool,
    last_locale: LanguagePreference,
    scroll_coalescer: ScrollCoalescer,
    coalesce_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Drop for ServicesView {
    fn drop(&mut self) {
        if self.filter_pid.is_some() {
            let _ = self
                .coordinator
                .send_command(EngineCommand::SetServicesFilterPid(None));
        }
    }
}

impl Focusable for ServicesView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.table.focus_handle(cx)
    }
}

impl ServicesView {
    /// Creates a new [`ServicesView`], immediately acquiring `MonitorDomain::Services`
    /// and syncing the backend PID filter.
    pub(crate) fn new(
        store: Entity<UiStore>,
        filter_pid: Option<u32>,
        shell_handle: ShellHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (coordinator, lease, services_supported, last_locale) = {
            let s = store.read(cx);
            (
                Arc::clone(s.coordinator()),
                s.acquire_lease(MonitorDomain::Services),
                s.snapshot()
                    .capabilities
                    .contains(CapabilityFlags::SERVICES),
                s.settings().language,
            )
        };

        // Always synchronize the backend PID filter on mount (including `None`) so
        // stale filters from previous navigations never leak into a fresh view.
        UiCommands::set_services_filter_pid(&store, filter_pid, cx);

        let table = cx.new(|cx| {
            let mut delegate = ServicesTableDelegate::new(store.clone(), shell_handle, cx);
            delegate.user_action_pending = filter_pid != delegate.slice.filter_pid;
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
                px(SERVICES_HEADER_HEIGHT),
                false,
                i18n::tr("services.title"),
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
                    if revs.services == current_rev {
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
                            d.locale = ServicesLocaleCache::build();
                            d.last_viewport_fp = None;
                            table.refresh(cx);
                            cx.notify();
                        });
                        this.table_pane.update(cx, |pane, cx| {
                            pane.set_accessibility_label(i18n::tr("services.title"), cx);
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
                        let name = d.row_at(*row_ix).map(|r| Arc::clone(&r.name));
                        d.selected_name = name;
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
                        let name = table
                            .delegate()
                            .row_at(*row_ix)
                            .map(|r| Arc::clone(&r.name));
                        {
                            let d = table.delegate_mut();
                            d.selected_name = name.clone();
                            d.reanchored_selection = None;
                            d.last_viewport_fp = None;
                        }
                        if name.is_some() && table.selected_row() != Some(*row_ix) {
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
                        d.selected_name = None;
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
            coordinator,
            _lease: lease,
            table,
            table_pane,
            filter_pid,
            toolbar_state,
            services_supported,
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
                snap.revisions.services,
                Arc::clone(&snap.services),
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
                || d.slice.sort_direction != next_slice.sort_direction
                || d.slice.filter_pid != next_slice.filter_pid;
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
                        compute_services_viewport_fingerprint(
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
                let supported = next_caps.contains(CapabilityFlags::SERVICES);
                if self.toolbar_state != next_toolbar || self.services_supported != supported {
                    self.toolbar_state = next_toolbar;
                    self.services_supported = supported;
                    cx.notify();
                }
            }
        }
    }

    fn sync_table_selection(
        table: &mut TableState<ServicesTableDelegate>,
        cx: &mut Context<TableState<ServicesTableDelegate>>,
    ) {
        let hint_row = table.selected_row();
        let selected_idx = table
            .delegate()
            .selected_name
            .as_deref()
            .and_then(|name| table.delegate().find_visible_index_by_name(name, hint_row));
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

    /// Updates or clears the active host PID filter (e.g., from `NavigationIntent::JumpToServices`).
    pub(crate) fn set_filter_pid(&mut self, pid: Option<u32>, cx: &mut Context<Self>) {
        if self.filter_pid == pid {
            return;
        }
        self.filter_pid = pid;
        self.scroll_coalescer.reset();
        self.coalesce_task = None;
        self.table.update(cx, |table, _| {
            table.delegate_mut().user_action_pending = true;
        });
        UiCommands::set_services_filter_pid(&self.store, pid, cx);
        cx.notify();
    }

    fn selected_service_name(&self, cx: &App) -> Option<String> {
        let table = self.table.read(cx);
        table
            .delegate()
            .selected_record(table.selected_row())
            .map(|r| r.name.to_string())
    }
}

impl Render for ServicesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        debug_assert_eq!(self._lease.domain(), MonitorDomain::Services);

        let theme = cx.theme();
        let border = theme.border;
        let fg = theme.foreground;
        let primary = theme.primary;
        let toolbar_state = self.toolbar_state;
        let supported = self.services_supported;
        let filter_pid = self.filter_pid;
        let store_console = self.store.clone();

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
                    .child(i18n::tr("services.title")),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        Button::new("services-start")
                            .small()
                            .outline()
                            .icon(IconName::Play)
                            .label(i18n::tr("services.start"))
                            .disabled(!toolbar_state.can_start)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(name) = this.selected_service_name(cx) {
                                    UiCommands::start_service(&this.store, name, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("services-stop")
                            .small()
                            .outline()
                            .icon(IconName::Square)
                            .label(i18n::tr("services.stop"))
                            .disabled(!toolbar_state.can_stop)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(name) = this.selected_service_name(cx) {
                                    UiCommands::stop_service(&this.store, name, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("services-restart")
                            .small()
                            .outline()
                            .icon(IconName::RotateCw)
                            .label(i18n::tr("services.restart"))
                            .disabled(!toolbar_state.can_restart)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(name) = this.selected_service_name(cx) {
                                    UiCommands::restart_service(&this.store, name, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("services-open-console")
                            .small()
                            .ghost()
                            .icon(IconName::ExternalLink)
                            .label(i18n::tr("services.open_services"))
                            .disabled(!supported)
                            .on_click(move |_, _, cx| {
                                UiCommands::open_services_console(&store_console, cx);
                            }),
                    ),
            );

        let pid_banner = filter_pid.map(|pid| {
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .px_4()
                .py_1p5()
                .bg(primary.opacity(0.10))
                .border_b_1()
                .border_color(primary.opacity(0.25))
                .child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .font_medium()
                        .text_color(primary)
                        .child(Icon::new(IconName::Search).xsmall().text_color(primary))
                        .child(format!("{}: {}", i18n::tr("services.filtered_pid"), pid)),
                )
                .child(
                    Button::new("services-clear-pid-filter")
                        .xsmall()
                        .ghost()
                        .label(i18n::tr("services.clear_filter"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.set_filter_pid(None, cx);
                        })),
                )
        });

        v_flex()
            .id("services-view-root")
            .size_full()
            .min_h_0()
            .child(toolbar)
            .when_some(pid_banner, |this, banner| this.child(banner))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.table_pane.clone()),
            )
    }
}
