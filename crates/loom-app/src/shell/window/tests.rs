use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::TitleBar;
use gpui_kit::component::sidebar::SidebarToggleButton;
use gpui_kit::{
    AppContext as _, Capslock, Context, Focusable as _, InteractiveElement as _, IntoElement,
    Modifiers, ModifiersChangedEvent, MouseButton, MouseDownEvent, ParentElement as _,
    PlatformInput, Render, TestAppContext, VisualTestContext, Window, div, point, px,
};
use loom_config::{AppSettings, ConfigStore};
use loom_engine::EngineCoordinator;
use loom_pal::{NativeBackend, PlatformBackend as _};
use loom_types::RefreshSpeed;

use crate::bridge::UiStore;
use crate::shell::Route;

use super::AppShell;
use super::shortcuts::{route_for_ctrl_digit, should_hold_ctrl_freeze};

struct TitleBarToggleHarness {
    collapsed: Rc<Cell<bool>>,
}

impl Render for TitleBarToggleHarness {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let collapsed = self.collapsed.get();
        let flag = self.collapsed.clone();
        TitleBar::new().child(
            div()
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .child(
                    SidebarToggleButton::new()
                        .collapsed(collapsed)
                        .accessibility_label("Toggle sidebar")
                        .on_click(move |_, _, _| {
                            flag.set(!collapsed);
                        }),
                ),
        )
    }
}

#[test]
fn ctrl_digit_mapping_and_hold_ctrl_freeze_guards() {
    assert_eq!(route_for_ctrl_digit("1"), Some(Route::Processes));
    assert_eq!(route_for_ctrl_digit("2"), Some(Route::Performance));
    assert_eq!(route_for_ctrl_digit("3"), Some(Route::AppHistory));
    assert_eq!(route_for_ctrl_digit("4"), Some(Route::Startup));
    assert_eq!(route_for_ctrl_digit("5"), Some(Route::Users));
    assert_eq!(route_for_ctrl_digit("6"), Some(Route::Details));
    assert_eq!(route_for_ctrl_digit("7"), Some(Route::Services));
    assert_eq!(route_for_ctrl_digit("8"), Some(Route::Settings));
    assert_eq!(route_for_ctrl_digit("9"), None);

    let ctrl = Modifiers {
        control: true,
        ..Modifiers::default()
    };
    let ctrl_shift = Modifiers {
        control: true,
        shift: true,
        ..Modifiers::default()
    };
    assert!(should_hold_ctrl_freeze(ctrl, false, false));
    assert!(!should_hold_ctrl_freeze(ctrl, true, false));
    assert!(!should_hold_ctrl_freeze(ctrl, false, true));
    assert!(!should_hold_ctrl_freeze(ctrl_shift, false, false));
    assert!(!should_hold_ctrl_freeze(Modifiers::default(), false, false));
}

#[gpui_kit::test]
fn titlebar_sidebar_toggle_stops_mousedown_propagation_and_toggles_on_click(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let collapsed = Rc::new(Cell::new(false));
    let view = cx.new(|_| TitleBarToggleHarness {
        collapsed: collapsed.clone(),
    });
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |window, cx| {
            cx.new(|cx| gpui_kit::base::Root::new(view.clone(), window, cx))
        })
        .unwrap()
    });
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    vcx.update(|window, cx| window.draw(cx).clear(cx));

    // Center of SidebarToggleButton inside TitleBar (left padding 12px + half of 24px = 24px, y = 17px).
    let at = point(px(24.), px(17.));
    let down_result = vcx.update(|window, cx| {
        window.dispatch_event(
            PlatformInput::MouseDown(MouseDownEvent {
                button: MouseButton::Left,
                position: at,
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            }),
            cx,
        )
    });
    assert!(
        !down_result.propagate,
        "left mouse-down on TitleBar SidebarToggleButton must stop propagation so Windows HTCAPTION does not swallow release"
    );

    vcx.simulate_mouse_up(at, MouseButton::Left, Modifiers::default());
    assert!(
        collapsed.get(),
        "clicking SidebarToggleButton must toggle collapsed state"
    );
}

