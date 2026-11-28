//! Live-data Processes overview vertical feature slice.
//!
//! Splits the static toolbar shell ([`ProcessesView`]) from the high-frequency
//! 2D-virtualized `gpui-kit` `DataTable` (`Entity<TableState<ProcessesTableDelegate>>`)
//! so live snapshot ticks update only the table entity while `gpui-fast` reuses
//! the surrounding shell and toolbar as retained subtrees.

mod columns;
mod context_menu;
mod delegate;
mod state;
mod toolbar;

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
    ParentElement as _, Pixels, Render, StatefulInteractiveElement as _, Styled as _, Subscription,
    Task, Window, div, px,
};
use loom_engine::{EngineCommand, EngineCoordinator};
use loom_pal::NativeBackend;
use loom_types::{
    CapabilityFlags, LanguagePreference, MonitorDomain, PlatformProfile, ProcessRowKind,
    ProcessTableSlice, ProcessViewMode,
};

use crate::bridge::{ScopedMonitorLease, UiCommands, UiStore, UiStoreEvent};
use crate::features::table_columns::{
    DataTablePane, ListViewTelemetryKind, TableInputBridge, TableKeyAction, ToolbarActionButton,
    ToolbarActionStyle, TypeToSearchState, classify_table_key_down,
    impl_table_type_to_search_input_handler, list_telemetry, spawn_csv_export,
};
use crate::i18n;
use crate::shell::ShellHandle;

use self::columns::{
    PROCESS_HEADER_HEIGHT, ProcessColumn, ProcessesLocaleCache, find_row_index_fast,
};
use self::context_menu::trigger_end_task_for_row;
use self::delegate::ProcessesTableDelegate;
use self::state::{can_terminate_process, find_selected_row_fast};
use self::toolbar::render_processes_toolbar;

struct PendingProcessesSnapshot {
    rev: u64,
    slice: Arc<ProcessTableSlice>,
    platform: PlatformProfile,
}

/// Live Processes overview view pairing a static toolbar with a 2D-virtualized `DataTable`.
///
/// Live metric snapshots update `self.table` directly without marking `ProcessesView`
/// dirty unless toolbar-visible state (`view_mode`, `hide_system_processes`, or locale) changes,
/// allowing `gpui-fast` to splice the table entity into the retained shell/toolbar frame.
pub(crate) struct ProcessesView {
    store: Entity<UiStore>,
    coordinator: Arc<EngineCoordinator<NativeBackend>>,
    _processes_lease: ScopedMonitorLease,
    table: Entity<TableState<ProcessesTableDelegate>>,
    table_pane: Entity<DataTablePane<ProcessesTableDelegate>>,
    end_task_button: Entity<ToolbarActionButton>,
    table_left_x: Rc<Cell<Pixels>>,
    process_net_lease: Option<ScopedMonitorLease>,
    view_mode: ProcessViewMode,
    hide_system_processes: bool,
    end_task_enabled: bool,
    last_locale: LanguagePreference,
    type_to_search: TypeToSearchState,
    pending_snapshot: Option<PendingProcessesSnapshot>,
    scroll_flush_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Drop for ProcessesView {
    fn drop(&mut self) {
        let _ = self
            .coordinator
            .send_command(EngineCommand::SetProcessOrderFreeze(false));
    }
}

impl Focusable for ProcessesView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.table.focus_handle(cx)
    }
}

impl_table_type_to_search_input_handler!(ProcessesView);

