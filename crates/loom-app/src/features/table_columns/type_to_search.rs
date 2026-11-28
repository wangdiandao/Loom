//! Direct type-to-search with CJK IME composition state machine, zero-size GPUI
//! [`TableInputBridge`] element, and shared keyboard-shortcut classification for tables.

use std::ops::Range;

use gpui_kit::{
    App, Bounds, Element, ElementId, ElementInputHandler, Entity, EntityInputHandler, FocusHandle,
    GlobalElementId, InspectorElementId, KeyDownEvent, LayoutId, Pixels, Position, Style, Window,
};

/// Pure UTF-8 / UTF-16 state machine for direct table type-to-search with CJK IME composition.
///
/// Uncommitted IME preedit text is tracked in `marked_range` without mutating `committed`,
/// so cancelling composition (via Escape or empty commit) leaves the previous query intact,
/// while committing text immediately updates the global search query.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct TypeToSearchState {
    committed: String,
    buffer: String,
    marked_range: Option<Range<usize>>,
}

impl TypeToSearchState {
    /// Synchronizes committed text from the global store when no IME composition is active.
    pub(crate) fn sync_committed(&mut self, query: &str) {
        if self.marked_range.is_none() && self.committed != query {
            self.committed.clear();
            self.committed.push_str(query);
            self.buffer.clear();
            self.buffer.push_str(query);
        }
    }

    /// Returns `true` when an IME preedit composition is currently active.
    #[inline]
    pub(crate) fn is_composing(&self) -> bool {
        self.marked_range.is_some()
    }

    /// Returns the currently active IME preedit text, if any.
    pub(crate) fn preedit_text(&self) -> Option<&str> {
        let range = self.marked_range.as_ref()?;
        self.buffer
            .get(range.start..range.end)
            .filter(|s| !s.is_empty())
    }

    /// Returns the UTF-16 length of the current text buffer.
    pub(crate) fn utf16_len(&self) -> usize {
        self.offset_to_utf16(self.buffer.len())
    }

    /// Returns the current caret range (collapsed at the end of the buffer) in UTF-16 units.
    pub(crate) fn selected_utf16_range(&self) -> Range<usize> {
        let end = self.utf16_len();
        end..end
    }

    /// Returns the active IME marked text range in UTF-16 units, if composing.
    pub(crate) fn marked_utf16_range(&self) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|r| self.range_to_utf16(r.clone()))
    }

    /// Extracts text for a UTF-16 range and returns `(adjusted_utf16_range, text)`.
    pub(crate) fn text_for_utf16_range(&self, range_utf16: Range<usize>) -> (Range<usize>, String) {
        let utf8 = self.range_from_utf16(range_utf16);
        let adjusted = self.range_to_utf16(utf8.clone());
        let text = self.buffer.get(utf8).unwrap_or_default().to_owned();
        (adjusted, text)
    }

    /// Updates or cancels uncommitted IME preedit text without altering `committed`.
    pub(crate) fn replace_and_mark(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        store_query: &str,
    ) {
        if self.marked_range.is_none() {
            self.sync_committed(store_query);
        }

        let target = range_utf16
            .map(|r| self.range_from_utf16(r))
            .or_else(|| self.marked_range.clone())
            .unwrap_or(self.buffer.len()..self.buffer.len());

        let sanitized = sanitize_search_input(new_text);
        if sanitized.is_empty() {
            self.buffer.clear();
            self.buffer.push_str(&self.committed);
            self.marked_range = None;
            return;
        }

        self.buffer.replace_range(target.clone(), &sanitized);
        self.marked_range = Some(target.start..target.start + sanitized.len());
    }

    /// Commits typed or IME-composed text into the search query.
    ///
    /// Returns `Some(next_query)` when the committed search query changed and should
    /// be dispatched via `UiCommands::set_search_query`. Cancelling an active composition
    /// with an empty string restores the previous `committed` query and returns `None`.
    pub(crate) fn replace_text(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        store_query: &str,
    ) -> Option<String> {
        let was_composing = self.marked_range.is_some();
        if !was_composing {
            self.sync_committed(store_query);
        }

        let sanitized = sanitize_search_input(text);
        if was_composing && sanitized.is_empty() && range_utf16.is_none() {
            // IME cancelled composition (e.g., Windows WM_IME_COMPOSITION lparam == 0):
            // restore previous committed query cleanly.
            self.buffer.clear();
            self.buffer.push_str(&self.committed);
            self.marked_range = None;
            return None;
        }

        if sanitized.is_empty() && range_utf16.is_none() {
            return None;
        }

        let target = range_utf16
            .map(|r| self.range_from_utf16(r))
            .or_else(|| self.marked_range.take())
            .unwrap_or(self.buffer.len()..self.buffer.len());

        self.buffer.replace_range(target, &sanitized);
        self.marked_range = None;

        if self.buffer != self.committed {
            self.committed.clear();
            self.committed.push_str(&self.buffer);
            Some(self.committed.clone())
        } else {
            None
        }
    }

    /// Cancels/unmarks any active IME preedit without altering the committed query.
    pub(crate) fn unmark_text(&mut self) {
        if self.marked_range.take().is_some() {
            self.buffer.clear();
            self.buffer.push_str(&self.committed);
        }
    }

    /// Removes the last Unicode scalar from the committed query when not composing.
    pub(crate) fn handle_backspace(&mut self, store_query: &str) -> Option<String> {
        if self.marked_range.is_some() {
            return None;
        }
        self.sync_committed(store_query);
        self.committed.pop()?;
        self.buffer.clear();
        self.buffer.push_str(&self.committed);
        Some(self.committed.clone())
    }

    /// Cancels active IME composition (preserving previous query) or clears a non-empty query.
    pub(crate) fn handle_escape(&mut self, store_query: &str) -> Option<String> {
        if self.marked_range.take().is_some() {
            self.buffer.clear();
            self.buffer.push_str(&self.committed);
            return None;
        }
        self.sync_committed(store_query);
        if self.committed.is_empty() {
            return None;
        }
        self.committed.clear();
        self.buffer.clear();
        Some(String::new())
    }

    fn offset_to_utf16(&self, byte_offset: usize) -> usize {
        let clamped = byte_offset.min(self.buffer.len());
        let mut utf16 = 0usize;
        for (idx, ch) in self.buffer.char_indices() {
            if idx >= clamped {
                break;
            }
            utf16 += ch.len_utf16();
        }
        utf16
    }

    fn offset_from_utf16(&self, utf16_offset: usize) -> usize {
        let mut utf16 = 0usize;
        for (idx, ch) in self.buffer.char_indices() {
            if utf16 >= utf16_offset {
                return idx;
            }
            utf16 += ch.len_utf16();
        }
        self.buffer.len()
    }

    fn range_to_utf16(&self, range: Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: Range<usize>) -> Range<usize> {
        let start = self.offset_from_utf16(range.start);
        let end = self.offset_from_utf16(range.end);
        start.min(end)..start.max(end)
    }
}