#[gpui_kit::test]
fn shell_hotkeys_route_navigation_pause_search_esc_and_ctrl_freeze(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let dir = tempfile::tempdir().expect("create temp dir");
    let config_store = ConfigStore::new(dir.path().join("settings.toml"));
    let settings = AppSettings {
        refresh_speed: RefreshSpeed::High500Ms,
        ..AppSettings::default()
    };
    let backend = NativeBackend::initialize().expect("init native backend");
    let coordinator =
        Arc::new(EngineCoordinator::new(backend, settings.clone()).expect("start coordinator"));
    let store = cx.new(|cx| UiStore::new(Arc::clone(&coordinator), config_store, settings, cx));

    let mut shell_entity = None;
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |window, cx| {
            let shell = cx.new(|cx| AppShell::new(store.clone(), Route::Processes, window, cx));
            shell_entity = Some(shell.clone());
            cx.new(|cx| gpui_kit::base::Root::new(shell, window, cx))
        })
        .unwrap()
    });
    let shell = shell_entity.expect("AppShell created");
    let mut vcx = VisualTestContext::from_window(window.into(), cx);
    vcx.update(|window, cx| window.draw(cx).clear(cx));

    for (stroke, expected) in [
        ("ctrl-2", Route::Performance),
        ("ctrl-3", Route::AppHistory),
        ("ctrl-4", Route::Startup),
        ("ctrl-5", Route::Users),
        ("ctrl-6", Route::Details),
        ("ctrl-7", Route::Services),
        ("ctrl-8", Route::Settings),
        ("ctrl-1", Route::Processes),
    ] {
        vcx.simulate_keystrokes(stroke);
        vcx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(
            shell.read_with(&vcx, |s, _| s.current_route()),
            expected,
            "shortcut `{stroke}` must navigate to {expected:?}"
        );
    }

    vcx.simulate_keystrokes("p");
    assert_eq!(
        store.read_with(&vcx, |s, _| s.settings().refresh_speed),
        RefreshSpeed::Paused
    );
    vcx.simulate_keystrokes("p");
    assert_eq!(
        store.read_with(&vcx, |s, _| s.settings().refresh_speed),
        RefreshSpeed::High500Ms
    );

    vcx.simulate_keystrokes("f5");
    assert_eq!(
        shell.read_with(&vcx, |s, _| s.current_route()),
        Route::Processes
    );

    vcx.update(|window, cx| {
        window.dispatch_event(
            PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                modifiers: Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
                capslock: Capslock::default(),
            }),
            cx,
        );
    });
    assert!(store.read_with(&vcx, |s, _| s.is_temp_frozen()));

    vcx.update(|window, cx| {
        window.dispatch_event(
            PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                modifiers: Modifiers::default(),
                capslock: Capslock::default(),
            }),
            cx,
        );
    });
    assert!(!store.read_with(&vcx, |s, _| s.is_temp_frozen()));

    vcx.simulate_keystrokes("/");
    vcx.update(|window, cx| {
        window.draw(cx).clear(cx);
        assert!(
            shell
                .read(cx)
                .search_input
                .focus_handle(cx)
                .is_focused(window)
        );
        shell.update(cx, |s, cx| s.apply_search_example("pid:1234", window, cx));
        window.draw(cx).clear(cx);
    });
    assert_eq!(
        store.read_with(&vcx, |s, _| s.search_query().to_owned()),
        "pid:1234"
    );

    vcx.simulate_keystrokes("p");
    assert_eq!(
        store.read_with(&vcx, |s, _| s.settings().refresh_speed),
        RefreshSpeed::High500Ms
    );

    vcx.simulate_keystrokes("escape");
    assert_eq!(
        store.read_with(&vcx, |s, _| s.search_query().to_owned()),
        ""
    );

    coordinator.shutdown();
}
