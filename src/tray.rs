//! # 系统托盘与常驻生命周期（Windows 原生实现）
//!
//! TLToolBox 定位为**桌面常驻**工具：主窗口可以隐藏、常驻于系统通知区
//! （System Tray），由托盘图标承载右键菜单与双击唤醒。本模块是常驻层的
//! 底层机制，负责：
//!
//! - 创建托盘图标（优先从**可执行文件内嵌图标资源**解码 32×32 应用图标——
//!   `build.rs` 已把 `res/app.ico` 经 winresource 编译为 RT_GROUP_ICON/RT_ICON，
//!   此处读取帧目录并解码 DIB 帧；资源缺失或格式异常时降级为**程序化生成的
//!   备用嵌入图标**——纯代码像素绘制，零额外资源文件）；
//! - 构建原生右键菜单（`muda` crate）：`显示主窗口` / `全部模块：开启/关闭` /
//!   （未提权时额外渲染）`以管理员身份重启` / `退出程序`；
//! - 监听托盘事件（菜单点击、双击图标），把用户意图以
//!   [`AppEvent::TrayAction`] 发布到事件总线，由装配层的“生命周期控制器”
//!   消费执行——托盘代码**不直接触碰** UI 与模块调度器。
//!
//! # 线程与消息泵模型（核心）
//!
//! ```text
//! 主线程(UI/Tokio)               托盘线程(OS 专用)              总线/调度层
//! ──────────────               ──────────────              ─────────────
//!   lifecycle_controller  ──SyncAllModules──▶ TrayManager      EventBus
//!    (订阅 ModuleStatus)        (控制通道 mpsc)   │  ▲             ▲  │
//!   TrayHandle::shutdown ──▶ WM_QUIT 定向投递    │  └─ 菜单/双击 ──┘  │
//!     (主线程收尾)               (PostThreadMessageW)   (bus.publish)  │
//!   forward_bus_events ◀─────────────────────────────────────────────┘
//!    (订阅总线 → UI)
//! ```
//!
//! 1. **托盘线程是唯一拥有托盘资源的线程**：`TrayIcon`、`muda::Menu` 及全部
//!    菜单项内部为 `Rc`（非 `Send`），且 Windows 上 `Shell_NotifyIcon` 的回调
//!    消息只投递给**创建该图标的线程**。因此托盘对象在专用 OS 线程内构造、
//!    由该线程自己运行 Win32 消息泵（`GetMessageW` → `TranslateMessage` →
//!    `DispatchMessageW`）驱动——右键菜单、双击、气泡等回调全部在该线程
//!    的窗口过程里触发，天然满足 tray-icon/muda 的线程亲和约束；
//! 2. **出站只发事件、入站只收命令**：托盘线程把用户意图经
//!    [`EventBus::publish`]（同步、无锁、非阻塞）投上事件总线；主线程把菜单
//!    文案刷新等低频控制经 **mpsc 控制通道 + `WM_TRAY_CONTROL` 线程消息唤醒**
//!    送回托盘线程。两个方向都是**单向、无应答、无等待**的投递；
//! 3. **零跨线程锁**：本模块不引入任何 `Mutex`/`RwLock`。唯一的共享可变状态
//!    是“全部模块聚合开关”缓存与菜单文案，全部位于托盘线程内部；总线事件是
//!    值语义快照（[`AppEvent`] `Clone`），线程之间只传递值，不共享引用；
//! 4. **单实例唤醒接收**：消息泵除 `WM_TRAY_CONTROL` 外还比对
//!    [`single_instance::register_wakeup_message`](crate::single_instance) 注册的
//!    广播编号（`TLTOOLBOX_WAKEUP_EXISTING_INSTANCE`，见
//!    [`crate::single_instance`]）——用户再次双击 exe 时，第二实例向
//!    `HWND_BROADCAST` 投递该消息（tray-icon 的隐藏窗口是**顶层**窗口，必然收到
//!    广播），本泵识别后经总线发布
//!    [`AppEvent::TrayAction(TrayAction::ShowWindow)`]，把静默常驻的主窗口还原
//!    前置——与托盘菜单「显示主窗口」/ 双击共用同一条唤醒管线。
//!
//! # 为什么不会死锁
//!
//! - 托盘线程**从不阻塞等待主线程**：消息泵只在自己的队列上 `GetMessageW`
//!   阻塞；总线发布是即发即弃的广播（无订阅者也不报错）；控制通道 send 同样
//!   非阻塞（mpsc 有界？此处为 `sync_channel` 无界/或标准 `channel` 有界——
//!   见下文说明，均为短指令、低频率，不会积压）；
//! - 主线程**从不阻塞等待托盘线程**（除进程收尾时一次性 `join` 停机线程，
//!   该线程此时已收到 `WM_QUIT` 即将退出，join 只是收尸）；
//! - UI 更新永远走 `slint::invoke_from_event_loop` 排队到 UI 线程执行，
//!   托盘线程/总线任务与 UI 线程之间不存在“我等你、你等我”的环。
//!
//! # 依赖配对说明
//!
//! `tray-icon 0.19` 原生依赖 `muda 0.15`（菜单类型直接互操作），二者必须同
//! 版本使用；Slint 的 winit 后端会自引一份更新的 muda，仅服务 Slint 原生菜单
//! 栏，与本模块的托盘菜单互不干扰（两份 muda 的全局事件通道相互独立）。

use crate::bus::{AppEvent, EventBus, TrayAction};
use std::fmt;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::JoinHandle;

// ---------------------------------------------------------------------------
// 公开常量：菜单项 ID（muda 以字符串 ID 区分菜单项）
// ---------------------------------------------------------------------------

/// “显示主窗口”菜单项 ID。
pub const MENU_ID_SHOW_WINDOW: &str = "show-window";
/// “全部模块：开启 / 关闭”菜单项 ID（文案随聚合状态动态切换）。
pub const MENU_ID_TOGGLE_ALL: &str = "toggle-all";
/// “以管理员身份重启”菜单项 ID（**仅未提权**运行形态下渲染，见 [`spawn`] 的
/// `elevated` 参数）。点击后发布 [`TrayAction::RestartAsAdmin`]，由装配层的
/// 生命周期控制器执行 [`crate::platform::restart_as_admin`]（UAC 确认 → 平滑收尾）。
pub const MENU_ID_RESTART_ADMIN: &str = "restart-admin";
/// “退出程序”菜单项 ID。
pub const MENU_ID_EXIT: &str = "exit-app";

/// “以管理员身份重启”菜单项文案。
pub const MENU_LABEL_RESTART_ADMIN: &str = "以管理员身份重启";

