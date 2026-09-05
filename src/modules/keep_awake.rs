//! # 系统防休眠模块（阶段二原生落地 · 第二个常驻守护模块）
//!
//! 通过 Win32 电源管理 API [`SetThreadExecutionState`] 注入
//! `ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED` 执行状态，阻止操作系统
//! 自动进入睡眠、阻止显示器因空闲超时熄灭；停止时以 `ES_CONTINUOUS` 还原系统默认的
//! 空闲休眠策略。典型场景：后台长任务（下载 / 渲染 / 值守脚本）运行期间保持机器清醒。
//!
//! ## 与弹窗拦截模块的架构差异：为什么本模块不需要专用原生线程
//!
//! [`PopupBlockerModule`](super::popup_blocker::PopupBlockerModule) 必须把
//! `SetWinEventHook` 与消息泵隔离到专用 OS 线程，因为 WinEvent 回调依赖线程消息队列。
//! 本模块截然不同：
//!
//! 1. `SetThreadExecutionState` 是 **kernel32 的即时内核调用**（微秒级、无阻塞、不触碰
//!    任何消息队列），在 Tokio 工作线程上直接调用完全安全；
//! 2. `ES_CONTINUOUS` 使注入的执行状态**保持粘性**：一次注入即持续生效，直至同一进程
//!    再次以 `ES_CONTINUOUS` 显式清除（实测：全新进程中首次调用返回的前态即携带
//!    `ES_CONTINUOUS` 基线位，arm / clear 往返清晰可观测）——状态归属进程、与具体调用
//!    线程无关，因此 `start` / `stop` 落在哪条 Tokio 工作线程上都作用于同一进程状态；
//! 3. 因此**无需派生任何后台线程 / 任务**：模块「运行」时系统侧没有任何常驻代码在跑，
//!    常驻消耗为零——这正是「常驻零性能消耗」的由来。
//!
//! ## 幂等性与进程级计数安全
//!
//! 模块以 `AtomicBool` 记录运行态，并把每次真实状态变迁与 **恰好一次** Win32 调用配对
//! （`lifecycle` 异步锁串行化并发启停；已在运行态的 `start`、已停止态的 `stop` 直接短路
//! 返回，不触碰 API）。无论 Windows 侧对 `ES_SYSTEM_REQUIRED` / `ES_DISPLAY_REQUIRED`
//! 采用置位还是引用计数语义，本模块的 arm / clear 都严格一一配对，重复 toggle 不会导致
//! 计数漂移或状态错乱。
//!
//! ## 失败语义（与状态机一致性）
//!
//! MSDN 约定：调用失败时返回 `NULL`（即 `EXECUTION_STATE(0)`）；成功时返回前一次执行
//! 状态。经验证成功路径返回值**恒非零**（基线至少含 `ES_CONTINUOUS` 位），因此返回 0
//! 可被无歧义地判定为失败。失败处理遵循「状态机与系统事实一致」原则：
//! - `start` 注入失败 → 保持停止态并上报错误（标志未生效，UI 开关自然回滚）；
//! - `stop` 还原失败 → **保持运行态**并上报错误（防休眠标志仍在生效，若置为停止态将
//!   出现「UI 显示已停止、系统仍被阻止睡眠」的虚假状态，且下次 `start` 会重复注入）。
//!
//! 进程退出兜底：执行状态按进程记账，进程终止时内核自动回收本进程注入的全部标志，
//! 不存在需在 `Drop` 中清理的系统资源（与弹窗模块需 Join 泵线程的收尾链路不同）。
//!
//! # Safety 说明
//!
//! 两处 FFI 调用均为纯标志位传参、无指针载荷，且位于 `unsafe extern` 边界内按
//! 文档签名调用；`SetThreadExecutionState` 对合法标志组合不存在可导致进程级未定义
//! 行为的路径。

use super::{ModuleError, ToolModule};
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

#[cfg(windows)]
use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
    EXECUTION_STATE,
};

/// 激活时注入的执行状态：持续生效 + 阻止系统睡眠 + 阻止显示器熄灭。
///
/// 以各命名常量的 `.0` 位域在 const 上下文中组合（`BitOr` 为非常量 trait 方法，
/// 不能用于常量初始化）。
#[cfg(windows)]
const AWAKE_FLAGS: EXECUTION_STATE =
    EXECUTION_STATE(ES_CONTINUOUS.0 | ES_SYSTEM_REQUIRED.0 | ES_DISPLAY_REQUIRED.0);

