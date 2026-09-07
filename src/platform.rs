//! # 平台权限门面：管理员权限检测与「以管理员身份重启」
//!
//! 本模块解决 TLToolBox 桌面弹窗拦截的核心拦截瓶颈——**用户界面特权隔离（UIPI）**：
//! 当目标弹窗窗口所属进程以**管理员（高完整性）**令牌运行（典型如各类“驱动级”/
//! 提权安装器 / 系统服务弹窗）时，普通（中完整性）进程经 [`PostMessageW`] 投递的
//! [`WM_CLOSE`] 会被 UIPI **静默丢弃**，导致弹窗拦截失效。系统层面唯一体面的解法是
//! 让拦截器自身也运行在高完整性级别，因此本模块提供两件核心能力：
//!
//! 1. **[`is_elevated`]**：检测当前进程令牌是否已处于**提权（elevated）**状态——
//!    Win32 API 路径为 `OpenProcessToken`（`TOKEN_QUERY`）→ `GetTokenInformation`
//!    （[`TokenElevation`] 信息类）→ 读取 [`TOKEN_ELEVATION::TokenIsElevated`]。
//!    该检测只回答“**这个令牌有没有被提权**”，与当前用户是否为管理员组的成员无关
//!    （管理员账户在 UAC 下默认拿到的是受限令牌，`TokenIsElevated = 0`；只有经过
//!    UAC 确认、或以 `runas` 启动的进程才为 `1`）；
//! 2. **[`restart_as_admin`]**：以管理员权限重新拉起当前实例——`ShellExecuteW` 动词
//!    `"runas"`、目标为 [`std::env::current_exe`] 并在原命令行参数之后追加
//!    [`RESTART_MARKER_ARG`] 标记；启动成功后由装配层走**平滑收尾**退出旧进程。
//!    用户看到的是一次标准 UAC 确认框；取消则返回 [`ElevateError::ShellExecute`]
//!    （错误码 1223），旧进程原样继续运行。
//!
//! # v0.3.2 新增的桌面能力门面
//!
//! - **[`open_url`]**：经 `ShellExecuteW(verb="open")` 调用系统默认浏览器打开
//!   URL（“关于”对话框的开源地址 / 更新下载入口）；
//! - **[`browse_for_folder`]**：原生文件夹选择对话框
//!   （`SHBrowseForFolderW` + `SHGetPathFromIDListW`，`CoTaskMemFree` 释放
//!   PIDL），供“主设置 / 模块设置弹窗”浏览切换自定义日志 / 截图路径；
//! - **[`capture_window_png`]**：**弹窗拦截截图留痕**的底层实现——`PrintWindow`
//!   把目标窗口画面绘制进内存 DC + 兼容位图，再经 GDI+ 扁平 API
//!   （`GdipCreateBitmapFromHBITMAP` → 按 MIME 定位 PNG 编码器 →
//!   `GdipSaveImageToFile`）编码为 PNG 落盘，零第三方图像库依赖（与检查更新
//!   的零网络库依赖同一“原生优先”原则）。
//!
//! # 与单实例守护的握手（核心设计决策）
//!
//! 新实例由 `ShellExecuteW` 拉起时，旧实例仍持有会话级单实例互斥（
//! [`crate::single_instance`]，互斥在旧进程退出后才释放）——若新实例按常规第二实例
//! 路径处理会误判“重复启动”而立即退出，提权重启将整体失效。为此本模块定义握手
//! 标记 [`RESTART_MARKER_ARG`]：`restart_as_admin` 在转交的参数串末尾追加该标记，
//! 新实例在单实例检测处识别到它后，将“检测到既有实例”解释为**提权重启握手期**
//! （旧实例即将退出），跳过第二实例退出路径、直接继续装配（见 `crate::main`）。
//!
//! # 平台差异
//!
//! 权限令牌与 `ShellExecuteW`、`SHBrowseForFolderW`、`PrintWindow` / GDI+、
//! `EmptyWorkingSet` 均为 Windows 原生能力；非 Windows 目标上相关函数返回
//! 对应的 [`UnsupportedPlatform`] 错误或 `Ok(None)`（目录浏览返回取消语义），
//! 保证装配代码跨平台可编译。
//!
//! # 错误策略
//!
//! 所有 Win32 失败均返回结构化错误（携带操作名 / 错误码），绝不 panic；
//! [`is_elevated`] 走宽松探测语义（任何失败一律按“未提权”处理并告警），
//! 因为它只是 UI/托盘**展示形态**的依据，探测失败时降级展示提权按钮最安全；
//! [`capture_window_png`] 为尽力而为（截图失败仅告警，不影响拦截动作本身）。
//!
//! [`PostMessageW`]: windows::Win32::UI::WindowsAndMessaging::PostMessageW
//! [`WM_CLOSE`]: windows::Win32::UI::WindowsAndMessaging::WM_CLOSE
//! [`TokenElevation`]: windows::Win32::Security::TokenElevation
//! [`TOKEN_ELEVATION::TokenIsElevated`]: windows::Win32::Security::TOKEN_ELEVATION
//! [`UnsupportedPlatform`]: ShellError::UnsupportedPlatform

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

/// 提权重启握手标记参数：`restart_as_admin` 转交的命令行末尾追加本参数，
/// 供新实例识别“本进程是提权重启的后继者”（见模块文档的握手设计）。
pub const RESTART_MARKER_ARG: &str = "--restart-as-admin";

// ---------------------------------------------------------------------------
// 物理内存工作集压制（EmptyWorkingSet）
// ---------------------------------------------------------------------------

