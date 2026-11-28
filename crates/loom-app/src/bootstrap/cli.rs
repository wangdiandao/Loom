//! Command-line argument parsing for `loom-app`.

use std::path::PathBuf;

use crate::shell::Route;
use crate::startup_trace::StartupTraceTarget;

/// Parsed command-line launch arguments for `loom-app`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct LaunchArgs {
    /// Optional route slug requested via `--restore-route=<slug>` (e.g., after elevated relaunch).
    pub(crate) restore_route: Option<Route>,
    /// Optional explicit `settings.toml` path via `--config=<path>` (useful for isolated tests).
    pub(crate) config_path: Option<PathBuf>,
    /// Skip the OS single-instance lock when `--allow-multiple` is passed (testing / automation).
    pub(crate) allow_multiple: bool,
    /// Optional opt-in startup phase trace destination via `--startup-trace[=<path>]`.
    pub(crate) startup_trace: Option<StartupTraceTarget>,
}

impl LaunchArgs {
    /// Parses CLI arguments from an iterator (typically `std::env::args().skip(1)`).
    pub(crate) fn parse_from<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut parsed = Self::default();
        let mut iter = args.into_iter().peekable();
        while let Some(raw) = iter.next() {
            let arg = raw.as_ref();
            if let Some(val) = arg.strip_prefix("--restore-route=") {
                parsed.restore_route = Route::parse(val);
            } else if arg == "--restore-route" {
                if let Some(next) = iter.next() {
                    parsed.restore_route = Route::parse(next.as_ref());
                }
            } else if let Some(val) = arg.strip_prefix("--config=") {
                if !val.trim().is_empty() {
                    parsed.config_path = Some(PathBuf::from(val));
                }
            } else if arg == "--config" {
                if let Some(next) = iter.next()
                    && !next.as_ref().trim().is_empty()
                {
                    parsed.config_path = Some(PathBuf::from(next.as_ref()));
                }
            } else if arg == "--allow-multiple" {
                parsed.allow_multiple = true;
            } else if let Some(val) = arg.strip_prefix("--startup-trace=") {
                let trimmed = val.trim();
                parsed.startup_trace = Some(if trimmed.is_empty() {
                    StartupTraceTarget::Stderr
                } else {
                    StartupTraceTarget::File(PathBuf::from(trimmed))
                });
            } else if arg == "--startup-trace" {
                let next_is_path = iter.peek().is_some_and(|next| {
                    !next.as_ref().starts_with("--") && !next.as_ref().trim().is_empty()
                });
                if next_is_path && let Some(next) = iter.next() {
                    parsed.startup_trace = Some(StartupTraceTarget::File(PathBuf::from(
                        next.as_ref().trim(),
                    )));
                } else {
                    parsed.startup_trace = Some(StartupTraceTarget::Stderr);
                }
            }
        }
        parsed
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::shell::Route;
    use crate::startup_trace::StartupTraceTarget;

    use super::LaunchArgs;

    #[test]
    fn launch_args_parses_restore_route_config_and_allow_multiple() {
        let posix_args = LaunchArgs::parse_from([
            "--restore-route=details",
            "--config=/var/lib/loom/settings.toml",
            "--allow-multiple",
            "--startup-trace=/tmp/loom-startup.log",
        ]);
        assert_eq!(posix_args.restore_route, Some(Route::Details));
        assert_eq!(
            posix_args.config_path,
            Some(PathBuf::from("/var/lib/loom/settings.toml"))
        );
        assert!(posix_args.allow_multiple);
        assert_eq!(
            posix_args.startup_trace,
            Some(StartupTraceTarget::File(PathBuf::from(
                "/tmp/loom-startup.log"
            )))
        );

        let windows_args = LaunchArgs::parse_from([
            "--restore-route",
            "settings",
            "--config",
            r"C:\ProgramData\Loom\settings.toml",
            "--startup-trace",
            "--allow-multiple",
        ]);
        assert_eq!(windows_args.restore_route, Some(Route::Settings));
        assert_eq!(
            windows_args.config_path,
            Some(PathBuf::from(r"C:\ProgramData\Loom\settings.toml"))
        );
        assert!(windows_args.allow_multiple);
        assert_eq!(windows_args.startup_trace, Some(StartupTraceTarget::Stderr));

        let split_trace = LaunchArgs::parse_from(["--startup-trace", r"C:\Temp\startup_trace.txt"]);
        assert_eq!(
            split_trace.startup_trace,
            Some(StartupTraceTarget::File(PathBuf::from(
                r"C:\Temp\startup_trace.txt"
            )))
        );

        let empty = LaunchArgs::parse_from(Vec::<&str>::new());
        assert_eq!(empty, LaunchArgs::default());
    }
}
