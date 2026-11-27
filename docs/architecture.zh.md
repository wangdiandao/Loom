# 系统架构

[English](architecture.md) | 中文

Loom 是一款基于 **Rust** 与 **GPUI Kit (`gpui-kit` / `gpui-component`)** 构建的跨平台（Windows / macOS / Linux）原生桌面任务管理器。当前阶段聚焦 **Windows** 平台实现，在对齐 Windows 11 自带任务管理器核心功能的基础上，重点解决**冷启动慢、高负载卡顿以及未响应窗口或慢速外置盘导致的启动阻塞**问题，并通过编译期静态平台抽象层（PAL）为后续扩展 Linux 与 macOS 后端提供清晰契约。

<a id="core-architectural-principles"></a>

## 1. 核心架构原则

1. **后端无头化与无重型运行时（Headless & Runtime-Free Core）**：系统采样、差分计算、进程树构建、搜索排序索引与管控命令调度均放在无 UI 依赖的 `loom-engine` 及更底层 Crate 中，不引入多线程 `tokio` 运行时，避免在高核数 CPU（如 32 逻辑核）启动时派生大量空闲工作线程。
2. **编译期静态平台抽象（Zero-Cost Static PAL）**：Windows、macOS、Linux 后端分别独立成 Crate，通过关联类型（Associated Types）、零大小扩展类型（ZST）与 `loom-pal` 的 `#[cfg(target_os)]` 静态类型别名完成单态化绑定，热路径避免虚表（`dyn Trait`）与装箱 Future。
3. **快慢双通道舱壁隔离（Dual-Lane Bulkhead Isolation）**：将亚毫秒级纯内存态内核调用（快通道）与可能发生磁盘、Shell 或驱动阻塞的元数据探测（慢通道）隔离，配合 `300 ms` 设备级超时熔断器，降低启动与运行期卡死风险。
4. **冷热数据分离与缓冲区复用**：以 `(pid, create_time_100ns)` 复合键标识进程生命周期；高频热指标使用 `Copy` 值结构覆写复用缓冲区，低频冷元数据（图标、PE 发布者、命令行）按路径二级共享缓存并仅解析一次。
5. **UI 垂直功能切片与组件组合**：`loom-app` 按 `shell` + `features/*` 切片组织，直接组合 `gpui-kit` 现成控件（`Table`、`AreaChart`、`LineChart`、`Sidebar`、`TitleBar`、`Dialog`、`PopupMenu`、`Tree`、`Select`）与 Flexbox 布局，避免根视图全局大状态、全局弹窗大枚举以及手写自定义 `Canvas`/Shader。
6. **分域只读快照与按需监控租约（RAII `MonitorLease`）**：通过 `ArcSwap<AppSnapshot>` 配合单调递增的分域 `Revision` 无锁发布视图投影；按需管理域（服务、启动应用、用户、应用历史、多核网格）通过 RAII `MonitorLease` 引用计数按需激活，离开页面或最小化窗口时自动停采或降频。
7. **普通权限启动与按需提权**：默认以普通用户权限启动，避免冷启动阶段弹出 UAC 确认框；仅在操作高完整性进程、启停系统服务或勾选以管理员运行新任务时触发 `runas` 或提权重启恢复当前视图。

<a id="performance-budgets-and-anti-hang-architecture"></a>

## 2. 性能预算与防卡死架构

### 2.1 性能与资源预算

以现代多核桌面环境（16 核 32 逻辑处理器、400+ 进程、10,000+ 线程）为基准设定工程预算：

