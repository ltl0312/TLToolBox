//! # 生产级 Win32 弹窗拦截模块
//!
//! ## 与 Tokio 的隔离（本模块的第一性原则）
//!
//! 使用 `WINEVENT_OUTOFCONTEXT` 跨进程监听系统级 UI 事件时，操作系统会把事件路由到
//! **安装钩子的线程所拥有的消息队列**，并由该线程运行的 Win32 消息泵
//! （`GetMessageW` / `DispatchMessageW`）派发回调。Tokio 工作线程是无栈协程调度载体，
//! 本身不运行标准 Win32 消息循环，因此在 Tokio 协程中直接挂接 / 轮询 WinEvent 回调
//! **永远不会被触发**。
//!
//! 本模块因此将 `SetWinEventHook` 与消息泵整体隔离到一条由 `std::thread` 派生的
//! **专用操作系统原生线程**（命名 `win32-popup-hook-pump`）中运行；Tokio 侧只负责
//! 生命周期编排（启停请求的收发与结果确认），绝不触碰 Win32 事件监听。
//!
//! ## 平滑卸载协议
//!
//! 卸载链路必须保证 `UnhookWinEvent`（要求与 `SetWinEventHook` 处于同一线程）一定执行，
//! 杜绝系统级事件钩子与消息泵线程泄漏：
//!
//! 1. `stop()` 先 `cancel()` [`CancellationToken`] 广播停机意图；
//! 2. 再向泵线程定向投递 `PostThreadMessageW(WM_QUIT)`，唤醒阻塞在 `GetMessageW`
//!    中的泵线程（退出码 0，循环终止）；
//! 3. 泵线程退出循环后**在同一线程上**调用 `UnhookWinEvent`，随后线程自然结束；
//! 4. `stop()` 通过 Join 该原生线程并施加超时，确保卸载动作**已经发生**才向调用方返回。
//!
//! ## 竞态防护
//!
//! `PostThreadMessageW` 要求目标线程**已经建立消息队列**，否则调用失败导致停机信号
//! 丢失、泵线程永久挂起。为此泵线程在回报自身线程 ID 之前，先以
//! `PeekMessageW(PM_NOREMOVE)` 强制建立队列，再与父侧完成一次性握手；握手成功后队列
//! 必然存在，任何后续投递的 `WM_QUIT` 都不会丢失。

use super::{ModuleError, ToolModule};
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
use tokio::sync::oneshot;

#[cfg(windows)]
use windows::Win32::{
    Foundation::{HMODULE, HWND, LPARAM, WPARAM},
    UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK},
    UI::WindowsAndMessaging::{
        DispatchMessageW, GetClassNameW, GetMessageW, GetWindowTextW, PeekMessageW, PostMessageW,
        PostThreadMessageW, TranslateMessage, EVENT_OBJECT_CREATE, MSG, PM_NOREMOVE,
        WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_CLOSE, WM_QUIT,
    },
};

#[cfg(windows)]
use windows::Win32::System::Threading::GetCurrentThreadId;

/// 停机协议中 Join 原生泵线程的等待上限。
///
/// 收到 `WM_QUIT` 后泵线程应立即退出；仅在极端场景（回调正阻塞于跨进程取窗口文本的
/// 系统内部超时窗口）下可能略慢，故设置宽松上限。
#[cfg(windows)]
const PUMP_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// 命中即关闭的目标窗口标题 / 类名黑名单（匹配窗口标题与窗口类名）。
#[cfg(windows)]
const BLACKLIST_PATTERNS: &[&str] = &["广告", "Flash Helper Service", "Update Notice", "推广弹窗"];

/// WinEvent 系统回调的裸函数指针类型（与 `WINEVENTPROC` 载荷一致）。
///
/// 显式声明以便对关联函数做 `as` 转换，规避泛型推断歧义。
#[cfg(windows)]
type WinEventCallback = unsafe extern "system" fn(HWINEVENTHOOK, u32, HWND, i32, i32, u32, u32);

