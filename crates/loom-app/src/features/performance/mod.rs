//! Real-time multi-device hardware Performance vertical feature slice.
//!
//! Holds an RAII [`ScopedMonitorLease`] for [`MonitorDomain::Performance`] while mounted,
//! and dynamically acquires/releases [`MonitorDomain::PerCoreCpu`] only while the CPU
//! logical-processors grid mode is active.

mod charts;
mod cpu;
mod disk;
mod gpu;
mod memory;
mod network;
mod state;
mod summary;

use std::sync::Arc;

use gpui_kit::component::notification::Notification;
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::{SelectEvent, SelectState};
use gpui_kit::component::{ActiveTheme as _, IndexPath, WindowExt as _, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, px,
};
use loom_types::{
    GpuAdapterSlice, LanguagePreference, MonitorDomain, NetAdapterSlice, PerformanceSlice,
    ThroughputRing,
};
use loom_ui_kit::{
    LoomSemanticColors, SparklineCardProps, format_bytes, format_freq_ghz,
    format_network_rate_mbps, format_opt_temp_celsius, sparkline_card, stepped_network_ceiling,
};

use crate::bridge::{ScopedMonitorLease, UiStore, UiStoreEvent};
use crate::i18n;

use charts::{
    HISTORY_WINDOW, history_ring_to_sparkline_points, throughput_pair_to_sparkline_points,
};
use gpu::GpuEngineOption;
use state::{CpuGraphMode, GpuViewMode, PerformanceViewState, SelectedDevice};
use summary::format_performance_summary;

/// Hardware Performance view (`CPU`, `Memory`, `Disk 0..N`, `Network 0..N`, `GPU 0..N`).
pub(crate) struct PerformanceView {
    store: Entity<UiStore>,
    _perf_lease: ScopedMonitorLease,
    per_core_lease: Option<ScopedMonitorLease>,
    state: PerformanceViewState,
    last_perf_revision: u64,
    last_locale: LanguagePreference,
    gpu_grid_selects: [Entity<SelectState<Vec<GpuEngineOption>>>; 4],
    gpu_single_select: Entity<SelectState<Vec<GpuEngineOption>>>,
    synced_gpu_luid: Option<u64>,
    synced_gpu_engines: Vec<Arc<str>>,
    synced_gpu_grid_indices: [usize; 4],
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

        let gpu_grid_selects = std::array::from_fn(|_| {
            cx.new(|cx| SelectState::new(Vec::<GpuEngineOption>::new(), None, window, cx))
        });
        let gpu_single_select =
            cx.new(|cx| SelectState::new(Vec::<GpuEngineOption>::new(), None, window, cx));

