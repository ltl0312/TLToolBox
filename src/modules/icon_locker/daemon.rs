//! Display topology fingerprinting and debounced display-change daemon.
//!
//! - [`topology_fingerprint`]：把各显示器（主屏优先、按 left/top/width/height 升序）
//!   归一化为一条唯一字符串，作为「绑定了哪套物理屏幕」的稳定标识；
//! - [`DisplayChangeWatchdog`]：专用原生线程运行消息泵，监听 `WM_DISPLAYCHANGE`；
//!   每次收到该消息都**重新武装**一枚 1500ms 一次性定时器（`SetTimer` 同 ID 重复
//!   调用天然重置计时的防抖语义），拓扑稳定 1500ms 后触发一次回调（自动还原），
//!   绝不在高频显示事件风暴中重复执行。

use std::fmt::Write;
use std::time::Duration;

/// 显示器拓扑变化后的防抖窗口（1500ms，可重置：连续变化会顺延到稳定后触发）。
pub const DISPLAY_DEBOUNCE: Duration = Duration::from_millis(1500);

/// 守护线程停机时 `Drop` 等待 join 的上限（v0.6.1 · S4）。
///
/// 该常量是"停机永不挂死"这一硬约束的落点：即使消息泵因任何原因未响应
/// `WM_CLOSE`（窗口创建异常、消息队列被外部阻塞等），`stop()` 也必须在有限时间内
/// 返回——绝不允许它在 async 上下文里永久阻塞一个 Tokio 工作线程并挡住进程收尾。
pub const STOP_JOIN_TIMEOUT: Duration = Duration::from_millis(2000);

/// 防抖定时器在消息泵里的 ID（`SetTimer` 载荷；'TL' 占位）。
#[cfg(windows)]
const DEBOUNCE_TIMER_ID: usize = 0x544C;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorRect {
    pub primary: bool,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

/// 按 (主屏优先, left, top, width, height) 排序并拼接为唯一拓扑指纹。
pub fn topology_fingerprint(monitors: &mut [MonitorRect]) -> String {
    monitors.sort_by_key(|m| (!m.primary, m.left, m.top, m.width, m.height));
    let mut result = String::new();
    for (index, monitor) in monitors.iter().enumerate() {
        if index > 0 {
            result.push('|');
        }
        let _ = write!(
            result,
            "{}:{},{},{}x{}",
            if monitor.primary { 'P' } else { 'S' },
            monitor.left,
            monitor.top,
            monitor.width,
            monitor.height
        );
    }
    result
}

/// 采集当前真实显示器拓扑并计算指纹。
#[cfg(windows)]
pub fn current_topology_fingerprint() -> String {
    use std::sync::Mutex;
    use windows::Win32::Foundation::{BOOL, LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
    };
    use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
    let monitors = Mutex::new(Vec::new());
    /// `EnumDisplayMonitors` 同步回调（**FFI 边界**：S5 起整体置于 `guard_ffi` 内）。
    ///
    /// 回调体含 `Mutex` 加锁与 `Vec` 增长（均可 panic）；panic 一旦跨
    /// `extern "system"` 展开会直接 abort 常驻进程。此处截停后返回 `BOOL(1)`
    /// 继续枚举——仅丢失本次显示器采集，不影响进程存活（顶层 `unwrap_or_default`
    /// 会退化为空指纹）。
    unsafe extern "system" fn callback(
        monitor: HMONITOR,
        _dc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        crate::ffi_guard::guard_ffi("icon_locker::EnumDisplayMonitors", || {
            let monitors = &*(data.0 as *const Mutex<Vec<MonitorRect>>);
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(monitor, &mut info).as_bool() {
                let r = info.rcMonitor;
                let _ = monitors.lock().map(|mut list| {
                    list.push(MonitorRect {
                        primary: (info.dwFlags & MONITORINFOF_PRIMARY) != 0,
                        left: r.left,
                        top: r.top,
                        width: (r.right - r.left).max(0) as u32,
                        height: (r.bottom - r.top).max(0) as u32,
                    })
                });
            }
            BOOL(1)
        })
        .unwrap_or(BOOL(1)) // panic 降级：继续枚举，放弃本次采集
    }
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(callback),
            LPARAM(&monitors as *const _ as isize),
        );
    }
    let mut monitors = monitors.into_inner().unwrap_or_default();
    topology_fingerprint(&mut monitors)
}

