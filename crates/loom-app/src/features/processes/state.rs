//! Pure state helpers, selection predicates, and bounded icon render cache for Processes view.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use gpui_kit::RenderImage;
use loom_types::{
    ExeIconRgba, ProcessCategory, ProcessKey, ProcessRowKind, ProcessRowProjection,
    ProcessSortColumn, ProcessStatusFlags, ResourceDisplayMode, SortDirection,
};
use loom_ui_kit::rgba_icon_to_render_image;

/// Maximum number of cached GPUI [`RenderImage`] icons retained by [`IconRenderCache`].
pub(crate) const MAX_CACHED_ICONS: usize = 512;

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

/// Bounded local cache mapping unique [`ExeIconRgba`] buffers to GPUI [`RenderImage`] handles.
///
/// Prevents re-swapping RGBA -> BGRA pixels and re-uploading textures on every frame.
#[derive(Default)]
pub(crate) struct IconRenderCache {
    entries: HashMap<IconCacheKey, IconCacheEntry>,
    max_entries: usize,
}

impl IconRenderCache {
    /// Creates a new bounded [`IconRenderCache`] capped at [`MAX_CACHED_ICONS`].
    pub(crate) fn new() -> Self {
        Self::with_capacity(MAX_CACHED_ICONS)
    }

    /// Creates a new bounded [`IconRenderCache`] with a custom capacity ceiling.
    pub(crate) fn with_capacity(max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_entries: max_entries.max(1),
        }
    }

    /// Returns a cached [`Arc<RenderImage>`] for `icon`, converting it once on cache miss.
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

/// Resource columns whose formatting can be toggled between `Values` and `Percent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResourceColumn {
    Memory,
    Disk,
    Network,
}

/// Updates one resource column's [`ResourceDisplayMode`] while leaving the other two unchanged.
#[inline]
pub(crate) const fn apply_resource_display_mode(
    memory: ResourceDisplayMode,
    disk: ResourceDisplayMode,
    network: ResourceDisplayMode,
    column: ResourceColumn,
    mode: ResourceDisplayMode,
) -> (
    ResourceDisplayMode,
    ResourceDisplayMode,
    ResourceDisplayMode,
) {
    match column {
        ResourceColumn::Memory => (mode, disk, network),
        ResourceColumn::Disk => (memory, mode, network),
        ResourceColumn::Network => (memory, disk, mode),
    }
}

/// Computes the next [`SortDirection`] when clicking a sortable Processes table column header.
#[inline]
pub(crate) fn next_sort_direction(
    active_column: ProcessSortColumn,
    active_direction: SortDirection,
    clicked_column: ProcessSortColumn,
) -> SortDirection {
    if active_column == clicked_column {
        active_direction.toggle()
    } else {
        match clicked_column {
            ProcessSortColumn::Name => SortDirection::Ascending,
            ProcessSortColumn::Status
            | ProcessSortColumn::Cpu
            | ProcessSortColumn::Memory
            | ProcessSortColumn::Disk
            | ProcessSortColumn::Network
            | ProcessSortColumn::Gpu => SortDirection::Descending,
        }
    }
}

/// Looks up the currently selected process row strictly by [`ProcessKey`] (never row index).
pub(crate) fn find_selected_row(
    rows: &[ProcessRowProjection],
    selected_key: Option<ProcessKey>,
) -> Option<&ProcessRowProjection> {
    let key = selected_key?;
    rows.iter()
        .find(|row| row.kind == ProcessRowKind::Process && row.key == key)
}

/// Returns `true` when `row` can be targeted by End Task (excludes System Idle / PID 0).
#[inline]
pub(crate) fn can_terminate_process(row: &ProcessRowProjection) -> bool {
    row.kind == ProcessRowKind::Process
        && row.key.pid != 0
        && !row.status.contains(ProcessStatusFlags::SYSTEM_IDLE)
}

/// Returns `true` when terminating `row` requires a confirmation modal dialog.
///
/// Windows system processes, critical processes, and OS-protected processes require
/// explicit confirmation before issuing `TerminateProcess`.
#[inline]
pub(crate) fn requires_end_task_confirmation(row: &ProcessRowProjection) -> bool {
    row.category == ProcessCategory::Windows
        || row
            .status
            .intersects(ProcessStatusFlags::CRITICAL | ProcessStatusFlags::PROTECTED)
}

