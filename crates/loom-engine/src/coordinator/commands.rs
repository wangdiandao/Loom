//! Incoming [`EngineCommand`] batching, view-state mutations, and fast/slow dispatch.

use std::time::Instant;

use crossbeam_channel::Sender;
use loom_pal_api::{PalError, PlatformBackend, SystemOperator};
use loom_types::{MonitorDomain, ProcessKey, RefreshSpeed};

use super::runner::FastLaneRunner;
use super::{SliceUpdateMask, push_ack, send_ack};
use crate::command::{
    CommandEnvelope, CommandEvent, CommandKind, CommandOutcome, CommandSuccess, EngineCommand,
};
use crate::query::ParsedQuery;
use crate::slow_lane::{SlowMgmtOp, SlowPriorityJob, SlowShellOp};
use crate::tree::collect_bottom_up_kill_order;

impl<B: PlatformBackend> FastLaneRunner<B> {
    /// Processes the received command plus any additional queued commands in one batch,
    /// coalescing repeated view-state changes into a single re-projection.
    ///
    /// Returns `true` if `EngineCommand::Shutdown` was received.
    pub(super) fn handle_command_batch(&mut self, first: CommandEnvelope) -> bool {
        let mut dirty = SliceUpdateMask::default();
        let mut need_sample_tick = false;
        let mut pending_acks: Vec<(CommandKind, Sender<CommandEvent>)> = Vec::new();
        let mut pending_events: Vec<(CommandEvent, Option<Sender<CommandEvent>>)> = Vec::new();
        let mut shutdown_requested = false;

        if self.apply_single_command(
            first,
            &mut dirty,
            &mut need_sample_tick,
            &mut pending_acks,
            &mut pending_events,
        ) {
            shutdown_requested = true;
        } else {
            while let Ok(next) = self.cmd_rx.try_recv() {
                if self.apply_single_command(
                    next,
                    &mut dirty,
                    &mut need_sample_tick,
                    &mut pending_acks,
                    &mut pending_events,
                ) {
                    shutdown_requested = true;
                    break;
                }
            }
        }

        if !shutdown_requested {
            if dirty.services {
                self.services_projection_dirty = true;
            }
            if dirty.startup {
                self.startup_projection_dirty = true;
            }
            if dirty.users {
                self.users_projection_dirty = true;
            }
            if dirty.app_history {
                self.app_history_projection_dirty = true;
            }
            if need_sample_tick {
                self.run_sample_tick();
                self.schedule_after_tick(self.tick_count <= 1);
                if !self.window_dormant && !self.temp_frozen {
                    let post_mask = SliceUpdateMask {
                        services: dirty.services
                            && self.leases.is_active(MonitorDomain::Services)
                            && self.services_polled_once,
                        startup: dirty.startup
                            && self.leases.is_active(MonitorDomain::Startup)
                            && self.startup_polled_once,
                        ..SliceUpdateMask::default()
                    };
                    if post_mask.any() {
                        self.publish_slices(post_mask);
                    }
                }
            } else if !self.window_dormant && !self.temp_frozen && dirty.any() {
                let include_pending_cold = self.pending_cold_publish;
                let processes_needed =
                    (dirty.processes || include_pending_cold) && self.processes_projection_active();
                let details_needed = (dirty.details || include_pending_cold)
                    && self.leases.is_active(MonitorDomain::Details);
                let mask = SliceUpdateMask {
                    processes: processes_needed,
                    details: details_needed,
                    performance: false,
                    include_per_core: false,
                    services: dirty.services
                        && self.leases.is_active(MonitorDomain::Services)
                        && self.services_polled_once,
                    startup: dirty.startup
                        && self.leases.is_active(MonitorDomain::Startup)
                        && self.startup_polled_once,
                    users: (dirty.users || include_pending_cold)
                        && self.leases.is_active(MonitorDomain::Users)
                        && (self.users_polled_once || !self.cached_user_sessions.is_empty()),
                    app_history: dirty.app_history
                        && self.leases.is_active(MonitorDomain::AppHistory)
                        && self.app_history_polled_once,
                };
                if mask.any() {
                    if include_pending_cold {
                        self.pending_cold_publish = false;
                        self.cold_flush_deadline = None;
                        self.last_cold_publish_at = Some(Instant::now());
                    }
                    self.publish_slices(mask);
                }
            }
        }

        for (event, reply_tx) in pending_events {
            self.events.emit(event, reply_tx);
        }

        for (kind, reply_tx) in pending_acks {
            send_ack(kind, reply_tx);
        }

        shutdown_requested
    }

