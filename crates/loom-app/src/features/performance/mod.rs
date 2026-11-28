//! Real-time multi-device hardware Performance vertical feature slice.
//!
//! Holds an RAII [`ScopedMonitorLease`] for [`MonitorDomain::Performance`] while mounted,
//! and dynamically acquires/releases [`MonitorDomain::PerCoreCpu`] strictly while the CPU
//! pane is selected in logical-processors grid mode.

mod cache;
mod charts;
mod cpu;
mod disk;
mod gpu;
mod memory;
mod network;
mod sidebar;
mod state;
mod summary;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use gpui_kit::component::notification::Notification;
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::{SelectEvent, SelectState};
use gpui_kit::component::{ActiveTheme as _, IndexPath, WindowExt as _, v_flex};
use gpui_kit::{
    AppContext as _, ClipboardItem, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, div, px,
};
use loom_types::{LanguagePreference, MonitorDomain, PerformanceSlice};
use loom_ui_kit::LoomSemanticColors;

use crate::bridge::{ScopedMonitorLease, UiStore, UiStoreEvent};
use crate::i18n;

use cache::PerformanceSeriesCache;
use gpu::{GpuEngineOption, GpuPaneCountOption};
use state::{
    CpuGraphMode, MAX_GPU_ENGINE_PANES, PerformanceViewState, SelectedDevice,
    effective_gpu_engine_count, max_gpu_engine_panes,
};
use summary::format_performance_summary;

/// Hardware Performance view (`CPU`, `Memory`, `Disk 0..N`, `Network 0..N`, `GPU 0..N`).
pub(crate) struct PerformanceView {
    store: Entity<UiStore>,
    _perf_lease: ScopedMonitorLease,
    per_core_lease: Option<ScopedMonitorLease>,
    state: PerformanceViewState,
    series_cache: PerformanceSeriesCache,
    last_perf_revision: u64,
    last_locale: LanguagePreference,
    gpu_pane_count_select: Entity<SelectState<Vec<GpuPaneCountOption>>>,
    gpu_engine_selects: [Entity<SelectState<Vec<GpuEngineOption>>>; MAX_GPU_ENGINE_PANES],
    synced_gpu_luid: Option<u64>,
    synced_gpu_engines: Vec<Arc<str>>,
    synced_gpu_engine_indices: [usize; MAX_GPU_ENGINE_PANES],
    synced_gpu_max_panes: usize,
    synced_gpu_effective_count: usize,
    synced_gpu_count_locale: Option<LanguagePreference>,
    _subscriptions: Vec<Subscription>,
}