        let mut subscriptions = Vec::with_capacity(6);

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
                    this.state.reconcile_selection(&perf);
                    this.sync_gpu_selects(&perf, window, cx);
                    cx.notify();
                }
                UiStoreEvent::SettingsChanged => {
                    let next_locale = store.read(cx).settings().language;
                    if this.last_locale != next_locale {
                        this.last_locale = next_locale;
                    }
                    cx.notify();
                }
                UiStoreEvent::SearchQueryChanged
                | UiStoreEvent::ElevationRequired { .. }
                | UiStoreEvent::CommandFailed { .. } => {}
            },
        ));

        for (slot, select) in gpu_grid_selects.iter().enumerate() {
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
                                .set_gpu_grid_engine(luid, slot, *engine_index, &gpu.engines);
                            this.synced_gpu_grid_indices[slot] = *engine_index;
                            cx.notify();
                        }
                    }
                },
            ));
        }

        subscriptions.push(cx.subscribe_in(
            &gpu_single_select,
            window,
            |this, _state, event: &SelectEvent<Vec<GpuEngineOption>>, _window, cx| {
                let SelectEvent::Confirm(Some(engine_index)) = event else {
                    return;
                };
                if let SelectedDevice::Gpu(luid) = this.state.selected_device {
                    let perf = this.store.read(cx).snapshot().performance.clone();
                    if let Some(gpu) = perf.gpus.iter().find(|g| g.luid == luid) {
                        this.state
                            .set_gpu_single_engine(luid, *engine_index, &gpu.engines);
                        cx.notify();
                    }
                }
            },
        ));

        let mut view = Self {
            store,
            _perf_lease: perf_lease,
            per_core_lease: None,
            state: PerformanceViewState::default(),
            last_perf_revision: initial_perf_rev,
            last_locale: initial_locale,
            gpu_grid_selects,
            gpu_single_select,
            synced_gpu_luid: None,
            synced_gpu_engines: Vec::new(),
            synced_gpu_grid_indices: [0; 4],
            _subscriptions: subscriptions,
        };

        let initial_perf = view.store.read(cx).snapshot().performance.clone();
        // Pin open-time Disk and Network adapter ordering immediately.
        let _ = view.state.ordered_disk_indices(&initial_perf.disks);
        let _ = view.state.ordered_network_indices(&initial_perf.networks);
        view.sync_gpu_selects(&initial_perf, window, cx);
        view
    }

    /// Switches the active hardware device shown in the right detail pane.
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
        match mode {
            CpuGraphMode::LogicalProcessors => {
                if self.per_core_lease.is_none() {
                    let lease = self.store.read(cx).acquire_lease(MonitorDomain::PerCoreCpu);
                    self.per_core_lease = Some(lease);
                }
            }
            CpuGraphMode::Overall => {
                self.per_core_lease = None;
            }
        }
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

    /// Toggles GPU view between `2×2` multi-engine grid and Single-engine mode.
    pub(crate) fn set_gpu_view_mode(&mut self, mode: GpuViewMode, cx: &mut Context<Self>) {
        if self.state.gpu_view_mode == mode {
            return;
        }
        self.state.gpu_view_mode = mode;
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

    /// Synchronizes the GPU engine dropdown `SelectState`s whenever the active GPU,
    /// its engine inventory, or auto-selected active engines change.
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

        let engine_names: Vec<Arc<str>> = gpu.engines.iter().map(|e| e.name.clone()).collect();
        let grid_indices = self.state.ensure_gpu_grid_engines(luid, &gpu.engines);
        let single_index = self.state.ensure_gpu_single_engine(luid, &gpu.engines);

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

            for (slot, select) in self.gpu_grid_selects.iter().enumerate() {
                let items = options.clone();
                let chosen =
                    (!items.is_empty()).then(|| IndexPath::default().row(grid_indices[slot]));
                select.update(cx, |state, cx| {
                    state.set_items(items, window, cx);
                    state.set_selected_index(chosen, window, cx);
                });
            }

            let single_chosen =
                (!options.is_empty()).then(|| IndexPath::default().row(single_index));
            self.gpu_single_select.update(cx, |state, cx| {
                state.set_items(options, window, cx);
                state.set_selected_index(single_chosen, window, cx);
            });

            self.synced_gpu_luid = Some(luid);
            self.synced_gpu_engines = engine_names;
            self.synced_gpu_grid_indices = grid_indices;
        } else if self.synced_gpu_grid_indices != grid_indices && !gpu.engines.is_empty() {
            for (slot, select) in self.gpu_grid_selects.iter().enumerate() {
                if self.synced_gpu_grid_indices[slot] != grid_indices[slot] {
                    let chosen = Some(IndexPath::default().row(grid_indices[slot]));
                    select.update(cx, |state, cx| {
                        state.set_selected_index(chosen, window, cx);
                    });
                }
            }
            self.synced_gpu_grid_indices = grid_indices;
        }
    }

    fn render_device_sidebar(
        &mut self,
        perf: &PerformanceSlice,
        selected: SelectedDevice,
        colors: &LoomSemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let latest = &perf.latest;
        let mut list = v_flex().w_full().gap_2().p_3();

        // 1. CPU card
        let cpu_pct = format!("{:.0}%", f32::from(latest.cpu_total_permille) / 10.0);
        let cpu_subtitle: SharedString = if latest.cpu_freq_mhz > 0 {
            format_freq_ghz(latest.cpu_freq_mhz).into()
        } else if !perf.cpu_info.brand_name.trim().is_empty() {
            SharedString::from(perf.cpu_info.brand_name.as_ref().to_owned())
        } else {
            SharedString::new_static("")
        };
        list = list.child(
            sparkline_card(
                SparklineCardProps {
                    id: "perf-card-cpu".into(),
                    title: i18n::tr("perf.cpu").into(),
                    subtitle: cpu_subtitle,
                    primary_metric: cpu_pct.into(),
                    points: history_ring_to_sparkline_points(&perf.cpu_history),
                    y_max: 100.0,
                    accent_color: colors.chart_cpu,
                    selected: selected == SelectedDevice::Cpu,
                },
                colors,
            )
            .on_click(cx.listener(|view, _, window, cx| {
                view.select_device(SelectedDevice::Cpu, window, cx);
            })),
        );

        // 2. Memory card
        let mem_permille = latest.mem_util_permille();
        let mem_pct = format!("{:.0}%", f32::from(mem_permille) / 10.0);
        let mem_subtitle = format!(
            "{} / {}",
            format_bytes(latest.mem_used_bytes),
            format_bytes(latest.mem_total_bytes)
        );
        list = list.child(
            sparkline_card(
                SparklineCardProps {
                    id: "perf-card-memory".into(),
                    title: i18n::tr("perf.memory").into(),
                    subtitle: mem_subtitle.into(),
                    primary_metric: mem_pct.into(),
                    points: history_ring_to_sparkline_points(&perf.mem_history),
                    y_max: 100.0,
                    accent_color: colors.chart_memory,
                    selected: selected == SelectedDevice::Memory,
                },
                colors,
            )
            .on_click(cx.listener(|view, _, window, cx| {
                view.select_device(SelectedDevice::Memory, window, cx);
            })),
        );

        // 3. Disk 0..N cards (stable open-time ordering via merge_stable_order)
        let disk_order = self.state.ordered_disk_indices(&perf.disks);
        for disk_slice_idx in disk_order {
            let disk = &perf.disks[disk_slice_idx];
            let disk_index = disk.index;
            let title = i18n::perf_disk_title(disk.index, disk.drive_letters.as_ref());
            let active_pct = format!("{:.0}%", f32::from(disk.active_time_permille) / 10.0);
            let media = i18n::disk_media_type_label(disk.media_type);
            list = list.child(
                sparkline_card(
                    SparklineCardProps {
                        id: SharedString::from(format!("perf-card-disk-{disk_index}")),
                        title: title.into(),
                        subtitle: media.into(),
                        primary_metric: active_pct.into(),
                        points: history_ring_to_sparkline_points(&disk.active_history),
                        y_max: 100.0,
                        accent_color: colors.chart_disk,
                        selected: selected == SelectedDevice::Disk(disk_index),
                    },
                    colors,
                )
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.select_device(SelectedDevice::Disk(disk_index), window, cx);
                })),
            );
        }

        // 4. Network 0..N cards (stable open-time ordering + stepped ceiling + gap-safe max)
        let send_short = i18n::tr("perf.send_short");
        let recv_short = i18n::tr("perf.recv_short");
        let net_order = self.state.ordered_network_indices(&perf.networks);
        for net_slice_idx in net_order {
            let net = &perf.networks[net_slice_idx];
            let luid = net.luid;
            let title = format_network_card_title(net);
            let subtitle = format!(
                "{send_short}: {}  {recv_short}: {}",
                format_network_rate_mbps(net.sent_bytes_per_sec),
                format_network_rate_mbps(net.recv_bytes_per_sec)
            );
            let max_bytes = network_combined_peak_bytes(net);
            let (y_max_unit, _unit, divisor) = stepped_network_ceiling(max_bytes);
            let combined_points =
                throughput_pair_to_sparkline_points(&net.recv_history, &net.sent_history, divisor);

            list = list.child(
                sparkline_card(
                    SparklineCardProps {
                        id: SharedString::from(format!("perf-card-net-{luid}")),
                        title: title.into(),
                        subtitle: subtitle.into(),
                        primary_metric: SharedString::new_static(""),
                        points: combined_points,
                        y_max: y_max_unit as f32,
                        accent_color: colors.chart_network,
                        selected: selected == SelectedDevice::Network(luid),
                    },
                    colors,
                )
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.select_device(SelectedDevice::Network(luid), window, cx);
                })),
            );
        }

        // 5. GPU 0..N cards (honest D3-asleep readout without fake 0°C)
        for gpu in &perf.gpus {
            let luid = gpu.luid;
            let title = i18n::perf_gpu_title(gpu.index);
            let primary_metric = format_gpu_sidebar_metric(gpu);
            let subtitle: SharedString = if gpu.name.trim().is_empty() {
                i18n::tr("perf.unavailable").into()
            } else {
                SharedString::from(gpu.name.as_ref().to_owned())
            };
            list = list.child(
                sparkline_card(
                    SparklineCardProps {
                        id: SharedString::from(format!("perf-card-gpu-{luid}")),
                        title: title.into(),
                        subtitle,
                        primary_metric,
                        points: history_ring_to_sparkline_points(&gpu.util_history),
                        y_max: 100.0,
                        accent_color: colors.chart_gpu,
                        selected: selected == SelectedDevice::Gpu(luid),
                    },
                    colors,
                )
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.select_device(SelectedDevice::Gpu(luid), window, cx);
                })),
            );
        }

        div()
            .id("perf-device-sidebar")
            .size_full()
            .overflow_y_scrollbar()
            .child(list)
            .into_any_element()
    }
}