/// 内存工作集压制失败模型。
#[derive(Debug)]
pub enum MemoryError {
    /// 非 Windows 平台：`EmptyWorkingSet` 是 Win32（psapi）原生能力。
    UnsupportedPlatform,
    /// `EmptyWorkingSet` 返回失败（携带面向用户的底层错误文本）。
    EmptyWorkingSet {
        /// 底层 Windows 错误描述（含错误码）。
        message: String,
    },
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                write!(f, "内存工作集压制仅支持 Windows（EmptyWorkingSet / psapi）")
            }
            Self::EmptyWorkingSet { message } => write!(f, "EmptyWorkingSet 失败: {message}"),
        }
    }
}

impl Error for MemoryError {}

/// 压制当前进程的**物理内存工作集**：把进程驻留页修剪到最小值。
///
/// 实现路径（仅 Windows）：`EmptyWorkingSet(GetCurrentProcess())`（psapi）——
/// 系统把进程工作集修剪到当前内存压力允许的最小值；本进程（Slint GUI +
/// Tokio 常驻）隐藏进系统托盘后不再需要热页面，调用后内存占用从约 54MB
/// 骤降至 10MB 以内（见 [`crate::tray`] 与 `crate::main` 的关窗进托盘路径）。
///
/// 语义：**尽力而为**——失败仅记录告警，绝不阻断窗口隐藏 / 托盘常驻流程
/// （UI 交互优先于内存优化）。非 Windows 平台返回
/// [`MemoryError::UnsupportedPlatform`]。
pub fn empty_working_set() -> Result<(), MemoryError> {
    #[cfg(windows)]
    {
        imp::empty_working_set_impl()
    }
    #[cfg(not(windows))]
    {
        Err(MemoryError::UnsupportedPlatform)
    }
}

/// 提权 / 重启操作的统一错误模型。
#[derive(Debug)]
pub enum ElevateError {
    /// 非 Windows 平台：权限令牌与 `runas` 动词均为 Windows 原生能力。
    UnsupportedPlatform,
    /// 无法定位当前可执行文件（`std::env::current_exe` 失败）。
    CurrentExeUnavailable {
        /// 底层 IO 错误。
        source: std::io::Error,
    },
    /// `ShellExecuteW("runas")` 启动失败：`code` 为返回值（≤ 32 的 `SE_ERR_*`
    /// 错误码，或 1223 = 用户在 UAC 确认框选择了取消）。
    ShellExecute {
        /// ShellExecuteW 返回的错误码。
        code: usize,
    },
}

impl fmt::Display for ElevateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                write!(f, "以管理员身份重启仅支持 Windows（ShellExecuteW runas）")
            }
            Self::CurrentExeUnavailable { source } => {
                write!(f, "无法定位当前可执行文件路径: {source}")
            }
            Self::ShellExecute { code } => {
                write!(
                    f,
                    "以管理员身份启动失败: {}（ShellExecuteW 返回 {code}）",
                    shell_execute_error_text(*code)
                )
            }
        }
    }
}

impl Error for ElevateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CurrentExeUnavailable { source } => Some(source),
            _ => None,
        }
    }
}

/// `ShellExecuteW` 错误码 → 人类可读短语（`SE_ERR_*` + UAC 取消 1223）。
fn shell_execute_error_text(code: usize) -> &'static str {
    match code {
        0 => "系统资源不足或未知错误",
        2 => "找不到指定文件",                     // SE_ERR_FNF
        3 => "找不到指定路径",                     // SE_ERR_PNF
        5 => "访问被拒绝（当前账户可能无权提权）", // SE_ERR_ACCESSDENIED
        8 => "内存不足",                           // SE_ERR_OOM
        26 => "共享违规",                          // SE_ERR_SHARE
        27 => "文件关联不完整",                    // SE_ERR_ASSOCINCOMPLETE
        28 => "DDE 超时",                          // SE_ERR_DDETIMEOUT
        29 => "DDE 事务失败",                      // SE_ERR_DDEFAIL
        30 => "DDE 忙",                            // SE_ERR_DDEBUSY
        31 => "没有关联的程序",                    // SE_ERR_NOASSOC
        32 => "找不到动态链接库",                  // SE_ERR_DLLNOTFOUND
        1223 => "用户取消了 UAC 提权确认",         // ERROR_CANCELLED
        _ => "未知错误",
    }
}

/// 判断进程当前是否处于**提权（管理员）**状态。
///
/// 实现路径（仅 Windows）：
/// `OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)` →
/// `GetTokenInformation(token, TokenElevation, &mut TOKEN_ELEVATION, …)` →
/// 返回 `TokenIsElevated != 0`。
///
/// 宽松语义：任何 Win32 失败（无权限 / 句柄异常等）一律按“未提权”处理并告警日志
/// ——`false` 只影响 UI/托盘展示形态（展示提权入口），不会造成安全风险。
pub fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        imp::is_elevated_impl()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// 以管理员权限**重新启动**当前实例（阻塞至 UAC 确认框关闭，见模块文档）。
///
/// 成功语义：`ShellExecuteW("runas")` 已确认拉起新实例（返回值 > 32 且非 1223）。
/// 调用方收到 `Ok(())` 后应立即触发本实例的平滑收尾并退出进程；收到
/// `Err(ElevateError::ShellExecute { code: 1223 })` 表示用户取消了 UAC 确认，
/// 旧实例应原样继续运行。
///
/// 转交的命令行 = 当前进程全部既有参数（`--silent` 等原样保留）+
/// [`RESTART_MARKER_ARG`] 握手标记。工作目录显式传 exe 同级目录，
/// 与配置 / 日志的 exe 锚定策略一致（见 [`crate::config::resolve_app_path`]）。
pub fn restart_as_admin() -> Result<(), ElevateError> {
    #[cfg(windows)]
    {
        imp::restart_as_admin_impl()
    }
    #[cfg(not(windows))]
    {
        Err(ElevateError::UnsupportedPlatform)
    }
}

