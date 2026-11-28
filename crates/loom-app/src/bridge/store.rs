//! Reactive GPUI [`UiStore`] entity bridging lock-free [`AppSnapshot`] slices and events.

use std::path::Path;
use std::sync::Arc;

use crossbeam_channel::Receiver;
use gpui_kit::{Context, EventEmitter, Task};
use loom_config::{AppSettings, ConfigError, ConfigStore, TableColumnSetting};
use loom_engine::{
    CommandEvent, CommandKind, EngineCommand, EngineCommandError, EngineCoordinator,
};
use loom_pal::NativeBackend;
use loom_types::{AppSnapshot, DomainRevisions, MonitorDomain, RefreshSpeed};

use crate::bridge::ScopedMonitorLease;

/// Structured events emitted by [`UiStore`] for shell and feature subscribers.
#[derive(Debug, Clone)]
pub(crate) enum UiStoreEvent {
    /// One or more domain slices changed revision in the latest [`AppSnapshot`].
    SnapshotUpdated(DomainRevisions),
    /// User preferences in [`AppSettings`] were updated.
    SettingsChanged,
    /// The global TitleBar search query changed.
    SearchQueryChanged,
    /// An engine command failed with [`loom_pal::PalError::ElevationRequired`].
    ElevationRequired {
        /// The command that requested elevated privileges.
        command: CommandKind,
    },
    /// An engine command failed with a non-elevation error.
    CommandFailed {
        /// The command that failed.
        command: CommandKind,
        /// Human-readable error details.
        message: String,
    },
}

/// Central GPUI state entity bridging [`EngineCoordinator`] and [`ConfigStore`].
pub(crate) struct UiStore {
    coordinator: Arc<EngineCoordinator<NativeBackend>>,
    config_store: ConfigStore,
    settings: AppSettings,
    last_active_refresh_speed: RefreshSpeed,
    temp_frozen: bool,
    snapshot: Arc<AppSnapshot>,
    last_revisions: DomainRevisions,
    search_query: String,
    event_rx: Receiver<CommandEvent>,
    dormant: bool,
    wake_task: Option<Task<()>>,
}

impl EventEmitter<UiStoreEvent> for UiStore {}

impl UiStore {
    /// Creates a new [`UiStore`] bound to a live [`EngineCoordinator`] and [`ConfigStore`].
    ///
    /// Snapshot and command-event synchronization is strictly event-driven via
    /// [`EngineCoordinator::snapshot_wake_signal`] with zero timer polling.
    pub(crate) fn new(
        coordinator: Arc<EngineCoordinator<NativeBackend>>,
        config_store: ConfigStore,
        settings: AppSettings,
        cx: &mut Context<Self>,
    ) -> Self {
        let snapshot = coordinator.snapshot();
        let last_revisions = snapshot.revisions;
        let event_rx = coordinator.subscribe_events();
        let last_active_refresh_speed = match settings.refresh_speed {
            RefreshSpeed::Paused => RefreshSpeed::Normal1S,
            active => active,
        };

        let mut store = Self {
            coordinator,
            config_store,
            settings,
            last_active_refresh_speed,
            temp_frozen: false,
            snapshot,
            last_revisions,
            search_query: String::new(),
            event_rx,
            dormant: false,
            wake_task: None,
        };
        store.start_wake_loop(cx);
        store
    }

    /// Returns `true` when any domain revision counter differs between `previous` and `next`.
    #[inline]
    pub(crate) const fn revisions_changed(
        previous: DomainRevisions,
        next: DomainRevisions,
    ) -> bool {
        previous.processes != next.processes
            || previous.details != next.details
            || previous.performance != next.performance
            || previous.services != next.services
            || previous.startup != next.startup
            || previous.users != next.users
            || previous.app_history != next.app_history
    }

    /// Computes `(next_speed, next_last_active)` when toggling pause/resume refresh cadence.
    #[inline]
    pub(crate) const fn compute_pause_toggle(
        current: RefreshSpeed,
        last_active: RefreshSpeed,
    ) -> (RefreshSpeed, RefreshSpeed) {
        let fallback_active = match last_active {
            RefreshSpeed::Paused => RefreshSpeed::Normal1S,
            active => active,
        };
        match current {
            RefreshSpeed::Paused => (fallback_active, fallback_active),
            active => (RefreshSpeed::Paused, active),
        }
    }

    /// Returns the shared [`EngineCoordinator`].
    #[inline]
    pub(crate) fn coordinator(&self) -> &Arc<EngineCoordinator<NativeBackend>> {
        &self.coordinator
    }

