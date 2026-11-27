//! Progressive startup pipeline, window lifecycle orchestration, tray residency, and PAL window helpers.

use std::borrow::Cow;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll, Waker};

use gpui_kit::assets::{Assets, icon_assets};
use gpui_kit::component::TitleBar;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, AssetSource, BorrowAppContext as _, Bounds, Entity,
    Global, QuitMode, SharedString, Subscription, Window, WindowBounds, WindowOptions, point, px,
    size,
};
use loom_config::{AppSettings, ConfigStore, WindowGeometry};
use loom_engine::{EngineCommand, EngineCoordinator};
use loom_pal::{
    GlobalHotkeyController, NativeBackend, PalError, PlatformBackend, SingleInstanceGuard,
};
use loom_types::CapabilityFlags;
use parking_lot::Mutex;

use crate::bridge::{UiStore, apply_theme};
use crate::i18n;
use crate::shell::{AppShell, Route, TrayAction, TrayController, TrayEventSignal};

icon_assets!(LoomExtraIcons, [LayoutList, List, ListTree]);

/// Application asset source combining `gpui-kit`'s default component bundle with
/// Loom's view-mode switcher icons (`LayoutList`, `List`, `ListTree`).
struct LoomAssets;

impl AssetSource for LoomAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        if path == loom_ui_kit::ECO_LEAF_ICON_PATH {
            return Ok(Some(Cow::Borrowed(loom_ui_kit::ECO_LEAF_SVG)));
        }
        if let Ok(Some(data)) = LoomExtraIcons.load(path) {
            return Ok(Some(data));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut items = Assets.list(path)?;
        items.extend(LoomExtraIcons.list(path)?);
        if path.is_empty() || path == "icons" || path == "icons/" {
            items.push(SharedString::new_static(loom_ui_kit::ECO_LEAF_ICON_PATH));
        }
        Ok(items)
    }
}

/// Single-instance OS mutex identifier shared across normal launches of Loom.
const SINGLE_INSTANCE_ID: &str = "io.github.wangdiandao.loom.single-instance";

const DEFAULT_WINDOW_WIDTH: f32 = 1120.0;
const DEFAULT_WINDOW_HEIGHT: f32 = 720.0;
const MIN_WINDOW_WIDTH: f32 = 760.0;
const MIN_WINDOW_HEIGHT: f32 = 500.0;

/// Parsed command-line launch arguments for `loom-app`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct LaunchArgs {
    /// Optional route slug requested via `--restore-route=<slug>` (e.g., after elevated relaunch).
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

#[derive(Default)]
struct HotkeySignalState {
    pending: bool,
    waker: Option<Waker>,
    closed: bool,
}

/// Zero-polling waker-backed wakeup signal delivering global hotkey presses onto the GPUI loop.
#[derive(Clone, Default)]
struct HotkeyWakeSignal {
    inner: Arc<Mutex<HotkeySignalState>>,
}

