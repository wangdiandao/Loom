//! `ProcessKey`-anchored row context menu, nested resource-values display submenu,
//! and end-task confirmation routing for [`super::ProcessesView`].

use std::path::PathBuf;

use gpui_kit::assets::IconName as AssetIconName;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::TableState;
use gpui_kit::component::{Icon, IconName};
use gpui_kit::{App, Context, Entity, FocusHandle, Focusable as _, Window};
use loom_types::{CapabilityFlags, ProcessRowProjection, ProcessStatusFlags, ResourceDisplayMode};

use crate::bridge::{UiCommands, UiStore};
use crate::features::table_columns::{
    CopyRowFields, build_copy_submenu, can_suspend_or_resume_process, dispatch_ignore_leak_path,
    dispatch_restart_explorer, dispatch_suspend_or_resume, is_explorer_image, leak_ignore_target,
};
use crate::i18n;
use crate::shell::{ShellHandle, dialogs};

use super::delegate::ProcessesTableDelegate;
use super::state::{
    ResourceColumn, apply_resource_display_mode, can_terminate_process, find_selected_row,
    is_efficiency_mode_eligible, requires_end_task_confirmation, search_online_query,
};

/// Triggers End Task on `row`, routing system/critical/protected processes through a confirmation dialog.
pub(super) fn trigger_end_task_for_row(
    store: &Entity<UiStore>,
    row: &ProcessRowProjection,
    window: &mut Window,
    cx: &mut App,
) {
    if !can_terminate_process(row) {
        return;
    }
    if requires_end_task_confirmation(row) {
        dialogs::open_confirm_end_task_dialog(
            store.clone(),
            row.key,
            row.display_name.to_string(),
            true,
            window,
            cx,
        );
    } else {
        UiCommands::terminate_process(store, row.key, cx);
    }
}

/// Builds the nested `Resource values > {Memory, Disk, Network} > {Values, Percents}` submenu.
pub(super) fn build_resource_values_submenu(
    menu: PopupMenu,
    store: Entity<UiStore>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let (mem_mode, disk_mode, net_mode) = {
        let settings = store.read(cx).settings();
        (
            settings.memory_display_mode,
            settings.disk_display_mode,
            settings.network_display_mode,
        )
    };

    let store_mem = store.clone();
    let store_disk = store.clone();
    let store_net = store;

    menu.submenu(
        i18n::tr("processes.menu_resource_memory"),
        window,
        cx,
        move |sub, _window, _cx| {
            add_resource_mode_items(
                sub,
                store_mem.clone(),
                mem_mode,
                disk_mode,
                net_mode,
                ResourceColumn::Memory,
            )
        },
    )
    .submenu(
        i18n::tr("processes.menu_resource_disk"),
        window,
        cx,
        move |sub, _window, _cx| {
            add_resource_mode_items(
                sub,
                store_disk.clone(),
                mem_mode,
                disk_mode,
                net_mode,
                ResourceColumn::Disk,
            )
        },
    )
    .submenu(
        i18n::tr("processes.menu_resource_network"),
        window,
        cx,
        move |sub, _window, _cx| {
            add_resource_mode_items(
                sub,
                store_net.clone(),
                mem_mode,
                disk_mode,
                net_mode,
                ResourceColumn::Network,
            )
        },
    )
}

fn add_resource_mode_items(
    menu: PopupMenu,
    store: Entity<UiStore>,
    mem_mode: ResourceDisplayMode,
    disk_mode: ResourceDisplayMode,
    net_mode: ResourceDisplayMode,
    column: ResourceColumn,
) -> PopupMenu {
    let current = match column {
        ResourceColumn::Memory => mem_mode,
        ResourceColumn::Disk => disk_mode,
        ResourceColumn::Network => net_mode,
    };

    let (val_m, val_d, val_n) = apply_resource_display_mode(
        mem_mode,
        disk_mode,
        net_mode,
        column,
        ResourceDisplayMode::Values,
    );
    let (pct_m, pct_d, pct_n) = apply_resource_display_mode(
        mem_mode,
        disk_mode,
        net_mode,
        column,
        ResourceDisplayMode::Percent,
    );

    let store_val = store.clone();
    let store_pct = store;

    menu.item(
        PopupMenuItem::new(i18n::tr("processes.menu_mode_values"))
            .checked(current == ResourceDisplayMode::Values)
            .on_click(move |_, _, cx| {
                UiCommands::set_resource_display_modes(&store_val, val_m, val_d, val_n, cx);
            }),
    )
    .item(
        PopupMenuItem::new(i18n::tr("processes.menu_mode_percent"))
            .checked(current == ResourceDisplayMode::Percent)
            .on_click(move |_, _, cx| {
                UiCommands::set_resource_display_modes(&store_pct, pct_m, pct_d, pct_n, cx);
            }),
    )
}

