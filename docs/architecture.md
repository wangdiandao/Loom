# System architecture

English | [中文](architecture.zh.md)

Loom is a cross-platform (Windows / macOS / Linux) native desktop Task Manager built with **Rust** and **GPUI Kit (`gpui-kit` / `gpui-component`)**. Development targets **Windows** first—matching the core capabilities of the built-in Windows 11 Task Manager while addressing **slow cold starts, high-load UI stutters, and startup hangs caused by hung windows or stalled storage devices**—and defines a compile-time Platform Abstraction Layer (PAL) ready for Linux and macOS backends.

<a id="core-architectural-principles"></a>

## 1. Core architectural principles

1. **Headless & runtime-free core**: System sampling, delta computation, process-tree construction, search/sort indexing, and command dispatch live in UI-agnostic crates (`loom-engine` and below) without a multi-thread `tokio` runtime, avoiding dozens of idle worker threads on high-core-count CPUs (such as 32-thread processors).
2. **Compile-time static PAL**: Windows, macOS, and Linux backends live in separate crates bound via associated types, zero-sized extension types (ZSTs), and `#[cfg(target_os)]` type aliases in `loom-pal`, avoiding `dyn Trait` vtables and boxed futures on hot paths.
3. **Dual-lane bulkhead isolation**: Sub-millisecond in-memory kernel queries (Fast Lane) are isolated from potentially blocking disk, shell, and driver metadata probes (Slow Lane) guarded by per-device `300 ms` circuit breakers, reducing startup freezes and runtime hangs.
4. **Cold/hot data separation & buffer reuse**: Processes are keyed by `(pid, create_time_100ns)` to handle PID reuse. Hot metrics use `Copy` structs written into reusable scratch buffers; cold metadata (icons, PE publisher, command line) is resolved once and shared via two-tier `Arc` interning.
5. **Feature-sliced, component-composed UI**: `loom-app` is organized into self-contained `shell` + `features/*` modules built by composing `gpui-kit` controls (`Table`, `AreaChart`, `LineChart`, `Sidebar`, `TitleBar`, `Dialog`, `PopupMenu`, `Tree`, `Select`) and Flexbox primitives—avoiding root-level global state structs, monolithic modal enums, and custom `Canvas`/shader drawing.
6. **Domain-sliced snapshots & RAII `MonitorLease`**: Snapshots publish lock-free via `ArcSwap<AppSnapshot>` with monotonic per-domain `Revision` counters. On-demand domains (`Services`, `Startup Apps`, `Users`, `App History`, per-core grids) poll only while leased via RAII `MonitorLease`.
7. **Standard-user launch & on-demand elevation**: Loom launches under standard user privileges without a startup UAC prompt, elevating only when acting on protected system processes, services, or administrative tasks.

<a id="performance-budgets-and-anti-hang-architecture"></a>

## 2. Performance budgets and anti-hang architecture

### 2.1 Engineering resource budgets

Target budgets measured against modern multi-core desktops (16 cores / 32 logical processors, 400+ processes, 10,000+ threads):

| Dimension | Loom Target Budget | Stock Windows 11 Task Manager Baseline Issue |
| --- | --- | --- |
| **Cold-start time to interactive** | **`< 50 ms`** (immediate priority boost, `< 35 ms` first interactive process table) | `800 – 3000+ ms` due to XAML Islands, COM, and WMI/PDH startup stalls |
| **Fault resilience on launch** | **Slow-I/O bulkhead + `300 ms` circuit breakers** | Prone to stalls or blank windows on hung apps, spun-down HDDs, faulty USB drives, or unreachable SMB shares |
| **Steady-state CPU (1s refresh)** | **`< 0.15%`** (`< 1 ms` single kernel sweep + delta calculation) | Commonly `0.8% – 3%+`, spiking on core grids or fast scrolling |
| **Idle & minimized GPU load** | **Event-driven redraw at refresh cadence (e.g. `1 FPS` at 1s); `0% GPU` when minimized / closed to tray** | Higher XAML composition bridge overhead |
| **Private working set (RAM)** | **`30 – 45 MB`** (target `< 45 MB`; reused scratch arenas + `20×20` icon cache) | Typically `80 – 160+ MB` |
| **Process OS thread count** | **`6 – 8` threads maximum** | Commonly spawns `30 – 60+` COM, XAML, WMI, and thread-pool workers |

