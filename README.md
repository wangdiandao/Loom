# Loom

English | [中文](README.zh.md)

Loom is a native desktop Task Manager for Windows, macOS, and Linux built with **Rust** and **GPUI Kit (`gpui-kit` / `gpui-component`)**. Development targets **Windows** first—matching the core capabilities of the built-in Windows 11 Task Manager while addressing slow cold starts, high-load UI stutters, and startup hangs—with a static Platform Abstraction Layer (PAL) prepared for Linux and macOS backends.

> **Status:** **`0.1.0`** — First stable Windows release with frozen `loom-config` v1 (`CONFIG_CONTRACT_VERSION = "1.0"`) and `loom-pal-api` v1 (`PAL_API_CONTRACT_VERSION = 1`) contracts, WiX MSI + Portable ZIP packaging, and automated GitHub Releases. Next milestones: Linux (`0.2.0`) and macOS (`0.3.0`) backends. See [Versioning and releases](docs/release.md) for the full roadmap.

## Design goals

- **Fast startup and hang isolation.** Raises process priority (`HIGH_PRIORITY_CLASS`) on entry to stay responsive under 100% CPU load, targets **`< 50 ms`** time to an interactive process table, and isolates slow disk, shell, and driver probes behind a bounded bulkhead with `300 ms` circuit breakers—without WMI, PDH, or cross-process blocking `GetWindowTextW` calls.
- **Low system footprint.** Avoids a multi-thread Tokio runtime (targeting `<= 8` OS threads even on 32-thread CPUs), reuses sampling buffers to avoid steady-state per-tick heap allocations (target `< 0.15%` CPU at a 1s interval), downsamples icons to `20×20`, redraws on demand at the refresh cadence, and releases GPU resources (`0%` GPU) when minimized or closed to the tray.
- **Modular cross-platform architecture.** Uses a unidirectional Cargo Workspace DAG, a headless core engine (`loom-engine`), separate per-OS backend crates (`loom-pal-windows`, plus reserved `loom-pal-macos` and `loom-pal-linux`), and compile-time monomorphized PAL traits without `dyn Trait` vtables on hot paths.
- **Component-based GPUI interface.** Organizes the 8 views (`Processes`, `Performance`, `App History`, `Startup Apps`, `Users`, `Details`, `Services`, `Settings`) as feature slices composed from `gpui-kit` (`gpui-component`) virtualized tables, `AreaChart`/`LineChart` plots, trees, context menus, and dialogs, avoiding custom `Canvas` or shader drawing.
- **Standard-user launch with on-demand elevation.** Launches under standard user privileges without a startup UAC prompt, elevating only when acting on protected system processes, services, or administrative tasks.

## Roadmap overview

Development follows semantic version milestones:

| Target Version | Scope |
| --- | --- |
| `0.0.1` | Workspace DAG skeleton, static PAL contracts (`loom-types`, `loom-pal-api`, `loom-pal`), Windows NT kernel sampler with slow-probe circuit breaker (`loom-pal-windows`), and headless `loom-engine`. |
| `0.0.2` | Composable `loom-ui-kit` widgets, progressive-boot `loom-app` shell (`TitleBar`, collapsible `Sidebar`, `UiStore`, tray residency, on-demand UAC helper), `rust-i18n` localization (`zh-CN` / `en-US`), and `Settings`. |
| **`0.0.3`** | **First daily-usable milestone (P0 core):** `Processes` (categorized groups and parent-child process tree) and `Details` views, `Run new task`, `End task` / `End process tree`, Efficiency Mode (`EcoQoS`), priority/affinity controls, and cross-view navigation. |
| `0.0.4` | `Performance` views without WMI/PDH: CPU (overall, logical core grid, kernel-time overlay), Memory (composition bar and SMBIOS metadata), Disk (NVMe/SATA detection, active time and transfer charts), Network, and GPU (`D3DKMT` multi-engine and VRAM/temperature). |
| `0.0.5` | System management views: `Services` (SCM start/stop/restart), `Startup Apps` (Registry/Folder/UWP and Last BIOS time), `Users` (WTS session trees), and `App History`. |
| `0.0.6` | Diagnostics and performance tuning: Wait Chain Traversal (`WCT` deadlock tree), `MiniDump` creation, UAC virtualization toggle, debugger attach, global hotkey, and fault-injection benchmarking. |
| **`0.1.0`** | **First stable Windows release:** Freeze `loom-config` v1 (`CONFIG_CONTRACT_VERSION = "1.0"`) migration guarantees and `loom-pal-api` v1 (`PAL_API_CONTRACT_VERSION = 1`) cross-platform static contracts, with Windows WiX MSI + Portable ZIP packaging, conditional Authenticode signing, and automated GitHub Releases. |
| `0.2.0` / `0.3.0` | Cross-platform backend rollout: `loom-pal-linux` (`0.2.0`) and `loom-pal-macos` (`0.3.0`) without changing the engine or UI architecture. |

## Project documentation

- [Documentation index](docs/index.md)
- [System architecture](docs/architecture.md)
- [Product design and feature audit](docs/design.md)
- [Development guide](docs/development.md)
- [Versioning and releases](docs/release.md)
- [Contributing](CONTRIBUTING.md)
- [Changelog](CHANGELOG.md)

## License

Loom is licensed under the **Apache License 2.0** (`Apache-2.0`). See [LICENSE](LICENSE).
