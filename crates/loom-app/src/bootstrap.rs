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
    Global, QuitMode, SharedString, Subscription, Window, WindowBounds, WindowDecorations,
    WindowKind, WindowOptions, point, px, size,
};
use loom_config::{AppSettings, ConfigStore, WindowGeometry};
use loom_engine::{EngineCommand, EngineCoordinator};
use loom_pal::{
    GlobalHotkeyController, NativeBackend, PalError, PlatformBackend, SingleInstanceGuard,
};
use loom_types::{CapabilityFlags, ProcessKey};
use parking_lot::Mutex;

use crate::bridge::{UiStore, apply_theme};
use crate::i18n;
use crate::shell::{
    AppShell, NavigationIntent, Route, TRAY_OVERVIEW_HEIGHT, TRAY_OVERVIEW_WIDTH, TrayAction,
    TrayController, TrayEventSignal, TrayOverviewView,
};
use crate::startup_trace::{self, StartupTraceTarget};

icon_assets!(
    LoomExtraIcons,
    [
        Activity,
        CircleQuestionMark,
        Download,
        FileLock,
        LayoutList,
        List,
        ListTree,
    ]
);

/// Application asset source combining `gpui-kit`'s default component bundle with
/// Loom's additional shell & view-mode icons.
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

#[derive(Default)]
struct ActivationSignalState {
    pending: Vec<Vec<String>>,
    waker: Option<Waker>,
    closed: bool,
}

/// Zero-polling waker-backed signal delivering single-instance activation payloads onto the GPUI loop.
#[derive(Clone, Default)]
struct ActivationWakeSignal {
    inner: Arc<Mutex<ActivationSignalState>>,
}