### 2.2 Progressive cold-start pipeline (`< 50 ms` target)

```mermaid
sequenceDiagram
    autonumber
    participant Main as "GPUI Main Thread (loom-app)"
    participant Fast as "Fast Kernel Sampler (HIGH / ABOVE_NORMAL)"
    participant Slow as "Slow Probe Bulkhead Pool (max 2 Threads, Low Priority)"

    Note over Main: T = 0 ~ 2 ms (Priority Boost & Config Load)
    Main->>Main: SetPriorityClass(HIGH_PRIORITY_CLASS) to preempt 100%-CPU workloads
    Main->>Main: Load settings.toml (under 4 KB, no Registry/COM wait)
    par Parallel Window Init & First Kernel Snapshot
        Main->>Main: gpui_kit::open_window initializes DX12 surface & shell (under 30 ms)
    and
        Main->>Fast: Spawn Fast Sampler & run first NtQuerySystemInformation (~1 ms)
        Fast-->>Main: Publish Frame-1 Process Slice (PID / image name / memory / status)
    end
    Note over Main: T ≈ 30 ~ 45 ms: Interactive Frame 1 (Search & End Task ready)
    Fast->>Fast: T = +250 ms calibration tick computes initial CPU% / I/O deltas, then enters 1s cadence
    Fast-)Slow: Enqueue cold metadata probes (20x20 icons, PE publisher, SMBIOS, disk models)
    Slow-->>Main: Backfill icons & descriptions asynchronously without blocking UI
```

### 2.3 Six Windows anti-hang constraints

1. **No WMI or PDH dependencies**: Do not call `IWbemServices` (cross-process RPC to `Winmgmt`) or `PdhOpenQuery` (loads third-party performance counter DLLs under global registry locks). Query CPU/processes via `ntdll`, NICs via `iphlpapi`, disks via guarded `IOCTL` calls, GPUs via `DXGI + gdi32!D3DKMT`, and RAM speed/slots via in-memory `GetSystemFirmwareTable('RSMB')` SMBIOS parsing (`< 0.05 ms`).
2. **No synchronous cross-process window messages**: Do not call `GetWindowTextW` or blocking `SendMessageW`, which can block indefinitely if another desktop app is hung. Use `InternalGetWindowText` and non-blocking `IsHungAppWindow`.
3. **Slow-disk & remote-share bulkhead circuit breakers**: Bypass deep file opens on UNC or `DRIVE_REMOTE` paths (falling back to memory-only `SHGFI_USEFILEATTRIBUTES` icons). Isolate storage `IOCTL` probes (`IOCTL_STORAGE_QUERY_PROPERTY`, `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`, and per-device `IOCTL_DISK_PERFORMANCE`), shell icon lookups, and PE version reads inside the Slow Probe Pool; any volume exceeding `300 ms` trips a 60-second `CircuitBreaker` cooldown so the Fast Lane remains non-blocking.
4. **No cross-process PEB reads via `ReadProcessMemory`**: Query command lines via `NtQueryInformationProcess(ProcessCommandLineInformation)` to avoid stalls on suspended or paged-out targets.
5. **No network revocation checks during publisher lookup**: Prefer local PE `VersionInfo` (`CompanyName` / `FileDescription`); when invoking `WinVerifyTrust`, pass `WTD_CACHE_ONLY_URL_RETRIEVAL` to avoid outbound CRL/OCSP waits.
6. **No persistent target process handles**: Extract the bulk of dynamic metrics directly from `SystemProcessInformation` buffers; open `PROCESS_QUERY_LIMITED_INFORMATION` handles transiently and close them immediately after querying.

<a id="system-topology-and-execution-domains"></a>

## 3. System topology and execution domains

Loom separates work into **three execution domains** inside a single user-mode process, crossing an elevation boundary only on explicit user action:

