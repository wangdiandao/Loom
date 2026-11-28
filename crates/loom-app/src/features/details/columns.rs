//! Column catalog, cached localized labels, and viewport fingerprinting for
//! [`super::DetailsView`].

use std::ops::Range;

use gpui_kit::SharedString;
use loom_types::{
    CapabilityFlags, DetailsSortColumn, DetailsTableSlice, PlatformProfile, ProcessArch,
    ProcessKey, ProcessPriority, ProcessStatusFlags, UacVirtualization, WebView2Role,
};
use loom_ui_kit::{quantize_bytes_display, quantize_cpu_time_secs, quantize_signed_bytes_display};

use crate::features::table_columns::{
    ColumnKey, ColumnSpec, FnvHasher64, TableColumnLayout, WebView2RoleLabels,
};
use crate::i18n;

use super::state::memory_heatmap_level;

pub(super) const DETAILS_HEADER_HEIGHT: f32 = 32.0;

// Default column widths (px) kept wide enough so horizontal scrolling prevents cramping.
const COL_NAME_W: f32 = 240.0;
const COL_PID_W: f32 = 76.0;
const COL_STATUS_W: f32 = 96.0;
const COL_USER_W: f32 = 140.0;
const COL_SESSION_W: f32 = 84.0;
const COL_CPU_W: f32 = 82.0;
const COL_CPU_TIME_W: f32 = 96.0;
const COL_MEM_W: f32 = 184.0;
const COL_WORKING_SET_W: f32 = 128.0;
const COL_WORKING_SET_DELTA_W: f32 = 168.0;
const COL_COMMIT_W: f32 = 112.0;
const COL_PRIORITY_W: f32 = 112.0;
const COL_HANDLES_W: f32 = 84.0;
const COL_THREADS_W: f32 = 76.0;
const COL_PLATFORM_W: f32 = 86.0;
pub(super) const COL_UAC_W: f32 = 132.0;
const COL_ELEVATED_W: f32 = 76.0;
const COL_GPU_MEM_W: f32 = 144.0;
const COL_PUBLISHER_W: f32 = 164.0;
const COL_DESCRIPTION_W: f32 = 200.0;
const COL_EXE_PATH_W: f32 = 300.0;
const COL_CMDLINE_W: f32 = 360.0;

#[cfg(test)]
const TABLE_HORIZONTAL_PADDING: f32 = 16.0;

/// Logical columns supported by [`super::DetailsView`] (distinct diagnostic column set from Processes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DetailsColumn {
    Name,
    Pid,
    Status,
    UserName,
    SessionId,
    Cpu,
    CpuTime,
    Memory,
    WorkingSet,
    WorkingSetDelta,
    CommitSize,
    Priority,
    Handles,
    Threads,
    Platform,
    UacVirtualization,
    Elevated,
    GpuMemory,
    Publisher,
    Description,
    ExePath,
    CommandLine,
}

impl ColumnKey for DetailsColumn {
    fn slug(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Pid => "pid",
            Self::Status => "status",
            Self::UserName => "user",
            Self::SessionId => "session_id",
            Self::Cpu => "cpu",
            Self::CpuTime => "cpu_time",
            Self::Memory => "memory",
            Self::WorkingSet => "working_set",
            Self::WorkingSetDelta => "working_set_delta",
            Self::CommitSize => "commit_size",
            Self::Priority => "priority",
            Self::Handles => "handles",
            Self::Threads => "threads",
            Self::Platform => "platform",
            Self::UacVirtualization => "uac",
            Self::Elevated => "elevated",
            Self::GpuMemory => "gpu_memory",
            Self::Publisher => "publisher",
            Self::Description => "description",
            Self::ExePath => "exe_path",
            Self::CommandLine => "command_line",
        }
    }
}