    /// Returns the active [`AppSettings`].
    #[inline]
    pub(crate) fn settings(&self) -> &AppSettings {
        &self.settings
    }

    /// Returns `true` while temporary freeze (e.g., Hold `Ctrl`) is active.
    #[inline]
    pub(crate) const fn is_temp_frozen(&self) -> bool {
        self.temp_frozen
    }

    /// Enables or disables temporary engine freeze without mutating persisted `RefreshSpeed`.
    pub(crate) fn set_temp_frozen(&mut self, frozen: bool, cx: &mut Context<Self>) {
        if self.temp_frozen == frozen {
            return;
        }
        self.temp_frozen = frozen;
        let _ = self.dispatch_command(EngineCommand::SetTempFreeze(frozen));
        cx.notify();
    }

    /// Records the last non-paused [`RefreshSpeed`] so pause/resume restores it cleanly.
    #[inline]
    pub(crate) fn record_active_refresh_speed(&mut self, speed: RefreshSpeed) {
        if speed != RefreshSpeed::Paused {
            self.last_active_refresh_speed = speed;
        }
    }

    /// Computes the next [`RefreshSpeed`] when toggling pause/resume.
    #[inline]
    pub(crate) fn next_pause_toggle_speed(&mut self) -> RefreshSpeed {
        let (next, remembered) =
            Self::compute_pause_toggle(self.settings.refresh_speed, self.last_active_refresh_speed);
        self.last_active_refresh_speed = remembered;
        next
    }

    /// Returns the latest immutable [`AppSnapshot`].
    #[inline]
    pub(crate) fn snapshot(&self) -> &Arc<AppSnapshot> {
        &self.snapshot
    }

    /// Returns the active global TitleBar search query.
    #[inline]
    pub(crate) fn search_query(&self) -> &str {
        &self.search_query
    }

    /// Updates the global case-insensitive search query, dispatches it to the engine,
    /// and notifies UI subscribers immediately.
    pub(crate) fn set_search_query(&mut self, query: String, cx: &mut Context<Self>) {
        if self.search_query == query {
            return;
        }
        self.search_query = query.clone();
        let _ = self.dispatch_command(EngineCommand::SetSearchQuery(query));
        cx.emit(UiStoreEvent::SearchQueryChanged);
        cx.notify();
    }

    /// Returns the backing `settings.toml` path.
    #[inline]
    pub(crate) fn config_path(&self) -> &Path {
        self.config_store.path()
    }

    /// Returns the persisted column layout (order, width, visibility) for the Processes table.
    #[inline]
    pub(crate) fn processes_columns(&self) -> &[TableColumnSetting] {
        &self.settings.processes_columns
    }

    /// Returns the persisted column layout (order, width, visibility) for the Details table.
    #[inline]
    pub(crate) fn details_columns(&self) -> &[TableColumnSetting] {
        &self.settings.details_columns
    }

    /// Acquires an RAII [`ScopedMonitorLease`] for an on-demand domain.
    pub(crate) fn acquire_lease(&self, domain: MonitorDomain) -> ScopedMonitorLease {
        ScopedMonitorLease::acquire(&self.coordinator, domain)
    }

    /// Dispatches an [`EngineCommand`] non-blockingly to the coordinator.
    pub(crate) fn dispatch_command(
        &self,
        command: EngineCommand,
    ) -> Result<(), EngineCommandError> {
        let kind = command.kind();
        let res = self.coordinator.send_command(command);
        if let Err(ref err) = res {
            tracing::warn!(?kind, %err, "failed to enqueue engine command");
        }
        res
    }

    /// Dispatches an [`EngineCommand`] and returns a oneshot receiver for its [`CommandEvent`].
    pub(crate) fn dispatch_command_with_reply(
        &self,
        command: EngineCommand,
    ) -> Result<Receiver<CommandEvent>, EngineCommandError> {
        let kind = command.kind();
        let res = self.coordinator.send_command_with_reply(command);
        if let Err(ref err) = res {
            tracing::warn!(?kind, %err, "failed to enqueue engine command with reply");
        }
        res
    }

    /// Mutates [`AppSettings`], persists atomically via [`ConfigStore`], and notifies subscribers.
    pub(crate) fn update_settings(
        &mut self,
        mutate: impl FnOnce(&mut AppSettings),
        cx: &mut Context<Self>,
    ) -> Result<(), ConfigError> {
        mutate(&mut self.settings);
        let save_res = self.config_store.save(&self.settings);
        if let Err(ref err) = save_res {
            tracing::warn!(%err, path = %self.config_store.path().display(), "failed to save settings.toml");
        }
        cx.emit(UiStoreEvent::SettingsChanged);
        cx.notify();
        save_res
    }

