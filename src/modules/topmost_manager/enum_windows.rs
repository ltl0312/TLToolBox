//! # Win32 顶层窗口轻量枚举与严苛过滤（v0.4.1 · 全局窗口置顶守护）
//!
//! 经 [`EnumWindows`] 一次性遍历当前桌面会话的全部顶层窗口，逐窗口完成
//! 六重严苛过滤与元数据采集，产出供 UI 弹窗列表与置顶引擎使用的
//! [`WindowInfo`] 快照：
//!
//! 1. **可见性 + 有标题**：`IsWindowVisible` 为真且 `GetWindowTextLengthW > 0`
//!    ——不可见窗口（隐藏到托盘、后台守护）与无标题窗口（纯工具容器）无置顶意义；
//! 2. **工具条剥离**：排除带 `WS_EX_TOOLWINDOW` 且未带 `WS_EX_APPWINDOW` 的窗口
//!    （工具窗不占任务栏、多为辅助浮层）；显式声明过 `WS_EX_APPWINDOW` 的窗口
//!    即使带工具条扩展样式也保留——它代表用户可见的独立应用本体；
//! 3. **DWM 挂起 / 虚拟桌面剥离**：`DwmGetWindowAttribute(DWMWA_CLOAKED)` 非 0 的
//!    窗口（被 DWM 隐藏——如虚拟桌面切换后隐藏、UWP 挂起等）一律剔除。注意该调用
//!    在无 DWM 合成（远程桌面会话、旧回退驱动）时返回 `E_NOTIMPL` / `S_FALSE`，
//!    此时按“不隐藏”放行（保守语义：宁可多列一个窗口，不可漏掉用户窗口）；
//! 4. **最小化剥离**（v0.4.1）：`IsIconic` 为真的窗口（最小化到任务栏）一律
//!    不出现在候选列表——置顶对最小化窗口无视觉意义，且最小化触发的自动解置顶
//!    会与列表展示产生时序噪音；
//! 5. **自身剥离**：排除 TLToolBox 自己的主窗口（防止用户把工具本体置顶造成
//!    “自己钉自己”的循环）；
//! 6. **桌面容器剥离**：排除系统任务栏（`Shell_TrayWnd`）、桌面（`Progman`）、
//!    开始按钮（`Button`）等桌面背景容器——它们属于 Shell 而非用户窗口。
//!
//! 采集字段：进程可执行文件名（`GetWindowThreadProcessId` + `OpenProcess` +
//! `QueryFullProcessImageNameW`）、标题（`GetWindowTextW`）、类名（`GetClassNameW`
//! 供容器剥离）、当前是否已带 `WS_EX_TOPMOST`（`GetWindowLongPtrW(GWL_EXSTYLE)`，
//! 供 UI 弹窗初始回显“已置顶”）。
//!
//! # 实现注意
//!
//! - **低内存、零图标**：本模块只产出纯文本快照（进程名 / 标题 / 类名 / 句柄值），
//!   绝不在列表侧加载窗口图标——`GetClassLongPtrW(GCL_HICON)` / `SendMessageW
//!   (WM_GETICON)` 等图标管线会为每个窗口解码 32bpp 位图，数百窗口即可撑爆
//!   Slint UI 的图形显存预算（见任务书“严禁加载图标以免爆显存”）；
//! - **句柄以 `isize` 传递**：Win32 `HWND` 不实现 `Send`（句柄语义为线程亲缘
//!   指针），跨线程 / 跨 Tokio 任务传递统一取 `hwnd.0 as isize` 裸值，回程经
//!   [`to_hwnd`] 还原（`as *mut c_void` 窄化由系统句柄空间保证——句柄值恒为
//!   内核对象表索引，宽度恒等于指针宽）；
//! - **UIPI 影响**：`QueryFullProcessImageNameW` 经 `PROCESS_QUERY_LIMITED_INFORMATION`
//!   打开进程，对任意完整性级别进程均可行；对已退出的进程（窗口句柄残留窗口）打开
//!   失败时进程名回退为 `<unknown.exe>`，标题仍正常采集；
//! - **回调约定**：`EnumWindows` 回调是 `unsafe extern "system" fn`，本模块经
//!   一个仅限枚举窗口内短暂存活的状态槽（线程局部变量）把回调收集到的条目
//!   逐条搬回收集向量——每次 [`enumerate_top_level_windows`] 调用都独占
//!   单次枚举窗口，槽内条目在回调返回后立即取走，不存在跨调用残留。

