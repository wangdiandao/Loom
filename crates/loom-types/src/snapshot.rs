//! Pre-computed view projections, on-demand monitor domains, and domain-split snapshots.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{
    AppHistorySortColumn, CapabilityFlags, DetailsSortColumn, DetailsViewMode, ExeIconRgba,
    LeakWarning, PerformanceSlice, PlatformProfile, ProcessArch, ProcessCategory, ProcessKey,
    ProcessPriority, ProcessSortColumn, ProcessStatusFlags, ProcessViewMode, ServicesSortColumn,
    SortDirection, StartupSortColumn, Timestamp100Ns, UacVirtualization, UsersSortColumn,
    WebView2Role,
};

/// On-demand monitoring domains gated by RAII `MonitorLease` reference counts.
///
/// Domains without an active lease are skipped by the engine so hidden tabs
/// and minimized windows incur zero unnecessary polling or projection overhead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MonitorDomain {
    /// Full-field Details table projection.
    Details,
    /// Hardware Performance metrics and charts.
    Performance,
    /// System services (SCM / systemd / launchd).
    Services,
    /// Startup applications.
    Startup,
    /// Logged-in user sessions.
    Users,
    /// Application resource usage history.
    AppHistory,
    /// Fine-grained per-logical-CPU utilization and histories.
    PerCoreCpu,
    /// Optional elevated-only per-process network byte attribution (e.g., ETW).
    ProcessNetwork,
}

impl MonitorDomain {
    /// Total number of [`MonitorDomain`] variants.
    pub const COUNT: usize = 8;

    /// Returns a dense array index in `0..Self::COUNT` for lock-free lease tables.
    #[inline]
    pub const fn index(self) -> usize {
        match self {
            Self::Details => 0,
            Self::Performance => 1,
            Self::Services => 2,
            Self::Startup => 3,
            Self::Users => 4,
            Self::AppHistory => 5,
            Self::PerCoreCpu => 6,
            Self::ProcessNetwork => 7,
        }
    }
}

/// Monotonically increasing per-domain revision counters embedded in [`AppSnapshot`].
///
/// UI slices compare their last-observed revision against these counters to
/// re-render only when their subscribed domain actually changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DomainRevisions {
    /// Revision of [`AppSnapshot::processes`].
    pub processes: u64,
    /// Revision of [`AppSnapshot::details`].
    pub details: u64,
    /// Revision of [`AppSnapshot::performance`].
    pub performance: u64,
    /// Revision of [`AppSnapshot::services`].
    pub services: u64,
    /// Revision of [`AppSnapshot::startup`].
    pub startup: u64,
    /// Revision of [`AppSnapshot::users`].
    pub users: u64,
    /// Revision of [`AppSnapshot::app_history`].
    pub app_history: u64,
}

/// Discriminator for flattened rows in the Processes table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProcessRowKind {
    /// Section separator header in grouped mode (Apps / Background / System).
    SectionHeader {
        /// Category represented by this section.
        category: ProcessCategory,
        /// Number of processes currently under this section.
        count: usize,
    },
    /// An individual process (or expandable process group/tree node).
    Process,
}