    /// Enters or exits dormancy when desktop windows are minimized/closed to tray.
    ///
    /// While dormant, the event-driven GPUI wake task is cancelled and dropped completely
    /// so zero UI wakeups or background polling occur while idle in the tray.
    pub(crate) fn set_window_dormant(&mut self, dormant: bool, cx: &mut Context<Self>) {
        if self.dormant == dormant {
            return;
        }
        self.dormant = dormant;
        if dormant && self.temp_frozen {
            self.temp_frozen = false;
            let _ = self.dispatch_command(EngineCommand::SetTempFreeze(false));
        }
        let _ = self.dispatch_command(EngineCommand::SetWindowDormant(dormant));

        if dormant {
            self.wake_task = None;
        } else {
            self.sync_from_coordinator(cx);
            self.start_wake_loop(cx);
        }
    }

    /// Returns `true` when the store is currently in dormant tray mode.
    #[cfg(test)]
    pub(crate) const fn is_dormant(&self) -> bool {
        self.dormant
    }

    /// Returns `true` when the event-driven wake task is actively running.
    #[cfg(test)]
    pub(crate) const fn has_active_wake_task(&self) -> bool {
        self.wake_task.is_some()
    }

    /// Synchronizes the latest snapshot and drains pending command events.
    ///
    /// Subscribers are notified **only** when [`DomainRevisions`] actually changed
    /// (or when a command event arrives).
    pub(crate) fn sync_from_coordinator(&mut self, cx: &mut Context<Self>) {
        let next = self.coordinator.snapshot();
        if self.apply_snapshot_if_changed(next) {
            cx.emit(UiStoreEvent::SnapshotUpdated(self.last_revisions));
        }

        while let Ok(event) = self.event_rx.try_recv() {
            if event.is_elevation_required() {
                cx.emit(UiStoreEvent::ElevationRequired {
                    command: event.kind,
                });
            } else if let Some(err) = event.error() {
                if matches!(err, loom_pal::PalError::Cancelled) {
                    continue;
                }
                tracing::warn!(kind = ?event.kind, %err, "engine command failed");
                cx.emit(UiStoreEvent::CommandFailed {
                    command: event.kind,
                    message: err.to_string(),
                });
            }
        }
    }

    /// Applies `next` only when domain revisions (or elevation status) changed.
    fn apply_snapshot_if_changed(&mut self, next: Arc<AppSnapshot>) -> bool {
        let changed = Self::revisions_changed(self.last_revisions, next.revisions)
            || self.snapshot.is_elevated != next.is_elevated;
        if changed {
            self.last_revisions = next.revisions;
            self.snapshot = next;
        }
        changed
    }

