//! `ProcessKey`-anchored row context menu and termination confirmation dialogs
//! for [`super::DetailsView`].

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::assets::IconName as AssetIconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::TableState;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, Context, Entity, FocusHandle, Focusable as _, ParentElement as _, Styled as _, Window,
    div, prelude::FluentBuilder as _, px,
};
use loom_types::{CapabilityFlags, ProcessStatusFlags, UacVirtualization};

use crate::bridge::{UiCommands, UiStore};
use crate::features::table_columns::{
    CopyRowFields, build_copy_submenu, can_suspend_or_resume_process, dispatch_ignore_leak_path,
    dispatch_restart_explorer, dispatch_suspend_or_resume, is_explorer_image, leak_ignore_target,
};
use crate::i18n;
use crate::shell::{ShellHandle, dialogs};

use super::affinity_dialog;
use super::delegate::DetailsTableDelegate;
use super::state::{
    PRIORITY_MENU_ORDER, ProcessActionTarget, can_set_affinity, can_set_priority,
    can_terminate_details_row, is_critical_or_system_process, is_eco_mode_eligible,
};
use super::wait_chain_dialog;

/// Terminates a process immediately for standard user processes, or opens the shared
/// high-risk confirmation dialog for critical/system processes.
pub(super) fn request_end_task(
    store: &Entity<UiStore>,
    target: &ProcessActionTarget,
    window: &mut Window,
    cx: &mut App,
) {
    if !can_terminate_details_row(target.status) {
        return;
    }
    if !is_critical_or_system_process(target.status) {
        UiCommands::terminate_process(store, target.key, cx);
        return;
    }

    dialogs::open_confirm_end_task_dialog(
        store.clone(),
        target.key,
        target.name.to_string(),
        true,
        window,
        cx,
    );
}

/// Opens a feature-local confirmation dialog before terminating an entire process tree
/// in bottom-up (leaves-to-root) order.
pub(super) fn open_end_process_tree_dialog(
    store: &Entity<UiStore>,
    target: &ProcessActionTarget,
    window: &mut Window,
    cx: &mut App,
) {
    if !can_terminate_details_row(target.status) {
        return;
    }
    let store = store.clone();
    let key = target.key;
    let name = target.name.to_string();
    let is_critical = is_critical_or_system_process(target.status);
    let confirmed = Rc::new(Cell::new(false));

    window.open_dialog(cx, move |dialog, _window, cx| {
        let store_ok = store.clone();
        let store_btn = store.clone();
        let confirmed_ok = Rc::clone(&confirmed);
        let confirmed_btn = Rc::clone(&confirmed);
        let theme = cx.theme();
        let warning = theme.warning;
        let danger = theme.danger;
        let fg = theme.foreground;
        let radius = theme.radius;
        let message = rust_i18n::t!(
            "details.confirm_tree_message",
            name = name.as_str(),
            pid = key.pid
        )
        .into_owned();

        dialog
            .w(px(460.))
            .centered()
            .title(i18n::tr("details.confirm_tree_title"))
            .child(
                v_flex()
                    .w_full()
                    .gap_3()
                    .child(div().text_sm().text_color(fg).child(message))
                    .when(is_critical, |container| {
                        container.child(
                            h_flex()
                                .w_full()
                                .items_start()
                                .gap_2()
                                .p_2p5()
                                .rounded(radius)
                                .bg(warning.opacity(0.14))
                                .border_1()
                                .border_color(warning.opacity(0.4))
                                .child(
                                    Icon::new(IconName::TriangleAlert)
                                        .small()
                                        .text_color(danger),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_xs()
                                        .text_color(danger)
                                        .child(i18n::tr("processes.confirm_end_critical")),
                                ),
                        )
                    }),
            )
            .on_ok(move |_, _, cx| {
                if !confirmed_ok.replace(true) {
                    UiCommands::terminate_process_tree(&store_ok, key, cx);
                }
                true
            })
            .footer(
                DialogFooter::new()
                    .gap_2()
                    .child(
                        Button::new("details-confirm-tree-cancel")
                            .outline()
                            .label(i18n::tr("common.cancel"))
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("details-confirm-tree-ok")
                            .danger()
                            .label(i18n::tr("details.confirm_tree_action"))
                            .on_click(move |_, window, cx| {
                                if !confirmed_btn.replace(true) {
                                    UiCommands::terminate_process_tree(&store_btn, key, cx);
                                }
                                window.close_dialog(cx);
                            }),
                    ),
            )
    });
}