/// 还原时注入的执行状态：仅 `ES_CONTINUOUS`，恢复操作系统默认的空闲休眠策略。
#[cfg(windows)]
const RESTORE_FLAGS: EXECUTION_STATE = ES_CONTINUOUS;

/// 系统防休眠模块。
///
/// 生命周期完全由内部可变性管理（`AtomicBool` 运行标志 + 异步锁串行化变迁），
/// 仅向调度器 / UI 暴露共享引用接口，天然满足 [`ToolModule`] 的 `Send + Sync` 契约。
#[derive(Clone)]
pub struct KeepAwakeModule {
    inner: Arc<KeepAwakeInner>,
}

/// 模块内部并发状态。
struct KeepAwakeInner {
    /// 串行化 `start` / `stop` 生命周期变迁，杜绝并发启停互相穿插。
    lifecycle: AsyncMutex<()>,
    /// 快速查询的运行标志（无锁读取路径，供 UI 高频轮询）。
    running: AtomicBool,
}

impl KeepAwakeModule {
    /// 构造一个尚未启动的防休眠模块。
    pub fn new() -> Self {
        Self {
            inner: Arc::new(KeepAwakeInner {
                lifecycle: AsyncMutex::new(()),
                running: AtomicBool::new(false),
            }),
        }
    }

    /// Windows 实现：注入阻止睡眠与熄屏的执行状态（幂等，仅真实变迁调用一次 API）。
    #[cfg(windows)]
    async fn start_native(&self) -> Result<(), ModuleError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        if self.inner.running.load(Ordering::Acquire) {
            return Ok(()); // 已在运行：幂等短路，不重复注入
        }

        // SAFETY: 纯标志位 FFI 调用，无指针载荷；参数为文档定义的合法组合。
        let previous = unsafe { SetThreadExecutionState(AWAKE_FLAGS) };
        if previous.0 == 0 {
            // 注入失败（MSDN：失败返回 NULL）：标志未生效，保持停止态并上报。
            return Err("SetThreadExecutionState 注入防休眠标志失败（返回空前态）".into());
        }

        self.inner.running.store(true, Ordering::Release);
        tracing::info!(
            target: "keep_awake",
            "系统防休眠已激活（ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED，上一执行状态 0x{:08X}）",
            previous.0
        );
        Ok(())
    }

    /// 非 Windows 兜底实现：仅翻转虚拟运行标志，供跨平台编译与调度联调。
    #[cfg(not(windows))]
    async fn start_virtual(&self) -> Result<(), ModuleError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        if self.inner.running.load(Ordering::Acquire) {
            return Ok(()); // 已在运行：幂等
        }
        self.inner.running.store(true, Ordering::Release);
        tracing::warn!(
            target: "keep_awake",
            "当前系统非 Windows 平台：系统防休眠模块仅维持虚拟生命周期"
        );
        Ok(())
    }

    /// 停止模块的公共实现（Windows / 非 Windows 共用，内部以 cfg 区分）。
    async fn stop_impl(&self) -> Result<(), ModuleError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        if !self.inner.running.load(Ordering::Acquire) {
            return Ok(()); // 未在运行：幂等短路，不重复还原
        }

        #[cfg(windows)]
        {
            // SAFETY: 纯标志位 FFI 调用，无指针载荷；ES_CONTINUOUS 为文档定义的还原标志。
            let previous = unsafe { SetThreadExecutionState(RESTORE_FLAGS) };
            if previous.0 == 0 {
                // 还原失败：防休眠标志仍由系统持有。保持运行态使状态机与系统事实一致，
                // 避免「UI 显示已停止、系统仍被阻止睡眠」的虚假状态。
                return Err("SetThreadExecutionState 还原默认休眠策略失败（返回空前态）".into());
            }
        }

        self.inner.running.store(false, Ordering::Release);
        tracing::info!(
            target: "keep_awake",
            "系统防休眠已还原（恢复操作系统默认空闲休眠策略）"
        );
        Ok(())
    }
}