/// Pre-computed row projection for O(1) virtualized rendering in the Processes table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRowProjection {
    /// Row kind (section header vs. process row).
    pub kind: ProcessRowKind,
    /// Composite process key (zeroed for section headers).
    pub key: ProcessKey,
    /// Indentation depth (`0` for top-level rows, `1+` for expanded children).
    pub depth: u16,
    /// Whether this row can be expanded to reveal child processes.
    pub has_children: bool,
    /// Whether this row's children are currently expanded.
    pub is_expanded: bool,
    /// Number of direct or grouped child processes under this row.
    pub child_count: u32,
    /// Raw executable/process image name.
    pub name: Arc<str>,
    /// Friendly display name (file description, window title, or fallback image name).
    pub display_name: Arc<str>,
    /// Optional primary window title.
    pub window_title: Option<Arc<str>>,
    /// Optional binary publisher name.
    pub publisher: Option<Arc<str>>,
    /// Full executable image path, if resolved on the slow lane.
    pub exe_path: Option<Arc<str>>,
    /// Full process command line, if resolved on the slow lane.
    pub command_line: Option<Arc<str>>,
    /// Optional DPI-matched RGBA icon.
    pub icon: Option<ExeIconRgba>,
    /// Grouping category of this process.
    pub category: ProcessCategory,
    /// Dynamic process status flags.
    pub status: ProcessStatusFlags,
    /// CPU utilization in permille (`0..=1000`).
    pub cpu_permille: u16,
    /// Memory footprint (private working set) in bytes.
    pub memory_bytes: u64,
    /// Memory utilization in permille of total RAM (`0..=1000`).
    pub memory_permille: u16,
    /// Combined disk read+write throughput in bytes per second.
    pub disk_bytes_per_sec: u64,
    /// Disk throughput share in permille (`0..=1000`) for percentage display mode.
    pub disk_permille: u16,
    /// Combined network recv+send throughput in bytes per second.
    pub net_bytes_per_sec: u64,
    /// Network throughput share in permille (`0..=1000`) for percentage display mode.
    pub net_permille: u16,
    /// GPU utilization in permille (`0..=1000`).
    pub gpu_util_permille: u16,
    /// Dedicated GPU memory used in bytes.
    pub gpu_dedicated_bytes: u64,
    /// Verified parent process key, if the parent is alive and older.
    pub parent_key: Option<ProcessKey>,
    /// Optional advisory resource-leak warning attached to this process.
    pub leak_warning: Option<LeakWarning>,
    /// Optional identified WebView2 subprocess role when this process is `msedgewebview2.exe`.
    pub webview2_role: Option<WebView2Role>,
}

/// Pre-computed row projection for O(1) virtualized rendering in the Details table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailsRowProjection {
    /// Composite process lifecycle key.
    pub key: ProcessKey,
    /// Verified parent process key, if the parent is alive and older.
    pub parent_key: Option<ProcessKey>,
    /// Indentation depth in tree mode (`0` in flat mode).
    pub depth: u16,
    /// Whether this node has child processes in tree mode.
    pub has_children: bool,
    /// Whether children are currently expanded in tree mode.
    pub is_expanded: bool,
    /// Executable image name.
    pub name: Arc<str>,
    /// Optional binary file description.
    pub description: Option<Arc<str>>,
    /// Optional binary publisher / company name.
    pub publisher: Option<Arc<str>>,
    /// Optional downsampled RGBA icon.
    pub icon: Option<ExeIconRgba>,
    /// Dynamic process status flags.
    pub status: ProcessStatusFlags,
    /// Owning user account name, if resolved.
    pub user_name: Option<Arc<str>>,
    /// OS login session identifier.
    pub session_id: u32,
    /// CPU utilization in permille (`0..=1000`).
    pub cpu_permille: u16,
    /// Cumulative CPU time (kernel + user) in 100-nanosecond ticks.
    pub cpu_time_100ns: u64,
    /// Private working set memory in bytes.
    pub private_working_set_bytes: u64,
    /// Total resident working set memory in bytes.
    pub working_set_bytes: u64,
    /// Private committed virtual memory in bytes.
    pub commit_bytes: u64,
    /// Active thread count.
    pub thread_count: u32,
    /// Open handle / file descriptor count.
    pub handle_count: u32,
    /// Scheduling priority class.
    pub priority: ProcessPriority,
    /// Process CPU architecture.
    pub arch: ProcessArch,
    /// UAC virtualization state.
    pub uac_virtualization: UacVirtualization,
    /// Dedicated GPU video memory in bytes.
    pub gpu_dedicated_bytes: u64,
    /// Shared GPU memory in bytes.
    pub gpu_shared_bytes: u64,
    /// Full executable image path, if resolved.
    pub exe_path: Option<Arc<str>>,
    /// Full command line, if resolved.
    pub command_line: Option<Arc<str>>,
    /// Optional advisory resource-leak warning attached to this process.
    pub leak_warning: Option<LeakWarning>,
    /// Optional identified WebView2 subprocess role when this process is `msedgewebview2.exe`.
    pub webview2_role: Option<WebView2Role>,
}