/// Builds the multi-level right-click [`PopupMenu`] anchored strictly to `target.key`.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_row_context_menu(
    menu: PopupMenu,
    store: &Entity<UiStore>,
    shell_handle: &ShellHandle,
    target: &ProcessActionTarget,
    copy_fields: CopyRowFields,
    capabilities: CapabilityFlags,
    window: &mut Window,
    cx: &mut Context<TableState<DetailsTableDelegate>>,
) -> (PopupMenu, Vec<FocusHandle>) {
    let key = target.key;
    let current_priority = target.priority;
    let can_terminate = can_terminate_details_row(target.status);
    let is_kernel_or_idle = !can_terminate;
    let eco_supported = capabilities.contains(CapabilityFlags::ECO_MODE);
    let suspend_supported = capabilities.contains(CapabilityFlags::SUSPEND_RESUME);
    let wait_chain_supported = capabilities.contains(CapabilityFlags::WAIT_CHAIN);
    let minidump_supported = capabilities.contains(CapabilityFlags::MINIDUMP);
    let uac_supported = capabilities.contains(CapabilityFlags::UAC_VIRTUALIZATION);
    let debugger_supported = capabilities.contains(CapabilityFlags::DEBUGGER_ATTACH);
    let eco_enabled = target.status.contains(ProcessStatusFlags::EFFICIENCY_MODE);
    let eco_eligible = is_eco_mode_eligible(target.status, capabilities);
    let is_suspended = target.status.contains(ProcessStatusFlags::SUSPENDED);
    let suspend_eligible =
        can_suspend_or_resume_process(key.pid, &target.name, target.status, capabilities);
    let is_explorer = is_explorer_image(&target.name);
    let leak_ignore = target
        .has_leak_warning
        .then(|| leak_ignore_target(target.exe_path.as_deref(), &target.name))
        .flatten();
    let uac_enabled = target.uac_virtualization == UacVirtualization::Enabled;
    let uac_toggleable = matches!(
        target.uac_virtualization,
        UacVirtualization::Enabled | UacVirtualization::Disabled
    );
    let priority_allowed = can_set_priority(target.status, capabilities);
    let affinity_allowed = can_set_affinity(target.status, capabilities);
    let has_exe_path = target.exe_path.is_some();

    let store_end = store.clone();
    let target_end = target.clone();

    let store_tree = store.clone();
    let target_tree = target.clone();

    let store_priority = store.clone();

    let store_affinity = store.clone();
    let affinity_name = target.name.to_string();

    let store_eco = store.clone();

    let store_open_loc = store.clone();
    let open_loc_path = target.exe_path.clone();

    let store_props = store.clone();
    let props_path = target.exe_path.clone();

    let store_search = store.clone();
    let search_query = target.name.to_string();
    let shell_services = shell_handle.clone();
    let can_jump_services = key.pid > 0 && capabilities.contains(CapabilityFlags::SERVICES);
    let mut submenu_handles = Vec::with_capacity(2);

    let priority_submenu = PopupMenu::build(window, cx, move |mut submenu, _window, _cx| {
        for level in PRIORITY_MENU_ORDER {
            let store_level = store_priority.clone();
            submenu = submenu.item(
                PopupMenuItem::new(i18n::process_priority_label(level))
                    .checked(current_priority == level)
                    .disabled(!priority_allowed)
                    .on_click(move |_, _, cx| {
                        UiCommands::set_priority(&store_level, key, level, cx);
                    }),
            );
        }
        submenu
    });
    submenu_handles.push(priority_submenu.focus_handle(cx));

    let copy_submenu = build_copy_submenu(copy_fields, window, cx);
    submenu_handles.push(copy_submenu.focus_handle(cx));

    let mut menu = menu
        .item(
            PopupMenuItem::new(i18n::tr("details.menu_end_task"))
                .icon(IconName::CircleX)
                .disabled(!can_terminate)
                .on_click(move |_, window, cx| {
                    request_end_task(&store_end, &target_end, window, cx);
                }),
        )
        .item(
            PopupMenuItem::new(i18n::tr("details.menu_end_process_tree"))
                .icon(IconName::Ban)
                .disabled(!can_terminate)
                .on_click(move |_, window, cx| {
                    open_end_process_tree_dialog(&store_tree, &target_tree, window, cx);
                }),
        );

    if suspend_supported {
        let store_suspend = store.clone();
        let (label, icon) = if is_suspended {
            (i18n::tr("proc_ux_menu_resume"), IconName::Play)
        } else {
            (i18n::tr("proc_ux_menu_suspend"), IconName::Pause)
        };
        menu = menu.item(
            PopupMenuItem::new(label)
                .icon(icon)
                .disabled(!suspend_eligible)
                .on_click(move |_, _, cx| {
                    dispatch_suspend_or_resume(&store_suspend, key, is_suspended, cx);
                }),
        );

        if is_explorer {
            let store_restart = store.clone();
            menu = menu.item(
                PopupMenuItem::new(i18n::tr("proc_ux_menu_restart_explorer"))
                    .icon(Icon::new(AssetIconName::RotateCw))
                    .on_click(move |_, _, cx| {
                        dispatch_restart_explorer(&store_restart, cx);
                    }),
            );
        }
    }

    menu = menu
        .separator()
        .item(
            PopupMenuItem::submenu(i18n::tr("details.menu_set_priority"), priority_submenu)
                .icon(IconName::ChevronsUpDown),
        )
        .item(
            PopupMenuItem::new(i18n::tr("details.menu_set_affinity"))
                .icon(IconName::Cpu)
                .disabled(!affinity_allowed)
                .on_click(move |_, window, cx| {
                    affinity_dialog::open_set_affinity_dialog(
                        store_affinity.clone(),
                        key,
                        affinity_name.clone(),
                        window,
                        cx,
                    );
                }),
        );

    if eco_supported {
        menu = menu.item(
            PopupMenuItem::new(i18n::tr("details.menu_efficiency_mode"))
                .checked(eco_enabled)
                .disabled(!eco_eligible)
                .on_click(move |_, _, cx| {
                    UiCommands::set_efficiency_mode(&store_eco, key, !eco_enabled, cx);
                }),
        );
    }

    if let Some(ignore_target) = leak_ignore {
        let store_leak = store.clone();
        menu = menu.item(
            PopupMenuItem::new(i18n::tr("leak_warning_ignore_menu"))
                .icon(IconName::TriangleAlert)
                .on_click(move |_, _, cx| {
                    dispatch_ignore_leak_path(&store_leak, ignore_target.clone(), cx);
                }),
        );
    }

    if wait_chain_supported || minidump_supported || uac_supported || debugger_supported {
        menu = menu.separator();
        if wait_chain_supported {
            let store_wct = store.clone();
            let wct_name = target.name.to_string();
            menu = menu.item(
                PopupMenuItem::new(i18n::tr("details.menu_analyze_wait_chain"))
                    .icon(IconName::Search)
                    .disabled(is_kernel_or_idle)
                    .on_click(move |_, window, cx| {
                        wait_chain_dialog::open_wait_chain_dialog(
                            store_wct.clone(),
                            key,
                            wct_name.clone(),
                            window,
                            cx,
                        );
                    }),
            );
        }
        if minidump_supported {
            let store_dump = store.clone();
            let dump_name = target.name.to_string();
            menu = menu.item(
                PopupMenuItem::new(i18n::tr("details.menu_create_dump"))
                    .icon(IconName::File)
                    .disabled(is_kernel_or_idle)
                    .on_click(move |_, window, cx| {
                        dialogs::open_minidump_dialog(
                            store_dump.clone(),
                            key,
                            dump_name.clone(),
                            window,
                            cx,
                        );
                    }),
            );
        }
        if uac_supported {
            let store_uac = store.clone();
            menu = menu.item(
                PopupMenuItem::new(i18n::tr("details.menu_uac_virtualization"))
                    .checked(uac_enabled)
                    .disabled(!uac_toggleable || is_kernel_or_idle)
                    .on_click(move |_, _, cx| {
                        UiCommands::set_uac_virtualization(&store_uac, key, !uac_enabled, cx);
                    }),
            );
        }
        if debugger_supported {
            let store_dbg = store.clone();
            menu = menu.item(
                PopupMenuItem::new(i18n::tr("details.menu_attach_debugger"))
                    .icon(IconName::SquareTerminal)
                    .disabled(is_kernel_or_idle)
                    .on_click(move |_, _, cx| {
                        UiCommands::attach_debugger(&store_dbg, key, cx);
                    }),
            );
        }
    }

    let built = menu
        .separator()
        .item(
            PopupMenuItem::submenu(i18n::tr("proc_ux_copy_submenu"), copy_submenu)
                .icon(Icon::new(AssetIconName::Copy)),
        )
        .separator()
        .item(
            PopupMenuItem::new(i18n::tr("details.menu_open_file_location"))
                .icon(IconName::FolderOpen)
                .disabled(!has_exe_path)
                .on_click(move |_, _, cx| {
                    if let Some(ref path) = open_loc_path {
                        UiCommands::open_file_location(
                            &store_open_loc,
                            PathBuf::from(path.as_ref()),
                            cx,
                        );
                    }
                }),
        )
        .item(
            PopupMenuItem::new(i18n::tr("details.menu_properties"))
                .icon(IconName::Info)
                .disabled(!has_exe_path)
                .on_click(move |_, _, cx| {
                    if let Some(ref path) = props_path {
                        UiCommands::show_file_properties(
                            &store_props,
                            PathBuf::from(path.as_ref()),
                            cx,
                        );
                    }
                }),
        )
        .item(
            PopupMenuItem::new(i18n::tr("details.menu_search_online"))
                .icon(IconName::Globe)
                .on_click(move |_, _, cx| {
                    UiCommands::search_online(&store_search, search_query.clone(), cx);
                }),
        )
        .separator()
        .item(
            PopupMenuItem::new(i18n::tr("details.menu_go_to_services"))
                .icon(IconName::Settings2)
                .disabled(!can_jump_services)
                .on_click(move |_, window, cx| {
                    shell_services.jump_to_services(key.pid, window, cx);
                }),
        );
    (built, submenu_handles)
}
