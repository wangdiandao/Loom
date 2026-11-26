# Changelog

See [versioning and releases](docs/release.md) for the version progression policy, milestone roadmap, and release process.

## Unreleased

### Engine & PAL

- Refined Windows **Processes** `Grouped` view (`PlatformKind::Windows` only) to group multi-process apps strictly by app identity—MSIX/UWP Package Family Name (`ProcessColdMeta::app_group_id` via `GetPackageFamilyName`) or executable path / image name—rather than swallowing different-executable OS children (keeping `WeChatAppEx.exe`, `crashpad_handler.exe`, etc. in `Background`), displaying total group process counts (`(N)`), aggregating CPU/Memory/Disk/Network/GPU metrics, rolling up `EFFICIENCY_MODE`/`NOT_RESPONDING` (and `SUSPENDED` only when all group members are suspended), and keeping `Process Tree` and non-Windows (`Linux` / `macOS`) grouping unchanged.
- Aligned the Windows **Processes** `Grouped` view (`PlatformKind::Windows` only) with Windows Task Manager: stopped nesting independent desktop apps or background daemons under `explorer.exe`, grouped windowless multi-process helper/renderer descendants (and same-image orphaned helpers) under their owning top-level `App` row with aggregated CPU/Memory/Disk/Network/GPU metrics and rolled-up EcoQoS/`Not responding` status flags, and auto-expanded owning `App` groups when searching for child processes—while keeping `Process Tree` and non-Windows (`Linux` / `macOS`) grouping unchanged.

### UI & UX

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