/// Published snapshot slice for the Processes view.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProcessTableSlice {
    /// Monotonic revision counter for this slice.
    pub revision: u64,
    /// Timestamp of the underlying kernel sample.
    pub timestamp: Timestamp100Ns,
    /// Active view mode used to construct `rows`.
    pub view_mode: ProcessViewMode,
    /// Active sort column used to construct `rows`.
    pub sort_column: ProcessSortColumn,
    /// Active sort direction used to construct `rows`.
    pub sort_direction: SortDirection,
    /// Total number of live processes (excluding section header rows).
    pub total_process_count: usize,
    /// Aggregate system CPU utilization in permille (`0..=1000`).
    pub total_cpu_permille: u16,
    /// Aggregate system memory utilization in permille (`0..=1000`).
    pub total_memory_permille: u16,
    /// Aggregate system disk throughput in bytes per second.
    pub total_disk_bytes_per_sec: u64,
    /// Aggregate system network throughput in bytes per second.
    pub total_net_bytes_per_sec: u64,
    /// Aggregate system GPU utilization in permille (`0..=1000`).
    pub total_gpu_permille: u16,
    /// Pre-sorted, pre-filtered flattened rows ready for virtual table rendering.
    pub rows: Vec<ProcessRowProjection>,
}

/// Published snapshot slice for the Details view.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DetailsTableSlice {
    /// Monotonic revision counter for this slice.
    pub revision: u64,
    /// Timestamp of the underlying kernel sample.
    pub timestamp: Timestamp100Ns,
    /// Active details view mode (`Flat` or `Tree`).
    pub view_mode: DetailsViewMode,
    /// Active sort column.
    pub sort_column: DetailsSortColumn,
    /// Active sort direction.
    pub sort_direction: SortDirection,
    /// Pre-sorted, pre-filtered rows ready for virtual table rendering.
    pub rows: Vec<DetailsRowProjection>,
}

/// Lifecycle state of an OS system service.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ServiceState {
    /// Service is stopped.
    Stopped,
    /// Service is starting.
    StartPending,
    /// Service is stopping.
    StopPending,
    /// Service is running.
    Running,
    /// Service is resuming from paused state.
    ContinuePending,
    /// Service is pausing.
    PausePending,
    /// Service is paused.
    Paused,
    /// Service state could not be mapped.
    #[default]
    Unknown,
}

/// Metadata and current status of an OS system service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRecord {
    /// Internal service name (unique identifier).
    pub name: Arc<str>,
    /// Human-readable service display name.
    pub display_name: Arc<str>,
    /// Optional service description.
    pub description: Option<Arc<str>>,
    /// Hosting process ID if the service is currently running.
    pub pid: Option<u32>,
    /// Current lifecycle state.
    pub state: ServiceState,
    /// Optional service load-order or host group name.
    pub group: Option<Arc<str>>,
}

/// Published snapshot slice for the Services view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicesSlice {
    /// Monotonic revision counter for this slice.
    pub revision: u64,
    /// Timestamp of the last service enumeration.
    pub timestamp: Timestamp100Ns,
    /// Active sort column used to build [`Self::visible_indices`].
    pub sort_column: ServicesSortColumn,
    /// Active sort direction used to build [`Self::visible_indices`].
    pub sort_direction: SortDirection,
    /// Optional hosting process ID filter currently applied to [`Self::visible_indices`].
    pub filter_pid: Option<u32>,
    /// Enumerated system services.
    pub items: Vec<ServiceRecord>,
    /// Pre-filtered and pre-sorted indices into [`Self::items`] ready for O(1) virtualized rendering.
    pub visible_indices: Vec<usize>,
}

impl Default for ServicesSlice {
    fn default() -> Self {
        Self {
            revision: 0,
            timestamp: Timestamp100Ns::default(),
            sort_column: ServicesSortColumn::Name,
            sort_direction: SortDirection::Ascending,
            filter_pid: None,
            items: Vec::new(),
            visible_indices: Vec::new(),
        }
    }
}

/// Estimated boot impact rating of a startup application.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StartupImpact {
    /// High startup latency impact.
    High,
    /// Medium startup latency impact.
    Medium,
    /// Low startup latency impact.
    Low,
    /// Impact has not yet been measured by the OS.
    #[default]
    NotMeasured,
    /// Entry is disabled or has negligible impact.
    None,
}

