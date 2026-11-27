# Versioning and releases

English | [中文](release.zh.md)

<a id="version-policy"></a>

## Version progression policy

All workspace crates (`loom-types`, `loom-config`, `loom-pal-api`, `loom-pal-windows`, `loom-pal-macos`, `loom-pal-linux`, `loom-pal`, `loom-engine`, `loom-ui-kit`, and `loom-app`) share a single version in the root `[workspace.package]`.

Loom tracks development order directly by semantic versions rather than abstract phases, following four stages:

1. **`0.0.x` — Pre-`0.1.0` milestones (`0.0.1`–`0.0.6`, focused on Windows)**
   - `0.0.1`–`0.0.2` establish the headless dual-lane engine, static PAL contracts, and `gpui-kit` desktop shell; **`0.0.3`** reaches daily-usable **P0 core** Windows process monitoring and control; `0.0.4`–`0.0.6` add the WMI/PDH-free hardware Performance views, Services/Startup/Users/App History views, and WCT/MiniDump diagnostics plus benchmark tuning.
   - Each completed milestone increments `patch` (`0.0.1` → `0.0.2` → `0.0.3` → ... → `0.0.6`).
   - Internal PAL traits and snapshot slice schemas may evolve freely; no formal end-user installers are distributed yet.
2. **`0.1.0` — First stable Windows release**
   - Locks `loom-config` v1 forward migration guarantees (`CONFIG_CONTRACT_VERSION = "1.0"`, `CURRENT_SCHEMA_VERSION = 1`), freezes the `loom-pal-api` v1 cross-platform static-dispatch contract (`PAL_API_CONTRACT_VERSION = 1`), verifies the engineering budgets (`< 50 ms` cold start, `< 0.15%` steady-state CPU, slow-I/O circuit-breaker isolation), and enables automated Windows WiX MSI + Portable ZIP packaging with conditional Authenticode signing via GitHub Releases.
3. **`0.y.z` (`0.1.0 <= v < 1.0.0`) — Public pre-1.0 iteration & multi-OS rollout**
   - Increment `minor` (`y`, resetting `z` to `0`) for new OS backend milestones (`0.2.0` `loom-pal-linux`, `0.3.0` `loom-pal-macos`), major feature additions, or breaking changes to configuration schemas or `loom-pal-api`.
   - Increment `patch` (`z`) for backward-compatible bug fixes, hardware sensor compatibility adjustments, security/dependency patch updates, and UX refinements.
   - Apply the highest applicable bump when combining multiple change types, and document breaking changes and migration paths in [CHANGELOG.md](../CHANGELOG.md).