#[cfg(not(windows))]
pub fn current_topology_fingerprint() -> String {
    String::new()
}

// ---------------------------------------------------------------------------
// WM_DISPLAYCHANGE 防抖守护线程
// ---------------------------------------------------------------------------

/// `HWND` 的线程安全包装。
///
/// 句柄仅在跨线程时作为**整数**传给 `PostMessageW`（该 API 任意线程安全），
/// 从不跨线程解引用窗口指针，故可安全地手动实现 `Send + Sync`——这使
/// `DisplayChangeWatchdog` 可被 [`IconLockerModule`](crate::modules::icon_locker::IconLockerModule)
/// 持有而不破坏 `ToolModule: Send + Sync` 契约。
#[cfg(windows)]
struct SendHwnd(windows::Win32::Foundation::HWND);
#[cfg(windows)]
unsafe impl Send for SendHwnd {}
#[cfg(windows)]
unsafe impl Sync for SendHwnd {}

/// 显示器拓扑变化守护句柄（Windows 专用实现；非 Windows 为无操作占位）。
pub struct DisplayChangeWatchdog {
    #[cfg(windows)]
    hwnd: SendHwnd,
    #[cfg(windows)]
    thread: Option<std::thread::JoinHandle<()>>,
}

/// 带超时的线程 join（v0.6.1 · S4）：超时返回 `false`。
///
/// `std::thread::JoinHandle` 没有原生超时 join，故用一个短命辅助线程承载阻塞式
/// `join()`，主线程经通道 `recv_timeout` 等待。超时即放弃等待——被 join 的线程
/// 会在进程退出时被操作系统回收；**宁可泄漏一个已无窗口可服务、必然已无副作用的
/// 线程，也绝不允许停机路径永久阻塞**（旧实现正是在 async 上下文里裸 `join`，
/// 一旦窗口创建失败即永久挂死整个应用收尾）。
#[cfg(windows)]
fn join_with_timeout(handle: std::thread::JoinHandle<()>, timeout: Duration) -> bool {
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let spawned = std::thread::Builder::new()
        .name("tlt-icon-locker-join-guard".to_string())
        .spawn(move || {
            let _ = handle.join();
            let _ = done_tx.send(());
        });
    if spawned.is_err() {
        // 辅助线程都派生失败：句柄随闭包析构（线程被 detach），直接判定超时。
        return false;
    }
    done_rx.recv_timeout(timeout).is_ok()
}

#[cfg(windows)]
impl DisplayChangeWatchdog {
    /// 派生守护线程：注册隐藏消息窗口并启动消息泵。
    ///
    /// 回调在防抖过期（1500ms 无新 `WM_DISPLAYCHANGE`）后于**守护线程**内执行；
    /// 调用方应保证回调内部是短促的消息投递（如经通道转交 Tokio 任务再执行 COM）。
    ///
    /// # 失败语义（v0.6.1 · S4）
    /// 窗口创建失败**不再静默降级为"运行中"**：守护线程经 `ready` 通道回传
    /// `Err(原因)` 后直接返回（不进入消息泵），`spawn` 随即 `join` 收尸并把错误
    /// 向上抛给模块 `start()`——模块据此保持"停止态"并告警，绝不出现"显示运行中
    /// 却毫无显示器响应"的假象。
    pub fn spawn<F>(on_restore: F) -> std::io::Result<Self>
    where
        F: Fn() + Send + 'static,
    {
        use std::sync::mpsc;
        let (ready_tx, ready_rx) = mpsc::channel::<Result<isize, String>>();
        let handle = std::thread::Builder::new()
            .name("icon-locker-display-watchdog".to_string())
            .spawn(move || pump::run_pump(Box::new(on_restore), ready_tx))?;

        // 初始化结果可能是 Ok(句柄) / Err(原因) / 通道关闭（线程在回报前 panic）。
        // 三种形态都不进入消息泵，故 join 必为瞬时完成，不存在挂死风险。
        let outcome = ready_rx.recv().unwrap_or_else(|_| {
            Err("显示器守护线程在回报初始化结果前退出（通道关闭）".to_string())
        });
        match outcome {
            Ok(hwnd) => Ok(Self {
                hwnd: SendHwnd(windows::Win32::Foundation::HWND(
                    hwnd as *mut core::ffi::c_void,
                )),
                thread: Some(handle),
            }),
            Err(reason) => {
                let _ = handle.join();
                Err(std::io::Error::other(reason))
            }
        }
    }

