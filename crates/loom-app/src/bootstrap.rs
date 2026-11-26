//! Progressive startup pipeline, window lifecycle orchestration, tray residency, and Win32 helpers.

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::TitleBar;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, BorrowAppContext as _, Bounds, Entity, Global, QuitMode,
    Subscription, Window, WindowBounds, WindowOptions, point, px, size,
};
use loom_config::{AppSettings, ConfigStore, WindowGeometry};
use loom_engine::{EngineCommand, EngineCoordinator};
use loom_pal::{NativeBackend, PlatformBackend, SingleInstanceGuard};

use crate::bridge::{UiStore, apply_theme};
use crate::i18n;
use crate::shell::{AppShell, Route, TrayAction, TrayController, TrayEventSignal};

/// Single-instance OS mutex identifier shared across normal launches of Loom.
const SINGLE_INSTANCE_ID: &str = "io.github.wangdiandao.loom.single-instance";

const DEFAULT_WINDOW_WIDTH: f32 = 1120.0;
const DEFAULT_WINDOW_HEIGHT: f32 = 720.0;
const MIN_WINDOW_WIDTH: f32 = 760.0;
const MIN_WINDOW_HEIGHT: f32 = 500.0;

/// Parsed command-line launch arguments for `loom-app`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct LaunchArgs {
    /// Optional route slug requested via `--restore-route=<slug>` (e.g., after UAC relaunch).
    pub(crate) restore_route: Option<Route>,
    /// Optional explicit `settings.toml` path via `--config=<path>` (useful for isolated tests).
    pub(crate) config_path: Option<PathBuf>,
    /// Skip the OS single-instance lock when `--allow-multiple` is passed (testing / automation).
    pub(crate) allow_multiple: bool,
}

impl LaunchArgs {
    /// Parses CLI arguments from an iterator (typically `std::env::args().skip(1)`).
    pub(crate) fn parse_from<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut parsed = Self::default();
        let mut iter = args.into_iter();
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
            }
        }
        parsed
    }
}

/// Application-wide GPUI global managing the active window handle, tray icon, and exit policy.
struct AppRuntime {
    store: Entity<UiStore>,
    active_window: Option<AnyWindowHandle>,
    active_route: Route,
    hiding_to_tray: bool,
    explicit_exit: bool,
    tray: Option<TrayController>,
    _window_closed_sub: Option<Subscription>,
    _single_instance: Option<SingleInstanceGuard>,
}

impl Global for AppRuntime {}

/// Launches the Loom desktop application using the progressive startup pipeline.
pub(crate) fn run(args: LaunchArgs) {
    // 1. Optional single-instance lock (bypassed when `--allow-multiple` is set for testing).
    let single_instance = if args.allow_multiple {
        None
    } else {
        match SingleInstanceGuard::acquire(SINGLE_INSTANCE_ID) {
            Ok(guard) if guard.is_single() => Some(guard),
            Ok(_) => {
                tracing::info!("another Loom instance is already running; exiting");
                return;
            }
            Err(err) => {
                tracing::warn!(%err, "single-instance lock failed; continuing startup");
                None
            }
        }
    };

    // 2. Synchronous, sub-millisecond settings load (< 1 ms).
    let config_store = match args.config_path {
        Some(path) => ConfigStore::new(path),
        None => ConfigStore::default_store(),
    };
    let (settings, load_err) = config_store.load_or_default();
    if let Some(err) = load_err {
        tracing::warn!(%err, path = %config_store.path().display(), "corrupt settings.toml; fell back to defaults");
    }

    // 3. Apply initial localization immediately before creating UI or tray items.
    i18n::apply_locale(settings.language);

    let initial_route = args
        .restore_route
        .unwrap_or_else(|| Route::from_startup_page(settings.startup_page));

    // 4. Initialize NativeBackend + EngineCoordinator BEFORE opening the window so Tick 0
    //    executes on `loom-fast-sampler` in parallel with DX12 / GPUI window creation!
    let coordinator = match NativeBackend::initialize()
        .and_then(|backend| EngineCoordinator::new(backend, settings.clone()))
    {
        Ok(coord) => Arc::new(coord),
        Err(err) => {
            tracing::error!(%err, "failed to initialize Loom native engine");
            std::process::exit(1);
        }
    };

    // 5. Launch GPUI Kit application event loop and open the main window.
    //    Use `QuitMode::Explicit` so closing the window with `close_to_tray` / `minimize_to_tray`
    //    enabled destroys the GPUI window (0% GPU) while keeping the tray icon alive.
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx| {
            gpui_kit::init(cx);
            apply_theme(settings.theme, None, cx);

            let store = cx.new(|cx| UiStore::new(coordinator, config_store, settings, cx));

            let (tray, tray_signal) = match TrayController::new() {
                Ok((controller, signal)) => (Some(controller), Some(signal)),
                Err(err) => {
                    tracing::warn!(%err, "failed to initialize system tray icon");
                    (None, None)
                }
            };

            cx.set_global(AppRuntime {
                store,
                active_window: None,
                active_route: initial_route,
                hiding_to_tray: false,
                explicit_exit: false,
                tray,
                _window_closed_sub: None,
                _single_instance: single_instance,
            });

            let closed_sub = cx.on_window_closed(|cx, closed_id| {
                handle_window_closed(cx, closed_id);
            });
            cx.update_global::<AppRuntime, _>(|runtime, _| {
                runtime._window_closed_sub = Some(closed_sub);
            });

            if let Some(signal) = tray_signal {
                spawn_tray_event_loop(signal, cx);
            }

            if let Err(err) = open_main_window(cx) {
                tracing::error!(%err, "failed to open Loom main window");
                cx.quit();
            }
        });
}