    fn start_wake_loop(&mut self, cx: &mut Context<Self>) {
        if self.dormant || self.wake_task.is_some() {
            return;
        }
        // `EngineCoordinator` publishes snapshots from its real OS `loom-fast-sampler` thread,
        // waking the `SnapshotWakeSignal` future directly. Inform GPUI's deterministic test
        // scheduler that cross-thread wakeups are expected under `#[gpui_kit::test]`.
        #[cfg(test)]
        cx.background_executor().allow_parking();

        let signal = self.coordinator.snapshot_wake_signal();
        self.wake_task = Some(cx.spawn(async move |weak, cx| {
            while signal.wait().await {
                let keep_running = weak
                    .update(cx, |store, cx| {
                        if store.dormant {
                            return false;
                        }
                        store.sync_from_coordinator(cx);
                        true
                    })
                    .unwrap_or(false);
                if !keep_running {
                    break;
                }
            }
        }));
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use gpui_kit::{AppContext as _, TestAppContext};
    use loom_config::{AppSettings, ConfigStore};
    use loom_engine::{EngineCommand, EngineCoordinator};
    use loom_pal::{NativeBackend, PlatformBackend as _};
    use loom_types::{DomainRevisions, RefreshSpeed};

    use super::{UiStore, UiStoreEvent};

    #[test]
    fn revisions_changed_detects_every_domain_delta() {
        let base = DomainRevisions::default();
        assert!(!UiStore::revisions_changed(base, base));

        let domains = [
            DomainRevisions {
                processes: 1,
                ..base
            },
            DomainRevisions { details: 1, ..base },
            DomainRevisions {
                performance: 1,
                ..base
            },
            DomainRevisions {
                services: 1,
                ..base
            },
            DomainRevisions { startup: 1, ..base },
            DomainRevisions { users: 1, ..base },
            DomainRevisions {
                app_history: 1,
                ..base
            },
        ];

        for next in domains {
            assert!(
                UiStore::revisions_changed(base, next),
                "expected revision change for {next:?}"
            );
            assert!(!UiStore::revisions_changed(next, next));
        }
    }

    #[test]
    fn pause_toggle_remembers_and_restores_previous_active_speed() {
        // High500Ms -> Paused (remember High500Ms) -> High500Ms.
        let (paused, remembered) =
            UiStore::compute_pause_toggle(RefreshSpeed::High500Ms, RefreshSpeed::Normal1S);
        assert_eq!(paused, RefreshSpeed::Paused);
        assert_eq!(remembered, RefreshSpeed::High500Ms);

        let (resumed, remembered2) = UiStore::compute_pause_toggle(paused, remembered);
        assert_eq!(resumed, RefreshSpeed::High500Ms);
        assert_eq!(remembered2, RefreshSpeed::High500Ms);

        // Low2S -> Paused (remember Low2S) -> Low2S.
        let (paused_low, rem_low) =
            UiStore::compute_pause_toggle(RefreshSpeed::Low2S, RefreshSpeed::Normal1S);
        assert_eq!(paused_low, RefreshSpeed::Paused);
        assert_eq!(rem_low, RefreshSpeed::Low2S);
        assert_eq!(
            UiStore::compute_pause_toggle(paused_low, rem_low),
            (RefreshSpeed::Low2S, RefreshSpeed::Low2S)
        );

        // Starting from Paused with Paused fallback restores Normal1S.
        assert_eq!(
            UiStore::compute_pause_toggle(RefreshSpeed::Paused, RefreshSpeed::Paused),
            (RefreshSpeed::Normal1S, RefreshSpeed::Normal1S)
        );
    }

    #[gpui_kit::test]
    fn ui_store_receives_event_driven_snapshot_and_command_wakeups_without_polling(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let config_store = ConfigStore::new(dir.path().join("settings.toml"));
        // Start paused so only explicit ForceRefresh / command events drive wakeups.
        let settings = AppSettings {
            refresh_speed: RefreshSpeed::Paused,
            ..AppSettings::default()
        };
        let backend = NativeBackend::initialize().expect("init native backend");
        let coordinator =
            Arc::new(EngineCoordinator::new(backend, settings.clone()).expect("start coordinator"));

        let store = cx.new(|cx| UiStore::new(Arc::clone(&coordinator), config_store, settings, cx));
        assert!(store.read_with(cx, |s, _| s.has_active_wake_task()));

        let snapshot_events = Arc::new(AtomicUsize::new(0));
        let cmd_failed_events = Arc::new(AtomicUsize::new(0));
        let snap_counter = Arc::clone(&snapshot_events);
        let cmd_counter = Arc::clone(&cmd_failed_events);

        let _sub = cx.update(|cx| {
            cx.subscribe(
                &store,
                move |_store, event: &UiStoreEvent, _cx| match event {
                    UiStoreEvent::SnapshotUpdated(_) => {
                        snap_counter.fetch_add(1, Ordering::AcqRel);
                    }
                    UiStoreEvent::CommandFailed { .. } => {
                        cmd_counter.fetch_add(1, Ordering::AcqRel);
                    }
                    _ => {}
                },
            )
        });

        // Trigger a snapshot push via ForceRefresh and a command event via invalid file-lock path.
        let _ = coordinator.send_command(EngineCommand::ForceRefresh);
        let _ = coordinator.send_command(EngineCommand::FindFileLocks {
            path: PathBuf::new(),
        });

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            cx.run_until_parked();
            if snapshot_events.load(Ordering::Acquire) > 0
                && cmd_failed_events.load(Ordering::Acquire) > 0
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(
            snapshot_events.load(Ordering::Acquire) > 0,
            "UiStore must wake up from SnapshotWakeSignal when a new snapshot is published"
        );
        assert!(
            cmd_failed_events.load(Ordering::Acquire) > 0,
            "UiStore must wake up from SnapshotWakeSignal when a CommandEvent is emitted"
        );

        // Entering dormancy cancels the wake task cleanly; waking restarts it.
        store.update(cx, |s, cx| s.set_window_dormant(true, cx));
        assert!(store.read_with(cx, |s, _| s.is_dormant() && !s.has_active_wake_task()));

        store.update(cx, |s, cx| s.set_window_dormant(false, cx));
        assert!(store.read_with(cx, |s, _| !s.is_dormant() && s.has_active_wake_task()));

        coordinator.shutdown();
    }
}