impl PerformanceView {
    /// Creates a new [`PerformanceView`] and acquires its RAII `MonitorDomain::Performance` lease.
    pub(crate) fn new(store: Entity<UiStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (perf_lease, initial_perf_rev, initial_locale) = {
            let s = store.read(cx);
            (
                s.acquire_lease(MonitorDomain::Performance),
                s.snapshot().revisions.performance,
                s.settings().language,
            )
        };

        let gpu_pane_count_select =
            cx.new(|cx| SelectState::new(Vec::<GpuPaneCountOption>::new(), None, window, cx));
        let gpu_engine_selects = std::array::from_fn(|_| {
            cx.new(|cx| SelectState::new(Vec::<GpuEngineOption>::new(), None, window, cx))
        });

        let mut subscriptions = Vec::with_capacity(MAX_GPU_ENGINE_PANES + 2);

        // Subscribe to UiStore domain updates — re-render ONLY when `revs.performance` or settings change.
        subscriptions.push(cx.subscribe_in(
            &store,
            window,
            |this, store, event: &UiStoreEvent, window, cx| match event {
                UiStoreEvent::SnapshotUpdated(revs) => {
                    if revs.performance == this.last_perf_revision {
                        return;
                    }
                    this.last_perf_revision = revs.performance;
                    let perf = store.read(cx).snapshot().performance.clone();
                    if this.state.reconcile_selection(&perf) {
                        this.sync_per_core_lease(cx);
                    }
                    this.sync_gpu_selects(&perf, window, cx);
                    cx.notify();
                }
                UiStoreEvent::SettingsChanged => {
                    let next_locale = store.read(cx).settings().language;
                    if this.last_locale != next_locale {
                        this.last_locale = next_locale;
                        let perf = store.read(cx).snapshot().performance.clone();
                        this.sync_gpu_selects(&perf, window, cx);
                    }
                    cx.notify();
                }
                UiStoreEvent::SearchQueryChanged
                | UiStoreEvent::ElevationRequired { .. }
                | UiStoreEvent::CommandFailed { .. } => {}
            },
        ));

        subscriptions.push(cx.subscribe_in(
            &gpu_pane_count_select,
            window,
            |this, _state, event: &SelectEvent<Vec<GpuPaneCountOption>>, _window, cx| {
                let SelectEvent::Confirm(Some(count)) = event else {
                    return;
                };
                if this.state.set_gpu_engine_count(*count) {
                    this.synced_gpu_effective_count = *count;
                    cx.notify();
                }
            },
        ));

        for (slot, select) in gpu_engine_selects.iter().enumerate() {
            subscriptions.push(cx.subscribe_in(
                select,
                window,
                move |this, _state, event: &SelectEvent<Vec<GpuEngineOption>>, _window, cx| {
                    let SelectEvent::Confirm(Some(engine_index)) = event else {
                        return;
                    };
                    if let SelectedDevice::Gpu(luid) = this.state.selected_device {
                        let perf = this.store.read(cx).snapshot().performance.clone();
                        if let Some(gpu) = perf.gpus.iter().find(|g| g.luid == luid) {
                            this.state
                                .set_gpu_engine_slot(luid, slot, *engine_index, &gpu.engines);
                            this.synced_gpu_engine_indices[slot] = *engine_index;
                            cx.notify();
                        }
                    }
                },
            ));
        }

        let mut view = Self {
            store,
            _perf_lease: perf_lease,
            per_core_lease: None,
            state: PerformanceViewState::default(),
            series_cache: PerformanceSeriesCache::default(),
            last_perf_revision: initial_perf_rev,
            last_locale: initial_locale,
            gpu_pane_count_select,
            gpu_engine_selects,
            synced_gpu_luid: None,
            synced_gpu_engines: Vec::new(),
            synced_gpu_engine_indices: [0; MAX_GPU_ENGINE_PANES],
            synced_gpu_max_panes: 0,
            synced_gpu_effective_count: 0,
            synced_gpu_count_locale: None,
            _subscriptions: subscriptions,
        };

        let initial_perf = view.store.read(cx).snapshot().performance.clone();
        // Pin open-time Disk and Network adapter ordering immediately.
        let _ = view.state.ordered_disk_indices(&initial_perf.disks);
        let _ = view.state.ordered_network_indices(&initial_perf.networks);
        view.sync_gpu_selects(&initial_perf, window, cx);
        view
    }

    /// Acquires or releases [`MonitorDomain::PerCoreCpu`] strictly based on whether
    /// the CPU logical-processors grid is currently active and visible.
    fn sync_per_core_lease(&mut self, cx: &Context<Self>) {
        let needs_per_core = self.state.selected_device == SelectedDevice::Cpu
            && self.state.cpu_graph_mode == CpuGraphMode::LogicalProcessors;
        if needs_per_core {
            if self.per_core_lease.is_none() {
                let lease = self.store.read(cx).acquire_lease(MonitorDomain::PerCoreCpu);
                self.per_core_lease = Some(lease);
            }
        } else {
            self.per_core_lease = None;
        }
    }

    /// Switches the active hardware device shown in the right detail pane,
    /// releasing `MonitorDomain::PerCoreCpu` whenever navigating away from CPU.
    pub(crate) fn select_device(
        &mut self,
        device: SelectedDevice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state.selected_device == device {
            return;
        }
        self.state.selected_device = device;
        self.sync_per_core_lease(cx);
        let perf = self.store.read(cx).snapshot().performance.clone();
        self.sync_gpu_selects(&perf, window, cx);
        cx.notify();
    }

    /// Switches between Overall CPU utilization and Logical processors grid,
    /// dynamically acquiring or dropping the [`MonitorDomain::PerCoreCpu`] lease.
    pub(crate) fn set_cpu_graph_mode(&mut self, mode: CpuGraphMode, cx: &mut Context<Self>) {
        if self.state.cpu_graph_mode == mode {
            return;
        }
        self.state.cpu_graph_mode = mode;
        self.sync_per_core_lease(cx);
        cx.notify();
    }

