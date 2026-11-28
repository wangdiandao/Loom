//! Reusable column catalog, visibility/order/width persistence, header context menu,
//! **Select columns** dialog, icon upload cache, scroll coalescer, viewport fingerprinting,
//! type-to-search IME bridge, CSV export, and shared table cell/header helpers used across
//! Loom's `DataTable` feature slices.

mod export;
mod icon_cache;
mod layout;
mod menu;
mod process_controls;
mod telemetry;
mod type_to_search;
mod viewport;
mod widgets;

#[cfg(test)]
mod tests;

pub(crate) use export::spawn_csv_export;
pub(crate) use icon_cache::{BudgetedIconLookup, IconRenderCache, resolve_budgeted_icon};
pub(crate) use layout::{ColumnKey, ColumnSpec, TableColumnLayout, to_column_sort};
pub(crate) use menu::{
    ColumnToggleItem, CopyRowFields, HeaderMenuAction, build_column_header_menu,
    build_copy_submenu, format_process_name_and_pid, open_select_columns_dialog,
    track_context_menu_focus,
};
pub(crate) use process_controls::{
    can_suspend_or_resume_process, dispatch_hide_system_processes, dispatch_ignore_leak_path,
    dispatch_restart_explorer, dispatch_suspend_or_resume, is_explorer_image, leak_ignore_target,
};
pub(crate) use telemetry::{ListTelemetryRegistry, ListViewTelemetryKind, list_telemetry};
pub(crate) use type_to_search::{
    TableInputBridge, TableKeyAction, TypeToSearchState, classify_table_key_down,
    impl_table_type_to_search_input_handler,
};
pub(crate) use viewport::{FnvHasher64, ScrollBurstCoalescer};
pub(crate) use widgets::{
    DataTablePane, OrderFreezeState, SortPausedBadge, ToolbarActionButton, ToolbarActionStyle,
    WebView2RoleLabels, expand_chevron_button, leak_warning_badge_with_prefix,
    render_ime_preedit_badge, render_metric_cell, render_mono_numeric_cell,
    render_mono_preview_cell, render_mono_text_cell, render_table_empty_state,
    render_table_header_row, render_table_th_cell, render_text_cell, webview2_role_badge,
};

// Preserved crate-internal re-exports for compatibility with sibling feature modules and tests.
#[allow(unused_imports)]
pub(crate) use export::{
    default_csv_export_dir, format_csv_export_filename, write_csv_export_file,
};
#[allow(unused_imports)]
pub(crate) use icon_cache::{MAX_CACHED_ICONS, MAX_ICON_UPLOADS_PER_FRAME};
#[allow(unused_imports)]
pub(crate) use menu::SelectColumnsDraft;
#[allow(unused_imports)]
pub(crate) use telemetry::ListTelemetrySnapshot;
#[allow(unused_imports)]
pub(crate) use viewport::{DEFAULT_MAX_DEFER_WINDOW, DEFAULT_SCROLL_QUIET_WINDOW};
#[allow(unused_imports)]
pub(crate) use widgets::{
    ToolbarActionClick, leak_warning_badge, should_attach_cell_tooltip, sort_paused_indicator,
    webview2_role_label,
};
