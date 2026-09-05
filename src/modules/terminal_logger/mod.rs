//! # 终端交互日志子系统（基础层 · 阶段一）
//!
//! 目标：为交互式 Shell 会话（PowerShell / cmd / bash）注入会话日志钩子，把
//! 用户在终端里执行的命令与输出沉淀为本地文件，供后续回看 / 审计使用。
//!
//! 本子系统按「基础层 → 装配层」两阶段推进，当前交付**基础层**：
//!
//! - [`anchor`]：通用**无损文本锚点插拔引擎**——以带 TAG 的成对标记行在
//!   Shell 配置文件中圈出 TLToolBox 管理的区块，支持注入 / 更新 / 彻底移除，
//!   且不破坏文件其余任何用户内容（详见模块内文档）；
//! - 配置侧：[`crate::config::AppConfig`] 新增 `terminal_log_dir` /
//!   `enabled_shells` 字段（缺省键回退默认值，见
//!   [`crate::config::AppConfig::effective_terminal_log_dir`]）。
//!
//! 后续阶段将在此基础上实现各 Shell 的钩子装配策略（PowerShell `$PROFILE`、
//! PSReadLine 历史钩子等）与 [`crate::modules::ToolModule`] 化的生命周期接入，
//! 使本子系统成为第四个常驻守护模块。

pub mod anchor;

pub use anchor::{block_end_marker, block_start_marker, inject_block, remove_block};
