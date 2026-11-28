//! Single-line UTF-8 safe preview truncation for long table cells.

use std::sync::Arc;

use gpui_kit::SharedString;

/// Default single-line character cap applied to long table cell preview strings
/// (such as command lines or executable paths) before passing them to text shaping.
pub const DEFAULT_CELL_PREVIEW_CHARS: usize = 256;

/// Produces a UTF-8 safe single-line preview capped at `max_chars` Unicode
/// scalar values, replacing any control characters (including `\r` and `\n`)
/// with spaces and appending `'…'` if truncated.
///
/// Returns `None` when `s` already has `<= max_chars` characters and contains
/// no control characters, allowing callers to reuse the original string without
/// allocating.
pub fn cap_single_line_preview(s: &str, max_chars: usize) -> Option<String> {
    let mut has_control = false;
    let mut truncated = false;
    let mut prefix_end = s.len();

    for (count, (byte_idx, ch)) in s.char_indices().enumerate() {
        if count == max_chars {
            truncated = true;
            prefix_end = byte_idx;
            break;
        }
        if ch.is_control() {
            has_control = true;
        }
    }

    if !truncated && !has_control {
        return None;
    }

    let prefix = &s[..prefix_end];
    let extra = if truncated { '…'.len_utf8() } else { 0 };
    let mut out = String::with_capacity(prefix.len() + extra);
    if has_control {
        for ch in prefix.chars() {
            if ch.is_control() {
                out.push(' ');
            } else {
                out.push(ch);
            }
        }
    } else {
        out.push_str(prefix);
    }
    if truncated {
        out.push('…');
    }
    Some(out)
}

/// Returns a single-line, length-capped [`SharedString`] preview of `arc`.
///
/// When `arc` fits within `max_chars` and contains no control characters,
/// returns `SharedString::from(Arc::clone(arc))` with zero heap allocation.
#[inline]
pub fn display_preview_shared(arc: &Arc<str>, max_chars: usize) -> SharedString {
    match cap_single_line_preview(arc, max_chars) {
        Some(capped) => SharedString::from(capped),
        None => SharedString::from(Arc::clone(arc)),
    }
}
