//! Column catalog, cached localized labels, row-div builder, and viewport fingerprinting
//! for [`super::ProcessesView`].

use std::ops::Range;

use gpui_kit::{Div, Hsla, InteractiveElement as _, SharedString, Stateful, Styled as _, div};
use loom_types::{
    PlatformProfile, ProcessCategory, ProcessKey, ProcessRowKind, ProcessRowProjection,
    ProcessSortColumn, ProcessTableSlice, ResourceDisplayMode, WebView2Role,
};
use loom_ui_kit::{
    HeatmapLevel, StatusLabels, quantize_bytes_display, quantize_net_bps_display,
    quantize_rate_bps_display,
};

use crate::features::table_columns::{
    ColumnKey, ColumnSpec, FnvHasher64, TableColumnLayout, WebView2RoleLabels,
};
use crate::i18n;

use super::state::stable_row_id;

pub(super) const PROCESS_HEADER_HEIGHT: f32 = 32.0;

/// Logical columns supported by [`super::ProcessesView`] (distinct from [`crate::features::details`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProcessColumn {
    Name,
    Category,
    Pid,
    ProcessName,
    Status,
    Cpu,
    Memory,
    Disk,
    Network,
    Gpu,
    Publisher,
    CommandLine,
}

impl ColumnKey for ProcessColumn {
    fn slug(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Category => "type",
            Self::Pid => "pid",
            Self::ProcessName => "process_name",
            Self::Status => "status",
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::Disk => "disk",
            Self::Network => "network",
            Self::Gpu => "gpu",
            Self::Publisher => "publisher",
            Self::CommandLine => "command_line",
        }
    }
}

impl ProcessColumn {
    /// Canonical toggleable column order presented in the **Select columns** dialog.
    pub(super) const SELECTABLE_ORDER: [Self; 11] = [
        Self::Category,
        Self::Pid,
        Self::ProcessName,
        Self::Status,
        Self::Publisher,
        Self::Cpu,
        Self::Memory,
        Self::Disk,
        Self::Network,
        Self::Gpu,
        Self::CommandLine,
    ];

    pub(super) fn default_specs() -> Vec<ColumnSpec<Self>> {
        vec![
            ColumnSpec::pinned(Self::Name, 260.0, 140.0, 720.0),
            ColumnSpec::hidden(Self::Category, 112.0, 72.0, 240.0),
            ColumnSpec::hidden(Self::Pid, 76.0, 56.0, 160.0),
            ColumnSpec::hidden(Self::ProcessName, 168.0, 96.0, 360.0),
            ColumnSpec::new(Self::Status, 80.0, 56.0, 200.0),
            ColumnSpec::new(Self::Cpu, 82.0, 60.0, 180.0),
            ColumnSpec::new(Self::Memory, 96.0, 68.0, 220.0),
            ColumnSpec::new(Self::Disk, 90.0, 68.0, 220.0),
            ColumnSpec::new(Self::Network, 92.0, 68.0, 220.0),
            ColumnSpec::new(Self::Gpu, 78.0, 60.0, 180.0),
            ColumnSpec::new(Self::Publisher, 176.0, 88.0, 480.0),
            ColumnSpec::hidden(Self::CommandLine, 320.0, 140.0, 1200.0),
        ]
    }

    pub(super) fn label(self) -> String {
        match self {
            Self::Name => i18n::tr("processes.col_name"),
            Self::Category => i18n::tr("processes.col_type"),
            Self::Pid => i18n::tr("processes.col_pid"),
            Self::ProcessName => i18n::tr("processes.col_process_name"),
            Self::Status => i18n::tr("processes.col_status"),
            Self::Cpu => i18n::tr("processes.col_cpu"),
            Self::Memory => i18n::tr("processes.col_memory"),
            Self::Disk => i18n::tr("processes.col_disk"),
            Self::Network => i18n::tr("processes.col_network"),
            Self::Gpu => i18n::tr("processes.col_gpu"),
            Self::Publisher => i18n::tr("processes.col_publisher"),
            Self::CommandLine => i18n::tr("processes.col_command_line"),
        }
    }

    pub(super) const fn sort_column(self) -> Option<ProcessSortColumn> {
        match self {
            Self::Name => Some(ProcessSortColumn::Name),
            Self::Category => Some(ProcessSortColumn::Category),
            Self::Pid => Some(ProcessSortColumn::Pid),
            Self::ProcessName => Some(ProcessSortColumn::ProcessName),
            Self::Status => Some(ProcessSortColumn::Status),
            Self::Cpu => Some(ProcessSortColumn::Cpu),
            Self::Memory => Some(ProcessSortColumn::Memory),
            Self::Disk => Some(ProcessSortColumn::Disk),
            Self::Network => Some(ProcessSortColumn::Network),
            Self::Gpu => Some(ProcessSortColumn::Gpu),
            Self::Publisher => Some(ProcessSortColumn::Publisher),
            Self::CommandLine => Some(ProcessSortColumn::CommandLine),
        }
    }

