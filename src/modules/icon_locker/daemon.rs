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
    unsafe extern "system" fn callback(
        monitor: HMONITOR,
        _dc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
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

#[cfg(windows)]
impl DisplayChangeWatchdog {
    /// 派生守护线程：注册隐藏消息窗口并启动消息泵。
    ///
    /// 回调在防抖过期（1500ms 无新 `WM_DISPLAYCHANGE`）后于**守护线程**内执行；
    /// 调用方应保证回调内部是短促的消息投递（如经通道转交 Tokio 任务再执行 COM）。
    pub fn spawn<F>(on_restore: F) -> std::io::Result<Self>
    where
        F: Fn() + Send + 'static,
    {
        use std::sync::mpsc;
        let (ready_tx, ready_rx) = mpsc::channel::<isize>();
        let handle = std::thread::Builder::new()
            .name("icon-locker-display-watchdog".to_string())
            .spawn(move || pump::run_pump(Box::new(on_restore), ready_tx))?;
        let hwnd = ready_rx
            .recv()
            .map_err(|_| std::io::Error::other("显示器守护线程初始化失败：窗口句柄通道关闭"))?;
        Ok(Self {
            hwnd: SendHwnd(windows::Win32::Foundation::HWND(
                hwnd as *mut core::ffi::c_void,
            )),
            thread: Some(handle),
        })
    }

    /// 请求守护线程退出（`WM_CLOSE` → 销毁窗口 → `PostQuitMessage`；`Drop` 时 Join）。
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
            let _ = handle.join();
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
        TranslateMessage, GWLP_USERDATA, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_LONG_PTR_INDEX,
        WM_CLOSE, WM_DESTROY, WM_DISPLAYCHANGE, WM_TIMER, WNDCLASSW, WS_OVERLAPPED,
    };

    /// 消息泵体：注册消息窗口类 → 创建 HWND_MESSAGE 隐藏窗口 → 把回调指针存入
    /// GWLP_USERDATA → 向调用方回传句柄 → 进入 GetMessageW 循环。窗口销毁时回收回调。
    pub(super) fn run_pump(callback: Box<dyn Fn() + Send>, ready: std::sync::mpsc::Sender<isize>) {
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
            let _ = RegisterClassW(&wc);

            let hwnd = CreateWindowExW(
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
            )
            .unwrap_or_default();

            // 回调所有权移交窗口用户数据；WM_DESTROY 时取回归还（Box::from_raw）。
            // 双重装箱：外层 Box<trait 对象> 的裸指针是**瘦指针**，可经 isize 存入
            // GWLP_USERDATA（直接 Box::into_raw(trait 对象) 得到的是 fat 指针，无法转换）。
            let raw = Box::into_raw(Box::new(callback));
            let _ = SetWindowLongPtrW(hwnd, WINDOW_LONG_PTR_INDEX(GWLP_USERDATA.0), raw as isize);

            // HWND 经整数跨线程传递（条件为：就绪通道只传裸值，不传 HWND 结构）。
            let _ = ready.send(hwnd.0 as isize);

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageW(&msg);
            }
            CoUninitialize();
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
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
}