use std::ffi::c_void;

#[cfg(windows)]
use windows::Win32::{
    Foundation::{BOOL, HWND, LPARAM},
    Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED},
    System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    },
    UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW,
        GetWindowThreadProcessId, IsIconic, IsWindowVisible, GWL_EXSTYLE, WS_EX_APPWINDOW,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    },
};

/// 顶层窗口枚举过滤后采集到的元数据快照（纯文本，供 UI 列表与置顶引擎共用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    /// 窗口句柄裸值（还原为 `HWND` 见 [`to_hwnd`]）。
    pub hwnd: isize,
    /// 窗口所属进程的可执行文件名（`notepad.exe`；查询失败时 `<unknown.exe>`）。
    pub process_name: String,
    /// 窗口标题（UTF-8 无损转换）。
    pub title: String,
    /// 窗口类名（供 Shell 容器剥离诊断与调试）。
    pub class_name: String,
    /// 当前扩展样式是否已带 `WS_EX_TOPMOST`（UI 弹窗据此回显置顶状态）。
    pub topmost: bool,
}

/// 系统任务栏窗口的类名。
const CLASS_SHELL_TRAYWND: &str = "Shell_TrayWnd";
/// Windows 桌面（壁纸承载）窗口的类名。
const CLASS_PROGMAN: &str = "Progman";
/// 开始按钮（任务栏左侧）的类名。
const CLASS_SHELL_BUTTON: &str = "Button";
/// 部分系统把桌面承载在 WorkerW 下（Progman 的旧形态），一并排除。
const CLASS_WORKERW: &str = "WorkerW";

/// 进程可执行文件查询失败（进程已退出 / 权限受限）时的回退名。
const UNKNOWN_PROCESS: &str = "<unknown.exe>";

/// 把 `isize` 裸值还原为 Win32 `HWND`。
///
/// # Safety 说明
/// 系统句柄值（含窗口句柄）恒为内核对象表索引且宽度等于指针宽；`isize` →
/// `*mut c_void` 的窄化保持位型不变，还原后仅用于 Win32 API 的只读查询 /
/// `IsWindow` 校验，不构成 Rust 指针别名或解引用。
#[allow(non_snake_case)]
pub fn to_hwnd(value: isize) -> HWND {
    HWND(value as *mut c_void)
}

