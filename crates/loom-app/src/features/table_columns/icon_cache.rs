//! Bounded GPU [`RenderImage`] icon cache with per-frame upload budgeting and
//! shared next-frame catch-up scheduling for virtualized table views.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use gpui_kit::{Context, RenderImage, Window};
use loom_types::ExeIconRgba;
use loom_ui_kit::rgba_icon_to_render_image;

use super::telemetry::{ListViewTelemetryKind, list_telemetry};

/// Maximum number of cached GPUI [`RenderImage`] icons retained by [`IconRenderCache`].
pub(crate) const MAX_CACHED_ICONS: usize = 512;

/// Maximum number of newly decoded icons converted into GPU [`RenderImage`] handles within
/// a single UI frame before remaining uncached icons defer to the next frame.
///
/// Prevents frame-time spikes when rapidly scrolling past dozens of never-seen processes;
/// cache hits consume zero budget.
pub(crate) const MAX_ICON_UPLOADS_PER_FRAME: usize = 6;

/// Outcome of a budgeted [`IconRenderCache::get_or_insert_budgeted`] lookup.
#[derive(Clone)]
pub(crate) enum BudgetedIconLookup {
    /// Icon was already cached (consumed zero frame upload budget).
    Hit(Arc<RenderImage>),
    /// Icon was converted and uploaded within the current frame's budget.
    Uploaded(Arc<RenderImage>),
    /// Per-frame upload budget was exhausted; caller should render a lightweight placeholder
    /// and schedule a next-frame catch-up pass.
    Deferred,
    /// Underlying RGBA payload was invalid and could not be converted.
    Invalid,
}

/// Unique key identifying an [`ExeIconRgba`] buffer by pointer address and dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct IconCacheKey {
    ptr: usize,
    width: u16,
    height: u16,
    len: usize,
}

impl IconCacheKey {
    #[inline]
    fn from_icon(icon: &ExeIconRgba) -> Self {
        Self {
            ptr: icon.rgba.as_ptr() as usize,
            width: icon.width,
            height: icon.height,
            len: icon.rgba.len(),
        }
    }
}

struct IconCacheEntry {
    /// Weak reference to the originating `Arc<[u8]>` to guard against ABA address reuse.
    source: Weak<[u8]>,
    image: Arc<RenderImage>,
}

/// Bounded local cache mapping unique [`ExeIconRgba`] buffers to GPUI [`RenderImage`] handles
/// with optional per-frame upload budgeting to avoid GPU texture upload spikes during fast scrolling.
///
/// Prevents re-swapping RGBA -> BGRA pixels and re-uploading textures on every frame.
pub(crate) struct IconRenderCache {
    entries: HashMap<IconCacheKey, IconCacheEntry>,
    max_entries: usize,
    max_uploads_per_frame: usize,
    uploads_this_frame: usize,
    deferred_this_frame: bool,
}

impl Default for IconRenderCache {
    fn default() -> Self {
        Self::new()
    }
}

impl IconRenderCache {
    /// Creates a new bounded [`IconRenderCache`] capped at [`MAX_CACHED_ICONS`] and
    /// [`MAX_ICON_UPLOADS_PER_FRAME`] new uploads per frame.
    pub(crate) fn new() -> Self {
        Self::with_limits(MAX_CACHED_ICONS, MAX_ICON_UPLOADS_PER_FRAME)
    }

    /// Creates a new bounded [`IconRenderCache`] with a custom capacity ceiling.
    #[cfg(test)]
    pub(crate) fn with_capacity(max_entries: usize) -> Self {
        Self::with_limits(max_entries, MAX_ICON_UPLOADS_PER_FRAME)
    }