    /// Toggles kernel-time shaded overlay on the Overall CPU chart.
    pub(crate) fn set_show_kernel_times(&mut self, show: bool, cx: &mut Context<Self>) {
        if self.state.show_kernel_times == show {
            return;
        }
        self.state.show_kernel_times = show;
        cx.notify();
    }

    /// Sets the number of visible GPU engine charts (`1..=MAX_GPU_ENGINE_PANES`)
    /// and synchronizes the count dropdown selection.
    pub(crate) fn set_gpu_engine_count(
        &mut self,
        count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.state.set_gpu_engine_count(count) {
            return;
        }
        let perf = self.store.read(cx).snapshot().performance.clone();
        self.sync_gpu_selects(&perf, window, cx);
        cx.notify();
    }

    /// Copies a plain-text summary of the currently selected hardware pane to
    /// the system clipboard and shows a confirmation [`Notification`].
    pub(crate) fn copy_selected_summary(&self, window: &mut Window, cx: &mut Context<Self>) {
        let perf = self.store.read(cx).snapshot().performance.clone();
        let selected = self.state.selected_device.resolve(&perf);
        let text = format_performance_summary(&perf, selected);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        window.push_notification(
            Notification::success(i18n::tr("perf.perf_polish_copied_summary")),
            cx,
        );
    }

    /// Synchronizes the GPU engine-count and per-pane engine dropdown `SelectState`s
    /// whenever the active GPU, its engine inventory, locale, or auto-selected active
    /// engines change.
    fn sync_gpu_selects(
        &mut self,
        perf: &PerformanceSlice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let SelectedDevice::Gpu(luid) = self.state.selected_device else {
            return;
        };
        let Some(gpu) = perf.gpus.iter().find(|g| g.luid == luid) else {
            return;
        };

        let max_panes = max_gpu_engine_panes(gpu.engines.len());
        let effective_count =
            effective_gpu_engine_count(self.state.gpu_engine_count, gpu.engines.len());
        let count_options_changed = self.synced_gpu_max_panes != max_panes
            || self.synced_gpu_count_locale != Some(self.last_locale);

        if count_options_changed {
            let count_items: Vec<GpuPaneCountOption> = (1..=max_panes)
                .map(|count| GpuPaneCountOption {
                    count,
                    label: SharedString::from(i18n::perf_gpu_engine_count_label(count)),
                })
                .collect();
            let chosen =
                (effective_count > 0).then(|| IndexPath::default().row(effective_count - 1));
            self.gpu_pane_count_select.update(cx, |state, cx| {
                state.set_items(count_items, window, cx);
                state.set_selected_index(chosen, window, cx);
            });
            self.synced_gpu_max_panes = max_panes;
            self.synced_gpu_effective_count = effective_count;
            self.synced_gpu_count_locale = Some(self.last_locale);
        } else if self.synced_gpu_effective_count != effective_count {
            let chosen =
                (effective_count > 0).then(|| IndexPath::default().row(effective_count - 1));
            self.gpu_pane_count_select.update(cx, |state, cx| {
                state.set_selected_index(chosen, window, cx);
            });
            self.synced_gpu_effective_count = effective_count;
        }

        let engine_names: Vec<Arc<str>> = gpu.engines.iter().map(|e| e.name.clone()).collect();
        let engine_indices = self.state.ensure_gpu_engine_indices(luid, &gpu.engines);

        let inventory_changed =
            self.synced_gpu_luid != Some(luid) || self.synced_gpu_engines != engine_names;

        if inventory_changed {
            let options: Vec<GpuEngineOption> = gpu
                .engines
                .iter()
                .enumerate()
                .map(|(index, eng)| GpuEngineOption {
                    index,
                    label: SharedString::from(eng.name.as_ref().to_owned()),
                })
                .collect();

            for (slot, select) in self.gpu_engine_selects.iter().enumerate() {
                let items = options.clone();
                let chosen =
                    (!items.is_empty()).then(|| IndexPath::default().row(engine_indices[slot]));
                select.update(cx, |state, cx| {
                    state.set_items(items, window, cx);
                    state.set_selected_index(chosen, window, cx);
                });
            }

            self.synced_gpu_luid = Some(luid);
            self.synced_gpu_engines = engine_names;
            self.synced_gpu_engine_indices = engine_indices;
        } else if self.synced_gpu_engine_indices != engine_indices && !gpu.engines.is_empty() {
            for (slot, select) in self.gpu_engine_selects.iter().enumerate() {
                if self.synced_gpu_engine_indices[slot] != engine_indices[slot] {
                    let chosen = Some(IndexPath::default().row(engine_indices[slot]));
                    select.update(cx, |state, cx| {
                        state.set_selected_index(chosen, window, cx);
                    });
                }
            }
            self.synced_gpu_engine_indices = engine_indices;
        }
    }
}