| 指标维度 | Loom 工程预算 | 原生 Windows 11 任务管理器常见问题 |
| :--- | :--- | :--- |
| **冷启动首帧可交互时间** | **`< 50 ms`**（启动提升优先级，`< 35 ms` 渲染首屏进程表） | XAML Islands / WMI / COM 同步初始化常耗时 `800 ~ 3000+ ms` |
| **异常环境抗阻塞能力** | **慢 I/O 舱壁隔离 + `300 ms` 熔断旁路** | 遇未响应窗口、休眠机械盘、异常 U 盘或断连网络盘时容易白屏或阻塞 |
| **稳态后台 CPU 占用（1s 周期）** | **`< 0.15%`**（单次内核扫描 + 差分 `< 1 ms`） | 常达 `0.8% ~ 3%+`，展开多核网格或高频滚动时升高 |
| **静止与最小化 GPU 占用** | **按刷新周期按需重绘（默认 1s 时约 `1 FPS`）；最小化 / 关窗托盘 `0% GPU`** | 跨框架合成桥接开销较高，最小化仍存在后台布局计算 |
| **私有工作集内存（Private WS）** | **`30 ~ 45 MB`**（上限 `< 45 MB`；复用 Scratch 缓冲 + `20×20` 图标缓存） | 常见占用 `80 ~ 160+ MB` |
| **自身系统线程总数** | **`6 ~ 8` 个系统线程以内** | 常见派生 `30 ~ 60+` 个线程池、COM 与性能计数器线程 |

### 2.2 渐进式冷启动管线（`< 50 ms` 目标）

```mermaid
sequenceDiagram
    autonumber
    participant Main as "GPUI 主线程 (loom-app)"
    participant Fast as "快通道内核采样线程 (HIGH / ABOVE_NORMAL)"
    participant Slow as "慢通道探测舱壁池 (最多2线程, 低优先级)"

    Note over Main: T = 0 ~ 2 ms (优先级提升与配置加载)
    Main->>Main: SetPriorityClass(HIGH_PRIORITY_CLASS) 防止 100% 负载时被饿死
    Main->>Main: 加载轻量级 settings.toml (小于 4 KB, 无注册表/COM 阻塞)
    par 窗口初始化与首帧快照并行执行
        Main->>Main: gpui_kit::open_window 创建 DX12 窗口与首屏骨架 (5~30 ms)
    and
        Main->>Fast: 启动快采线程执行首次 NtQuerySystemInformation (~1 ms)
        Fast-->>Main: 发布首帧进程表 (PID / 映像名 / 专用工作集 / 状态)
    end
    Note over Main: T ≈ 30 ~ 45 ms: 首帧可交互 (可搜索并执行结束任务)
    Fast->>Fast: T = +250 ms 二次校准采样，产出初始 CPU% / I/O 速率后切入 1s 稳态周期
    Fast-)Slow: 异步投递新 ProcessKey 冷元数据与硬件静态信息探测任务
    Slow-->>Main: 分批异步回填 20x20 图标、PE 发布者、SMBIOS 与磁盘型号
```

### 2.3 Windows 六项防卡死约束

1. **不依赖 WMI 与 PDH**：不调用 `IWbemServices`（跨进程 RPC 至 `Winmgmt`）与 `PdhOpenQuery`（加载第三方性能计数器 DLL 并争抢全局注册表锁）。CPU/进程调用 `ntdll`，网卡调用 `iphlpapi`，磁盘调用带隔离保护的 `IOCTL`，GPU 调用 `DXGI + gdi32!D3DKMT`，内存频率与插槽直接解析 `GetSystemFirmwareTable('RSMB')` 原始 SMBIOS 表（`< 0.05 ms`）。
2. **不跨进程发送同步窗口消息**：不调用 `GetWindowTextW` 或阻塞式 `SendMessageW`（桌面上存在未响应程序时可能长时间阻塞调用线程）。统一使用 `InternalGetWindowText` 与非阻塞 `IsHungAppWindow`。
3. **慢速磁盘与断连网络盘舱壁熔断**：对 UNC 路径或 `DRIVE_REMOTE` 网络映射盘跳过打开远程文件提取图标/签名，退回 `SHGFI_USEFILEATTRIBUTES` 内存级扩展名图标；磁盘静态属性与卷映射（`IOCTL_STORAGE_QUERY_PROPERTY`、`IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`）、每设备 `IOCTL_DISK_PERFORMANCE` 采样以及 Shell 图标/PE 版本读取均隔离在慢通道舱壁池中，单次探测超过 `300 ms` 触发设备级 `CircuitBreaker` 冷却 60 秒并旁路跳过，避免阻塞快通道。
4. **不使用 `ReadProcessMemory` 跨进程读目标 PEB**：进程命令行统一通过 `NtQueryInformationProcess(ProcessCommandLineInformation)` 获取，规避目标进程挂起或换页锁导致的等待。
5. **发布者解析不触发外网吊销检查**：优先读取本地 PE `VersionInfo`（`CompanyName` / `FileDescription`）；若校验 Authenticode 签名，附加 `WTD_CACHE_ONLY_URL_RETRIEVAL` 避免发起 CRL/OCSP 网络请求。
6. **不长期持有目标进程句柄**：大部分动态指标直接取自 `SystemProcessInformation` 缓冲；按需打开 `PROCESS_QUERY_LIMITED_INFORMATION` 后立即 `CloseHandle`，避免句柄泄漏与目录占用。

