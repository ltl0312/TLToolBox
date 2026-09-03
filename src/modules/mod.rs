//! # 模块系统契约层（阶段一重构 · 架构转型后）
//!
//! TLToolBox 转型为「常驻守护 + 开关控制」的轻量原生 Windows 工具箱后，本文件是
//! 系统服务级插件（Daemon Module）的底层契约层。阶段二引入的调度管理器
//! （`crate::manager::ModuleManager`）仅通过本契约的共享引用接口调度模块，
//! 杜绝独占可变借用。
//!
//! # 线程安全模型
//!
//! 历史版本中 `start(&mut self)` / `stop(&mut self)` 依赖独占可变借用，迫使调度器在
//! `Arc<Mutex<ModuleManager>>` 全局锁保护下遍历并调度模块：一旦某模块的启动/自检过程
//! 耗时较长，全局互斥锁将被长时间持有，Slint UI 的状态拉取与事件回调随之完全阻塞，
//! 引发界面掉帧甚至冻结。
//!
//! 自阶段一起，契约全面收敛为共享借用（`&self`），并强制 `Send + Sync` 超特征约束：
//! 实现者必须通过**内部可变性**（`AtomicBool`、`tokio::sync::RwLock`/`Mutex` 等并发
//! 原语）管理并发生命周期，使元数据查询、健康状态拉取与并发启停调度互不阻塞。

use async_trait::async_trait;
use std::error::Error;

/// 模块生命周期操作统一的错误类型。
pub type ModuleError = Box<dyn Error + Send + Sync>;

/// 系统级模块契约。
///
/// 所有实现必须满足：
/// 1. **共享借用调度**：`start` / `stop` 均接受 `&self`，内部状态一律使用并发原语封装；
/// 2. **线程安全**：实现类型必须是 `Send + Sync`，可被 `Arc` 包装后在多线程与
///    并发调度场景中安全共享；
/// 3. **幂等性**：`start` 与 `stop` 应可重复调用——已在运行时的 `start`、未在运行时的
///    `stop` 均须直接返回 `Ok(())`，不得产生资源泄漏或状态错乱。
#[async_trait]
pub trait ToolModule: Send + Sync {
    /// 模块唯一标识符（注册表键，如 `"popup_blocker"`）。
    fn id(&self) -> &'static str;

    /// 模块人类可读名称（用于 UI 列表展示）。
    fn display_name(&self) -> &'static str;

    /// 模块功能描述（用于 UI 展示与调试诊断）。
    fn description(&self) -> &'static str;

    /// 启动模块。实现内部负责派生所需的原生线程 / 异步任务并完成资源就绪握手；
    /// 幂等：若已在运行态则直接返回 `Ok(())`。
    async fn start(&self) -> Result<(), ModuleError>;

    /// 停止模块。实现内部负责向运行载体发送停机信号并等待其完成资源释放；
    /// 幂等：若已处于停止态则直接返回 `Ok(())`。
    async fn stop(&self) -> Result<(), ModuleError>;

    /// 查询模块当前运行状态（无锁快速路径，供 UI / 调度器高频轮询）。
    fn is_running(&self) -> bool;
}

/// Win32 弹窗拦截模块（阶段一原生落地实现）。
pub mod popup_blocker;
