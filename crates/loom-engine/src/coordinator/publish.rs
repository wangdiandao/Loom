//! Atomic domain-sliced snapshot publication, lease-edge wakeups, and cold-metadata coalescing.

use std::sync::Arc;
use std::time::Instant;

use loom_pal_api::PlatformBackend;
use loom_types::MonitorDomain;

use super::SliceUpdateMask;
use super::runner::FastLaneRunner;

impl<B: PlatformBackend> FastLaneRunner<B> {
    pub(super) fn flush_pending_cold_publish(&mut self) {
        self.cold_flush_deadline = None;
        if !self.pending_cold_publish || self.window_dormant || self.temp_frozen {
            self.pending_cold_publish = false;
            return;
        }
        self.pending_cold_publish = false;
        self.last_cold_publish_at = Some(Instant::now());
        let processes_active = self.processes_projection_active();
        let details_leased = self.leases.is_active(MonitorDomain::Details);
        let users_active = self.leases.is_active(MonitorDomain::Users)
            && (self.users_polled_once || !self.cached_user_sessions.is_empty());
        let mask = SliceUpdateMask {
            processes: processes_active,
            details: details_leased,
            users: users_active,
            ..SliceUpdateMask::default()
        };
        if mask.any() {
            self.publish_slices(mask);
        }
    }

    pub(super) fn handle_lease_wakeup(&mut self) {
        // Always reconcile ProcessNetwork ETW state immediately on lease edge (even if dormant).
        self.sync_process_network_state();

        if self.window_dormant || self.temp_frozen || self.tick_count == 0 {
            return;
        }

        let processes_leased = self.leases.is_active(MonitorDomain::Processes);
        let need_processes = processes_leased
            && (self.current_snapshot.revisions.processes == 0
                || self.last_processes_projected_tick != self.tick_count);

        let details_leased = self.leases.is_active(MonitorDomain::Details);
        let need_details = details_leased
            && (self.current_snapshot.revisions.details == 0
                || self.last_details_projected_tick != self.tick_count);

        let perf_leased = self.leases.is_active(MonitorDomain::Performance);
        let per_core_leased = self.leases.is_active(MonitorDomain::PerCoreCpu);
        let need_perf = ((perf_leased || per_core_leased)
            && self.last_perf_sampled_tick != self.tick_count)
            || (per_core_leased && self.last_per_core_sampled_tick != self.tick_count);

        if need_perf {
            self.sample_full_performance(per_core_leased);
        }

        let need_services = self.services_projection_dirty
            && self.services_polled_once
            && self.leases.is_active(MonitorDomain::Services);
        let need_startup = self.startup_projection_dirty
            && self.startup_polled_once
            && self.leases.is_active(MonitorDomain::Startup);
        let need_users = self.users_projection_dirty
            && (self.users_polled_once || !self.cached_user_sessions.is_empty())
            && self.leases.is_active(MonitorDomain::Users);
        let need_app_history = self.app_history_projection_dirty
            && self.app_history_polled_once
            && self.leases.is_active(MonitorDomain::AppHistory);

        let lease_mask = SliceUpdateMask {
            processes: need_processes,
            details: need_details,
            performance: need_perf,
            include_per_core: per_core_leased,
            services: need_services,
            startup: need_startup,
            users: need_users,
            app_history: need_app_history,
        };
        if lease_mask.any() {
            self.publish_slices(lease_mask);
        }

        self.enqueue_performance_slow_probes();
        self.enqueue_leased_slow_domains();
    }

    pub(super) fn publish_slices(&mut self, mask: SliceUpdateMask) {
        let mut any_updated = false;

        if mask.processes {
            let rev = self.current_snapshot.revisions.processes.saturating_add(1);
            self.current_snapshot.revisions.processes = rev;
            self.current_snapshot.processes =
                Arc::new(self.projection.project_processes_with_query(
                    &self.view_config,
                    &self.parsed_query,
                    &self.cold_cache,
                    rev,
                ));
            self.last_processes_projected_tick = self.tick_count;
            any_updated = true;
        }

        if mask.details {
            let rev = self.current_snapshot.revisions.details.saturating_add(1);
            self.current_snapshot.revisions.details = rev;
            self.current_snapshot.details = Arc::new(self.projection.project_details_with_query(
                &self.view_config,
                &self.parsed_query,
                &self.cold_cache,
                rev,
            ));
            self.last_details_projected_tick = self.tick_count;
            any_updated = true;
        }

        if mask.performance {
            let rev = self
                .current_snapshot
                .revisions
                .performance
                .saturating_add(1);
            self.current_snapshot.revisions.performance = rev;
            self.current_snapshot.performance = Arc::new(
                self.projection
                    .project_performance(mask.include_per_core, rev),
            );
            any_updated = true;
        }

        if mask.services
            && self.leases.is_active(MonitorDomain::Services)
            && self.services_polled_once
        {
            let rev = self.current_snapshot.revisions.services.saturating_add(1);
            self.current_snapshot.revisions.services = rev;
            self.current_snapshot.services = Arc::new(self.projection.project_services(
                &self.cached_services,
                &self.view_config,
                rev,
            ));
            self.services_projection_dirty = false;
            any_updated = true;
        }

        if mask.startup && self.leases.is_active(MonitorDomain::Startup) && self.startup_polled_once
        {
            let rev = self.current_snapshot.revisions.startup.saturating_add(1);
            self.current_snapshot.revisions.startup = rev;
            self.current_snapshot.startup = Arc::new(self.projection.project_startup(
                self.cached_startup_bios_ms,
                &self.cached_startup_entries,
                &self.view_config,
                rev,
            ));
            self.startup_projection_dirty = false;
            any_updated = true;
        }

        if mask.users
            && self.leases.is_active(MonitorDomain::Users)
            && (self.users_polled_once || !self.cached_user_sessions.is_empty())
        {
            let users_rev = self.current_snapshot.revisions.users.saturating_add(1);
            self.current_snapshot.revisions.users = users_rev;
            self.current_snapshot.users = Arc::new(self.projection.project_users(
                &self.cached_user_sessions,
                &self.view_config,
                &self.cold_cache,
                users_rev,
            ));
            self.users_projection_dirty = false;
            any_updated = true;
        }

        if mask.app_history
            && self.leases.is_active(MonitorDomain::AppHistory)
            && self.app_history_polled_once
        {
            let hist_rev = self
                .current_snapshot
                .revisions
                .app_history
                .saturating_add(1);
            self.current_snapshot.revisions.app_history = hist_rev;
            self.current_snapshot.app_history = Arc::new(self.projection.project_app_history(
                self.persisted_app_history_since,
                &self.persisted_app_history,
                &self.view_config,
                hist_rev,
            ));
            self.app_history_projection_dirty = false;
            any_updated = true;
        }

        if any_updated {
            self.snapshot_store
                .store(Arc::new(self.current_snapshot.clone()));
            self.wake_signal.notify();
        }
    }
}
