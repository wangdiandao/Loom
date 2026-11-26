# 文档索引

[English](index.md) | 中文

<a id="product-and-architecture"></a>

## 产品与架构

- [产品设计与功能盘点](design.zh.md)记录对标 Windows 11 自带任务管理器 8 个页面、右键菜单、硬件性能图表与管理对话框的功能盘点、优先级分级（`P0` / `P1` / `P2` / `Drop`）及基于 `gpui-kit` 的原生桌面界面规范。
- [系统架构](architecture.zh.md)定义性能与资源预算、渐进冷启动与防卡死双通道舱壁隔离机制、Cargo Workspace 单向依赖边界、静态 PAL 契约、Windows 采集与控制子系统、UI 垂直功能切片及开源生态复用策略。

<a id="development-and-releases"></a>

## 开发与发布

- [贡献指南](../CONTRIBUTING.zh.md)说明 Issue 提交、性能预算与架构合规、PR 评审与上游协作规范。
- [开发指南](development.zh.md)说明分支约定、模块组织、Windows 防卡死编码规范、测试层级、构建环境与本地检查项。
- [版本与发布](release.zh.md)定义版本号递进规则（`0.0.x` → `0.1.0` → `0.y.z` → `1.0.0`）、按版本号驱动的开发路线、变更日志规则与发布流程。