// 单次 `EnumWindows` 枚举的状态槽：收集回调产出的条目。
// 用线程局部变量承载是因为 `EnumWindows` 回调为裸 `unsafe extern "system" fn`，
// 无法捕获收集向量；每次 [`enumerate_top_level_windows`] 调用独占一次枚举窗口，
// 先清空槽位再开枚举、回调返回后立即取走——槽内条目不会跨调用残留或泄漏。
#[cfg(windows)]
thread_local! {
    static ENUM_BUCKET: std::cell::RefCell<Vec<WindowInfo>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// 供 [`to_hwnd`] 之外把 HWND 转回裸值（回调与日志共用）。
#[cfg(windows)]
pub fn hwnd_value(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

/// 本进程（TLToolBox）自身主窗口的类名排除名单——由 [`is_self_window`] 消费。
///
/// 弹窗遮罩层的 Slint 窗口通常以 `slint` 前缀注册（winit 后端把每条顶层窗口
/// 挂到该类别下）；UI 主窗口标题固定为 `TLToolBox - 桌面实用工具箱`。为避免把
/// 工具本体自身纳入可置顶列表，两条判据任一命中即判定为自身窗口。
const OWN_TITLE_MARKER: &str = "TLToolBox";

/// 判定窗口是否属于 TLToolBox 自身（标题以产品名开头 / 类名含 slint 标记）。
fn is_self_window(title: &str, class_name: &str) -> bool {
    title.starts_with(OWN_TITLE_MARKER)
        || class_name.to_ascii_lowercase().contains("slint")
}

/// 判定一个窗口是否属于系统桌面容器（任务栏 / 桌面 / 开始按钮等）。
///
/// 这些窗口属于 Shell 的桌面背景承载，把它们列入“可置顶窗口”既无意义也会
/// 制造噪音（任务栏无法被普通置顶语义覆盖）。类名匹配忽略大小写。
fn is_desktop_shell_container(class_name: &str) -> bool {
    [
        CLASS_SHELL_TRAYWND,
        CLASS_PROGMAN,
        CLASS_WORKERW,
        CLASS_SHELL_BUTTON,
    ]
    .iter()
    .any(|candidate| class_name.eq_ignore_ascii_case(candidate))
}

/// 读取窗口的扩展样式（`GWL_EXSTYLE`）；查询失败返回 0。
///
/// 0 无歧义：`WS_EX_*` 标志位在 0 上全部为假——失败语义与“无任何扩展样式”
/// 等价，不会造成误判（顶多把个别异常窗口当作非置顶窗口处理）。
#[cfg(windows)]
fn get_ex_style(hwnd: HWND) -> u32 {
    // SAFETY: GetWindowLongPtrW 为只读查询；返回值按窗口句柄语义使用。
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 }
}

/// 读取窗口所属进程的可执行文件名（`notepad.exe` 形态）。
///
/// 流程：`GetWindowThreadProcessId` 取 PID → `OpenProcess`（仅
/// `PROCESS_QUERY_LIMITED_INFORMATION`，对高完整性进程亦可行）→
/// `QueryFullProcessImageNameW` 取完整路径 → 取末级文件名。
/// 任一步失败（进程已退出 / 句柄受限）返回 [`UNKNOWN_PROCESS`]。
#[cfg(windows)]
fn process_name_of(hwnd: HWND) -> String {
    unsafe {
        let mut pid = 0u32;
        if GetWindowThreadProcessId(hwnd, Some(&mut pid)) == 0 || pid == 0 {
            return UNKNOWN_PROCESS.to_string();
        }
        // PROCESS_QUERY_LIMITED_INFORMATION 是最小权限集：可读映像路径，
        // 对提升进程同样可打开（QueryFullProcessImageNameW 不受 UIPI 拦截）。
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(handle) => {
                let mut buf = [0u16; 1024];
                let mut size = buf.len() as u32;
                let ok = QueryFullProcessImageNameW(
                    handle,
                    PROCESS_NAME_WIN32,
                    windows::core::PWSTR(buf.as_mut_ptr()),
                    &mut size,
                );
                // 句柄为内核句柄表条目，无 RAII 析构需求；显式释放避免句柄表
                // 在连续枚举中膨胀。
                let _ = windows::Win32::Foundation::CloseHandle(handle);
                if ok.is_ok() && size > 0 {
                    let path = String::from_utf16_lossy(&buf[..size as usize]);
                    let name = path.rsplit(['/', '\\']).next().unwrap_or(UNKNOWN_PROCESS);
                    if name.is_empty() {
                        UNKNOWN_PROCESS.to_string()
                    } else {
                        name.to_string()
                    }
                } else {
                    UNKNOWN_PROCESS.to_string()
                }
            }
            Err(_) => UNKNOWN_PROCESS.to_string(),
        }
    }
}