    /// 请求守护线程退出（`WM_CLOSE` → 销毁窗口 → `PostQuitMessage`；`Drop` 时 Join）。
    ///
    /// 句柄恒为有效窗口（构造期已判空，见 [`Self::spawn`]），故投递必然有接收方。
    pub fn request_stop(&mut self) {
        let hwnd = self.hwnd.0;
        if !hwnd.0.is_null() {
            let _ = unsafe {
                windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    hwnd,
                    windows::Win32::UI::WindowsAndMessaging::WM_CLOSE,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(0),
                )
            };
        }
    }
}

#[cfg(windows)]
impl Drop for DisplayChangeWatchdog {
    fn drop(&mut self) {
        self.request_stop();
        if let Some(handle) = self.thread.take() {
            if !join_with_timeout(handle, STOP_JOIN_TIMEOUT) {
                // 超时不再无限等待（S4 的核心修复）：告警留痕后放弃 join。
                tracing::error!(
                    target: "icon_locker",
                    "显示器守护线程在 {}ms 内未退出，放弃 join 以避免停机挂死（线程将在进程退出时被系统回收）",
                    STOP_JOIN_TIMEOUT.as_millis()
                );
            }
        }
    }
}

#[cfg(not(windows))]
impl DisplayChangeWatchdog {
    pub fn spawn<F>(_on_restore: F) -> std::io::Result<Self>
    where
        F: Fn() + Send + 'static,
    {
        Ok(Self)
    }

    pub fn request_stop(&mut self) {}
}