/// Win32 弹窗拦截模块。
///
/// 生命周期完全由内部可变性管理（`AtomicBool` + 内部锁），对调度器 / UI 仅暴露
/// 共享引用接口，天然满足 [`ToolModule`] 的 `Send + Sync` 契约。
#[derive(Clone)]
pub struct PopupBlockerModule {
    inner: Arc<PopupBlockerInner>,
}

/// 模块内部并发状态。
struct PopupBlockerInner {
    /// 串行化 `start` / `stop` 生命周期变迁，杜绝并发启停互相穿插。
    lifecycle: AsyncMutex<()>,
    /// 快速查询的运行标志（无锁读取路径，供 UI 高频轮询）。
    running: AtomicBool,
    /// 当前活动运行上下文（一次运行仅对应一条泵线程）。
    active: StdMutex<Option<ActiveRun>>,
}

/// 单次运行（泵线程）的运行时上下文。
struct ActiveRun {
    /// 停机广播令牌：`cancel()` 即请求退出。
    cancel: CancellationToken,
    #[cfg(windows)]
    /// 泵线程线程 ID（供 `PostThreadMessageW(WM_QUIT)` 定向投递）。
    thread_id: u32,
    #[cfg(windows)]
    /// 泵线程 Join 句柄（`stop()` 借此确认 `UnhookWinEvent` 已执行完毕）。
    thread: std::thread::JoinHandle<()>,
}

impl PopupBlockerModule {
    /// 构造一个尚未启动的弹窗拦截模块。
    pub fn new() -> Self {
        Self {
            inner: Arc::new(PopupBlockerInner {
                lifecycle: AsyncMutex::new(()),
                running: AtomicBool::new(false),
                active: StdMutex::new(None),
            }),
        }
    }

