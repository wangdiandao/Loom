//! Column header context menu, **Select columns** modal dialog, smart row copy submenu,
//! and shared context-menu focus-out tracking.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::table::{TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, IconName, WindowExt as _, v_flex};
use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, Entity, FocusHandle, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, div,
    px,
};

use crate::i18n;

/// Callback executed by a table column header context menu item.
pub(crate) type HeaderMenuAction = Rc<dyn Fn(&mut Window, &mut App)>;

/// Builds the standard table column header right-click [`PopupMenu`].
///
/// Includes `Hide column` (disabled when `hide_action` is `None`, e.g., on the pinned Name
/// column or empty header space), `Select columns...`, optional view-specific `extra_items`,
/// and `Reset default columns`.
pub(crate) fn build_column_header_menu<FExtra>(
    mut menu: PopupMenu,
    hide_action: Option<HeaderMenuAction>,
    on_select_columns: HeaderMenuAction,
    extra_items: FExtra,
    on_reset_defaults: HeaderMenuAction,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu
where
    FExtra: FnOnce(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu,
{
    let can_hide = hide_action.is_some();
    menu = menu
        .item(
            PopupMenuItem::new(i18n::tr("table.menu_hide_column"))
                .disabled(!can_hide)
                .on_click(move |_, window, cx| {
                    if let Some(ref action) = hide_action {
                        action(window, cx);
                    }
                }),
        )
        .item(
            PopupMenuItem::new(i18n::tr("table.menu_select_columns"))
                .icon(IconName::Settings2)
                .on_click(move |_, window, cx| {
                    on_select_columns(window, cx);
                }),
        );

    menu = extra_items(menu, window, cx);

    menu.separator().item(
        PopupMenuItem::new(i18n::tr("table.menu_reset_columns")).on_click(move |_, window, cx| {
            on_reset_defaults(window, cx);
        }),
    )
}

/// Single checkbox entry displayed inside the **Select columns** dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnToggleItem {
    /// Stable column slug matching [`super::ColumnKey::slug`].
    pub(crate) slug: &'static str,
    /// Localized display label shown next to the checkbox.
    pub(crate) label: String,
    /// Current visibility toggle state.
    pub(crate) visible: bool,
}

/// Pure draft state for the **Select columns** checklist dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectColumnsDraft {
    items: Vec<ColumnToggleItem>,
}

impl SelectColumnsDraft {
    /// Creates a new draft from ordered [`ColumnToggleItem`] entries.
    pub(crate) fn new(items: Vec<ColumnToggleItem>) -> Self {
        Self { items }
    }

    /// Returns the ordered checklist items.
    #[inline]
    pub(crate) fn items(&self) -> &[ColumnToggleItem] {
        &self.items
    }

    /// Updates the checked state of item `index`.
    pub(crate) fn set_checked(&mut self, index: usize, checked: bool) {
        if let Some(item) = self.items.get_mut(index) {
            item.visible = checked;
        }
    }

    /// Returns the slugs of all currently checked items.
    pub(crate) fn selected_slugs(&self) -> Vec<&'static str> {
        self.items
            .iter()
            .filter(|item| item.visible)
            .map(|item| item.slug)
            .collect()
    }
}

type ConfirmColumnsCallback = Rc<dyn Fn(Vec<&'static str>, &mut App)>;

struct SelectColumnsDialogView {
    draft: SelectColumnsDraft,
    on_confirm: ConfirmColumnsCallback,
}

impl SelectColumnsDialogView {
    fn new(items: Vec<ColumnToggleItem>, on_confirm: ConfirmColumnsCallback) -> Self {
        Self {
            draft: SelectColumnsDraft::new(items),
            on_confirm,
        }
    }
}

impl Render for SelectColumnsDialogView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;

        let checkboxes = self
            .draft
            .items()
            .iter()
            .enumerate()
            .map(|(idx, item)| {
                let id = SharedString::from(format!("select-column-checkbox-{}", item.slug));
                let label = item.label.clone();
                let checked = item.visible;
                div().w_full().py_0p5().child(
                    Checkbox::new(id)
                        .label(label)
                        .checked(checked)
                        .on_change(cx.listener(move |this, next: &bool, _, cx| {
                            this.draft.set_checked(idx, *next);
                            cx.notify();
                        })),
                )
            })
            .collect::<Vec<_>>();

        v_flex()
            .w_full()
            .gap_3()
            .child(
                v_flex()
                    .id("select-columns-scroll")
                    .w_full()
                    .max_h(px(340.))
                    .overflow_y_scrollbar()
                    .gap_2()
                    .py_1()
                    .pr_2()
                    .border_t_1()
                    .border_b_1()
                    .border_color(border.opacity(0.6))
                    .children(checkboxes),
            )
            .child(
                DialogFooter::new()
                    .pt_1()
                    .gap_2()
                    .child(
                        Button::new("select-columns-cancel")
                            .outline()
                            .label(i18n::tr("common.cancel"))
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("select-columns-ok")
                            .primary()
                            .label(i18n::tr("common.ok"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                let selected = this.draft.selected_slugs();
                                (this.on_confirm)(selected, cx);
                                window.close_dialog(cx);
                            })),
                    ),
            )
    }
}

