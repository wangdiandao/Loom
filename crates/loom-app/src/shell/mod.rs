//! Desktop window shell, navigation routing, shell dialogs, diagnostic summary, and system tray controller.

pub(crate) mod diagnostic_summary;
pub(crate) mod dialogs;
pub(crate) mod nav;
pub(crate) mod tray;
pub(crate) mod tray_overview;
pub(crate) mod window;

pub(crate) use nav::{NavigationIntent, Route, ShellHandle};
pub(crate) use tray::{TrayAction, TrayController, TrayEventSignal};
pub(crate) use tray_overview::{TRAY_OVERVIEW_HEIGHT, TRAY_OVERVIEW_WIDTH, TrayOverviewView};
pub(crate) use window::AppShell;