    /// WinEvent 事件回调：由泵线程在 `DispatchMessageW` 派发阶段被系统调用。
    ///
    /// # Safety / 约束
    /// - 必须与 `WINEVENTPROC` 布局一致（`unsafe extern "system"`）；
    /// - 运行于专用原生线程的系统回调上下文，**严禁**在其中执行任何异步 / Tokio
    ///   操作，也禁止可能 `panic` / 跨 FFI 边界展开的代码。
    #[cfg(windows)]
    unsafe extern "system" fn win_event_proc(
        _hook: HWINEVENTHOOK,
        event: u32,
        hwnd: HWND,
        id_object: i32,
        id_child: i32,
        _event_thread: u32,
        _event_time: u32,
    ) {
        // OBJID_WINDOW == 0 && CHILDID_SELF == 0：只关心窗口本体（而非子元素/子对象）的创建。
        if event != EVENT_OBJECT_CREATE || hwnd.0.is_null() || id_object != 0 || id_child != 0 {
            return;
        }

        // SAFETY: 以下 Win32 查询均为进程内只读调用，hwnd 可能已失效（窗口刚销毁），
        // 但 GetClassNameW / GetWindowTextW 对失效句柄返回 0，不会引发访问违规。
        unsafe {
            let mut class_buf = [0u16; 256];
            let mut title_buf = [0u16; 256];
            let class_len = GetClassNameW(hwnd, &mut class_buf).max(0) as usize;
            let title_len = GetWindowTextW(hwnd, &mut title_buf).max(0) as usize;

            let class_name = String::from_utf16_lossy(&class_buf[..class_len.min(class_buf.len())]);
            let title = String::from_utf16_lossy(&title_buf[..title_len.min(title_buf.len())]);

            // 拉丁字母模式做大小写不敏感匹配；中文模式直接包含匹配。
            let matched = BLACKLIST_PATTERNS.iter().any(|pattern| {
                if pattern.is_ascii() {
                    let needle = pattern.to_ascii_lowercase();
                    title.to_ascii_lowercase().contains(&needle)
                        || class_name.to_ascii_lowercase().contains(&needle)
                } else {
                    title.contains(pattern) || class_name.contains(pattern)
                }
            });
            if !matched {
                return;
            }

            tracing::warn!(
                target: "popup_blocker",
                "捕获目标弹窗: 标题=\"{title}\", 类名=\"{class_name}\", HWND=0x{:X}; 下发 WM_CLOSE 关闭指令",
                hwnd.0 as usize,
            );
            // 关闭指令为异步消息投递，不等待目标窗口处理，杜绝在系统回调中阻塞。
            let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }

    /// 泵线程主体：运行于专用操作系统原生线程，承载钩子安装与标准 Win32 消息泵。
    ///
    /// 线程退出前必须完成 `UnhookWinEvent`（钩子只能由安装线程卸载）。
    #[cfg(windows)]
    fn pump_thread_main(ready_tx: oneshot::Sender<Result<u32, String>>) {
        // SAFETY:
        // - 本函数整体运行在由 `std::thread::Builder::spawn` 派生的专用原生线程中；
        // - `PeekMessageW` 仅用于建立本线程消息队列（取不到消息也无副作用）；
        // - `SetWinEventHook`（WINEVENT_OUTOFCONTEXT）将回调挂载到本线程消息队列，
        //   由紧随其后的 `GetMessageW` / `DispatchMessageW` 标准消息泵承载派发；
        // - 退出路径在同线程调用 `UnhookWinEvent`，满足钩子卸载的线程亲和约束。
        unsafe {
            // 1) 强制建立本线程消息队列。
            //    `PostThreadMessageW` 只接受“已建队列”的线程；先建队再回报线程 ID，
            //    从根上消除“WM_QUIT 早于队列建立而投递失败”的竞态。
            let mut seed = MSG::default();
            let _ = PeekMessageW(&mut seed, None, 0, 0, PM_NOREMOVE);

            // 2) 在本线程安装系统级 UI 事件钩子（监听 EVENT_OBJECT_CREATE）。
            //    回调内置于本进程代码，无需按模块句柄定位 DLL，故传 NULL。
            let callback_module: Option<&HMODULE> = None;
            let hook = SetWinEventHook(
                EVENT_OBJECT_CREATE,
                EVENT_OBJECT_CREATE,
                callback_module,
                Some(Self::win_event_proc as WinEventCallback),
                0, // idProcess = 0：监听所有进程
                0, // idThread  = 0：监听所有线程
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );

            if hook.0.is_null() {
                let _ = ready_tx.send(Err(
                    "SetWinEventHook 安装失败：句柄为空（可能缺少必要权限或处于无桌面会话）"
                        .to_string(),
                ));
                return;
            }

            let thread_id = GetCurrentThreadId();

            // 3) 与父侧握手。若父侧已放弃等待（oneshot 关闭 / future 被取消），
            //    立即卸载钩子并退出，绝不遗留无主事件钩子。
            if ready_tx.send(Ok(thread_id)).is_err() {
                let _ = UnhookWinEvent(hook);
                return;
            }

            tracing::info!(
                target: "popup_blocker",
                "Win32 原生事件钩子已就绪（泵线程 {thread_id}），进入消息循环"
            );

            // 4) 标准 Win32 消息泵：GetMessageW 取到 WM_QUIT 时返回 FALSE（0），循环退出。
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                if msg.message == WM_QUIT {
                    // 显式防御：正常情况下 WM_QUIT 已令 GetMessageW 返回 FALSE。
                    break;
                }
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageW(&msg);
            }

            // 5) 泵退出后在同一线程安全卸载钩子，随后线程自然结束。
            let _ = UnhookWinEvent(hook);
            tracing::info!(target: "popup_blocker", "Win32 原生事件钩子已安全卸载，泵线程退出");
        }
    }

    /// Windows 实现：派生专用原生线程运行钩子消息泵。
    #[cfg(windows)]
    async fn start_native(&self) -> Result<(), ModuleError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        if self.inner.running.load(Ordering::Acquire) {
            return Ok(()); // 已在运行：幂等
        }

        let cancel = CancellationToken::new();
        let (ready_tx, ready_rx) = oneshot::channel::<Result<u32, String>>();