```mermaid
flowchart TD
    subgraph MainProc["loom-app Process (Standard User by Default, HIGH_PRIORITY_CLASS)"]
        subgraph UIDomain["Execution Domain 1: GPUI Main Thread (Event-Driven Steady / Interactive)"]
            Shell["AppShell (TitleBar Search / Collapsible Sidebar / Shell Dialogs / Toast)"]
            subgraph Features["Independent UI Feature Slices"]
                FProc["feature::processes"]
                FPerf["feature::performance"]
                FHist["feature::app_history"]
                FStart["feature::startup"]
                FUser["feature::users"]
                FDet["feature::details"]
                FSvc["feature::services"]
                FSet["feature::settings"]
            end
            UIKit["loom-ui-kit (gpui-kit Composites, No Custom Draw)"]
            Store["UiStore (Per-Domain Revision Slice Subscriptions)"]
            Tray["TrayController (Zero-GPU Residency When Window Closed)"]

            Shell --> Features
            Features --> UIKit
            Features --> Store
        end

        subgraph Bridge["Lock-Free In-Process Bridge"]
            CmdTx["CommandBus (Bounded EngineCommand Channel)"]
            SnapSwap["SnapshotStore (ArcSwap AppSnapshot + DomainDirtyFlags)"]
            LeaseReg["MonitorLease (RAII On-Demand Stream Refcount)"]
        end

        subgraph FastDomain["Execution Domain 2: Fast Kernel Sampler (1 Dedicated Thread, under 1 ms/tick)"]
            Coord["EngineCoordinator (Deltas / 60-pt Rings / ProcessTree / Pre-Sorted Projections)"]
            FastPAL["FastSystemSampler (ntdll / iphlpapi / D3DKMT / InternalGetWindowText)"]
            Coord --> FastPAL
        end

        subgraph SlowDomain["Execution Domain 3: Slow Probe Bulkhead Pool (1-2 Parked Threads + 300 ms Breaker)"]
            ProbePool["SlowMetadataProber + Disk IOCTL + CircuitBreaker"]
            OnDemand["On-Demand Managers (SCM Services / Startup / WTS Users / AppHistory / WCT / MiniDump)"]
        end

        Features -- "dispatch EngineCommand" --> CmdTx
        Tray -- "dispatch EngineCommand" --> CmdTx
        Features -- "hold MonitorLease" --> LeaseReg
        LeaseReg --> Coord
        CmdTx --> Coord
        ProbePool -. "lock-free backfill (ExeStaticAsset / Disk)" .-> Coord
        Coord -- "dispatch blocking ops" --> OnDemand
        Coord -- "publish pre-sorted slices" --> SnapSwap
        SnapSwap --> Store
    end

    subgraph ElevBoundary["On-Demand Elevation Boundary (Explicit User Trigger Only)"]
        RunAs["UAC ShellExecuteExW('runas') Task or Elevated Relaunch Restoring Active Route"]
    end

    OnDemand -. "on ElevationRequired + user confirm" .-> RunAs
```

<a id="workspace-module-layout-and-dependency-dag"></a>

## 4. Workspace module layout and dependency DAG

A 10-crate Cargo Workspace enforces a compile-time acyclic dependency graph:

```mermaid
flowchart BT
    types["loom-types"]
    cfg["loom-config"]
    pal_api["loom-pal-api"]
    pal_win["loom-pal-windows"]
    pal_mac["loom-pal-macos"]
    pal_lin["loom-pal-linux"]
    pal["loom-pal"]
    engine["loom-engine"]
    uikit["loom-ui-kit"]
    app["loom-app"]

    pal_api --> types
    pal_win --> pal_api
    pal_win --> types
    pal_mac --> pal_api
    pal_mac --> types
    pal_lin --> pal_api
    pal_lin --> types
    pal --> pal_api
    pal -. "cfg(windows)" .-> pal_win
    pal -. "cfg(target_os = macos)" .-> pal_mac
    pal -. "cfg(target_os = linux)" .-> pal_lin
    engine --> types
    engine --> cfg
    engine --> pal_api
    uikit --> types
    app --> types
    app --> cfg
    app --> pal
    app --> engine
    app --> uikit
```

### Crate responsibilities and boundaries

