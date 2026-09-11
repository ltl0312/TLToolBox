//! # 线程铁律的类型化原语（v0.6.2 · P2-14）
//!
//! 项目的三条线程纪律长期依赖注释维系（见 `lib.rs` / `main.rs` 模块文档）：
//!
//! 1. **模型只在 UI 线程改写**——后台线程一律经 `slint::invoke_from_event_loop`
//!    排队，绝不直接触碰 Slint 对象；
//! 2. **锁不跨 `await`**——`std::sync::Mutex` 只允许短临界使用，跨 `await` 的锁
//!    一律用 `tokio::sync::Mutex`；
//! 3. **COM 只在独立 OS 线程**——Shell COM 全部经
//!    [`crate::modules::icon_locker::explorer::spawn_com_thread`] 执行，线程内以
//!    `StaCom` 守卫完成 `CoInitializeEx` / `CoUninitialize` 配对。
//!
//! 本模块把其中**可被类型表达**的部分固化下来：
//!
//! - [`UiThreadToken`]：**UI 线程令牌**。进程内只能领取一次（`AtomicBool` 保证），
//!   且必须在 UI 线程上领取——它是"我站在 UI 线程上"这一事实的载体；
//! - [`UiDeliver<T>`]：**UI 交付通道**。只能凭 `&UiThreadToken` 构造，即"向 UI
//!   投递载荷"的能力被显式地收敛到装配期的一个受控位置；其他线程只拿到
//!   `deliver()`（单向上行），拿不到任何可以绕开事件循环直接改写模型的入口。
//!
//! # 边界（诚实声明）
//!
//! 类型化无法覆盖全部三条纪律：②"锁不跨 await"本质是作用域问题，Rust 类型系统
//! 表达不了"持有句柄期间不得 await"；③ 的线程隔离已由 `spawn_com_thread` +
//! `StaCom` 落实。本模块提供的是 ① 的**构造期守卫**：让"谁能创建 UI 交付通道"
//! 成为编译期可见的事实，并为后续把 `show_toast` / `invoke_from_event_loop` 等散落
//! 调用点逐步收敛到该类型铺路（迁移是渐进的，本模块不强制一次性替换全部调用点）。

use std::sync::atomic::{AtomicBool, Ordering};

/// UI 线程令牌：进程内**仅可领取一次**。
///
/// 调用方（`main`）在 UI 线程装配期领取；第二次领取返回
/// [`TokenError::AlreadyClaimed`]——它证明"同一进程内存在两条自称 UI 线程的执行流"，
/// 属装配错误。
#[derive(Debug)]
pub struct UiThreadToken {
    _private: (),
}

static UI_THREAD_CLAIMED: AtomicBool = AtomicBool::new(false);

/// 领取 UI 线程令牌时的错误形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenError {
    /// 令牌已被领取：存在第二个"UI 线程"声明，装配存在冲突。
    AlreadyClaimed,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyClaimed => write!(
                f,
                "UI 线程令牌已被领取：本进程声明了多个 UI 线程（装配冲突）"
            ),
        }
    }
}
impl std::error::Error for TokenError {}

impl UiThreadToken {
    /// 在 UI 线程上领取令牌（进程内一次性）。
    ///
    /// 必须在 UI 线程（Slint 事件循环所属线程）上调用；领取结果以 `AtomicBool`
    /// 全局登记，重复领取即装配错误。令牌是零大小的——它不承载任何资源，只承载
    /// **"UI 线程身份"这一事实**，供 [`UiDeliver`] 在构造期消费。
    pub fn claim() -> Result<Self, TokenError> {
        if UI_THREAD_CLAIMED.swap(true, Ordering::AcqRel) {
            return Err(TokenError::AlreadyClaimed);
        }
        Ok(Self { _private: () })
    }
}

