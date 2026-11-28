# 版本与发布

[English](release.md) | 中文

<a id="version-policy"></a>

## 版本号递进规则

Workspace 内全部内部 Crate（`loom-types`、`loom-config`、`loom-pal-api`、`loom-pal-windows`、`loom-pal-macos`、`loom-pal-linux`、`loom-pal`、`loom-engine`、`loom-ui-kit` 与 `loom-app`）统一使用根清单 `[workspace.package]` 的语义化版本号。

Loom 不使用抽象的 Phase 划分开发顺序，所有里程碑直接以语义化版本号定义，遵循以下递进规则：

1. **`0.0.x` — `0.1.0` 前期里程碑（`0.0.1`–`0.0.6`，聚焦 Windows 实现）**
   - `0.0.1`–`0.0.2` 完成无头双通道引擎、静态 PAL 契约与 `gpui-kit` 窗口壳层；**`0.0.3`** 达到日常可用的 **P0 进程管理核心**；`0.0.4`–`0.0.6` 依次补齐无 WMI/PDH 的硬件性能图表、服务/启动项/用户/应用历史管理，以及等待链/内存转储诊断与基准调优。
   - 每完成一个里程碑递增 `patch`（`0.0.1` → `0.0.2` → `0.0.3` → … → `0.0.6`）。
   - 允许内部 PAL Trait 与快照切片结构调整；暂不面向终端用户分发正式安装包。
2. **`0.1.0` — 首个 Windows 稳定版**
   - 冻结 `loom-config` v1 配置向前迁移保障（`CONFIG_CONTRACT_VERSION = "1.0"`、`CURRENT_SCHEMA_VERSION = 1`）与 `loom-pal-api` v1 跨平台静态契约（`PAL_API_CONTRACT_VERSION = 1`），完成性能预算验收（`< 50 ms` 冷启动、`< 0.15%` 稳态 CPU、慢 I/O 熔断隔离），并启用基于 GitHub Releases 的 Windows WiX MSI 与便携 ZIP 自动化打包及条件 Authenticode 代码签名。
3. **`0.y.z`（`0.1.0 <= v < 1.0.0`）— 公开迭代与多平台扩展期**
   - 递增 `minor`（`y`，同时将 `z` 归零）：交付新的平台后端里程碑（如 `0.2.0` 落地 `loom-pal-linux`、`0.3.0` 落地 `loom-pal-macos`）、主要功能扩展，或发生配置 Schema / `loom-pal-api` 破坏性调整。
   - 递增 `patch`（`z`）：向后兼容的缺陷修复、硬件传感器兼容修正、依赖安全小版本更新与交互微调。
   - 同一版本包含多种变更时采用最高适用级别；破坏性变更须在 [CHANGELOG.md](../CHANGELOG.md) 中写明迁移步骤。
