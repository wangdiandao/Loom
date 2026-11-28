# Changelog

See [versioning and releases](docs/release.md) for the version progression policy, milestone roadmap, and release process.

## Unreleased

### Engine & PAL

- Extended `loom-types` additively while keeping `Copy` hot-sample layouts intact: added `GAP_SENTINEL` discontinuity markers (`push_gap`, `is_gap`, `to_vec_opt_f32` / `to_vec_opt`, gap-aware `max_value`) on `HistoryRing<N>` and `ThroughputRing<N>`, heterogeneous `CpuCoreKind` (`CpuStaticInfo::core_kinds`), `d3_asleep` on `GpuAdapterSample`/`GpuAdapterSlice`, `SocketProtocol`/`ProcessSocketRecord`, `FileLockHolder`, `LeakKind`/`LeakWarning`, `WebView2Role`, `MonitorDomain::ProcessNetwork`, and `CapabilityFlags::{SUSPEND_RESUME, FILE_LOCKS, PROCESS_SOCKETS, ELEVATED_PROCESS_NET}`.
- Extended frozen v1 `loom-pal-api` traits (`PAL_API_CONTRACT_VERSION = 1`) exclusively with defaulted additive methods: `AppHistoryReader::save_history`, `FastSystemSampler::set_per_process_network_enabled`, `SlowMetadataProber::list_listening_ports`, and `SystemOperator::{suspend_process, resume_process, restart_explorer, find_file_locks}`.
- Upgraded `loom-engine` orchestration and wired Windows Wave 1 telemetry glue (`CpuStaticInfo::core_kinds`, `GpuAdapterSample::d3_asleep`, and elevated-only `ProcessNetwork` ETW collector lifecycle):
  - Added zero-polling event-driven `SnapshotWakeSignal` (`snapshot_wake_signal`, `subscribe_snapshot_wake`, `set_snapshot_waker`) firing on every `AppSnapshot` publication and `CommandEvent` emission.
  - Added row-order freeze (`SetProcessOrderFreeze`, `SetDetailsOrderFreeze`), temporary Hold-`Ctrl` freeze (`SetTempFreeze` preserving `RefreshSpeed`), and immediate `ForceRefresh`.
  - Added structured search query parser (`query.rs`: OR `,`, AND whitespace, `!term`/`-term` negation, `pid:`, `user:`, `cpu:`, `mem:`, `status:`, `:PORT`/`port:PORT`) with strictly on-demand slow-lane `:PORT` lookup (`2 s` cache while active, zero scans otherwise) and `SetHideSystemProcesses` filtering (exempting interactive shell tools).
  - Added gap-sentinel (`push_gap`) discontinuity insertion across performance history rings on pause-resume, sampling error, or wall/clock jump (`> max(interval * 3, 5 s)`), plus periodic and shutdown `AppHistoryReader::save_history` persistence without double-counting.
  - Wired `SuspendProcess`, `ResumeProcess`, `RestartExplorer`, `FindFileLocks` (`CommandSuccess::FileLocks`), RAII `MonitorDomain::ProcessNetwork` ETW lease gating, zero-syscall WebView2 host attribution & role classification (`webview2.rs`, `SetWebView2HostGrouping`), and zero-syscall in-memory resource leak heuristics (`leaks.rs`: Handles / Children / Orphans, `SetShowLeakWarnings`, `IgnoreLeakPath`).

### Windows Backend

- Implemented WMI/PDH-free Windows process controls and diagnostics in `loom-pal-windows` while advertising `CapabilityFlags::{SUSPEND_RESUME, FILE_LOCKS, PROCESS_SOCKETS, ELEVATED_PROCESS_NET}`:
  - Safe `suspend_process` / `resume_process` via dynamically loaded `ntdll!NtSuspendProcess` / `NtResumeProcess` with `ProcessKey` verification, `IsProcessCritical` refusal, and `dwm.exe`/kernel/self-PID guards.
  - Safe `restart_explorer` and de-elevated `run_new_task` spawning via the desktop shell primary token (`GetShellWindow` -> `DuplicateTokenEx` -> `CreateProcessWithTokenW`) when Loom runs elevated, with graceful fallback.
  - On-demand listening TCP/UDP port enumeration (`ports.rs`) via dynamically loaded `iphlpapi.dll` (`GetExtendedTcpTable` / `GetExtendedUdpTable` for IPv4 & IPv6).
  - File/directory lock inspection (`filelock.rs`) via dynamically loaded Restart Manager (`rstrtmgr.dll`) with RAII session teardown and recursive binary-split batch fallback.
  - Clean-room elevated-only per-process ETW network byte collector (`etw_net.rs`, `Microsoft-Windows-Kernel-Network`, bounded to 1 thread with clean `ControlTraceW` teardown) and wired `AppHistoryReader::save_history`.
