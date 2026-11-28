//! On-demand Details vertical feature slice gated by an RAII [`ScopedMonitorLease`].
//!
//! Splits the static toolbar + highlight banner ([`DetailsView`]) from the
//! high-frequency 2D-virtualized `gpui-kit` `DataTable` (`Entity<TableState<DetailsTableDelegate>>`)
//! so live snapshot ticks update only the table entity while `gpui-fast` reuses
//! the surrounding shell and toolbar as retained subtrees.

mod affinity_dialog;
mod columns;
mod context_menu;
mod delegate;
mod state;
mod toolbar;
mod wait_chain_dialog;

#[cfg(test)]
mod tests;

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::table::{TableEvent, TableState};
use gpui_kit::component::{ElementExt as _, IconName, Size, v_flex};
use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    ParentElement as _, Pixels, Render, ScrollStrategy, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, Window, div, prelude::FluentBuilder as _, px,
};
use loom_engine::{EngineCommand, EngineCoordinator};
use loom_pal::NativeBackend;
use loom_types::{
    CapabilityFlags, DetailsTableSlice, DetailsViewMode, LanguagePreference, MonitorDomain,
    PlatformProfile, ProcessKey,
};

use crate::bridge::{ScopedMonitorLease, UiCommands, UiStore, UiStoreEvent};
use crate::features::table_columns::{
    DataTablePane, ListViewTelemetryKind, TableInputBridge, TableKeyAction, ToolbarActionButton,
    ToolbarActionStyle, TypeToSearchState, classify_table_key_down,
    impl_table_type_to_search_input_handler, list_telemetry, spawn_csv_export,
};
use crate::i18n;
use crate::shell::ShellHandle;

use self::columns::{DETAILS_HEADER_HEIGHT, DetailsLocaleCache};
use self::context_menu::request_end_task;
use self::delegate::DetailsTableDelegate;
use self::state::{can_terminate_details_row, find_row_index_by_key, find_row_index_by_key_fast};
use self::toolbar::{render_details_toolbar, render_highlight_banner};

struct PendingDetailsSnapshot {
    rev: u64,
    details: Arc<DetailsTableSlice>,
    capabilities: CapabilityFlags,
    platform: PlatformProfile,
    total_ram_bytes: u64,
}

/// Live full-field Details table view holding an RAII `MonitorDomain::Details` lease.
///
/// Splits the static toolbar/banner from the 2D-virtualized `DataTable` entity so
/// live snapshot updates refresh only `self.table` without re-rendering the outer shell.
pub(crate) struct DetailsView {
    store: Entity<UiStore>,
    coordinator: Arc<EngineCoordinator<NativeBackend>>,
    _details_lease: ScopedMonitorLease,
    table: Entity<TableState<DetailsTableDelegate>>,
    table_pane: Entity<DataTablePane<DetailsTableDelegate>>,
    end_task_button: Entity<ToolbarActionButton>,
    table_left_x: Rc<Cell<Pixels>>,
    view_mode: DetailsViewMode,
    highlighted_key: Option<ProcessKey>,
    end_task_enabled: bool,
    last_locale: LanguagePreference,
    type_to_search: TypeToSearchState,
    pending_snapshot: Option<PendingDetailsSnapshot>,
    scroll_flush_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Drop for DetailsView {
    fn drop(&mut self) {
        let _ = self
            .coordinator
            .send_command(EngineCommand::SetDetailsOrderFreeze(false));
    }
}

impl Focusable for DetailsView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.table.focus_handle(cx)
    }
}

impl_table_type_to_search_input_handler!(DetailsView);

