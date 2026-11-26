# Changelog

See [versioning and releases](docs/release.md) for the version progression policy, milestone roadmap, and release process.

## Unreleased

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