    fn apply_single_command(
        &mut self,
        envelope: CommandEnvelope,
        dirty: &mut SliceUpdateMask,
        need_sample_tick: &mut bool,
        pending_acks: &mut Vec<(CommandKind, Sender<CommandEvent>)>,
        pending_events: &mut Vec<(CommandEvent, Option<Sender<CommandEvent>>)>,
    ) -> bool {
        let kind = envelope.command.kind();
        let reply = envelope.reply;

        match envelope.command {
            EngineCommand::Shutdown => {
                if let Some(reply_tx) = reply {
                    pending_acks.push((kind, reply_tx));
                }
                return true;
            }
            EngineCommand::SetRefreshSpeed(speed) => {
                let was_paused = self.refresh_speed == RefreshSpeed::Paused;
                self.refresh_speed = speed;
                if was_paused && speed != RefreshSpeed::Paused {
                    self.pending_history_gap = true;
                }
                self.schedule_after_tick(false);
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetSearchQuery(query) => {
                if self.view_config.search_query != query {
                    self.parsed_query = ParsedQuery::parse(&query);
                    self.view_config.search_query = query;
                    if self.parsed_query.has_port_predicate() {
                        self.maybe_enqueue_listening_ports(Instant::now());
                    } else {
                        self.projection.clear_listening_ports();
                        self.last_port_scan_at = None;
                    }
                    dirty.processes = true;
                    dirty.details = true;
                    dirty.services = true;
                    dirty.startup = true;
                    dirty.users = true;
                    dirty.app_history = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetProcessViewMode(mode) => {
                if self.view_config.process_view_mode != mode {
                    self.view_config.process_view_mode = mode;
                    dirty.processes = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetProcessSort { column, direction } => {
                if self.view_config.process_sort_column != column
                    || self.view_config.process_sort_direction != direction
                {
                    self.view_config.process_sort_column = column;
                    self.view_config.process_sort_direction = direction;
                    dirty.processes = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::ToggleProcessExpanded(key) => {
                if !self.view_config.process_expanded.remove(&key) {
                    self.view_config.process_expanded.insert(key);
                }
                dirty.processes = true;
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetDetailsViewMode(mode) => {
                if self.view_config.details_view_mode != mode {
                    self.view_config.details_view_mode = mode;
                    dirty.details = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetDetailsSort { column, direction } => {
                if self.view_config.details_sort_column != column
                    || self.view_config.details_sort_direction != direction
                {
                    self.view_config.details_sort_column = column;
                    self.view_config.details_sort_direction = direction;
                    dirty.details = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::ToggleDetailsExpanded(key) => {
                if !self.view_config.details_expanded.remove(&key) {
                    self.view_config.details_expanded.insert(key);
                }
                dirty.details = true;
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetServicesSort { column, direction } => {
                if self.view_config.services_sort_column != column
                    || self.view_config.services_sort_direction != direction
                {
                    self.view_config.services_sort_column = column;
                    self.view_config.services_sort_direction = direction;
                    dirty.services = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetServicesFilterPid(pid) => {
                if self.view_config.services_filter_pid != pid {
                    self.view_config.services_filter_pid = pid;
                    dirty.services = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetStartupSort { column, direction } => {
                if self.view_config.startup_sort_column != column
                    || self.view_config.startup_sort_direction != direction
                {
                    self.view_config.startup_sort_column = column;
                    self.view_config.startup_sort_direction = direction;
                    dirty.startup = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetUsersSort { column, direction } => {
                if self.view_config.users_sort_column != column
                    || self.view_config.users_sort_direction != direction
                {
                    self.view_config.users_sort_column = column;
                    self.view_config.users_sort_direction = direction;
                    dirty.users = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::ToggleUserSessionExpanded(session_id) => {
                if !self.view_config.users_expanded.remove(&session_id) {
                    self.view_config.users_expanded.insert(session_id);
                }
                dirty.users = true;
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetAppHistorySort { column, direction } => {
                if self.view_config.app_history_sort_column != column
                    || self.view_config.app_history_sort_direction != direction
                {
                    self.view_config.app_history_sort_column = column;
                    self.view_config.app_history_sort_direction = direction;
                    dirty.app_history = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetWindowDormant(dormant) => {
                let was_dormant = self.window_dormant;
                self.window_dormant = dormant;
                self.sync_process_network_state();
                if dormant {
                    self.next_tick = None;
                    *need_sample_tick = false;
                } else if was_dormant {
                    *need_sample_tick = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetProcessOrderFreeze(frozen) => {
                if frozen {
                    self.view_config
                        .freeze_process_order(&self.current_snapshot.processes.rows);
                    dirty.processes = true;
                } else if self.view_config.process_order_frozen {
                    self.view_config.process_order_frozen = false;
                    self.view_config.frozen_process_order.clear();
                    dirty.processes = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetDetailsOrderFreeze(frozen) => {
                if frozen {
                    if self.current_snapshot.details.rows.is_empty()
                        && self.leases.is_active(MonitorDomain::Details)
                    {
                        let rev = self.current_snapshot.revisions.details.saturating_add(1);
                        let slice = self.projection.project_details_with_query(
                            &self.view_config,
                            &self.parsed_query,
                            &self.cold_cache,
                            rev,
                        );
                        self.view_config.freeze_details_order(&slice.rows);
                    } else {
                        self.view_config
                            .freeze_details_order(&self.current_snapshot.details.rows);
                    }
                    dirty.details = true;
                } else if self.view_config.details_order_frozen {
                    self.view_config.details_order_frozen = false;
                    self.view_config.frozen_details_order.clear();
                    dirty.details = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetTempFreeze(frozen) => {
                let was_frozen = self.temp_frozen;
                self.temp_frozen = frozen;
                if frozen {
                    self.next_tick = None;
                } else if was_frozen && !self.window_dormant {
                    if self.refresh_speed != RefreshSpeed::Paused {
                        *need_sample_tick = true;
                    } else {
                        self.next_tick = None;
                    }
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::ForceRefresh => {
                *need_sample_tick = true;
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::TerminateProcess(key) => {
                let outcome = match self.backend.operator().terminate_process(key) {
                    Ok(()) => CommandOutcome::Ok(CommandSuccess::Ack),
                    Err(err) => CommandOutcome::err(err),
                };
                self.events
                    .emit(CommandEvent::new(kind, Some(key), outcome), reply);
            }
            EngineCommand::TerminateProcessTree(root_key) => {
                let targets = collect_bottom_up_kill_order(self.projection.latest_tree(), root_key);
                let outcome = match self.backend.operator().terminate_process_tree(&targets) {
                    Ok(report) => CommandOutcome::Ok(CommandSuccess::ProcessTreeTerminated(report)),
                    Err(err) => CommandOutcome::err(err),
                };
                self.events
                    .emit(CommandEvent::new(kind, Some(root_key), outcome), reply);
            }
            EngineCommand::SuspendProcess(key) => {
                match self.backend.operator().suspend_process(key) {
                    Ok(()) => {
                        *need_sample_tick = true;
                        pending_events.push((
                            CommandEvent::new(
                                kind,
                                Some(key),
                                CommandOutcome::Ok(CommandSuccess::Ack),
                            ),
                            reply,
                        ));
                    }
                    Err(err) => {
                        self.events.emit(
                            CommandEvent::new(kind, Some(key), CommandOutcome::err(err)),
                            reply,
                        );
                    }
                }
            }
            EngineCommand::ResumeProcess(key) => {
                match self.backend.operator().resume_process(key) {
                    Ok(()) => {
                        *need_sample_tick = true;
                        pending_events.push((
                            CommandEvent::new(
                                kind,
                                Some(key),
                                CommandOutcome::Ok(CommandSuccess::Ack),
                            ),
                            reply,
                        ));
                    }
                    Err(err) => {
                        self.events.emit(
                            CommandEvent::new(kind, Some(key), CommandOutcome::err(err)),
                            reply,
                        );
                    }
                }
            }
            EngineCommand::RestartExplorer => {
                self.dispatch_slow_shell_op(kind, None, SlowShellOp::RestartExplorer, reply);
            }
            EngineCommand::FindFileLocks { path } => {
                self.dispatch_slow_shell_op(kind, None, SlowShellOp::FindFileLocks(path), reply);
            }
            EngineCommand::QueryTaskManagerReplacement => {
                self.dispatch_slow_shell_op(
                    kind,
                    None,
                    SlowShellOp::QueryTaskManagerReplacement,
                    reply,
                );
            }
            EngineCommand::SetTaskManagerReplacement(enabled) => {
                self.dispatch_slow_shell_op(
                    kind,
                    None,
                    SlowShellOp::SetTaskManagerReplacement(enabled),
                    reply,
                );
            }
            EngineCommand::OpenNativeTaskManager => {
                self.dispatch_slow_shell_op(kind, None, SlowShellOp::OpenNativeTaskManager, reply);
            }
            EngineCommand::QueryAdminStartup => {
                self.dispatch_slow_shell_op(kind, None, SlowShellOp::QueryAdminStartup, reply);
            }
            EngineCommand::SetAdminStartup(enabled) => {
                self.dispatch_slow_shell_op(
                    kind,
                    None,
                    SlowShellOp::SetAdminStartup(enabled),
                    reply,
                );
            }
            EngineCommand::SetWebView2HostGrouping(enabled) => {
                if self.view_config.webview2_host_grouping != enabled {
                    self.view_config.webview2_host_grouping = enabled;
                    dirty.processes = true;
                    dirty.details = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetShowLeakWarnings(enabled) => {
                if self.view_config.show_leak_warnings != enabled {
                    self.view_config.show_leak_warnings = enabled;
                    dirty.processes = true;
                    dirty.details = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::IgnoreLeakPath(path) => {
                let trimmed = path.trim();
                if !trimmed.is_empty()
                    && !self
                        .view_config
                        .leak_ignored_paths
                        .iter()
                        .any(|existing| existing.eq_ignore_ascii_case(trimmed))
                {
                    self.view_config.leak_ignored_paths.push(trimmed.to_owned());
                    dirty.processes = true;
                    dirty.details = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetHideSystemProcesses(hidden) => {
                if self.view_config.hide_windows_system_processes != hidden {
                    self.view_config.hide_windows_system_processes = hidden;
                    dirty.processes = true;
                    dirty.details = true;
                }
                push_ack(kind, reply, pending_acks);
            }
            EngineCommand::SetEfficiencyMode { key, enabled } => {
                match self.backend.operator().set_efficiency_mode(key, enabled) {
                    Ok(()) => {
                        self.cold_cache.set_efficiency_mode(key, enabled);
                        dirty.processes = true;
                        dirty.details = true;
                        pending_events.push((
                            CommandEvent::new(
                                kind,
                                Some(key),
                                CommandOutcome::Ok(CommandSuccess::Ack),
                            ),
                            reply,
                        ));
                    }
                    Err(err) => {
                        self.events.emit(
                            CommandEvent::new(kind, Some(key), CommandOutcome::err(err)),
                            reply,
                        );
                    }
                }
            }
            EngineCommand::SetPriority { key, priority } => {
                match self.backend.operator().set_priority(key, priority) {
                    Ok(()) => {
                        *need_sample_tick = true;
                        pending_events.push((
                            CommandEvent::new(
                                kind,
                                Some(key),
                                CommandOutcome::Ok(CommandSuccess::Ack),
                            ),
                            reply,
                        ));
                    }
                    Err(err) => {
                        self.events.emit(
                            CommandEvent::new(kind, Some(key), CommandOutcome::err(err)),
                            reply,
                        );
                    }
                }
            }
            EngineCommand::GetAffinity(key) => {
                let outcome = match self.backend.operator().get_affinity(key) {
                    Ok(mask) => CommandOutcome::Ok(CommandSuccess::Affinity(mask)),
                    Err(err) => CommandOutcome::err(err),
                };
                self.events
                    .emit(CommandEvent::new(kind, Some(key), outcome), reply);
            }
            EngineCommand::SetAffinity { key, mask } => {
                let outcome = match self.backend.operator().set_affinity(key, &mask) {
                    Ok(()) => CommandOutcome::Ok(CommandSuccess::Ack),
                    Err(err) => CommandOutcome::err(err),
                };
                self.events
                    .emit(CommandEvent::new(kind, Some(key), outcome), reply);
            }
            EngineCommand::SetUacVirtualization { key, enabled } => {
                match self.backend.operator().set_uac_virtualization(key, enabled) {
                    Ok(()) => {
                        self.cold_cache.set_uac_virtualization(key, enabled);
                        dirty.details = true;
                        pending_events.push((
                            CommandEvent::new(
                                kind,
                                Some(key),
                                CommandOutcome::Ok(CommandSuccess::Ack),
                            ),
                            reply,
                        ));
                    }
                    Err(err) => {
                        self.events.emit(
                            CommandEvent::new(kind, Some(key), CommandOutcome::err(err)),
                            reply,
                        );
                    }
                }
            }
            EngineCommand::AnalyzeWaitChain(key) => {
                self.dispatch_slow_shell_op(
                    kind,
                    Some(key),
                    SlowShellOp::AnalyzeWaitChain(key),
                    reply,
                );
            }
            EngineCommand::WriteMiniDump { key, dest, cancel } => {
                self.dispatch_slow_shell_op(
                    kind,
                    Some(key),
                    SlowShellOp::WriteMiniDump { key, dest, cancel },
                    reply,
                );
            }
            EngineCommand::AttachDebugger(key) => {
                self.dispatch_slow_shell_op(
                    kind,
                    Some(key),
                    SlowShellOp::AttachDebugger(key),
                    reply,
                );
            }
            EngineCommand::BrowseExecutable => {
                self.dispatch_slow_shell_op(kind, None, SlowShellOp::BrowseExecutable, reply);
            }
            EngineCommand::RunNewTask(req) => {
                self.dispatch_slow_shell_op(kind, None, SlowShellOp::RunNewTask(req), reply);
            }
            EngineCommand::OpenFileLocation(path) => {
                self.dispatch_slow_shell_op(kind, None, SlowShellOp::OpenFileLocation(path), reply);
            }
            EngineCommand::ShowFileProperties(path) => {
                self.dispatch_slow_shell_op(
                    kind,
                    None,
                    SlowShellOp::ShowFileProperties(path),
                    reply,
                );
            }
            EngineCommand::SearchOnline(query) => {
                self.dispatch_slow_shell_op(kind, None, SlowShellOp::SearchOnline(query), reply);
            }
            EngineCommand::RelaunchElevated(route) => {
                self.dispatch_slow_shell_op(
                    kind,
                    None,
                    SlowShellOp::RelaunchElevated(route),
                    reply,
                );
            }
            EngineCommand::StartService(name) => {
                self.dispatch_slow_mgmt_op(kind, SlowMgmtOp::StartService(name), reply);
            }
            EngineCommand::StopService(name) => {
                self.dispatch_slow_mgmt_op(kind, SlowMgmtOp::StopService(name), reply);
            }
            EngineCommand::RestartService(name) => {
                self.dispatch_slow_mgmt_op(kind, SlowMgmtOp::RestartService(name), reply);
            }
            EngineCommand::OpenServicesConsole => {
                self.dispatch_slow_mgmt_op(kind, SlowMgmtOp::OpenServicesConsole, reply);
            }
            EngineCommand::SetStartupEnabled { id, enabled } => {
                self.dispatch_slow_mgmt_op(
                    kind,
                    SlowMgmtOp::SetStartupEnabled { id, enabled },
                    reply,
                );
            }
            EngineCommand::DisconnectSession(session_id) => {
                self.dispatch_slow_mgmt_op(kind, SlowMgmtOp::DisconnectSession(session_id), reply);
            }
            EngineCommand::ClearAppHistory => {
                self.dispatch_slow_mgmt_op(kind, SlowMgmtOp::ClearAppHistory, reply);
            }
        }

        false
    }

    fn dispatch_slow_shell_op(
        &self,
        kind: CommandKind,
        target: Option<ProcessKey>,
        op: SlowShellOp,
        reply: Option<Sender<CommandEvent>>,
    ) {
        if let Err(SlowPriorityJob::ShellOp { reply, .. }) = self
            .slow_pool
            .try_enqueue_priority(SlowPriorityJob::ShellOp { op, reply })
        {
            let event = CommandEvent::new(
                kind,
                target,
                CommandOutcome::err(PalError::CircuitOpen {
                    target: "slow-lane".to_owned(),
                    retry_after_ms: 100,
                }),
            );
            self.events.emit(event, reply);
        }
    }

    fn dispatch_slow_mgmt_op(
        &self,
        kind: CommandKind,
        op: SlowMgmtOp,
        reply: Option<Sender<CommandEvent>>,
    ) {
        if let Err(SlowPriorityJob::MgmtOp { reply, .. }) = self
            .slow_pool
            .try_enqueue_priority(SlowPriorityJob::MgmtOp { op, reply })
        {
            let event = CommandEvent::new(
                kind,
                None,
                CommandOutcome::err(PalError::CircuitOpen {
                    target: "slow-lane".to_owned(),
                    retry_after_ms: 100,
                }),
            );
            self.events.emit(event, reply);
        }
    }
}