fn spawn_tray_event_loop(signal: TrayEventSignal, cx: &mut App) {
    cx.spawn(async move |cx| {
        while let Some(action) = signal.next().await {
            let should_continue = cx.update(|cx| match action {
                TrayAction::Show => {
                    show_or_reopen_main_window(cx);
                    true
                }
                TrayAction::Exit => {
                    quit_application(cx);
                    false
                }
            });
            if !should_continue {
                break;
            }
        }
    })
    .detach();
}

fn build_window_options(settings: &AppSettings) -> WindowOptions {
    let window_bounds = settings
        .window_geometry
        .filter(|g| g.width >= MIN_WINDOW_WIDTH as u32 && g.height >= MIN_WINDOW_HEIGHT as u32)
        .map(|g| {
            let bounds = Bounds::new(
                point(px(g.x as f32), px(g.y as f32)),
                size(px(g.width as f32), px(g.height as f32)),
            );
            if g.maximized {
                WindowBounds::Maximized(bounds)
            } else {
                WindowBounds::Windowed(bounds)
            }
        })
        .unwrap_or_else(|| {
            WindowBounds::Windowed(Bounds::new(
                point(px(120.), px(80.)),
                size(px(DEFAULT_WINDOW_WIDTH), px(DEFAULT_WINDOW_HEIGHT)),
            ))
        });

    WindowOptions {
        window_bounds: Some(window_bounds),
        window_min_size: Some(size(px(MIN_WINDOW_WIDTH), px(MIN_WINDOW_HEIGHT))),
        focus: true,
        show: true,
        ..TitleBar::window_options()
    }
}

fn open_main_window(cx: &mut App) -> gpui_kit::Result<()> {
    let (store, route, settings) = {
        let runtime = cx.global::<AppRuntime>();
        let store = runtime.store.clone();
        let route = runtime.active_route;
        let settings = store.read(cx).settings().clone();
        (store, route, settings)
    };

    let options = build_window_options(&settings);
    let (window_handle, _shell) = gpui_kit::open_window(options, cx, move |window, cx| {
        apply_theme(settings.theme, Some(window), cx);
        cx.new(|cx| AppShell::new(store, route, window, cx))
    })?;

    cx.update_global::<AppRuntime, _>(|runtime, _| {
        runtime.active_window = Some(window_handle);
        runtime.hiding_to_tray = false;
    });

    Ok(())
}