/// Returns `true` when `row` is eligible for Efficiency Mode (`EcoQoS`) toggling.
///
/// System Idle (PID 0), System kernel (PID 4), `SYSTEM_IDLE`, and `PROTECTED`
/// processes do not allow EcoQoS toggling and must disable the control.
#[inline]
pub(crate) fn is_efficiency_mode_eligible(row: &ProcessRowProjection) -> bool {
    row.kind == ProcessRowKind::Process
        && row.key.pid != 0
        && row.key.pid != 4
        && !row
            .status
            .intersects(ProcessStatusFlags::SYSTEM_IDLE | ProcessStatusFlags::PROTECTED)
}

/// Returns a subtle secondary text label (image name or window title) when distinct from `display_name`.
pub(crate) fn secondary_subtitle(row: &ProcessRowProjection) -> Option<&str> {
    let display = row.display_name.trim();
    let image = row.name.trim();
    if !image.is_empty() && !display.eq_ignore_ascii_case(image) {
        return Some(image);
    }
    if let Some(title) = row.window_title.as_deref().map(str::trim)
        && !title.is_empty()
        && !display.eq_ignore_ascii_case(title)
    {
        return Some(title);
    }
    None
}

/// Returns the best query string for "Search online" on a process row.
pub(crate) fn search_online_query(row: &ProcessRowProjection) -> String {
    let image = row.name.trim();
    if !image.is_empty() {
        image.to_owned()
    } else {
        row.display_name.trim().to_owned()
    }
}