- Added a hybrid GPU `D3` power-gate (`GpuPowerGate` + dynamic `cfgmgr32.dll` `DEVPKEY_Device_PowerData`) in `loom-pal-windows` so sleeping hybrid discrete GPUs (`D1`–`D3`) skip `KMTQAITYPE_ADAPTERPERFDATA` without waking the dGPU, and idle `D0` queries are throttled to `20 s`.
- Implemented pure kernel-mode `gdi32!D3DKMTQueryStatistics` per-process GPU telemetry (`gpu_dedicated_bytes`, `gpu_shared_bytes`, `gpu_util_permille`) using short-lived `PROCESS_QUERY_LIMITED_INFORMATION` handles, zero PDH, `checked_sub` counter-reset safety, and stale `ProcessKey` pruning.
- Upgraded Windows fast-lane CPU accounting to prefer `NtQuerySystemInformationEx(SystemProcessorIdleCycleTimeInformation)` with QPC-validated per-group cycle baselines and clean fallback to `SystemProcessorPerformanceInformation`, plus heterogeneous `P`/`E`/`LP-E` core topology classification via `EfficiencyClass` and L3 cache sharing.
- Added PnP storage controller driver allowlisting (`DEVPKEY_Device_Service`: `stornvme`, `storahci`, `atapi`), rotational HDD activity-change guarding, `CM_Register_Notification` safe-eject cooperation (`DEVICEQUERYREMOVE`), `checked_sub` counter-reset hardening for disk and network throughput deltas, and additive `ProcessSequenceNumber` (`NtQueryInformationProcess` class `92`) PID-reuse protection.

### UI & UX

- Migrated `UiStore` in `loom-app` from 20 Hz (`50 ms`) timer polling to zero-polling event-driven push wakeups via `EngineCoordinator::snapshot_wake_signal()` (cancelling cleanly in dormant tray mode), and exposed typed `UiCommands` bridge helpers (`toggle_pause_refresh` remembering prior active cadence, `force_refresh`, `set_temp_freeze`, `set_process_order_freeze`, `set_details_order_freeze`, `suspend_process`, `resume_process`, `restart_explorer`, `find_file_locks`, `set_webview2_host_grouping`, `set_show_leak_warnings`, `ignore_leak_path`, and `set_hide_system_processes`).
- Added global desktop shell keyboard shortcuts (`Ctrl+F` / `/`, `Esc`, `Space` / `P`, `F5`, `Ctrl+1`–`Ctrl+8`, and Hold-`Ctrl` temporary freeze with focus-loss auto-release and TitleBar status badges), plus an interactive `?` structured search syntax help `Popover` with clickable query example pills.
- Added a TitleBar shell tools menu featuring a privacy-safe **Copy diagnostic summary** action (strictly omitting PIDs, usernames, command lines, paths, IPs, and hardware serials) and a Restart Manager **Find file locks** dialog (`FileLockHolder` rows with **Details** jump and **Restart Explorer** action), alongside a lightweight `0% GPU` system tray **Quick Overview** popover (`~380×440`) sharing `UiStore`/`ArcSwap<AppSnapshot>` and destroying its window immediately on close or focus loss.
- Added pure-Rust UI layout, chart scaling, and export logic in `loom-ui-kit::logic`: smooth stepped disk throughput (`stepped_throughput_ceiling`) and network bit-rate (`stepped_network_ceiling`) Y-axis ceilings, adaptive logical-core grid layout scoring (`compute_core_grid_layout` targeting `~1.4:1` plot aspect ratio with `8%` resize hysteresis and `Full`/`Short`/`Hidden` progressive header degradation), active GPU engine pane selection (`select_active_gpu_engines`), stable open-time ordering (`merge_stable_order`), and UTF-8 BOM / RFC 4180 formula-injection-safe CSV formatting helpers (`format_csv_cell`, `build_csv_document`).
- Added W3C WCAG 2.1 relative luminance and contrast solver utilities (`srgb_relative_luminance`, `wcag_contrast_ratio`, `ensure_wcag_contrast`, `hsla_to_srgb`) plus heterogeneous CPU core (`cpu_core_p`, `cpu_core_e`, `cpu_core_lp_e`), `leak_warning`, and `sort_paused` semantic color tokens in `LoomSemanticColors` with automated light/dark AA contrast verification (`>= 4.5:1` text, `>= 3.0:1` charts/accents).
- Added honest unavailable metric formatters (`UNAVAILABLE_LABEL` `"—"`, `format_opt_percent_permille`, `format_opt_temp_celsius`) and gap-aware sparkline window/segment builders (`GapAwareSparklinePoint`, `PERMILLE_GAP_SENTINEL`, `SparklinePoint::from_opt_f32_segments` / `from_opt_permille_segments` / `from_f32_gap_safe_segments` / `from_permille_sentinel_segments` / `from_opt_f32_padded` / `from_opt_f32_window`) in `loom-ui-kit`.

### Config & Localization

