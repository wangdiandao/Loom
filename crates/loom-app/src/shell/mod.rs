//! Desktop window shell, navigation routing, shell dialogs, and system tray controller.

pub(crate) mod dialogs;
pub(crate) mod nav;
pub(crate) mod tray;
pub(crate) mod window;

pub(crate) use nav::{Route, ShellHandle};
pub(crate) use tray::{TrayAction, TrayController, TrayEventSignal};
pub(crate) use window::AppShell;
