//! # 跨线程响应式事件总线（阶段二 · 架构转型后）
//!
//! 基于 [`tokio::sync::broadcast`] 构建的一对多广播通道，统一承载系统内两类跨任务事件：
//!
//! - [`AppEvent::ModuleStatusChanged`]：模块调度器在生命周期操作（启动 / 停止）**结束后**
//!   广播模块的“最终真实状态”。UI 据此刷新视图或回滚开关，从根本上消除
//!   “界面状态与底层进程事实不一致”的虚假状态（见 `crate::manager::ModuleManager::toggle`）；
//! - [`AppEvent::AppLogAppended`]：装配与模块启停产生的应用运行日志流（原
//!   `AgentLogAppended` 随 agent 层退役更名而来），供 UI 日志面板消费；
//! - [`AppEvent::TrayAction`]：系统托盘的常驻指令（[`TrayAction`]），由托盘线程
//!   （`crate::tray`）在用户点击菜单 / 双击图标时发布，被“生命周期控制器”常驻任务
//!   消费以驱动显示窗口、全量开关模块、退出程序。
//!
//! # 设计约束
//!
//! - **发布者永不阻塞、永不等待**：`publish` 是同步非阻塞调用，broadcast 通道内部为无锁
//!   环形缓冲；慢消费者只会造成最旧事件被淘汰（下次读取收到 `Lagged` 提示），绝不会拖慢
//!   发布方——因此调度器可以在模块状态落定后安全地同步广播，无需引入异步锁。
//! - **订阅者迟到不补发**：事件仅在发送时刻向当时存活的接收者投递；启动早期（UI 尚未
//!   订阅）产生的事件允许被丢弃，这正是桌面应用“先装配、后订阅”生命周期下的预期行为。
//! - **无订阅者即丢弃**：`send` 返回 `Err` 时仅记录 `debug` 日志，不向调用方报错。

use tokio::sync::broadcast;

/// 总线默认通道容量：环形缓冲可容纳的未消费事件数。
///
/// 容量需权衡“订阅者重启时的可回放深度”与内存占用；256 对高频认知日志足够平滑
/// （阶段四日志转发任务常驻消费，几乎不会触发淘汰）。
pub const DEFAULT_CHANNEL_CAPACITY: usize = 256;

/// 应用级事件载荷。
///
/// 所有变体携带自有字段（自包含负载），且事件可跨任务克隆分发；
/// 各发布端与订阅端通过该枚举解耦，不共享任何可变状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEvent {
    /// 模块运行状态发生变更（或一次 toggle 请求落定后对现状的同步广播）。
    ModuleStatusChanged {
        /// 模块唯一标识符（与 [`crate::modules::ToolModule::id`] 一致，如 `"popup_blocker"`）。
        id: String,
        /// 变更落定后模块的真实运行状态。
        is_running: bool,
    },
    /// 应用运行日志：装配 / 模块启停等运行时事件追加一条控制台日志。
    ///
    /// 构造点位于 `crate::main` 的装配与回调层（如模块启停失败告警、开机自启
    /// 同步结果）。该变体在转型前名为 `AgentLogAppended`、承载 Agent 推理认知
    /// 日志流；agent 层退役后更名为通用应用日志，继续由 UI 日志面板消费。
    AppLogAppended {
        /// 日志级别标签（`INFO` / `SUCCESS` / `WARN` / `ERROR` 等，供 UI 着色）。
        level: String,
        /// 日志正文。
        message: String,
    },
    /// 系统托盘发出的常驻指令（构造点：`crate::tray` 托盘线程的消息泵）。
    ///
    /// 托盘线程不直接触碰 UI / 模块调度器，只把用户意图以本事件投上总线；
    /// 具体执行由装配层（`crate::main`）的“生命周期控制器”订阅者完成，
    /// 从而把平台回调线程与 Tokio 异步世界彻底解耦。
    TrayAction(TrayAction),
    /// 后台守护线程请求弹出一条 Toast（v0.4.0：窗口置顶模块的守护线程在
    /// `SetWindowPos` 遭遇 UIPI 拦截时发布，提示用户提权运行）。
    ///
    /// 构造点位于模块的原生守护线程（无法直接触碰 UI）；总线桥接层收到后经
    /// `show_toast` 在 UI 线程展示。与 UI 回调路径的同步 Toast 互为补充——
    /// UI 同步操作仍直接调用 Toast 入口，只有“后台线程需要提示用户”的
    /// 场景才走本事件。
    ToastRequested(String),
}

/// 托盘用户操作 → 应用级常驻指令。
///
/// 全部变体语义上都是“用户意图”，执行结果一律回落到模块调度器的真实状态
/// 事件（[`AppEvent::ModuleStatusChanged`]），不携带任何跨线程可变状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// 用户请求显示主窗口（点击托盘菜单“显示主窗口”或双击托盘图标）。
    ///
    /// 由生命周期控制器经 `slint::invoke_from_event_loop` 在 UI 线程执行
    /// `show()`——托盘线程与总线均不直接持有任何 Slint 对象。
    ShowWindow,
    /// 用户请求把全部模块统一切换到 `enable` 指定的状态（`true` = 全部开启，
    /// `false` = 全部关闭），对应托盘菜单“全部模块：开启 / 关闭”。
    ToggleAllModules(bool),
    /// 用户请求以管理员权限重启当前程序（托盘菜单“以管理员身份重启” / UI 盾牌
    /// 提权按钮共用）。仅在**未提权**运行形态下暴露入口。
    ///
    /// 由生命周期控制器执行 [`crate::platform::restart_as_admin`]（阻塞至 UAC
    /// 确认框关闭）：成功后调度 UI 事件循环退出 → 旧进程走平滑收尾；失败（用户
    /// 取消 / 无权提权）仅告警并保持原样运行。
    RestartAsAdmin,
    /// 用户请求退出程序（托盘菜单“退出程序”）。
    ///
    /// 生命周期控制器收到后调度 `slint::quit_event_loop`，主线程 `run` 返回并
    /// 进入平滑收尾（逆序停止全部模块、关闭托盘线程）。
    ExitApp,
}