        // 派生专用操作系统原生线程承载钩子与消息泵（严禁放置于 Tokio 协程中）。
        let thread = std::thread::Builder::new()
            .name("win32-popup-hook-pump".to_string())
            .spawn(move || Self::pump_thread_main(ready_tx))
            .map_err(|e| -> ModuleError { Box::new(e) })?;

        // 等待原生线程完成“建队列 + 装钩子”握手，安装失败则回收线程并上报。
        let thread_id = match ready_rx.await {
            Ok(Ok(thread_id)) => thread_id,
            Ok(Err(reason)) => {
                // 线程已自行退出；Join 仅回收句柄，随后向调用方暴露失败原因。
                let _ = thread.join();
                return Err(reason.into());
            }
            Err(_) => {
                // 通道关闭：泵线程检测到父侧放弃后已自行卸载退出，此处回收句柄。
                let _ = thread.join();
                return Err("弹窗拦截模块启动握手被中断".into());
            }
        };

        self.inner.running.store(true, Ordering::Release);
        *self.active_lock()? = Some(ActiveRun {
            cancel,
            thread_id,
            thread,
        });

        tracing::info!(target: "popup_blocker", "弹窗拦截模块已启动（泵线程 {thread_id}）");
        Ok(())
    }

    /// 非 Windows 兜底实现：仅维持虚拟生命周期，供跨平台编译与调度联调。
    #[cfg(not(windows))]
    async fn start_virtual(&self) -> Result<(), ModuleError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        if self.inner.running.load(Ordering::Acquire) {
            return Ok(()); // 已在运行：幂等
        }

        let cancel = CancellationToken::new();
        let token = cancel.clone();
        tokio::spawn(async move {
            tracing::warn!(
                target: "popup_blocker",
                "当前系统非 Windows 平台：弹窗拦截模块仅维持虚拟生命周期"
            );
            token.cancelled().await;
            tracing::debug!(target: "popup_blocker", "虚拟生命周期已结束");
        });

        self.inner.running.store(true, Ordering::Release);
        *self.active_lock()? = Some(ActiveRun { cancel });

        tracing::info!(target: "popup_blocker", "弹窗拦截模块已启动（虚拟生命周期）");
        Ok(())
    }

    /// 取得活动上下文锁（将“锁中毒”这类异常状态显式上报为模块错误）。
    fn active_lock(&self) -> Result<std::sync::MutexGuard<'_, Option<ActiveRun>>, ModuleError> {
        self.inner
            .active
            .lock()
            .map_err(|_| -> ModuleError { "popup_blocker: 活动上下文锁中毒".into() })
    }

    /// 停止模块的公共实现（Windows / 非 Windows 共用，内部以 cfg 区分）。
    async fn stop_impl(&self) -> Result<(), ModuleError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        if !self.inner.running.load(Ordering::Acquire) {
            return Ok(()); // 未在运行：幂等
        }

        let run = self.active_lock()?.take();
        let Some(run) = run else {
            // running 与 active 不一致（理论不可达）：保守复位后返回。
            self.inner.running.store(false, Ordering::Release);
            return Ok(());
        };

        // 1) 广播停机意图（Drop 兜底路径与未来的外部取消均复用该令牌）。
        run.cancel.cancel();

        #[cfg(windows)]
        {
            // 2) 向泵线程消息队列定向投递 WM_QUIT，唤醒阻塞中的 GetMessageW。
            //    队列在握手阶段已建立，此处投递必然成功。
            // SAFETY: WM_QUIT 为已定义消息常量，参数类型匹配；投递失败仅返回错误码，无 UB。
            let _ = unsafe { PostThreadMessageW(run.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };

            // 3) Join 泵线程并施加超时：确保 UnhookWinEvent 已在安装线程上执行完毕。
            //    三层 Result 展开：timeout → spawn_blocking JoinHandle → 线程 join。
            let join_result = tokio::time::timeout(
                PUMP_JOIN_TIMEOUT,
                tokio::task::spawn_blocking(move || run.thread.join()),
            )
            .await;

            match join_result {
                Ok(Ok(Ok(()))) => {
                    tracing::debug!(target: "popup_blocker", "泵线程已退出，事件钩子已卸载");
                }
                Ok(Ok(Err(panic))) => {
                    // std 线程 join 的 panic 载荷为 Box<dyn Any + Send>，需 downcast 取可读文本。
                    let payload = panic
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "未知 panic 载荷".to_string());
                    tracing::error!(target: "popup_blocker", "泵线程异常终止: {payload}");
                    self.inner.running.store(false, Ordering::Release);
                    return Err(format!("弹窗拦截泵线程异常终止: {payload}").into());
                }
                Ok(Err(task_err)) => {
                    tracing::error!(target: "popup_blocker", "Join 阻塞任务异常终止: {task_err}");
                    self.inner.running.store(false, Ordering::Release);
                    return Err(format!("弹窗拦截 Join 阻塞任务异常终止: {task_err}").into());
                }
                Err(_elapsed) => {
                    // 极端场景（回调阻塞于跨进程窗口查询的系统超时窗口内）：泵线程转入
                    // 分离式收尾——WM_QUIT 已投递，UnhookWinEvent 最终仍会在该线程执行。
                    tracing::error!(
                        target: "popup_blocker",
                        "泵线程未在 {PUMP_JOIN_TIMEOUT:?} 内退出，已转入分离式收尾"
                    );
                }
            }
        }

        self.inner.running.store(false, Ordering::Release);
        Ok(())
    }
}