| Crate | Kind | Core Responsibility | Dependency & Decoupling Boundary |
| --- | --- | --- | --- |
| `loom-types` | lib | Cross-platform compact domain models (`ProcessKey`, `ProcessHotSample`, `ProcessColdMeta`, hardware snapshots, services, startup entries, sessions, history, `CapabilityFlags`). | No OS or UI dependencies; hot structs use `Copy` POD layouts. |
| `loom-config` | lib | Local configuration (`settings.toml`: startup page, refresh cadence, theme, locale, window bounds, always-on-top/tray behavior, column preferences), schema migrations, atomic writes. | Synchronous load targeting `< 1 ms`; no dependency on platform backends or UI. |
| `loom-pal-api` | lib | Static PAL traits (`PlatformBackend`, `FastSystemSampler`, `SlowMetadataProber`, etc.), caller-reused `SampleScratch` arenas, and `PalError`. | Depends solely on `loom-types`; does not expose raw OS handle or header types. |
| `loom-pal-windows` | lib | Windows sampling and system control backend (`ntdll`, `D3DKMT`, `IOCTL`, `SMBIOS`, `SCM`, `WTS`, `WCT`, `MiniDump`). | Compiled only under `cfg(windows)` using `windows-sys`; encapsulates `unsafe` FFI and enforces the six anti-hang constraints. |
| `loom-pal-macos` | lib | macOS backend skeleton (`Mach` / `libproc` / `IOKit` / `launchd` targets). | Compiled only under `cfg(target_os = "macos")`; contract stub scheduled for full implementation in `0.3.0`. |
| `loom-pal-linux` | lib | Linux backend skeleton (`procfs` / `sysfs` / `netlink` / `drm` / `systemd` targets). | Compiled only under `cfg(target_os = "linux")`; contract stub scheduled for full implementation in `0.2.0`. |
| `loom-pal` | lib | Compile-time platform facade (`pub type NativeBackend = ...`) plus single-instance lock, tray adapter, and power/display observers. | Keeps `#[cfg(target_os)]` conditionals out of upper crates without vtable indirection. |
| `loom-engine` | lib | Headless sampling and orchestration engine (`EngineCoordinator<B>`, delta engine, 60-point quantized rings, anti-reuse process tree, pre-sorted view projections, `MonitorLease`). | Prohibits GPUI or multi-thread Tokio; headless-testable against `MockBackend`. |
| `loom-ui-kit` | lib | Thin composition layer over `gpui-kit`: heatmap cells, sidebar sparkline cards, logical-core grids, segmented memory bars, semantic theme tokens, and `rust-i18n` helpers. | No custom `Canvas`/shader drawing; holds no domain state. |
| `loom-app` | bin | Desktop composition root & feature-sliced UI: priority boost, progressive boot pipeline, `AppShell`, and 8 isolated `features/*` modules. | Feature slices do not cross-import internals; cross-view jumps (`JumpToDetails`, `JumpToServices`) route through `ShellHandle`. |

<a id="feature-sliced-ui-architecture"></a>

## 5. Feature-sliced UI architecture (`loom-app` & `loom-ui-kit`)

```text
crates/loom-app/src/
├── main.rs                  # Entrypoint: HIGH_PRIORITY_CLASS boost, single-instance check
├── bootstrap.rs             # Parallel settings load + EngineCoordinator<NativeBackend> + GPUI window open
├── bridge/
│   ├── store.rs             # UiStore: per-domain Arc<*Slice> + Revision subscriptions
│   ├── commands.rs          # EngineCommand dispatch & ElevationRequired notification prompt
│   └── monitor_lease.rs     # RAII MonitorLease guard (auto-releases on unmount or minimize)
├── shell/
│   ├── window.rs            # Root-wrapped window (TitleBar search + collapsible Sidebar + outlet)
│   ├── nav.rs               # Declarative route registry & cross-view NavigationIntent bus
│   ├── dialogs.rs           # Cross-view shell dialogs only (Run new task / generic confirmations)
│   └── tray.rs              # Resident tray controller (destroys GPUI window on close to free VRAM)
└── features/                # 8 autonomous vertical slices with no cross-imports
    ├── processes/           # Categorized groups / parent-child tree toggle + heatmap table + menu
    ├── performance/         # Left sparkline device rail + CPU/Memory/Disk/Network/GPU views
    ├── app_history/         # Cumulative resource usage table + clear history action
    ├── startup/             # Startup entries + Last BIOS time + enable/disable controls
    ├── users/               # Expandable user-session process tree + disconnect actions
    ├── details/             # Full-column table/tree + scoped Affinity / Wait Chain dialogs + menu
    ├── services/            # SCM services table + start/stop/restart toolbar + PID jump
    └── settings/            # Startup page / update speed / always-on-top & tray / theme & locale
```

Feature-specific dialogs (such as `Set affinity` and `Analyze wait chain` inside `features/details/`) remain scoped to their owning slice and open through `gpui-kit`'s `Root` dialog layer, avoiding a monolithic global modal enum in `shell/`.