/// 轻量克隆式事件总线句柄。
///
/// 内部仅持有一个 [`broadcast::Sender`]，因此：
///
/// - `Clone` 为 O(1) 浅拷贝，可自由注入模块管理器、UI 转发任务等多个发布端；
/// - `Send + Sync`，可跨线程共享（无需再包一层 `Arc` / 锁）。
#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<AppEvent>,
}

impl EventBus {
    /// 以指定容量创建事件总线。
    ///
    /// # Panics
    /// `capacity` 为 0 时 panic（broadcast 通道要求容量 ≥ 1）。
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "EventBus 通道容量必须大于 0，收到 {capacity}");
        let (sender, _initial_rx) = broadcast::channel(capacity);
        Self { sender }
    }

    /// 向总线发布一条事件（同步、非阻塞、即发即弃）。
    ///
    /// 返回实际送达的订阅者数量；返回 0 表示当前没有订阅者，事件被丢弃并记录
    /// `debug` 日志——这属于预期语义而非错误，故不向调用方返回 `Result`。
    pub fn publish(&self, event: AppEvent) -> usize {
        match self.sender.send(event) {
            Ok(subscribers) => subscribers,
            Err(_send_error) => {
                tracing::debug!(
                    target: "bus",
                    "事件被丢弃：总线当前无活跃订阅者（事件在 UI 常驻订阅前的启动期丢失属预期行为）"
                );
                0
            }
        }
    }

    /// 创建一个新的订阅端。
    ///
    /// 订阅端只接收订阅之后发布的事件；若消费速度长期落后于发布速度，`recv` 将返回
    /// [`broadcast::error::RecvError::Lagged`] 提示跳过了多少条事件。
    pub fn subscribe(&self) -> broadcast::Receiver<AppEvent> {
        self.sender.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(DEFAULT_CHANNEL_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast::error::RecvError;

    /// 构造测试用事件。
    fn status_event(running: bool) -> AppEvent {
        AppEvent::ModuleStatusChanged {
            id: "popup_blocker".into(),
            is_running: running,
        }
    }

    #[test]
    fn default_capacity_is_sane() {
        // 编译期常量断言：容量常量改动时立即在构建期暴露回归。
        const { assert!(DEFAULT_CHANNEL_CAPACITY >= 16) };
    }

    #[tokio::test]
    async fn publish_reaches_single_subscriber() {
        let bus = EventBus::new(8);
        let mut rx = bus.subscribe();

        assert_eq!(bus.publish(status_event(true)), 1, "应送达 1 个订阅者");
        match rx.recv().await {
            Ok(AppEvent::ModuleStatusChanged { is_running, .. }) => assert!(is_running),
            other => panic!("收到意外事件: {other:?}"),
        }
    }

    #[tokio::test]
    async fn publish_fans_out_to_all_subscribers() {
        let bus = EventBus::new(8);
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        assert_eq!(bus.publish(status_event(false)), 2, "应送达全部 2 个订阅者");
        assert_eq!(rx1.recv().await.unwrap(), status_event(false));
        assert_eq!(rx2.recv().await.unwrap(), status_event(false));
    }

    #[tokio::test]
    async fn cloned_bus_shares_the_same_channel() {
        let bus = EventBus::new(8);
        let bus_clone = bus.clone();
        let mut rx = bus.subscribe();

        // 经克隆句柄发布，原始句柄的订阅者同样能收到。
        bus_clone.publish(status_event(true));
        assert_eq!(rx.recv().await.unwrap(), status_event(true));
    }

    #[tokio::test]
    async fn publish_without_subscribers_drops_event_quietly() {
        let bus = EventBus::new(8);
        // 未持有任何订阅端时发布：不 panic、不报错，仅返回 0。
        assert_eq!(bus.publish(status_event(true)), 0);
    }

    #[tokio::test]
    async fn lagging_subscriber_receives_lagged_notice() {
        // 容量 2 的环形缓冲：发布 3 条后最早一条被淘汰，慢订阅者应收到 Lagged(1)，
        // 随后可继续读取仍驻留缓冲的最新事件——发布者全程未被阻塞。
        let bus = EventBus::new(2);
        let mut rx = bus.subscribe();

        bus.publish(status_event(true)); // 位置 0
        bus.publish(status_event(false)); // 位置 1
        bus.publish(status_event(true)); // 位置 2：位置 0 被淘汰

        match rx.recv().await {
            Err(RecvError::Lagged(skipped)) => assert_eq!(skipped, 1),
            other => panic!("慢订阅者应收到 Lagged 提示，实际: {other:?}"),
        }
        assert_eq!(rx.recv().await.unwrap(), status_event(false));
        assert_eq!(rx.recv().await.unwrap(), status_event(true));
    }
}