impl DetailsColumn {
    pub(super) fn default_specs(capabilities: CapabilityFlags) -> Vec<ColumnSpec<Self>> {
        let mut cols = vec![
            ColumnSpec::pinned(Self::Name, COL_NAME_W, 140.0, 640.0),
            ColumnSpec::new(Self::Pid, COL_PID_W, 56.0, 160.0),
            ColumnSpec::new(Self::Status, COL_STATUS_W, 68.0, 200.0),
            ColumnSpec::new(Self::UserName, COL_USER_W, 80.0, 360.0),
            ColumnSpec::hidden(Self::SessionId, COL_SESSION_W, 64.0, 160.0),
            ColumnSpec::new(Self::Cpu, COL_CPU_W, 60.0, 180.0),
            ColumnSpec::hidden(Self::CpuTime, COL_CPU_TIME_W, 76.0, 200.0),
            ColumnSpec::new(Self::Memory, COL_MEM_W, 96.0, 320.0),
            ColumnSpec::hidden(Self::WorkingSet, COL_WORKING_SET_W, 88.0, 280.0),
            ColumnSpec::hidden(Self::WorkingSetDelta, COL_WORKING_SET_DELTA_W, 96.0, 320.0),
            ColumnSpec::hidden(Self::CommitSize, COL_COMMIT_W, 84.0, 260.0),
            ColumnSpec::new(Self::Priority, COL_PRIORITY_W, 72.0, 240.0),
            ColumnSpec::hidden(Self::Handles, COL_HANDLES_W, 64.0, 180.0),
            ColumnSpec::hidden(Self::Threads, COL_THREADS_W, 60.0, 160.0),
            ColumnSpec::new(Self::Platform, COL_PLATFORM_W, 64.0, 180.0),
        ];
        if capabilities.contains(CapabilityFlags::UAC_VIRTUALIZATION) {
            cols.push(ColumnSpec::new(
                Self::UacVirtualization,
                COL_UAC_W,
                84.0,
                260.0,
            ));
        }
        cols.extend([
            ColumnSpec::hidden(Self::Elevated, COL_ELEVATED_W, 60.0, 160.0),
            ColumnSpec::new(Self::GpuMemory, COL_GPU_MEM_W, 88.0, 280.0),
            ColumnSpec::new(Self::Publisher, COL_PUBLISHER_W, 88.0, 420.0),
            ColumnSpec::hidden(Self::Description, COL_DESCRIPTION_W, 100.0, 480.0),
            ColumnSpec::hidden(Self::ExePath, COL_EXE_PATH_W, 120.0, 900.0),
            ColumnSpec::new(Self::CommandLine, COL_CMDLINE_W, 140.0, 1200.0),
        ]);
        cols
    }

    pub(super) fn selectable_order(capabilities: CapabilityFlags) -> Vec<Self> {
        let mut order = vec![
            Self::Pid,
            Self::Status,
            Self::UserName,
            Self::SessionId,
            Self::Cpu,
            Self::CpuTime,
            Self::Memory,
            Self::WorkingSet,
            Self::WorkingSetDelta,
            Self::CommitSize,
            Self::Priority,
            Self::Handles,
            Self::Threads,
            Self::Platform,
        ];
        if capabilities.contains(CapabilityFlags::UAC_VIRTUALIZATION) {
            order.push(Self::UacVirtualization);
        }
        order.extend([
            Self::Elevated,
            Self::GpuMemory,
            Self::Publisher,
            Self::Description,
            Self::ExePath,
            Self::CommandLine,
        ]);
        order
    }

    pub(super) fn label(self, platform: PlatformProfile) -> String {
        match self {
            Self::Name => i18n::tr("details.col_name"),
            Self::Pid => i18n::tr("details.col_pid"),
            Self::Status => i18n::tr("details.col_status"),
            Self::UserName => i18n::tr("details.col_user"),
            Self::SessionId => i18n::tr("details.col_session_id"),
            Self::Cpu => i18n::tr("details.col_cpu"),
            Self::CpuTime => i18n::tr("details.col_cpu_time"),
            Self::Memory => i18n::tr("details.col_memory"),
            Self::WorkingSet => i18n::tr("details.col_working_set"),
            Self::WorkingSetDelta => i18n::tr("details.col_working_set_delta"),
            Self::CommitSize => i18n::tr("details.col_commit"),
            Self::Priority => i18n::tr("details.col_priority"),
            Self::Handles => i18n::tr("details.col_handles"),
            Self::Threads => i18n::tr("details.col_threads"),
            Self::Platform => i18n::tr("details.col_platform"),
            Self::UacVirtualization => i18n::virtualization_column_label(platform),
            Self::Elevated => i18n::tr("details.col_elevated"),
            Self::GpuMemory => i18n::tr("details.col_gpu_memory"),
            Self::Publisher => i18n::tr("details.col_publisher"),
            Self::Description => i18n::tr("details.col_description"),
            Self::ExePath => i18n::tr("details.col_exe_path"),
            Self::CommandLine => i18n::tr("details.col_command_line"),
        }
    }

