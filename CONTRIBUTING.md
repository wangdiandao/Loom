# Contributing to Loom

English | [中文](CONTRIBUTING.zh.md)

## Issues and pull requests

- Search existing issues and PRs before opening a new one. Bug reports should include a minimal reproduction, operating system and build number, CPU/GPU hardware summary, privilege state (standard user vs. administrator), and expected vs. observed behavior. Feature proposals should describe the concrete monitoring or troubleshooting workflow and explain how it fits within Loom's scope and resource budgets in [Product design and feature audit](docs/design.md).
- Branch from the latest `main` using `<type>/<short-topic>` (for example `feat/pal-windows-d3dkmt`, `fix/process-tree-pid-reuse`, `docs/release-policy`). Keep each PR focused on a single problem and link the corresponding issue. See [branch naming conventions](docs/development.md#contribution-workflow-and-branch-naming).

## Scope, performance, and architecture compliance

- Feature additions, backend samplers, and UI changes must follow [Product design and feature audit](docs/design.md) and [System architecture](docs/architecture.md):
  - Do not add `gpui`, `gpui-kit`, or multi-thread `tokio` dependencies to `loom-engine`, `loom-pal*`, `loom-config`, or `loom-types`.
  - Keep compile-time static dispatch across platform crates (`loom-pal-api` → `loom-pal-{windows,macos,linux}` → `loom-pal`); avoid `dyn Trait` vtables or per-tick heap allocations on hot sampling paths.
  - Follow the Windows anti-hang constraints in `loom-pal-windows`: no WMI, no PDH, no cross-process `GetWindowTextW` or blocking `SendMessageW`, no `ReadProcessMemory` PEB scraping, and slow-I/O bulkhead isolation with circuit-breaker timeouts.
  - Keep `loom-app` organized as independent vertical feature slices (`shell` + `features/*`) composed from `gpui-kit` (`gpui-component`) controls, without custom `Canvas` or raw shader drawing, root-level global state structs, or monolithic modal enums.
- Proposals to re-introduce items marked as `Drop` in [the feature audit](docs/design.md#part-1-complete-feature-audit-and-triage) require an issue explaining why they belong in Loom without regressing startup latency (`< 50 ms` target) or steady-state CPU/RAM budgets.

## PR review and merge

- Follow the [coding conventions](docs/development.md#coding-conventions) and [testing conventions](docs/development.md#testing-conventions). Contributors are responsible for reviewing and verifying all submitted code, including AI-assisted work.
- When a change updates public PAL traits (`loom-pal-api`) or configuration schemas (`loom-config`), document the signature changes, compatibility impact, and migration path in the PR description and [CHANGELOG.md](CHANGELOG.md).
- Record reproducible verification commands, target platform, privilege mode, and benchmark/resource measurements in the PR description. Include screenshots or recordings for visual UI changes.
- Record user-visible changes under `Unreleased` in [CHANGELOG.md](CHANGELOG.md). Pure documentation changes are excluded; see [changelog rules](docs/release.md#changelog).
- PRs merge after maintainer approval, resolution of review comments, and green required CI checks on the latest commit.

## Upstream issues and contributions

Report issues directly to the upstream project when behavior belongs outside Loom:

- Styled UI components (`Table`, `AreaChart`, `LineChart`, `Sidebar`, `Dialog`, `PopupMenu`, `Tree`) and umbrella packaging belong in [`gpui-kit`](https://github.com/longbridge/gpui-kit).
- Reusable control behavior, focus, and IME handling belong in GPUI Kit's `gpui-base`.
- Windowing, DirectX 12 / Metal / Vulkan GPU rendering, and platform events belong in Zed's GPUI.

When an upstream bug impacts Loom, link the upstream issue or PR from a local tracking issue and document any temporary workaround along with its removal condition. See [upstream contributions](docs/development.md#upstream-contributions).