impl Render for PerformanceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (perf_rev, perf, sidebar_collapsed) = {
            let store = self.store.read(cx);
            let snap = store.snapshot();
            (
                snap.revisions.performance,
                snap.performance.clone(),
                store.settings().sidebar_collapsed,
            )
        };
        let colors = LoomSemanticColors::from_theme(cx.theme());

        // Refresh cached sparklines and invalidate stale detail series only when
        // `revisions.performance` has actually advanced.
        self.series_cache.sync(perf_rev, &perf);

        // Gracefully resolve selected device in case hardware disappeared between ticks.
        let selected = self.state.selected_device.resolve(&perf);

        let left_sidebar = sidebar::render_device_sidebar(
            &perf,
            selected,
            &mut self.state,
            &self.series_cache,
            &colors,
            cx,
        );

        // Compute adaptive logical-core grid layout from current viewport dimensions,
        // persisting `prev_core_grid_columns` for the 8% resize hysteresis dead-band.
        let viewport = window.viewport_size();
        let nav_width = if sidebar_collapsed { 56.0 } else { 200.0 };
        let avail_width = (viewport.width.as_f32() - nav_width - 268.0 - 40.0).max(240.0);
        let avail_height = (viewport.height.as_f32() - 380.0).clamp(220.0, 900.0);
        let core_grid_layout =
            self.state
                .compute_core_grid(perf.per_core_history.len(), avail_width, avail_height);

        let detail_content = match selected {
            SelectedDevice::Memory => {
                memory::render_memory_detail(&perf, &mut self.series_cache, &colors, cx)
                    .into_any_element()
            }
            SelectedDevice::Disk(index) if perf.disks.iter().any(|d| d.index == index) => {
                let disk_slice = perf.disks.iter().find(|d| d.index == index).unwrap();
                disk::render_disk_detail(disk_slice, &mut self.series_cache, &colors, cx)
                    .into_any_element()
            }
            SelectedDevice::Network(luid) if perf.networks.iter().any(|n| n.luid == luid) => {
                let net_slice = perf.networks.iter().find(|n| n.luid == luid).unwrap();
                network::render_network_detail(net_slice, &mut self.series_cache, &colors, cx)
                    .into_any_element()
            }
            SelectedDevice::Gpu(luid) if perf.gpus.iter().any(|g| g.luid == luid) => {
                let gpu_slice = perf.gpus.iter().find(|g| g.luid == luid).unwrap();
                let engine_indices = self
                    .state
                    .ensure_gpu_engine_indices(luid, &gpu_slice.engines);
                let max_panes = max_gpu_engine_panes(gpu_slice.engines.len());
                let effective_count = effective_gpu_engine_count(
                    self.state.gpu_engine_count,
                    gpu_slice.engines.len(),
                );
                gpu::render_gpu_detail(
                    gpu_slice,
                    effective_count,
                    max_panes,
                    engine_indices,
                    &self.gpu_pane_count_select,
                    &self.gpu_engine_selects,
                    &mut self.series_cache,
                    &colors,
                    cx,
                )
                .into_any_element()
            }
            _ => cpu::render_cpu_detail(
                &perf,
                self.state.cpu_graph_mode,
                self.state.show_kernel_times,
                core_grid_layout,
                &mut self.series_cache,
                &colors,
                cx,
            )
            .into_any_element(),
        };

        let right_detail = div()
            .id("perf-detail-scroll")
            .size_full()
            .overflow_y_scrollbar()
            .child(v_flex().w_full().p_5().child(detail_content));

        h_resizable("perf-split-layout")
            .child(
                resizable_panel()
                    .size(px(268.))
                    .size_range(px(196.)..px(400.))
                    .fixed()
                    .child(left_sidebar),
            )
            .child(resizable_panel().child(right_detail))
    }
}