    pub(super) const fn sort_column(self) -> Option<DetailsSortColumn> {
        match self {
            Self::Name => Some(DetailsSortColumn::Name),
            Self::Pid => Some(DetailsSortColumn::Pid),
            Self::Status => Some(DetailsSortColumn::Status),
            Self::UserName => Some(DetailsSortColumn::UserName),
            Self::SessionId => Some(DetailsSortColumn::SessionId),
            Self::Cpu => Some(DetailsSortColumn::Cpu),
            Self::CpuTime => Some(DetailsSortColumn::CpuTime),
            Self::Memory => Some(DetailsSortColumn::Memory),
            Self::WorkingSet => Some(DetailsSortColumn::WorkingSet),
            Self::WorkingSetDelta => Some(DetailsSortColumn::WorkingSetDelta),
            Self::CommitSize => Some(DetailsSortColumn::CommitSize),
            Self::Priority => Some(DetailsSortColumn::Priority),
            Self::Handles => Some(DetailsSortColumn::Handles),
            Self::Threads => Some(DetailsSortColumn::Threads),
            Self::Platform => Some(DetailsSortColumn::Arch),
            Self::UacVirtualization => Some(DetailsSortColumn::UacVirtualization),
            Self::Elevated => Some(DetailsSortColumn::Elevated),
            Self::GpuMemory => Some(DetailsSortColumn::GpuMemory),
            Self::Publisher => Some(DetailsSortColumn::Publisher),
            Self::Description => Some(DetailsSortColumn::Description),
            Self::ExePath => Some(DetailsSortColumn::ExePath),
            Self::CommandLine => Some(DetailsSortColumn::CommandLine),
        }
    }

    pub(super) const fn right_aligned(self) -> bool {
        matches!(
            self,
            Self::Pid
                | Self::SessionId
                | Self::Cpu
                | Self::CpuTime
                | Self::Memory
                | Self::WorkingSet
                | Self::WorkingSetDelta
                | Self::CommitSize
                | Self::Handles
                | Self::Threads
                | Self::GpuMemory
        )
    }

    pub(super) const fn is_heatmap(self) -> bool {
        matches!(self, Self::Cpu | Self::Memory)
    }
}

/// Total minimum default width of the Details table, adapting to advertised [`CapabilityFlags`].
#[cfg(test)]
pub(super) fn table_min_width(capabilities: CapabilityFlags) -> f32 {
    TableColumnLayout::new(DetailsColumn::default_specs(capabilities))
        .visible_columns()
        .iter()
        .map(|c| c.width)
        .sum::<f32>()
        + TABLE_HORIZONTAL_PADDING
}

/// Cached localized strings for [`super::delegate::DetailsTableDelegate`] to avoid
/// per-frame `i18n::tr` allocations.
pub(super) struct DetailsLocaleCache {
    pub(super) col_labels: [SharedString; 22],
    pub(super) sort_paused_tooltip: SharedString,
    pub(super) leak_warning_prefix: SharedString,
    pub(super) running: SharedString,
    pub(super) suspended: SharedString,
    pub(super) elevated_yes: SharedString,
    pub(super) elevated_no: SharedString,
    pub(super) loading: SharedString,
    pub(super) empty_search: SharedString,
    priority_idle: SharedString,
    priority_below_normal: SharedString,
    priority_normal: SharedString,
    priority_above_normal: SharedString,
    priority_high: SharedString,
    priority_realtime: SharedString,
    arch_x64: SharedString,
    arch_x86: SharedString,
    arch_arm64: SharedString,
    arch_unknown: SharedString,
    uac_enabled: SharedString,
    uac_disabled: SharedString,
    uac_not_allowed: SharedString,
    uac_unknown: SharedString,
    wv2: WebView2RoleLabels,
}

