# 开发指南

[English](development.md) | 中文

<a id="contribution-workflow-and-branch-naming"></a>

## 贡献流程与分支命名

- Issue 提议、性能预算与评审要求见[贡献指南](../CONTRIBUTING.zh.md)。
- 从最新 `main` 创建分支，使用小写英文与连字符 `<类型>/<简短主题>`（如 `feat/pal-windows-ntproc`、`fix/tree-kill-order`、`docs/architecture-update`），并在 PR 说明中关联对应 Issue。
- 前缀按改动目的选用：`feat/`、`fix/`、`docs/`、`test/`、`refactor/`、`ci/`、`chore/`、`release/`。

<a id="workspace-and-module-organization"></a>

## Workspace 与模块组织

Loom 通过 10-Crate Workspace 约束编译期单向依赖 DAG，完整拓扑见[系统架构](architecture.zh.md#workspace-module-layout-and-dependency-dag)：

- **领域模型与配置底座（`crates/loom-{types,config}`）**：纯数据结构与快速（目标 `< 1 ms`）本地配置持久化，无 OS API 与 UI 依赖。
- **静态平台抽象与各端实现（`crates/loom-{pal-api,pal-windows,pal-macos,pal-linux,pal}`）**：`loom-pal-api` 定义编译期单态化 Trait 与复用缓冲契约；各平台独立 Crate 实现具体系统调用；`loom-pal` 通过 `#[cfg(target_os)]` 静态导出 `NativeBackend`。
- **无头采样与业务引擎（`crates/loom-engine`）**：承载快慢双通道调度、60 点定长量化时序环、防复用进程树、预排序视图投影与 `MonitorLease` 租约管理，**禁止引入 GPUI 或多线程 Tokio 依赖**。
- **组合式 UI 底座与桌面主程序（`crates/loom-{ui-kit,app}`）**：`loom-ui-kit` 基于 `gpui-kit` 封装复合控件；`loom-app` 采用 **Shell + 垂直功能切片** 结构（`shell/` + `features/{processes,performance,app_history,startup,users,details,services,settings}/`）。

<a id="coding-conventions"></a>

## 编码规范

- **文档先行**：从[文档索引](index.zh.md)定位当前规格。涉及架构、PAL 契约、性能预算或用户可见行为变更时，在同一个 PR 中同步更新中英双语文档（`*.md` 与 `*.zh.md`）。
- **保持静态分派与无头边界**：
  - `loom-engine` 及更底层 Crate 保持无头可测，不引入多线程异步运行时。
  - 跨平台调用采用泛型单态化（`EngineCoordinator<B: PlatformBackend>`）与 `Copy` 复用缓冲区，热路径避免 `Box<dyn Trait>` 与逐周期 `String`/`Vec` 堆分配。
- **遵守 Windows 六项防卡死约束**：
  - `loom-pal-windows` 不依赖 WMI 或 PDH；不调用 `GetWindowTextW` 或阻塞式跨进程 `SendMessageW`（使用 `InternalGetWindowText` 与 `IsHungAppWindow`）；不使用 `ReadProcessMemory` 跨进程读 PEB（使用 `ProcessCommandLineInformation`）。
  - 图标提取、PE 版本/签名读取（附加 `WTD_CACHE_ONLY_URL_RETRIEVAL`）与磁盘 `IOCTL` 查询运行在慢通道舱壁池并挂载 `300 ms` `CircuitBreaker`，网络驱动器路径走内存属性旁路。
- **遵守 UI 切片与组件组合规则**：
  - 每个功能模块独立管理自身的 GPUI `Entity`、`TableState`、`Input` 状态、滚动句柄与功能内局部弹窗；不向根窗口堆积跨页可变字段，也不维护全局弹窗大枚举。
  - 表格、面积图/折线图、微缩波形图、树与菜单均通过 `gpui-kit`（`gpui-component`）现成组件与 Flexbox 组合完成，避免手写自定义 `Canvas` 或 Shader。
  - 表格选中态与右键动作绑定 `ProcessKey (pid, create_time_100ns)`，按需视图绑定 RAII `MonitorLease`。
- **原子落盘与 Rust 约定**：`settings.toml` 写入统一经过同目录 `tempfile -> sync_all -> atomic rename`；收窄模块可见性（`pub(super)` / `pub(crate)`），库边界使用 `thiserror` 定义结构化错误（区分 `PalError::ElevationRequired`）。

<a id="testing-conventions"></a>

## 测试规范

按验证目标选择最低有效测试层级：

| 层级 | 机制与位置 | 覆盖范围 |
| --- | --- | --- |
| 纯逻辑单元测试 | `crates/` 下的普通 `#[test]` | `loom-config` 序列化与迁移、定长量化 `HistoryRing`、防 PID 复用进程树构建、SMBIOS 原始字节流解析、热力图分档与单位格式化。 |
| 无头引擎与故障注入集成测试 | `crates/loom-engine/tests/` | 基于 `MockBackend` 端到端验证 250ms 首帧差分、`MonitorLease` 按需启停、自底向上进程树终止顺序，以及模拟慢盘 5s 超时注入下快通道 `< 2 ms` 非阻塞执行。 |
| 无头 UI 集成测试 | 基于 `gpui-kit` 测试支撑的 `#[gpui::test]` | 8 个功能切片挂载渲染、`UiStore` 分域 `Revision` 增量刷新、`ProcessKey` 选中防漂移、跨页导航跳转与中英双语切换。 |
| 桌面真机与冒烟验证 | `agent-desktop` 自动化交互 + 本机实测 | 使用 `agent-desktop` 验证真实窗口布局、无障碍快照、搜索过滤与右键菜单交互；实测冷启动耗时（`< 50 ms`）、100% CPU 负载抢占唤醒、最小化 `0% GPU` 休眠与按需 UAC 提权流程。 |

测试编写约定：

- 不依赖 GPUI `App` / `Window` 时优先使用普通 `#[test]`。
- 每个防卡死机制（如 `CircuitBreaker` 熔断、PID 快速回收复用）均应配备确定性回归测试。
- 验证结束后清理临时转储文件、日志与截图产物。

<a id="upstream-contributions"></a>

## 上游贡献

- 确认 `gpui-kit`（`gpui-component` / `gpui-base`）或 Zed `gpui` 存在缺陷时，先基于当前锁定版本构造最小复现。
- 遵循对应上游仓库的贡献、测试与许可规则，保持补丁最小化。
- 在本地跟踪 Issue 中关联上游 Issue/PR，并记录临时适配逻辑的移除条件。

<a id="development-environment"></a>

## 开发环境

安装 [Rustup](https://rustup.rs/) 并与 `rust-toolchain.toml`（于 `0.0.1` 初始化）中锁定的工具链版本保持一致。

| 平台 | 构建依赖 |
| --- | --- |
| Windows 11 x64 / ARM64（当前主力目标） | Visual Studio Build Tools（「使用 C++ 的桌面开发」工作负载，含 MSVC 与 Windows 11 SDK）。 |
| Linux（Ubuntu 24.04 x64，`0.2.0` 目标） | 标准 Rust/C 工具链、GPUI 所需的 Wayland/X11 与 Vulkan 开发头文件、`dbus` 开发库。 |
| macOS（Apple Silicon / x64，`0.3.0` 目标） | Xcode 命令行工具。 |

在并行 Git worktree 中开发时，将 `CARGO_TARGET_DIR` 指向主检出的 `target` 目录以复用增量构建缓存。

<a id="local-checks"></a>

## 本地检查

提交或更新 PR 前，按改动范围运行检查（Rust Workspace 于 `0.0.1` 初始化后启用）：

```powershell
git diff --check origin/main
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
$env:RUSTDOCFLAGS = '-D warnings'; cargo doc --locked --workspace --no-deps --all-features
cargo test --locked --workspace --all-features
```

后端与引擎迭代期间的定向无头测试：

```powershell
cargo test --locked -p loom-types -p loom-config -p loom-pal-windows -p loom-engine
```
