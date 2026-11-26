# Loom 贡献指南

[English](CONTRIBUTING.md) | 中文

## Issue 与 PR

- 提交前先搜索现有 Issue 和 PR。缺陷报告须包含最小复现步骤、操作系统版本与内部版本号、CPU/GPU 硬件概况、运行权限状态（普通用户或管理员）以及预期与实际行为。功能提议须说明具体的监控或排障场景，并对照[产品设计与功能盘点](docs/design.zh.md)与性能预算说明契合度。
- 从最新 `main` 创建分支，使用 `<类型>/<简短主题>` 命名（如 `feat/pal-windows-d3dkmt`、`fix/process-tree-pid-reuse`、`docs/release-policy`）。每个 PR 只聚焦一个问题并关联对应 Issue。详见[分支命名约定](docs/development.zh.md#contribution-workflow-and-branch-naming)。

## 范围、性能预算与架构合规

- 功能新增、后端采集器调整与界面修改须遵守[产品设计与功能盘点](docs/design.zh.md)及[系统架构](docs/architecture.zh.md)：
  - 禁止在 `loom-engine`、`loom-pal*`、`loom-config` 或 `loom-types` 中引入 `gpui`、`gpui-kit` 或多线程 `tokio` 依赖。
  - 保持跨平台 Crate 间的编译期静态分派（`loom-pal-api` → `loom-pal-{windows,macos,linux}` → `loom-pal`），避免在高频采样热路径引入虚表（`dyn Trait`）或逐周期堆内存分配。
  - 遵守 `loom-pal-windows` 的防卡死约束：不使用 WMI 与 PDH，不跨进程调用 `GetWindowTextW` 或阻塞式 `SendMessageW`，不使用 `ReadProcessMemory` 跨进程读 PEB；慢速磁盘、图标与签名探测隔离在慢通道舱壁池并附带超时熔断保护。
  - `loom-app` 保持独立垂直功能切片（`shell` + `features/*`），通过 `gpui-kit`（`gpui-component`）现成控件组合构建，避免自定义 `Canvas` 或着色器、根视图全局大状态以及全局弹窗大枚举。
- 若希望重新引入[全量功能盘点](docs/design.zh.md#part-1-complete-feature-audit-and-triage)中标记为 `Drop` 的项目，须先提交 Issue 说明其必要性，并评估对冷启动目标（`< 50 ms`）与稳态资源预算的影响。

## PR 评审与合并

- 遵守[编码规范](docs/development.zh.md#coding-conventions)与[测试规范](docs/development.zh.md#testing-conventions)。贡献者须对所提交代码负责，包括对 AI 辅助编写的代码进行审查与验证。
- 若改动涉及公开平台抽象契约（`loom-pal-api`）或配置 Schema（`loom-config`），须在 PR 说明中列出签名变更与兼容性影响，并在[变更日志](CHANGELOG.md)记录迁移步骤。
- 在 PR 说明中记录可复现的验证命令、测试平台、权限模式与启动/资源测量结果；涉及界面视觉改动时附截图或录屏。
- 面向用户的变更在合并前写入 [CHANGELOG.md](CHANGELOG.md) 的 `Unreleased` 下；纯文档改动不写入变更日志，详见[变更日志规则](docs/release.zh.md#changelog)。
- PR 经维护者批准、所有评审意见解决且最新提交通过全部必选 CI 检查后合并。

## 向上游提交 Issue 与贡献

超出 Loom 职责范围的问题，应直接向对应上游项目提交 Issue 或修复：

- 桌面组件库（`Table`、`AreaChart`、`LineChart`、`Sidebar`、`Dialog`、`PopupMenu`、`Tree`）与聚合发包归属 [`gpui-kit`](https://github.com/longbridge/gpui-kit)。
- 通用控件行为、焦点与输入法基础能力归属 GPUI Kit 的 `gpui-base`。
- 窗口管理、DirectX 12 / Metal / Vulkan GPU 渲染与平台底层事件归属 Zed 的 GPUI。

上游问题影响 Loom 时，在本地跟踪 Issue 中关联上游链接，并记录临时适配方案及移除条件。详见[上游贡献](docs/development.zh.md#upstream-contributions)。