/// “全部模块”菜单项文案（当前聚合状态为“全部开启”时）。
pub const MENU_LABEL_TOGGLE_OFF: &str = "全部模块：关闭";
/// “全部模块”菜单项文案（当前聚合状态为“非全部开启”时）。
pub const MENU_LABEL_TOGGLE_ON: &str = "全部模块：开启";

/// 由聚合状态推导“全部模块”菜单项文案。
///
/// 文案描述的是**点击后将要执行的动作**：全部开启时点击 = 全部关闭，
/// 否则点击 = 全部开启。
pub fn toggle_all_label(all_enabled: bool) -> &'static str {
    if all_enabled {
        MENU_LABEL_TOGGLE_OFF
    } else {
        MENU_LABEL_TOGGLE_ON
    }
}

// ---------------------------------------------------------------------------
// 跨线程控制指令（主线程 → 托盘线程）
// ---------------------------------------------------------------------------

/// 主线程 → 托盘线程的控制指令。
///
/// 指令极低频（模块状态变更 / 退出时各一次），经 mpsc 控制通道投递，
/// 并通过向托盘线程投递 `WM_TRAY_CONTROL` 消息唤醒阻塞中的消息泵。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    /// 同步“全部模块”聚合开关状态（`true` = 全部开启），托盘据此刷新菜单文案。
    SyncAllModules(bool),
    /// 请求托盘线程退出（配合 `WM_QUIT` 定向投递，见 [`TrayControl::request_shutdown`]）。
    Shutdown,
}

// ---------------------------------------------------------------------------
// 错误模型（跨平台）
// ---------------------------------------------------------------------------

/// 托盘初始化 / 运行错误。
#[derive(Debug)]
pub enum TrayError {
    /// 非 Windows 平台：系统托盘是 Win32（`Shell_NotifyIcon`）原生能力。
    UnsupportedPlatform,
    /// 托盘初始化失败（图标 / 菜单 / 系统通知区不可用等）。
    Init {
        /// 失败原因描述。
        reason: String,
    },
    /// 无法创建托盘线程。
    SpawnThread {
        /// 底层 IO 错误。
        source: std::io::Error,
    },
}

impl fmt::Display for TrayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                write!(f, "系统托盘仅支持 Windows（Shell_NotifyIcon 原生能力）")
            }
            Self::Init { reason } => write!(f, "托盘初始化失败: {reason}"),
            Self::SpawnThread { source } => write!(f, "托盘线程创建失败: {source}"),
        }
    }
}

impl std::error::Error for TrayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SpawnThread { source } => Some(source),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// 公开句柄与控制端
// ---------------------------------------------------------------------------

/// 托盘控制端（`Clone` 为 O(1) 浅拷贝）。
///
/// 供主线程的“生命周期控制器”等**多个**消费者共享，向托盘线程投递控制
/// 指令；所有方法均为非阻塞投递，可安全地在任意线程调用。
#[derive(Clone)]
pub struct TrayControl {
    tx: mpsc::Sender<TrayCommand>,
    /// 托盘线程 ID（`0` = 线程尚未就绪）；用于 `WM_TRAY_CONTROL` 唤醒。
    tray_thread_id: Arc<AtomicU32>,
}

impl TrayControl {
    /// 同步“全部模块”聚合状态，让托盘菜单文案与调度层事实收敛。
    ///
    /// 非阻塞：send 失败（托盘线程已退出）时静默忽略。
    pub fn sync_all_modules(&self, enabled: bool) {
        self.send(TrayCommand::SyncAllModules(enabled));
    }

    /// 请求托盘线程停机（进程收尾时调用；随后由 [`TrayHandle::shutdown`] join）。
    ///
    /// 同时投递 `WM_QUIT`：即使托盘线程正阻塞在 `GetMessageW`、甚至正停在
    /// 右键弹出菜单的模态循环里，也能立即唤醒并使其退出。
    pub fn request_shutdown(&self) {
        self.send(TrayCommand::Shutdown);
        platform::post_wm_quit(&self.tray_thread_id);
    }

    /// 底层非阻塞投递（send + 唤醒消息）。
    fn send(&self, command: TrayCommand) {
        if self.tx.send(command).is_ok() {
            platform::wake_tray_thread(&self.tray_thread_id);
        }
    }
}

/// 托盘句柄：持有控制端与线程 join 句柄，由 [`spawn`] 返回。
///
/// 主线程用它把控制器任务所需的 [`TrayControl`] 克隆出去，并在进程收尾时
/// [`shutdown`](Self::shutdown) 平滑关闭托盘线程。
pub struct TrayHandle {
    control: TrayControl,
    join: Option<JoinHandle<()>>,
}

impl TrayHandle {
    /// 克隆控制端（供生命周期控制器等后台任务共享）。
    pub fn control(&self) -> TrayControl {
        self.control.clone()
    }

    /// 请求停机并等待托盘线程退出（进程收尾的一次性调用）。
    ///
    /// 托盘线程收到 `WM_QUIT` / `Shutdown` 后立即结束消息泵、释放图标资源并
    /// 退出；此处 join 只做收尾，不参与任何业务锁，不会死锁。
    pub fn shutdown(mut self) {
        self.control.request_shutdown();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        // 兜底：未显式 shutdown 即被丢弃（如启动早期错误路径）时，至少请求
        // 托盘线程退出，避免图标残留。不在此 join——Drop 路径不阻塞调用方。
        self.control.request_shutdown();
    }
}

// ---------------------------------------------------------------------------
// 平台实现门面
// ---------------------------------------------------------------------------

/// 启动系统托盘（创建托盘图标 + 右键菜单 + 后台消息泵线程）。
///
/// - `bus`：托盘事件（[`TrayAction`]）的发布出口；
/// - `all_modules_enabled`：装配时刻“全部模块”的聚合状态，用于菜单初始文案；
/// - `elevated`：当前进程是否处于**提权（管理员）**状态。为 `false`（常规运行，
///   受 UIPI 限制无法向高权限窗口投递关闭消息）时，右键菜单额外渲染
///   「以管理员身份重启」项（[`MENU_ID_RESTART_ADMIN`]）；为 `true` 时隐藏该项
///   ——已提权的实例不需要也无权再自我提权，避免无意义地重复弹 UAC。
///
/// 返回 [`TrayHandle`]；初始化失败（通知区不可用等）返回 [`TrayError`]，
/// 由调用方决定降级（无托盘继续运行）而非崩溃。
#[cfg(windows)]
pub fn spawn(
    bus: EventBus,
    all_modules_enabled: bool,
    elevated: bool,
) -> Result<TrayHandle, TrayError> {
    platform::spawn_impl(bus, all_modules_enabled, elevated)
}

