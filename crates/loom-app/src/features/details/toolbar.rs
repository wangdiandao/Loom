//! Page toolbar and cross-navigation highlight banner for [`super::DetailsView`].

use gpui_kit::assets::IconName as AssetIconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex};
use gpui_kit::{
    Context, IntoElement, ParentElement as _, Styled as _, div, prelude::FluentBuilder as _,
};
use loom_types::{DetailsViewMode, ProcessKey};

use crate::bridge::UiCommands;
use crate::features::table_columns::render_ime_preedit_badge;
use crate::i18n;
use crate::shell::dialogs;

use super::DetailsView;

/// Renders the static top toolbar for [`DetailsView`].
pub(super) fn render_details_toolbar(
    view: &DetailsView,
    cx: &mut Context<DetailsView>,
) -> impl IntoElement + use<> {
    let view_mode = view.view_mode;
    let preedit_badge: Option<String> = view
        .type_to_search
        .preedit_text()
        .map(|pre| format!("{}: {pre}", i18n::tr("proc_ux_ime_searching")));

    let theme = cx.theme();
    let border = theme.border;
    let fg = theme.foreground;
    let muted_bg = theme.muted;
    let primary = theme.primary;
    let radius = theme.radius;

    let store_flat = view.store.clone();
    let store_tree = view.store.clone();
    let store_run_task = view.store.clone();

    let flat_label = i18n::tr("details.view_flat");
    let tree_label = i18n::tr("details.view_tree");
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
                .items_center()
                .gap_2p5()
                .flex_wrap()
                .child(
                    div()
                        .text_lg()
                        .font_semibold()
                        .text_color(fg)
                        .child(i18n::tr("details.title")),
                )
                .child(
                    h_flex()
                        .items_center()
                        .gap_1()
                        .p_0p5()
                        .rounded(radius)
                        .bg(muted_bg.opacity(0.65))
                        .child(
                            Button::new("details-mode-flat")
                                .xsmall()
                                .icon(Icon::new(AssetIconName::List))
                                .tooltip(flat_label.clone())
                                .accessibility_label(flat_label)
                                .map(|btn| {
                                    if view_mode == DetailsViewMode::Flat {
                                        btn.primary()
                                    } else {
                                        btn.ghost()
                                    }
                                })
                                .on_click(move |_, _, cx| {
                                    UiCommands::set_details_view_mode(
                                        &store_flat,
                                        DetailsViewMode::Flat,
                                        cx,
                                    );
                                }),
                        )
                        .child(
                            Button::new("details-mode-tree")
                                .xsmall()
                                .icon(Icon::new(AssetIconName::ListTree))
                                .tooltip(tree_label.clone())
                                .accessibility_label(tree_label)
                                .map(|btn| {
                                    if view_mode == DetailsViewMode::Tree {
                                        btn.primary()
                                    } else {
                                        btn.ghost()
                                    }
                                })
                                .on_click(move |_, _, cx| {
                                    UiCommands::set_details_view_mode(
                                        &store_tree,
                                        DetailsViewMode::Tree,
                                        cx,
                                    );
                                }),
                        ),
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
                    Button::new("details-export-csv")
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
                    Button::new("details-run-new-task")
                        .small()
                        .primary()
                        .icon(IconName::Play)
                        .label(i18n::tr("details.run_new_task"))
                        .on_click(move |_, window, cx| {
                            dialogs::open_run_new_task_dialog(store_run_task.clone(), window, cx);
                        }),
                )
                .child(view.end_task_button.clone()),
        )
}

/// Renders the optional cross-page jump highlight banner (`Go to details` -> highlighted PID).
pub(super) fn render_highlight_banner(
    key: ProcessKey,
    cx: &mut Context<DetailsView>,
) -> impl IntoElement + use<> {
    let primary = cx.theme().primary;
    h_flex()
        .w_full()
        .items_center()
        .justify_between()
        .px_4()
        .py_1p5()
        .bg(primary.opacity(0.10))
        .border_b_1()
        .border_color(primary.opacity(0.25))
        .child(
            h_flex()
                .items_center()
                .gap_2()
                .text_xs()
                .font_medium()
                .text_color(primary)
                .child(Icon::new(IconName::Search).xsmall().text_color(primary))
                .child(format!(
                    "{}: {}",
                    i18n::tr("details.highlighted_pid"),
                    key.pid
                )),
        )
        .child(
            Button::new("details-clear-highlight")
                .xsmall()
                .ghost()
                .label(i18n::tr("details.clear_highlight"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.set_highlighted_key(None, cx);
                })),
        )
}