- Added backward-compatible `#[serde(default)]` v1 optional preferences to `loom-config` (`AppSettings::{webview2_host_grouping, show_leak_warnings, leak_ignored_paths, hide_windows_system_processes}`) while keeping `CONFIG_CONTRACT_VERSION = "1.0"` and `CURRENT_SCHEMA_VERSION = 1` intact.

## 0.1.0 - 2026-11-27

### Engine & PAL

- Froze the cross-platform zero-cost static-dispatch PAL contract in `loom-pal-api` (`PAL_API_CONTRACT_VERSION = 1`), locking all 8 core traits (`PlatformBackend`, `FastSystemSampler`, `SlowMetadataProber`, `ServiceManager`, `StartupManager`, `SessionManager`, `AppHistoryReader`, and `SystemOperator`), associated type/thread-safety bounds, `Copy` hot sample layouts (`ProcessKey`, `ProcessHotSample<()>`, `SystemPerfSample`), and reusable scratch buffers for Windows and upcoming `0.2.0` Linux / `0.3.0` macOS backends, guarded by compile-time contract-freeze tests.

### Config & Localization

- Froze the `loom-config` v1 configuration forward-migration contract (`CONFIG_CONTRACT_VERSION = "1.0"`, `CURRENT_SCHEMA_VERSION = 1`), backed by deterministic golden `settings.toml` round-trip tests, legacy/partial v0 forward-migration coverage, forward tolerance for unknown future optional keys, and structured `ConfigError::UnsupportedSchemaVersion` rejection for future incompatible schema versions (`schema_version = 2`).

### Windows Backend & Release Packaging

- Delivered Loom's first stable Windows release (`0.1.0`), consolidating all 8 native Task Manager views (`Processes`, `Performance`, `App History`, `Startup Apps`, `Users`, `Details`, `Services`, and `Settings`), WMI/PDH-free hardware performance telemetry, kernel diagnostics (`WCT` wait chains, streaming `MiniDump`, live UAC virtualization, JIT debugger attach, global wakeup hotkey), and verified engineering budgets (`< 50 ms` cold start, `< 0.15%` steady-state CPU, bounded threads, and `300 ms` slow-I/O circuit-breaker isolation).
- Added Windows WiX v4/v5 MSI installer definition (`packaging/windows/wix/main.wxs`) installing `loom-app.exe`, `LICENSE`, and Start Menu shortcut (`Loom Task Manager`) with stable `UpgradeCode` and `MajorUpgrade` support.
- Added Windows release packaging script (`packaging/windows/package-release.ps1`) emitting `loom-<Version>-windows-<Arch>-portable.zip`, `loom-<Version>-windows-<Arch>.msi`, and `SHA256SUMS.txt`, with conditional Authenticode code signing (`LOOM_SIGN_PFX_BASE64` + `LOOM_SIGN_PFX_PASSWORD` or `LOOM_SIGN_CERT_PATH`) and secure temporary `.pfx` cleanup.
- Added automated CI release workflow (`.github/workflows/release.yml`) triggered on `v*` tags and `workflow_dispatch` dry-runs: verifies Windows workspace and Linux headless crates, packages Windows MSI + Portable ZIP artifacts with optional signing secrets (`WINDOWS_SIGN_PFX_BASE64` / `WINDOWS_SIGN_PFX_PASSWORD`), extracts matching release notes from `CHANGELOG.md`, and publishes GitHub Releases with checksums.

### Contributors

- @wangdiandao

**Full Changelog**: https://github.com/wangdiandao/Loom/compare/v0.0.6...v0.1.0

## 0.0.6 - 2026-11-27

### Engine & PAL

- Added `CapabilityFlags::{DEBUGGER_ATTACH, GLOBAL_HOTKEY}` in `loom-types`, `PalError::Cancelled` and `SystemOperator::attach_debugger` in `loom-pal-api`, backward-compatible `AppSettings::global_hotkey` persistence with whitespace sanitization in `loom-config`, and cross-platform `GlobalHotkeyController` in `loom-pal`.
- Added engine commands `SetUacVirtualization`, `AnalyzeWaitChain`, `WriteMiniDump` (with shared `Arc<AtomicBool>` cancellation), and `AttachDebugger` in `loom-engine`, routing wait-chain, minidump, and debugger operations onto the bounded slow-lane bulkhead pool while updating `ColdMetaCache` and reprojecting Details live on UAC virtualization toggles.

### Windows Backend

- Implemented Windows Wait Chain Traversal (`OpenThreadWaitChainSession` / `GetThreadWaitChain` dynamically loaded from `advapi32.dll` with RAII session cleanup), cancellable streaming memory dumps (`MiniDumpWriteDump` dynamically loaded from `dbghelp.dll` with `CancelCallback` partial-file cleanup), live UAC virtualization toggling (`SetTokenInformation(TokenVirtualizationEnabled)`), JIT debugger attachment (`AeDebug\Debugger` registry lookup + `CreateProcessW`), and zero-idle on-demand global wakeup hotkeys (`RegisterHotKey` / `UnregisterHotKey`) in `loom-pal-windows`, advertising `CapabilityFlags::{WAIT_CHAIN, MINIDUMP, DEBUGGER_ATTACH, GLOBAL_HOTKEY}`.