<a id="system-topology-and-execution-domains"></a>

## 3. 系统拓扑与执行域边界

系统在单一用户态主进程内划分为 **三个隔离的执行域**，按需提权操作通过独立的系统 Shell 提权边界完成：

```mermaid
flowchart TD
    subgraph MainProc["loom-app 主进程 (默认普通用户权限, HIGH_PRIORITY_CLASS)"]
        subgraph UIDomain["执行域 1: GPUI 主线程 (按需稳态重绘 / 交互帧率)"]
            Shell["AppShell (TitleBar 全局搜索 / 可折叠 Sidebar / 壳层弹窗 / Toast)"]
            subgraph Features["独立 UI 垂直功能切片 (互不直接依赖)"]
                FProc["feature::processes"]
                FPerf["feature::performance"]
                FHist["feature::app_history"]
                FStart["feature::startup"]
                FUser["feature::users"]
                FDet["feature::details"]
                FSvc["feature::services"]
                FSet["feature::settings"]
            end
            UIKit["loom-ui-kit (基于 gpui-kit 的复合控件, 无自绘)"]
            Store["UiStore (分域 Revision 切片订阅)"]
            Tray["TrayController (窗口关闭后零 GPU 常驻)"]

            Shell --> Features
            Features --> UIKit
            Features --> Store
        end

        subgraph Bridge["进程内无锁桥接层"]
            CmdTx["CommandBus (有界 EngineCommand 通道)"]
            SnapSwap["SnapshotStore (ArcSwap AppSnapshot + DomainDirtyFlags)"]
            LeaseReg["MonitorLease (RAII 按需采样租约计数)"]
        end

        subgraph FastDomain["执行域 2: 快通道内核采样线程 (1 专属线程, 低于 1 ms/tick, 无阻塞 I/O)"]
            Coord["EngineCoordinator (差分 / 60点定长环 / 防复用进程树 / 预排序投影)"]
            FastPAL["FastSystemSampler (ntdll / iphlpapi / D3DKMT / InternalGetWindowText)"]
            Coord --> FastPAL
        end

        subgraph SlowDomain["执行域 3: 慢通道探测舱壁池 (1~2 按需休眠线程 + 300 ms 熔断器)"]
            ProbePool["SlowMetadataProber + 磁盘 IOCTL + CircuitBreaker"]
            OnDemand["按需管理器 (SCM Services / Startup / WTS Users / AppHistory / WCT / MiniDump)"]
        end

        Features -- "派发 EngineCommand" --> CmdTx
        Tray -- "派发 EngineCommand" --> CmdTx
        Features -- "持有 MonitorLease" --> LeaseReg
        LeaseReg --> Coord
        CmdTx --> Coord
        ProbePool -. "异步无锁回填 (ExeStaticAsset / Disk)" .-> Coord
        Coord -- "执行低频/阻塞指令" --> OnDemand
        Coord -- "原子发布预排序分域快照" --> SnapSwap
        SnapSwap --> Store
    end

    subgraph ElevBoundary["按需提权边界 (仅用户显式触发时拉起)"]
        RunAs["UAC ShellExecuteExW('runas') 单次提权任务 / 提权重启实例并恢复当前页"]
    end

    OnDemand -. "遇 ElevationRequired 用户确认后" .-> RunAs
```

<a id="workspace-module-layout-and-dependency-dag"></a>

## 4. Workspace 模块划分与依赖拓扑

通过 10-Crate Cargo Workspace 在编译期约束单向依赖 DAG：