/// A single registered startup application entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupEntry {
    /// Stable platform identifier for toggling this entry.
    pub id: Arc<str>,
    /// Display name of the startup item.
    pub name: Arc<str>,
    /// Optional publisher name.
    pub publisher: Option<Arc<str>>,
    /// Optional command or target path.
    pub command: Option<Arc<str>>,
    /// Resolved local executable or target file path, when available for shell actions.
    pub file_path: Option<Arc<str>>,
    /// Optional DPI-matched RGBA icon.
    pub icon: Option<ExeIconRgba>,
    /// Whether this startup entry is currently enabled.
    pub enabled: bool,
    /// Estimated startup impact.
    pub impact: StartupImpact,
}

/// Published snapshot slice for the Startup Applications view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupSlice {
    /// Monotonic revision counter for this slice.
    pub revision: u64,
    /// Timestamp of the last startup scan.
    pub timestamp: Timestamp100Ns,
    /// Last BIOS/firmware boot time in milliseconds, if reported by the OS.
    pub last_bios_time_ms: Option<u64>,
    /// Active sort column used to build [`Self::visible_indices`].
    pub sort_column: StartupSortColumn,
    /// Active sort direction used to build [`Self::visible_indices`].
    pub sort_direction: SortDirection,
    /// Enumerated startup entries.
    pub items: Vec<StartupEntry>,
    /// Pre-filtered and pre-sorted indices into [`Self::items`] ready for O(1) virtualized rendering.
    pub visible_indices: Vec<usize>,
}

impl Default for StartupSlice {
    fn default() -> Self {
        Self {
            revision: 0,
            timestamp: Timestamp100Ns::default(),
            last_bios_time_ms: None,
            sort_column: StartupSortColumn::Name,
            sort_direction: SortDirection::Ascending,
            items: Vec::new(),
            visible_indices: Vec::new(),
        }
    }
}

/// Connection state of a logged-in OS user session.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UserSessionState {
    /// Session is actively attached to a local or remote display.
    Active,
    /// Session is disconnected but retains running processes.
    Disconnected,
    /// Session is idle.
    Idle,
    /// Session state is unknown.
    #[default]
    Unknown,
}

/// Compact live process summary nested inside [`UserSessionRecord`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSessionProcess {
    /// Composite process lifecycle key.
    pub key: ProcessKey,
    /// Raw executable/process image name.
    pub name: Arc<str>,
    /// Friendly display name (file description, window title, or fallback image name).
    pub display_name: Arc<str>,
    /// Dynamic process status flags.
    pub status: ProcessStatusFlags,
    /// CPU utilization in permille (`0..=1000`).
    pub cpu_permille: u16,
    /// Private working set memory in bytes.
    pub memory_bytes: u64,
    /// Combined disk throughput in bytes per second.
    pub disk_bytes_per_sec: u64,
    /// Combined network throughput in bytes per second.
    pub net_bytes_per_sec: u64,
}

/// Aggregated resource metrics for a single OS user session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSessionRecord {
    /// OS session identifier.
    pub session_id: u32,
    /// Logged-in user account name.
    pub user_name: Arc<str>,
    /// Current session connection state.
    pub state: UserSessionState,
    /// Number of running processes belonging to this session.
    pub process_count: u32,
    /// Aggregated CPU utilization in permille (`0..=1000`).
    pub cpu_permille: u16,
    /// Aggregated private working set memory in bytes.
    pub memory_bytes: u64,
    /// Aggregated disk throughput in bytes per second.
    pub disk_bytes_per_sec: u64,
    /// Aggregated network throughput in bytes per second.
    pub net_bytes_per_sec: u64,
    /// Live processes running in this session, sorted by CPU and memory descending.
    pub processes: Vec<UserSessionProcess>,
}

/// Pre-computed flattened row projection for O(1) virtualized rendering in the Users table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsersRowProjection {
    /// Top-level expandable user session header row.
    Session {
        /// OS session identifier.
        session_id: u32,
        /// Logged-in user account name.
        user_name: Arc<str>,
        /// Current session connection state.
        state: UserSessionState,
        /// Number of running processes belonging to this session.
        process_count: u32,
        /// Aggregated CPU utilization in permille (`0..=1000`).
        cpu_permille: u16,
        /// Aggregated private working set memory in bytes.
        memory_bytes: u64,
        /// Aggregated disk throughput in bytes per second.
        disk_bytes_per_sec: u64,
        /// Aggregated network throughput in bytes per second.
        net_bytes_per_sec: u64,
        /// Whether this session has child processes that can be expanded.
        has_children: bool,
        /// Whether this session's child processes are currently expanded.
        is_expanded: bool,
    },
    /// Indented child process row nested under an expanded user session.
    Process {
        /// Owning OS session identifier.
        session_id: u32,
        /// Live process metrics and identity.
        process: UserSessionProcess,
    },
}

