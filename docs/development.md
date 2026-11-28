# Development guide

English | [中文](development.zh.md)

<a id="contribution-workflow-and-branch-naming"></a>

## Contribution workflow and branch naming

- See [Contributing](../CONTRIBUTING.md) for issue guidelines, performance budget compliance, and PR review rules.
- Always branch from the latest `main` using lowercase kebab-case `<type>/<short-topic>` (for example `feat/pal-windows-ntproc`, `fix/tree-kill-order`, `docs/architecture-update`), and reference the tracking issue in the PR description.
- Allowed prefixes: `feat/`, `fix/`, `docs/`, `test/`, `refactor/`, `ci/`, `chore/`, `release/`.

<a id="workspace-and-module-organization"></a>

## Workspace and module organization

Loom uses a 10-crate unidirectional Cargo Workspace DAG documented in [System architecture](architecture.md#workspace-module-layout-and-dependency-dag):

- **Domain types & config (`crates/loom-{types,config}`)** provide pure POD models and fast (`< 1 ms` target) local configuration persistence without OS or UI dependencies.
- **Static PAL & OS backends (`crates/loom-{pal-api,pal-windows,pal-macos,pal-linux,pal}`)** define monomorphized contracts and reusable scratch arenas in `loom-pal-api`, isolate per-OS FFI into focused submodules inside dedicated backend crates (e.g. `loom-pal-windows/src/{fast_sampler,slow_prober,operator,startup,filelock,perf/{disk,gpu},taskmgr,admin}/`), and re-export `NativeBackend` statically in `loom-pal`.
- **Headless sampling engine (`crates/loom-engine`)** organizes the dual-lane scheduler (`coordinator/`), pre-sorted view projections (`projection/`), bulkhead probe pool (`slow_lane/`), anti-PID-reuse process tree, structured query engine, and RAII `MonitorLease` ref-counting—**without GPUI or multi-thread Tokio dependencies**.
- **Composable UI kit & desktop app (`crates/loom-{ui-kit,app}`)** organize `gpui-kit` composites into focused modules (`theme/`, `format/`, `heatmap`, `sparkline`, `logic`) in `loom-ui-kit` and follow a **Bootstrap + Shell + Feature Slices** layout (`bootstrap/`, `shell/{window,dialogs}/`, and `features/{table_columns,processes,performance,app_history,startup,users,details,services,settings}/`) in `loom-app`.

<a id="coding-conventions"></a>

## Coding conventions

- **Update docs alongside contract changes**: Locate active specifications from [Documentation index](index.md). Update matching English (`*.md`) and Chinese (`*.zh.md`) docs in the same PR whenever architecture, PAL contracts, performance budgets, or user-visible behavior change.
- **Preserve static dispatch and headless boundaries**:
  - Keep `loom-engine` and lower crates headless-testable without multi-thread async runtimes.
  - Use static monomorphization (`EngineCoordinator<B: PlatformBackend>`) and caller-owned `SampleScratch` buffers; avoid `Box<dyn Trait>` and per-tick `String`/`Vec` heap allocations on hot paths.
- **Follow the six Windows anti-hang constraints**:
  - `loom-pal-windows` does not depend on WMI or PDH, does not call `GetWindowTextW` or blocking cross-process `SendMessageW` (use `InternalGetWindowText` and `IsHungAppWindow`), and does not read remote PEBs via `ReadProcessMemory` (use `ProcessCommandLineInformation`).
  - Isolate icon extraction, PE version/signature reads (passing `WTD_CACHE_ONLY_URL_RETRIEVAL`), and disk `IOCTL` queries inside the Slow Probe Bulkhead guarded by `300 ms` `CircuitBreaker` timeouts, bypassing deep opens on network shares.
- **Follow UI slice and component-composition rules**:
  - Each feature module owns its private GPUI `Entity`, `TableState`, `Input` states, scroll handles, and feature-scoped dialogs. Do not add cross-view mutable fields to the root window or introduce a global modal enum.
  - Build tables, area/line charts, sparklines, trees, and menus by composing `gpui-kit` (`gpui-component`) widgets and Flexbox primitives—do not introduce custom `Canvas` or raw GPU shader drawing.
  - Bind table selections and context-menu targets to `ProcessKey (pid, create_time_100ns)`, and gate on-demand views with RAII `MonitorLease`.
- **Atomic writes & idiomatic Rust**: Write `settings.toml` via same-directory `tempfile -> sync_all -> atomic rename`; keep module visibility narrow (`pub(super)` / `pub(crate)`), and model library errors explicitly with `thiserror` (surfacing `PalError::ElevationRequired` clearly).

<a id="testing-conventions"></a>

## Testing conventions

Select the lowest effective test tier for the behavior being verified:

| Tier | Mechanism & Location | Coverage |
| --- | --- | --- |
| Pure unit tests | Standard `#[test]` inside `crates/` | `loom-config` migrations, quantized `HistoryRing`, anti-PID-reuse process tree construction, raw SMBIOS byte parsing, heatmap bucketing, and unit formatters. |
| Headless engine & fault-injection tests | `crates/loom-engine/tests/` | End-to-end `MockBackend` tests covering `+250 ms` bootstrap delta ticks, `MonitorLease` ref-counting, bottom-up tree kill ordering, and simulated 5s stalled-disk probes verifying `< 2 ms` non-blocking Fast Lane execution. |
| Headless UI tests | `#[gpui::test]` using `gpui-kit` test harness | Feature slice mounting, `UiStore` selective `Revision` updates, `ProcessKey` selection stability across resorts, cross-view jumps, and live bilingual switching. |
| Desktop real-window & smoke verification | `agent-desktop` automation + host checks | Verify real window layout, accessibility tree snapshots, search filtering, and context menus via `agent-desktop`; measure `< 50 ms` cold start, 100%-CPU preemption wakeup, `0% GPU` minimized hibernation, and on-demand UAC elevation. |

Testing guidelines:

- Prefer plain `#[test]` whenever GPUI `App` / `Window` is not required.
- Hold `i18n::locale_test_lock()` in any `loom-app` test that mutates global `rust_i18n` locale or asserts locale-dependent output so parallel `cargo test` runs remain deterministic.
- Add deterministic regression tests for anti-hang invariants (`CircuitBreaker` trips, PID reuse edge cases).
- Clean up temporary dump files, logs, and screenshots immediately after verification.

<a id="upstream-contributions"></a>

## Upstream contributions

- When encountering defects in `gpui-kit` (`gpui-component` / `gpui-base`) or Zed's GPUI, isolate a minimal reproduction against the pinned upstream version first.
- Follow the upstream repository's contribution and licensing rules, keeping patches minimal and focused.
- Link upstream issues or PRs in a local Loom tracking issue and record the removal condition for any temporary workaround.

<a id="development-environment"></a>

## Development environment

Install [Rustup](https://rustup.rs/) matching the workspace toolchain pinned in `rust-toolchain.toml` (once initialized in `0.0.1`).

| Platform | Prerequisites |
| --- | --- |
| Windows 11 x64 / ARM64 (primary target) | Visual Studio Build Tools ("Desktop development with C++" workload: MSVC + Windows 11 SDK). |
| Linux (Ubuntu 24.04 x64, `0.2.0` target) | Standard Rust/C toolchain, Wayland/X11 and Vulkan headers required by GPUI, and `dbus` development headers. |
| macOS (Apple Silicon / x64, `0.3.0` target) | Xcode Command Line Tools. |

When working across parallel Git worktrees, point `CARGO_TARGET_DIR` to the main checkout's `target` directory to reuse incremental build artifacts.

<a id="local-checks"></a>

## Local checks

Before opening or updating a PR, run the checks matching your change scope (active once the Rust workspace is initialized in `0.0.1`):

```powershell
git diff --check origin/main
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
$env:RUSTDOCFLAGS = '-D warnings'; cargo doc --locked --workspace --no-deps --all-features
cargo test --locked --workspace --all-features
```

Targeted headless checks during backend and engine iteration:

```powershell
cargo test --locked -p loom-types -p loom-config -p loom-pal-windows -p loom-engine
```