impl HotkeyWakeSignal {
    fn trigger(&self) {
        let waker = {
            let mut state = self.inner.lock();
            if state.closed {
                return;
            }
            state.pending = true;
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn close(&self) {
        let waker = {
            let mut state = self.inner.lock();
            state.closed = true;
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn next(&self) -> NextHotkeyWake {
        NextHotkeyWake {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct NextHotkeyWake {
    inner: Arc<Mutex<HotkeySignalState>>,
}

impl Future for NextHotkeyWake {
    type Output = bool;

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let mut state = self.inner.lock();
        if state.pending {
            state.pending = false;
            return Poll::Ready(true);
        }
        if state.closed {
            return Poll::Ready(false);
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

/// Application-wide GPUI global managing the active window handle, tray icon, global hotkey, and exit policy.
struct AppRuntime {
    store: Entity<UiStore>,
    active_window: Option<AnyWindowHandle>,
    active_route: Route,
    hiding_to_tray: bool,
    explicit_exit: bool,
    tray: Option<TrayController>,
    hotkey: Option<GlobalHotkeyController>,
    hotkey_signal: Option<HotkeyWakeSignal>,
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
        .with_assets(LoomAssets)
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx| {
            gpui_kit::init(cx);
            apply_theme(settings.theme, None, cx);

            let initial_hotkey = settings.global_hotkey.clone();
            let store = cx.new(|cx| UiStore::new(coordinator, config_store, settings, cx));

            cx.set_global(AppRuntime {
                store,
                active_window: None,
                active_route: initial_route,
                hiding_to_tray: false,
                explicit_exit: false,
                tray: None,
                hotkey: None,
                hotkey_signal: None,
                _window_closed_sub: None,
                _single_instance: single_instance,
            });

            let closed_sub = cx.on_window_closed(|cx, closed_id| {
                handle_window_closed(cx, closed_id);
            });
            cx.update_global::<AppRuntime, _>(|runtime, _| {
                runtime._window_closed_sub = Some(closed_sub);
            });

            // Open the main window first before registering Explorer tray icon / global hotkey
            // so Shell_NotifyIconW IPC never delays first-frame window appearance.
            if let Err(err) = open_main_window(cx) {
                tracing::error!(%err, "failed to open Loom main window");
                cx.quit();
                return;
            }

            cx.defer(move |cx| {
                if !cx.has_global::<AppRuntime>() || cx.global::<AppRuntime>().explicit_exit {
                    return;
                }

                let (tray, tray_signal) = match TrayController::new() {
                    Ok((controller, signal)) => (Some(controller), Some(signal)),
                    Err(err) => {
                        tracing::warn!(%err, "failed to initialize system tray icon");
                        (None, None)
                    }
                };

                let hotkey_signal = HotkeyWakeSignal::default();
                let trigger_signal = hotkey_signal.clone();
                let mut hotkey = GlobalHotkeyController::new(Arc::new(move || {
                    trigger_signal.trigger();
                }));
                if NativeBackend::CAPABILITIES.contains(CapabilityFlags::GLOBAL_HOTKEY)
                    && let Some(ref spec) = initial_hotkey
                    && let Err(err) = hotkey.set_hotkey(Some(spec))
                {
                    tracing::warn!(%err, hotkey = %spec, "failed to register configured global wakeup hotkey");
                }

                cx.update_global::<AppRuntime, _>(|runtime, _| {
                    runtime.tray = tray;
                    runtime.hotkey = Some(hotkey);
                    runtime.hotkey_signal = Some(hotkey_signal.clone());
                });

                if let Some(signal) = tray_signal {
                    spawn_tray_event_loop(signal, cx);
                }
                spawn_hotkey_event_loop(hotkey_signal, cx);
            });
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

fn spawn_hotkey_event_loop(signal: HotkeyWakeSignal, cx: &mut App) {
    cx.spawn(async move |cx| {
        while signal.next().await {
            cx.update(|cx| {
                show_or_reopen_main_window(cx);
            });
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
        loom_pal::window::restore_and_focus_main_window();
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
        if let Some(signal) = runtime.hotkey_signal.take() {
            signal.close();
        }
        runtime.hotkey = None;
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

/// Applies native Always-on-Top window z-order via [`loom_pal::window`].
pub(crate) fn apply_always_on_top(enabled: bool) {
    loom_pal::window::set_main_window_always_on_top(enabled);
}

/// Applies or clears the system-wide global wakeup hotkey live via [`GlobalHotkeyController`].
pub(crate) fn apply_global_hotkey(spec: Option<&str>, cx: &mut App) -> Result<(), PalError> {
    if !cx.has_global::<AppRuntime>() {
        return Ok(());
    }
    let runtime = cx.global_mut::<AppRuntime>();
    if let Some(controller) = runtime.hotkey.as_mut() {
        controller.set_hotkey(spec)
    } else {
        Ok(())
    }
}

/// Returns `true` when Loom's native main window is currently minimized.
pub(crate) fn is_main_window_minimized() -> bool {
    loom_pal::window::is_main_window_minimized()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::shell::Route;

    use super::LaunchArgs;

    #[test]
    fn launch_args_parses_restore_route_config_and_allow_multiple() {
        // Inline `--flag=value` syntax with a POSIX-style path.
        let posix_args = LaunchArgs::parse_from([
            "--restore-route=details",
            "--config=/var/lib/loom/settings.toml",
            "--allow-multiple",
        ]);
        assert_eq!(posix_args.restore_route, Some(Route::Details));
        assert_eq!(
            posix_args.config_path,
            Some(PathBuf::from("/var/lib/loom/settings.toml"))
        );
        assert!(posix_args.allow_multiple);

        // Split `--flag value` syntax with a Windows-style path.
        let windows_args = LaunchArgs::parse_from([
            "--restore-route",
            "settings",
            "--config",
            r"C:\ProgramData\Loom\settings.toml",
        ]);
        assert_eq!(windows_args.restore_route, Some(Route::Settings));
        assert_eq!(
            windows_args.config_path,
            Some(PathBuf::from(r"C:\ProgramData\Loom\settings.toml"))
        );
        assert!(!windows_args.allow_multiple);

        let empty = LaunchArgs::parse_from(Vec::<&str>::new());
        assert_eq!(empty, LaunchArgs::default());
    }
}