    pub(super) const fn right_aligned(self) -> bool {
        matches!(
            self,
            Self::Pid | Self::Cpu | Self::Memory | Self::Disk | Self::Network | Self::Gpu
        )
    }

    pub(super) const fn is_heatmap(self) -> bool {
        matches!(
            self,
            Self::Cpu | Self::Memory | Self::Disk | Self::Network | Self::Gpu
        )
    }
}

#[inline]
pub(super) fn find_row_index_fast(
    rows: &[ProcessRowProjection],
    key: Option<ProcessKey>,
    hint_idx: Option<usize>,
) -> Option<usize> {
    let target = key?;
    if let Some(idx) = hint_idx
        && let Some(row) = rows.get(idx)
        && row.kind == ProcessRowKind::Process
        && row.key == target
    {
        return Some(idx);
    }
    rows.iter()
        .position(|row| row.kind == ProcessRowKind::Process && row.key == target)
}

/// Builds the row container returned by `TableDelegate::render_tr`.
///
/// Must not attach `.hover(...)` because `TableState::render_table_row` unconditionally
/// registers its own hover style on the returned element.
#[inline]
pub(super) fn build_process_row_div(
    row: Option<(ProcessRowKind, ProcessKey)>,
    row_ix: usize,
    section_bg: Hsla,
) -> Stateful<Div> {
    let Some((kind, key)) = row else {
        return div().id(("proc-empty", row_ix));
    };
    match kind {
        ProcessRowKind::SectionHeader { .. } => {
            div().id(("proc-section", row_ix)).bg(section_bg).text_xs()
        }
        ProcessRowKind::Process => div().id(("proc-row", stable_row_id(key))).text_xs(),
    }
}

/// Cached localized strings for [`super::delegate::ProcessesTableDelegate`] to avoid
/// per-frame `i18n::tr` lookups.
pub(super) struct ProcessesLocaleCache {
    col_labels: [SharedString; 12],
    pub(super) sort_paused_tooltip: SharedString,
    pub(super) status_labels: StatusLabels,
    pub(super) status_elevated: SharedString,
    pub(super) status_running: SharedString,
    pub(super) leak_warning_prefix: SharedString,
    cat_app: SharedString,
    cat_background: SharedString,
    cat_system: SharedString,
    pub(super) empty_search: SharedString,
    wv2: WebView2RoleLabels,
}

impl ProcessesLocaleCache {
    pub(super) fn build(platform: PlatformProfile) -> Self {
        Self {
            col_labels: [
                ProcessColumn::Name.label().into(),
                ProcessColumn::Category.label().into(),
                ProcessColumn::Pid.label().into(),
                ProcessColumn::ProcessName.label().into(),
                ProcessColumn::Status.label().into(),
                ProcessColumn::Cpu.label().into(),
                ProcessColumn::Memory.label().into(),
                ProcessColumn::Disk.label().into(),
                ProcessColumn::Network.label().into(),
                ProcessColumn::Gpu.label().into(),
                ProcessColumn::Publisher.label().into(),
                ProcessColumn::CommandLine.label().into(),
            ],
            sort_paused_tooltip: i18n::tr("proc_ux_sort_paused_tooltip").into(),
            status_labels: StatusLabels::new(
                i18n::tr("processes.status_efficiency"),
                i18n::tr("processes.status_suspended"),
                i18n::tr("processes.status_not_responding"),
            ),
            status_elevated: i18n::tr("processes.status_elevated").into(),
            status_running: i18n::tr("details.status_running").into(),
            leak_warning_prefix: i18n::tr("leak_warning_badge_prefix").into(),
            cat_app: i18n::process_category_label(ProcessCategory::App, platform).into(),
            cat_background: i18n::process_category_label(ProcessCategory::Background, platform)
                .into(),
            cat_system: i18n::process_category_label(ProcessCategory::System, platform).into(),
            empty_search: i18n::tr("processes.empty_search").into(),
            wv2: WebView2RoleLabels::build(),
        }
    }

    #[inline]
    pub(super) fn column_label(&self, col: ProcessColumn) -> SharedString {
        self.col_labels[col as usize].clone()
    }

    #[inline]
    pub(super) fn category_label(&self, category: ProcessCategory) -> SharedString {
        match category {
            ProcessCategory::App => self.cat_app.clone(),
            ProcessCategory::Background => self.cat_background.clone(),
            ProcessCategory::System => self.cat_system.clone(),
        }
    }

    #[inline]
    pub(super) fn webview2_label(&self, role: WebView2Role) -> SharedString {
        self.wv2.get(role)
    }
}