impl DetailsView {
    /// Creates a new [`DetailsView`], acquiring `MonitorDomain::Details` immediately.
    pub(crate) fn new(
        store: Entity<UiStore>,
        highlighted_key: Option<ProcessKey>,
        shell_handle: ShellHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let lease = store.read(cx).acquire_lease(MonitorDomain::Details);
        let (coordinator, view_mode, last_locale, initial_query) = {
            let s = store.read(cx);
            (
                Arc::clone(s.coordinator()),
                s.snapshot().details.view_mode,
                s.settings().language,
                s.search_query().to_owned(),
            )
        };

        let table = cx.new(|cx| {
            let delegate =
                DetailsTableDelegate::new(store.clone(), highlighted_key, shell_handle, cx);
            let mut state = TableState::new(delegate, window, cx)
                .row_selectable(true)
                .col_selectable(false)
                .col_resizable(true)
                .col_movable(true)
                .sortable(true);
            Self::try_scroll_to_pending_in_table(&mut state, cx);
            state
        });
        let table_pane = cx.new(|_| {
            DataTablePane::new(
                table.clone(),
                Size::Small,
                px(DETAILS_HEADER_HEIGHT),
                false,
                i18n::tr("details.title"),
            )
        });
        let end_task_enabled = table.read(cx).delegate().can_end_selected();
        let view_weak = cx.entity().downgrade();
        let end_task_button = cx.new(|_| {
            ToolbarActionButton::new(
                "details-end-task",
                IconName::CircleX,
                i18n::tr("details.end_task"),
                ToolbarActionStyle::DangerOutline,
                end_task_enabled,
                Rc::new(move |window, cx| {
                    let _ = view_weak.update(cx, |this, cx| {
                        this.trigger_end_task_for_selected(window, cx);
                    });
                }),
            )
        });

        let mut subscriptions = Vec::with_capacity(2);

        // Subscribe to UiStore: route live ticks directly to `self.table`.
        let table_for_store = table.clone();
        subscriptions.push(
            cx.subscribe(&store, move |this, store, event, cx| match event {
                UiStoreEvent::SnapshotUpdated(revs) => {
                    let latest_rev = this
                        .pending_snapshot
                        .as_ref()
                        .map_or_else(|| table_for_store.read(cx).delegate().last_rev, |p| p.rev);
                    if revs.details == latest_rev {
                        return;
                    }
                    let (next_details, next_caps, next_platform, next_ram) = {
                        let snap = store.read(cx).snapshot();
                        (
                            Arc::clone(&snap.details),
                            snap.capabilities,
                            snap.platform,
                            snap.performance.latest.mem_total_bytes,
                        )
                    };

                    let should_defer = table_for_store.update(cx, |table, _| {
                        let d = table.delegate_mut();
                        let is_user_initiated = d.last_rev == 0
                            || d.details.revision == 0
                            || d.pending_scroll_key.is_some()
                            || d.take_user_initiated()
                            || d.details.sort_column != next_details.sort_column
                            || d.details.sort_direction != next_details.sort_direction
                            || d.details.view_mode != next_details.view_mode
                            || d.details.rows.is_empty() != next_details.rows.is_empty();
                        d.scroll_coalescer
                            .should_defer_snapshot(Instant::now(), is_user_initiated)
                    });

                    if should_defer {
                        list_telemetry().record_scroll_defer(ListViewTelemetryKind::Details);
                        this.pending_snapshot = Some(PendingDetailsSnapshot {
                            rev: revs.details,
                            details: next_details,
                            capabilities: next_caps,
                            platform: next_platform,
                            total_ram_bytes: next_ram,
                        });
                        this.ensure_scroll_flush_scheduled(cx);
                    } else {
                        this.pending_snapshot = None;
                        this.scroll_flush_task = None;
                        this.commit_snapshot(
                            revs.details,
                            next_details,
                            next_caps,
                            next_platform,
                            next_ram,
                            cx,
                        );
                    }
                }
                UiStoreEvent::SettingsChanged => {
                    let next_locale = store.read(cx).settings().language;
                    if this.last_locale != next_locale {
                        this.last_locale = next_locale;
                        table_for_store.update(cx, |table, cx| {
                            let platform = table.delegate().platform;
                            let (badge, next_tip) = {
                                let d = table.delegate_mut();
                                d.locale = DetailsLocaleCache::build(platform);
                                d.invalidate_viewport_fingerprint();
                                (
                                    d.sort_paused_badge.clone(),
                                    d.locale.sort_paused_tooltip.clone(),
                                )
                            };
                            badge.update(cx, |b, cx| b.set_tooltip(next_tip, cx));
                            table.refresh(cx);
                            cx.notify();
                        });
                        this.table_pane.update(cx, |pane, cx| {
                            pane.set_accessibility_label(i18n::tr("details.title"), cx);
                        });
                        this.end_task_button.update(cx, |btn, cx| {
                            btn.set_label(i18n::tr("details.end_task"), cx);
                        });
                        cx.notify();
                    }
                }
                UiStoreEvent::SearchQueryChanged => {
                    let query = store.read(cx).search_query().to_owned();
                    this.type_to_search.sync_committed(&query);
                    table_for_store.update(cx, |table, _| {
                        table.delegate_mut().mark_user_initiated();
                    });
                }
                UiStoreEvent::ElevationRequired { .. } | UiStoreEvent::CommandFailed { .. } => {}
            }),
        );

        // Subscribe to DataTable events: ProcessKey selection & column width persistence.
        subscriptions.push(cx.subscribe(
            &table,
            |this, table, event: &TableEvent, cx| match event {
                TableEvent::SelectRow(row_ix) => {
                    let next_end_enabled = table.update(cx, |table, _| {
                        let d = table.delegate_mut();
                        if d.reanchored_selection == Some(Some(*row_ix)) {
                            d.reanchored_selection = None;
                            d.selected_row_hint = Some(*row_ix);
                            return d
                                .details
                                .rows
                                .get(*row_ix)
                                .is_some_and(|r| can_terminate_details_row(r.status));
                        }
                        d.reanchored_selection = None;
                        d.invalidate_viewport_fingerprint();
                        match d.details.rows.get(*row_ix) {
                            Some(row) => {
                                d.selected_key = Some(row.key);
                                d.selected_row_hint = Some(*row_ix);
                                can_terminate_details_row(row.status)
                            }
                            None => {
                                d.selected_key = None;
                                d.selected_row_hint = None;
                                false
                            }
                        }
                    });
                    this.sync_end_task_enabled(next_end_enabled, cx);
                }
                TableEvent::RightClickedRow(Some(row_ix)) => {
                    let next_end_enabled = table.update(cx, |table, cx| {
                        let (key_opt, end_enabled) =
                            match table.delegate().details.rows.get(*row_ix) {
                                Some(row) => (Some(row.key), can_terminate_details_row(row.status)),
                                None => (None, false),
                            };
                        {
                            let d = table.delegate_mut();
                            d.selected_key = key_opt;
                            d.selected_row_hint = key_opt.map(|_| *row_ix);
                            d.reanchored_selection = None;
                            d.invalidate_viewport_fingerprint();
                        }
                        if key_opt.is_some() && table.selected_row() != Some(*row_ix) {
                            table.set_selected_row(*row_ix, cx);
                        }
                        end_enabled
                    });
                    this.sync_end_task_enabled(next_end_enabled, cx);
                }
                TableEvent::ClearSelection => {
                    let was_reanchor = table.update(cx, |table, _| {
                        let d = table.delegate_mut();
                        if d.reanchored_selection == Some(None) {
                            d.reanchored_selection = None;
                            d.selected_row_hint = None;
                            return true;
                        }
                        d.reanchored_selection = None;
                        d.selected_key = None;
                        d.selected_row_hint = None;
                        d.invalidate_viewport_fingerprint();
                        false
                    });
                    if !was_reanchor {
                        this.sync_end_task_enabled(false, cx);
                    }
                }
                TableEvent::ColumnWidthsChanged(widths) => {
                    table.update(cx, |table, cx| {
                        if table
                            .delegate_mut()
                            .column_layout
                            .apply_visible_widths(widths)
                        {
                            table.delegate_mut().invalidate_viewport_fingerprint();
                            table.delegate().persist_column_layout(cx);
                        }
                    });
                }
                _ => {}
            },
        ));

        let mut type_to_search = TypeToSearchState::default();
        type_to_search.sync_committed(&initial_query);

        Self {
            store,
            coordinator,
            _details_lease: lease,
            table,
            table_pane,
            end_task_button,
            table_left_x: Rc::new(Cell::new(px(0.0))),
            view_mode,
            highlighted_key,
            end_task_enabled,
            last_locale,
            type_to_search,
            pending_snapshot: None,
            scroll_flush_task: None,
            _subscriptions: subscriptions,
        }
    }