/// Builds the full `ProcessKey`-anchored right-click context menu for a process row.
pub(super) fn build_row_context_menu(
    mut menu: PopupMenu,
    store: Entity<UiStore>,
    shell: ShellHandle,
    fallback_row: ProcessRowProjection,
    copy_fields: CopyRowFields,
    window: &mut Window,
    cx: &mut Context<TableState<ProcessesTableDelegate>>,
) -> (PopupMenu, Vec<FocusHandle>) {
    let key = fallback_row.key;
    // Borrow snapshot briefly without cloning the full AppSnapshot / PerformanceSnapshot rings.
    let (caps, row) = {
        let store_ref = store.read(cx);
        let snap = store_ref.snapshot();
        let live_row = find_selected_row(&snap.processes.rows, Some(key))
            .cloned()
            .unwrap_or(fallback_row);
        (snap.capabilities, live_row)
    };
    let eco_supported = caps.contains(CapabilityFlags::ECO_MODE);
    let suspend_supported = caps.contains(CapabilityFlags::SUSPEND_RESUME);
    let minidump_supported = caps.contains(CapabilityFlags::MINIDUMP);
    let debugger_supported = caps.contains(CapabilityFlags::DEBUGGER_ATTACH);

    let has_children = row.has_children;
    let is_expanded = row.is_expanded;
    let can_end = can_terminate_process(&row);
    let is_kernel_or_idle = row
        .status
        .intersects(ProcessStatusFlags::SYSTEM_IDLE | ProcessStatusFlags::KERNEL);
    let eco_eligible = is_efficiency_mode_eligible(&row, caps);
    let eco_active = row.status.contains(ProcessStatusFlags::EFFICIENCY_MODE);
    let is_suspended = row.status.contains(ProcessStatusFlags::SUSPENDED);
    let suspend_eligible = can_suspend_or_resume_process(key.pid, &row.name, row.status, caps);
    let is_explorer = is_explorer_image(&row.name);
    let leak_ignore = row
        .leak_warning
        .as_ref()
        .and_then(|_| leak_ignore_target(row.exe_path.as_deref(), &row.name));
    let exe_path = row.exe_path.clone();
    let query = search_online_query(&row);
    let mut submenu_handles = Vec::with_capacity(2);

    if has_children {
        let store_expand = store.clone();
        let expand_label = if is_expanded {
            i18n::tr("processes.menu_collapse")
        } else {
            i18n::tr("processes.menu_expand")
        };
        let expand_icon = if is_expanded {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        };
        menu = menu.item(PopupMenuItem::new(expand_label).icon(expand_icon).on_click(
            move |_, _, cx| {
                UiCommands::toggle_process_expanded(&store_expand, key, cx);
            },
        ));
    }

    let store_end = store.clone();
    let row_for_end = row.clone();
    menu = menu.item(
        PopupMenuItem::new(i18n::tr("processes.menu_end_task"))
            .icon(IconName::CircleX)
            .disabled(!can_end)
            .on_click(move |_, window, cx| {
                trigger_end_task_for_row(&store_end, &row_for_end, window, cx);
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

    if eco_supported {
        let store_eco = store.clone();
        menu = menu.item(
            PopupMenuItem::new(i18n::tr("processes.menu_efficiency_mode"))
                .checked(eco_active)
                .disabled(!eco_eligible)
                .on_click(move |_, _, cx| {
                    UiCommands::set_efficiency_mode(&store_eco, key, !eco_active, cx);
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

    if minidump_supported || debugger_supported {
        menu = menu.separator();
        if minidump_supported {
            let store_dump = store.clone();
            let dump_name = row.name.to_string();
            menu = menu.item(
                PopupMenuItem::new(i18n::tr("processes.menu_create_dump"))
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
        if debugger_supported {
            let store_dbg = store.clone();
            menu = menu.item(
                PopupMenuItem::new(i18n::tr("processes.menu_attach_debugger"))
                    .icon(IconName::SquareTerminal)
                    .disabled(is_kernel_or_idle)
                    .on_click(move |_, _, cx| {
                        UiCommands::attach_debugger(&store_dbg, key, cx);
                    }),
            );
        }
    }

    let copy_submenu = build_copy_submenu(copy_fields, window, cx);
    submenu_handles.push(copy_submenu.focus_handle(cx));

    let store_res = store.clone();
    let res_submenu = PopupMenu::build(window, cx, move |sub, window, cx| {
        build_resource_values_submenu(sub, store_res.clone(), window, cx)
    });
    submenu_handles.push(res_submenu.focus_handle(cx));

    menu = menu
        .separator()
        .item(
            PopupMenuItem::submenu(i18n::tr("proc_ux_copy_submenu"), copy_submenu)
                .icon(Icon::new(AssetIconName::Copy)),
        )
        .item(PopupMenuItem::submenu(
            i18n::tr("processes.menu_resource_values"),
            res_submenu,
        ))
        .separator();

    let shell_details = shell;
    menu = menu.item(
        PopupMenuItem::new(i18n::tr("processes.jump_to_details"))
            .icon(IconName::ArrowRight)
            .on_click(move |_, window, cx| {
                shell_details.jump_to_details(key, window, cx);
            }),
    );

    let store_loc = store.clone();
    let path_loc = exe_path.clone();
    menu = menu.item(
        PopupMenuItem::new(i18n::tr("processes.menu_open_file_location"))
            .icon(IconName::FolderOpen)
            .disabled(path_loc.is_none())
            .on_click(move |_, _, cx| {
                if let Some(ref p) = path_loc {
                    UiCommands::open_file_location(&store_loc, PathBuf::from(p.as_ref()), cx);
                }
            }),
    );

    let store_search = store.clone();
    menu = menu.item(
        PopupMenuItem::new(i18n::tr("processes.menu_search_online"))
            .icon(IconName::Globe)
            .on_click(move |_, _, cx| {
                UiCommands::search_online(&store_search, query.clone(), cx);
            }),
    );

    let store_props = store;
    let path_props = exe_path;
    let built = menu.item(
        PopupMenuItem::new(i18n::tr("processes.menu_properties"))
            .icon(IconName::Info)
            .disabled(path_props.is_none())
            .on_click(move |_, _, cx| {
                if let Some(ref p) = path_props {
                    UiCommands::show_file_properties(&store_props, PathBuf::from(p.as_ref()), cx);
                }
            }),
    );
    (built, submenu_handles)
}
