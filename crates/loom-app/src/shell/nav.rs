//! Declarative application routing table and cross-page [`NavigationIntent`] bus.

use std::rc::Rc;

use gpui_kit::component::IconName;
use gpui_kit::{App, Window};
use loom_types::{ProcessKey, StartupPage};

use crate::i18n;

/// All 8 top-level navigation routes in Loom's desktop shell.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Route {
    /// Primary live Processes view.
    #[default]
    Processes,
    /// Multi-device hardware Performance graphs (`v0.0.4`).
    Performance,
    /// Cumulative App History table (`v0.0.5`).
    AppHistory,
    /// Startup applications manager (`v0.0.5`).
    Startup,
    /// Logged-in user sessions (`v0.0.5`).
    Users,
    /// On-demand full-field Details view (bound to `MonitorDomain::Details`).
    Details,
    /// System services manager (`v0.0.5`).
    Services,
    /// Inline application Settings page (pinned in sidebar footer).
    Settings,
}

impl Route {
    /// Ordered list of the 7 primary routes shown in the main sidebar menu.
    ///
    /// [`Route::Settings`] is pinned separately in the sidebar footer.
    pub(crate) const PRIMARY_NAV: [Self; 7] = [
        Self::Processes,
        Self::Performance,
        Self::AppHistory,
        Self::Startup,
        Self::Users,
        Self::Details,
        Self::Services,
    ];

    /// Every route variant in declaration order.
    #[cfg(test)]
    pub(crate) const ALL: [Self; 8] = [
        Self::Processes,
        Self::Performance,
        Self::AppHistory,
        Self::Startup,
        Self::Users,
        Self::Details,
        Self::Services,
        Self::Settings,
    ];

    /// Canonical lowercase route slug used for `--restore-route=<slug>` and elevation relaunch.
    #[inline]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Processes => "processes",
            Self::Performance => "performance",
            Self::AppHistory => "app-history",
            Self::Startup => "startup",
            Self::Users => "users",
            Self::Details => "details",
            Self::Services => "services",
            Self::Settings => "settings",
        }
    }

    /// Parses a route identifier from CLI flags (case-insensitive, tolerant of `_` vs `-`).
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let normalized = raw.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "processes" | "process" => Some(Self::Processes),
            "performance" | "perf" => Some(Self::Performance),
            "app-history" | "app_history" | "apphistory" => Some(Self::AppHistory),
            "startup" | "startup-apps" | "startup_apps" => Some(Self::Startup),
            "users" | "user" => Some(Self::Users),
            "details" | "detail" => Some(Self::Details),
            "services" | "service" => Some(Self::Services),
            "settings" | "setting" => Some(Self::Settings),
            _ => None,
        }
    }

    /// Maps a persisted [`StartupPage`] preference to its corresponding [`Route`].
    #[inline]
    pub(crate) const fn from_startup_page(page: StartupPage) -> Self {
        match page {
            StartupPage::Processes => Self::Processes,
            StartupPage::Performance => Self::Performance,
            StartupPage::AppHistory => Self::AppHistory,
            StartupPage::Startup => Self::Startup,
            StartupPage::Users => Self::Users,
            StartupPage::Details => Self::Details,
            StartupPage::Services => Self::Services,
        }
    }

    /// Returns the localized display label for this route.
    pub(crate) fn label(self) -> String {
        match self {
            Self::Processes => i18n::tr("nav.processes"),
            Self::Performance => i18n::tr("nav.performance"),
            Self::AppHistory => i18n::tr("nav.app_history"),
            Self::Startup => i18n::tr("nav.startup"),
            Self::Users => i18n::tr("nav.users"),
            Self::Details => i18n::tr("nav.details"),
            Self::Services => i18n::tr("nav.services"),
            Self::Settings => i18n::tr("nav.settings"),
        }
    }

    /// Returns the `gpui-kit` bundled [`IconName`] for this route.
    #[inline]
    pub(crate) const fn icon(self) -> IconName {
        match self {
            Self::Processes => IconName::LayoutDashboard,
            Self::Performance => IconName::Cpu,
            Self::AppHistory => IconName::ChartPie,
            Self::Startup => IconName::Play,
            Self::Users => IconName::User,
            Self::Details => IconName::FileText,
            Self::Services => IconName::Settings2,
            Self::Settings => IconName::Settings,
        }
    }

    /// Returns the scheduled milestone version for unimplemented P1 routes, or `None` for live views.
    #[inline]
    pub(crate) const fn milestone_version(self) -> Option<&'static str> {
        match self {
            Self::AppHistory | Self::Startup | Self::Users | Self::Services => Some("0.0.5"),
            Self::Processes | Self::Performance | Self::Details | Self::Settings => None,
        }
    }

    /// Returns the localized roadmap description for an unimplemented P1 route.
    pub(crate) fn milestone_description(self) -> Option<String> {
        match self {
            Self::AppHistory => Some(i18n::tr("milestone.app_history_desc")),
            Self::Startup => Some(i18n::tr("milestone.startup_desc")),
            Self::Users => Some(i18n::tr("milestone.users_desc")),
            Self::Services => Some(i18n::tr("milestone.services_desc")),
            Self::Processes | Self::Performance | Self::Details | Self::Settings => None,
        }
    }
}