impl DetailsLocaleCache {
    pub(super) fn build(platform: PlatformProfile) -> Self {
        Self {
            col_labels: [
                DetailsColumn::Name.label(platform).into(),
                DetailsColumn::Pid.label(platform).into(),
                DetailsColumn::Status.label(platform).into(),
                DetailsColumn::UserName.label(platform).into(),
                DetailsColumn::SessionId.label(platform).into(),
                DetailsColumn::Cpu.label(platform).into(),
                DetailsColumn::CpuTime.label(platform).into(),
                DetailsColumn::Memory.label(platform).into(),
                DetailsColumn::WorkingSet.label(platform).into(),
                DetailsColumn::WorkingSetDelta.label(platform).into(),
                DetailsColumn::CommitSize.label(platform).into(),
                DetailsColumn::Priority.label(platform).into(),
                DetailsColumn::Handles.label(platform).into(),
                DetailsColumn::Threads.label(platform).into(),
                DetailsColumn::Platform.label(platform).into(),
                DetailsColumn::UacVirtualization.label(platform).into(),
                DetailsColumn::Elevated.label(platform).into(),
                DetailsColumn::GpuMemory.label(platform).into(),
                DetailsColumn::Publisher.label(platform).into(),
                DetailsColumn::Description.label(platform).into(),
                DetailsColumn::ExePath.label(platform).into(),
                DetailsColumn::CommandLine.label(platform).into(),
            ],
            sort_paused_tooltip: i18n::tr("proc_ux_sort_paused_tooltip").into(),
            leak_warning_prefix: i18n::tr("leak_warning_badge_prefix").into(),
            running: i18n::tr("details.status_running").into(),
            suspended: i18n::tr("details.status_suspended").into(),
            elevated_yes: i18n::tr("details.elevated_yes").into(),
            elevated_no: i18n::tr("details.elevated_no").into(),
            loading: i18n::tr("details.loading").into(),
            empty_search: i18n::tr("details.empty_search").into(),
            priority_idle: i18n::process_priority_label(ProcessPriority::Idle).into(),
            priority_below_normal: i18n::process_priority_label(ProcessPriority::BelowNormal)
                .into(),
            priority_normal: i18n::process_priority_label(ProcessPriority::Normal).into(),
            priority_above_normal: i18n::process_priority_label(ProcessPriority::AboveNormal)
                .into(),
            priority_high: i18n::process_priority_label(ProcessPriority::High).into(),
            priority_realtime: i18n::process_priority_label(ProcessPriority::Realtime).into(),
            arch_x64: i18n::process_arch_label(ProcessArch::X64).into(),
            arch_x86: i18n::process_arch_label(ProcessArch::X86).into(),
            arch_arm64: i18n::process_arch_label(ProcessArch::Arm64).into(),
            arch_unknown: i18n::process_arch_label(ProcessArch::Unknown).into(),
            uac_enabled: i18n::uac_virtualization_label(UacVirtualization::Enabled).into(),
            uac_disabled: i18n::uac_virtualization_label(UacVirtualization::Disabled).into(),
            uac_not_allowed: i18n::uac_virtualization_label(UacVirtualization::NotAllowed).into(),
            uac_unknown: i18n::uac_virtualization_label(UacVirtualization::Unknown).into(),
            wv2: WebView2RoleLabels::build(),
        }
    }

    #[inline]
    pub(super) fn column_label(&self, col: DetailsColumn) -> SharedString {
        self.col_labels[col as usize].clone()
    }

    #[inline]
    pub(super) fn webview2_label(&self, role: WebView2Role) -> SharedString {
        self.wv2.get(role)
    }

    #[inline]
    pub(super) fn priority_label(&self, p: ProcessPriority) -> SharedString {
        match p {
            ProcessPriority::Idle => self.priority_idle.clone(),
            ProcessPriority::BelowNormal => self.priority_below_normal.clone(),
            ProcessPriority::Normal => self.priority_normal.clone(),
            ProcessPriority::AboveNormal => self.priority_above_normal.clone(),
            ProcessPriority::High => self.priority_high.clone(),
            ProcessPriority::Realtime => self.priority_realtime.clone(),
        }
    }

    #[inline]
    pub(super) fn arch_label(&self, arch: ProcessArch) -> SharedString {
        match arch {
            ProcessArch::X64 => self.arch_x64.clone(),
            ProcessArch::X86 => self.arch_x86.clone(),
            ProcessArch::Arm64 => self.arch_arm64.clone(),
            ProcessArch::Unknown => self.arch_unknown.clone(),
        }
    }

    #[inline]
    pub(super) fn uac_label(&self, uac: UacVirtualization) -> SharedString {
        match uac {
            UacVirtualization::Enabled => self.uac_enabled.clone(),
            UacVirtualization::Disabled => self.uac_disabled.clone(),
            UacVirtualization::NotAllowed => self.uac_not_allowed.clone(),
            UacVirtualization::Unknown => self.uac_unknown.clone(),
        }
    }
}

