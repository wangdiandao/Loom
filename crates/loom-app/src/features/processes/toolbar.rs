//! Page toolbar composition for [`super::ProcessesView`].

use gpui_kit::assets::IconName as AssetIconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex};
use gpui_kit::{
    Context, IntoElement, ParentElement as _, Styled as _, div, prelude::FluentBuilder as _,
};
use loom_types::ProcessViewMode;

use crate::bridge::UiCommands;
use crate::features::table_columns::{dispatch_hide_system_processes, render_ime_preedit_badge};
use crate::i18n;
use crate::shell::dialogs;

use super::ProcessesView;

/// Renders the static top toolbar for [`ProcessesView`].
pub(super) fn render_processes_toolbar(
    view: &ProcessesView,
    cx: &mut Context<ProcessesView>,
) -> impl IntoElement + use<> {
    let view_mode = view.view_mode;
    let hide_system = view.hide_system_processes;
    let preedit_badge: Option<String> = view
        .type_to_search
        .preedit_text()
        .map(|pre| format!("{}: {pre}", i18n::tr("proc_ux_ime_searching")));

    let theme = cx.theme();
    let border = theme.border;
    let fg = theme.foreground;
    let primary = theme.primary;
    let muted_bg = theme.muted;
    let radius = theme.radius;

    let store_grouped = view.store.clone();
    let store_tree = view.store.clone();
    let store_hide_sys = view.store.clone();
    let store_run_task = view.store.clone();

    let grouped_label = i18n::tr("processes.view_grouped");
    let tree_label = i18n::tr("processes.view_tree");
    let hide_sys_label = i18n::tr("proc_ux_hide_system_tooltip");
    let export_csv_label = i18n::tr("csv_export_button");

    h_flex()
        .w_full()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_3()
        .px_4()
        .py_2p5()
        .border_b_1()
        .border_color(border)
        .child(
            h_flex()
                .flex_wrap()
                .items_center()
                .gap_2p5()
                .child(
                    div()
                        .text_lg()
                        .font_semibold()
                        .text_color(fg)
                        .child(i18n::tr("processes.title")),
                )
                .child(
                    h_flex()
                        .items_center()
                        .gap_1()
                        .p_0p5()
                        .rounded(radius)
                        .bg(muted_bg.opacity(0.65))
                        .child(
                            Button::new("processes-mode-grouped")
                                .xsmall()
                                .icon(Icon::new(AssetIconName::LayoutList))
                                .tooltip(grouped_label.clone())
                                .accessibility_label(grouped_label)
                                .map(|btn| {
                                    if view_mode == ProcessViewMode::Grouped {
                                        btn.primary()
                                    } else {
                                        btn.ghost()
                                    }
                                })
                                .on_click(move |_, _, cx| {
                                    UiCommands::set_process_view_mode(
                                        &store_grouped,
                                        ProcessViewMode::Grouped,
                                        cx,
                                    );
                                }),
                        )
                        .child(
                            Button::new("processes-mode-tree")
                                .xsmall()
                                .icon(Icon::new(AssetIconName::ListTree))
                                .tooltip(tree_label.clone())
                                .accessibility_label(tree_label)
                                .map(|btn| {
                                    if view_mode == ProcessViewMode::Tree {
                                        btn.primary()
                                    } else {
                                        btn.ghost()
                                    }
                                })
                                .on_click(move |_, _, cx| {
                                    UiCommands::set_process_view_mode(
                                        &store_tree,
                                        ProcessViewMode::Tree,
                                        cx,
                                    );
                                }),
                        ),
                )
                .child(
                    Button::new("processes-hide-system-toggle")
                        .xsmall()
                        .icon(Icon::new(AssetIconName::EyeOff))
                        .tooltip(hide_sys_label.clone())
                        .accessibility_label(hide_sys_label)
                        .map(|btn| {
                            if hide_system {
                                btn.primary()
                            } else {
                                btn.ghost()
                            }
                        })
                        .on_click(move |_, _, cx| {
                            dispatch_hide_system_processes(&store_hide_sys, !hide_system, cx);
                        }),
                )
                .when_some(preedit_badge, |el, text| {
                    el.child(render_ime_preedit_badge(text, primary, radius))
                }),
        )
        .child(
            h_flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .flex_shrink_0()
                .child(
                    Button::new("processes-export-csv")
                        .small()
                        .outline()
                        .icon(Icon::new(AssetIconName::Download))
                        .tooltip(export_csv_label.clone())
                        .accessibility_label(export_csv_label)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.trigger_export_csv(window, cx);
                        })),
                )
                .child(
                    Button::new("processes-run-new-task")
                        .small()
                        .primary()
                        .icon(IconName::Play)
                        .label(i18n::tr("processes.run_new_task"))
                        .on_click(move |_, window, cx| {
                            dialogs::open_run_new_task_dialog(store_run_task.clone(), window, cx);
                        }),
                )
                .child(view.end_task_button.clone()),
        )
}