impl Render for PerformanceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (perf, sidebar_collapsed) = {
            let store = self.store.read(cx);
            (
                store.snapshot().performance.clone(),
                store.settings().sidebar_collapsed,
            )
        };
        let colors = LoomSemanticColors::from_theme(cx.theme());

        // Gracefully resolve selected device in case hardware disappeared between ticks.
        let selected = self.state.selected_device.resolve(&perf);

        let left_sidebar = self.render_device_sidebar(&perf, selected, &colors, cx);

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
            SelectedDevice::Cpu => cpu::render_cpu_detail(
                &perf,
                self.state.cpu_graph_mode,
                self.state.show_kernel_times,
                core_grid_layout,
                &colors,
                cx,
            )
            .into_any_element(),
            SelectedDevice::Memory => {
                memory::render_memory_detail(&perf, &colors, cx).into_any_element()
            }
            SelectedDevice::Disk(index) => {
                if let Some(disk_slice) = perf.disks.iter().find(|d| d.index == index) {
                    disk::render_disk_detail(disk_slice, &colors, cx).into_any_element()
                } else {
                    cpu::render_cpu_detail(
                        &perf,
                        self.state.cpu_graph_mode,
                        self.state.show_kernel_times,
                        core_grid_layout,
                        &colors,
                        cx,
                    )
                    .into_any_element()
                }
            }
            SelectedDevice::Network(luid) => {
                if let Some(net_slice) = perf.networks.iter().find(|n| n.luid == luid) {
                    network::render_network_detail(net_slice, &colors, cx).into_any_element()
                } else {
                    cpu::render_cpu_detail(
                        &perf,
                        self.state.cpu_graph_mode,
                        self.state.show_kernel_times,
                        core_grid_layout,
                        &colors,
                        cx,
                    )
                    .into_any_element()
                }
            }
            SelectedDevice::Gpu(luid) => {
                if let Some(gpu_slice) = perf.gpus.iter().find(|g| g.luid == luid) {
                    let grid_indices = self.state.ensure_gpu_grid_engines(luid, &gpu_slice.engines);
                    let single_index = self
                        .state
                        .ensure_gpu_single_engine(luid, &gpu_slice.engines);
                    gpu::render_gpu_detail(
                        gpu_slice,
                        self.state.gpu_view_mode,
                        grid_indices,
                        single_index,
                        &self.gpu_grid_selects,
                        &self.gpu_single_select,
                        &colors,
                        cx,
                    )
                    .into_any_element()
                } else {
                    cpu::render_cpu_detail(
                        &perf,
                        self.state.cpu_graph_mode,
                        self.state.show_kernel_times,
                        core_grid_layout,
                        &colors,
                        cx,
                    )
                    .into_any_element()
                }
            }
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

