//! # 端口猎手 · 进程安全终止与二次确认契约（v0.5.0）
//!
//! 本模块实现「一键释放端口」的最低层动作——**安全终止占用进程**，并承担
//! UIPI 防御与 Toast 反馈：
//!
//! - [`kill_process_and_release_port`]：经 [`OpenProcess`]（`PROCESS_TERMINATE`）+
//!   [`TerminateProcess`] 终止进程（纯 Win32，返回结构化 [`PortError`]）；
//! - [`kill_process_with_events`]：在低层动作之上叠加事件总线反馈——成功即发布
//!   「已成功释放端口 {port}，结束进程 {process_name} (PID: {pid})」Toast；遭遇
//!   [`ERROR_ACCESS_DENIED`]（UIPI / 高完整性进程拦截）时发布
//!   「需要管理员权限，请通过顶部盾牌提权运行」Toast。
//!
//! # 错误处理与 UIPI 防御
//!
//! - [`PortError::OpenProcessFailed`] / [`PortError::TerminateFailed`] 携带 Win32
//!   错误码；错误码 5（[`ERROR_ACCESS_DENIED`]）即 UIPI 拦截或完整性级别不足，
//!   调用方经 [`PortError::is_access_denied`] 判定后提示用户以管理员身份运行
//!   （TLToolBox 顶部盾牌按钮 / 托盘菜单「以管理员身份重启」）；
//! - 释放成功后**立即**向事件总线发送成功 Toast（见 [`kill_process_with_events`]）
//!   并返回 `Ok(())`；审计日志与模块明细日志由装配层（`crate::main` / 模块）负责。
//!
//! # 二次确认（配置开关）
//!
//! `confirm_before_kill` 决定 UI 是否在点击「一键释放」后先行展示行内确认态；
//! 本模块只执行「确认后的最终动作」，确认流程自身的分支逻辑放在装配层测试
//! （见 `src/main.rs` 与配置节单测）。

use crate::bus::AppEvent;

#[cfg(windows)]
use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED};
#[cfg(windows)]
use windows::Win32::System::Threading::{
    OpenProcess, TerminateProcess, PROCESS_TERMINATE,
};

/// 释放端口（终止进程）的 Win32 失败模型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortError {
    /// 监听表扫描失败（`GetExtendedTcpTable` / `GetExtendedUdpTable` 非零返回）。
    ScanFailed {
        /// 失败环节（探测尺寸 / 枚举）。
        context: &'static str,
        /// Windows 错误码。
        code: u32,
    },
    /// `OpenProcess(PROCESS_TERMINATE)` 失败（含 `ERROR_ACCESS_DENIED` = UIPI）。
    OpenProcessFailed {
        /// 目标 PID。
        pid: u32,
        /// Windows 错误码。
        code: u32,
    },
    /// `TerminateProcess` 失败（含 `ERROR_ACCESS_DENIED` = UIPI 拦截）。
    TerminateFailed {
        /// 目标 PID。
        pid: u32,
        /// Windows 错误码。
        code: u32,
    },
    /// 非 Windows 平台（端口猎手为 Win32 原生能力）。
    UnsupportedPlatform,
}

impl PortError {
    /// 是否为 `ERROR_ACCESS_DENIED`（5）——UIPI 拦截 / 目标完整性级别更高的
    /// 标准信号；调用方据此提示用户提权运行（当前进程未提权时）。
    pub fn is_access_denied(&self) -> bool {
        match self {
            #[cfg(windows)]
            PortError::OpenProcessFailed { code, .. }
            | PortError::TerminateFailed { code, .. } => *code == ERROR_ACCESS_DENIED.0,
            _ => false,
        }
    }

    /// 错误码（无码变体返回 `None`）。
    pub fn code(&self) -> Option<u32> {
        match self {
            PortError::ScanFailed { code, .. }
            | PortError::OpenProcessFailed { code, .. }
            | PortError::TerminateFailed { code, .. } => Some(*code),
            PortError::UnsupportedPlatform => None,
        }
    }

    /// 面向用户 / 日志的 Win32 错误码摘要（`Win32 错误码 <code> (<名称>)`）。
    pub fn win32_summary(&self) -> String {
        let name = match self.code() {
            Some(5) => "ERROR_ACCESS_DENIED",
            Some(87) => "ERROR_INVALID_PARAMETER",
            Some(6) => "ERROR_INVALID_HANDLE",
            Some(code) => return format!("Win32 错误码 {code}"),
            None => return "不支持的平台".to_string(),
        };
        // 上面闭包分支已处理 Some；这里取回具体码补全文本。
        let code = self.code().unwrap_or(0);
        format!("Win32 错误码 {code} ({name})")
    }
}

impl std::fmt::Display for PortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PortError::ScanFailed { context, .. } => {
                write!(f, "{context}失败: {}", self.win32_summary())
            }
            PortError::OpenProcessFailed { pid, .. } => {
                write!(f, "打开进程 (PID: {pid}) 失败: {}", self.win32_summary())
            }
            PortError::TerminateFailed { pid, .. } => {
                write!(f, "终止进程 (PID: {pid}) 失败: {}", self.win32_summary())
            }
            PortError::UnsupportedPlatform => write!(f, "端口猎手仅支持 Windows 平台"),
        }
    }
}

impl std::error::Error for PortError {}