/// Computes a fast 64-bit viewport fingerprint over the currently visible rows and columns
/// of a [`DetailsTableSlice`] so unchanged live ticks can skip `cx.notify()`.
#[allow(clippy::too_many_arguments)]
pub(super) fn compute_details_viewport_fingerprint(
    details: &DetailsTableSlice,
    column_layout: &TableColumnLayout<DetailsColumn>,
    visible_rows: &Range<usize>,
    visible_cols: &Range<usize>,
    selected_idx: Option<usize>,
    highlighted_key: Option<ProcessKey>,
    total_ram_bytes: u64,
) -> u64 {
    let mut h = FnvHasher64::new();
    let rows_len = details.rows.len();
    let start = visible_rows.start.min(rows_len);
    let end = visible_rows.end.min(rows_len).max(start);
    let cols_mask = column_layout.visible_cols_mask(visible_cols);

    h.write_bool(details.revision == 0);
    h.write_usize(rows_len);
    h.write_u8(details.view_mode as u8);
    h.write_u8(details.sort_column as u8);
    h.write_u8(details.sort_direction as u8);
    h.write_usize(selected_idx.unwrap_or(usize::MAX));
    if let Some(hk) = highlighted_key {
        h.write_u8(1);
        h.write_u32(hk.pid);
        h.write_u64(hk.create_time_100ns);
    } else {
        h.write_u8(0);
    }
    h.write_u64(total_ram_bytes);
    h.write_usize(start);
    h.write_usize(end);
    h.write_u64(cols_mask);

    let visible_specs = column_layout.visible_columns();
    for row in &details.rows[start..end] {
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
                DetailsColumn::Name => {
                    h.write_str(&row.name);
                    h.write_u8(row.webview2_role.map_or(u8::MAX, |r| r as u8));
                    if let Some(icon) = row.icon.as_ref() {
                        h.write_usize(icon.rgba.as_ptr() as usize);
                        h.write_usize(icon.rgba.len());
                    } else {
                        h.write_u8(0);
                    }
                }
                DetailsColumn::Pid => {}
                DetailsColumn::Status => {
                    h.write_bool(row.status.contains(ProcessStatusFlags::SUSPENDED));
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
                DetailsColumn::UserName => {
                    if let Some(u) = row.user_name.as_deref() {
                        h.write_u8(1);
                        h.write_str(u);
                    } else {
                        h.write_u8(0);
                    }
                }
                DetailsColumn::SessionId => {
                    h.write_u32(row.session_id);
                }
                DetailsColumn::Cpu => {
                    h.write_u16(row.cpu_permille);
                }
                DetailsColumn::CpuTime => {
                    h.write_u64(quantize_cpu_time_secs(row.cpu_time_100ns));
                }
                DetailsColumn::Memory => {
                    h.write_u64(quantize_bytes_display(row.private_working_set_bytes));
                    h.write_u8(
                        memory_heatmap_level(row.private_working_set_bytes, total_ram_bytes) as u8,
                    );
                }
                DetailsColumn::WorkingSet => {
                    h.write_u64(quantize_bytes_display(row.working_set_bytes));
                }
                DetailsColumn::WorkingSetDelta => {
                    h.write_u64(quantize_signed_bytes_display(row.working_set_delta_bytes));
                }
                DetailsColumn::CommitSize => {
                    h.write_u64(quantize_bytes_display(row.commit_bytes));
                }
                DetailsColumn::Priority => {
                    h.write_u8(row.priority as u8);
                }
                DetailsColumn::Handles => {
                    h.write_u32(row.handle_count);
                }
                DetailsColumn::Threads => {
                    h.write_u32(row.thread_count);
                }
                DetailsColumn::Platform => {
                    h.write_u8(row.arch as u8);
                }
                DetailsColumn::UacVirtualization => {
                    h.write_u8(row.uac_virtualization as u8);
                }
                DetailsColumn::Elevated => {
                    h.write_bool(row.status.contains(ProcessStatusFlags::ELEVATED));
                }
                DetailsColumn::GpuMemory => {
                    h.write_u64(quantize_bytes_display(row.gpu_dedicated_bytes));
                }
                DetailsColumn::Publisher => {
                    if let Some(p) = row.publisher.as_deref() {
                        h.write_u8(1);
                        h.write_str(p);
                    } else {
                        h.write_u8(0);
                    }
                }
                DetailsColumn::Description => {
                    if let Some(d) = row.description.as_deref() {
                        h.write_u8(1);
                        h.write_str(d);
                    } else {
                        h.write_u8(0);
                    }
                }
                DetailsColumn::ExePath => {
                    if let Some(p) = row.exe_path.as_deref() {
                        h.write_u8(1);
                        h.write_preview_str(p);
                    } else {
                        h.write_u8(0);
                    }
                }
                DetailsColumn::CommandLine => {
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

    h.finish()
}