```mermaid
flowchart BT
    types["loom-types"]
    cfg["loom-config"]
    pal_api["loom-pal-api"]
    pal_win["loom-pal-windows"]
    pal_mac["loom-pal-macos"]
    pal_lin["loom-pal-linux"]
    pal["loom-pal"]
    engine["loom-engine"]
    uikit["loom-ui-kit"]
    app["loom-app"]

    pal_api --> types
    pal_win --> pal_api
    pal_win --> types
    pal_mac --> pal_api
    pal_mac --> types
    pal_lin --> pal_api
    pal_lin --> types
    pal --> pal_api
    pal -. "cfg(windows)" .-> pal_win
    pal -. "cfg(target_os = macos)" .-> pal_mac
    pal -. "cfg(target_os = linux)" .-> pal_lin
    engine --> types
    engine --> cfg
    engine --> pal_api
    uikit --> types
    app --> types
    app --> cfg
    app --> pal
    app --> engine
    app --> uikit
```

### 各 Crate 职责与边界

| Crate | 类型 | 核心职责 | 依赖约束与边界 |
| :--- | :--- | :--- | :--- |
| `loom-types` | lib | 跨平台紧凑领域模型（`ProcessKey`、`ProcessHotSample`、`ProcessColdMeta`、五大硬件快照、服务/启动项/用户/历史记录、`CapabilityFlags`） | 无 OS 依赖、无 UI 依赖；高频热结构使用 `Copy` 紧凑内存布局 |
| `loom-config` | lib | 本地配置持久化（`settings.toml`：启动页、刷新周期、主题、语言、窗口几何、置顶/托盘行为、列偏好）、Schema 向前迁移与原子落盘 | 同步加载目标 `< 1 ms`，不依赖平台后端或 UI |
| `loom-pal-api` | lib | 静态平台抽象 Trait 族（`PlatformBackend`、`FastSystemSampler`、`SlowMetadataProber` 等）、调用方复用缓冲 `SampleScratch` 与 `PalError` | 仅依赖 `loom-types`，不暴露特定操作系统句柄或头文件类型 |
| `loom-pal-windows` | lib | Windows 采集与系统控制实现（`ntdll` / `D3DKMT` / `IOCTL` / `SMBIOS` / `SCM` / `WTS` / `WCT` / `MiniDump`） | 仅 `cfg(windows)` 编译；采用 `windows-sys`，内部收敛 `unsafe` FFI，遵守六项防卡死约束 |
| `loom-pal-macos` | lib | macOS 后端预留骨架（`Mach` / `libproc` / `IOKit` / `launchd` 映射位） | 仅 `cfg(target_os = "macos")` 编译；当前提供编译期契约占位桩，计划于 `0.3.0` 实现 |
| `loom-pal-linux` | lib | Linux 后端预留骨架（`procfs` / `sysfs` / `netlink` / `drm` / `systemd` 映射位） | 仅 `cfg(target_os = "linux")` 编译；当前提供编译期契约占位桩，计划于 `0.2.0` 实现 |
| `loom-pal` | lib | 平台编译期静态门面（导出 `pub type NativeBackend = ...`）及跨平台单实例锁、系统托盘与电源/显示状态适配 | 消除上层代码中的 `#[cfg(target_os)]` 分支，无虚表动态包装 |
| `loom-engine` | lib | 无头采样与业务编排引擎（`EngineCoordinator<B>`、差分计算、60 点定长量化环形队列、防复用进程树、预排序视图投影、`MonitorLease`） | 禁止依赖 GPUI Crate 或多线程 Tokio；支持注入 `MockBackend` 进行无头基准与故障注入测试 |
| `loom-ui-kit` | lib | 基于 `gpui-kit` 的薄组合层：热力图单元格、性能侧边栏微缩 `AreaChart` 卡片、逻辑核响应式网格、内存四段比例条、主题语义色与 `rust-i18n` 辅助 | 隔离上游 `gpui-kit` 细节，无自定义 `Canvas`/Shader 绘制，不持有业务领域状态 |
| `loom-app` | bin | 桌面组合根与垂直功能切片 UI：入口优先级提升、渐进启动管线、`AppShell` 与 8 个独立 `features/*` 模块 | 各 `feature` 模块不互相 import 内部实现，跨页导航（`转到详细信息` / `转到服务`）经由 `ShellHandle` 与 `NavigationIntent` 完成 |

<a id="feature-sliced-ui-architecture"></a>