4. **`1.0.0+` — 跨平台稳定期（遵循 [SemVer 2.0.0](https://semver.org/lang/zh-CN/)）**
   - `major`：配置 Schema 或公开契约的不兼容变更。
   - `minor`：向后兼容的新功能与公开弃用。
   - `patch`：向后兼容的缺陷修复与安全补丁。

<a id="versioned-roadmap"></a>

## 按版本号驱动的开发路线

| 目标版本 | 里程碑主题 | 交付范围 | 验收标准 |
| --- | --- | --- | --- |
| **`0.0.1`** | 工程骨架、静态 PAL 与无头引擎 | 建立 10-Crate Cargo Workspace 单向依赖 DAG；实现 `loom-types`、`loom-config`（`< 1 ms` 加载与原子落盘）、`loom-pal-api`、`loom-pal`（含 macOS/Linux 编译期占位桩）、`loom-pal-windows` 快通道（`HIGH_PRIORITY_CLASS` 提升、`NtQuerySystemInformation` 复用缓冲采样、`250 ms` 首帧差分、慢通道 `300 ms` `CircuitBreaker` 舱壁池）与 `loom-engine`（无 Tokio 调度器、防 PID 复用进程树、分域 `ArcSwap<AppSnapshot>` 与 `MonitorLease`）。 | 无头集成测试通过 `< 2 ms` 进程扫描、PID 回收防环路验证、慢盘超时注入下快通道不阻塞验证，且后台线程数受控。 |
| **`0.0.2`** | `gpui-kit` 窗口壳层、双语与设置页 | 实现 `loom-ui-kit`（热力图单元格、微缩 `AreaChart` 卡片、内存四段条、主题语义色）与 `loom-app` 壳层（渐进启动、`TitleBar` 全局防抖搜索、可折叠 `Sidebar`、`UiStore` 分域投影、托盘常驻与最小化/关窗 `0% GPU` 休眠、按需 UAC 提权辅助、`rust-i18n` `zh-CN`/`en-US` 双语切换，以及**设置**页）。 | 桌面窗口在 `< 50 ms` 内进入可交互首帧，支持中英文与深浅色即时切换，刷新率（0.5s/1s/2s/暂停）与置顶即时生效，最小化时释放 GPU 资源（`0%`）。 |
| **`0.0.3`** | **首个日常可用里程碑（P0 进程管理核心）** | 交付 **进程**（应用/后台/Windows 三段分组 + 父子进程树切换、状态标识、资源值/百分比切换、热力图列、`20×20` 图标与 PE 发布者异步回填）与 **详细信息**（全字段虚拟化表/树），以及核心管控：**运行新任务**（含浏览与管理员勾选）、**结束任务**、自底向上**结束进程树**、**效能模式（EcoQoS）**、优先级、CPU 相关性、打开文件位置/属性/在线搜索与跨页定位。 | 可在 Windows 11 上完成日常进程监控与高负载终止操作，在存在未响应窗口或断连网络盘时不发生启动或运行阻塞。 |
| **`0.0.4`** | 无 WMI/PDH 性能监控视图（P1 上半） | 交付 **性能** 页左栏微缩波形列表与右栏五大硬件详情视图：**CPU**（总体 / 多逻辑核响应式网格、叠加显示内核时间双层 `AreaChart`、动态倍频与 L1–L3 缓存拓扑）、**内存**（60s 曲线、四段比例条、`SMBIOS` 频率与插槽）、**磁盘 0..N**（盘符关联、NVMe/SATA 识别、活动时间与读写速率双图）、**以太网 / Wi-Fi**（自适应量纲吞吐双曲线、IPv4/IPv6）与 **GPU 0..N**（`D3DKMT` `2×2` 多引擎下拉切换与单引擎视图、专享/共享显存曲线、核心温度与 PCI 位置）。 | 全部图表均由 `gpui-kit` `AreaChart`/`LineChart` 组合渲染，无自定义绘制；1s 稳态采样下自身 CPU 占用 `< 0.15%`。 |
| **`0.0.5`** | 系统管理外围视图（P1 下半） | 交付 **服务**（SCM 枚举、PID 与服务组、启动/停止/重启、双向联动详细信息页）、**启动应用**（`上次 BIOS 所用时间`、注册表/启动目录/UWP 聚合、启用/禁用）、**用户**（WTS 会话聚合、展开会话进程树、断开连接）与 **应用历史记录**（累计 CPU 时间与网络统计及清空历史）。 | 覆盖[产品设计与功能盘点](design.zh.md)中全部 8 个页面的 P0 + P1 功能，未激活按需页面无后台轮询开销。 |
| **`0.0.6`** | 内核诊断与基准调优（P2 增强） | 交付 **分析等待链（WCT）**（基于 `Tree` 渲染死锁/阻塞链并支持直接结束阻塞目标）、**创建内存转储文件（MiniDump）**、**UAC 虚拟化**切换、附加调试器、全局唤醒快捷键，并在 100% CPU 负载、未响应窗口与慢速外置盘条件下完成压测与调优。 | 达到工程预算目标（`< 50 ms` 冷启动、`< 0.15%` CPU、`30–45 MB` 私有工作集、`<= 8` 线程）。 |
| **`0.1.0`** | **首个 Windows 稳定版** | 冻结 `loom-config` v1（`CONFIG_CONTRACT_VERSION = "1.0"`）向前迁移承诺与 `loom-pal-api` v1（`PAL_API_CONTRACT_VERSION = 1`）跨平台静态契约；提供 WiX v4/v5 MSI 安装包（`packaging/windows/wix/main.wxs`）、便携 ZIP 打包脚本（`packaging/windows/package-release.ps1`）、SHA-256 校验清单与条件 Authenticode 签名及 `.github/workflows/release.yml` 自动化发布工作流。 | 提供契约稳定的 Windows 正式发布版与自动化 MSI + 便携 ZIP 发行产物。 |
| **`0.2.0`** | Linux 平台后端实现 | 在不改动 `loom-engine` 与 `loom-app` 架构的前提下实现 `loom-pal-linux`（`procfs`、`sysfs`、`netlink`、`drm` GPU、`systemd` 服务与 XDG 自启）。 | Linux 桌面（Wayland / X11）具备日常任务管理与硬件性能监控能力。 |
| **`0.3.0`** | macOS 平台后端实现 | 实现 `loom-pal-macos`（`Mach` task/host info、`libproc`、`IOKit` 磁盘/GPU/SMC 温度、`launchd` 服务与 `SMAppService` 启动项）。 | 覆盖 Windows / Linux / macOS 三个桌面平台。 |

<a id="changelog"></a>

## 变更日志

面向用户的变更在 PR 合并时记入 [CHANGELOG.md](../CHANGELOG.md) 的 `Unreleased`，包括新功能、可感知修复、防卡死与性能改进、平台要求变化以及配置/PAL 契约迁移说明。纯文档、仅 CI 调整或无外部影响的内部重构不写入变更日志。条目按模块分组（`Engine & PAL`、`Windows Backend`、`Performance & Diagnostics`、`UI & UX`、`Config & Localization`、`Breaking & Migration`）。

准备发版时：

1. 对照上一个发布 tag 至今的全部提交核对 `Unreleased`。
2. 将本次变更移至 `## X.Y.Z - YYYY-MM-DD` 标题下，并在顶部保留空的 `## Unreleased` 节。
3. 从 `0.1.0` 起，在版本节末尾附贡献者列表与 `Full Changelog` 对比链接，直接作为 GitHub Release 正文。

<a id="prepare-and-publish"></a>

## 准备与发布

1. **发布 PR**：更新 `[workspace.package].version`（及内部 crate 依赖版本）、刷新 `Cargo.lock`、整理 `CHANGELOG.md` 并同步文档中的版本说明。
2. **CI 与桌面冒烟检查**：确认 `main` 上全部 CI 检查通过（无头 `loom-engine` 与故障注入测试、`#[gpui::test]` 界面测试、Clippy、Rustdoc），并执行桌面冒烟检查（`< 50 ms` 冷启动、托盘常驻与关窗释放显存、进程树终止、硬件波形刷新与按需 UAC 提权）。
3. **Windows 打包与条件签名**：
   - 安装包定义：`packaging/windows/wix/main.wxs`（WiX v4/v5 XML 清单，安装 `loom-app.exe`、`LICENSE` 及开始菜单快捷方式 `Loom Task Manager`，并在完整卸载删除文件前通过提权延迟 CustomAction 自动调用 `--loom-taskmgr-uninstall` 与 `--loom-admin-uninstall` 清理系统集成项）。
   - 打包脚本：`packaging/windows/package-release.ps1` 生成 `loom-<Version>-windows-<Arch>-portable.zip`、`loom-<Version>-windows-<Arch>.msi`（未传 `-SkipMsi` 且 `wix` 可用时）以及 `SHA256SUMS.txt`。
   - 条件 Authenticode 签名：在 GitHub Actions 配置 `WINDOWS_SIGN_PFX_BASE64` 与 `WINDOWS_SIGN_PFX_PASSWORD` Secret（映射为脚本环境变量 `LOOM_SIGN_PFX_BASE64` / `LOOM_SIGN_PFX_PASSWORD`，本地亦可使用 `LOOM_SIGN_CERT_PATH`）。未配置时脚本安全跳过签名而不中断流程；临时解码出的 `.pfx` 文件会在 `finally` 块中覆写归零并删除。
4. **打标与 GitHub Release 发布**：在 `main` 上创建带附注的 `vX.Y.Z` tag 并推送，触发 `.github/workflows/release.yml` 自动完成 Windows 与 Linux 无头校验、Windows 发行打包、提取 `CHANGELOG.md` 对应版本节正文并发布附带 `.msi`、`.zip` 与 `SHA256SUMS.txt` 的 GitHub Release。亦可通过 `workflow_dispatch` 传入 `version` 执行干跑打包验证（不创建 GitHub Release）。
