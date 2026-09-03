//! # TLToolBox 库入口（架构转型：轻量级原生 Windows 工具箱）
//!
//! TLToolBox 已自「大模型 Agent 工作站」转型为**专注桌面常驻与开关控制的
//! 轻量级原生 Windows 工具箱**：LLM / HTTP（agent 层与 reqwest）整体退役，
//! 不再存在网络依赖。工程维持「库 + 二进制」双目标：
//!
//! - **库（本文件）**：承载全部本地业务基础设施——注册表开机自启（[`autostart`]）、
//!   事件总线（[`bus`]）、配置引擎（[`config`]）、模块调度器（[`manager`]）、
//!   系统托盘与常驻生命周期（[`tray`]）与常驻守护模块（[`modules`]）。`tests/` 下的集成测试与 `src/main.rs`
//!   可执行程序都以 `tltoolbox` 为依赖，确保 `cargo test` 能在**脱离 GUI
//!   会话**的前提下验证模块调度与配置/自启链路；
//! - **二进制（`src/main.rs`）**：最终装配点——加载配置并同步注册表自启状态、
//!   实例化 Slint 界面（`ui/app.slint`，单栏「桌面实用工具箱」：顶部全局控制栏 +
//!   模块开关卡片流，无内嵌日志面板），把本库的异步基础设施经
//!   `slint::invoke_from_event_loop` 桥接到 UI 主线程消息循环（详见其模块文档）。
//!
//! 模块树刻意保持扁平，与历史 crate 内布局一致（`crate::xxx` 相对路径不因
//! 转型而改变），便于增量迁移。

pub mod autostart;
pub mod bus;
pub mod config;
pub mod manager;
pub mod modules;
pub mod tray;