### Performance & Diagnostics

- Deferred Explorer system tray (`Shell_NotifyIconW`) and global wakeup hotkey registration past `open_main_window` in `loom-app` so OS shell IPC does not delay initial window creation on startup.
- Added deterministic 512-process high-load fault-injection stress tests (`high_load_500_processes_hung_windows_and_slow_disk_stress_test`) and dormancy/unleased-domain zero-overhead regression tests (`steady_state_dormancy_and_unleased_domains_zero_overhead_test`) in `loom-engine`, verifying non-blocking fast-lane ticks under `410–500 ms` slow-disk/asset stalls, `300 ms` `CircuitBreaker` drive bypass, `NOT_RESPONDING` hung-window projection, bottom-up process tree termination, and zero sampling ticks while dormant.
- Extended real-Windows headless end-to-end verification (`headless_windows_engineering_budget_verification` in `loom-app`) covering cold/warm config load latency, `EngineCoordinator::<NativeBackend>::new` time-to-first-snapshot (`< 50 ms` release), bounded `<= 3` engine background thread delta, steady-state 1s headless CPU% budget, and full `Performance`/`PerCoreCpu`/`Services`/`Startup`/`Users`/`AppHistory` RAII lease acquire/release isolation.

### UI & UX

- Added feature-local **Analyze wait chain** `Dialog` in **Details** using `gpui-kit`'s `Tree` widget with deadlock badges and inline **End blocking process** controls, plus a shared **Create memory dump file** progress/cancel dialog (with **Open file location** on completion) accessible from both **Processes** and **Details** context menus.
- Wired **UAC virtualization** checkbox toggling and **Attach debugger** context menu actions (gated by `CapabilityFlags`), and added a live **Global wakeup hotkey** preset selector (`Disabled`, `Ctrl+Shift+L`, `Ctrl+Alt+L`, `Ctrl+Shift+M`, `Alt+Shift+T`) in **Settings** that wakes/restores Loom from minimized or tray state, with full `en-US` and `zh-CN` localization.

## 0.0.5 - 2026-11-27

### Engine & PAL

- Extended `loom-types` with `UserSessionProcess` (nested inside `UserSessionRecord::processes`) and `StartupEntry::file_path` for per-session process inspection and startup shell actions.
- Added 7 slow-lane system management commands in `loom-engine` (`StartService`, `StopService`, `RestartService`, `OpenServicesConsole`, `SetStartupEnabled`, `DisconnectSession`, and `ClearAppHistory`) with `PalError::ElevationRequired` propagation and automatic immediate refresh of leased domains (`Services`, `Startup`, `Users`, `AppHistory`).
- Wired live in-memory `Users` session process aggregation (`process_count`, clamped `cpu_permille`, `memory_bytes`, `disk_bytes_per_sec`, `net_bytes_per_sec`, and sorted `processes`) and live cumulative `AppHistory` CPU/network delta tracking merged with persisted baselines while strictly enforcing zero polling when unleased.

### Windows Backend

- Implemented WMI/PDH-free Windows system management backends in `loom-pal-windows` and advertised `CapabilityFlags::{SERVICES, STARTUP_APPS, USER_SESSIONS, APP_HISTORY}`:
  - **Services** (`WindowsServiceManager`): SCM enumeration via `EnumServicesStatusExW`, cached `svchost -k <Group>` / `Group` and indirect `@...` description resolution, lifecycle control (`StartServiceW` / `ControlService` / restart polling), and `services.msc` launcher with `ERROR_ACCESS_DENIED` mapped to `PalError::ElevationRequired`.
  - **Startup Apps** (`WindowsStartupManager`): BIOS POST time (`FwPOSTTime`), `HKCU`/`HKLM`/`WOW6432Node` Run keys, User/Common Startup folders (with offline `.lnk` target parsing), UWP `StartupTask` states, UNC/`DRIVE_REMOTE`-safe local PE `CompanyName` lookup, and `StartupApproved` / UWP toggle writes.
  - **Users** (`WindowsSessionManager`): WTS session enumeration (`WTSEnumerateSessionsW` / `WTSQuerySessionInformationW`) skipping Session 0/listener rows and non-blocking `WTSDisconnectSession`.
  - **App History** (`WindowsAppHistoryReader`): crash-safe atomic local history persistence and clear store under `%LOCALAPPDATA%\Loom\app_history.dat`.

### UI & UX