#[cfg(windows)]
mod pump {
    use super::DEBOUNCE_TIMER_ID;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
        GetWindowLongPtrW, KillTimer, PostQuitMessage, RegisterClassW, SetTimer, SetWindowLongPtrW,
        TranslateMessage, UnregisterClassW, GWLP_USERDATA, HWND_MESSAGE, MSG, WINDOW_EX_STYLE,
        WINDOW_LONG_PTR_INDEX, WM_CLOSE, WM_DESTROY, WM_DISPLAYCHANGE, WM_TIMER, WNDCLASSW,
        WS_OVERLAPPED,
    };

    /// 消息泵体：注册消息窗口类 → 创建 HWND_MESSAGE 隐藏窗口 → 把回调指针存入
    /// GWLP_USERDATA → 向调用方回传句柄 → 进入 GetMessageW 循环。窗口销毁时回收回调。
    ///
    /// # 失败语义（v0.6.1 · S4）
    /// `CreateWindowExW` 失败（或返回空句柄）时**必须立刻经 `ready` 回传 `Err`
    /// 并返回**：旧实现 `unwrap_or_default()` 会把失败变成空 HWND，却仍然
    /// `ready.send(0)` 并进入 `GetMessageW` —— 该线程没有窗口、也没人会向它投递
    /// `WM_QUIT`，`GetMessageW` 永不返回，`Drop` 里的 `join` 于是永久挂死
    /// （而 `drop` 发生在 async 上下文，等于拖死应用收尾 + 占死一个 Tokio worker）。
    pub(super) fn run_pump(
        callback: Box<dyn Fn() + Send>,
        ready: std::sync::mpsc::Sender<Result<isize, String>>,
    ) {
        unsafe {
            // 守护线程本身也按 STA 初始化（回调若直接做 COM，保证单元环境就绪）。
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

            let class_name = PCWSTR(windows::core::w!("TLToolBoxIconLockerWatchdog").as_ptr());
            let instance = GetModuleHandleW(None).unwrap_or_default();
            let wc = WNDCLASSW {
                style: Default::default(),
                lpfnWndProc: Some(window_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance.into(),
                hIcon: Default::default(),
                hCursor: Default::default(),
                hbrBackground: Default::default(),
                lpszMenuName: PCWSTR::null(),
                lpszClassName: class_name,
            };
            // v0.6.2（L17）：`RegisterClassW` 的返回值是**窗口类 atom**；旧实现
            // 直接丢弃，且线程退出后从不 `UnregisterClassW`——每次 start/stop 循环
            // 都泄漏一个 atom（系统全局表有限，长驻反复启停会累积）。现检查返回值
            // （失败时 CreateWindowExW 必然失败，S4 的回传错误已能暴露），退出路径
            // 成对注销。
            let class_atom = RegisterClassW(&wc);

            // —— S4 关键点：窗口创建必须判空，失败即回传错误并返回（不进消息泵）——
            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class_name,
                PCWSTR::null(),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                None,
                instance,
                None,
            ) {
                Ok(hwnd) if !hwnd.0.is_null() => hwnd,
                Ok(_) => {
                    let code = windows::Win32::Foundation::GetLastError().0;
                    let _ = ready.send(Err(format!(
                        "CreateWindowExW 返回空窗口句柄（Win32 错误码 {code}）"
                    )));
                    return; // 不进入 GetMessageW：避免无窗口的线程永久挂死
                }
                Err(err) => {
                    let _ = ready.send(Err(format!("CreateWindowExW 失败: {err}")));
                    return;
                }
            };

            // 回调所有权移交窗口用户数据；WM_DESTROY 时取回归还（Box::from_raw）。
            // 双重装箱：外层 Box<trait 对象> 的裸指针是**瘦指针**，可经 isize 存入
            // GWLP_USERDATA（直接 Box::into_raw(trait 对象) 得到的是 fat 指针，无法转换）。
            let raw = Box::into_raw(Box::new(callback));
            let _ = SetWindowLongPtrW(hwnd, WINDOW_LONG_PTR_INDEX(GWLP_USERDATA.0), raw as isize);

            // HWND 经整数跨线程传递（条件为：就绪通道只传裸值，不传 HWND 结构）。
            let _ = ready.send(Ok(hwnd.0 as isize));

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageW(&msg);
            }

            // v0.6.2（L17）：与 RegisterClassW 成对注销（仅当注册成功，atom 非零），
            // 杜绝 start/stop 反复循环下的窗口类 atom 泄漏。窗口此时已随 WM_CLOSE
            // 销毁，类无引用，注销必然安全。
            if class_atom != 0 {
                let _ = UnregisterClassW(class_name, instance);
            }
            CoUninitialize();
        }
    }

    /// 窗口过程（**FFI 边界**：S5 起整体置于 `guard_ffi` 内）。
    ///
    /// 回调解引用 GWLP_USERDATA 中的回调对象并调用用户逻辑（可 panic）；panic 一旦
    /// 跨 `extern "system"` 展开会 abort 整个常驻进程。此处截停后转发
    /// `DefWindowProcW` 兜底——防抖定时器最多丢一拍，进程与消息泵保持存活。
    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        crate::ffi_guard::guard_ffi("icon_locker::watchdog_window_proc", || {
            match msg {
                WM_DISPLAYCHANGE => {
                    // 重新武装防抖定时器：同 ID 的 SetTimer 覆盖上次设定，实现「可重置」。
                    let _ = SetTimer(
                        hwnd,
                        DEBOUNCE_TIMER_ID,
                        super::DISPLAY_DEBOUNCE.as_millis() as u32,
                        None,
                    );
                    LRESULT(0)
                }
                WM_TIMER if wparam.0 == DEBOUNCE_TIMER_ID => {
                    let _ = KillTimer(hwnd, DEBOUNCE_TIMER_ID);
                    let raw = GetWindowLongPtrW(hwnd, WINDOW_LONG_PTR_INDEX(GWLP_USERDATA.0));
                    if raw != 0 {
                        let callback = &*(raw as *const Box<dyn Fn() + Send>);
                        callback();
                    }
                    LRESULT(0)
                }
                WM_CLOSE => {
                    let _ = DestroyWindow(hwnd);
                    LRESULT(0)
                }
                WM_DESTROY => {
                    // 回收回调（防泄漏）；随后 PostQuitMessage 令 GetMessageW 返回退出。
                    let raw = GetWindowLongPtrW(hwnd, WINDOW_LONG_PTR_INDEX(GWLP_USERDATA.0));
                    if raw != 0 {
                        drop(Box::from_raw(raw as *mut Box<dyn Fn() + Send>));
                    }
                    PostQuitMessage(0);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        })
        .unwrap_or_else(|| DefWindowProcW(hwnd, msg, wparam, lparam))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn primary_and_geometry_order_is_stable() {
        let mut monitors = [
            MonitorRect {
                primary: false,
                left: -1920,
                top: 0,
                width: 1920,
                height: 1080,
            },
            MonitorRect {
                primary: true,
                left: 0,
                top: 0,
                width: 2560,
                height: 1440,
            },
            MonitorRect {
                primary: false,
                left: 2560,
                top: 0,
                width: 1920,
                height: 1080,
            },
        ];
        assert_eq!(
            topology_fingerprint(&mut monitors),
            "P:0,0,2560x1440|S:-1920,0,1920x1080|S:2560,0,1920x1080"
        );
    }

    #[test]
    fn debounce_window_is_exactly_1500ms() {
        assert_eq!(DISPLAY_DEBOUNCE, Duration::from_millis(1500));
    }

    // ---- S4：守护线程创建失败 / 停机挂死（v0.6.1 整改） ----

    /// 停机等待上限必须是一个有界的短常量（S4 的硬约束落点）。
    #[test]
    fn stop_join_timeout_is_bounded() {
        assert!(
            STOP_JOIN_TIMEOUT <= Duration::from_secs(5),
            "停机 join 上限必须足够短，否则 async 上下文的 stop() 仍会长时间阻塞收尾"
        );
        assert!(!STOP_JOIN_TIMEOUT.is_zero(), "上限不得为 0（否则永不等待）");
    }

    /// 带超时的 join 语义：正常线程在超时前完成（`true`），卡死线程到点即返回
    /// （`false`，且**不**长时间阻塞调用方）——这是 `stop()` 不再挂死的直接保障。
    #[cfg(windows)]
    #[test]
    fn join_with_timeout_never_blocks_forever() {
        let finished = std::thread::spawn(|| {});
        assert!(
            join_with_timeout(finished, Duration::from_secs(2)),
            "已结束的线程应被正常 join"
        );

        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let stuck = std::thread::spawn(move || {
            let _ = release_rx.recv(); // 模拟永不退出的消息泵
        });
        let started = std::time::Instant::now();
        assert!(
            !join_with_timeout(stuck, Duration::from_millis(200)),
            "卡死线程必须在超时后放弃等待"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "带超时的 join 不得长期阻塞调用方，实际耗时 {:?}",
            started.elapsed()
        );
        let _ = release_tx.send(()); // 放行，避免测试进程残留线程
    }

    /// 真实守护线程的 spawn → stop → drop 全链路必须有界完成（无桌面会话时跳过）。
    #[cfg(windows)]
    #[test]
    fn watchdog_spawn_stop_drop_is_bounded() {
        let started = std::time::Instant::now();
        match DisplayChangeWatchdog::spawn(|| {}) {
            Ok(mut watchdog) => {
                watchdog.request_stop();
                drop(watchdog);
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "spawn + stop + drop 必须在有限时间内完成，实际 {:?}",
                    started.elapsed()
                );
            }
            Err(err) => eprintln!("当前环境无法创建消息窗口，跳过真实守护线程回归: {err}"),
        }
    }
}