impl ProcessesView {
    /// Creates a new [`ProcessesView`] backed by a 2D-virtualized `DataTable`.
    pub(crate) fn new(
        store: Entity<UiStore>,
        shell: ShellHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (
            coordinator,
            processes_lease,
            view_mode,
            hide_system_processes,
            last_locale,
            initial_query,
        ) = {
            let s = store.read(cx);
            (
                Arc::clone(s.coordinator()),
                s.acquire_lease(MonitorDomain::Processes),
                s.snapshot().processes.view_mode,
                s.settings().hide_windows_system_processes,
                s.settings().language,
                s.search_query().to_owned(),
            )
        };

        let table = cx.new(|cx| {
            let delegate = ProcessesTableDelegate::new(store.clone(), shell, cx);
            TableState::new(delegate, window, cx)
                .row_selectable(true)
                .col_selectable(false)
                .col_resizable(true)
                .col_movable(true)
                .sortable(true)
        });
        let table_pane = cx.new(|_| {
            DataTablePane::new(
                table.clone(),
                Size::Medium,
                px(PROCESS_HEADER_HEIGHT),
                false,
                i18n::tr("processes.title"),
            )
        });
        let view_weak = cx.entity().downgrade();
        let end_task_button = cx.new(|_| {
            ToolbarActionButton::new(
                "processes-end-task",
                IconName::CircleX,
                i18n::tr("processes.end_task"),
                ToolbarActionStyle::DangerWhenEnabled,
                false,
                Rc::new(move |window, cx| {
                    let _ = view_weak.update(cx, |this, cx| {
                        this.trigger_end_task_for_selected(window, cx);
                    });
                }),
            )
        });

        let mut subscriptions = Vec::with_capacity(2);

        // Subscribe to UiStore: push live snapshots straight to `self.table`; notify outer
        // `ProcessesView` ONLY when toolbar-visible state or locale changed.
        let table_for_store = table.clone();
        subscriptions.push(
            cx.subscribe(&store, move |this, store, event, cx| match event {
                UiStoreEvent::SnapshotUpdated(revs) => {
                    let latest_rev = this
                        .pending_snapshot
                        .as_ref()
                        .map_or_else(|| table_for_store.read(cx).delegate().last_rev, |p| p.rev);
                    if revs.processes == latest_rev {
                        return;
                    }
                    this.sync_process_network_lease(cx);

                    let (next_slice, next_platform) = {
                        let snap = store.read(cx).snapshot();
                        (Arc::clone(&snap.processes), snap.platform)
                    };

                    let should_defer = table_for_store.update(cx, |table, _| {
                        let d = table.delegate_mut();
                        let is_user_initiated = d.last_rev == 0
                            || d.take_user_initiated()
                            || d.slice.sort_column != next_slice.sort_column
                            || d.slice.sort_direction != next_slice.sort_direction
                            || d.slice.view_mode != next_slice.view_mode
                            || d.slice.rows.is_empty() != next_slice.rows.is_empty();
                        d.scroll_coalescer
                            .should_defer_snapshot(Instant::now(), is_user_initiated)
                    });

                    if should_defer {
                        list_telemetry().record_scroll_defer(ListViewTelemetryKind::Processes);
                        this.pending_snapshot = Some(PendingProcessesSnapshot {
                            rev: revs.processes,
                            slice: next_slice,
                            platform: next_platform,
                        });
                        this.ensure_scroll_flush_scheduled(cx);
                    } else {
                        this.pending_snapshot = None;
                        this.scroll_flush_task = None;
                        this.commit_snapshot(revs.processes, next_slice, next_platform, cx);
                    }
                }
                UiStoreEvent::SettingsChanged => {
                    let (next_locale, mem_mode, disk_mode, net_mode, hide_sys) = {
                        let s = store.read(cx).settings();
                        (
                            s.language,
                            s.memory_display_mode,
                            s.disk_display_mode,
                            s.network_display_mode,
                            s.hide_windows_system_processes,
                        )
                    };
                    let locale_changed = this.last_locale != next_locale;
                    let hide_sys_changed = this.hide_system_processes != hide_sys;
                    this.last_locale = next_locale;
                    this.hide_system_processes = hide_sys;

                    this.sync_process_network_lease(cx);

                    table_for_store.update(cx, |table, cx| {
                        let d = table.delegate_mut();
                        let mode_changed = d.mem_mode != mem_mode
                            || d.disk_mode != disk_mode
                            || d.net_mode != net_mode;
                        d.mem_mode = mem_mode;
                        d.disk_mode = disk_mode;
                        d.net_mode = net_mode;
                        if hide_sys_changed {
                            d.mark_user_initiated();
                        }
                        if locale_changed {
                            let platform = d.platform;
                            d.locale = ProcessesLocaleCache::build(platform);
                            d.section_header_cache = [None, None, None];
                            let next_tip = d.locale.sort_paused_tooltip.clone();
                            let badge = d.sort_paused_badge.clone();
                            d.invalidate_viewport_fingerprint();
                            badge.update(cx, |b, cx| b.set_tooltip(next_tip, cx));
                            table.refresh(cx);
                        } else if mode_changed {
                            d.invalidate_viewport_fingerprint();
                        }
                        if locale_changed || mode_changed {
                            cx.notify();
                        }
                    });

                    if locale_changed {
                        this.table_pane.update(cx, |pane, cx| {
                            pane.set_accessibility_label(i18n::tr("processes.title"), cx);
                        });
                        this.end_task_button.update(cx, |btn, cx| {
                            btn.set_label(i18n::tr("processes.end_task"), cx);
                        });
                    }
                    if locale_changed || hide_sys_changed {
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

        // Subscribe to DataTable events: ProcessKey selection sync and column width persistence.
        subscriptions.push(cx.subscribe(
            &table,
            |this, table, event: &TableEvent, cx| match event {
                TableEvent::SelectRow(row_ix) => {
                    let next_end_enabled = table.update(cx, |table, _| {
                        let d = table.delegate_mut();
                        if d.reanchored_selection == Some(Some(*row_ix)) {
                            d.reanchored_selection = None;
                            d.selected_row_hint = Some(*row_ix);
                            return d.slice.rows.get(*row_ix).is_some_and(can_terminate_process);
                        }
                        d.reanchored_selection = None;
                        d.invalidate_viewport_fingerprint();
                        match d.slice.rows.get(*row_ix) {
                            Some(row) if row.kind == ProcessRowKind::Process => {
                                d.selected_key = Some(row.key);
                                d.selected_row_hint = Some(*row_ix);
                                can_terminate_process(row)
                            }
                            _ => {
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
                        let (key_opt, end_enabled) = match table.delegate().slice.rows.get(*row_ix)
                        {
                            Some(row) if row.kind == ProcessRowKind::Process => {
                                (Some(row.key), can_terminate_process(row))
                            }
                            _ => (None, false),
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

        let mut view = Self {
            store,
            coordinator,
            _processes_lease: processes_lease,
            table,
            table_pane,
            end_task_button,
            table_left_x: Rc::new(Cell::new(px(0.0))),
            process_net_lease: None,
            view_mode,
            hide_system_processes,
            end_task_enabled: false,
            last_locale,
            type_to_search,
            pending_snapshot: None,
            scroll_flush_task: None,
            _subscriptions: subscriptions,
        };
        view.sync_process_network_lease(cx);
        view
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

    /// Commits a live [`ProcessTableSlice`] to `self.table`, re-anchoring selection in `O(1)`
    /// steady state and notifying `TableState` only when the visible viewport fingerprint changed.
    ///
    /// Returns `true` if `TableState` was notified to repaint.
    fn commit_snapshot(
        &mut self,
        rev: u64,
        next_slice: Arc<ProcessTableSlice>,
        next_platform: PlatformProfile,
        cx: &mut Context<Self>,
    ) -> bool {
        let commit_start = Instant::now();
        let (next_view_mode, next_end_enabled, table_notified) =
            self.table.update(cx, |table, cx| {
                let prev_selected_row = table.selected_row();
                let (sort_changed, prev_fp, had_visible_rows) = {
                    let d = table.delegate_mut();
                    let sort_changed = d.slice.sort_column != next_slice.sort_column
                        || d.slice.sort_direction != next_slice.sort_direction;
                    let had_visible = !d.visible_rows.is_empty();
                    let prev_fp = d
                        .last_viewport_fingerprint
                        .unwrap_or_else(|| d.compute_viewport_fingerprint(prev_selected_row));

                    d.row_cache.begin_epoch();
                    d.row_cache.prune_if_needed();
                    d.slice = next_slice;
                    d.platform = next_platform;
                    d.last_rev = rev;
                    (sort_changed, prev_fp, had_visible)
                };

                let (view_mode, selected_idx, end_enabled) = {
                    let d = table.delegate_mut();
                    let selected_idx = find_row_index_fast(
                        &d.slice.rows,
                        d.selected_key,
                        d.selected_row_hint.or(prev_selected_row),
                    );
                    d.selected_row_hint = selected_idx;
                    let end_enabled = selected_idx
                        .and_then(|idx| d.slice.rows.get(idx))
                        .is_some_and(can_terminate_process);
                    (d.slice.view_mode, selected_idx, end_enabled)
                };

                // Re-anchor selection index to ProcessKey without scrolling or clearing next_fp.
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

                let next_fp = {
                    let d = table.delegate_mut();
                    let fp = d.compute_viewport_fingerprint(selected_idx);
                    d.last_viewport_fingerprint = Some(fp);
                    fp
                };

                if sort_changed {
                    table.refresh(cx);
                }

                let should_notify =
                    !had_visible_rows || sort_changed || selection_changed || prev_fp != next_fp;
                if should_notify {
                    cx.notify();
                }
                (view_mode, end_enabled, should_notify)
            });

        list_telemetry().record_commit(
            ListViewTelemetryKind::Processes,
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
                    this.commit_snapshot(pending.rev, pending.slice, pending.platform, cx);
                }
            });
        }));
    }

    /// Acquires or releases `MonitorDomain::ProcessNetwork` strictly when the Network column is
    /// visible, the backend advertises `ELEVATED_PROCESS_NET`, and Loom is running elevated.
    fn sync_process_network_lease(&mut self, cx: &App) {
        let should_acquire = {
            let store_ref = self.store.read(cx);
            let snap = store_ref.snapshot();
            let network_visible = self
                .table
                .read(cx)
                .delegate()
                .column_layout
                .is_visible(ProcessColumn::Network);
            network_visible
                && snap.is_elevated
                && snap
                    .capabilities
                    .contains(CapabilityFlags::ELEVATED_PROCESS_NET)
        };
        if should_acquire {
            if self.process_net_lease.is_none() {
                self.process_net_lease = Some(
                    self.store
                        .read(cx)
                        .acquire_lease(MonitorDomain::ProcessNetwork),
                );
            }
        } else {
            self.process_net_lease = None;
        }
    }

    fn trigger_end_task_for_selected(&self, window: &mut Window, cx: &mut Context<Self>) {
        let (selected_key, selected_hint, slice) = {
            let d = self.table.read(cx).delegate();
            (d.selected_key, d.selected_row_hint, Arc::clone(&d.slice))
        };
        let Some(row) = find_selected_row_fast(&slice.rows, selected_key, selected_hint) else {
            return;
        };
        trigger_end_task_for_row(&self.store, row, window, cx);
    }

    fn toggle_selected_expanded(&self, want_expanded: bool, cx: &mut Context<Self>) -> bool {
        let target_key = {
            let d = self.table.read(cx).delegate();
            find_selected_row_fast(&d.slice.rows, d.selected_key, d.selected_row_hint)
                .filter(|row| row.has_children && row.is_expanded != want_expanded)
                .map(|row| row.key)
        };
        let Some(key) = target_key else {
            return false;
        };
        self.table.update(cx, |table, _| {
            table.delegate_mut().mark_user_initiated();
        });
        UiCommands::toggle_process_expanded(&self.store, key, cx);
        true
    }

    fn trigger_export_csv(&self, window: &mut Window, cx: &mut Context<Self>) {
        let (headers, rows) = self.table.read(cx).delegate().export_visible_csv_payload();
        spawn_csv_export(
            self.store.clone(),
            "loom-processes",
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

impl Render for ProcessesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toolbar = render_processes_toolbar(self, cx);
        let table_focus = self.table.focus_handle(cx);
        let view_entity = cx.entity();
        let table_left_x_click = Rc::clone(&self.table_left_x);
        let table_left_x_prepaint = Rc::clone(&self.table_left_x);

        v_flex()
            .id("processes-view-root")
            .size_full()
            .min_h_0()
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
            .child(toolbar)
            .child(
                div()
                    .id("processes-table-hover-zone")
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