/// Published snapshot slice for the Users view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsersSlice {
    /// Monotonic revision counter for this slice.
    pub revision: u64,
    /// Timestamp of the last session aggregation.
    pub timestamp: Timestamp100Ns,
    /// Active sort column used to build [`Self::rows`].
    pub sort_column: UsersSortColumn,
    /// Active sort direction used to build [`Self::rows`].
    pub sort_direction: SortDirection,
    /// Active and disconnected user sessions.
    pub sessions: Vec<UserSessionRecord>,
    /// Pre-filtered, pre-sorted, and pre-flattened session and expanded child process rows.
    pub rows: Vec<UsersRowProjection>,
}

impl Default for UsersSlice {
    fn default() -> Self {
        Self {
            revision: 0,
            timestamp: Timestamp100Ns::default(),
            sort_column: UsersSortColumn::Cpu,
            sort_direction: SortDirection::Descending,
            sessions: Vec::new(),
            rows: Vec::new(),
        }
    }
}

/// Cumulative historical resource usage record for an application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppHistoryRecord {
    /// Platform application identifier (e.g., AUMID or package family name).
    pub app_id: Arc<str>,
    /// Human-readable application display name.
    pub name: Arc<str>,
    /// Cumulative CPU time in 100-nanosecond ticks.
    pub cpu_time_100ns: u64,
    /// Cumulative total network traffic in bytes.
    pub network_bytes: u64,
    /// Cumulative metered network traffic in bytes.
    pub metered_network_bytes: u64,
}

/// Published snapshot slice for the App History view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppHistorySlice {
    /// Monotonic revision counter for this slice.
    pub revision: u64,
    /// Timestamp of the last history refresh.
    pub timestamp: Timestamp100Ns,
    /// Unix epoch timestamp (in seconds) since which history has been accumulated.
    pub since_epoch_secs: Option<u64>,
    /// Active sort column used to build [`Self::visible_indices`].
    pub sort_column: AppHistorySortColumn,
    /// Active sort direction used to build [`Self::visible_indices`].
    pub sort_direction: SortDirection,
    /// Cumulative per-application history entries.
    pub items: Vec<AppHistoryRecord>,
    /// Pre-filtered and pre-sorted indices into [`Self::items`] ready for O(1) virtualized rendering.
    pub visible_indices: Vec<usize>,
}

impl Default for AppHistorySlice {
    fn default() -> Self {
        Self {
            revision: 0,
            timestamp: Timestamp100Ns::default(),
            since_epoch_secs: None,
            sort_column: AppHistorySortColumn::CpuTime,
            sort_direction: SortDirection::Descending,
            items: Vec::new(),
            visible_indices: Vec::new(),
        }
    }
}

/// Root lock-free immutable snapshot published via `ArcSwap<AppSnapshot>`.
///
/// Each domain slice is wrapped in an independent `Arc` so unchanged domains
/// are reused by pointer copy without cloning their backing vectors.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AppSnapshot {
    /// Per-domain monotonic revision numbers for incremental UI updates.
    pub revisions: DomainRevisions,
    /// Static platform identity profile advertised by the active backend.
    pub platform: PlatformProfile,
    /// Static capabilities advertised by the active platform backend.
    pub capabilities: CapabilityFlags,
    /// Whether the Loom process itself is currently running elevated.
    pub is_elevated: bool,
    /// Pre-projected Processes table slice.
    pub processes: Arc<ProcessTableSlice>,
    /// Pre-projected Details table slice.
    pub details: Arc<DetailsTableSlice>,
    /// Hardware Performance metrics and 60-point history slice.
    pub performance: Arc<PerformanceSlice>,
    /// System Services slice.
    pub services: Arc<ServicesSlice>,
    /// Startup Applications slice.
    pub startup: Arc<StartupSlice>,
    /// User Sessions slice.
    pub users: Arc<UsersSlice>,
    /// Application History slice.
    pub app_history: Arc<AppHistorySlice>,
}