fn format_network_card_title(net: &NetAdapterSlice) -> String {
    let conn = i18n::net_connection_type_label(net.connection_type);
    let name = net.name.trim();
    if name.is_empty() || name.eq_ignore_ascii_case(&conn) {
        conn
    } else {
        format!("{conn} ({name})")
    }
}

/// Computes the gap-safe peak combined network throughput (recv + sent) in bytes/sec.
fn network_combined_peak_bytes(net: &NetAdapterSlice) -> u64 {
    let history_peak = net
        .recv_history
        .iter()
        .zip(net.sent_history.iter())
        .map(|(r, s)| {
            let rv = if ThroughputRing::<HISTORY_WINDOW>::is_gap(r) {
                0
            } else {
                r
            };
            let sv = if ThroughputRing::<HISTORY_WINDOW>::is_gap(s) {
                0
            } else {
                s
            };
            rv.saturating_add(sv)
        })
        .max()
        .unwrap_or(0);
    history_peak.max(
        net.recv_bytes_per_sec
            .saturating_add(net.sent_bytes_per_sec),
    )
}

/// Formats the primary metric displayed on a sidebar GPU sparkline card,
/// rendering an honest localized `"Sleeping (D3)"` label when powered down.
pub(crate) fn format_gpu_sidebar_metric(gpu: &GpuAdapterSlice) -> SharedString {
    if gpu.d3_asleep {
        return i18n::tr("perf.perf_polish_gpu_sleeping").into();
    }
    let util_pct = format!("{:.0}%", f32::from(gpu.util_permille) / 10.0);
    if gpu.temperature_celsius.is_some() {
        let temp = format_opt_temp_celsius(gpu.temperature_celsius, gpu.d3_asleep);
        format!("{util_pct} · {temp}").into()
    } else {
        util_pct.into()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui_kit::component::ActiveTheme as _;
    use gpui_kit::{AppContext as _, TestAppContext, VisualTestContext};
    use loom_config::{AppSettings, ConfigStore};
    use loom_engine::EngineCoordinator;
    use loom_pal::{NativeBackend, PlatformBackend};
    use loom_types::{
        CpuCoreKind, CpuStaticInfo, GpuAdapterSlice, GpuEngineSlice, HistoryRing,
        LanguagePreference, MonitorDomain, PerformanceSlice,
    };
    use loom_ui_kit::{LoomSemanticColors, compute_core_grid_layout};

    use crate::bridge::UiStore;
    use crate::i18n;

    use super::state::{CpuGraphMode, GpuViewMode, SelectedDevice};
    use super::{PerformanceView, cpu, format_gpu_sidebar_metric, gpu};

    #[gpui_kit::test]
    fn performance_view_acquires_and_releases_performance_and_per_core_leases(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);

        let dir = tempfile::tempdir().expect("create temp config dir");
        let config_store = ConfigStore::new(dir.path().join("settings.toml"));
        let settings = AppSettings::default();
        let backend = NativeBackend::initialize().expect("initialize native backend");
        let coordinator =
            Arc::new(EngineCoordinator::new(backend, settings.clone()).expect("start coordinator"));

        assert!(!coordinator.leases().is_active(MonitorDomain::Performance));
        assert!(!coordinator.leases().is_active(MonitorDomain::PerCoreCpu));

        let store = cx.new(|cx| UiStore::new(coordinator.clone(), config_store, settings, cx));

        let mut perf_view_handle = None;
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                let view = cx.new(|cx| PerformanceView::new(store.clone(), window, cx));
                perf_view_handle = Some(view.clone());
                cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
            })
            .expect("open performance test window")
        });
        let perf_view = perf_view_handle.expect("PerformanceView constructed");

        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.update(|window, cx| window.draw(cx).clear(cx));

        // Mount acquires Performance lease, NOT PerCoreCpu.
        assert!(coordinator.leases().is_active(MonitorDomain::Performance));
        assert!(!coordinator.leases().is_active(MonitorDomain::PerCoreCpu));

        // Toggling Logical Processors acquires PerCoreCpu lease dynamically.
        vcx.update(|_window, cx| {
            perf_view.update(cx, |view, cx| {
                view.set_cpu_graph_mode(CpuGraphMode::LogicalProcessors, cx);
            });
        });
        assert!(coordinator.leases().is_active(MonitorDomain::Performance));
        assert!(coordinator.leases().is_active(MonitorDomain::PerCoreCpu));

        // Switching back to Overall drops PerCoreCpu lease immediately while keeping Performance.
        vcx.update(|_window, cx| {
            perf_view.update(cx, |view, cx| {
                view.set_cpu_graph_mode(CpuGraphMode::Overall, cx);
            });
        });
        assert!(coordinator.leases().is_active(MonitorDomain::Performance));
        assert!(!coordinator.leases().is_active(MonitorDomain::PerCoreCpu));

        // Re-acquire PerCoreCpu, then close window & drop handles -> all leases released.
        vcx.update(|_window, cx| {
            perf_view.update(cx, |view, cx| {
                view.set_cpu_graph_mode(CpuGraphMode::LogicalProcessors, cx);
            });
        });
        assert!(coordinator.leases().is_active(MonitorDomain::PerCoreCpu));

        drop(perf_view);
        vcx.update(|window, _| window.remove_window());
        vcx.run_until_parked();

        assert!(
            !coordinator.leases().is_active(MonitorDomain::Performance),
            "Performance lease must be released when PerformanceView drops"
        );
        assert!(
            !coordinator.leases().is_active(MonitorDomain::PerCoreCpu),
            "PerCoreCpu lease must be released when PerformanceView drops"
        );

        coordinator.shutdown();
    }

    #[gpui_kit::test]
    fn performance_view_copy_summary_writes_clipboard_and_renders_hybrid_and_d3_gpu(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        i18n::apply_locale(LanguagePreference::EnUs);

        let dir = tempfile::tempdir().expect("create temp config dir");
        let config_store = ConfigStore::new(dir.path().join("settings.toml"));
        let settings = AppSettings::default();
        let backend = NativeBackend::initialize().expect("initialize native backend");
        let coordinator =
            Arc::new(EngineCoordinator::new(backend, settings.clone()).expect("start coordinator"));

        let store = cx.new(|cx| UiStore::new(coordinator.clone(), config_store, settings, cx));

        let mut perf_view_handle = None;
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                let view = cx.new(|cx| PerformanceView::new(store.clone(), window, cx));
                perf_view_handle = Some(view.clone());
                cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))
            })
            .expect("open performance test window")
        });
        let perf_view = perf_view_handle.expect("PerformanceView constructed");

        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.update(|window, cx| window.draw(cx).clear(cx));

        // Verify Copy summary copies CPU summary to clipboard cleanly.
        vcx.update(|window, cx| {
            perf_view.update(cx, |view, cx| {
                view.copy_selected_summary(window, cx);
            });
        });
        let copied_cpu = vcx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("clipboard should contain CPU summary");
        assert!(copied_cpu.contains("CPU"));
        assert!(copied_cpu.contains("Utilization:"));

        // Switch to Memory and verify Copy summary updates clipboard.
        vcx.update(|window, cx| {
            perf_view.update(cx, |view, cx| {
                view.select_device(SelectedDevice::Memory, window, cx);
                view.copy_selected_summary(window, cx);
            });
        });
        let copied_mem = vcx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("clipboard should contain Memory summary");
        assert!(copied_mem.contains("Memory"));
        assert!(copied_mem.contains("In use:"));

        // Exercise rendering synthetic hybrid P/E/LP-E logical core grid and sleeping D3 GPU pane.
        vcx.update(|_window, cx| {
            let colors = LoomSemanticColors::from_theme(cx.theme());

            let mut hybrid_perf = PerformanceSlice {
                cpu_info: CpuStaticInfo {
                    brand_name: Arc::from("Hybrid Test CPU"),
                    core_kinds: vec![
                        CpuCoreKind::Performance,
                        CpuCoreKind::Performance,
                        CpuCoreKind::Efficiency,
                        CpuCoreKind::LowPowerEfficiency,
                    ],
                    ..Default::default()
                },
                per_core_permille: vec![420, 180, 95, HistoryRing::<60>::GAP_SENTINEL],
                per_core_history: vec![
                    {
                        let mut r = HistoryRing::<60>::new();
                        r.push(420);
                        r.push_gap();
                        r.push(350);
                        r
                    },
                    HistoryRing::<60>::new(),
                    HistoryRing::<60>::new(),
                    HistoryRing::<60>::new(),
                ],
                ..Default::default()
            };
            let layout = compute_core_grid_layout(4, 900.0, 500.0, None);
            assert!(layout.columns >= 1);
            let grid_el = cpu::render_logical_cpu_grid(&hybrid_perf, layout, &colors);
            drop(grid_el);

            let sleeping_gpu = GpuAdapterSlice {
                index: 1,
                luid: 9999,
                name: Arc::from("Discrete RTX Test GPU"),
                temperature_celsius: Some(0),
                util_permille: 0,
                d3_asleep: true,
                engines: vec![GpuEngineSlice {
                    name: Arc::from("3D"),
                    util_permille: 0,
                    history: HistoryRing::<60>::new(),
                }],
                ..Default::default()
            };
            assert_eq!(
                format_gpu_sidebar_metric(&sleeping_gpu).as_ref(),
                "Sleeping (D3)"
            );
            hybrid_perf.gpus.push(sleeping_gpu);

            perf_view.update(cx, |view, cx| {
                let _gpu_el = gpu::render_gpu_detail(
                    &hybrid_perf.gpus[0],
                    GpuViewMode::Grid2x2,
                    [0, 0, 0, 0],
                    0,
                    &view.gpu_grid_selects,
                    &view.gpu_single_select,
                    &colors,
                    cx,
                );
            });
        });

        drop(perf_view);
        vcx.update(|window, _| window.remove_window());
        vcx.run_until_parked();
        coordinator.shutdown();
    }
}