- Implemented the four Milestone `0.0.5` on-demand system management vertical feature slices in `loom-app`, each gated by an RAII `ScopedMonitorLease` (`Services`, `Startup`, `Users`, `AppHistory`), composed exclusively with `gpui-kit` `DataTable`/`Dialog`/`PopupMenu` widgets and reactive to the TitleBar global search box via `UiStoreEvent::SearchQueryChanged`:
  - **Services** (`ServicesView`): split static toolbar (`Start`, `Stop`, `Restart`, `Open Services`), clearable host-PID filter banner (`JumpToServices(pid)`), sortable `Name`/`PID`/`Description`/`Status`/`Group` table anchored by service name, and row context menu with `Go to details` resolving live `ProcessKey` by PID.
  - **Startup Apps** (`StartupView`): split static toolbar (`Enable`, `Disable`, and right-aligned `Last BIOS time: X.X s`), sortable `Name`/`Publisher`/`Status`/`Startup impact` table anchored by entry ID, and row context menu with `Open file location`, `Properties`, and `Search online`.
  - **Users** (`UsersView`): split static toolbar (`Disconnect` with confirmation `Dialog`, `Manage user accounts` launching `netplwiz`), expandable session-tree table with `heatmap_cell` CPU/Memory/Disk/Network columns, auto-expanding child search filtering, and child process `End task` & `Go to details` context actions.
  - **App History** (`AppHistoryView`): split static toolbar (`Delete usage history` with confirmation `Dialog` and `Resource usage since YYYY-MM-DD HH:MM` indicator) and sortable `Name`/`CPU time`/`Network`/`Metered network` heatmap table.
- Completed bidirectional **Details** <-> **Services** navigation by adding `ShellHandle::jump_to_services` and the **Go to service(s)** context menu action in `DetailsView`, retiring the temporary upcoming-milestone placeholder across all 8 routes, and adding full `en-US` and `zh-CN` localization.

## 0.0.4 - 2026-11-27

### Engine & PAL

- Added fixed-size stack-allocated `ThroughputRing<N>` and structured hardware telemetry types (`CpuStaticInfo`, `MemoryHardwareInfo`, `HardwareStaticInfo`, `DiskMediaType`, `DiskDeviceSample`/`DiskDeviceSlice`, `NetConnectionType`, `NetAdapterSample`/`NetAdapterSlice`, `GpuEngineSample`/`GpuEngineSlice`, `GpuAdapterSample`/`GpuAdapterSlice`) in `loom-types`, extending `PerformanceSlice` with per-device metadata and 60-point history rings.
- Extended `PerfSampleScratch` (`networks`, `gpus`) and `SlowMetadataProber` (`probe_hardware_static_info`, `probe_disk_performance`) in `loom-pal-api`, and wired `loom-engine` to gate static hardware and deduplicated slow-lane disk polling strictly behind `MonitorDomain::Performance` leases while maintaining 60-point histories across CPU, Memory, Per-Core CPU, Disk `0..N`, Network adapters, and GPU `0..N` engines.
- Fixed a fast-lane lease wakeup race in `FastLaneRunner` where acquiring `MonitorDomain::PerCoreCpu` on the same tick after `MonitorDomain::Performance` had already sampled skipped per-core sampling until the next tick (stalling indefinitely when `RefreshSpeed::Paused`).
- Refined Windows **Processes** `Grouped` view (`PlatformKind::Windows` only) to group multi-process apps strictly by app identity—MSIX/UWP Package Family Name (`ProcessColdMeta::app_group_id` via `GetPackageFamilyName`) or executable path / image name—rather than swallowing different-executable OS children (keeping `WeChatAppEx.exe`, `crashpad_handler.exe`, etc. in `Background`), displaying total group process counts (`(N)`), aggregating CPU/Memory/Disk/Network/GPU metrics, rolling up `EFFICIENCY_MODE`/`NOT_RESPONDING` (and `SUSPENDED` only when all group members are suspended), and keeping `Process Tree` and non-Windows (`Linux` / `macOS`) grouping unchanged.
- Aligned the Windows **Processes** `Grouped` view (`PlatformKind::Windows` only) with Windows Task Manager: stopped nesting independent desktop apps or background daemons under `explorer.exe`, grouped windowless multi-process helper/renderer descendants (and same-image orphaned helpers) under their owning top-level `App` row with aggregated CPU/Memory/Disk/Network/GPU metrics and rolled-up EcoQoS/`Not responding` status flags, and auto-expanded owning `App` groups when searching for child processes—while keeping `Process Tree` and non-Windows (`Linux` / `macOS`) grouping unchanged.

### Windows Backend