/// 低层动作：终止指定 PID 的进程（Windows：`OpenProcess(PROCESS_TERMINATE)` +
/// `TerminateProcess(handle, 1)`），使端口随之释放。
///
/// - `port` / `protocol` 仅用于错误文本的上下文回显（无副作用）；
/// - 成功返回 `Ok(())`；任何失败返回结构化 [`PortError`]（含错误码）；
/// - 不含 Toast / 审计——反馈职责见 [`kill_process_with_events`] 与装配层。
pub fn kill_process_and_release_port(
    pid: u32,
    port: u16,
    protocol: &str,
) -> Result<(), PortError> {
    #[cfg(windows)]
    {
        let _ = (port, protocol);
        // SAFETY: OpenProcess 以 PROCESS_TERMINATE 打开句柄；失败返回错误码。
        let handle = match unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
            Ok(handle) => handle,
            Err(err) => {
                return Err(PortError::OpenProcessFailed {
                    pid,
                    // Error::code() 为 HRESULT（0x8007xxxx 形态）；低 16 位即
                    // Win32 原始错误码（HRESULT::from_win32 编码契约）。
                    code: (err.code().0 as u32) & 0xFFFF,
                })
            }
        };

        // SAFETY: TerminateProcess 以退出码 1 强制终结目标进程；句柄随后释放。
        let result = unsafe { TerminateProcess(handle, 1) };
        // SAFETY: 句柄使用完毕，CloseHandle 释放（失败仅返回错误码，忽略）。
        let _ = unsafe { CloseHandle(handle) };

        match result {
            Ok(()) => Ok(()),
            Err(err) => Err(PortError::TerminateFailed {
                pid,
                code: (err.code().0 as u32) & 0xFFFF,
            }),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (pid, port, protocol);
        Err(PortError::UnsupportedPlatform)
    }
}

/// 带事件总线反馈的释放入口：执行 [`kill_process_and_release_port`] 后按时序发布
/// Toast（成功 / UIPI 拦截两路）。
///
/// - 成功：`AppEvent::ToastRequested("已成功释放端口 {port}，结束进程 {process_name}
///   (PID: {pid})")`；
/// - `ERROR_ACCESS_DENIED`：`ToastRequested("需要管理员权限，请通过顶部盾牌提权运行")`
///   （UIPI 防御——终止更高完整性进程需要本程序以管理员身份运行）；
/// - 其他错误：不发布 Toast（由装配层写审计并弹通用失败提示）。
///
/// `bus` 为 `None` 时仅执行低层动作（无 Toast），供测试 / 无总线场景使用。
pub fn kill_process_with_events(
    bus: Option<&crate::bus::EventBus>,
    pid: u32,
    port: u16,
    protocol: &str,
    process_name: &str,
) -> Result<(), PortError> {
    let outcome = kill_process_and_release_port(pid, port, protocol);
    let Some(bus) = bus else {
        return outcome;
    };
    match &outcome {
        Ok(()) => {
            bus.publish(AppEvent::ToastRequested(format!(
                "已成功释放端口 {port}，结束进程 {process_name} (PID: {pid})"
            )));
        }
        Err(err) if err.is_access_denied() => {
            bus.publish(AppEvent::ToastRequested(
                "需要管理员权限，请通过顶部盾牌提权运行".to_string(),
            ));
        }
        Err(_) => {}
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 端口 / 协议的参数上下文不影响低层动作的错误判定（纯文档参数）。
    #[test]
    fn error_enum_reports_access_denied_and_codes() {
        let denied_open = PortError::OpenProcessFailed { pid: 42, code: 5 };
        assert!(denied_open.is_access_denied(), "错误码 5 应判定为 UIPI 拦截");
        assert_eq!(denied_open.code(), Some(5));
        assert_eq!(denied_open.win32_summary(), "Win32 错误码 5 (ERROR_ACCESS_DENIED)");
        assert!(
            denied_open.to_string().contains("PID: 42"),
            "错误文本应回显 PID"
        );

        let denied_term = PortError::TerminateFailed { pid: 7, code: 5 };
        assert!(denied_term.is_access_denied());

        let other = PortError::OpenProcessFailed { pid: 1, code: 87 };
        assert!(!other.is_access_denied(), "87 不是 access denied");
        assert_eq!(other.win32_summary(), "Win32 错误码 87 (ERROR_INVALID_PARAMETER)");

        let scan = PortError::ScanFailed { context: "GetExtendedTcpTable 枚举", code: 122 };
        assert!(!scan.is_access_denied());
        assert!(scan.to_string().contains("GetExtendedTcpTable"));
    }

    /// 未知 PID 的终止必然失败且错误可读（Windows 集成）；非 Windows 平台
    /// 直接返回 UnsupportedPlatform。
    #[cfg(windows)]
    #[test]
    fn killing_nonexistent_process_fails_gracefully() {
        let err = kill_process_and_release_port(u32::MAX, 8080, "TCP")
            .expect_err("不存在的 PID 应报错而非 panic");
        assert!(
            err.code().is_some(),
            "应携带 Win32 错误码，实际: {err}"
        );
        // 打开不存在的进程 = ERROR_INVALID_PARAMETER(87) 或 ACCESS_DENIED(5)，
        // 两者都不应 panic 且 message 可展示。
        assert!(!err.to_string().is_empty());
    }

    /// 不存在的总线句柄（None）不发布事件仍返回底层结果（低层组合可测）。
    #[test]
    fn kill_without_bus_still_reports_low_level_result() {
        // u32::MAX 恒为不存在的 PID：低层必然失败且带码；总线为 None 时结果
        // 不被总线逻辑改写（契约验证点）。
        let err = kill_process_with_events(None, u32::MAX, 8080, "TCP", "ghost.exe").unwrap_err();
        #[cfg(not(windows))]
        assert_eq!(err, PortError::UnsupportedPlatform);
        #[cfg(windows)]
        assert!(err.code().is_some(), "Windows 下应携带 Win32 错误码，实际: {err}");
    }
}