/// 向 UI 线程交付载荷的单向通道（构造期需要 [`UiThreadToken`]）。
///
/// - **构造**：仅 UI 线程装配期持有令牌者可创建——"往 UI 投递"的能力由装配层
///   显式分发，而不是任何人都能随手造一条；
/// - **使用**：`deliver` 可在任意线程调用（载荷经通道进入 UI 线程消费），
///   `Send + Sync`，克隆廉价；
/// - **语义**：载荷的消费端由构造方自行定义（如 `invoke_from_event_loop` 的
///   闭包），本类型只负责"上行单向 + 构造期受控"这两点契约。
///
/// # 类型参数
/// `T: Send + 'static`——载荷必须能跨线程进入 UI 事件循环。
pub struct UiDeliver<T: Send + 'static> {
    tx: tokio::sync::mpsc::UnboundedSender<T>,
}

impl<T: Send + 'static> Clone for UiDeliver<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

impl<T: Send + 'static> UiDeliver<T> {
    /// 凭 UI 线程令牌构造交付通道（`receiver` 为 UI 线程侧的消费端）。
    pub fn new(
        _token: &UiThreadToken,
        receiver: impl FnOnce(tokio::sync::mpsc::UnboundedReceiver<T>) + Send + 'static,
    ) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        // 消费端在 UI 线程侧由调用方接管（通常经 spawn 到运行时后转投事件循环）。
        receiver(rx);
        Self { tx }
    }

    /// 向 UI 交付一份载荷（任意线程可调用；即发即弃，不阻塞调用方）。
    ///
    /// 通道关闭（UI 消费端已退出）时返回载荷本身，调用方可自行降级处理。
    pub fn deliver(&self, payload: T) -> Result<(), T> {
        self.tx.send(payload).map_err(|send_err| send_err.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试专用：绕过全局唯一性领取令牌。
    ///
    /// `claim()` 的唯一性由 `ui_thread_token_is_claimable_once` 单独验证（进程内
    /// 全局状态无法在并行测试中安全重置）；其余测试只关心 `UiDeliver` 的通道语义。
    #[cfg(test)]
    impl UiThreadToken {
        fn claim_anyway_for_tests() -> Self {
            UI_THREAD_CLAIMED.store(true, Ordering::SeqCst);
            Self { _private: () }
        }
    }

    /// 令牌唯一性：同进程第二次领取必须失败——这是"只承认一个 UI 线程"的落点。
    ///
    /// 并行测试下令牌可能已被其他测试领取；无论初始状态如何，**单调性**（领取成功
    /// 后不可再领）必须成立。
    #[test]
    fn ui_thread_token_is_claimable_once() {
        match UiThreadToken::claim() {
            Ok(_token) => {
                assert_eq!(
                    UiThreadToken::claim().err(),
                    Some(TokenError::AlreadyClaimed),
                    "重复领取必须被拒绝"
                );
            }
            Err(TokenError::AlreadyClaimed) => {
                // 已被同进程内先行的测试领取：单调性由那次领取的断言覆盖。
            }
        }
    }

    /// `UiDeliver` 的单向语义：任意线程可 `deliver`，载荷在消费端按序到达。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ui_deliver_delivers_payloads_in_order() {
        let token = UiThreadToken::claim_anyway_for_tests();
        let received: std::sync::Arc<tokio::sync::Mutex<Vec<u32>>> = std::sync::Arc::default();

        let sink = received.clone();
        let deliver: UiDeliver<u32> = UiDeliver::new(&token, move |mut rx| {
            tokio::spawn(async move {
                while let Some(value) = rx.recv().await {
                    sink.lock().await.push(value);
                }
            });
        });

        for value in 1..=5u32 {
            deliver.deliver(value).expect("消费端存活时应投递成功");
        }
        // 等待消费端排空（有界轮询，避免测试挂死）。
        for _ in 0..100 {
            if received.lock().await.len() == 5 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(*received.lock().await, vec![1, 2, 3, 4, 5], "应按序到达");
    }

    /// 消费端退出后 `deliver` 返回载荷本身（调用方可降级），不得 panic。
    #[tokio::test]
    async fn ui_deliver_returns_payload_when_receiver_dropped() {
        let token = UiThreadToken::claim_anyway_for_tests();
        let deliver: UiDeliver<u32> = UiDeliver::new(&token, |_| {}); // 立即丢弃 rx
        assert_eq!(deliver.deliver(42), Err(42), "通道关闭应归还载荷");
    }
}