## 5. UI 层模块化架构（`loom-app` & `loom-ui-kit`）

```text
crates/loom-app/src/
├── main.rs                  # 入口：HIGH_PRIORITY_CLASS 提升、单实例唤醒、组合根启动
├── bootstrap.rs             # 配置加载 + EngineCoordinator<NativeBackend> + GPUI 窗口并行拉起
├── bridge/
│   ├── store.rs             # UiStore：按域持有 Arc<*Slice> 与 Revision 增量订阅
│   ├── commands.rs          # EngineCommand 派发与权限拦截反馈 (ElevationRequired -> 提权通知)
│   └── monitor_lease.rs     # RAII MonitorLease 封装（视图卸载或最小化自动释放按需采样）
├── shell/
│   ├── window.rs            # Root 包裹的主窗口（TitleBar 搜索栏 + 可折叠 Sidebar + 页面出口）
│   ├── nav.rs               # 声明式路由表与跨页定位总线 (JumpToDetails / JumpToServices)
│   ├── dialogs.rs           # 仅放置跨页通用壳层弹窗（运行新任务 / 通用确认）
│   └── tray.rs              # 托盘常驻控制器（关窗销毁 GPUI 窗口释放显存）
└── features/                # 8 个独立垂直功能切片，互不直接依赖
    ├── processes/           # 进程页（三段分类视图 / 父子进程树切换 + 热力图表格 + 右键菜单）
    ├── performance/         # 性能页（左微缩波形栏 + 右 CPU/内存/磁盘/网络/GPU 子视图）
    ├── app_history/         # 应用历史记录页（累计资源表 + 清空历史）
    ├── startup/             # 启动应用页（上次 BIOS 耗时 + 启用/禁用管理）
    ├── users/               # 用户页（会话级聚合行 + 可展开会话进程树 + 断开连接）
    ├── details/             # 详细信息页（全字段虚拟化表/树 + 局部相关性/等待链弹窗 + 菜单）
    ├── services/            # 服务页（SCM 服务表 + 启停重启工具栏 + 进程联动）
    └── settings/            # 设置页（启动页 / 刷新速度 / 窗口置顶与托盘 / 主题与语言）
```

特定功能弹窗（如 `features/details/` 中的「设置相关性」与「分析等待链」）保留在各自功能切片内，通过 `gpui-kit` 的 `Root` 弹窗层挂载，不在 `shell/` 中维护全局弹窗大枚举。

### 5.1 分域只读快照与预计算视图投影

`AppSnapshot` 拆分为独立的 `Arc` 子切片，并在 `loom-engine` 后台线程完成排序、搜索过滤与树展开扁平化：

- `processes: Arc<ProcessTableSlice>`（已按当前排序列、搜索词与展开态扁平化的 `Vec<ProcessRowProjection>`，选中态与右键菜单锚定 `ProcessKey`，避免行重排导致误操作；`ProjectionEngine` 绑定 `PlatformBackend::PLATFORM`，仅在 `PlatformKind::Windows` 分组视图下启用对齐任务管理器的壳层解嵌套与多进程应用聚合，不影响 Linux/macOS 分组与通用 `Process Tree`）
- `details: Arc<DetailsTableSlice>`（仅在 `Details` 页激活持有租约时构建投影）
- `performance: Arc<PerformanceSlice>`（五大类硬件摘要、60 点定长量化历史环与设备元数据）
- `services: Arc<ServicesSlice>` / `startup: Arc<StartupSlice>` / `users: Arc<UsersSlice>` / `app_history: Arc<AppHistorySlice>`（均受 `MonitorLease` 门控，未查看时无后台轮询与投影开销）
- `settings: Arc<SettingsSlice>`（当前生效配置的只读投影）

`loom-app` 启用 `gpui-kit` 的 `gpui-fast` 渲染后端（保留子树与局部 Splice、Taffy 布局节点跨帧复用、`NumberShaping` 数字快速整形、GPU Scroll Layers 瓦片缓存与 D3D11 状态去重），并将静态页面工具栏与高频刷新的二维虚拟化 `DataTable`（`Entity<TableState<D>>`）解耦。GPUI 主线程在 `TableDelegate::render_td` 中仅对视口可见行与可见列做 `O(1)` 索引读取（目标 UI 线程单帧 `< 0.5 ms`，交互滚动稳跑 `120 FPS`）。