### 5.1 Sliced read-only projections

`AppSnapshot` partitions state into independent `Arc` slices pre-sorted, pre-filtered, and tree-flattened off the UI thread inside `loom-engine`:

- `processes: Arc<ProcessTableSlice>` (flattened `Vec<ProcessRowProjection>`; selection and right-click actions anchor to `ProcessKey` rather than row index so dynamic resorts do not target the wrong process; `ProjectionEngine` is parameterized by `PlatformBackend::PLATFORM` so Windows Task Manager-aligned shell un-nesting and multi-process app aggregation apply strictly to `PlatformKind::Windows` without altering Linux/macOS grouping or true OS `Process Tree` views)
- `details: Arc<DetailsTableSlice>` (projected only while `Details` holds an active `MonitorLease`)
- `performance: Arc<PerformanceSlice>` (device summaries, 60-point quantized history rings, and static hardware metadata)
- `services: Arc<ServicesSlice>` / `startup: Arc<StartupSlice>` / `users: Arc<UsersSlice>` / `app_history: Arc<AppHistorySlice>` (gated by `MonitorLease`; no polling overhead while unmounted)
- `settings: Arc<SettingsSlice>` (read-only configuration projection)

`TableDelegate::render_td` performs `O(1)` slice indexing across only the ~25–35 visible viewport rows (targeting `< 0.5 ms` per frame on the UI thread).

### 5.2 `gpui-kit` widget mapping

| UI Surface / Visual Element | `gpui-kit` (`gpui-component`) Control | Composition Strategy (No Custom Drawing) |
| --- | --- | --- |
| **Window chrome & global search** | `Root` + `TitleBar` + `Input` | Centered debounced `.cleanable(true)` input filtering by name, publisher, or PID |
| **Collapsible navigation rail** | `Sidebar`, `SidebarMenu`, `SidebarMenuItem`, `SidebarFooter` | Hamburger toggle switches `.collapsed(bool)`; `Settings` pinned in `SidebarFooter` |
| **Virtualized tables & tree tables** | `Table` + `TableState<D>` + `TableDelegate` | Column 0 composes `h_flex().pl(px(depth * 16.))` + chevron `Button` + cached `Img`; heatmap cells apply quantized theme background opacity |
| **Multi-level context menus** | `PopupMenu` / `.context_menu(...)` | Supports checkmarks (`✓ Efficiency mode`), separators, and nested submenus (`Set priority >`, `Resource values >`) |
| **Performance left device sparklines** | `v_virtual_list` + `AreaChart` | Configures `AreaChart::new(pts).linear().y_domain(0., 100.).point_count(60).x_axis(false).grid(false).interactive(false)` |
| **CPU kernel-time overlay & core grid** | Multi-series `AreaChart` + responsive grid | Dual `.y(...)` accessors render total + kernel utilization gradients; logical-core mode tiles compact `AreaChart` cells |
| **Disk dual charts / NIC / GPU engines** | `AreaChart` / `LineChart` + `Select` | Dynamic `y_domain` auto-scales throughput; GPU `2×2` engine headers embed borderless `Select` pickers |
| **Memory composition bar** | `h_flex` + `relative(ratio)` + `Tooltip` | Proportional Flexbox segments (`In use / Modified / Standby / Free`) with hover `Tooltip` |
| **Run Task / Affinity / WCT / Elevation** | `Dialog`, `Input`, `Checkbox`, `Tree`, `Notification` | Wait-chain blocking graphs render in `Tree`; `ElevationRequired` surfaces a relaunch `Notification` |

<a id="core-backend-subsystems"></a>

## 6. Core backend subsystems

### 6.1 Anti-PID-reuse process tree & tree termination (`loom-engine` & `loom-pal-windows`)

Windows frequently recycles exited `PID`s. Loom keys each process instance by `ProcessKey { pid, create_time_100ns }`:

- **Parentage validation**: A process attaches to `parent_pid` only if `parent.create_time_100ns <= child.create_time_100ns`; otherwise it promotes to a root node, preventing cyclic trees and stale metadata collisions.
- **Bottom-up tree termination (`KillProcessTree`)**: Collects validated descendants by `ProcessKey` and suspends/terminates leaves before parents so watchdog parents do not respawn children mid-termination.
- **Two-tier cold asset deduplication**: `ProcessKey -> Arc<ProcessColdMeta> -> Arc<ExeStaticAsset>` (keyed by normalized path). Extracted icons are downsampled to `20×20` RGBA8 (`1.6 KB` per unique binary) and shared across multi-process apps.