impl ActivationWakeSignal {
    fn push(&self, args: Vec<String>) {
        let waker = {
            let mut state = self.inner.lock();
            if state.closed {
                return;
            }
            if state.pending.len() < 16 {
                state.pending.push(args);
            }
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

    fn next(&self) -> NextActivationWake {
        NextActivationWake {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct NextActivationWake {
    inner: Arc<Mutex<ActivationSignalState>>,
}

impl Future for NextActivationWake {
    type Output = Option<Vec<String>>;

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let mut state = self.inner.lock();
        if !state.pending.is_empty() {
            return Poll::Ready(Some(state.pending.remove(0)));
        }
        if state.closed {
            return Poll::Ready(None);
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

/// Application-wide GPUI global managing the active windows, tray icon, global hotkey, and exit policy.
struct AppRuntime {
    store: Entity<UiStore>,
    active_window: Option<AnyWindowHandle>,
    active_shell: Option<Entity<AppShell>>,
    main_window_visible: bool,
    tray_overview_window: Option<AnyWindowHandle>,
    active_route: Route,
    pending_details_highlight: Option<ProcessKey>,
    hiding_to_tray: bool,
    explicit_exit: bool,
    post_frame_initialized: bool,
    initial_hotkey: Option<String>,
    tray: Option<TrayController>,
    hotkey: Option<GlobalHotkeyController>,
    hotkey_signal: Option<HotkeyWakeSignal>,
    activation_signal: Option<ActivationWakeSignal>,
    _window_closed_sub: Option<Subscription>,
    single_instance: Option<SingleInstanceGuard>,
}

impl Global for AppRuntime {}

/// Launches the Loom desktop application using the progressive startup pipeline.
pub(crate) fn run(args: LaunchArgs, raw_args: Vec<String>) {
    // 1. Optional cross-UIPI single-instance mutex lock (bypassed when `--allow-multiple` is set).
    //    Pass `on_activate = None` here so we acquire only the named mutex without creating a
    //    hidden receiver window or thread on the cold-start critical path; the `WM_COPYDATA`
    //    receiver starts non-blockingly once the first UI frame paints.
    let activation_signal = ActivationWakeSignal::default();
    let single_instance = if args.allow_multiple {
        None
    } else {
        match SingleInstanceGuard::acquire_or_notify(SINGLE_INSTANCE_ID, &raw_args, None) {
            Ok(guard) if guard.is_single() => Some(guard),
            Ok(_) => {
                tracing::info!(
                    "another Loom instance is already running; forwarded activation and exiting"
                );
                return;
            }
            Err(err) => {
                tracing::warn!(%err, "single-instance lock failed; continuing startup");
                None
            }
        }
    };
    startup_trace::mark("single_instance_lock");

    // If this is a fresh cold start (no existing primary instance) and persistent administrator
    // startup is enabled on this account, hand off to the elevated scheduled-task broker before
    // initializing GPUI or the sampling engine.
    if !args.allow_multiple && loom_pal::admin::route(&raw_args) {
        drop(single_instance);
        return;
    }

    // Queue any Task Manager replacement readiness handshake passed to this fresh primary instance.
    let _ = loom_pal::taskmgr::queue_handshake_args(&raw_args);

    // 2. Synchronous, sub-millisecond settings load (< 1 ms).
    let config_store = match args.config_path {
        Some(path) => ConfigStore::new(path),
        None => ConfigStore::default_store(),
    };
    let (settings, load_err) = config_store.load_or_default();
    if let Some(err) = load_err {
        tracing::warn!(%err, path = %config_store.path().display(), "corrupt settings.toml; fell back to defaults");
    }
    startup_trace::mark("config_loaded");

    // 3. Apply initial localization immediately before creating UI or tray items.
    i18n::apply_locale(settings.language);

    let initial_route = args
        .restore_route
        .unwrap_or_else(|| Route::from_startup_page(settings.startup_page));

    // 4. Spawn EngineCoordinator BEFORE opening the window so sampler construction, slow-pool
    //    creation, and Tick 0 execute on `loom-fast-sampler` in parallel with GPUI/DirectX init!
    //    Enable MonitorDomain::Processes lease gating so switching away from the Processes
    //    tab (with no tray quick-overview open) skips per-tick Processes table projection.
    let engine_config = loom_engine::EngineConfig {
        gate_processes_by_lease: true,
        project_processes_on_boot: initial_route == Route::Processes,
        ..loom_engine::EngineConfig::default()
    };
    let coordinator = match NativeBackend::initialize().and_then(|backend| {
        EngineCoordinator::with_config(backend, settings.clone(), engine_config)
    }) {
        Ok(coord) => Arc::new(coord),
        Err(err) => {
            tracing::error!(%err, "failed to initialize Loom native engine");
            std::process::exit(1);
        }
    };
    startup_trace::mark("engine_spawned");

    // 5. Launch GPUI Kit application event loop and open the main window.
    //    Use `QuitMode::Explicit` so closing the window with `close_to_tray` / `minimize_to_tray`
    //    enabled destroys the GPUI window (0% GPU) while keeping the tray icon alive.
    gpui_kit::application()
        .with_assets(LoomAssets)
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx| {
            startup_trace::mark("gpui_app_ready");
            gpui_kit::init(cx);
            startup_trace::mark("gpui_kit_init");

            let initial_hotkey = settings.global_hotkey.clone();
            let store = cx.new(|cx| UiStore::new(coordinator, config_store, settings, cx));
            startup_trace::mark("store_created");

            cx.set_global(AppRuntime {
                store,
                active_window: None,
                active_shell: None,
                main_window_visible: false,
                tray_overview_window: None,
                active_route: initial_route,
                pending_details_highlight: None,
                hiding_to_tray: false,
                explicit_exit: false,
                post_frame_initialized: false,
                initial_hotkey,
                tray: None,
                hotkey: None,
                hotkey_signal: None,
                activation_signal: Some(activation_signal),
                _window_closed_sub: None,
                single_instance,
            });

            let closed_sub = cx.on_window_closed(|cx, closed_id| {
                handle_window_closed(cx, closed_id);
            });
            cx.update_global::<AppRuntime, _>(|runtime, _| {
                runtime._window_closed_sub = Some(closed_sub);
            });

            // Open the main window first (applying theme/accent once with `Some(window)`).
            // Single-instance receiver, Explorer tray icon, and global hotkey are deferred
            // strictly past first-frame paint via `window.on_next_frame` inside `open_main_window`.
            if let Err(err) = open_main_window(cx) {
                tracing::error!(%err, "failed to open Loom main window");
                cx.quit();
                return;
            }
            startup_trace::mark("main_window_opened");

            // Complete any pending startup Task Manager readiness handshake and acknowledge any
            // persistent administrator broker waiting for this window to appear.
            complete_taskmgr_handshakes(false, cx);
            loom_pal::admin::acknowledge(&raw_args);
        });
}

/// Completes non-critical desktop shell integrations strictly after the first frame has painted:
/// starts the deferred cross-UIPI `WM_COPYDATA` receiver, registers the system tray icon, and
/// registers any configured global wakeup hotkey.
fn initialize_post_frame_shell(cx: &mut App) {
    if !cx.has_global::<AppRuntime>() {
        return;
    }
    let (activation_signal, initial_hotkey) = {
        let runtime = cx.global_mut::<AppRuntime>();
        if runtime.explicit_exit || runtime.post_frame_initialized {
            return;
        }
        runtime.post_frame_initialized = true;
        let activation_signal = runtime.activation_signal.clone();
        if let (Some(guard), Some(wake_signal)) =
            (runtime.single_instance.as_mut(), activation_signal.clone())
            && let Err(err) = guard.start_activation_receiver(Arc::new(move |forwarded_args| {
                wake_signal.push(forwarded_args);
            }))
        {
            tracing::warn!(%err, "failed to start deferred single-instance activation receiver");
        }
        (activation_signal, runtime.initial_hotkey.take())
    };
    startup_trace::mark("post_frame_receiver_started");

    let (tray, tray_signal) = match TrayController::new() {
        Ok((controller, signal)) => (Some(controller), Some(signal)),
        Err(err) => {
            tracing::warn!(%err, "failed to initialize system tray icon");
            (None, None)
        }
    };
    startup_trace::mark("post_frame_tray_ready");

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
    startup_trace::mark("post_frame_hotkey_ready");

    cx.update_global::<AppRuntime, _>(|runtime, _| {
        runtime.tray = tray;
        runtime.hotkey = Some(hotkey);
        runtime.hotkey_signal = Some(hotkey_signal.clone());
    });

    if let Some(signal) = tray_signal {
        spawn_tray_event_loop(signal, cx);
    }
    spawn_hotkey_event_loop(hotkey_signal, cx);
    if let Some(signal) = activation_signal {
        spawn_activation_event_loop(signal, cx);
    }
}

fn spawn_tray_event_loop(signal: TrayEventSignal, cx: &mut App) {
    cx.spawn(async move |cx| {
        while let Some(action) = signal.next().await {
            let should_continue = cx.update(|cx| match action {
                TrayAction::QuickOverview => {
                    toggle_tray_overview_window(cx);
                    true
                }
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

fn spawn_activation_event_loop(signal: ActivationWakeSignal, cx: &mut App) {
    cx.spawn(async move |cx| {
        while let Some(args) = signal.next().await {
            cx.update(|cx| {
                let is_taskmgr_handshake = loom_pal::taskmgr::queue_handshake_args(&args);
                if is_taskmgr_handshake {
                    complete_taskmgr_handshakes(true, cx);
                } else {
                    show_or_reopen_main_window(cx);
                }
            });
        }
    })
    .detach();
}

fn complete_taskmgr_handshakes(wake_existing_window: bool, cx: &mut App) {
    let completions = loom_pal::taskmgr::complete_pending_handshakes();
    for completion in completions {
        if completion.should_show() {
            if wake_existing_window {
                show_or_reopen_main_window(cx);
            } else {
                loom_pal::window::restore_and_focus_main_window();
            }
        }
        completion.acknowledge();
    }
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

fn build_tray_overview_window_options(cx: &App) -> WindowOptions {
    let margin_x = px(12.0);
    let margin_bottom = px(12.0);
    let win_size = size(px(TRAY_OVERVIEW_WIDTH), px(TRAY_OVERVIEW_HEIGHT));

    let origin = if let Some(display) = cx.primary_display() {
        let visible = display.visible_bounds();
        let x = (visible.right() - win_size.width - margin_x).max(visible.left());
        let y = (visible.bottom() - win_size.height - margin_bottom).max(visible.top());
        point(x, y)
    } else {
        point(px(640.0), px(220.0))
    };

    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(origin, win_size))),
        titlebar: None,
        focus: true,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_decorations: Some(WindowDecorations::Client),
        ..Default::default()
    }
}

fn sync_dormancy(cx: &mut App) {
    if !cx.has_global::<AppRuntime>() {
        return;
    }
    let (store, should_be_dormant) = {
        let runtime = cx.global::<AppRuntime>();
        let any_active_surface = (runtime.active_window.is_some() && runtime.main_window_visible)
            || runtime.tray_overview_window.is_some();
        (runtime.store.clone(), !any_active_surface)
    };
    store.update(cx, |store, cx| {
        store.set_window_dormant(should_be_dormant, cx);
    });
}

fn close_tray_overview_window(cx: &mut App) {
    if !cx.has_global::<AppRuntime>() {
        return;
    }
    let existing =
        cx.update_global::<AppRuntime, _>(|runtime, _| runtime.tray_overview_window.take());
    if let Some(handle) = existing {
        let _ = handle.update(cx, |_, window, _| {
            window.remove_window();
        });
    }
}

fn toggle_tray_overview_window(cx: &mut App) {
    if !cx.has_global::<AppRuntime>() {
        return;
    }
    if cx.global::<AppRuntime>().tray_overview_window.is_some() {
        close_tray_overview_window(cx);
        sync_dormancy(cx);
        return;
    }

    let (store, theme, accent_color) = {
        let runtime = cx.global::<AppRuntime>();
        let store = runtime.store.clone();
        let settings = store.read(cx).settings();
        (store, settings.theme, settings.accent_color)
    };

    // Wake sampling before opening the popover so vitals & top processes update immediately.
    store.update(cx, |store, cx| {
        store.set_window_dormant(false, cx);
        let _ = store.dispatch_command(EngineCommand::ForceRefresh);
    });

    let options = build_tray_overview_window_options(cx);
    match gpui_kit::open_window(options, cx, move |window, cx| {
        apply_theme(theme, accent_color, Some(window), cx);
        cx.new(|cx| TrayOverviewView::new(store, window, cx))
    }) {
        Ok((handle, _view)) => {
            cx.update_global::<AppRuntime, _>(|runtime, _| {
                runtime.tray_overview_window = Some(handle);
            });
        }
        Err(err) => {
            tracing::error!(%err, "failed to open tray quick overview popover");
            sync_dormancy(cx);
        }
    }
}

fn open_main_window(cx: &mut App) -> gpui_kit::Result<()> {
    let (store, route, pending_highlight, settings) = {
        let runtime = cx.global_mut::<AppRuntime>();
        let store = runtime.store.clone();
        let route = runtime.active_route;
        let pending_highlight = runtime.pending_details_highlight.take();
        let settings = store.read(cx).settings().clone();
        (store, route, pending_highlight, settings)
    };

    let options = build_window_options(&settings);
    let needs_post_frame = !cx.global::<AppRuntime>().post_frame_initialized;
    let (window_handle, shell) = gpui_kit::open_window(options, cx, move |window, cx| {
        let initial_system_accent =
            apply_theme(settings.theme, settings.accent_color, Some(window), cx);
        if needs_post_frame {
            // Two-hop `on_next_frame`: first hop runs at Frame 0 start, second hop runs strictly
            // after Frame 0 has completed layout, paint, and swapchain presentation.
            window.on_next_frame(move |window, _cx| {
                startup_trace::mark("first_frame_begin");
                window.on_next_frame(move |_window, cx| {
                    startup_trace::mark("first_frame_painted");
                    initialize_post_frame_shell(cx);
                });
            });
        }
        cx.new(|cx| {
            AppShell::new_with_highlight(
                store,
                route,
                pending_highlight,
                initial_system_accent,
                window,
                cx,
            )
        })
    })?;

    cx.update_global::<AppRuntime, _>(|runtime, _| {
        runtime.active_window = Some(window_handle);
        runtime.active_shell = Some(shell);
        runtime.main_window_visible = true;
        runtime.hiding_to_tray = false;
    });

    Ok(())
}

fn show_or_reopen_main_window(cx: &mut App) {
    close_tray_overview_window(cx);

    let existing = cx.global::<AppRuntime>().active_window;
    if let Some(handle) = existing {
        loom_pal::window::restore_and_focus_main_window();
        let updated = handle.update(cx, |_, window, _| {
            window.activate_window();
        });
        if updated.is_ok() {
            cx.update_global::<AppRuntime, _>(|runtime, _| {
                runtime.main_window_visible = true;
            });
            sync_dormancy(cx);
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

/// Restores or reopens the main Loom window from the tray Quick Overview popover,
/// optionally navigating directly to `Details` highlighted at `jump_key`.
pub(crate) fn open_main_window_from_tray_overview(jump_key: Option<ProcessKey>, cx: &mut App) {
    if !cx.has_global::<AppRuntime>() {
        return;
    }

    close_tray_overview_window(cx);

    if let Some(key) = jump_key {
        let existing = {
            let runtime = cx.global::<AppRuntime>();
            runtime.active_window.zip(runtime.active_shell.clone())
        };
        if let Some((handle, shell)) = existing {
            loom_pal::window::restore_and_focus_main_window();
            let updated = handle.update(cx, |_, window, cx| {
                window.activate_window();
                shell.update(cx, |shell, cx| {
                    shell.handle_external_intent(NavigationIntent::JumpToDetails(key), window, cx);
                });
            });
            if updated.is_ok() {
                cx.update_global::<AppRuntime, _>(|runtime, _| {
                    runtime.main_window_visible = true;
                });
                sync_dormancy(cx);
                return;
            }
        }

        cx.update_global::<AppRuntime, _>(|runtime, _| {
            runtime.active_route = Route::Details;
            runtime.pending_details_highlight = Some(key);
        });
    }

    show_or_reopen_main_window(cx);
}

fn handle_window_closed(cx: &mut App, closed_id: gpui_kit::WindowId) {
    if !cx.has_global::<AppRuntime>() {
        return;
    }

    // First check if the closed window was the lightweight tray Quick Overview popover.
    let closed_tray_overview = cx.update_global::<AppRuntime, _>(|runtime, _| {
        let matched = runtime
            .tray_overview_window
            .is_some_and(|handle| handle.window_id() == closed_id);
        if matched {
            runtime.tray_overview_window = None;
        }
        matched
    });
    if closed_tray_overview {
        tracing::debug!("tray quick overview popover destroyed (0% GPU)");
        sync_dormancy(cx);
        return;
    }

    let (matched, stay_in_tray) = {
        let runtime = cx.global_mut::<AppRuntime>();
        let matched = runtime
            .active_window
            .is_some_and(|handle| handle.window_id() == closed_id);
        if !matched {
            return;
        }
        runtime.active_window = None;
        runtime.active_shell = None;
        runtime.main_window_visible = false;
        let hiding = std::mem::take(&mut runtime.hiding_to_tray);
        let explicit_exit = runtime.explicit_exit;
        let has_tray = runtime.tray.is_some();
        let store = runtime.store.clone();
        let close_to_tray = store.read(cx).settings().close_to_tray;
        let stay_in_tray = !explicit_exit && has_tray && (close_to_tray || hiding);
        (matched, stay_in_tray)
    };

    if !matched {
        return;
    }

    if stay_in_tray {
        tracing::info!("Loom window closed to system tray (0% GPU dormant mode)");
        sync_dormancy(cx);
    } else {
        quit_application(cx);
    }
}

/// Exits the current Loom instance cleanly so a prepared privilege-switch broker can take over.
pub(crate) fn quit_for_privilege_switch(cx: &mut App) {
    quit_application(cx);
}

fn quit_application(cx: &mut App) {
    if !cx.has_global::<AppRuntime>() {
        cx.quit();
        return;
    }

    let (store, active_window, tray_overview_window) = {
        let runtime = cx.global_mut::<AppRuntime>();
        runtime.explicit_exit = true;
        if let Some(signal) = runtime.hotkey_signal.take() {
            signal.close();
        }
        if let Some(signal) = runtime.activation_signal.take() {
            signal.close();
        }
        runtime.hotkey = None;
        runtime.tray = None;
        runtime.single_instance = None;
        runtime.active_shell = None;
        runtime.main_window_visible = false;
        (
            runtime.store.clone(),
            runtime.active_window.take(),
            runtime.tray_overview_window.take(),
        )
    };

    if let Some(handle) = tray_overview_window {
        let _ = handle.update(cx, |_, window, _| {
            window.remove_window();
        });
    }

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

/// Updates the main window visibility state and reconciles engine dormancy across all shell windows.
///
/// Returns `true` when [`AppRuntime`] handled dormancy orchestration, or `false` in standalone tests.
pub(crate) fn set_main_window_visible(visible: bool, cx: &mut App) -> bool {
    if cx.has_global::<AppRuntime>() {
        cx.update_global::<AppRuntime, _>(|runtime, _| {
            runtime.main_window_visible = visible;
        });
        sync_dormancy(cx);
        true
    } else {
        false
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
    use crate::startup_trace::StartupTraceTarget;

    use super::LaunchArgs;

    #[test]
    fn launch_args_parses_restore_route_config_and_allow_multiple() {
        // Inline `--flag=value` syntax with a POSIX-style path.
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

        // Split `--flag value` syntax with a Windows-style path and bare `--startup-trace`.
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

        // Split `--startup-trace <path>` syntax.
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

    #[test]
    fn loom_assets_loads_all_extra_and_feature_icons() {
        use gpui_kit::AssetSource as _;
        use gpui_kit::assets::IconName as AssetIconName;

        let icons = [
            AssetIconName::Activity,
            AssetIconName::CircleQuestionMark,
            AssetIconName::Copy,
            AssetIconName::Download,
            AssetIconName::EyeOff,
            AssetIconName::FileLock,
            AssetIconName::LayoutList,
            AssetIconName::List,
            AssetIconName::ListTree,
            AssetIconName::RotateCw,
        ];

        for icon in icons {
            let path = icon.path();
            let loaded = super::LoomAssets
                .load(path.as_ref())
                .expect("asset load should not error");
            assert!(
                loaded.as_ref().is_some_and(|bytes| !bytes.is_empty()),
                "icon `{path}` must resolve from LoomAssets"
            );
        }
    }
}