- Implemented WMI/PDH-free Windows hardware performance monitoring in `loom-pal-windows`:
  - **CPU**: dynamic and nominal frequency via `CallNtPowerInformation(ProcessorInformation)`, static sockets/physical cores/L1–L3 cache topology via `GetLogicalProcessorInformationEx`, processor brand from registry, and firmware virtualization status via `IsProcessorFeaturePresent(PF_VIRT_FIRMWARE_ENABLED)`.
  - **Memory**: modified/standby/free page composition via `NtQuerySystemInformation(SystemMemoryListInformation)` with safe fallback and a pure-Rust SMBIOS Type 17 parser (`GetSystemFirmwareTable('RSMB', 0)`) for RAM speed (MT/s), slot counts, form factor, and hardware-reserved bytes.
  - **Disk `0..N`**: slow-lane physical disk telemetry via `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`, `IOCTL_STORAGE_QUERY_PROPERTY` (model, NVMe/SSD/HDD detection), `IOCTL_DISK_GET_LENGTH_INFO`, and `IOCTL_DISK_PERFORMANCE` deltas with mandatory `DRIVE_REMOTE`/`DRIVE_CDROM` bypass and per-device `300 ms` `CircuitBreaker` isolation.
  - **Network**: operational Ethernet and Wi-Fi adapter throughput, link speed, IPv4/IPv6, and DNS suffix via `iphlpapi` (`GetIfTable2` + `GetAdaptersAddresses`).
  - **GPU `0..N`**: dynamic `dxgi.dll` (`CreateDXGIFactory1`) and `gdi32.dll` (`D3DKMTQueryStatistics` / `D3DKMTQueryAdapterInfo`) VRAM usage, per-node engine utilization, optional core temperature, PCI location, WDDM/DirectX version, and display driver version with safe headless/software fallback.

### UI & UX

- Implemented the **Performance** vertical feature slice (`PerformanceView`) with an `h_resizable` split layout, scrollable left sparkline device rail (`CPU`, `Memory`, `Disk 0..N`, `Ethernet`/`Wi-Fi`, `GPU 0..N`), and live `gpui-kit` `AreaChart` detail views with zero custom `Canvas` or GPU shaders:
  - **CPU**: overall vs. responsive logical-processors grid (`1`/`2`/`4`/`8` columns) with dynamic RAII `MonitorDomain::PerCoreCpu` lease gating, `Show kernel times` dual-series overlay, right-click chart context menu, and full speed/topology/virtualization/L1–L3 cache metrics.
  - **Memory**: 60-second usage chart, interactive 4-segment `memory_composition_bar` with localized tooltips and legend, and live pool/commit + SMBIOS hardware metrics.
  - **Disk `0..N`**: active-time % and adaptive-scale Read/Write transfer rate charts, NVMe/SSD/HDD and circuit-breaker bypass badges, and latency/capacity/drive-letter metrics.
  - **Network**: adaptive-scale (`Kbps`/`Mbps`/`Gbps`) Receive/Send throughput chart with link speed, IPv4/IPv6, and DNS suffix details.
  - **GPU `0..N`**: `2×2` multi-engine grid (with per-pane `Select` engine dropdowns defaulting to `3D`/`Copy`/`Video Decode`/`Video Encode`) and single-engine view toggle, Dedicated/Shared VRAM charts, temperature, driver/DirectX version, and PCI location.
- Added `format_freq_ghz`, `format_bits_per_sec`, `format_latency_us`, `adaptive_throughput_scale`, `adaptive_network_scale`, right-aligned padded `SparklinePoint` constructors, and dual-series semantic chart colors in `loom-ui-kit`, plus full `en-US` and `zh-CN` localization for the Performance view.
- Enabled `gpui-kit`'s `gpui-fast` rendering backend (Retained Subtrees + Splice, cross-frame Taffy layout retention, fast `NumberShaping`, GPU Scroll Layers, and D3D11 pipeline state deduplication) and migrated both **Processes** and **Details** tables to split static toolbars + 2D-virtualized `gpui-kit` `DataTable` entities with left-pinned `Name` columns, single table-level row context menus, lazy accessibility labels, zero-copy `SharedString` cell text, zero-allocation fast-path metric formatters, and path-cached `EcoQoS` SVG icons targeting 120 FPS scrolling and live updates.
- Added table column header right-click context menus (`Hide column`, `Select columns...`, `Reset default columns`, plus Processes-only `Resource values >`) and a scrollable **Select columns** checkbox dialog to both **Processes** and **Details** tables with distinct per-page column catalogs (`Type`, `PID`, `Process name`, `Status`, `Publisher`, `CPU`, `Memory`, `Disk`, `Network`, `GPU`, `Command line` on Processes; `PID`, `Status`, `User name`, `Session ID`, `CPU`, `CPU time`, `Working set delta (memory)`, `Working set (memory)`, `Commit size`, `Priority`, `Handles`, `Threads`, `Platform`, `UAC virtualization`, `Elevated`, `Dedicated GPU memory`, `Publisher`, `Description`, `Image path name`, `Command line` on Details), full-column sorting, and persistent layout/visibility storage in `settings.toml`.
- Replaced the **Processes** (`Grouped` / `Process tree`) and **Details** (`Flat list` / `Process tree`) text view-mode switcher buttons with compact icon-only toggle buttons (`LayoutList` / `List` / `ListTree`) with localized hover tooltips and accessibility labels.
- Fixed the **Settings** sidebar footer entry height by rendering it with `SidebarMenuItem` directly so it matches the standard `28px` height and collapsed tooltip behavior of the primary navigation items.
- Fixed the **TitleBar** sidebar collapse/expand toggle button (and search box controls) failing to respond to clicks on Windows by stopping left mouse-down propagation before `DefWindowProcW` enters a `WM_NCLBUTTONDOWN(HTCAPTION)` window-drag loop.
- Added interactive column header resizing (draggable column dividers with min/max clamping) and horizontal drag-and-drop column reordering (with live insertion indicator and ghost header preview) to both the **Processes** and **Details** tables, preserving custom column order and widths across page navigation within the session.
- Removed the trailing grey `.exe` subtitle suffix from **Processes** rows so grouped app rows display clean names and counts (e.g., `QQ (5)`, `Weixin (5)`, `Loom 任务管理器`).