### 6.2 WMI/PDH-free hardware telemetry (`loom-pal-windows::performance`)

- **CPU**: `NtQuerySystemInformation(SystemProcessorPerformanceInformation)` for per-core `Idle/Kernel/User` ticks; `GetLogicalProcessorInformationEx` cached at boot for sockets, physical cores, NUMA, and L1/L2/L3 caches; `CallNtPowerInformation` for live clock speed (GHz).
- **Memory**: `GlobalMemoryStatusEx` + `GetPerformanceInfo` + `SystemMemoryListInformation` for memory composition; `GetSystemFirmwareTable('RSMB')` parsing for RAM speed (MT/s), slots used, and form factor.
- **Disk**: `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` maps physical drives to drive letters (e.g., `Disk 0 (C: D:)`), `IOCTL_STORAGE_QUERY_PROPERTY` identifies NVMe / SATA SSD / HDD and model strings, and `IOCTL_DISK_PERFORMANCE` provides active time, read/write rates, and response latency—all isolated per device with `300 ms` circuit-breaker timeouts.
- **Network**: `GetIfTable2` + `GetAdaptersAddresses` provide 64-bit octet counters, link speeds, media type (Ethernet / Wi-Fi), and IPv4/IPv6/DNS metadata.
- **GPU**: Vendor-neutral `DXGI` + `gdi32!D3DKMTQueryStatistics` measures per-node engine utilization (`3D`, `Copy`, `Video Encode`, `Video Decode`) plus dedicated/shared VRAM, paired with `D3DKMTQueryAdapterInfo(KMTQAITYPE_ADAPTERPERFDATA)` for die temperature and PCI bus location.

### 6.3 Cross-platform semantic normalization (for `0.2.0` Linux & `0.3.0` macOS)

| Unified Concept (`loom-types`) | Windows (`loom-pal-windows`) | Linux (`loom-pal-linux`, `0.2.0`) | macOS (`loom-pal-macos`, `0.3.0`) |
| --- | --- | --- | --- |
| **Lifecycle key `ProcessKey`** | `(PID, CreateTime FILETIME)` | `(pid_t, /proc/[pid]/stat starttime)` | `(pid_t, pbi_start_tvsec)` |
| **Efficiency mode (`EcoMode`)** | `EcoQoS` (`PROCESS_POWER_THROTTLING`) | `SCHED_IDLE` / `nice` / cgroup weight | Task QoS / App Nap |
| **System service (`ServiceRecord`)** | Windows SCM (`EnumServicesStatusExW`) | `systemd` D-Bus Units | `launchd` Daemons & Agents |
| **Startup entry (`StartupEntry`)** | Registry `Run`/`StartupApproved` + Folders + UWP `StartupTask` | XDG Autostart (`~/.config/autostart/*.desktop`) | `SMAppService` / `LaunchAgents` |
| **User session (`UserSession`)** | WTS (`WTSEnumerateSessionsW`) | `systemd-logind` Sessions | `utmpx` / Console User |
| **Virtualization / sandbox column** | UAC Virtualization (`TokenVirtualizationEnabled`) | Namespace / Flatpak / Snap | App Sandbox Entitlements |
| **Wait-chain diagnostics (`WaitChain`)** | Win32 WCT (`GetThreadWaitChain`) | `/proc/[pid]/task/[tid]/wchan` | `proc_pidinfo` Thread `wchan` |
| **On-demand elevation** | `ShellExecuteExW("runas")` | `pkexec` / Polkit Agent | Authorization Services |

<a id="core-extension-contracts"></a>

## 7. Core static extension contracts

