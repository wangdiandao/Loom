//! Shell-scoped dialogs shared across multiple pages.
//!
//! Feature-specific dialogs (such as `Details`'s `SetAffinityDialog` or wait-chain tree)
//! MUST live inside their own `features/*` slice and mount via `gpui-kit`'s `Root` dialog
//! layer. Only truly cross-page shell dialogs (`RunNewTaskDialog`, high-risk process
//! termination confirmation, memory minidump progress, and file-lock finder) belong here.

mod confirm_end_task;
mod file_locks;
mod minidump;
mod run_new_task;

use gpui_kit::component::{Icon, IconName, Sizable as _, h_flex};
use gpui_kit::{Hsla, IntoElement, ParentElement as _, Styled as _, div};

pub(crate) use confirm_end_task::open_confirm_end_task_dialog;
pub(crate) use file_locks::open_file_locks_dialog;
pub(crate) use minidump::open_minidump_dialog;
pub(crate) use run_new_task::open_run_new_task_dialog;

/// Renders a compact inline form validation error row shared across shell dialogs.
pub(super) fn validation_error_row(message: String, danger: Hsla) -> impl IntoElement {
    h_flex()
        .items_center()
        .gap_1p5()
        .text_xs()
        .text_color(danger)
        .child(Icon::new(IconName::CircleAlert).xsmall().text_color(danger))
        .child(div().flex_1().min_w_0().child(message))
}
