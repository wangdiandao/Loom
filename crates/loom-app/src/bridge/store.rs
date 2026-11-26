//! Reactive GPUI [`UiStore`] entity bridging lock-free [`AppSnapshot`] slices and events.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Receiver;
use gpui_kit::{Context, EventEmitter, Task};
use loom_config::{AppSettings, ConfigError, ConfigStore};
use loom_engine::{
    CommandEvent, CommandKind, EngineCommand, EngineCommandError, EngineCoordinator,
};
use loom_pal::NativeBackend;
use loom_types::{AppSnapshot, DomainRevisions, MonitorDomain};

use crate::bridge::ScopedMonitorLease;

/// Lightweight polling cadence used while a window is open and visible to notice
/// new lock-free [`AppSnapshot`] revisions published by `loom-fast-sampler`.
const SNAPSHOT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Structured events emitted by [`UiStore`] for shell and feature subscribers.
#[derive(Debug, Clone)]
pub(crate) enum UiStoreEvent {
    /// One or more domain slices changed revision in the latest [`AppSnapshot`].
    SnapshotUpdated(DomainRevisions),
    /// User preferences in [`AppSettings`] were updated.
    SettingsChanged,
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
    snapshot: Arc<AppSnapshot>,
    last_revisions: DomainRevisions,
    event_rx: Receiver<CommandEvent>,
    dormant: bool,
    poll_task: Option<Task<()>>,
}

impl EventEmitter<UiStoreEvent> for UiStore {}

impl UiStore {
    /// Creates a new [`UiStore`] bound to a live [`EngineCoordinator`] and [`ConfigStore`].
    pub(crate) fn new(
        coordinator: Arc<EngineCoordinator<NativeBackend>>,
        config_store: ConfigStore,
        settings: AppSettings,
        cx: &mut Context<Self>,
    ) -> Self {
        let snapshot = coordinator.snapshot();
        let last_revisions = snapshot.revisions;
        let event_rx = coordinator.subscribe_events();

        let mut store = Self {
            coordinator,
            config_store,
            settings,
            snapshot,
            last_revisions,
            event_rx,
            dormant: false,
            poll_task: None,
        };
        store.start_polling(cx);
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

    /// Returns the latest immutable [`AppSnapshot`].
    #[inline]
    pub(crate) fn snapshot(&self) -> &Arc<AppSnapshot> {
        &self.snapshot
    }

    /// Returns the backing `settings.toml` path.
    #[inline]
    pub(crate) fn config_path(&self) -> &Path {
        self.config_store.path()
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

    /// Enters or exits dormancy when the desktop window is minimized/closed to tray.
    ///
    /// While dormant, the GPUI snapshot polling loop is dropped completely so zero
    /// idle polling occurs in the UI thread.
    pub(crate) fn set_window_dormant(&mut self, dormant: bool, cx: &mut Context<Self>) {
        if self.dormant == dormant {
            return;
        }
        self.dormant = dormant;
        let _ = self.dispatch_command(EngineCommand::SetWindowDormant(dormant));

        if dormant {
            self.poll_task = None;
        } else {
            self.sync_from_coordinator(cx);
            self.start_polling(cx);
        }
    }

    /// Synchronizes the latest snapshot and drains pending command events.
    ///
    /// Subscribers are notified **only** when [`DomainRevisions`] actually changed
    /// (or when a command event arrives).
    pub(crate) fn sync_from_coordinator(&mut self, cx: &mut Context<Self>) {
        let next = self.coordinator.snapshot();
        if self.apply_snapshot_if_changed(next) {
            cx.emit(UiStoreEvent::SnapshotUpdated(self.last_revisions));
            cx.notify();
        }

        while let Ok(event) = self.event_rx.try_recv() {
            if event.is_elevation_required() {
                cx.emit(UiStoreEvent::ElevationRequired {
                    command: event.kind,
                });
            } else if let Some(err) = event.error() {
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

    fn start_polling(&mut self, cx: &mut Context<Self>) {
        if self.dormant || self.poll_task.is_some() {
            return;
        }
        self.poll_task = Some(cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor().timer(SNAPSHOT_POLL_INTERVAL).await;
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
    use loom_types::DomainRevisions;

    use super::UiStore;

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
}