/// Computes a deterministic, allocation-free element ID discriminator from [`ProcessKey`].
#[inline]
pub(crate) fn stable_row_id(key: ProcessKey) -> usize {
    let mixed = u64::from(key.pid).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ key.create_time_100ns.rotate_left(17);
    mixed as usize
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use loom_types::{
        ExeIconRgba, ProcessCategory, ProcessKey, ProcessRowKind, ProcessRowProjection,
        ProcessSortColumn, ProcessStatusFlags, ResourceDisplayMode, SortDirection,
    };

    use super::*;

    fn sample_process_row(
        pid: u32,
        create_time: u64,
        name: &str,
        display_name: &str,
        category: ProcessCategory,
        status: ProcessStatusFlags,
    ) -> ProcessRowProjection {
        ProcessRowProjection {
            kind: ProcessRowKind::Process,
            key: ProcessKey::new(pid, create_time),
            depth: 0,
            has_children: false,
            is_expanded: false,
            child_count: 0,
            name: Arc::from(name),
            display_name: Arc::from(display_name),
            window_title: None,
            publisher: None,
            exe_path: None,
            icon: None,
            category,
            status,
            cpu_permille: 0,
            memory_bytes: 0,
            memory_permille: 0,
            disk_bytes_per_sec: 0,
            disk_permille: 0,
            net_bytes_per_sec: 0,
            net_permille: 0,
            gpu_util_permille: 0,
            gpu_dedicated_bytes: 0,
        }
    }

    fn sample_section_header(category: ProcessCategory, count: usize) -> ProcessRowProjection {
        let mut row = sample_process_row(0, 0, "", "", category, ProcessStatusFlags::empty());
        row.kind = ProcessRowKind::SectionHeader { category, count };
        row
    }

    #[test]
    fn process_key_selection_stays_anchored_when_rows_reorder_or_pid_reused() {
        let target_key = ProcessKey::new(4242, 100_000);
        let other_key = ProcessKey::new(1337, 90_000);
        let recycled_pid_key = ProcessKey::new(4242, 200_000);

        let initial_rows = vec![
            sample_section_header(ProcessCategory::App, 2),
            sample_process_row(
                other_key.pid,
                other_key.create_time_100ns,
                "alpha.exe",
                "Alpha",
                ProcessCategory::App,
                ProcessStatusFlags::empty(),
            ),
            sample_process_row(
                target_key.pid,
                target_key.create_time_100ns,
                "target.exe",
                "Target App",
                ProcessCategory::App,
                ProcessStatusFlags::empty(),
            ),
        ];

        // Initially at index 2.
        let found_initial = find_selected_row(&initial_rows, Some(target_key))
            .expect("target should be found at initial position");
        assert_eq!(found_initial.key, target_key);
        assert_eq!(found_initial.display_name.as_ref(), "Target App");

        // After live re-sort, target moves to index 0 (and another process takes index 2).
        let reordered_rows = vec![
            sample_process_row(
                target_key.pid,
                target_key.create_time_100ns,
                "target.exe",
                "Target App",
                ProcessCategory::App,
                ProcessStatusFlags::empty(),
            ),
            sample_section_header(ProcessCategory::Background, 1),
            sample_process_row(
                other_key.pid,
                other_key.create_time_100ns,
                "alpha.exe",
                "Alpha",
                ProcessCategory::Background,
                ProcessStatusFlags::empty(),
            ),
        ];

        let found_reordered = find_selected_row(&reordered_rows, Some(target_key))
            .expect("target must stay anchored to ProcessKey after re-sort");
        assert_eq!(found_reordered.key, target_key);
        assert_eq!(found_reordered.display_name.as_ref(), "Target App");

        // If the original process exits and the OS recycles PID 4242 with a newer create_time,
        // selection MUST NOT silently attach to the recycled PID.
        let recycled_rows = vec![sample_process_row(
            recycled_pid_key.pid,
            recycled_pid_key.create_time_100ns,
            "new_process.exe",
            "New Process",
            ProcessCategory::App,
            ProcessStatusFlags::empty(),
        )];
        assert!(find_selected_row(&recycled_rows, Some(target_key)).is_none());
    }

    #[test]
    fn eco_mode_eligibility_blocks_system_idle_pid4_and_protected_processes() {
        let idle = sample_process_row(
            0,
            0,
            "System Idle Process",
            "System Idle Process",
            ProcessCategory::Windows,
            ProcessStatusFlags::SYSTEM_IDLE,
        );
        let kernel = sample_process_row(
            4,
            1,
            "System",
            "System",
            ProcessCategory::Windows,
            ProcessStatusFlags::empty(),
        );
        let protected = sample_process_row(
            812,
            500,
            "csrss.exe",
            "Client Server Runtime Process",
            ProcessCategory::Windows,
            ProcessStatusFlags::PROTECTED | ProcessStatusFlags::CRITICAL,
        );
        let system_idle_flagged = sample_process_row(
            999,
            500,
            "idle_stub",
            "Idle Stub",
            ProcessCategory::Windows,
            ProcessStatusFlags::SYSTEM_IDLE,
        );
        let normal_app = sample_process_row(
            5120,
            80_000,
            "notepad.exe",
            "Notepad",
            ProcessCategory::App,
            ProcessStatusFlags::empty(),
        );
        let already_eco_bg = sample_process_row(
            6120,
            90_000,
            "worker.exe",
            "Worker",
            ProcessCategory::Background,
            ProcessStatusFlags::EFFICIENCY_MODE,
        );

        assert!(!is_efficiency_mode_eligible(&idle));
        assert!(!is_efficiency_mode_eligible(&kernel));
        assert!(!is_efficiency_mode_eligible(&protected));
        assert!(!is_efficiency_mode_eligible(&system_idle_flagged));
        assert!(is_efficiency_mode_eligible(&normal_app));
        assert!(is_efficiency_mode_eligible(&already_eco_bg));

        assert!(!can_terminate_process(&idle));
        assert!(can_terminate_process(&normal_app));
        assert!(requires_end_task_confirmation(&kernel));
        assert!(requires_end_task_confirmation(&protected));
        assert!(!requires_end_task_confirmation(&normal_app));
        assert!(!requires_end_task_confirmation(&already_eco_bg));
    }

    #[test]
    fn icon_render_cache_deduplicates_same_arc_and_respects_capacity() {
        let mut cache = IconRenderCache::with_capacity(2);

        let icon_a = ExeIconRgba {
            width: 2,
            height: 2,
            rgba: Arc::from([0x10u8; 16]),
        };
        let icon_a_clone = icon_a.clone();
        let icon_b = ExeIconRgba {
            width: 2,
            height: 2,
            rgba: Arc::from([0x20u8; 16]),
        };
        let icon_c = ExeIconRgba {
            width: 2,
            height: 2,
            rgba: Arc::from([0x30u8; 16]),
        };

        let first = cache
            .get_or_insert(&icon_a)
            .expect("valid RGBA icon renders");
        let second = cache
            .get_or_insert(&icon_a_clone)
            .expect("cloned Arc hits cache");
        assert!(
            Arc::ptr_eq(&first, &second),
            "identical icon Arc must reuse the exact same RenderImage without re-converting"
        );
        assert_eq!(cache.len(), 1);

        let img_b = cache.get_or_insert(&icon_b).expect("second unique icon");
        assert!(!Arc::ptr_eq(&first, &img_b));
        assert_eq!(cache.len(), 2);

        // Inserting a third live icon triggers bounded eviction so cache never grows unbounded.
        let _img_c = cache.get_or_insert(&icon_c).expect("third icon");
        assert!(cache.len() <= 2);

        // Invalid icons return None and do not pollute the cache.
        let invalid = ExeIconRgba {
            width: 0,
            height: 0,
            rgba: Arc::from([]),
        };
        assert!(cache.get_or_insert(&invalid).is_none());
    }

    #[test]
    fn resource_display_mode_toggle_updates_only_selected_column() {
        let (m, d, n) = apply_resource_display_mode(
            ResourceDisplayMode::Values,
            ResourceDisplayMode::Values,
            ResourceDisplayMode::Values,
            ResourceColumn::Memory,
            ResourceDisplayMode::Percent,
        );
        assert_eq!(
            (m, d, n),
            (
                ResourceDisplayMode::Percent,
                ResourceDisplayMode::Values,
                ResourceDisplayMode::Values
            )
        );

        let (m, d, n) = apply_resource_display_mode(
            m,
            d,
            n,
            ResourceColumn::Disk,
            ResourceDisplayMode::Percent,
        );
        assert_eq!(
            (m, d, n),
            (
                ResourceDisplayMode::Percent,
                ResourceDisplayMode::Percent,
                ResourceDisplayMode::Values
            )
        );

        let (m, d, n) = apply_resource_display_mode(
            m,
            d,
            n,
            ResourceColumn::Network,
            ResourceDisplayMode::Percent,
        );
        assert_eq!(
            (m, d, n),
            (
                ResourceDisplayMode::Percent,
                ResourceDisplayMode::Percent,
                ResourceDisplayMode::Percent
            )
        );

        let (m, d, n) = apply_resource_display_mode(
            m,
            d,
            n,
            ResourceColumn::Memory,
            ResourceDisplayMode::Values,
        );
        assert_eq!(
            (m, d, n),
            (
                ResourceDisplayMode::Values,
                ResourceDisplayMode::Percent,
                ResourceDisplayMode::Percent
            )
        );
    }

    #[test]
    fn next_sort_direction_toggles_active_and_defaults_appropriately() {
        // Clicking the currently active column flips its direction.
        assert_eq!(
            next_sort_direction(
                ProcessSortColumn::Cpu,
                SortDirection::Descending,
                ProcessSortColumn::Cpu
            ),
            SortDirection::Ascending
        );
        assert_eq!(
            next_sort_direction(
                ProcessSortColumn::Cpu,
                SortDirection::Ascending,
                ProcessSortColumn::Cpu
            ),
            SortDirection::Descending
        );

        // Switching to Name defaults to Ascending; switching to metric columns defaults to Descending.
        assert_eq!(
            next_sort_direction(
                ProcessSortColumn::Cpu,
                SortDirection::Descending,
                ProcessSortColumn::Name
            ),
            SortDirection::Ascending
        );
        assert_eq!(
            next_sort_direction(
                ProcessSortColumn::Name,
                SortDirection::Ascending,
                ProcessSortColumn::Memory
            ),
            SortDirection::Descending
        );
    }

    #[test]
    fn secondary_subtitle_prefers_distinct_image_or_window_title() {
        let mut row = sample_process_row(
            100,
            1,
            "Code.exe",
            "Visual Studio Code",
            ProcessCategory::App,
            ProcessStatusFlags::empty(),
        );
        assert_eq!(secondary_subtitle(&row), Some("Code.exe"));

        row.display_name = Arc::from("Code.exe");
        assert_eq!(secondary_subtitle(&row), None);

        row.window_title = Some(Arc::from("loom — main.rs"));
        assert_eq!(secondary_subtitle(&row), Some("loom — main.rs"));
    }
}