4. **`1.0.0+` — Cross-platform stable releases ([SemVer 2.0.0](https://semver.org/))**
   - Increment `major` for incompatible configuration schema or public contract changes.
   - Increment `minor` for backward-compatible features and public deprecations.
   - Increment `patch` for backward-compatible bug and security fixes.

<a id="versioned-roadmap"></a>

## Version-driven development roadmap

| Target Version | Milestone Title | Deliverable Scope | Exit Criteria |
| --- | --- | --- | --- |
| **`0.0.1`** | Workspace skeleton, static PAL & headless engine | Initialize 10-crate Cargo Workspace DAG; implement `loom-types`, `loom-config` (`< 1 ms` load & atomic writes), `loom-pal-api`, `loom-pal` (including macOS/Linux compile-time stubs), `loom-pal-windows` Fast Lane (`HIGH_PRIORITY_CLASS` boost, `NtQuerySystemInformation` scratch-reused sampler, `+250 ms` bootstrap delta, Slow Bulkhead `300 ms` `CircuitBreaker`), and `loom-engine` (Tokio-free coordinator, anti-PID-reuse process tree, sliced `ArcSwap<AppSnapshot>`, `MonitorLease`). | Headless integration tests pass `< 2 ms` process sweeps, PID-reuse cycle prevention, non-blocking Fast Lane under simulated stalled-disk probes, and bounded OS thread counts. |
| **`0.0.2`** | `gpui-kit` desktop shell, localization & Settings | Implement `loom-ui-kit` (heatmap cells, sparkline `AreaChart` cards, segmented memory bar, theme tokens) and `loom-app` shell (progressive boot, `TitleBar` debounced global search, collapsible `Sidebar`, `UiStore` selective projections, tray residency & `0% GPU` minimize/close hibernation, on-demand UAC elevation helper, `rust-i18n` `zh-CN`/`en-US` switching, and **Settings** view). | Desktop window reaches interactive Frame 1 within `< 50 ms`, switches languages and themes live, applies update speeds (`0.5s`/`1s`/`2s`/`Paused`) and always-on-top immediately, and releases GPU usage (`0%`) when minimized. |
| **`0.0.3`** | **First daily-usable milestone (P0 core task management)** | Deliver **Processes** (Apps/Background/Windows grouping + parent-child Process Tree toggle, status badges, Values/Percents switch, heatmap columns, async `20×20` icon & PE publisher backfill) and **Details** (full-column virtualized table/tree), plus **Run new task** (Browse & Admin checkbox), **End task**, bottom-up **End process tree**, **Efficiency mode (`EcoQoS`)**, priority, CPU affinity, file location/properties/online search, and cross-view jumps. | Handles daily Windows 11 process monitoring and high-load termination without hanging on frozen windows or disconnected network shares. |
| **`0.0.4`** | WMI/PDH-free Performance views (P1 part 1) | Deliver **Performance** left sparkline rail and five hardware detail views: **CPU** (overall / logical-core grid, `Show kernel times` dual-series `AreaChart`, live turbo speed & L1–L3 topology), **Memory** (60s chart, 4-segment bar, raw `SMBIOS` speed & slots), **Disk 0..N** (drive letters, NVMe/SATA detection, active time + R/W transfer charts), **Ethernet / Wi-Fi** (auto-scaling throughput + IPv4/IPv6), and **GPU 0..N** (`D3DKMT` `2×2` multi-engine & single-engine views, dedicated/shared VRAM, die temperature & PCI bus). | All charts render via `gpui-kit` `AreaChart`/`LineChart` without custom drawing, staying within `< 0.15%` steady-state CPU at 1s cadence. |
| **`0.0.5`** | System management views (P1 part 2) | Deliver **Services** (SCM enumeration, PID & group, start/stop/restart, bidirectional `Details` jump), **Startup Apps** (`Last BIOS time`, Registry/Folder/UWP entries, enable/disable), **Users** (WTS session aggregation, expandable user process trees, disconnect), and **App History** (cumulative CPU/network usage & clear history). | Covers all 8 views in [Product design and feature audit](design.md) for P0 + P1 with no background polling on inactive lease-gated tabs. |
| **`0.0.6`** | Kernel diagnostics & benchmark tuning (P2) | Deliver **Analyze wait chain (`WCT`)** (`Tree` deadlock/blocker inspector with direct target kill), **Create memory dump (`MiniDump`)**, **UAC virtualization** toggle, debugger attach, global wakeup hotkey, and fault-injection benchmark tuning under 100% CPU load, hung windows, and stalled external drives. | Meets engineering budgets (`< 50 ms` cold start, `< 0.15%` CPU, `30–45 MB` private working set, `<= 8` threads). |
| **`0.1.0`** | **First stable Windows release** | Freeze `loom-config` v1 (`CONFIG_CONTRACT_VERSION = "1.0"`) forward migration guarantees and `loom-pal-api` v1 (`PAL_API_CONTRACT_VERSION = 1`) cross-platform static contracts; ship Windows WiX v4/v5 MSI installers (`packaging/windows/wix/main.wxs`), portable ZIP archives (`packaging/windows/package-release.ps1`), SHA-256 manifests, and conditional Authenticode signing via `.github/workflows/release.yml`. | Stable Windows release with contract stability guarantees and automated MSI + Portable ZIP GitHub Release artifacts. |
| **`0.2.0`** | Linux platform backend (`loom-pal-linux`) | Implement `loom-pal-linux` (`procfs`, `sysfs`, `netlink`, `drm` GPU, `systemd` D-Bus units, XDG autostart) without architectural changes to `loom-engine` or `loom-app`. | Daily task management and hardware telemetry parity on Linux (Wayland / X11). |
| **`0.3.0`** | macOS platform backend (`loom-pal-macos`) | Implement `loom-pal-macos` (`Mach` host/task APIs, `libproc`, `IOKit` storage/GPU/thermal sensors, `launchd`, `SMAppService`). | Cross-platform coverage across Windows, Linux, and macOS. |

<a id="changelog"></a>

## Changelog

Record user-visible changes under `Unreleased` in [CHANGELOG.md](../CHANGELOG.md) as PRs merge—new features, observable fixes, anti-hang or resource improvements, platform requirements, and configuration/PAL migrations. Exclude pure documentation, CI-only, or internal-only refactorings without user-visible impact. Group entries by area (`Engine & PAL`, `Windows Backend`, `Performance & Diagnostics`, `UI & UX`, `Config & Localization`, `Breaking & Migration`).

When cutting a release:

1. Reconcile `Unreleased` against the commit range since the previous tag.
2. Move entries under `## X.Y.Z - YYYY-MM-DD`, preserving an empty `## Unreleased` section at the top.
3. Include contributor credits and a `Full Changelog` GitHub compare link for `0.1.0` and later releases.

<a id="prepare-and-publish"></a>

## Prepare and publish

1. **Release PR**: Bump `[workspace.package].version` (and internal crate version pins), update `Cargo.lock`, finalize `CHANGELOG.md`, and verify version references in documentation.
2. **Automated checks & smoke verification**: Ensure all CI checks pass on `main` (headless `loom-engine` & fault-injection tests, `#[gpui::test]` UI tests, Clippy, Rustdoc) and run host smoke verification (`< 50 ms` cold start, tray residency & VRAM release on window close, process tree termination, live hardware charts, and on-demand UAC elevation).
3. **Windows packaging & conditional signing**:
   - Installer definition: `packaging/windows/wix/main.wxs` (WiX v4/v5 XML schema installing `loom-app.exe`, `LICENSE`, and Start Menu shortcut `Loom Task Manager`).
   - Packaging script: `packaging/windows/package-release.ps1` emits `loom-<Version>-windows-<Arch>-portable.zip`, `loom-<Version>-windows-<Arch>.msi` (when `-SkipMsi` is omitted and `wix` is installed), and `SHA256SUMS.txt`.
   - Conditional Authenticode signing: configure GitHub Actions secrets `WINDOWS_SIGN_PFX_BASE64` and `WINDOWS_SIGN_PFX_PASSWORD` (passed to the script as `LOOM_SIGN_PFX_BASE64` and `LOOM_SIGN_PFX_PASSWORD`, or locally via `LOOM_SIGN_CERT_PATH`). When unset, signing is skipped cleanly without failing the workflow; temporary decoded `.pfx` files are overwritten and deleted in a `finally` block.
4. **Tag and GitHub Release**: Create an annotated `vX.Y.Z` tag on `main` and push it to trigger `.github/workflows/release.yml`, which runs verification on Windows and Linux headless, builds and packages the Windows release artifacts, extracts the matching version section from `CHANGELOG.md`, and publishes the GitHub Release attaching `.msi`, `.zip`, and `SHA256SUMS.txt`. Use `workflow_dispatch` with a `version` input to run dry-run packaging tests without publishing a GitHub Release.