fn show_or_reopen_main_window(cx: &mut App) {
    let existing = cx.global::<AppRuntime>().active_window;
    if let Some(handle) = existing {
        restore_native_window_if_minimized();
        let updated = handle.update(cx, |_, window, _| {
            window.activate_window();
        });
        if updated.is_ok() {
            return;
        }
    }

    let store = cx.global::<AppRuntime>().store.clone();
    store.update(cx, |store, cx| {
        store.set_window_dormant(false, cx);
        let _ = store.dispatch_command(EngineCommand::ForceRefresh);
    });

    if let Err(err) = open_main_window(cx) {
        tracing::error!(%err, "failed to reopen Loom window from system tray");
    }
}

fn handle_window_closed(cx: &mut App, closed_id: gpui_kit::WindowId) {
    if !cx.has_global::<AppRuntime>() {
        return;
    }

    let (matched, store, stay_in_tray) = {
        let runtime = cx.global_mut::<AppRuntime>();
        let matched = runtime
            .active_window
            .is_some_and(|handle| handle.window_id() == closed_id);
        if !matched {
            return;
        }
        runtime.active_window = None;
        let hiding = std::mem::take(&mut runtime.hiding_to_tray);
        let explicit_exit = runtime.explicit_exit;
        let has_tray = runtime.tray.is_some();
        let store = runtime.store.clone();
        let close_to_tray = store.read(cx).settings().close_to_tray;
        let stay_in_tray = !explicit_exit && has_tray && (close_to_tray || hiding);
        (matched, store, stay_in_tray)
    };

    if !matched {
        return;
    }

    if stay_in_tray {
        tracing::info!("Loom window closed to system tray (0% GPU dormant mode)");
        store.update(cx, |store, cx| {
            store.set_window_dormant(true, cx);
        });
    } else {
        quit_application(cx);
    }
}

fn quit_application(cx: &mut App) {
    if !cx.has_global::<AppRuntime>() {
        cx.quit();
        return;
    }

    let (store, active_window) = {
        let runtime = cx.global_mut::<AppRuntime>();
        runtime.explicit_exit = true;
        runtime.tray = None;
        runtime._single_instance = None;
        (runtime.store.clone(), runtime.active_window.take())
    };

    if let Some(handle) = active_window {
        let _ = handle.update(cx, |_, window, _| {
            window.remove_window();
        });
    }

    store.read(cx).coordinator().shutdown();
    cx.quit();
}

/// Records the active [`Route`] so reopening from the system tray restores the same page.
pub(crate) fn record_active_route(route: Route, cx: &mut App) {
    if cx.has_global::<AppRuntime>() {
        cx.update_global::<AppRuntime, _>(|runtime, _| {
            runtime.active_route = route;
        });
    }
}

/// Marks the upcoming window close as an intentional hide-to-tray triggered by minimizing.
pub(crate) fn mark_hiding_to_tray(cx: &mut App) {
    if cx.has_global::<AppRuntime>() {
        cx.update_global::<AppRuntime, _>(|runtime, _| {
            runtime.hiding_to_tray = true;
        });
    }
}

/// Updates localized tray menu strings and tooltip after a live language change.
pub(crate) fn refresh_localized_shell_chrome(cx: &mut App) {
    if cx.has_global::<AppRuntime>()
        && let Some(tray) = cx.global::<AppRuntime>().tray.as_ref()
    {
        tray.update_locale();
    }
}

/// Persists the current window bounds into `AppSettings::window_geometry`.
pub(crate) fn persist_window_geometry(store: &Entity<UiStore>, window: &Window, cx: &mut App) {
    let bounds = window.window_bounds();
    let maximized = matches!(bounds, WindowBounds::Maximized(_));
    let rect = bounds.get_bounds();
    let width = f32::from(rect.size.width).round().max(MIN_WINDOW_WIDTH) as u32;
    let height = f32::from(rect.size.height).round().max(MIN_WINDOW_HEIGHT) as u32;
    let geometry = WindowGeometry {
        x: f32::from(rect.origin.x).round() as i32,
        y: f32::from(rect.origin.y).round() as i32,
        width,
        height,
        maximized,
    };
    store.update(cx, |store, cx| {
        let _ = store.update_settings(|s| s.window_geometry = Some(geometry), cx);
    });
}