```rust
// crates/loom-pal-api: compile-time monomorphized platform backend contract (no vtables)
pub trait PlatformBackend: Send + 'static {
    /// Compact Copy extension payload (use `()` 0-byte ZST on platforms needing no extra fields)
    type ProcessExt: Copy + Default + Send + Sync + 'static;

    type FastSampler: FastSystemSampler<Ext = Self::ProcessExt>;
    type SlowProber: SlowMetadataProber;
    type ServiceMgr: ServiceManager;
    type StartupMgr: StartupManager;
    type SessionMgr: SessionManager;
    type HistoryReader: AppHistoryReader;
    type Operator: SystemOperator;

    /// Static platform identity profile (`Windows` / `Linux` / `macOS`)
    const PLATFORM: PlatformProfile;

    /// Compile-time capability flags enabling dead-code elimination of unsupported branches
    const CAPABILITIES: CapabilityFlags;

    /// Startup priority boost on process entry
    fn boost_self_priority();

    /// Host main window z-order & restore hooks (default no-op for headless/unsupported platforms)
    fn set_main_window_always_on_top(_enabled: bool) {}
    fn is_main_window_minimized() -> bool { false }
    fn restore_and_focus_main_window() {}

    fn initialize() -> Result<Self, PalError>
    where
        Self: Sized;
}

// crates/loom-pal-api: Fast Lane contract (in-memory kernel calls only; reuses SampleScratch)
pub trait FastSystemSampler: Send {
    type Ext: Copy + Default + Send + Sync + 'static;

    fn sample_processes(
        &mut self,
        scratch: &mut ProcessSampleScratch<Self::Ext>,
    ) -> Result<Timestamp100Ns, PalError>;

    fn sample_performance(
        &mut self,
        scratch: &mut PerfSampleScratch,
    ) -> Result<Timestamp100Ns, PalError>;
}

// crates/loom-pal-api: system control & diagnostics contract (returns PalError::ElevationRequired)
pub trait SystemOperator: Send + Sync {
    fn run_new_task(&self, req: &RunTaskRequest) -> Result<Option<u32>, PalError>;
    fn terminate_process(&self, key: ProcessKey) -> Result<(), PalError>;
    fn terminate_process_tree(&self, targets: &[ProcessKey]) -> Result<TreeKillReport, PalError>;
    fn set_efficiency_mode(&self, key: ProcessKey, enabled: bool) -> Result<(), PalError>;
    fn set_priority(&self, key: ProcessKey, priority: ProcessPriority) -> Result<(), PalError>;
    fn set_affinity(&self, key: ProcessKey, mask: &CpuAffinityMask) -> Result<(), PalError>;
    fn set_uac_virtualization(&self, key: ProcessKey, enabled: bool) -> Result<(), PalError>;
    fn analyze_wait_chain(&self, key: ProcessKey) -> Result<Vec<WaitChainNode>, PalError>;
    fn write_minidump(&self, key: ProcessKey, dest: &Path, cancel: &AtomicBool) -> Result<(), PalError>;
    fn relaunch_self_elevated(&self, restore_route: &str) -> Result<(), PalError>;
}
```

<a id="ecosystem-reuse"></a>

## 8. Open-source ecosystem reuse

| Capability Domain | Mature Crates Reused | Thin In-House Layer |
| --- | --- | --- |
| **Desktop UI, charts & tables** | `gpui-kit` (umbrella crate bundling compatible `gpui`, `gpui-base`, `gpui-component`, and icon assets) | `loom-ui-kit`: heatmap cells, sparkline device cards, core grids, and segmented memory bar composites |
| **Windows kernel & graphics FFI** | `windows-sys` (C ABI bindings: `ntdll`, `iphlpapi`, `gdi32`, `dxgi`, `advapi32`, `dbghelp`, `wtsapi32`, `shell32`) | `loom-pal-windows`: safe wrappers, reusable scratch buffers, raw SMBIOS parser, and `CircuitBreaker` bulkhead |
| **Lock-free snapshots & channels** | `arc-swap` + `crossbeam-channel` / `async-channel` + `parking_lot` (no multi-thread Tokio) | Per-domain `Revision` projections, pre-sorted view builder, and RAII `MonitorLease` |
| **Compact storage & bitsets** | `bitflags` + `smallvec` | Fixed-size quantized `HistoryRing<N>`, global `StringInterner`, and `20×20` icon cache |
| **Tray & single-instance** | `tray-icon` (with `ksni` reserved for Linux) + `single-instance` + `directories` | Window teardown VRAM release, single-instance focus wakeup, and minimize-hibernation hooks |
| **Config & atomic writes** | `serde` + `toml` + `tempfile` + `thiserror` | `loom-config` fast startup load, schema migrations, and `tempfile -> sync_all -> rename` |
| **Localization & diagnostics** | `rust-i18n` + `tracing` + `tracing-subscriber` | Live `zh-CN` / `en-US` switching and non-blocking diagnostic logging |