fn sanitize_search_input(raw: &str) -> String {
    raw.chars().filter(|c| !c.is_control()).collect()
}

/// Logical keyboard shortcut intents shared across Processes and Details tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TableKeyAction {
    /// `Ctrl+C` / `Cmd+C`: copy short `"name (pid)"` text.
    CopyShort,
    /// `Ctrl+Shift+C` / `Cmd+Shift+C`: copy structured multi-field row summary.
    CopySummary,
    /// `Delete`: terminate the selected process (with critical-process confirmation).
    EndTask,
    /// `ArrowRight`: expand the selected collapsible group/tree row if collapsed.
    ExpandRow,
    /// `ArrowLeft`: collapse the selected collapsible group/tree row if expanded.
    CollapseRow,
    /// `Backspace`: delete the last character from direct type-to-search.
    BackspaceSearch,
    /// `Escape`: cancel active IME composition or clear the search query.
    EscapeSearch,
}

/// Classifies a [`KeyDownEvent`] into a [`TableKeyAction`], if applicable.
pub(crate) fn classify_table_key_down(event: &KeyDownEvent) -> Option<TableKeyAction> {
    let m = &event.keystroke.modifiers;
    let primary_mod = m.control || m.platform;
    let no_modifiers = !m.control && !m.alt && !m.platform && !m.shift && !m.function;

    if primary_mod && !m.alt && event.keystroke.key.eq_ignore_ascii_case("c") {
        return Some(if m.shift {
            TableKeyAction::CopySummary
        } else {
            TableKeyAction::CopyShort
        });
    }

    if no_modifiers {
        return match event.keystroke.key.as_str() {
            "delete" => Some(TableKeyAction::EndTask),
            "right" => Some(TableKeyAction::ExpandRow),
            "left" => Some(TableKeyAction::CollapseRow),
            "escape" => Some(TableKeyAction::EscapeSearch),
            "backspace" => Some(TableKeyAction::BackspaceSearch),
            _ => None,
        };
    }

    if !m.control && !m.alt && !m.platform && event.keystroke.key == "backspace" {
        return Some(TableKeyAction::BackspaceSearch);
    }

    None
}

/// Zero-size, non-drawing GPUI element that binds an [`EntityInputHandler`] to a table's
/// [`FocusHandle`] during paint so Windows/macOS/Linux IME composition and direct typing
/// work seamlessly whenever the table is focused.
pub(crate) struct TableInputBridge<V: EntityInputHandler> {
    focus_handle: FocusHandle,
    entity: Entity<V>,
}

impl<V: EntityInputHandler> TableInputBridge<V> {
    /// Creates a new [`TableInputBridge`] binding `entity` to `focus_handle`.
    pub(crate) fn new(focus_handle: FocusHandle, entity: Entity<V>) -> Self {
        Self {
            focus_handle,
            entity,
        }
    }
}