impl Default for KeepAwakeModule {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolModule for KeepAwakeModule {
    fn id(&self) -> &'static str {
        "keep_awake"
    }

    fn display_name(&self) -> &'static str {
        "系统防休眠"
    }

    fn description(&self) -> &'static str {
        "阻止显示器关闭与系统自动睡眠，保持后台任务持续运行"
    }

    async fn start(&self) -> Result<(), ModuleError> {
        #[cfg(windows)]
        {
            self.start_native().await
        }
        #[cfg(not(windows))]
        {
            self.start_virtual().await
        }
    }

    async fn stop(&self) -> Result<(), ModuleError> {
        self.stop_impl().await
    }

    fn is_running(&self) -> bool {
        self.inner.running.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 元数据契约：注册表键 / 展示名 / 描述必须与装配层与 UI 的约定一致。
    #[test]
    fn metadata_identity_matches_registry_contract() {
        let module = KeepAwakeModule::new();
        assert_eq!(module.id(), "keep_awake");
        assert_eq!(module.display_name(), "系统防休眠");
        assert_eq!(
            module.description(),
            "阻止显示器关闭与系统自动睡眠，保持后台任务持续运行"
        );
        assert!(!module.is_running(), "新模块应处于停止态");
    }

    /// 生命周期幂等性验证：重复 `start` / `stop` 不得重复注入 / 重复还原，
    /// 状态机多轮收敛且无错乱（真实变迁与 API 调用严格一一配对）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_start_stop_is_idempotent_and_converges() {
        let module = KeepAwakeModule::new();
        assert!(!module.is_running());

        for cycle in 1..=3 {
            module.start().await.expect("启动应成功");
            assert!(module.is_running(), "第 {cycle} 轮启动后应处于运行态");

            module.start().await.expect("重复启动应幂等成功");
            assert!(module.is_running(), "重复启动不得破坏运行态");

            module.stop().await.expect("停止应成功");
            assert!(!module.is_running(), "第 {cycle} 轮停止后应退出运行态");

            module.stop().await.expect("重复停止应幂等成功");
            assert!(!module.is_running(), "重复停止不得破坏停止态");
        }
    }

    /// 状态流转验证：新建 → 启动 →（重复启动）→ 停止 →（重复停止）→ 再启动，
    /// 每一步后 `is_running` 与目标状态严格一致。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn state_transitions_follow_start_stop_semantics() {
        let module = KeepAwakeModule::new();

        // 停止态下 stop 为无操作（幂等），状态保持不变。
        module.stop().await.expect("停止态下重复停止应幂等成功");
        assert!(!module.is_running());

        // 启动 → 运行态。
        module.start().await.expect("首次启动应成功");
        assert!(module.is_running());

        // 停止 → 停止态。
        module.stop().await.expect("停止应成功");
        assert!(!module.is_running());

        // 再次启动 → 再次运行态（模块可反复启停）。
        module.start().await.expect("再次启动应成功");
        assert!(module.is_running());

        // 收尾还原，避免测试进程残留防休眠标志。
        module.stop().await.expect("收尾停止应成功");
        assert!(!module.is_running());
    }

    /// 并发调度验证：多个任务同时发起 start / stop，生命周期锁应保证串行收敛，
    /// 最终状态一致且无死锁（真实 API 调用次数被约束为每轮变迁恰好一次）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_lifecycle_operations_are_serialized() {
        let module = KeepAwakeModule::new();

        let mut starters = Vec::new();
        for _ in 0..4 {
            let m = module.clone();
            starters.push(tokio::spawn(async move { m.start().await }));
        }
        for handle in starters {
            handle
                .await
                .expect("并发 start 任务应正常结束")
                .expect("并发 start 应成功");
        }
        assert!(module.is_running(), "并发启动后应收敛到运行态");

        let mut stoppers = Vec::new();
        for _ in 0..4 {
            let m = module.clone();
            stoppers.push(tokio::spawn(async move { m.stop().await }));
        }
        for handle in stoppers {
            handle
                .await
                .expect("并发 stop 任务应正常结束")
                .expect("并发 stop 应成功");
        }
        assert!(!module.is_running(), "并发停止后应收敛到停止态");
    }
}