    /// Creates a new bounded [`IconRenderCache`] with custom capacity and per-frame upload budget.
    pub(crate) fn with_limits(max_entries: usize, max_uploads_per_frame: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_entries: max_entries.max(1),
            max_uploads_per_frame: max_uploads_per_frame.max(1),
            uploads_this_frame: 0,
            deferred_this_frame: false,
        }
    }

    /// Resets the per-frame GPU icon upload budget at the start of a new frame.
    #[inline]
    pub(crate) fn begin_frame(&mut self) {
        self.uploads_this_frame = 0;
        self.deferred_this_frame = false;
    }

    /// Returns whether at least one uncached icon was deferred during the current frame.
    #[cfg(test)]
    #[inline]
    pub(crate) const fn has_deferred_uploads(&self) -> bool {
        self.deferred_this_frame
    }

    /// Returns a cached [`Arc<RenderImage>`] for `icon`, converting it unconditionally on miss.
    #[cfg(test)]
    pub(crate) fn get_or_insert(&mut self, icon: &ExeIconRgba) -> Option<Arc<RenderImage>> {
        let key = IconCacheKey::from_icon(icon);
        if let Some(entry) = self.entries.get(&key)
            && entry
                .source
                .upgrade()
                .is_some_and(|existing| Arc::ptr_eq(&existing, &icon.rgba))
        {
            return Some(Arc::clone(&entry.image));
        }

        let rendered = rgba_icon_to_render_image(icon)?;

        if self.entries.len() >= self.max_entries {
            self.prune();
        }

        self.entries.insert(
            key,
            IconCacheEntry {
                source: Arc::downgrade(&icon.rgba),
                image: Arc::clone(&rendered),
            },
        );
        Some(rendered)
    }

    /// Returns a cached [`Arc<RenderImage>`] for `icon` while enforcing the per-frame upload budget.
    ///
    /// Cache hits always return [`BudgetedIconLookup::Hit`] without consuming budget. Cache misses
    /// beyond `max_uploads_per_frame` return [`BudgetedIconLookup::Deferred`] so the caller can
    /// render a lightweight placeholder icon and schedule a next-frame catch-up pass.
    pub(crate) fn get_or_insert_budgeted(&mut self, icon: &ExeIconRgba) -> BudgetedIconLookup {
        let key = IconCacheKey::from_icon(icon);
        if let Some(entry) = self.entries.get(&key)
            && entry
                .source
                .upgrade()
                .is_some_and(|existing| Arc::ptr_eq(&existing, &icon.rgba))
        {
            return BudgetedIconLookup::Hit(Arc::clone(&entry.image));
        }

        if self.uploads_this_frame >= self.max_uploads_per_frame {
            self.deferred_this_frame = true;
            return BudgetedIconLookup::Deferred;
        }

        let Some(rendered) = rgba_icon_to_render_image(icon) else {
            return BudgetedIconLookup::Invalid;
        };

        if self.entries.len() >= self.max_entries {
            self.prune();
        }

        self.entries.insert(
            key,
            IconCacheEntry {
                source: Arc::downgrade(&icon.rgba),
                image: Arc::clone(&rendered),
            },
        );
        self.uploads_this_frame += 1;
        BudgetedIconLookup::Uploaded(rendered)
    }

    /// Reclaims entries whose underlying `ExeIconRgba` buffer was dropped, or clears
    /// the cache if all live entries exceed `max_entries`.
    fn prune(&mut self) {
        self.entries
            .retain(|_, entry| entry.source.strong_count() > 0);
        if self.entries.len() >= self.max_entries {
            self.entries.clear();
        }
    }

    /// Returns the number of cached entries currently retained.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Resolves an optional process icon through [`IconRenderCache::get_or_insert_budgeted`],
/// records upload/defer counters in [`list_telemetry`], and schedules a single next-frame
/// catch-up pass on `window` when the per-frame upload budget is exhausted.
pub(crate) fn resolve_budgeted_icon<T: 'static>(
    icon: Option<&ExeIconRgba>,
    cache: &mut IconRenderCache,
    catchup_scheduled: &mut bool,
    telemetry_kind: ListViewTelemetryKind,
    window: &mut Window,
    cx: &mut Context<T>,
    on_catchup: impl FnOnce(&mut T, &mut Context<T>) + 'static,
) -> Option<Arc<RenderImage>> {
    let icon = icon?;
    match cache.get_or_insert_budgeted(icon) {
        BudgetedIconLookup::Hit(img) => Some(img),
        BudgetedIconLookup::Uploaded(img) => {
            list_telemetry().record_icon_upload(telemetry_kind);
            Some(img)
        }
        BudgetedIconLookup::Deferred => {
            list_telemetry().record_icon_defer(telemetry_kind);
            if !*catchup_scheduled {
                *catchup_scheduled = true;
                let weak = cx.entity().downgrade();
                window.on_next_frame(move |_window, cx| {
                    let _ = weak.update(cx, |entity, cx| {
                        on_catchup(entity, cx);
                        cx.notify();
                    });
                });
            }
            None
        }
        BudgetedIconLookup::Invalid => None,
    }
}
