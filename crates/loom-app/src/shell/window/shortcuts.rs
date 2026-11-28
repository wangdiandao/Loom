//! Global desktop keyboard shortcuts and Hold-`Ctrl` temporary freeze handlers for [`AppShell`].

use gpui_kit::component::WindowExt as _;
use gpui_kit::{Context, Focusable as _, KeyDownEvent, Modifiers, ModifiersChangedEvent, Window};

use crate::bridge::UiCommands;
use crate::shell::nav::Route;

use super::AppShell;

/// Maps `Ctrl+1`..`Ctrl+8` digit keys to their corresponding [`Route`].
pub(crate) fn route_for_ctrl_digit(key: &str) -> Option<Route> {
    match key {
        "1" => Some(Route::Processes),
        "2" => Some(Route::Performance),
        "3" => Some(Route::AppHistory),
        "4" => Some(Route::Startup),
        "5" => Some(Route::Users),
        "6" => Some(Route::Details),
        "7" => Some(Route::Services),
        "8" => Some(Route::Settings),
        _ => None,
    }
}

/// Returns `true` when Hold-`Ctrl` temporary freeze should be active for `modifiers`.
#[inline]
pub(crate) const fn should_hold_ctrl_freeze(
    modifiers: Modifiers,
    has_focused_input: bool,
    has_modal_overlay: bool,
) -> bool {
    modifiers.control
        && !modifiers.alt
        && !modifiers.shift
        && !modifiers.platform
        && !has_focused_input
        && !has_modal_overlay
}

impl AppShell {
    pub(super) fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let has_modal = window.has_active_dialog(cx) || window.has_active_sheet(cx);
        if has_modal {
            return;
        }

        let mods = event.keystroke.modifiers;
        let key = event.keystroke.key.as_str();
        let ctrl_only = mods.control && !mods.alt && !mods.shift && !mods.platform;
        let no_mods = !mods.control && !mods.alt && !mods.shift && !mods.platform;
        let input_focused = window.has_focused_input(cx);

        // `Ctrl+F`: focus search box from anywhere in the main shell.
        if ctrl_only && key.eq_ignore_ascii_case("f") {
            self.focus_search_input(window, cx);
            cx.stop_propagation();
            return;
        }

        // `Ctrl+1`..`Ctrl+8`: switch directly to the 8 navigation routes.
        if ctrl_only && let Some(target_route) = route_for_ctrl_digit(key) {
            self.navigate_to(target_route, None, None, window, cx);
            cx.stop_propagation();
            return;
        }

        // `F5`: force immediate sampling refresh even when paused.
        if no_mods && key.eq_ignore_ascii_case("f5") {
            UiCommands::force_refresh(&self.store, cx);
            cx.stop_propagation();
            return;
        }

        // `Esc`: dismiss syntax popover, clear active search query, or blur search input.
        if no_mods && key == "escape" {
            if self.syntax_help_open {
                self.syntax_help_open = false;
                cx.notify();
                cx.stop_propagation();
                return;
            }
            let search_focused = self.search_input.focus_handle(cx).is_focused(window);
            let has_search_text = !self.search_input.read(cx).value().is_empty()
                || !self.store.read(cx).search_query().is_empty();
            if search_focused && has_search_text {
                self.clear_search_query(window, cx);
                cx.stop_propagation();
                return;
            }
            if search_focused {
                self.focus_handle.focus(window, cx);
                cx.stop_propagation();
                return;
            }
            return;
        }

        // Single-key shortcuts (`/`, `Space`, `P`) are suppressed whenever a text Input has focus.
        if input_focused || !no_mods {
            return;
        }

        match key {
            "/" => {
                self.focus_search_input(window, cx);
                cx.stop_propagation();
            }
            "space" | " " | "p" | "P" => {
                UiCommands::toggle_pause_refresh(&self.store, cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    pub(super) fn handle_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let has_modal = window.has_active_dialog(cx) || window.has_active_sheet(cx);
        let input_focused = window.has_focused_input(cx);
        let freeze = should_hold_ctrl_freeze(event.modifiers, input_focused, has_modal);
        let was_frozen = self.store.read(cx).is_temp_frozen();
        UiCommands::set_temp_freeze(&self.store, freeze, cx);
        if was_frozen != freeze {
            cx.notify();
        }
    }
}