/// Cross-page navigation intents dispatched by features via [`ShellHandle`].
///
/// Prevents vertical feature slices from importing each other's internal views.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NavigationIntent {
    /// Navigate directly to `Route`.
    Navigate(Route),
    /// Switch to [`Route::Details`] and highlight/scroll to the target [`ProcessKey`].
    JumpToDetails(ProcessKey),
    /// Switch to [`Route::Services`] and filter/highlight services hosted by the given PID.
    #[allow(dead_code)]
    JumpToServices(u32),
}

type NavDispatcher = Rc<dyn Fn(NavigationIntent, &mut Window, &mut App)>;

/// Decoupled handle passed to feature slices for issuing shell-level navigation intents.
#[derive(Clone)]
pub(crate) struct ShellHandle {
    dispatch: NavDispatcher,
}

impl ShellHandle {
    /// Creates a new [`ShellHandle`] backed by `dispatch`.
    pub(crate) fn new(
        dispatch: impl Fn(NavigationIntent, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            dispatch: Rc::new(dispatch),
        }
    }

    /// Dispatches a [`NavigationIntent`] to the owning [`super::AppShell`].
    pub(crate) fn send(&self, intent: NavigationIntent, window: &mut Window, cx: &mut App) {
        (self.dispatch)(intent, window, cx);
    }

    /// Navigates to [`Route::Details`] targeting `key`.
    pub(crate) fn jump_to_details(&self, key: ProcessKey, window: &mut Window, cx: &mut App) {
        self.send(NavigationIntent::JumpToDetails(key), window, cx);
    }

    /// Navigates to `route`.
    #[allow(dead_code)]
    pub(crate) fn navigate(&self, route: Route, window: &mut Window, cx: &mut App) {
        self.send(NavigationIntent::Navigate(route), window, cx);
    }
}

#[cfg(test)]
mod tests {
    use loom_types::StartupPage;

    use super::Route;

    #[test]
    fn route_string_roundtrip_and_aliases() {
        for route in Route::ALL {
            let slug = route.as_str();
            assert_eq!(
                Route::parse(slug),
                Some(route),
                "roundtrip failed for {route:?} ('{slug}')"
            );
            assert_eq!(
                Route::parse(&slug.to_uppercase()),
                Some(route),
                "case-insensitive parse failed for '{slug}'"
            );
        }

        assert_eq!(Route::parse("app_history"), Some(Route::AppHistory));
        assert_eq!(Route::parse("  details "), Some(Route::Details));
        assert_eq!(Route::parse("unknown-page"), None);
        assert_eq!(Route::parse(""), None);
    }

    #[test]
    fn startup_page_maps_to_expected_route() {
        assert_eq!(
            Route::from_startup_page(StartupPage::Processes),
            Route::Processes
        );
        assert_eq!(
            Route::from_startup_page(StartupPage::Performance),
            Route::Performance
        );
        assert_eq!(
            Route::from_startup_page(StartupPage::AppHistory),
            Route::AppHistory
        );
        assert_eq!(
            Route::from_startup_page(StartupPage::Startup),
            Route::Startup
        );
        assert_eq!(Route::from_startup_page(StartupPage::Users), Route::Users);
        assert_eq!(
            Route::from_startup_page(StartupPage::Details),
            Route::Details
        );
        assert_eq!(
            Route::from_startup_page(StartupPage::Services),
            Route::Services
        );
    }

    #[test]
    fn live_routes_have_no_milestone_notice_while_p1_routes_do() {
        assert_eq!(Route::Processes.milestone_version(), None);
        assert_eq!(Route::Performance.milestone_version(), None);
        assert_eq!(Route::Details.milestone_version(), None);
        assert_eq!(Route::Settings.milestone_version(), None);

        assert_eq!(Route::AppHistory.milestone_version(), Some("0.0.5"));
        assert_eq!(Route::Startup.milestone_version(), Some("0.0.5"));
        assert_eq!(Route::Users.milestone_version(), Some("0.0.5"));
        assert_eq!(Route::Services.milestone_version(), Some("0.0.5"));
    }
}
