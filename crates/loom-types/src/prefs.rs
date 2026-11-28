//! User preference, view mode, sorting, and display enums shared across config, engine, and UI.

use serde::{Deserialize, Serialize};

/// Presentation mode of the Processes page.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProcessViewMode {
    /// Three-section categorized grouping (Apps / Background / Windows).
    #[default]
    Grouped,
    /// Verified parent-child process tree view.
    Tree,
}

/// Presentation mode of the Details page.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DetailsViewMode {
    /// Flat sortable process table.
    #[default]
    Flat,
    /// Verified parent-child process tree with full detail columns.
    Tree,
}

/// Sortable columns in the Processes table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProcessSortColumn {
    /// Process / application display name.
    Name,
    /// Process grouping category (`App` / `Background` / `System`).
    Category,
    /// Numeric process identifier.
    Pid,
    /// Raw executable/process image name.
    ProcessName,
    /// Process status flags.
    Status,
    /// CPU utilization.
    #[default]
    Cpu,
    /// Memory footprint.
    Memory,
    /// Disk I/O throughput.
    Disk,
    /// Network throughput.
    Network,
    /// GPU utilization / memory.
    Gpu,
    /// Binary publisher / company name.
    Publisher,
    /// Full process command line string.
    CommandLine,
}

/// Sortable columns in the Details table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DetailsSortColumn {
    /// Process executable name.
    #[default]
    Name,
    /// Numeric process identifier.
    Pid,
    /// Dynamic process status.
    Status,
    /// Owning user account name.
    UserName,
    /// OS login session identifier.
    SessionId,
    /// CPU utilization.
    Cpu,
    /// Cumulative CPU time (kernel + user).
    CpuTime,
    /// Private working set memory.
    Memory,
    /// Total resident working set memory.
    WorkingSet,
    /// Signed change in total resident working set memory since the previous tick.
    WorkingSetDelta,
    /// Private committed virtual memory.
    CommitSize,
    /// Scheduling priority class.
    Priority,
    /// Open kernel handle / file descriptor count.
    Handles,
    /// Active OS thread count.
    Threads,
    /// Process CPU architecture.
    Arch,
    /// UAC virtualization state.
    UacVirtualization,
    /// Process token elevation status.
    Elevated,
    /// Dedicated GPU memory.
    GpuMemory,
    /// Binary publisher / company name.
    Publisher,
    /// Binary file description.
    Description,
    /// Full executable image path.
    ExePath,
    /// Full command line string.
    CommandLine,
}

/// Sortable columns in the Services table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ServicesSortColumn {
    /// Internal service name.
    #[default]
    Name,
    /// Hosting process identifier.
    Pid,
    /// Service description or display name.
    Description,
    /// Service lifecycle status.
    Status,
    /// Service load-order or host group.
    Group,
}

/// Sortable columns in the Startup Applications table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StartupSortColumn {
    /// Display name of the startup application.
    #[default]
    Name,
    /// Publisher or vendor name.
    Publisher,
    /// Enabled or disabled status.
    Status,
    /// Estimated boot latency impact.
    Impact,
}

/// Sortable columns in the Users table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UsersSortColumn {
    /// User account or process display name.
    User,
    /// Session connection state or process running status.
    Status,
    /// Aggregated or per-process CPU utilization.
    #[default]
    Cpu,
    /// Aggregated or per-process memory usage.
    Memory,
    /// Aggregated or per-process disk I/O throughput.
    Disk,
    /// Aggregated or per-process network throughput.
    Network,
}

/// Sortable columns in the App History table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AppHistorySortColumn {
    /// Application display name.
    Name,
    /// Cumulative CPU time.
    #[default]
    CpuTime,
    /// Cumulative total network traffic.
    Network,
    /// Cumulative metered network traffic.
    MeteredNetwork,
}

/// Sort ordering direction for tables.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SortDirection {
    /// Smallest / A-to-Z first.
    Ascending,
    /// Largest / Z-to-A first.
    #[default]
    Descending,
}

impl SortDirection {
    /// Returns the opposite sort direction.
    #[inline]
    pub const fn toggle(self) -> Self {
        match self {
            Self::Ascending => Self::Descending,
            Self::Descending => Self::Ascending,
        }
    }
}

/// Formatting mode for resource columns (memory, disk, network) in the Processes table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResourceDisplayMode {
    /// Display absolute units (e.g., `128.4 MB`, `12.0 MB/s`).
    #[default]
    Values,
    /// Display percentages of total system capacity (e.g., `4.2%`).
    Percent,
}

/// Real-time sampling cadence for the fast-lane engine thread.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RefreshSpeed {
    /// High frequency: `500 ms` per tick.
    High500Ms,
    /// Normal default frequency: `1000 ms` per tick.
    #[default]
    Normal1S,
    /// Low frequency: `2000 ms` per tick.
    Low2S,
    /// Fast sampling timer paused until resumed by user.
    Paused,
}

impl RefreshSpeed {
    /// Returns the tick interval in milliseconds, or `None` when [`RefreshSpeed::Paused`].
    #[inline]
    pub const fn interval_ms(self) -> Option<u64> {
        match self {
            Self::High500Ms => Some(500),
            Self::Normal1S => Some(1_000),
            Self::Low2S => Some(2_000),
            Self::Paused => None,
        }
    }
}

/// Application visual theme preference.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ThemePreference {
    /// Follow the operating-system light/dark theme automatically.
    #[default]
    System,
    /// Force light theme.
    Light,
    /// Force dark theme.
    Dark,
}

/// Application UI localization preference.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LanguagePreference {
    /// Simplified Chinese (`zh-CN`).
    #[default]
    ZhCn,
    /// English (`en-US`).
    EnUs,
}

impl LanguagePreference {
    /// Returns the BCP-47 locale tag string (`"zh-CN"` or `"en-US"`).
    #[inline]
    pub const fn locale_tag(self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::EnUs => "en-US",
        }
    }
}

/// Default navigation page opened when Loom launches.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StartupPage {
    /// Processes view.
    #[default]
    Processes,
    /// Performance monitoring view.
    Performance,
    /// Application usage history view.
    AppHistory,
    /// Startup applications view.
    Startup,
    /// Logged-in users and sessions view.
    Users,
    /// Full process details view.
    Details,
    /// System services view.
    Services,
}