impl<V: EntityInputHandler> Element for TableInputBridge<V> {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = Style {
            position: Position::Absolute,
            ..Style::default()
        };
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.handle_input(
            &self.focus_handle,
            ElementInputHandler::new(bounds, self.entity.clone()),
            cx,
        );
    }
}

/// Implements [`gpui_kit::EntityInputHandler`] for a table view holding `store: Entity<UiStore>`,
/// `table: Entity<TableState<_>>`, and `type_to_search: TypeToSearchState`.
///
/// Eliminates duplicate 100-line IME/UTF-16 handler boilerplate across `ProcessesView` and
/// `DetailsView` while keeping focus gating identical.
macro_rules! impl_table_type_to_search_input_handler {
    ($view:ty) => {
        impl gpui_kit::EntityInputHandler for $view {
            fn text_for_range(
                &mut self,
                range: std::ops::Range<usize>,
                adjusted_range: &mut Option<std::ops::Range<usize>>,
                _window: &mut gpui_kit::Window,
                cx: &mut gpui_kit::Context<Self>,
            ) -> Option<String> {
                let query = self.store.read(cx).search_query().to_owned();
                self.type_to_search.sync_committed(&query);
                let (adj, text) = self.type_to_search.text_for_utf16_range(range);
                adjusted_range.replace(adj);
                Some(text)
            }

            fn selected_text_range(
                &mut self,
                _ignore_disabled_input: bool,
                _window: &mut gpui_kit::Window,
                cx: &mut gpui_kit::Context<Self>,
            ) -> Option<gpui_kit::UTF16Selection> {
                let query = self.store.read(cx).search_query().to_owned();
                self.type_to_search.sync_committed(&query);
                Some(gpui_kit::UTF16Selection {
                    range: self.type_to_search.selected_utf16_range(),
                    reversed: false,
                })
            }

            fn marked_text_range(
                &self,
                _window: &mut gpui_kit::Window,
                _cx: &mut gpui_kit::Context<Self>,
            ) -> Option<std::ops::Range<usize>> {
                self.type_to_search.marked_utf16_range()
            }

            fn unmark_text(
                &mut self,
                _window: &mut gpui_kit::Window,
                cx: &mut gpui_kit::Context<Self>,
            ) {
                if self.type_to_search.is_composing() {
                    self.type_to_search.unmark_text();
                    cx.notify();
                }
            }

            fn replace_text_in_range(
                &mut self,
                range: Option<std::ops::Range<usize>>,
                text: &str,
                _window: &mut gpui_kit::Window,
                cx: &mut gpui_kit::Context<Self>,
            ) {
                let was_composing = self.type_to_search.is_composing();
                let current = self.store.read(cx).search_query().to_owned();
                if let Some(next_query) = self.type_to_search.replace_text(range, text, &current) {
                    crate::bridge::UiCommands::set_search_query(&self.store, next_query, cx);
                }
                if was_composing {
                    cx.notify();
                }
            }

            fn replace_and_mark_text_in_range(
                &mut self,
                range: Option<std::ops::Range<usize>>,
                new_text: &str,
                _new_selected_range: Option<std::ops::Range<usize>>,
                _window: &mut gpui_kit::Window,
                cx: &mut gpui_kit::Context<Self>,
            ) {
                let current = self.store.read(cx).search_query().to_owned();
                self.type_to_search
                    .replace_and_mark(range, new_text, &current);
                cx.notify();
            }

            fn bounds_for_range(
                &mut self,
                _range_utf16: std::ops::Range<usize>,
                element_bounds: gpui_kit::Bounds<gpui_kit::Pixels>,
                _window: &mut gpui_kit::Window,
                _cx: &mut gpui_kit::Context<Self>,
            ) -> Option<gpui_kit::Bounds<gpui_kit::Pixels>> {
                Some(gpui_kit::Bounds::new(
                    element_bounds.origin,
                    gpui_kit::size(gpui_kit::px(1.0), gpui_kit::px(20.0)),
                ))
            }

            fn character_index_for_point(
                &mut self,
                _point: gpui_kit::Point<gpui_kit::Pixels>,
                _window: &mut gpui_kit::Window,
                _cx: &mut gpui_kit::Context<Self>,
            ) -> Option<usize> {
                Some(self.type_to_search.utf16_len())
            }

            fn text_length_utf16(
                &mut self,
                _window: &mut gpui_kit::Window,
                cx: &mut gpui_kit::Context<Self>,
            ) -> Option<usize> {
                let query = self.store.read(cx).search_query().to_owned();
                self.type_to_search.sync_committed(&query);
                Some(self.type_to_search.utf16_len())
            }

            fn accepts_text_input(
                &self,
                window: &mut gpui_kit::Window,
                cx: &mut gpui_kit::Context<Self>,
            ) -> bool {
                self.table.focus_handle(cx).is_focused(window)
            }
        }
    };
}

pub(crate) use impl_table_type_to_search_input_handler;