/// 非 Windows 兜底：系统托盘不可用（见 [`TrayError::UnsupportedPlatform`]）。
#[cfg(not(windows))]
pub fn spawn(
    _bus: EventBus,
    _all_modules_enabled: bool,
    _elevated: bool,
) -> Result<TrayHandle, TrayError> {
    Err(TrayError::UnsupportedPlatform)
}

/// 非 Windows 占位实现：无托盘能力，唤醒 / 停机投递均为 no-op
/// （`TrayControl` 的公共方法跨平台编译，但非 Windows 上永远不会产生
/// 真实托盘线程，因此这些调用是安全的空操作）。
#[cfg(not(windows))]
mod platform {
    use super::*;

    /// no-op：无托盘线程可唤醒。
    pub(super) fn wake_tray_thread(_thread_id: &Arc<AtomicU32>) {}

    /// no-op：无托盘线程可停机。
    pub(super) fn post_wm_quit(_thread_id: &Arc<AtomicU32>) {}
}

/// Windows 真实实现（见模块文档的线程模型）。
#[cfg(windows)]
mod platform {
    use super::*;

    use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HGLOBAL, HMODULE, HRSRC, HWND, LPARAM, WPARAM};
    use windows::Win32::System::LibraryLoader::{
        FindResourceW, GetModuleHandleW, LoadResource, LockResource, SizeofResource,
    };
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, MSG, PM_REMOVE, PeekMessageW, PostThreadMessageW,
        TranslateMessage, WM_APP, WM_QUIT,
    };

    /// 托盘线程唤醒消息（主线程投递，非 `WM_QUIT` 时仅唤醒不携带载荷）。
    const WM_TRAY_CONTROL: u32 = WM_APP + 0x0100;

    /// 备用嵌入图标的规格（32×32，Windows 托盘按 DPI 缩放）。
    const ICON_SIZE: usize = 32;

    // ---- 内嵌图标资源常量（与 build.rs 的 winresource 嵌入约定一致） ----

    /// RT_ICON 资源类型（winuser.h 预定义 #3）。
    const RT_ICON: u16 = 3;
    /// RT_GROUP_ICON 资源类型（winuser.h 预定义 #14）。
    const RT_GROUP_ICON: u16 = 14;
    /// winresource::set_icon 固定把图标组写为资源名 "1"（应用图标约定）。
    const APP_ICON_GROUP_ID: u16 = 1;
    /// 托盘图标目标边长（32 px；帧目录按此就近选帧，DPI 缩放交给系统）。
    const TRAY_ICON_PX: u32 = 32;

    // ------------------------------------------------------------------
    // 托盘管理器（托盘线程独享；非 Send —— 内部为 Rc 句柄）
    // ------------------------------------------------------------------

    /// 托盘管理器：托盘图标 + 右键菜单 + 事件路由（**托盘线程独享实例**）。
    ///
    /// 本结构体在托盘线程内构造与销毁，绝不跨线程移动：`muda` 菜单项内部为
    /// `Rc<RefCell<_>>`，`tray-icon` 的隐藏窗口绑定创建线程。所有方法只能在
    /// 拥有它的托盘线程（消息泵循环内）调用。
    pub struct TrayManager {
        /// 事件总线出口：菜单点击 / 双击 → [`AppEvent::TrayAction`]。
        bus: EventBus,
        /// 托盘图标本体（持有右键菜单；Drop 时移除图标并销毁隐藏窗口）。
        _tray_icon: TrayIcon,
        /// “全部模块”菜单项句柄（动态文案）。
        toggle_all_item: MenuItem,
        /// 托盘线程本地缓存的聚合开关状态（与菜单文案一致；由
        /// [`TrayCommand::SyncAllModules`] 从调度层事实收敛）。
        all_enabled: bool,
    }

    impl TrayManager {
        /// 在**当前线程**构造托盘管理器（本函数运行于托盘线程内）。
        ///
        /// 任一环节失败（图标 / 菜单 / 系统通知区不可用）返回 [`TrayError`]，
        /// 托盘线程据此上报初始化失败并退出，调用方降级为无托盘模式。
        fn new(bus: EventBus, all_enabled: bool, elevated: bool) -> Result<Self, TrayError> {
            // 1) 右键菜单：显示主窗口 / 分隔 / 全部模块 / 分隔 /
            //    （未提权时：以管理员身份重启 /）退出程序。
            let menu = Menu::new();
            let show_item = MenuItem::with_id(MENU_ID_SHOW_WINDOW, "显示主窗口", true, None);
            let separator_a = PredefinedMenuItem::separator();
            let toggle_item = MenuItem::with_id(
                MENU_ID_TOGGLE_ALL,
                toggle_all_label(all_enabled),
                true,
                None,
            );
            let separator_b = PredefinedMenuItem::separator();

            for item in [
                &show_item as &dyn muda::IsMenuItem,
                &separator_a,
                &toggle_item,
                &separator_b,
            ] {
                menu.append(item)
                    .map_err(|err| TrayError::Init {
                        reason: format!("右键菜单装配失败: {err}"),
                    })?;
            }

            // 2) 提权入口：仅在未提权（受 UIPI 限制、拦截高权限窗口可能失败）时
            //    渲染；已提权实例隐藏该项——菜单在启动装配时按 `elevated` 快照
            //    一次性定型，进程生命周期内提权状态不会漂移，无需运行期增删。
            if !elevated {
                let restart_item = MenuItem::with_id(
                    MENU_ID_RESTART_ADMIN,
                    MENU_LABEL_RESTART_ADMIN,
                    true,
                    None,
                );
                menu.append(&restart_item)
                    .map_err(|err| TrayError::Init {
                        reason: format!("右键菜单装配失败: {err}"),
                    })?;
            }

            let exit_item = MenuItem::with_id(MENU_ID_EXIT, "退出程序", true, None);
            menu.append(&exit_item)
                .map_err(|err| TrayError::Init {
                    reason: format!("右键菜单装配失败: {err}"),
                })?;

            // 3) 图标：优先从 exe 内嵌图标资源（build.rs 嵌入的 res/app.ico）
            //    解码 32×32 帧；资源缺失 / 非 32bpp DIB / 解析失败时降级为
            //    纯代码像素绘制的备用图标，保证托盘任何构建形态下都有图标。
            let icon = match load_embedded_app_icon() {
                Ok(icon) => {
                    tracing::debug!(target: "tray", "托盘图标：已从 exe 内嵌图标资源加载 32×32 帧");
                    icon
                }
                Err(err) => {
                    tracing::warn!(
                        target: "tray",
                        "托盘图标资源加载失败，降级为程序化备用图标: {err}"
                    );
                    build_fallback_icon()?
                }
            };

            // 4) 托盘图标：绑定菜单；左键单击**不**弹出菜单（仅右键弹出，
            //    左键保留给双击 → 显示主窗口）。
            let tray_icon = TrayIconBuilder::new()
                .with_tooltip("TLToolBox")
                .with_menu(Box::new(menu))
                .with_icon(icon)
                .with_menu_on_left_click(false)
                .build()
                .map_err(|err| TrayError::Init {
                    reason: format!("托盘图标创建失败: {err}"),
                })?;

            Ok(Self {
                bus,
                _tray_icon: tray_icon,
                toggle_all_item: toggle_item,
                all_enabled,
            })
        }

        /// 更新聚合开关缓存与“全部模块”菜单文案（仅托盘线程内调用）。
        fn set_all_enabled(&mut self, enabled: bool) {
            if self.all_enabled == enabled {
                return;
            }
            self.all_enabled = enabled;
            self.toggle_all_item.set_text(toggle_all_label(enabled));
        }

        /// 处理一条菜单事件：路由为用户动作并发布到事件总线。
        fn handle_menu_event(&mut self, event: MenuEvent) {
            let action = if event.id == MENU_ID_SHOW_WINDOW {
                Some(TrayAction::ShowWindow)
            } else if event.id == MENU_ID_TOGGLE_ALL {
                // 取反聚合缓存得到“点击将执行的目标状态”，并先行收敛文案；
                // 调度层落定后的 ModuleStatusChanged 会经 SyncAllModules 再次校正。
                let target = !self.all_enabled;
                self.set_all_enabled(target);
                Some(TrayAction::ToggleAllModules(target))
            } else if event.id == MENU_ID_RESTART_ADMIN {
                // 提权重启（仅未提权菜单渲染该项）：发布到总线，由装配层生命周期
                // 控制器执行 restart_as_admin（UAC 确认 → 成功则平滑收尾退出）。
                Some(TrayAction::RestartAsAdmin)
            } else if event.id == MENU_ID_EXIT {
                Some(TrayAction::ExitApp)
            } else {
                None // 未知菜单项（防御性忽略）
            };

            if let Some(action) = action {
                self.bus.publish(AppEvent::TrayAction(action));
            }
        }

        /// 处理一条托盘图标事件：双击左键 → 显示主窗口。
        fn handle_tray_icon_event(&mut self, event: TrayIconEvent) {
            if let TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } = event
            {
                self.bus.publish(AppEvent::TrayAction(TrayAction::ShowWindow));
            }
            // 其余事件（单击、移动、进出等）暂不消费。
        }

        /// 排空控制通道（主线程 → 托盘线程的指令）。
        fn drain_controls(&mut self, cmd_rx: &mpsc::Receiver<TrayCommand>) -> bool {
            let mut keep_running = true;
            while let Ok(command) = cmd_rx.try_recv() {
                match command {
                    TrayCommand::SyncAllModules(enabled) => self.set_all_enabled(enabled),
                    TrayCommand::Shutdown => keep_running = false,
                }
            }
            keep_running
        }

        /// 排空 muda 菜单事件通道（事件在窗口过程 / 弹出菜单模态循环中入队）。
        fn drain_menu_events(&mut self) {
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                self.handle_menu_event(event);
            }
        }

        /// 排空 tray-icon 图标事件通道。
        fn drain_tray_icon_events(&mut self) {
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                self.handle_tray_icon_event(event);
            }
        }

        /// 运行 Win32 消息泵直至停机（消费本结构体，退出时自动释放图标）。
        ///
        /// `wakeup_message_id`：第二实例的唤醒广播编号（
        /// [`crate::single_instance::register_wakeup_message`]，`0` = 未注册成功）。
        /// 广播经 `HWND_BROADCAST` 投递到托盘线程创建的顶层隐藏窗口，泵在此
        /// 识别并发布 [`AppEvent::TrayAction(TrayAction::ShowWindow)`]。
        fn run(mut self, cmd_rx: mpsc::Receiver<TrayCommand>, wakeup_message_id: u32) {
            tracing::debug!(target: "tray", "托盘消息泵已启动");
            loop {
                // 1) 控制指令（可被 WM_TRAY_CONTROL 唤醒后到达）。
                if !self.drain_controls(&cmd_rx) {
                    break; // 收到 Shutdown
                }
                // 2) 菜单 / 图标事件（刚完成的 DispatchMessageW 可能已入队）。
                self.drain_menu_events();
                self.drain_tray_icon_events();

                // 3) 阻塞泵取一条消息（含托盘隐藏窗口的 Shell_NotifyIcon 回调）。
                let mut msg = MSG::default();
                let ret = unsafe { GetMessageW(&mut msg, HWND::default(), 0, 0) };
                if ret.0 == 0 {
                    break; // WM_QUIT（TrayControl::request_shutdown 定向投递）
                }
                if ret.0 == -1 {
                    tracing::warn!(target: "tray", "GetMessageW 失败，托盘消息泵退出");
                    break;
                }

                if msg.message == WM_TRAY_CONTROL {
                    // 唤醒消息只用于跳出 GetMessageW，本身无载荷，无需分发。
                } else if wakeup_message_id != 0 && msg.message == wakeup_message_id {
                    // 第二实例的唤醒广播（单实例守护）：把静默常驻的主窗口还原
                    // 前置。发布 AppEvent::TrayAction(ShowWindow)，由生命周期
                    // 控制器经 invoke_from_event_loop 在 UI 线程执行。
                    self.bus.publish(AppEvent::TrayAction(TrayAction::ShowWindow));
                } else {
                    unsafe {
                        let _ = TranslateMessage(&msg);
                        let _ = DispatchMessageW(&msg);
                    }
                }
            }
            tracing::debug!(target: "tray", "托盘消息泵已退出，正在释放图标资源");
            // self 在此析构：TrayIcon::drop → Shell_NotifyIcon(NIM_DELETE) +
            // DestroyWindow(隐藏窗口)；菜单 HMENU 随之释放。
        }
    }

    // ------------------------------------------------------------------
    // 托盘线程（在独立 OS 线程上构造 TrayManager 并泵消息）
    // ------------------------------------------------------------------

    /// 在托盘线程内完成的引导：建消息队列 → 构造管理器 → 回报结果 → 泵消息。
    fn run_tray_thread(
        bus: EventBus,
        all_enabled: bool,
        elevated: bool,
        cmd_rx: mpsc::Receiver<TrayCommand>,
        init_tx: mpsc::Sender<Result<(), String>>,
        thread_id: Arc<AtomicU32>,
    ) {
        // 先调用一次 PeekMessageW：确保本线程消息队列存在，之后
        // PostThreadMessageW(WM_TRAY_CONTROL / WM_QUIT) 才能可靠送达。
        let mut probe = MSG::default();
        unsafe {
            let _ = PeekMessageW(&mut probe, HWND::default(), 0, 0, PM_REMOVE);
        }
        thread_id.store(unsafe { GetCurrentThreadId() }, Ordering::Release);

        // 注册第二实例的唤醒广播编号（同一字符串系统内唯一、跨进程一致），
        // 供消息泵识别「再次启动 exe」投递来的 HWND_BROADCAST 唤醒消息。
        let wakeup_message_id = crate::single_instance::register_wakeup_message();

        let result = TrayManager::new(bus, all_enabled, elevated);
        if let Err(err) = &result {
            tracing::warn!(target: "tray", "托盘初始化失败: {err}");
        }
        // 先行回报：父线程据此判断 spawn 成败（管理器失败时不进入消息泵）。
        let outcome = result
            .as_ref()
            .map(|_| ())
            .map_err(|err| err.to_string());
        let _ = init_tx.send(outcome);

        if let Ok(manager) = result {
            manager.run(cmd_rx, wakeup_message_id);
        }
    }

    /// Windows 平台实现入口：派生托盘线程并等待其初始化结果。
    pub(super) fn spawn_impl(
        bus: EventBus,
        all_enabled: bool,
        elevated: bool,
    ) -> Result<TrayHandle, TrayError> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<TrayCommand>();
        let (init_tx, init_rx) = mpsc::channel::<Result<(), String>>();
        let thread_id = Arc::new(AtomicU32::new(0));

        let join = std::thread::Builder::new()
            .name("tltoolbox-tray".into())
            .spawn({
                let init_tx = init_tx.clone();
                let thread_id = Arc::clone(&thread_id);
                move || run_tray_thread(bus, all_enabled, elevated, cmd_rx, init_tx, thread_id)
            })
            .map_err(|source| TrayError::SpawnThread { source })?;

        // 等待托盘线程完成图标/菜单初始化（构造失败会立即回包并退出线程）。
        match init_rx.recv() {
            Ok(Ok(())) => Ok(TrayHandle {
                control: TrayControl {
                    tx: cmd_tx,
                    tray_thread_id: thread_id,
                },
                join: Some(join),
            }),
            Ok(Err(reason)) => {
                let _ = join.join(); // 线程已自行退出，join 仅为收尸
                Err(TrayError::Init { reason })
            }
            Err(_recv_error) => {
                // 线程在回报前 panic / 被系统杀死。
                Err(TrayError::Init {
                    reason: "托盘线程在初始化完成前异常退出".into(),
                })
            }
        }
    }

    // ------------------------------------------------------------------
    // 主线程 → 托盘线程的唤醒 / 停机（见 TrayControl 的调用约定）
    // ------------------------------------------------------------------

    /// 向托盘线程投递 `WM_TRAY_CONTROL`，唤醒阻塞中的 `GetMessageW`。
    pub(super) fn wake_tray_thread(thread_id: &Arc<AtomicU32>) {
        let tid = thread_id.load(Ordering::Acquire);
        if tid != 0 {
            unsafe {
                let _ = PostThreadMessageW(tid, WM_TRAY_CONTROL, WPARAM(0), LPARAM(0));
            }
        }
    }

    /// 向托盘线程投递 `WM_QUIT`（停机信号，令其消息泵返回 0 退出）。
    pub(super) fn post_wm_quit(thread_id: &Arc<AtomicU32>) {
        let tid = thread_id.load(Ordering::Acquire);
        if tid != 0 {
            unsafe {
                let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
    }

    // ------------------------------------------------------------------
    // 内嵌应用图标加载（优先路径）
    // ------------------------------------------------------------------
    //
    // build.rs 把 res/app.ico 经 winresource 编译成两级 PE 资源：
    //   - RT_GROUP_ICON(14)，名 "1"：帧目录，记录各尺寸帧（宽/高/色深）与
    //     对应 RT_ICON 资源的数字 ID（GRPICONDIRENTRY 尾部 2 字节 wID）；
    //   - RT_ICON(3)，名 = wID：单帧原始字节（<256px 为 32bpp DIB/BMP 帧，
    //     256px 一般为 PNG 压缩帧）。
    // 托盘只需 32×32 帧：读组目录 → 就近选 32bpp 帧 → 读 RT_ICON 字节 →
    // 自行解码 DIB 为 RGBA（纯函数见模块尾部，带单元测试）。任何一步失败
    // 都返回 Err，由调用方降级到 build_fallback_icon，绝不因图标问题阻断托盘。

    /// 读取当前进程模块内一个数字 ID 资源的原始字节。
    ///
    /// `name` / `r#type` 均为资源数字 ID（`MAKEINTRESOURCE` 语义）。
    unsafe fn load_resource_bytes(module: HMODULE, name: u16, r#type: u16) -> Result<Vec<u8>, String> {
        // 数字 ID → 伪指针（低 16 位即 ID，等同 MAKEINTRESOURCEW）。
        let name_ptr = PCWSTR(name as usize as *const u16);
        let type_ptr = PCWSTR(r#type as usize as *const u16);
        let hres: HRSRC = FindResourceW(module, name_ptr, type_ptr);
        if hres.is_invalid() {
            return Err(format!("FindResourceW 未找到资源 #{}（类型 #{})", name, r#type));
        }
        let hglobal: HGLOBAL = LoadResource(module, hres).map_err(|err| err.to_string())?;
        if hglobal.is_invalid() {
            return Err(format!("LoadResource 失败（资源 #{name}）"));
        }
        let size = SizeofResource(module, hres) as usize;
        let ptr = LockResource(hglobal);
        if ptr.is_null() {
            return Err(format!("LockResource 返回空指针（资源 #{name}）"));
        }
        Ok(std::slice::from_raw_parts(ptr as *const u8, size).to_vec())
    }

    /// 主入口：从 exe 内嵌图标资源取托盘用 32×32 图标。
    fn load_embedded_app_icon() -> Result<Icon, TrayError> {
        let fail = |step: &str, reason: String| TrayError::Init {
            reason: format!("exe 内嵌图标资源不可用（{step}）: {reason}"),
        };

        let (rgba, width, height) = (|| -> Result<(Vec<u8>, u32, u32), TrayError> {
            unsafe {
                // 1) exe 主模块句柄（NULL 模块名 = 当前进程可执行文件）。
                let module = GetModuleHandleW(None).map_err(|err| {
                    fail("GetModuleHandleW", format!("无法取得进程模块句柄: {err}"))
                })?;

                // 2) 读组目录并按“就近于 32px、32bpp 优先”排定候选帧。
                let group = load_resource_bytes(module, APP_ICON_GROUP_ID, RT_GROUP_ICON)
                    .map_err(|reason| fail("RT_GROUP_ICON(#1)", reason))?;
                let frame_ids = super::preferred_icon_frame_ids(&group, TRAY_ICON_PX)
                    .ok_or_else(|| {
                        fail("帧目录解析", "组图标不含任何合法帧条目".into())
                    })?;

                // 3) 依序尝试各候选帧：DIB 解码成功即用（256px PNG 帧天然解码
                //    失败、自动跳过，不影响 32px DIB 帧命中）。
                for frame_id in frame_ids {
                    let dib = load_resource_bytes(module, frame_id, RT_ICON)
                        .map_err(|reason| fail("RT_ICON 读取", reason))?;
                    if let Some(decoded) = super::decode_bmp_frame_to_rgba(&dib) {
                        return Ok(decoded);
                    }
                }
                Err(fail("RT_ICON 解码", "候选帧均非可解码的 32bpp DIB".into()))
            }
        })()?;

        Icon::from_rgba(rgba, width, height).map_err(|err| TrayError::Init {
            reason: format!("内嵌图标转托盘 Icon 失败: {err}"),
        })
    }

    // ------------------------------------------------------------------
    // 备用嵌入图标（程序化像素绘制）
    // ------------------------------------------------------------------

    /// 生成备用托盘图标：品牌蓝圆角方块 + 白色 “T” 字形。
    ///
    /// 纯代码逐像素绘制（零资源文件、零解码依赖），仅当 exe 内嵌图标资源
    /// 无法解码（如无资源构建 / 帧目录异常）时作为兜底路径被调用。
    fn build_fallback_icon() -> Result<Icon, TrayError> {
        let mut rgba = vec![0u8; ICON_SIZE * ICON_SIZE * 4];
        for y in 0..ICON_SIZE {
            for x in 0..ICON_SIZE {
                let pixel = pixel_color(x as i32, y as i32);
                if let Some((r, g, b)) = pixel {
                    let offset = (y * ICON_SIZE + x) * 4;
                    rgba[offset] = r;
                    rgba[offset + 1] = g;
                    rgba[offset + 2] = b;
                    rgba[offset + 3] = 255;
                }
            }
        }
        Icon::from_rgba(rgba, ICON_SIZE as u32, ICON_SIZE as u32).map_err(|err| {
            TrayError::Init {
                reason: format!("备用托盘图标生成失败: {err}"),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// 像素绘制纯函数（平台无关，可单测）
// ---------------------------------------------------------------------------

/// 备用图标品牌色（RGB）。
const BRAND_RGB: (u8, u8, u8) = (0x2D, 0x74, 0xE8);
/// 备用图标字形色（白色）。
const GLYPH_RGB: (u8, u8, u8) = (0xFF, 0xFF, 0xFF);

/// 判定坐标是否落在备用图标的圆角方块内（32×32 画布，内边距 4）。
pub(crate) fn inside_rounded_square(x: i32, y: i32) -> bool {
    const X0: i32 = 4;
    const X1: i32 = 27;
    const Y0: i32 = 4;
    const Y1: i32 = 27;
    const RADIUS: i32 = 5;

    // 中部十字区域（排除四角）。
    if (X0 + RADIUS..=X1 - RADIUS).contains(&x) && (Y0..=Y1).contains(&y) {
        return true;
    }
    if (Y0 + RADIUS..=Y1 - RADIUS).contains(&y) && (X0..=X1).contains(&x) {
        return true;
    }
    // 四角圆弧（1/4 圆）。
    let corners = [
        (X0 + RADIUS, Y0 + RADIUS),
        (X1 - RADIUS, Y0 + RADIUS),
        (X0 + RADIUS, Y1 - RADIUS),
        (X1 - RADIUS, Y1 - RADIUS),
    ];
    corners
        .iter()
        .any(|&(cx, cy)| (x - cx).pow(2) + (y - cy).pow(2) <= RADIUS.pow(2))
}

/// 判定坐标是否落在白色 “T” 字形内（横梁 + 中柱）。
pub(crate) fn inside_glyph_t(x: i32, y: i32) -> bool {
    // 横梁：y ∈ [8, 11]，x ∈ [9, 22]。
    let bar = (8..=11).contains(&y) && (9..=22).contains(&x);
    // 中柱：x ∈ [14, 17]，y ∈ [12, 23]。
    let stem = (12..=23).contains(&y) && (14..=17).contains(&x);
    bar || stem
}

/// 计算 32×32 备用图标上某像素的颜色；`None` = 透明。
pub(crate) fn pixel_color(x: i32, y: i32) -> Option<(u8, u8, u8)> {
    if inside_glyph_t(x, y) {
        Some(GLYPH_RGB)
    } else if inside_rounded_square(x, y) {
        Some(BRAND_RGB)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// 内嵌图标帧解析纯函数（Windows 托盘图标资源解码；单测见下方模块）
// ---------------------------------------------------------------------------

/// RT_GROUP_ICON 帧目录条目长度（GRPICONDIRENTRY = 14 字节；与磁盘 .ico 的
/// 16 字节 ICONDIRENTRY 差在结尾：组条目是 2 字节 wID 而非 4 字节数据偏移）。
#[cfg(windows)]
const GRP_ENTRY_LEN: usize = 14;
/// ICO/BMP 帧的 BITMAPINFOHEADER 长度（32bpp、无调色板）。
#[cfg(windows)]
const BMP_HEADER_LEN: usize = 40;

/// RT_GROUP_ICON 帧目录中的一条帧记录。
#[cfg(windows)]
#[derive(Clone, Copy)]
struct IconFrameEntry {
    /// 帧宽（组目录中 0 表示 256）。选帧逻辑按“就近于目标宽”使用。
    width: u32,
    /// 帧高（0 表示 256）。当前选帧只按宽就近；高度保留供校验与未来
    /// 逐监视器 DPI 选帧策略使用。
    #[allow(dead_code)]
    height: u32,
    /// 每像素位数（32 = 含 alpha 的 DIB / PNG 帧）。
    bit_count: u16,
    /// 指向 RT_ICON 资源的数字 ID。
    id: u16,
}

/// 解析 RT_GROUP_ICON 帧目录字节（布局同磁盘 .ico 的 ICONDIR，条目为组格式）。
#[cfg(windows)]
fn parse_group_entries(group: &[u8]) -> Option<Vec<IconFrameEntry>> {
    if group.len() < 6 {
        return None;
    }
    let count = u16::from_le_bytes(group[4..6].try_into().ok()?) as usize;
    if group.len() < 6 + count * GRP_ENTRY_LEN {
        return None;
    }
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let e = 6 + i * GRP_ENTRY_LEN;
        entries.push(IconFrameEntry {
            width: if group[e] == 0 { 256 } else { group[e] as u32 },
            height: if group[e + 1] == 0 { 256 } else { group[e + 1] as u32 },
            bit_count: u16::from_le_bytes(group[e + 6..e + 8].try_into().ok()?),
            id: u16::from_le_bytes(group[e + 12..e + 14].try_into().ok()?),
        });
    }
    Some(entries)
}

/// 依“32bpp 优先；就近不小于 `target` 的帧升序，全小于则大帧优先兜底”的
/// 规则，排定候选帧的 RT_ICON ID 序列。目录为空/非法返回 `None`。
#[cfg(windows)]
fn preferred_icon_frame_ids(group: &[u8], target: u32) -> Option<Vec<u16>> {
    let entries = parse_group_entries(group)?;
    if entries.is_empty() {
        return None;
    }
    // 1) 优先 32bpp 池；组内没有 32bpp 帧时退而求其次使用全部帧。
    let bpp32: Vec<&IconFrameEntry> = entries
        .iter()
        .filter(|entry| entry.bit_count == 32)
        .collect();
    let pool = if bpp32.is_empty() {
        entries.iter().collect()
    } else {
        bpp32
    };
    // 2) 排序：先试“不小于 target”的最小帧（避免把小帧放大发虚）；
    //    全部不足时按宽降序（取最大帧兜底），仍未中则整表尝试。
    let mut ordered: Vec<IconFrameEntry> = pool.into_iter().cloned().collect();
    ordered.sort_by_key(|entry| entry.width);
    let split = ordered.partition_point(|entry| entry.width < target);
    let mut ids: Vec<u16> = Vec::with_capacity(ordered.len());
    ids.extend(ordered[split..].iter().map(|entry| entry.id));
    ids.extend(ordered[..split].iter().rev().map(|entry| entry.id));
    if ids.is_empty() {
        None
    } else {
        Some(ids)
    }
}

/// 把一段 RT_ICON 的 32bpp DIB（BITMAPINFOHEADER + BGRA XOR 像素，尾部可附
/// AND 掩码，此处忽略）解码为 RGBA。仅接受 BI_RGB + 32bpp；256px 的 PNG
/// 压缩帧在此解码失败（`None`），由调用方跳过该候选。
#[cfg(windows)]
fn decode_bmp_frame_to_rgba(dib: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    if dib.len() < BMP_HEADER_LEN {
        return None;
    }
    let bi_size = u32::from_le_bytes(dib[0..4].try_into().ok()?) as usize;
    if bi_size < BMP_HEADER_LEN || dib.len() < bi_size {
        return None;
    }
    let width = i32::from_le_bytes(dib[4..8].try_into().ok()?);
    let height_raw = i32::from_le_bytes(dib[8..12].try_into().ok()?);
    if width <= 0 || height_raw == 0 {
        return None;
    }
    let bit_count = u16::from_le_bytes(dib[14..16].try_into().ok()?);
    let compression = u32::from_le_bytes(dib[16..20].try_into().ok()?);
    if bit_count != 32 || compression != 0 {
        return None;
    }
    let w = width as u32;
    let h = height_raw.unsigned_abs();
    let top_down = height_raw < 0;
    let row_bytes = w as usize * 4;
    if dib.len() < bi_size + row_bytes * h as usize {
        return None;
    }
    let mut rgba = vec![0u8; row_bytes * h as usize];
    for y in 0..h as usize {
        // ICO 内 DIB 帧默认自底向上（biHeight > 0）；自顶向下（<0）原样拷贝。
        let src_row = if top_down { y } else { h as usize - 1 - y };
        let src = bi_size + src_row * row_bytes;
        let dst = y * row_bytes;
        for x in 0..w as usize {
            let o = src + x * 4;
            let alpha = dib[o + 3];
            if alpha == 0 {
                // 全透明像素清色，避免托盘合成时残留脏色。
                rgba[dst + x * 4..dst + x * 4 + 4].fill(0);
            } else {
                rgba[dst + x * 4] = dib[o + 2]; // B → R
                rgba[dst + x * 4 + 1] = dib[o + 1];
                rgba[dst + x * 4 + 2] = dib[o]; // R → B
                rgba[dst + x * 4 + 3] = alpha;
            }
        }
    }
    Some((rgba, w, h))
}

// ---------------------------------------------------------------------------
// 单元测试（纯函数层，跨平台）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_label_flips_with_aggregate_state() {
        assert_eq!(toggle_all_label(true), MENU_LABEL_TOGGLE_OFF);
        assert_eq!(toggle_all_label(false), MENU_LABEL_TOGGLE_ON);
    }

    #[test]
    fn rounded_square_covers_center_and_excludes_corners() {
        assert!(inside_rounded_square(16, 16), "画布中心应在方块内");
        assert!(inside_rounded_square(5, 16), "左边带应在方块内");
        assert!(!inside_rounded_square(0, 0), "画布角落应透明");
        assert!(!inside_rounded_square(31, 31), "右下角应透明");
        assert!(!inside_rounded_square(16, 31), "下边缘外应透明");
    }

    #[test]
    fn glyph_t_is_drawn_inside_and_centered() {
        assert!(inside_glyph_t(15, 10), "横梁中部应着色");
        assert!(inside_glyph_t(16, 20), "中柱下部应着色");
        assert!(!inside_glyph_t(10, 20), "横梁下方两侧应留给底色");
        assert!(inside_rounded_square(10, 20), "但仍在圆角方块内（品牌蓝）");
        // 像素合成：字形内为白色，字形外方块内为品牌蓝，方块外透明。
        assert_eq!(pixel_color(15, 10), Some(GLYPH_RGB));
        assert_eq!(pixel_color(10, 20), Some(BRAND_RGB));
        assert_eq!(pixel_color(0, 0), None);
    }

    #[test]
    fn icon_canvas_is_fully_filled_within_bounds() {
        // 整幅画布上每个像素都能求出颜色（含透明），不越界不 panic。
        for y in 0..32 {
            for x in 0..32 {
                let _ = pixel_color(x, y);
            }
        }
        // 字形必须被方块完全包住（保证视觉上不“穿帮”）。
        for y in 0..32 {
            for x in 0..32 {
                if inside_glyph_t(x, y) {
                    assert!(inside_rounded_square(x, y), "字形像素必须位于方块内: ({x},{y})");
                }
            }
        }
    }

    // ---- 内嵌图标帧解析（Windows 构建产物形态的合成字节） ----

    /// 构造 RT_GROUP_ICON 帧目录字节（仅宽/色深/ID 字段参与选择逻辑）。
    #[cfg(windows)]
    fn synth_group(entries: &[(u8, u16, u16)]) -> Vec<u8> {
        // entries: (宽度字节 0=256, 色深, wID)
        let mut bytes = vec![0u8, 0, 1, 0];
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (width, bpp, id) in entries {
            bytes.push(*width); // bWidth
            bytes.push(*width); // bHeight（同宽，正方形帧）
            bytes.push(0); // bColorCount
            bytes.push(0); // bReserved
            bytes.extend_from_slice(&1u16.to_le_bytes()); // wPlanes
            bytes.extend_from_slice(&bpp.to_le_bytes()); // wBitCount
            bytes.extend_from_slice(&0u32.to_le_bytes()); // dwBytesInRes（选择逻辑不读）
            bytes.extend_from_slice(&id.to_le_bytes()); // wID
        }
        bytes
    }

    /// 构造 32bpp DIB 帧字节：`bi_height` 为正（自底向上）或负（自顶向下）；
    /// 内容为每条边沿像素的红/绿/蓝三原色 + 白角，便于校验通道与翻转。
    #[cfg(windows)]
    fn synth_dib(size: u32, bi_height: i32) -> Vec<u8> {
        let mut dib = vec![0u8; BMP_HEADER_LEN + (size as usize) * (size as usize) * 4];
        dib[0..4].copy_from_slice(&(BMP_HEADER_LEN as u32).to_le_bytes());
        dib[4..8].copy_from_slice(&(size as i32).to_le_bytes());
        dib[8..12].copy_from_slice(&bi_height.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib[16..20].copy_from_slice(&0u32.to_le_bytes()); // BI_RGB
        let row = size as usize * 4;
        for y in 0..size as usize {
            for x in 0..size as usize {
                let dst_row = if bi_height > 0 {
                    size as usize - 1 - y
                } else {
                    y
                };
                let o = BMP_HEADER_LEN + dst_row * row + x * 4;
                // BGRA：红列 / 绿行 / 蓝对角线 / 白角（右下）。
                let (r, g, b) = if x == (size as usize - 1) && y == (size as usize - 1) {
                    (255, 255, 255)
                } else if x == y {
                    (0, 0, 255)
                } else if x == size as usize - 1 {
                    (255, 0, 0)
                } else if y == size as usize - 1 {
                    (0, 255, 0)
                } else {
                    (10, 20, 30)
                };
                dib[o] = b;
                dib[o + 1] = g;
                dib[o + 2] = r;
                dib[o + 3] = 255;
            }
        }
        dib
    }

    #[cfg(windows)]
    #[test]
    fn group_picker_prefers_32bpp_then_nearest_not_smaller() {
        // 32bpp：16(#2)/32(#3)/256(#4)；24bpp：48(#5)。target=32 应首选 #3，
        // 且 256 帧（PNG、解码会失败）排在 16 帧之前仍无碍——选择只看尺寸。
        let group = synth_group(&[(16, 32, 2), (32, 32, 3), (0, 32, 4), (48, 24, 5)]);
        let ids = preferred_icon_frame_ids(&group, 32).expect("帧目录应合法");
        assert_eq!(ids[0], 3, "32px 32bpp 帧应排在首位");
        // 无 32bpp 时退回全部帧，且大帧（48）优先于小帧兜底。
        let group24 = synth_group(&[(16, 24, 2), (48, 24, 5)]);
        let ids24 = preferred_icon_frame_ids(&group24, 32).expect("帧目录应合法");
        assert_eq!(ids24[0], 5, "48px 24bpp 应作为最接近 32px 的兜底首选");
        // 非法目录（截断）→ None。
        assert!(preferred_icon_frame_ids(&[0u8, 0], 32).is_none());
        assert!(parse_group_entries(&[0u8; 0]).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn dib_decoder_flips_rows_and_swaps_channels() {
        for (size, bi_height) in [(2u32, 2i32), (2, -2), (3, 3)] {
            let dib = synth_dib(size, bi_height);
            let (rgba, w, h) = decode_bmp_frame_to_rgba(&dib).expect("32bpp DIB 应可解码");
            assert_eq!((w, h), (size, size));
            let at = |x: usize, y: usize| {
                let o = (y * size as usize + x) * 4;
                (rgba[o], rgba[o + 1], rgba[o + 2], rgba[o + 3])
            };
            let last = size as usize - 1;
            // 右下角白（合成器先判白角，再判对角线）。
            assert_eq!(at(last, last), (255, 255, 255, 255), "右下角应为白");
            // 右列红 / 底行绿：验证 BGRA → RGBA 通道互换 + 自底向上翻转到原位。
            assert_eq!(at(last, 0), (255, 0, 0, 255), "右列应为红");
            assert_eq!(at(0, last), (0, 255, 0, 255), "底行应为绿");
            // 主对角线蓝（含 (0,0)）。
            assert_eq!(at(0, 0), (0, 0, 255, 255), "(0,0) 位于对角线应为蓝");
            if size >= 3 {
                assert_eq!(at(1, 1), (0, 0, 255, 255), "内部对角像素应为蓝");
                assert_eq!(at(1, 0), (10, 20, 30, 255), "内部普通像素保持底色");
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn dib_decoder_rejects_non_32bpp_and_truncated() {
        // 非 32bpp（24bpp）与截断字节 → None。
        let mut dib = synth_dib(2, 2);
        dib[14] = 24;
        assert!(decode_bmp_frame_to_rgba(&dib).is_none());
        let mut short = synth_dib(2, 2);
        short.truncate(30);
        assert!(decode_bmp_frame_to_rgba(&short).is_none());
        // PNG 魔数开头的 256 帧（非 DIB）→ None。
        let png_like = [0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        assert!(decode_bmp_frame_to_rgba(&png_like).is_none());
    }
}
