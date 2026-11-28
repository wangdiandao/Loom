//! Shared process-control eligibility predicates and command dispatchers
//! used across Processes and Details views.

use gpui_kit::{App, Entity};
use loom_engine::EngineCommand;
use loom_types::{CapabilityFlags, ProcessKey, ProcessStatusFlags};

use crate::bridge::UiStore;

/// Returns `true` when `image_name` refers to `explorer.exe` (case-insensitive).
#[inline]
pub(crate) fn is_explorer_image(image_name: &str) -> bool {
    image_name.trim().eq_ignore_ascii_case("explorer.exe")
}

/// Returns `true` when a process can safely be suspended or resumed.
#[inline]
pub(crate) fn can_suspend_or_resume_process(
    pid: u32,
    image_name: &str,
    status: ProcessStatusFlags,
    capabilities: CapabilityFlags,
) -> bool {
    capabilities.contains(CapabilityFlags::SUSPEND_RESUME)
        && pid != 0
        && pid != std::process::id()
        && !image_name.trim().eq_ignore_ascii_case("dwm.exe")
        && !status.intersects(
            ProcessStatusFlags::SYSTEM_IDLE
                | ProcessStatusFlags::KERNEL
                | ProcessStatusFlags::PROTECTED
                | ProcessStatusFlags::CRITICAL,
        )
}

/// Returns the best path/name identifier for `EngineCommand::IgnoreLeakPath`.
pub(crate) fn leak_ignore_target(exe_path: Option<&str>, image_name: &str) -> Option<String> {
    if let Some(path) = exe_path.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(path.to_owned());
    }
    let name = image_name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// Dispatches `EngineCommand::SuspendProcess` or `EngineCommand::ResumeProcess`.
pub(crate) fn dispatch_suspend_or_resume(
    store: &Entity<UiStore>,
    key: ProcessKey,
    currently_suspended: bool,
    cx: &mut App,
) {
    let cmd = if currently_suspended {
        EngineCommand::ResumeProcess(key)
    } else {
        EngineCommand::SuspendProcess(key)
    };
    let _ = store.read(cx).dispatch_command(cmd);
}

/// Dispatches `EngineCommand::RestartExplorer`.
pub(crate) fn dispatch_restart_explorer(store: &Entity<UiStore>, cx: &mut App) {
    let _ = store
        .read(cx)
        .dispatch_command(EngineCommand::RestartExplorer);
}

/// Dispatches `EngineCommand::IgnoreLeakPath` and persists the ignored path in `settings.toml`.
pub(crate) fn dispatch_ignore_leak_path(store: &Entity<UiStore>, target: String, cx: &mut App) {
    let trimmed = target.trim().to_owned();
    if trimmed.is_empty() {
        return;
    }
    store.update(cx, |s, cx| {
        let _ = s.dispatch_command(EngineCommand::IgnoreLeakPath(trimmed.clone()));
        let _ = s.update_settings(
            |cfg| {
                if !cfg
                    .leak_ignored_paths
                    .iter()
                    .any(|p| p.eq_ignore_ascii_case(&trimmed))
                {
                    cfg.leak_ignored_paths.push(trimmed.clone());
                }
            },
            cx,
        );
    });
}

/// Dispatches `EngineCommand::SetHideSystemProcesses` and persists it in `settings.toml`.
pub(crate) fn dispatch_hide_system_processes(store: &Entity<UiStore>, hidden: bool, cx: &mut App) {
    store.update(cx, |s, cx| {
        let _ = s.dispatch_command(EngineCommand::SetHideSystemProcesses(hidden));
        let _ = s.update_settings(|cfg| cfg.hide_windows_system_processes = hidden, cx);
    });
}