/// 读取窗口标题（UTF-16 → 无损 UTF-8）。
#[cfg(windows)]
fn title_of(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd).max(0) as usize;
        if len == 0 {
            return String::new();
        }
        // +1 容纳终止符；GetWindowTextW 返回的字节数不含终止符。
        let mut buf = vec![0u16; len + 1];
        let written = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
        String::from_utf16_lossy(&buf[..written.min(len)])
    }
}

/// 读取窗口类名（UTF-16 → 无损 UTF-8；失败返回空串）。
#[cfg(windows)]
fn class_name_of(hwnd: HWND) -> String {
    unsafe {
        let mut buf = [0u16; 256];
        let written = GetClassNameW(hwnd, &mut buf).max(0) as usize;
        String::from_utf16_lossy(&buf[..written.min(buf.len())])
    }
}

/// 判定窗口是否被 DWM 隐藏（`DWMWA_CLOAKED`）。
///
/// 被 cloak 的窗口对用户不可见（虚拟桌面切换隐藏、UWP 挂起、DWM 动画退场等），
/// 不应进入可置顶列表。`DwmGetWindowAttribute` 在无 DWM 合成的环境（远程桌面
/// 回退 / 旧驱动）返回失败——此时按“未隐藏”放行（保守语义：宁可多列，不可漏）。
#[cfg(windows)]
fn is_cloaked(hwnd: HWND) -> bool {
    unsafe {
        let mut cloaked: u32 = 0;
        let result = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut c_void, std::mem::size_of::<u32>() as u32);
        result.is_ok() && cloaked != 0
    }
}

/// 单条窗口是否通过全部六重过滤（供测试与实现共用的**纯判定函数**）。
///
/// # 参数
/// - `ex_style`：窗口扩展样式（工具条剥离 / 置顶位回读）；
/// - `title` / `class_name`：已采集的窗口标题 / 类名（非空标题前置校验由调用方做）；
/// - `own_title` / `own_class`：TLToolBox 自身窗口的标题 / 类名（自身剥离）；
/// - `is_iconic`：窗口是否处于最小化态（由调用方以 Win32 `IsIconic` 采集；
///   v0.4.1 起最小化的窗口一律不入列）。
///
/// # 测试便利
/// 本函数把「工具条剥离 / 自身剥离 / 桌面容器剥离 / 最小化剥离」四条**纯判定规则**
/// 独立出来，使过滤算法无需真实窗口即可单测（真实窗口路径经
/// [`enumerate_top_level_windows`] 集成验证）。
pub fn passes_visible_filter(
    ex_style: u32,
    title: &str,
    class_name: &str,
    own_title: &str,
    own_class: &str,
    is_iconic: bool,
) -> bool {
    // 规则 4（v0.4.1）：最小化剥离——最小化到任务栏的窗口不在列表中展示。
    if is_iconic {
        return false;
    }
    // 规则 2：工具条剥离——带 WS_EX_TOOLWINDOW 且未声明 WS_EX_APPWINDOW。
    let is_tool_window = ex_style & WS_EX_TOOLWINDOW.0 != 0 && ex_style & WS_EX_APPWINDOW.0 == 0;
    if is_tool_window {
        return false;
    }
    // 规则 5：自身剥离。
    if is_self_window(title, class_name)
        || (title == own_title && class_name == own_class && !own_title.is_empty())
    {
        return false;
    }
    // 规则 6：桌面容器剥离。
    if is_desktop_shell_container(class_name) {
        return false;
    }
    true
}