/// Opens the reusable **Select columns** modal dialog inside `gpui-kit`'s `Root` dialog layer.
pub(crate) fn open_select_columns_dialog(
    items: Vec<ColumnToggleItem>,
    on_confirm: ConfirmColumnsCallback,
    window: &mut Window,
    cx: &mut App,
) {
    let dialog_view = cx.new(|_| SelectColumnsDialogView::new(items, on_confirm));
    window.open_dialog(cx, move |dialog, _window, _cx| {
        dialog
            .w(px(360.))
            .centered()
            .title(i18n::tr("table.select_columns_title"))
            .child(dialog_view.clone())
    });
}

/// Tracks focus across a root row [`PopupMenu`] and its submenus, invoking `on_dismiss`
/// once focus leaves all menu surfaces so order-freeze unfreezes reliably.
pub(crate) fn track_context_menu_focus<D: TableDelegate>(
    subs_out: &mut Vec<Subscription>,
    root_focus: FocusHandle,
    submenu_handles: Vec<FocusHandle>,
    window: &mut Window,
    cx: &mut Context<TableState<D>>,
    on_dismiss: impl Fn(&mut TableState<D>, &mut Context<TableState<D>>) + Copy + 'static,
) {
    let mut tracked_handles = Vec::with_capacity(1 + submenu_handles.len());
    tracked_handles.push(root_focus);
    tracked_handles.extend(submenu_handles);
    let tracked = Rc::new(tracked_handles);

    subs_out.clear();
    for handle in tracked.iter() {
        let all_handles = Rc::clone(&tracked);
        let sub = cx.on_focus_out(handle, window, move |table, _, window, cx| {
            let still_in_menu = all_handles
                .iter()
                .any(|h| h.is_focused(window) || h.contains_focused(window, cx));
            if !still_in_menu {
                on_dismiss(table, cx);
            }
        });
        subs_out.push(sub);
    }
}

// ---------------------------------------------------------------------------
// Smart context-aware copying
// ---------------------------------------------------------------------------

/// Formats the default `Ctrl+C` clipboard representation (`"<name> (<pid>)"`).
#[inline]
pub(crate) fn format_process_name_and_pid(name: &str, pid: u32) -> String {
    format!("{name} ({pid})")
}

/// Snapshot of copyable fields for a right-clicked process row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CopyRowFields {
    /// Primary process image/display name.
    pub(crate) name: String,
    /// Process numeric PID.
    pub(crate) pid: u32,
    /// Resolved executable path, when known.
    pub(crate) exe_path: Option<String>,
    /// Resolved command line, when known.
    pub(crate) command_line: Option<String>,
    /// Pre-formatted multi-field row summary (matching `Ctrl+Shift+C`).
    pub(crate) row_summary: String,
    /// Optional `(column_label, cell_text)` for the specific column that was right-clicked.
    pub(crate) clicked_column: Option<(String, String)>,
}

/// Builds the `"Copy >"` row context submenu shared by Processes and Details.
pub(crate) fn build_copy_submenu(
    fields: CopyRowFields,
    window: &mut Window,
    cx: &mut App,
) -> Entity<PopupMenu> {
    PopupMenu::build(window, cx, move |mut sub, _window, _cx| {
        let copy_name = fields.name.clone();
        let copy_pid = fields.pid.to_string();
        let copy_path = fields
            .exe_path
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let copy_cmdline = fields
            .command_line
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let copy_summary = fields.row_summary.clone();
        let clicked_col = fields
            .clicked_column
            .clone()
            .filter(|(_, val)| !val.trim().is_empty() && val.trim() != "—");

        sub = sub
            .item(
                PopupMenuItem::new(i18n::tr("proc_ux_copy_name"))
                    .disabled(copy_name.is_empty())
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_name.clone()));
                    }),
            )
            .item(
                PopupMenuItem::new(i18n::tr("proc_ux_copy_pid")).on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_pid.clone()));
                }),
            )
            .item(
                PopupMenuItem::new(i18n::tr("proc_ux_copy_path"))
                    .disabled(copy_path.is_none())
                    .on_click(move |_, _, cx| {
                        if let Some(ref p) = copy_path {
                            cx.write_to_clipboard(ClipboardItem::new_string(p.clone()));
                        }
                    }),
            )
            .item(
                PopupMenuItem::new(i18n::tr("proc_ux_copy_cmdline"))
                    .disabled(copy_cmdline.is_none())
                    .on_click(move |_, _, cx| {
                        if let Some(ref c) = copy_cmdline {
                            cx.write_to_clipboard(ClipboardItem::new_string(c.clone()));
                        }
                    }),
            )
            .item(
                PopupMenuItem::new(i18n::tr("proc_ux_copy_summary")).on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_summary.clone()));
                }),
            );

        if let Some((col_label, cell_text)) = clicked_col {
            let label =
                rust_i18n::t!("proc_ux_copy_column", column = col_label.as_str()).into_owned();
            sub = sub
                .separator()
                .item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(cell_text.clone()));
                }));
        }

        sub
    })
}