impl Default for PopupBlockerModule {
    fn default() -> Self {
        Self::new()
    }
}

/// 兜底清理：即使调用方遗忘 `stop()`，实例消亡时也要唤醒泵线程，确保
/// `UnhookWinEvent` 最终在安装线程上执行（JoinHandle 随 `run` 析构而分离，不阻塞当前线程）。
impl Drop for PopupBlockerInner {
    fn drop(&mut self) {
        let run = match self.active.try_lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => return, // 极端的并发消亡场景：放弃兜底，交由进程退出统一回收。
        };
        let Some(run) = run else { return };

        run.cancel.cancel();
        #[cfg(windows)]
        unsafe {
            let _ = PostThreadMessageW(run.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        // `run` 在此析构：Windows 平台下 JoinHandle 被丢弃 = 线程分离，
        // 泵线程退出 GetMessageW 循环后自行执行 UnhookWinEvent。
    }
}

#[async_trait]
impl ToolModule for PopupBlockerModule {
    fn id(&self) -> &'static str {
        "popup_blocker"
    }

    fn display_name(&self) -> &'static str {
        "桌面弹窗拦截"
    }

    fn description(&self) -> &'static str {
        "通过 Win32 原生事件钩子毫秒级拦截广告弹窗与流氓进程窗口"
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

    /// 生命周期幂等性验证：重复 `start` / `stop` 不得产生线程 / 钩子泄漏。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_start_stop_is_idempotent_and_converges() {
        let module = PopupBlockerModule::new();
        assert!(!module.is_running());

        for cycle in 1..=3 {
            module.start().await.expect("启动应成功");
            assert!(module.is_running(), "第 {cycle} 轮启动后应处于运行态");

            module.start().await.expect("重复启动应幂等成功");
            assert!(module.is_running());

            module.stop().await.expect("停止应成功");
            assert!(!module.is_running(), "第 {cycle} 轮停止后应退出运行态");

            module.stop().await.expect("重复停止应幂等成功");
            assert!(!module.is_running());
        }
    }

    /// 并发调度验证：多个任务同时发起 start / stop，生命周期锁应保证串行收敛，
    /// 全程只存在一条泵线程，最终状态一致且无死锁。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_lifecycle_operations_are_serialized() {
        let module = PopupBlockerModule::new();

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
        assert!(module.is_running());

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
        assert!(!module.is_running());
    }
}