### 5.2 `gpui-kit` 组件映射

| 界面区域 / 视觉元素 | 选用的 `gpui-kit` (`gpui-component`) 控件 | 组合实现方式（无自定义绘制） |
| :--- | :--- | :--- |
| **窗口顶栏与全局搜索** | `Root` + `TitleBar` + `Input` | 居中搜索框开启 `.cleanable(true)`，支持名称/发布者/PID 防抖过滤 |
| **左侧导航与底部设置** | `Sidebar`、`SidebarMenu`、`SidebarMenuItem`、`SidebarFooter` | 汉堡按钮控制 `.collapsed(bool)` 展开/图标态，底部固定「设置」 |
| **虚拟化业务表格与树表** | `DataTable` + `TableState<D>` + `TableDelegate` | 首列左固定（`fixed_left`）通过 `h_flex().pl(px(depth * 16.))` + 折叠箭头 `Button` + 降采样 `Img` 呈现进程树/用户树；滚动列双向虚拟化；表级单一右键菜单；热力图列按占用分档设置单元格背景色透明度 |
| **多级右键菜单** | `PopupMenu` / `.context_menu(...)` | 支持勾选标记（`✓ 效能模式`）、分隔线与二级子菜单（`设置优先级 >`、`资源值 >`） |
| **性能页左栏设备微缩波形图** | `v_virtual_list` + `AreaChart` | 配置 `AreaChart::new(pts).linear().y_domain(0., 100.).point_count(60).x_axis(false).grid(false).interactive(false)` |
| **CPU 总体+内核双层图 & 多核网格** | 多序列 `AreaChart` + 响应式网格容器 | 单一 `AreaChart` 挂载总利用率与内核时间双 `.y(...)` 渐变序列；多核模式按逻辑核数排列紧凑 `AreaChart` 单元 |
| **磁盘双图 / 网卡吞吐 / GPU 多引擎** | `AreaChart` / `LineChart` + `Select` | 动态 `y_domain` 实现自适应量纲刻度；GPU `2×2` 子图标题嵌入无边框 `Select` 切换引擎 |
| **内存组成四段比例条** | `h_flex` + `relative(ratio)` + `Tooltip` | Flexbox 相对宽度色块拼接「使用中 / 已修改 / 备用 / 可用」，每段挂载 `Tooltip` |
| **运行新任务 / 相关性 / 等待链弹窗** | `Dialog`、`Input`、`Checkbox`、`Tree`、`Notification` | 等待链阻塞树使用 `Tree` 控件；遇 `ElevationRequired` 弹出附带「以管理员重启」按钮的 `Notification` |

<a id="core-backend-subsystems"></a>

## 6. 核心后端子系统设计

### 6.1 防 PID 复用进程树与进程树终止 (`loom-engine` & `loom-pal-windows`)

Windows 内核会高频复用已退出进程的 `PID`。Loom 统一使用 `ProcessKey { pid, create_time_100ns }`：

- **父子关系校验**：仅当父进程存在且满足 `parent.create_time_100ns <= child.create_time_100ns` 时才挂载为子节点，否则提升为根级节点，避免伪父子环路与缓存串号。
- **自底向上结束进程树（`KillProcessTree`）**：按校验后的 `ProcessKey` 子树收集后代，先挂起或自叶向根依次终止，避免守护型父进程在退出过程中重新拉起子进程。
- **二级冷资产去重缓存**：`ProcessKey -> Arc<ProcessColdMeta> -> Arc<ExeStaticAsset>`（按规范化路径索引）。提取 EXE 图标后缩放到 `20×20` RGBA8（单图标约 `1.6 KB`）并释放原始大图句柄，多进程应用共享同一份静态资产。

### 6.2 无 WMI/PDH 五大硬件实时采样 (`loom-pal-windows::performance`)