/// Computes a fast 64-bit viewport fingerprint over the currently visible rows and columns
/// of a [`ProcessTableSlice`] so unchanged live ticks can skip `cx.notify()`.
#[allow(clippy::too_many_arguments)]
pub(super) fn compute_processes_viewport_fingerprint(
    slice: &ProcessTableSlice,
    column_layout: &TableColumnLayout<ProcessColumn>,
    visible_rows: &Range<usize>,
    visible_cols: &Range<usize>,
    selected_idx: Option<usize>,
    mem_mode: ResourceDisplayMode,
    disk_mode: ResourceDisplayMode,
    net_mode: ResourceDisplayMode,
) -> u64 {
    let mut h = FnvHasher64::new();
    let rows_len = slice.rows.len();
    let start = visible_rows.start.min(rows_len);
    let end = visible_rows.end.min(rows_len).max(start);
    let cols_mask = column_layout.visible_cols_mask(visible_cols);

    h.write_usize(rows_len);
    h.write_u8(slice.view_mode as u8);
    h.write_u8(slice.sort_column as u8);
    h.write_u8(slice.sort_direction as u8);
    h.write_usize(selected_idx.unwrap_or(usize::MAX));
    h.write_u8(mem_mode as u8);
    h.write_u8(disk_mode as u8);
    h.write_u8(net_mode as u8);
    h.write_usize(start);
    h.write_usize(end);
    h.write_u64(cols_mask);

    let visible_specs = column_layout.visible_columns();
    for row in &slice.rows[start..end] {
        match row.kind {
            ProcessRowKind::SectionHeader { category, count } => {
                h.write_u8(0);
                h.write_u8(category as u8);
                h.write_usize(count);
            }
            ProcessRowKind::Process => {
                h.write_u8(1);
                h.write_u32(row.key.pid);
                h.write_u64(row.key.create_time_100ns);
                h.write_u16(row.depth);
                h.write_bool(row.has_children);
                h.write_bool(row.is_expanded);

                for (col_ix, spec) in visible_specs.iter().enumerate() {
                    if col_ix < 64 && (cols_mask & (1u64 << col_ix)) == 0 {
                        continue;
                    }
                    match spec.id {
                        ProcessColumn::Name => {
                            h.write_str(&row.display_name);
                            h.write_u32(row.child_count);
                            h.write_u8(row.webview2_role.map_or(u8::MAX, |r| r as u8));
                            if let Some(icon) = row.icon.as_ref() {
                                h.write_usize(icon.rgba.as_ptr() as usize);
                                h.write_usize(icon.rgba.len());
                            } else {
                                h.write_u8(0);
                            }
                        }
                        ProcessColumn::Category => {
                            h.write_u8(row.category as u8);
                        }
                        ProcessColumn::Pid => {}
                        ProcessColumn::ProcessName => {
                            h.write_str(&row.name);
                        }
                        ProcessColumn::Status => {
                            h.write_u16(row.status.bits());
                            if let Some(w) = row.leak_warning.as_ref() {
                                h.write_u8(1);
                                h.write_u8(w.kind as u8);
                                h.write_u64(w.current_value);
                                h.write_u64(w.baseline_value);
                                h.write_str(&w.summary);
                            } else {
                                h.write_u8(0);
                            }
                        }
                        ProcessColumn::Cpu => {
                            h.write_u16(row.cpu_permille);
                        }
                        ProcessColumn::Memory => match mem_mode {
                            ResourceDisplayMode::Percent => {
                                h.write_u16(row.memory_permille);
                            }
                            ResourceDisplayMode::Values => {
                                h.write_u64(quantize_bytes_display(row.memory_bytes));
                                h.write_u8(HeatmapLevel::from_permille(row.memory_permille) as u8);
                            }
                        },
                        ProcessColumn::Disk => {
                            match disk_mode {
                                ResourceDisplayMode::Percent => {
                                    h.write_u16(row.disk_permille);
                                }
                                ResourceDisplayMode::Values => {
                                    h.write_u64(quantize_rate_bps_display(row.disk_bytes_per_sec));
                                }
                            }
                            h.write_u8(
                                HeatmapLevel::from_bytes_per_sec(row.disk_bytes_per_sec) as u8
                            );
                        }
                        ProcessColumn::Network => {
                            match net_mode {
                                ResourceDisplayMode::Percent => {
                                    h.write_u16(row.net_permille);
                                }
                                ResourceDisplayMode::Values => {
                                    h.write_u64(quantize_net_bps_display(row.net_bytes_per_sec));
                                }
                            }
                            h.write_u8(
                                HeatmapLevel::from_bytes_per_sec(row.net_bytes_per_sec) as u8
                            );
                        }
                        ProcessColumn::Gpu => {
                            h.write_u16(row.gpu_util_permille);
                        }
                        ProcessColumn::Publisher => {
                            if let Some(p) = row.publisher.as_deref() {
                                h.write_u8(1);
                                h.write_str(p);
                            } else {
                                h.write_u8(0);
                            }
                        }
                        ProcessColumn::CommandLine => {
                            if let Some(c) = row.command_line.as_deref() {
                                h.write_u8(1);
                                h.write_preview_str(c);
                            } else {
                                h.write_u8(0);
                            }
                        }
                    }
                }
            }
        }
    }

    h.finish()
}