/// Applies native Windows Always-on-Top (`HWND_TOPMOST` / `HWND_NOTOPMOST`) to Loom's window.
#[cfg(windows)]
pub(crate) fn apply_always_on_top(enabled: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos,
    };

    if let Some(hwnd) = find_main_window_hwnd() {
        let insert_after = if enabled {
            HWND_TOPMOST
        } else {
            HWND_NOTOPMOST
        };
        // SAFETY: `hwnd` is a live top-level window belonging to the current process.
        unsafe {
            SetWindowPos(
                hwnd,
                insert_after,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

/// No-op placeholder on non-Windows platforms until macOS/Linux window z-order adapters ship.
#[cfg(not(windows))]
pub(crate) fn apply_always_on_top(_enabled: bool) {}

/// Returns `true` when Loom's native main window is currently minimized (`IsIconic`).
#[cfg(windows)]
pub(crate) fn is_main_window_minimized() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::IsIconic;

    find_main_window_hwnd().is_some_and(|hwnd| {
        // SAFETY: `hwnd` is owned by the current process.
        unsafe { IsIconic(hwnd) != 0 }
    })
}

/// Returns `false` on non-Windows platforms.
#[cfg(not(windows))]
pub(crate) fn is_main_window_minimized() -> bool {
    false
}

#[cfg(windows)]
fn restore_native_window_if_minimized() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IsIconic, SW_RESTORE, SetForegroundWindow, ShowWindow,
    };

    if let Some(hwnd) = find_main_window_hwnd() {
        // SAFETY: `hwnd` is a top-level window owned by the current process.
        unsafe {
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            }
            SetForegroundWindow(hwnd);
        }
    }
}

#[cfg(not(windows))]
fn restore_native_window_if_minimized() {}

/// Locates the current process's primary top-level visible Win32 window (`HWND`).
#[cfg(windows)]
fn find_main_window_hwnd() -> Option<windows_sys::Win32::Foundation::HWND> {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GW_OWNER, GetWindow, GetWindowTextLengthW, GetWindowThreadProcessId,
        IsWindowVisible,
    };
    use windows_sys::core::BOOL;

    struct SearchCtx {
        pid: u32,
        found: HWND,
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` points to the stack-local `SearchCtx` for the duration of `EnumWindows`.
        let ctx = unsafe { &mut *(lparam as *mut SearchCtx) };
        let mut win_pid: u32 = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut win_pid);
            if win_pid == ctx.pid
                && IsWindowVisible(hwnd) != 0
                && GetWindow(hwnd, GW_OWNER).is_null()
                && GetWindowTextLengthW(hwnd) > 0
            {
                ctx.found = hwnd;
                return 0; // stop enumeration
            }
        }
        1
    }

    let mut ctx = SearchCtx {
        pid: std::process::id(),
        found: std::ptr::null_mut(),
    };
    // SAFETY: `enum_proc` only reads/writes `ctx` while `EnumWindows` executes synchronously.
    unsafe {
        EnumWindows(Some(enum_proc), (&mut ctx as *mut SearchCtx) as LPARAM);
    }

    if ctx.found.is_null() {
        None
    } else {
        Some(ctx.found)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::shell::Route;

    use super::LaunchArgs;

    #[test]
    fn launch_args_parses_restore_route_config_and_allow_multiple() {
        let args = LaunchArgs::parse_from([
            "--restore-route=details",
            "--config=C:\\temp\\loom-test.toml",
            "--allow-multiple",
        ]);
        assert_eq!(args.restore_route, Some(Route::Details));
        assert_eq!(
            args.config_path,
            Some(PathBuf::from("C:\\temp\\loom-test.toml"))
        );
        assert!(args.allow_multiple);

        let split_args = LaunchArgs::parse_from([
            "--restore-route",
            "settings",
            "--config",
            "/tmp/settings.toml",
        ]);
        assert_eq!(split_args.restore_route, Some(Route::Settings));
        assert_eq!(
            split_args.config_path,
            Some(PathBuf::from("/tmp/settings.toml"))
        );
        assert!(!split_args.allow_multiple);

        let empty = LaunchArgs::parse_from(Vec::<&str>::new());
        assert_eq!(empty, LaunchArgs::default());
    }
}