- **CPU**：`NtQuerySystemInformation(SystemProcessorPerformanceInformation)` 获取每逻辑核 `Idle/Kernel/User` 时间片；`GetLogicalProcessorInformationEx` 启动缓存插槽、物理核、NUMA 与 L1/L2/L3 缓存；`CallNtPowerInformation` 计算实时主频（GHz）。
- **内存**：`GlobalMemoryStatusEx` + `GetPerformanceInfo` + `SystemMemoryListInformation` 获取内存组成；解析 `GetSystemFirmwareTable('RSMB')` 获取内存频率（MT/s）、插槽数与外形规格。
- **磁盘**：通过 `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` 建立物理盘与盘符映射（如 `磁盘 0 (C: D:)`），`IOCTL_STORAGE_QUERY_PROPERTY` 识别 NVMe / SATA SSD / HDD 与型号，`IOCTL_DISK_PERFORMANCE` 读取活动时间、读写速率与平均响应延迟，并按设备隔离受 `300 ms` 熔断器保护。
- **网络**：`GetIfTable2` + `GetAdaptersAddresses` 采集 64 位收发字节计数、链路速率、连接类型（以太网 / Wi-Fi）及 IPv4/IPv6/DNS。
- **GPU**：厂商中立的 `DXGI` + `gdi32!D3DKMTQueryStatistics` 采集各 Node 引擎（`3D`、`Copy`、`Video Encode`、`Video Decode` 等）时间片差分与全局/每进程专享及共享显存，配合 `D3DKMTQueryAdapterInfo(KMTQAITYPE_ADAPTERPERFDATA)` 读取核心温度与 PCI 总线位置。

### 6.3 跨平台归一化映射（面向 `0.2.0` Linux 与 `0.3.0` macOS）

| 统一抽象概念 (`loom-types`) | Windows (`loom-pal-windows`) | Linux (`loom-pal-linux`, `0.2.0`) | macOS (`loom-pal-macos`, `0.3.0`) |
| :--- | :--- | :--- | :--- |
| **生命周期主键 `ProcessKey`** | `(PID, CreateTime FILETIME)` | `(pid_t, /proc/[pid]/stat starttime)` | `(pid_t, pbi_start_tvsec)` |
| **效能/节能模式 (`EcoMode`)** | `EcoQoS` (`PROCESS_POWER_THROTTLING`) | `SCHED_IDLE` / `nice` / cgroup weight | Task QoS / App Nap |
| **系统服务 (`ServiceRecord`)** | Windows SCM (`EnumServicesStatusExW`) | `systemd` D-Bus Units | `launchd` Daemons & Agents |
| **启动应用 (`StartupEntry`)** | 注册表 `Run`/`StartupApproved` + 启动目录 + UWP `StartupTask` | XDG Autostart (`~/.config/autostart/*.desktop`) | `SMAppService` / `LaunchAgents` |
| **用户会话 (`UserSession`)** | WTS (`WTSEnumerateSessionsW`) | `systemd-logind` Sessions | `utmpx` / Console User |
| **虚拟化/沙箱列** | UAC 虚拟化 (`TokenVirtualizationEnabled`) | Namespace / Flatpak / Snap | App Sandbox Entitlements |
| **等待链诊断 (`WaitChain`)** | Win32 WCT (`GetThreadWaitChain`) | `/proc/[pid]/task/[tid]/wchan` | `proc_pidinfo` Thread `wchan` |
| **按需管理员提权** | `ShellExecuteExW("runas")` | `pkexec` / Polkit Agent | Authorization Services |

<a id="core-extension-contracts"></a>

## 7. 核心静态扩展契约