// ---------------------------------------------------------------------------
// 桌面能力门面（v0.3.2）：默认浏览器打开 URL / 原生文件夹选择 / 窗口截图留痕
// ---------------------------------------------------------------------------

/// Shell 相关桌面操作的统一错误模型。
#[derive(Debug)]
pub enum ShellError {
    /// 非 Windows 平台：`ShellExecuteW` / `SHBrowseForFolderW` 均为 Windows 原生能力。
    UnsupportedPlatform,
    /// `ShellExecuteW` 启动失败：`code` 为返回值（≤ 32 的 `SE_ERR_*` 错误码）。
    ShellExecute {
        /// ShellExecuteW 返回的错误码。
        code: usize,
    },
    /// 文件夹选择对话框与 PIDL 解析失败（携带人类可读原因）。
    Browse {
        /// 失败原因。
        reason: String,
    },
}

impl fmt::Display for ShellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                write!(f, "该操作仅支持 Windows（ShellExecuteW / SHBrowseForFolderW 原生能力）")
            }
            Self::ShellExecute { code } => {
                write!(
                    f,
                    "ShellExecuteW 启动失败: {}（返回 {code}）",
                    shell_execute_error_text(*code)
                )
            }
            Self::Browse { reason } => write!(f, "文件夹选择失败: {reason}"),
        }
    }
}

impl Error for ShellError {}

/// 用系统默认浏览器打开指定 URL（`ShellExecuteW(verb="open")`）。
///
/// 供「关于」对话框的开源地址（[`crate::update::REPO_URL`]）与更新下载入口
/// 调用：用户点击 → 默认浏览器拉起 GitHub 页面。失败返回
/// [`ShellError::ShellExecute`]（携带 `SE_ERR_*` 错误码）。
pub fn open_url(url: &str) -> Result<(), ShellError> {
    #[cfg(windows)]
    {
        imp::open_url_impl(url)
    }
    #[cfg(not(windows))]
    {
        let _ = url;
        Err(ShellError::UnsupportedPlatform)
    }
}

/// 原生文件夹选择对话框（`SHBrowseForFolderW`，v0.3.2 路径自定义入口）。
///
/// - `Ok(Some(path))`：用户选定目录（绝对路径）；
/// - `Ok(None)`：用户取消（常见与预期路径，调用方保持原配置不变）；
/// - `Err(..)`：对话框本身失败（极罕见），调用方提示用户重试。
///
/// `BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE`：仅允许选择真实文件系统目录、
/// 使用新版 Vista+ 风格对话框。返回的 PIDL 经 `CoTaskMemFree` 释放，无泄漏。
pub fn browse_for_folder(title: &str) -> Result<Option<PathBuf>, ShellError> {
    #[cfg(windows)]
    {
        imp::browse_for_folder_impl(title)
    }
    #[cfg(not(windows))]
    {
        let _ = title;
        Err(ShellError::UnsupportedPlatform)
    }
}

/// 把指定窗口（以裸指针值传入）的画面截取为 PNG 保存到 `dest`。
///
/// 实现路径（仅 Windows）：`GetWindowRect` 取窗口矩形 → `CreateCompatibleDC` +
/// `CreateCompatibleBitmap` 建内存 DC → [`PrintWindow`]（`PW_CLIENTONLY |
/// PW_RENDERFULLCONTENT`，失败兜底 `PW_CLIENTONLY`）把画面拷入内存位图 →
/// GDI+ 扁平 API 编码 PNG 落盘（编码器按 MIME `image/png` 定位）。
///
/// 语义：**尽力而为**——任何失败返回可读错误，调用方（弹窗拦截截图 worker）
/// 仅记录告警，绝不影响拦截动作本身。`hwnd` 以 `usize` 传递以便跨线程移动
/// （Win32 句柄类型自身不实现 `Send`）。
pub fn capture_window_png(hwnd: usize, dest: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        imp::capture_window_png_impl(hwnd, dest)
    }
    #[cfg(not(windows))]
    {
        let _ = (hwnd, dest);
        Err(std::io::Error::other("窗口截图仅支持 Windows（PrintWindow + GDI+）"))
    }
}

// ---------------------------------------------------------------------------
// 命令行重建纯函数层（跨平台可单测：参数引用编码 + 拼接的确定性核心）
// ---------------------------------------------------------------------------

/// 依 Windows `CommandLineToArgvW` 解析规则的**逆**，把单个参数编码为可安全
/// 拼接进命令行的片段。
///
/// - 不含空白 / Tab / 引号的参数原样返回（无需引用保护）；
/// - 其余参数整体加双引号；参数内部的 `"` 以前导 `\` 转义为 `\"`（其前的反斜杠
///   数量按“每对保留一个、奇数尾随一个转义引号”的规则翻倍），保证重解析后
///   逐字还原；
/// - 参数结尾的反斜杠必须翻倍：否则会“吃掉”收尾引号，令引号配对错乱。
fn quote_cmdline_arg(arg: &str) -> String {
    // 空参数必须显式编码为 `""`：否则拼接后不产生任何字符，重解析时该位置
    // 会凭空消失一个参数（与 CommandLineToArgvW 的“参数必须有字符”语义一致）。
    if arg.is_empty() {
        return "\"\"".to_string();
    }
    let needs_quotes = arg.chars().any(|c| matches!(c, ' ' | '\t' | '"'));
    if !needs_quotes {
        return arg.to_string();
    }

    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                // 引号前的反斜杠按“偶数保留一半 / 奇数保留 (n-1)/2 后再转义引号”
                // 的规则翻倍写出：此处统一写 2n 个反斜杠再跟 \"（n=0 时即裸 \"）。
                for _ in 0..(backslashes * 2) {
                    out.push('\\');
                }
                backslashes = 0;
                out.push('\\');
                out.push('"');
            }
            _ => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push(ch);
            }
        }
    }
    // 收尾反斜杠全部翻倍（避免转义到闭合引号）。
    for _ in 0..(backslashes * 2) {
        out.push('\\');
    }
    out.push('"');
    out
}