- Simplified **Processes** status badges to compact icon-only indicators with hover tooltips and streamlined the **Processes** toolbar by removing the summary metric pills, **Efficiency mode** button, and **Refresh** button (retaining Efficiency mode in the context menu).
- Streamlined the **Details** toolbar by removing the on-demand lease badge, row count pill, **End process tree** toolbar button, and **Refresh** button (retaining **End process tree** in the context menu); simplified **Details** status cells to plain text (`Running` / `Suspended`), stripped machine/domain prefixes from displayed usernames, and renamed the memory column label to **工作集增量（内存）** (`Working set delta (memory)`).
- Removed explanatory subtitle and per-row description texts from the **Settings** page for a cleaner, more compact layout.

## 0.0.3 - 2026-11-26

### Engine & PAL

- Extended `loom-types`, `loom-pal-api`, and `loom-engine` for the P0 daily-driver process management milestone: added `ProcessRowProjection` / `DetailsRowProjection` full-field metrics (CPU, Memory, Disk throughput, Network throughput, GPU utilization, dedicated GPU memory, CPU time, Working Set, Private Working Set, Commit, platform architecture, UAC virtualization, EcoQoS state, PE publisher/description, and command line), `ProcessSortColumn` / `DetailsSortColumn`, and `DetailsViewMode` (`Flat` / `Tree`).
- Added `EngineCommand` and `SystemOperator` support for `GetAffinity` (multi-group >64 CPU affinity mask readback), `BrowseExecutable` (native file picker routed on the slow lane), `SetProcessSort`, `SetDetailsSort`, `SetDetailsViewMode`, `ToggleDetailsExpanded`, `OpenFileLocation`, `ShowFileProperties`, and `SearchOnline`.
- Added oneshot reply channels (`send_command_with_reply`) in `EngineCoordinator` so interactive dialogs (`RunNewTaskDialog` Browse and `SetAffinityDialog` affinity load) resolve asynchronously without blocking the UI or fast sampler threads.

### Windows Backend

- Implemented live EcoQoS Efficiency Mode probing (`GetProcessInformation(ProcessPowerThrottle)`) and toggling (`SetProcessInformation` + `BELOW_NORMAL_PRIORITY_CLASS` fallback) with `ProcessKey` creation-time identity verification.
- Implemented multi-group (>64 logical processors) CPU affinity read/write (`GetLogicalProcessorInformationEx` + `GetProcessGroupAffinity` / `SetProcessAffinityMask`), 6-level priority classes, UAC virtualization and elevated token probing, native `GetOpenFileNameW` executable picker, and shell actions (`explorer.exe /select,`, `ShellExecuteExW` properties, and online search).
- Completed non-blocking top-level window grouping, per-process disk I/O rate deltas, and slow-lane PE `VersionInfo` publisher/description plus `20x20` RGBA8 icon backfill with UNC/`DRIVE_REMOTE` network-drive bypass and `300 ms` circuit-breaker protection.

### UI & UX

- Upgraded the **Processes** slice with 3-section grouped headers (`Apps` / `Background processes` / `Windows processes`), parent-child **Process Tree** toggle, sortable columns, `20x20` cached process icons, amber intensity heatmaps, status badges (`Efficiency mode`, `Suspended`, `Not responding`, `Elevated`), per-column **Resource Values** (`Values` vs. `Percent`) toggle, `ProcessKey`-anchored selection & `Delete` hotkey, and full right-click context menu (`Expand`/`Collapse`, `End task`, `Efficiency mode`, `Resource values`, `Open file location`, `Search online`, `Properties`, and `Jump to details`).
- Added the shell-scoped **Run new task** modal dialog (accessible from both **Processes** and **Details** toolbars) with native executable **Browse**, **Create this task with administrative privileges** checkbox, and persisted MRU command history pills.
- Upgraded the RAII-leased **Details** slice with full-field virtualized **Flat** and **Process Tree** views (`Name`, `PID`, `Status`, `User name`, `CPU`, `Memory`, `Priority`, `Platform`, `UAC virtualization`, `Dedicated GPU memory`, `Publisher`, `Command line`), sortable headers, `JumpToDetails(ProcessKey)` cross-page scroll-and-highlight, bottom-up **End process tree** confirmation, 6-level **Set priority** submenu, feature-local multi-group **Set Affinity** dialog (`All processors` + per-group `CPU 0..N` checkboxes), and shell actions.