```rust
// crates/loom-pal-api: 编译期单态化平台后端总契约（无虚表、无装箱 Future）
pub trait PlatformBackend: Send + 'static {
    /// 平台专属紧凑进程扩展字段（Copy 语义；无需扩展的平台设为 `()` 占 0 字节）
    type ProcessExt: Copy + Default + Send + Sync + 'static;

    type FastSampler: FastSystemSampler<Ext = Self::ProcessExt>;
    type SlowProber: SlowMetadataProber;
    type ServiceMgr: ServiceManager;
    type StartupMgr: StartupManager;
    type SessionMgr: SessionManager;
    type HistoryReader: AppHistoryReader;
    type Operator: SystemOperator;

    /// 静态平台身份画像（`Windows` / `Linux` / `macOS`）
    const PLATFORM: PlatformProfile;

    /// 编译期静态能力位图：用于消除当前平台不支持的 UI/控制分支
    const CAPABILITIES: CapabilityFlags;

    /// 进程入口优先级提升
    fn boost_self_priority();

    /// 宿主主窗口置顶与恢复钩子（无头或未实现平台默认空操作）
    fn set_main_window_always_on_top(_enabled: bool) {}
    fn is_main_window_minimized() -> bool { false }
    fn restore_and_focus_main_window() {}

    fn initialize() -> Result<Self, PalError>
    where
        Self: Sized;
}

// crates/loom-pal-api: 快通道采样契约（仅限纯内存态内核调用，复用 SampleScratch 避免逐周期堆分配）
pub trait FastSystemSampler: Send {
    type Ext: Copy + Default + Send + Sync + 'static;

    fn sample_processes(
        &mut self,
        scratch: &mut ProcessSampleScratch<Self::Ext>,
    ) -> Result<Timestamp100Ns, PalError>;

    fn sample_performance(
        &mut self,
        scratch: &mut PerfSampleScratch,
    ) -> Result<Timestamp100Ns, PalError>;
}

// crates/loom-pal-api: 系统管控与诊断契约（统一结构化返回 ElevationRequired）
pub trait SystemOperator: Send + Sync {
    fn run_new_task(&self, req: &RunTaskRequest) -> Result<Option<u32>, PalError>;
    fn terminate_process(&self, key: ProcessKey) -> Result<(), PalError>;
    fn terminate_process_tree(&self, targets: &[ProcessKey]) -> Result<TreeKillReport, PalError>;
    fn set_efficiency_mode(&self, key: ProcessKey, enabled: bool) -> Result<(), PalError>;
    fn set_priority(&self, key: ProcessKey, priority: ProcessPriority) -> Result<(), PalError>;
    fn set_affinity(&self, key: ProcessKey, mask: &CpuAffinityMask) -> Result<(), PalError>;
    fn set_uac_virtualization(&self, key: ProcessKey, enabled: bool) -> Result<(), PalError>;
    fn analyze_wait_chain(&self, key: ProcessKey) -> Result<Vec<WaitChainNode>, PalError>;
    fn write_minidump(&self, key: ProcessKey, dest: &Path, cancel: &AtomicBool) -> Result<(), PalError>;
    fn relaunch_self_elevated(&self, restore_route: &str) -> Result<(), PalError>;
}
```

<a id="ecosystem-reuse"></a>

## 8. 开源生态复用策略

| 能力域 | 复用的成熟开源 Crate | 项目自研薄层 |
| :--- | :--- | :--- |
| **桌面 UI 与图表表格组件** | `gpui-kit`（聚合兼容版本的 `gpui`、`gpui-base`、`gpui-component` 与内置图标资源） | `loom-ui-kit`：热力图单元格、微缩波形卡片、多核网格与四段内存条组合 |
| **Windows 原生内核与图形接口** | `windows-sys`（C ABI 绑定：`ntdll`、`iphlpapi`、`gdi32`、`dxgi`、`advapi32`、`dbghelp`、`wtsapi32`、`shell32`） | `loom-pal-windows`：Safe Rust 封装、复用缓冲管理、SMBIOS 解析与 `CircuitBreaker` 慢 I/O 熔断 |
| **无锁快照与跨线程通道** | `arc-swap` + `crossbeam-channel` / `async-channel` + `parking_lot`（无多线程 Tokio） | 分域 `Revision` 切片投影、预排序视图构建与 `MonitorLease` RAII 租约 |
| **紧凑结构与位图** | `bitflags` + `smallvec` | 定长量化 `HistoryRing<N>`、全局字符串驻留池（`StringInterner`）与 `20×20` 图标缓存 |
| **系统托盘与单实例** | `tray-icon`（Linux 预留 `ksni`）+ `single-instance` + `directories` | 关窗销毁 GPUI 窗口释放显存、单实例唤醒聚焦与最小化休眠状态联动 |
| **配置与原子落盘** | `serde` + `toml` + `tempfile` + `thiserror` | `loom-config` 快速启动读取、Schema 迁移与 `tempfile -> sync_all -> rename` |
| **多语言与诊断日志** | `rust-i18n` + `tracing` + `tracing-subscriber` | `zh-CN` / `en-US` 实时切换与非阻塞诊断日志输出 |