    #[inline]
    fn sync_end_task_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.end_task_enabled != enabled {
            self.end_task_enabled = enabled;
            self.end_task_button.update(cx, |btn, cx| {
                btn.set_enabled(enabled, cx);
            });
        }
    }

    /// Commits a live [`DetailsTableSlice`] to `self.table`, re-anchoring selection in `O(1)`
    /// steady state and notifying `TableState` only when the visible viewport fingerprint changed.
    ///
    /// Returns `true` if `TableState` was notified to repaint.
    fn commit_snapshot(
        &mut self,
        rev: u64,
        next_details: Arc<DetailsTableSlice>,
        next_caps: CapabilityFlags,
        next_platform: PlatformProfile,
        next_ram: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        let commit_start = Instant::now();
        let (next_view_mode, next_end_enabled, table_notified) =
            self.table.update(cx, |table, cx| {
                let prev_selected_row = table.selected_row();
                let (sort_changed, prev_fp, had_visible_rows, had_pending_scroll) = {
                    let d = table.delegate_mut();
                    let sort_changed = d.details.sort_column != next_details.sort_column
                        || d.details.sort_direction != next_details.sort_direction;
                    let had_visible = !d.visible_rows.is_empty();
                    let had_pending_scroll = d.pending_scroll_key.is_some();
                    let prev_fp = d
                        .last_viewport_fingerprint
                        .unwrap_or_else(|| d.compute_viewport_fingerprint(prev_selected_row));

                    d.row_cache.begin_epoch();
                    d.row_cache.prune_if_needed();
                    let platform_changed = d.platform != next_platform;
                    d.details = next_details;
                    d.capabilities = next_caps;
                    d.platform = next_platform;
                    d.total_ram_bytes = next_ram;
                    d.last_rev = rev;
                    if platform_changed {
                        d.locale = DetailsLocaleCache::build(next_platform);
                    }
                    (sort_changed, prev_fp, had_visible, had_pending_scroll)
                };

                let (view_mode, selected_idx) = {
                    let d = table.delegate_mut();
                    let selected_idx = d.selected_key.and_then(|sel| {
                        find_row_index_by_key_fast(
                            &d.details.rows,
                            sel,
                            d.selected_row_hint.or(prev_selected_row),
                        )
                    });
                    d.selected_row_hint = selected_idx;
                    (d.details.view_mode, selected_idx)
                };

                let mut selection_changed = false;
                if let Some(idx) = selected_idx {
                    if prev_selected_row != Some(idx) {
                        table.delegate_mut().reanchored_selection = Some(Some(idx));
                        table.set_selected_row(idx, cx);
                        selection_changed = true;
                    }
                } else if prev_selected_row.is_some() {
                    table.delegate_mut().reanchored_selection = Some(None);
                    table.clear_selection(cx);
                    selection_changed = true;
                }

                Self::try_scroll_to_pending_in_table(table, cx);
                let scrolled_to_pending =
                    had_pending_scroll && table.delegate().pending_scroll_key.is_none();

                let effective_selected_idx = table.selected_row();
                let next_fp = {
                    let d = table.delegate_mut();
                    let fp = d.compute_viewport_fingerprint(effective_selected_idx);
                    d.last_viewport_fingerprint = Some(fp);
                    fp
                };

                if sort_changed {
                    table.refresh(cx);
                }

                let should_notify = !had_visible_rows
                    || sort_changed
                    || selection_changed
                    || scrolled_to_pending
                    || prev_fp != next_fp;
                if should_notify {
                    cx.notify();
                }
                let end_enabled = table.delegate().can_end_selected();
                (view_mode, end_enabled, should_notify)
            });

        list_telemetry().record_commit(
            ListViewTelemetryKind::Details,
            commit_start.elapsed().as_micros() as u64,
            table_notified,
        );

        self.sync_end_task_enabled(next_end_enabled, cx);
        if self.view_mode != next_view_mode {
            self.view_mode = next_view_mode;
            cx.notify();
        }

        table_notified
    }

    fn ensure_scroll_flush_scheduled(&mut self, cx: &mut Context<Self>) {
        if self.scroll_flush_task.is_some() {
            return;
        }
        let table = self.table.clone();
        self.scroll_flush_task = Some(cx.spawn(async move |weak, cx| {
            loop {
                let delay = match cx.update(|cx| {
                    table
                        .read(cx)
                        .delegate()
                        .scroll_coalescer
                        .remaining_quiet_delay(Instant::now())
                }) {
                    Some(d) if !d.is_zero() => d,
                    Some(_) => break,
                    None => return,
                };
                cx.background_executor()
                    .timer(delay.max(Duration::from_millis(1)))
                    .await;
            }

            let _ = weak.update(cx, |this, cx| {
                this.scroll_flush_task = None;
                let had_pending = this.table.update(cx, |table, _| {
                    table.delegate_mut().scroll_coalescer.take_pending()
                });
                if had_pending && let Some(pending) = this.pending_snapshot.take() {
                    this.commit_snapshot(
                        pending.rev,
                        pending.details,
                        pending.capabilities,
                        pending.platform,
                        pending.total_ram_bytes,
                        cx,
                    );
                }
            });
        }));
    }

    fn try_scroll_to_pending_in_table(
        table: &mut TableState<DetailsTableDelegate>,
        cx: &mut Context<TableState<DetailsTableDelegate>>,
    ) {
        let Some(target) = table.delegate().pending_scroll_key else {
            return;
        };
        let found = find_row_index_by_key(&table.delegate().details.rows, target).or_else(|| {
            (target.create_time_100ns == 0)
                .then(|| {
                    table
                        .delegate()
                        .details
                        .rows
                        .iter()
                        .position(|row| row.key.pid == target.pid)
                })
                .flatten()
        });
        if let Some(idx) = found {
            let resolved_key = table.delegate().details.rows[idx].key;
            table
                .vertical_scroll_handle
                .scroll_to_item(idx, ScrollStrategy::Center);
            {
                let d = table.delegate_mut();
                d.selected_key = Some(resolved_key);
                d.selected_row_hint = Some(idx);
                if d.highlighted_key == Some(target) {
                    d.highlighted_key = Some(resolved_key);
                }
                d.reanchored_selection = Some(Some(idx));
                d.pending_scroll_key = None;
            }
            table.set_selected_row(idx, cx);
        }
    }

    /// Updates the highlighted target [`ProcessKey`] (e.g., from `NavigationIntent::JumpToDetails`).
    pub(crate) fn set_highlighted_key(&mut self, key: Option<ProcessKey>, cx: &mut Context<Self>) {
        self.highlighted_key = key;
        let next_end_enabled = self.table.update(cx, |table, cx| {
            let d = table.delegate_mut();
            d.highlighted_key = key;
            d.invalidate_viewport_fingerprint();
            if let Some(target) = key {
                d.selected_key = Some(target);
                d.pending_scroll_key = Some(target);
                d.mark_user_initiated();
                Self::try_scroll_to_pending_in_table(table, cx);
            } else {
                d.pending_scroll_key = None;
            }
            cx.notify();
            table.delegate().can_end_selected()
        });
        self.sync_end_task_enabled(next_end_enabled, cx);
        cx.notify();
    }

    fn trigger_end_task_for_selected(&self, window: &mut Window, cx: &mut Context<Self>) {
        let target = self.table.read(cx).delegate().selected_target();
        if let Some(target) = target {
            request_end_task(&self.store, &target, window, cx);
        }
    }

    fn toggle_selected_expanded(&self, want_expanded: bool, cx: &mut Context<Self>) -> bool {
        let target_key = {
            let d = self.table.read(cx).delegate();
            if d.details.view_mode != DetailsViewMode::Tree {
                return false;
            }
            d.selected_row()
                .filter(|row| row.has_children && row.is_expanded != want_expanded)
                .map(|row| row.key)
        };
        let Some(key) = target_key else {
            return false;
        };
        self.table.update(cx, |table, _| {
            table.delegate_mut().mark_user_initiated();
        });
        UiCommands::toggle_details_expanded(&self.store, key, cx);
        true
    }

    fn trigger_export_csv(&self, window: &mut Window, cx: &mut Context<Self>) {
        let (headers, rows) = self.table.read(cx).delegate().export_visible_csv_payload();
        spawn_csv_export(
            self.store.clone(),
            "loom-details",
            headers,
            rows,
            window,
            cx,
        );
    }

    fn copy_selected_short(&self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.table.read(cx).delegate().selected_copy_short() else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    fn copy_selected_summary(&self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.table.read(cx).delegate().selected_copy_summary() else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }
}