## 0.0.2 - 2026-11-26

### UI & UX

- Implemented `loom-ui-kit` composed widgets on top of `gpui-kit` without custom `Canvas` or GPU shaders (`LoomSemanticColors` theme semantics, `heatmap_cell`, `AreaChart`-based `sparkline_card`, four-segment `memory_composition_bar`, `status_badges`, `20x20` RGBA8 icon adapter, and unit/rate formatters).
- Implemented the `loom-app` desktop shell (`AppShell`) with progressive startup, `TitleBar` global debounced search, collapsible `Sidebar` navigation, domain-sliced `UiStore`/`UiCommands` bridge, RAII `ScopedMonitorLease` domain gating, and initial Processes & Details slices.
- Added OS system tray residency (`tray-icon`) with waker-backed event delivery, window geometry persistence, and `0% GPU` dormancy by releasing the main window on minimize/close-to-tray and reopening on demand.
- Added on-demand UAC elevation notifications (`Notification`) with single-click elevated relaunch and `--restore-route` route restoration.
- Implemented the inline **Settings** page (`SettingsView`) with live controls for startup page, refresh cadence (`0.5s`/`1s`/`2s`/`Paused`), always-on-top, tray/close-to-tray residency, display preferences, and administrator relaunch.

### Config & Localization

- Added `rust-i18n` localization (`zh-CN` and `en-US`) with live zero-restart language switching across the window title bar, navigation sidebar, system tray menu, and feature views.
- Added live `System` / `Light` / `Dark` theme mode switching synced with `gpui-kit` and persisted atomically via `ConfigStore`.

## 0.0.1 - 2026-11-26

### Engine & PAL

- Established the 10-crate unidirectional Cargo workspace DAG (`loom-types`, `loom-config`, `loom-pal-api`, `loom-pal-windows`, `loom-pal-macos`, `loom-pal-linux`, `loom-pal`, `loom-engine`, `loom-ui-kit`, and `loom-app`) with zero UI or multi-threaded `tokio` dependencies in core/engine crates.
- Implemented static compile-time Platform Abstraction Layer (`loom-pal-api` traits, reusable scratch arenas, structured `PalError`, and `loom-pal` `#[cfg]` `NativeBackend` facade plus `SingleInstanceGuard` and macOS/Linux compile-time stubs).
- Implemented the headless dual-lane `EngineCoordinator` in `loom-engine` with bounded OS thread budget (`1` fast-lane sampling/projection thread + `1..=2` slow-lane bulkhead workers), `+250 ms` startup calibration tick, lock-free domain-sliced `ArcSwap<AppSnapshot>`, RAII `MonitorLease` on-demand domain gating, window dormancy suspension, PID-reuse-safe `ProcessTree` with bottom-up kill ordering, slow-lane `ColdMetaCache` deduplication, and `300 ms` circuit-breaker fault isolation.
- Added headless mock fault-injection tests and real-OS Windows end-to-end integration verification (`crates/loom-app/tests/headless_e2e.rs`).

### Windows Backend

- Implemented zero-allocation steady-state `WindowsFastSampler` using `NtQuerySystemInformation` (`SystemProcessInformation` and `SystemProcessorPerformanceInformation`) with `HIGH_PRIORITY_CLASS` self-boost and non-blocking top-level window discovery (`InternalGetWindowText`, `IsHungAppWindow`, `DwmGetWindowAttribute`) free of WMI, PDH, and cross-process `GetWindowTextW`/`SendMessageW`.
- Implemented `WindowsSlowProber` with RAII handle scoping, `ProcessKey` creation-time identity verification, `ProcessCommandLineInformation` command-line queries (without cross-process PEB `ReadProcessMemory`), SID account caching, local PE `VersionInfo` + `20x20` RGBA8 icon extraction, mandatory UNC/`DRIVE_REMOTE` network-drive bypass, and a deterministic `300 ms`/`60 s` `CircuitBreaker`.
- Implemented core `WindowsSystemOperator` operations (`terminate_process`, bottom-up `terminate_process_tree`, EcoQoS Efficiency Mode, priority classes, multi-group CPU affinity, shell actions, and on-demand UAC elevation helpers).

### Config & Localization

- Implemented `loom-config` (`AppSettings`, `ConfigStore`, and forward schema migration from v0/partial TOML to v1) with `< 1 ms` warm synchronous loads, sanitized bounded `run_task_history`, and crash-safe atomic persistence (`tempfile` in target directory -> `sync_all` -> atomic rename).
- Added core domain preference models in `loom-types` for `zh-CN`/`en-US` language selection, theme mode, refresh cadence, startup page, and view/resource display preferences.
