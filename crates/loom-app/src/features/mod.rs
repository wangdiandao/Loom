//! Independent vertical UI feature slices for Loom.
//!
//! Feature modules MUST NOT import each other's internals. Cross-page navigation
//! flows exclusively through [`crate::shell::nav::ShellHandle`] and
//! [`crate::shell::nav::NavigationIntent`].

pub(crate) mod app_history;
pub(crate) mod details;
pub(crate) mod performance;
pub(crate) mod processes;
pub(crate) mod services;
pub(crate) mod settings;
pub(crate) mod startup;
pub(crate) mod table_columns;
pub(crate) mod users;

pub(crate) use app_history::AppHistoryView;
pub(crate) use details::DetailsView;
pub(crate) use performance::PerformanceView;
pub(crate) use processes::ProcessesView;
pub(crate) use services::ServicesView;
pub(crate) use settings::SettingsView;
pub(crate) use startup::StartupView;
pub(crate) use users::UsersView;
