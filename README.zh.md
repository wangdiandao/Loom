# Loom

[English](README.md) | 中文

Loom 是一款基于 **Rust** 与 **GPUI Kit (`gpui-kit` / `gpui-component`)** 构建的跨平台（Windows / macOS / Linux）原生桌面任务管理器。当前阶段聚焦 **Windows** 平台实现，对齐 Windows 11 自带任务管理器的核心功能，重点解决冷启动慢、高负载卡顿以及异常设备或未响应窗口导致的启动阻塞问题，并通过静态平台抽象层（PAL）为后续扩展 Linux 与 macOS 后端提供接口边界。

> **当前状态**：**`0.1.0`** —— 首个 Windows 稳定正式版，已冻结 `loom-config` v1（`CONFIG_CONTRACT_VERSION = "1.0"`）与 `loom-pal-api` v1（`PAL_API_CONTRACT_VERSION = 1`）跨平台契约，并提供 Windows WiX MSI 与便携 ZIP 自动化发布。下一阶段将依次扩展 Linux（`0.2.0`）与 macOS（`0.3.0`）后端。完整版本路线见[版本与发布](docs/release.zh.md)。

## 设计目标

- **快速冷启动与防阻塞隔离**：启动入口将自身优先级提升为 `HIGH_PRIORITY_CLASS`，避免在 100% CPU 负载下被饿死；首帧可交互目标为 **`< 50 ms`**。采用快慢双通道舱壁隔离与 `300 ms` 超时熔断，不依赖 WMI 与 PDH，不跨进程调用同步窗口消息（如 `GetWindowTextW`），在存在未响应窗口、休眠机械盘、异常外置盘或断连网络盘时保持界面响应。
- **低系统资源占用**：不引入多线程 Tokio 运行时（32 逻辑核环境下自身线程目标控制在 `<= 8` 个），热路径复用缓冲区避免稳态逐周期堆分配（1s 刷新周期下目标 `< 0.15%` CPU），配合 `20×20` 降采样图标缓存与按刷新周期驱动的按需重绘，最小化或关窗常驻托盘时释放渲染资源（`0%` GPU 占用）。
- **模块化与零开销跨平台抽象**：通过 Cargo Workspace 单向依赖 DAG 分层隔离，核心引擎（`loom-engine`）无头运行，各平台后端独立成 Crate（`loom-pal-windows` 及预留的 `loom-pal-macos`、`loom-pal-linux`），通过泛型单态化与关联类型消除热路径虚表开销。
- **组件组合式 GPUI 界面**：8 个功能视图（进程、性能、应用历史记录、启动应用、用户、详细信息、服务、设置）按垂直功能切片组织，直接组合 `gpui-kit`（`gpui-component`）内置虚拟化表格、多序列 `AreaChart`/`LineChart`、树控件、右键菜单与对话框，避免手写自定义 `Canvas` 或着色器。
- **普通权限启动与按需提权**：默认以普通用户权限直接启动，避免冷启动弹出 UAC 确认框；仅在操作高完整性系统进程、启停系统服务或勾选以管理员身份运行新任务时按需提权。

## 版本路线概览

开发顺序按语义化版本号推进：

| 目标版本 | 交付范围 |
| --- | --- |
| `0.0.1` | Workspace 单向依赖骨架、静态 PAL 契约（`loom-types`、`loom-pal-api`、`loom-pal`）、Windows 内核态快采与舱壁熔断底座（`loom-pal-windows`）以及无头引擎 `loom-engine`。 |
| `0.0.2` | 组合式 UI 底座（`loom-ui-kit`）、渐进启动主窗口壳层（`TitleBar`、可折叠 `Sidebar`、`UiStore`、托盘常驻、按需提权辅助）、`rust-i18n` 中英双语（`zh-CN` / `en-US`）与「设置」页。 |
| **`0.0.3`** | **首个日常可用里程碑（P0 核心）**：「进程」（三段分类分组与防 PID 复用进程树切换）与「详细信息」页，运行新任务、结束任务/进程树、效能模式（`EcoQoS`）、优先级与相关性设置及跨页定位。 |
| `0.0.4` | 无 WMI/PDH 的「性能」监控视图：CPU（总体 / 多逻辑核网格 / 内核时间叠加）、内存（四段比例条与 SMBIOS 信息）、磁盘（NVMe/SATA 识别与活动/速率图）、网络与 GPU（`D3DKMT` 多引擎与显存温度）。 |
| `0.0.5` | 系统管理视图：「服务」（SCM 启停管理）、「启动应用」（含上次 BIOS 时间）、「用户」（WTS 会话树与断开连接）与「应用历史记录」。 |
| `0.0.6` | 内核诊断与性能调优：分析等待链（`WCT` 阻塞树）、创建内存转储（`MiniDump`）、UAC 虚拟化切换、调试器附加、全局唤醒热键及故障注入压测调优。 |
| **`0.1.0`** | **首个稳定契约与 Windows 正式版**：冻结 `loom-config` v1（`CONFIG_CONTRACT_VERSION = "1.0"`）迁移承诺与 `loom-pal-api` v1（`PAL_API_CONTRACT_VERSION = 1`）跨平台静态契约，提供 Windows WiX MSI + 便携 ZIP 打包、条件 Authenticode 签名与自动化 GitHub Releases 发布。 |
| `0.2.0` / `0.3.0` | 跨平台后端扩展：在不改动引擎与 UI 架构的前提下依次实现 `loom-pal-linux`（`0.2.0`）与 `loom-pal-macos`（`0.3.0`）。 |

## 项目文档

- [文档索引](docs/index.zh.md)
- [系统架构](docs/architecture.zh.md)
- [产品设计与功能盘点](docs/design.zh.md)
- [开发指南](docs/development.zh.md)
- [版本与发布](docs/release.zh.md)
- [贡献指南](CONTRIBUTING.zh.md)
- [变更日志](CHANGELOG.md)

## 许可协议

Loom 采用 **Apache-2.0** 开源许可协议，详见 [LICENSE](LICENSE)。