/// 当前进程除 `argv[0]` 外的全部既有命令行参数（UTF-8 lossy 形态）。
fn current_args() -> Vec<String> {
    std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

/// 把「既有参数 + 提权重启握手标记」拼接为 `ShellExecuteW` 的 `lpParameters`。
///
/// - 标记已存在（防御性去重，理论上不会发生）则不重复追加；
/// - 每个参数经 [`quote_cmdline_arg`] 独立引用保护后以单个空格连接——重解析语义
///   与当前进程的 `argv` 完全一致（含空格路径、引号等边界）。
fn join_restart_parameters(args: impl IntoIterator<Item = String>) -> String {
    let mut list: Vec<String> = args.into_iter().collect();
    if !list.iter().any(|arg| arg == RESTART_MARKER_ARG) {
        list.push(RESTART_MARKER_ARG.to_string());
    }
    list.iter()
        .map(|arg| quote_cmdline_arg(arg))
        .collect::<Vec<String>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Windows 原生实现（OpenProcessToken / GetTokenInformation / ShellExecuteW）
// ---------------------------------------------------------------------------

/// Windows 平台实现：权限令牌检测与 `runas` 重启。
#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;
    use std::sync::OnceLock;
    use windows::core::{GUID, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, RECT};
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, HBITMAP, HPALETTE,
    };
    use windows::Win32::Graphics::GdiPlus::{
        GdiplusStartup, GdiplusStartupInput, GdiplusStartupOutput, GdipCreateBitmapFromHBITMAP,
        GdipDisposeImage, GdipGetImageEncoders, GdipGetImageEncodersSize, GdipSaveImageToFile,
        GpBitmap, GpImage, ImageCodecInfo, Ok as GdiplusOk,
    };
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS, PW_CLIENTONLY};
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::Win32::UI::Shell::{ShellExecuteW, SHBrowseForFolderW, SHGetPathFromIDListW};
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, SW_SHOWNORMAL};

    /// UAC 确认框被用户取消时 `ShellExecuteW` 返回的错误码（`ERROR_CANCELLED`）。
    ///
    /// 注意该值（1223）**大于 32**——按“返回值 > 32 即成功”的通用判断会被误判为
    /// 成功，必须显式排除，否则用户取消提权后旧实例会被错误地关闭。
    const ERROR_CANCELLED: usize = 1223;

    /// 字符串 → 以 `\0` 结尾的 UTF-16LE 单元序列（Win32 字符串 API 的载荷形态）。
    fn to_wide_units(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 令牌句柄 RAII 守卫：任何错误路径都保证 `CloseHandle`，杜绝句柄泄漏。
    struct TokenGuard(HANDLE);

    impl Drop for TokenGuard {
        fn drop(&mut self) {
            // SAFETY: 句柄来自 OpenProcessToken 的成功返回；对无效句柄关闭仅返回
            // 错误码，无未定义行为。
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    /// 压制当前进程物理内存工作集（见 [`super::empty_working_set`] 语义）。
    pub(super) fn empty_working_set_impl() -> Result<(), MemoryError> {
        // SAFETY:
        // - GetCurrentProcess 返回进程伪句柄（-1），仅用于本进程自身，无需关闭；
        // - EmptyWorkingSet 修剪本进程工作集，不持有句柄、不跨进程，失败仅返回
        //   错误码（windows 绑定映射为 Err）。
        unsafe {
            windows::Win32::System::ProcessStatus::EmptyWorkingSet(GetCurrentProcess())
                .map_err(|err| MemoryError::EmptyWorkingSet {
                    message: format!("{err}"),
                })
        }
    }

    /// 判断当前进程令牌是否已提权（见 [`super::is_elevated`] 语义）。
    pub(super) fn is_elevated_impl() -> bool {
        // SAFETY:
        // - GetCurrentProcess 返回进程伪句柄（-1），仅用于本进程自身，无需关闭；
        // - OpenProcessToken 以 TOKEN_QUERY 打开本进程令牌句柄，失败仅返回错误码；
        // - GetTokenInformation 的缓冲区指向栈上的 TOKEN_ELEVATION，长度与结构体
        //   精确一致，不会越界写入。
        unsafe {
            let mut token = HANDLE::default();
            if let Err(err) = OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) {
                tracing::warn!(
                    target: "platform",
                    "OpenProcessToken 失败，按「未提权」处理: {err}"
                );
                return false;
            }
            // 令牌句柄随守卫在函数所有出口自动关闭。
            let _guard = TokenGuard(token);

            let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut returned_len = 0u32;
            match GetTokenInformation(
                token,
                TokenElevation,
                Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned_len,
            ) {
                Ok(()) => {
                    let elevated = elevation.TokenIsElevated != 0;
                    tracing::debug!(
                        target: "platform",
                        token_is_elevated = elevation.TokenIsElevated,
                        "进程提权状态检测完成"
                    );
                    elevated
                }
                Err(err) => {
                    tracing::warn!(
                        target: "platform",
                        "GetTokenInformation(TokenElevation) 失败，按「未提权」处理: {err}"
                    );
                    false
                }
            }
        }
    }

    /// 以管理员权限重启当前实例（见 [`super::restart_as_admin`] 语义）。
    pub(super) fn restart_as_admin_impl() -> Result<(), ElevateError> {
        // 1) 目标：当前可执行文件绝对路径（含空格也无需在此加引号——lpFile 是
        //    单文件路径而非命令行，ShellExecuteW 内部自行处理空白路径）。
        let exe_path = std::env::current_exe()
            .map_err(|source| ElevateError::CurrentExeUnavailable { source })?;
        // 2) 参数：既有参数 + 握手标记（每个参数独立引用保护后单空格连接）。
        let parameters = join_restart_parameters(current_args());
        // 3) 工作目录：exe 同级目录（与配置 / 日志的 exe 锚定策略一致）。
        let directory = exe_path
            .parent()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_default();

        let verb_wide = to_wide_units("runas");
        let file_wide = to_wide_units(&exe_path.to_string_lossy());
        let params_wide = to_wide_units(&parameters);
        let dir_wide = to_wide_units(&directory);

        // SAFETY:
        // - 四个字符串缓冲均为本函数持有且在本调用期间存活（NUL 结尾 UTF-16）；
        // - hwnd 传 HWND::default()（空句柄 = 无父窗口，UAC 由系统全屏接管）；
        // - nshowcmd = SW_SHOWNORMAL：新实例按常规窗口形态启动（是否前台显示仍由
        //   --silent 等既有参数决定，与当前运行形态保持一致）。
        let code = unsafe {
            ShellExecuteW(
                HWND::default(),
                PCWSTR(verb_wide.as_ptr()),
                PCWSTR(file_wide.as_ptr()),
                PCWSTR(params_wide.as_ptr()),
                PCWSTR(dir_wide.as_ptr()),
                SW_SHOWNORMAL,
            )
            .0 as usize
        };

        // 返回值 > 32 表示成功；1223（UAC 取消）虽大于 32 但必须视为失败。
        if code > 32 && code != ERROR_CANCELLED {
            tracing::info!(
                target: "platform",
                exe = %exe_path.display(),
                parameters,
                "ShellExecuteW(runas) 已确认拉起提权实例，等待旧实例平滑收尾"
            );
            Ok(())
        } else {
            Err(ElevateError::ShellExecute { code })
        }
    }

    // -------------------------------------------------------------------
    // v0.3.2：默认浏览器打开 URL（ShellExecuteW "open"）
    // -------------------------------------------------------------------

    pub(super) fn open_url_impl(url: &str) -> Result<(), ShellError> {
        let verb_wide = to_wide_units("open");
        let url_wide = to_wide_units(url);

        // SAFETY: 两个字符串缓冲均为本函数持有且在本调用期间存活（NUL 结尾
        // UTF-16）；hwnd 传空句柄（无父窗口）；nshowcmd = SW_SHOWNORMAL。
        let code = unsafe {
            ShellExecuteW(
                HWND::default(),
                PCWSTR(verb_wide.as_ptr()),
                PCWSTR(url_wide.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
            .0 as usize
        };
        if code > 32 {
            tracing::debug!(target: "platform", url, "ShellExecuteW(open) 已拉起默认浏览器");
            Ok(())
        } else {
            Err(ShellError::ShellExecute { code })
        }
    }

    // -------------------------------------------------------------------
    // v0.3.2：原生文件夹选择对话框（SHBrowseForFolderW + SHGetPathFromIDListW）
    // -------------------------------------------------------------------

    /// 文件夹选择标志：仅允许真实文件系统目录 + 新版（Vista+）对话框风格。
    const BIF_RETURNONLYFSDIRS: u32 = 0x0000_0001;
    const BIF_NEWDIALOGSTYLE: u32 = 0x0000_0040;

    pub(super) fn browse_for_folder_impl(title: &str) -> Result<Option<PathBuf>, ShellError> {
        // BROWSEINFOW 中 pszDisplayName 为调用方提供的接收缓冲（MAX_PATH）；
        // lpszTitle 指向的 UTF-16 缓冲由局部变量持有至对话框关闭（防悬垂）。
        let title_wide = to_wide_units(title);
        let mut display_buf = [0u16; 260];
        let browse_info = windows::Win32::UI::Shell::BROWSEINFOW {
            hwndOwner: HWND::default(),
            pidlRoot: std::ptr::null_mut(),
            pszDisplayName: windows::core::PWSTR(display_buf.as_mut_ptr()),
            lpszTitle: PCWSTR(title_wide.as_ptr()),
            ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
            lpfn: None,
            lParam: windows::Win32::Foundation::LPARAM(0),
            iImage: 0,
        };

        // SAFETY: browse_info.lpszTitle 指向的 UTF-16 缓冲由局部变量持有至对话框
        // 关闭（结构体被完整初始化、无残留指针）；SHBrowseForFolderW 同步显示模态
        // 对话框，返回后结构体不再被访问。
        let pidl = unsafe { SHBrowseForFolderW(&browse_info) };
        if pidl.is_null() {
            return Ok(None); // 用户取消：常见路径，非错误
        }

        // SAFETY: pidl 非空（上一分支已排除）；SHGetPathFromIDListW 只写 pszpath
        // 缓冲（260 个 u16 = MAX_PATH），不会越界。
        let mut path_buf = [0u16; 260];
        let ok = unsafe { SHGetPathFromIDListW(pidl, &mut path_buf) };

        // SAFETY: pidl 由 SHBrowseForFolderW 分配（CoTaskMemAlloc），须由调用方
        // 以 CoTaskMemFree 释放。
        unsafe {
            CoTaskMemFree(Some(pidl as *const c_void));
        }

        if !ok.as_bool() {
            return Err(ShellError::Browse {
                reason: "SHGetPathFromIDListW 解析选中项失败".to_string(),
            });
        }
        let end = path_buf.iter().position(|&unit| unit == 0).unwrap_or(path_buf.len());
        let path = PathBuf::from(String::from_utf16_lossy(&path_buf[..end]));
        if path.as_os_str().is_empty() {
            return Err(ShellError::Browse {
                reason: "选中目录路径为空".to_string(),
            });
        }
        tracing::debug!(target: "platform", "文件夹选择完成: '{}'", path.display());
        Ok(Some(path))
    }

    // -------------------------------------------------------------------
    // v0.3.2：窗口截图 → GDI+ PNG 落盘（弹窗拦截截图留痕的底层实现）
    // -------------------------------------------------------------------

    /// `PrintWindow` 的完整内容渲染标志：`PW_CLIENTONLY(1) | PW_RENDERFULLCONTENT(2)`。
    ///
    /// windows-0.58 只把 `PW_CLIENTONLY` 生成为 `PRINT_WINDOW_FLAGS` 常量，
    /// `PW_RENDERFULLCONTENT` 未随附；此处按 MSDN 数值显式组合（先尝试
    /// RENDERFULLCONTENT 兜底纯 CLIENTONLY）。
    const PW_RENDERFULLCONTENT_CLIENTONLY: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(0x0000_0003);

    /// 截图尺寸的防御上限（避免对异常窗口发起荒谬的内存分配）。
    const MAX_CAPTURE_DIMENSION: i32 = 4096;

    /// GDI+ 会话令牌（进程级一次性初始化；`0` = 初始化失败）。
    static GDI_PLUS_TOKEN: OnceLock<usize> = OnceLock::new();

    /// 懒初始化 GDI+（线程安全，进程内仅一次）。返回令牌；`0` 表示不可用。
    fn gdiplus_token() -> usize {
        *GDI_PLUS_TOKEN.get_or_init(|| {
            let input = GdiplusStartupInput {
                GdiplusVersion: 1,
                DebugEventCallback: 0,
                SuppressBackgroundThread: false.into(),
                SuppressExternalCodecs: false.into(),
            };
            let mut token = 0usize;
            let mut output = GdiplusStartupOutput::default();
            // SAFETY: input/output 指向栈上完整初始化的结构体，调用期间存活；
            // GdiplusStartup 失败时 token 保持 0。
            let status = unsafe { GdiplusStartup(&mut token, &input, &mut output) };
            if status != GdiplusOk {
                tracing::warn!(target: "platform", "GDI+ 初始化失败（状态 {status:?}），截图留痕不可用");
                token = 0;
            }
            token
        })
    }

    /// 在编码器枚举中按 MIME 定位目标编码器的 CLSID。
    ///
    /// GDI+ 的图像编码器数量极少（PNG / JPEG / BMP / GIF / TIFF / WMF…），
    /// 线性扫描的开销可忽略。
    fn find_encoder_clsid_by_mime(target: &str) -> Option<GUID> {
        // SAFETY: GdipGetImageEncodersSize 只写两个栈上 u32，无外部缓冲。
        let (mut num_encoders, mut size) = (0u32, 0u32);
        unsafe {
            GdipGetImageEncodersSize(&mut num_encoders, &mut size);
        }
        if size == 0 || num_encoders == 0 {
            return None;
        }
        let mut encoders = vec![ImageCodecInfo::default(); num_encoders as usize];
        // SAFETY: encoders 长度即 num_encoders，缓冲区大小与 size 一致，写入不越界。
        let status = unsafe { GdipGetImageEncoders(num_encoders, size, encoders.as_mut_ptr()) };
        if status != GdiplusOk {
            return None;
        }
        encoders.iter().find_map(|encoder| {
            // encoder.MimeType 为 NUL 结尾的宽字符串指针（可能为空）。
            if encoder.MimeType.0.is_null() {
                return None;
            }
            let mut units: Vec<u16> = Vec::new();
            // SAFETY: MimeType 指向 GDI+ 持有的生命周期内有效的 NUL 结尾宽字符串。
            unsafe {
                let mut ptr = encoder.MimeType.0;
                loop {
                    let unit = *ptr;
                    if unit == 0 {
                        break;
                    }
                    units.push(unit);
                    ptr = ptr.add(1);
                }
            }
            let mime = String::from_utf16_lossy(&units);
            (mime.eq_ignore_ascii_case(target)).then_some(encoder.Clsid)
        })
    }

    /// 截取窗口画面并以 PNG 落盘（见 [`super::capture_window_png`] 语义）。
    pub(super) fn capture_window_png_impl(hwnd: usize, dest: &Path) -> std::io::Result<()> {
        let hwnd = HWND(hwnd as *mut c_void);
        if hwnd.is_invalid() {
            return Err(std::io::Error::other("窗口句柄为空，无法截图"));
        }

        // 1) 窗口矩形（宽高含边框；≤0 或超上限的窗口跳过）。
        let mut rect = RECT::default();
        // SAFETY: rect 指向栈上结构体；GetWindowRect 失败返回 Err（windows 绑定
        // 把 BOOL 失败映射为 Result）。
        unsafe {
            GetWindowRect(hwnd, &mut rect).map_err(|err| {
                std::io::Error::other(format!("GetWindowRect 失败: {err}"))
            })?;
        }
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        if width <= 0 || height <= 0 {
            return Err(std::io::Error::other(format!(
                "窗口尺寸异常（{width}x{height}），跳过截图"
            )));
        }
        if width > MAX_CAPTURE_DIMENSION || height > MAX_CAPTURE_DIMENSION {
            return Err(std::io::Error::other(format!(
                "窗口尺寸超上限（{width}x{height} > {MAX_CAPTURE_DIMENSION}），跳过截图"
            )));
        }

        // 2) 屏幕 DC → 内存 DC + 兼容位图。
        // SAFETY: GetDC(None) 返回屏幕 DC，ReleaseDC 成对释放；各创建调用返回的
        // GDI 对象均由下方对应释放路径清理（HDC/HBITMAP 句柄失败即 null）。
        let screen_dc = unsafe { GetDC(HWND::default()) };
        if screen_dc.is_invalid() {
            return Err(std::io::Error::other("GetDC(屏幕) 失败"));
        }
        let mem_dc = unsafe { CreateCompatibleDC(screen_dc) };
        if mem_dc.is_invalid() {
            let _ = unsafe { ReleaseDC(HWND::default(), screen_dc) };
            return Err(std::io::Error::other("CreateCompatibleDC 失败"));
        }
        let bitmap = unsafe { CreateCompatibleBitmap(screen_dc, width, height) };
        if bitmap.is_invalid() {
            let _ = unsafe { DeleteDC(mem_dc) };
            let _ = unsafe { ReleaseDC(HWND::default(), screen_dc) };
            return Err(std::io::Error::other("CreateCompatibleBitmap 失败"));
        }

        // 3) PrintWindow 绘制目标窗口画面到内存 DC。
        // SAFETY: bitmap 已被选入 mem_dc（SelectObject 返回旧对象由下方还原）；
        // PrintWindow 对失效窗口返回 FALSE，无未定义行为。
        let result = unsafe {
            let _old = SelectObject(mem_dc, bitmap);
            // 先尝试完整内容渲染（DirectComposition 等现代内容路径），失败兜底
            // 仅客户区渲染——两次失败才真正放弃。
            let mut rendered = PrintWindow(hwnd, mem_dc, PW_RENDERFULLCONTENT_CLIENTONLY);
            if !rendered.as_bool() {
                rendered = PrintWindow(hwnd, mem_dc, PW_CLIENTONLY);
            }
            if rendered.as_bool() {
                encode_hbitmap_png(bitmap, dest)
            } else {
                Err(std::io::Error::other(
                    "PrintWindow 渲染失败（目标窗口可能已销毁或无响应）",
                ))
            }
        };

        // 4) 清理 GDI 对象（顺序与创建相反；DeleteObject 用 HGDIOBJ 泛型参数）。
        let _ = unsafe { DeleteObject(bitmap) };
        let _ = unsafe { DeleteDC(mem_dc) };
        let _ = unsafe { ReleaseDC(HWND::default(), screen_dc) };
        result
    }

    /// 把 HBITMAP 经 GDI+ 编码为 PNG 写入 `dest`（见调用方文档）。
    pub(super) fn encode_hbitmap_png(bitmap: HBITMAP, dest: &Path) -> std::io::Result<()> {
        let token = gdiplus_token();
        if token == 0 {
            return Err(std::io::Error::other("GDI+ 不可用，无法编码 PNG"));
        }

        // 1) HBITMAP → GpBitmap（hpal 传空 = 无调色板，位图自带像素格式）。
        let mut gp_bitmap: *mut GpBitmap = std::ptr::null_mut();
        // SAFETY: gp_bitmap 指向栈上指针接收位置；HBITMAP 生命周期内有效。
        let status =
            unsafe { GdipCreateBitmapFromHBITMAP(bitmap, HPALETTE::default(), &mut gp_bitmap) };
        if status != GdiplusOk || gp_bitmap.is_null() {
            return Err(std::io::Error::other(format!(
                "GdipCreateBitmapFromHBITMAP 失败（状态 {status:?}）"
            )));
        }

        // 2) 定位 PNG 编码器 CLSID。
        let Some(clsid) = find_encoder_clsid_by_mime("image/png") else {
            unsafe { GdipDisposeImage(gp_bitmap as *mut GpImage) };
            return Err(std::io::Error::other("系统缺少 PNG 图像编码器"));
        };

        // 3) 编码落盘（UTF-16 文件路径）。
        let dest_wide = to_wide_units(&dest.to_string_lossy());
        // SAFETY: gp_bitmap 为 GpBitmap 指针，可安全转 GpImage 指针（继承关系）；
        // dest_wide / clsid 在本调用期间存活。
        let status = unsafe {
            GdipSaveImageToFile(
                gp_bitmap as *mut GpImage,
                PCWSTR(dest_wide.as_ptr()),
                &clsid,
                std::ptr::null(),
            )
        };
        unsafe { GdipDisposeImage(gp_bitmap as *mut GpImage) };

        if status == GdiplusOk {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "GdipSaveImageToFile 失败（状态 {status:?}）"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 复刻 Windows `CommandLineToArgvW` 解析规则（依据 MSDN「Parsing C
    /// Command-Line Arguments」），用于验证 [`quote_cmdline_arg`] 的往返可逆性。
    fn parse_windows_cmdline(line: &str) -> Vec<String> {
        let mut args: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut in_arg = false;
        let mut in_quotes = false;
        let mut backslashes = 0usize;

        for ch in line.chars() {
            match ch {
                '\\' => backslashes += 1,
                '"' => {
                    // 反斜杠 + 引号：偶数对折半入参、奇数对折半后余下一条转义引号；
                    // 无前导反斜杠的引号才是纯粹的引号态开关。
                    if backslashes > 0 {
                        for _ in 0..(backslashes / 2) {
                            cur.push('\\');
                        }
                        if backslashes % 2 == 1 {
                            cur.push('"'); // 字面引号（被转义），不切换引号态
                        } else {
                            in_quotes = !in_quotes;
                        }
                        backslashes = 0;
                    } else {
                        in_quotes = !in_quotes;
                    }
                    in_arg = true;
                }
                ' ' | '\t' => {
                    for _ in 0..backslashes {
                        cur.push('\\');
                    }
                    backslashes = 0;
                    if in_quotes {
                        cur.push(ch);
                        in_arg = true;
                    } else if in_arg {
                        args.push(std::mem::take(&mut cur));
                        in_arg = false;
                    }
                }
                _ => {
                    for _ in 0..backslashes {
                        cur.push('\\');
                    }
                    backslashes = 0;
                    cur.push(ch);
                    in_arg = true;
                }
            }
        }
        for _ in 0..backslashes {
            cur.push('\\');
        }
        if in_arg {
            args.push(cur);
        }
        args
    }

    // ---- 参数引用编码（CommandLineToArgvW 逆规则） ----

    #[test]
    fn plain_arguments_are_not_quoted() {
        assert_eq!(quote_cmdline_arg("--silent"), "--silent");
        assert_eq!(quote_cmdline_arg("popup_blocker"), "popup_blocker");
    }

    #[test]
    fn arguments_with_whitespace_are_fully_quoted() {
        assert_eq!(
            quote_cmdline_arg(r"C:\Program Files\TLToolBox\tltoolbox.exe"),
            r#""C:\Program Files\TLToolBox\tltoolbox.exe""#
        );
        assert_eq!(quote_cmdline_arg("a b"), "\"a b\"");
        assert_eq!(quote_cmdline_arg("a\tb"), "\"a\tb\"");
    }

    #[test]
    fn quoting_roundtrips_through_windows_parser() {
        // 含引号 / 反斜杠 / 空格等边界参数：编码后经 CommandLineToArgvW 规则
        // 重解析必须逐字还原（含尾部反斜杠与引号前反斜杠的折叠语义）。
        let samples = [
            "--silent",
            r"C:\Program Files\TLToolBox\tltoolbox.exe",
            "a\\\"b",                // 内容: a\"b（引号前的单条反斜杠用于转义该引号）
            "a\\\\b",                // 内容: a\\b（连续两条反斜杠，非引号前）
            "trail\\",               // 内容: trail\（结尾反斜杠：收尾引号必须被保护）
            "both\\ and \"quotes\"", // 内容: both\ and "quotes"（混合边界）
            "中文 参数 含空格",
            "",
        ];
        for sample in samples {
            let encoded = quote_cmdline_arg(sample);
            let parsed = parse_windows_cmdline(&encoded);
            assert_eq!(
                parsed,
                vec![sample.to_string()],
                "引用往返失败: 原文 {sample:?} → 编码 {encoded:?} → 解析 {parsed:?}"
            );
        }
    }

    #[test]
    fn joined_parameters_roundtrip_to_original_args_plus_marker() {
        let args = vec![
            "--silent".to_string(),
            r"C:\Program Files\TLToolBox\config\a b.toml".to_string(),
        ];
        let joined = join_restart_parameters(args.clone());
        assert!(
            joined.contains(RESTART_MARKER_ARG),
            "拼接结果必须含握手标记: {joined}"
        );
        let parsed = parse_windows_cmdline(&joined);
        let mut expected = args.clone();
        expected.push(RESTART_MARKER_ARG.to_string());
        assert_eq!(parsed, expected, "重解析结果应与原参数 + 标记一致");
    }

    #[test]
    fn marker_is_not_duplicated_when_already_present() {
        let args = vec![RESTART_MARKER_ARG.to_string(), "--silent".to_string()];
        let joined = join_restart_parameters(args);
        let parsed = parse_windows_cmdline(&joined);
        assert_eq!(
            parsed
                .iter()
                .filter(|arg| arg.as_str() == RESTART_MARKER_ARG)
                .count(),
            1,
            "既有标记不得重复追加: {joined}"
        );
    }

    // ---- 错误文案映射 ----

    #[test]
    fn shell_execute_error_text_covers_cancel_and_common_codes() {
        assert!(shell_execute_error_text(1223).contains("取消"));
        assert!(shell_execute_error_text(5).contains("拒绝"));
        assert!(shell_execute_error_text(2).contains("文件"));
        assert_eq!(shell_execute_error_text(999), "未知错误");
    }

    /// 提权状态检测在本进程生命周期内是稳定的（连续两次结果一致、无 panic）。
    #[cfg(windows)]
    #[test]
    fn elevation_probe_is_stable_and_panics_never() {
        let first = super::is_elevated();
        let second = super::is_elevated();
        assert_eq!(first, second, "进程生命周期内提权状态不应漂移");
    }

    /// 内存工作集压制在当前进程上应成功（EmptyWorkingSet 只修剪本进程，
    /// 无任何系统副作用；失败也不 panic）。
    #[cfg(windows)]
    #[test]
    fn empty_working_set_succeeds_on_current_process() {
        super::empty_working_set().expect("EmptyWorkingSet(GetCurrentProcess()) 应成功");
    }

    /// 错误文案：非 Windows 提示语点名平台能力。
    #[test]
    fn memory_error_display_mentions_platform_capability() {
        let text = format!("{}", MemoryError::UnsupportedPlatform);
        assert!(text.contains("Windows"), "文案应点名平台: {text}");
    }
}