/// 枚举全部符合置顶候选条件的顶层窗口（`Vec` 顺序即 `EnumWindows` 的 Z-Order
/// 自顶向下顺序，UI 列表按此排布）。
///
/// 每条窗口均通过可见性 / 有标题 / 工具条 / DWM cloak / 最小化 / 自身 / 桌面容器
/// 七重过滤（标题为空串、不可见或最小化者直接短路，不再消耗后续查询）。
pub fn enumerate_top_level_windows() -> Vec<WindowInfo> {
    #[cfg(windows)]
    {
        let mut results = Vec::new();
        ENUM_BUCKET.with(|bucket| {
            // 每次调用独占一次枚举：先清空上次残留（防御性）。
            bucket.borrow_mut().clear();
            // SAFETY: EnumWindowsProc 回调在 EnumWindows 返回前同步调用完毕，
            // 槽位生命周期严格包含于本闭包内；回调不触碰任何 Rust 借用。
            unsafe {
                let _ = EnumWindows(
                    Some(enum_proc),
                    LPARAM(0),
                );
            }
            results = std::mem::take(&mut *bucket.borrow_mut());
        });
        results
    }
    #[cfg(not(windows))]
    {
        let _ = UNKNOWN_PROCESS;
        let _ = to_hwnd;
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// 模块级 FFI 读取器（供 topmost_manager 模块在置顶时实时采集窗口身份）
// ---------------------------------------------------------------------------

/// 【FFI 读取器】读取窗口标题（UTF-16 → 无损 UTF-8；标题长度 0 返回空串）。
///
/// # Safety
/// `hwnd` 为 Win32 窗口句柄；`GetWindowTextW` 对已失效句柄返回 0，不构成
/// 内存违规（与 [`title_of`] 同语义，仅独立成 pub 供模块层在锁外复用）。
#[cfg(windows)]
pub unsafe fn window_title_raw(hwnd: HWND) -> String {
    title_of(hwnd)
}

/// 【FFI 读取器】读取窗口所属进程的可执行文件名（失败回退 `<unknown.exe>`）。
///
/// # Safety
/// 同 [`window_title_raw`]：句柄失效 / 进程退出时按占位名返回，无内存违规。
#[cfg(windows)]
pub unsafe fn process_name_raw(hwnd: HWND) -> String {
    process_name_of(hwnd)
}

/// `EnumWindows` 同步回调：逐窗口完成采集与过滤后推入线程局部槽位。
#[cfg(windows)]
unsafe extern "system" fn enum_proc(hwnd: HWND, _lparam: LPARAM) -> BOOL {
    // 可见性 + 标题长度前置校验（规则 1）：不满足则跳过全部后续查询
    //（GetWindowTextW / OpenProcess 对无效候选是无谓开销）。
    if !IsWindowVisible(hwnd).as_bool() {
        return true.into();
    }
    // 最小化剥离（规则 4，v0.4.1）：最小化到任务栏的窗口一律不在列表中展示。
    if IsIconic(hwnd).as_bool() {
        return true.into();
    }
    let title_len = GetWindowTextLengthW(hwnd);
    if title_len <= 0 {
        return true.into();
    }

    let title = title_of(hwnd);
    if title.trim().is_empty() {
        return true.into();
    }
    let class_name = class_name_of(hwnd);

    // DWM cloak 校验（规则 3）：被 DWM 隐藏者不入列。
    if is_cloaked(hwnd) {
        return true.into();
    }

    let ex_style = get_ex_style(hwnd);
    if !passes_visible_filter(ex_style, &title, &class_name, "", "", false) {
        return true.into();
    }

    let topmost = ex_style & WS_EX_TOPMOST.0 != 0;
    ENUM_BUCKET.with(|bucket| {
        bucket.borrow_mut().push(WindowInfo {
            hwnd: hwnd_value(hwnd),
            process_name: process_name_of(hwnd),
            title,
            class_name,
            topmost,
        });
    });
    true.into()
}

/// 单测只读校验用的守卫常量：确认编译期过滤器名单不被误删（配合纯函数测试）。
#[cfg(test)]
const _FILTER_CONTRACT: fn(u32, &str, &str, &str, &str, bool) -> bool = passes_visible_filter;

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use windows::Win32::UI::WindowsAndMessaging::{WS_EX_APPWINDOW, WS_EX_TOOLWINDOW};

    // -----------------------------------------------------------------------
    // 过滤算法纯判定测试（不依赖真实窗口）
    // -----------------------------------------------------------------------

    /// 最小化窗口（IsIconic 为真）必须被剔除（v0.4.1：最小化到任务栏不入列）。
    #[test]
    fn minimized_window_is_filtered() {
        assert!(!passes_visible_filter(0, "无标题 - 记事本", "Notepad", "", "", true));
        // 即使带工具条扩展样式，最小化也优先剥离（最小化是最强过滤前置）。
        assert!(!passes_visible_filter(
            WS_EX_TOOLWINDOW.0,
            "浮动工具条",
            "ToolWindowClass",
            "",
            "",
            true
        ));
    }

    /// 非最小化普通窗口应通过最小化剥离。
    #[test]
    fn non_minimized_window_passes_iconic_check() {
        assert!(passes_visible_filter(0, "无标题 - 记事本", "Notepad", "", "", false));
    }

    /// 工具条窗口（WS_EX_TOOLWINDOW 且无 WS_EX_APPWINDOW）必须被剔除。
    #[test]
    fn tool_window_without_appwindow_is_filtered() {
        let ex = WS_EX_TOOLWINDOW.0;
        assert!(!passes_visible_filter(
            ex,
            "浮动工具条",
            "ToolWindowClass",
            "",
            "",
            false
        ));
    }

    /// 同时声明 WS_EX_APPWINDOW 的工具条窗口应保留（用户可见的独立应用本体）。
    #[test]
    fn tool_window_with_appwindow_is_kept() {
        let ex = WS_EX_TOOLWINDOW.0 | WS_EX_APPWINDOW.0;
        assert!(passes_visible_filter(ex, "工具面板", "SomeApp", "", "", false));
    }

    /// 普通窗口（无工具条扩展样式）应通过。
    #[test]
    fn plain_window_passes() {
        assert!(passes_visible_filter(0, "无标题 - 记事本", "Notepad", "", "", false));
    }

    /// 自身窗口剥离：标题以产品名开头即视为 TLToolBox 自身。
    #[test]
    fn self_window_by_title_is_filtered() {
        assert!(!passes_visible_filter(0, "TLToolBox - 桌面实用工具箱", "SomeWin", "", "", false));
    }

    /// 自身窗口剥离：类名含 slint 标记即视为 TLToolBox 自身（winit 后端窗口）。
    #[test]
    fn self_window_by_slint_class_is_filtered() {
        assert!(!passes_visible_filter(0, "任意标题", "slint-window-0x1", "", "", false));
    }

    /// 显式传入的“自身窗口”完整匹配也剥离（模块装配时把主窗口句柄带名注入）。
    #[test]
    fn explicit_own_window_pair_is_filtered() {
        assert!(!passes_visible_filter(
            0,
            "TLToolBox - 桌面实用工具箱",
            "slint-window",
            "TLToolBox - 桌面实用工具箱",
            "slint-window",
            false
        ));
    }

    /// 桌面容器剥离：任务栏 / 桌面 / 开始按钮全部剔除。
    #[test]
    fn desktop_shell_containers_are_filtered() {
        for class in [
            "Shell_TrayWnd",
            "Progman",
            "WorkerW",
            "Button",
            "shell_traywnd", // 大小写不敏感
        ] {
            assert!(
                !passes_visible_filter(0, "任务栏", class, "", "", false),
                "桌面容器类名 {class} 应被剔除"
            );
        }
    }

    /// 名称贴近但并非系统容器的类名不得误杀（如用户窗口自定义类名 ButtonEx）。
    #[test]
    fn lookalike_classes_are_not_over_filtered() {
        assert!(passes_visible_filter(0, "应用窗口", "ButtonEx", "", "", false));
        assert!(passes_visible_filter(0, "应用窗口", "WorkerWnd", "", "", false));
    }

    /// 判定谓词在空标题等已由调用方短路的前提下仍不 panic（容错）。
    #[test]
    fn predicate_tolerates_empty_inputs() {
        assert!(passes_visible_filter(0, "", "", "", "", false));
    }
}