impl Render for DetailsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        debug_assert_eq!(self._details_lease.domain(), MonitorDomain::Details);

        let toolbar = render_details_toolbar(self, cx);
        let highlight_banner = self
            .highlighted_key
            .map(|key| render_highlight_banner(key, cx));
        let table_focus = self.table.focus_handle(cx);
        let view_entity = cx.entity();
        let table_left_x_click = Rc::clone(&self.table_left_x);
        let table_left_x_prepaint = Rc::clone(&self.table_left_x);

        v_flex()
            .id("details-view-root")
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let Some(action) = classify_table_key_down(event) else {
                    return;
                };
                match action {
                    TableKeyAction::CopyShort => {
                        if this.copy_selected_short(cx) {
                            cx.stop_propagation();
                        }
                    }
                    TableKeyAction::CopySummary => {
                        if this.copy_selected_summary(cx) {
                            cx.stop_propagation();
                        }
                    }
                    TableKeyAction::EndTask => {
                        if this.end_task_enabled {
                            this.trigger_end_task_for_selected(window, cx);
                            cx.stop_propagation();
                        }
                    }
                    TableKeyAction::ExpandRow => {
                        if this.toggle_selected_expanded(true, cx) {
                            cx.stop_propagation();
                        }
                    }
                    TableKeyAction::CollapseRow => {
                        if this.toggle_selected_expanded(false, cx) {
                            cx.stop_propagation();
                        }
                    }
                    TableKeyAction::BackspaceSearch => {
                        let current = this.store.read(cx).search_query().to_owned();
                        if let Some(next) = this.type_to_search.handle_backspace(&current) {
                            UiCommands::set_search_query(&this.store, next, cx);
                            cx.stop_propagation();
                        }
                    }
                    TableKeyAction::EscapeSearch => {
                        let was_composing = this.type_to_search.is_composing();
                        let current = this.store.read(cx).search_query().to_owned();
                        if let Some(next) = this.type_to_search.handle_escape(&current) {
                            UiCommands::set_search_query(&this.store, next, cx);
                            cx.stop_propagation();
                        } else if was_composing {
                            cx.notify();
                            cx.stop_propagation();
                        }
                    }
                }
            }))
            .size_full()
            .min_h_0()
            .child(toolbar)
            .when_some(highlight_banner, |this, banner| this.child(banner))
            .child(
                div()
                    .id("details-table-hover-zone")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        this.table.update(cx, |table, cx| {
                            table.delegate_mut().set_table_hovered(*hovered, cx);
                        });
                    }))
                    .capture_any_mouse_down(cx.listener(
                        move |this, event: &MouseDownEvent, _, cx| {
                            if event.button != MouseButton::Right {
                                return;
                            }
                            let local_x = f32::from(event.position.x - table_left_x_click.get());
                            this.table.update(cx, |table, _| {
                                let scroll_x = f32::from(
                                    (-table.horizontal_scroll_handle.offset().x).max(px(0.0)),
                                );
                                let col_ix = table
                                    .delegate()
                                    .column_layout
                                    .column_index_at_x(local_x, scroll_x);
                                table.delegate_mut().last_right_clicked_col = col_ix;
                            });
                        },
                    ))
                    .child(TableInputBridge::new(table_focus, view_entity))
                    .child(self.table_pane.clone())
                    .on_prepaint(move |bounds, _, _| {
                        table_left_x_prepaint.set(bounds.left());
                    }),
            )
    }
}
