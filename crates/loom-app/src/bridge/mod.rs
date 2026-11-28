//! Domain bridge connecting `loom-engine` and `loom-config` to GPUI views.

pub(crate) mod commands;
pub(crate) mod monitor_lease;
pub(crate) mod store;

pub(crate) use commands::{UiCommands, apply_theme, resolve_accent_rgb, system_accent_rgb};
pub(crate) use monitor_lease::ScopedMonitorLease;
pub(crate) use store::{UiStore, UiStoreEvent};
