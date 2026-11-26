# Documentation index

English | [中文](index.zh.md)

<a id="product-and-architecture"></a>

## Product and architecture

- [Product design and feature audit](design.md) records the feature audit of Windows 11 Task Manager across all 8 views, context menus, hardware charts, and dialogs, triage decisions (`P0` / `P1` / `P2` / `Drop`), and native `gpui-kit` UI specifications.
- [System architecture](architecture.md) defines performance and resource budgets, the progressive cold-start and anti-hang bulkhead architecture, Cargo Workspace DAG boundaries, static PAL contracts, Windows telemetry subsystems, feature-sliced UI modularization, and ecosystem reuse.

<a id="development-and-releases"></a>

## Development and releases

- [Contributing](../CONTRIBUTING.md) describes issues, performance and architecture compliance, PR review, and upstream collaboration.
- [Development guide](development.md) describes branch conventions, crate boundaries, Windows anti-hang coding rules, testing tiers, build prerequisites, and local verification checks.
- [Versioning and releases](release.md) defines version progression rules (`0.0.x` → `0.1.0` → `0.y.z` → `1.0.0`), the version-driven development roadmap, changelog rules, and the release workflow.
