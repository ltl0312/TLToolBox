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
//! # 系统关键进程闸门（v0.6.1 · S2 整改，终止动作前的最后一道闸门）
//!
//! [`kill_process_and_release_port`] 在 `OpenProcess` **之前**无条件执行一次身份
//! 复核：`is_kernel_owner(pid) || is_system_service_entry(pid, 当前镜像名, port)`
//! 命中即返回 [`PortError::ProtectedProcess`]，**永不进入终止调用**。
//!
//! 三重失守是 S2 的成因，本闸门针对性地堵住前两点：
//! 1. 扫描期的 `SYSTEM_IMAGE_BLACKLIST` 只作用于**展示**——`show_system_ports ==
//!    true` 时 `lsass.exe` / `services.exe` / `svchost.exe` 等条目照常出现在列表；
//!    **"可显示"不等于"可终止"**，故本闸门与展示选项彻底解耦（不接收该开关）；
//! 2. UI 侧释放按钮曾不区分系统行（`confirm_before_kill` 默认可关闭 → 单击即终止）；
//! 3. 旧实现在终止前零身份复核（`let _ = (port, protocol);` 连进程名都丢弃）。
//!
//! 镜像名取**终止时刻**的实时查询结果（`QueryFullProcessImageNameW`），而非 UI
//! 送来的陈旧缓存名——这同时消除了 PID 复用带来的身份错配（S3 的缓解项）。
//!
//! # 目标新鲜度核验（v0.6.1 · S3 整改，消除 PID 复用 TOCTOU）
//!
//! 用户点击的目标 `pid` 来自**上一次扫描的缓存**，点击可能发生在数分钟之后。从
//! 「枚举到该 PID」到「真正 `OpenProcess`」之间，原进程可能已退出、端口可能已被
//! 释放，或 PID 被系统**复用**给任意其他进程（含系统服务 / 提权进程）——旧实现
//! 直接用陈旧 PID 执行终止，会误杀与目标端口毫无关系的进程。
//!
//! 现于终止前追加两道核验（与 S2 闸门共用同一次实时身份查询）：
//! 1. **身份复核**：实时镜像名 vs 扫描缓存的展示名，不一致即判定 PID 已被复用；
//! 2. **端口新鲜度**：重新枚举 TCP / UDP 表，确认该 PID **此刻仍持有**该端口。
//!
//! 任一不过即返回 [`PortError::StaleTarget`]（不发起任何终止调用），提示用户
//! 「请刷新列表后重试」。核验所需的超长路径读取由
//! [`scanner::query_process_identity`] 的按需扩容保证（M9②）。
//!
//! # 二次确认（配置开关）
//!
//! `confirm_before_kill` 决定 UI 是否在点击「一键释放」后先行展示行内确认态；
//! 本模块只执行「确认后的最终动作」，确认流程自身的分支逻辑放在装配层测试
//! （见 `src/main.rs` 与配置节单测）。

use crate::bus::AppEvent;
use crate::modules::port_hunter::scanner;

#[cfg(windows)]
use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED};
#[cfg(windows)]
use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

/// 终止前**新鲜度核验**未通过的具体项（v0.6.1 · S3）。
///
/// 三类都不代表"点击有误"——用户点的行在点击那一刻是真实存在的，只是从扫描到
/// 点击之间世界变了。因此提示语是「请刷新后重试」，而不是任何权限 / 配置建议。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleCheck {
    /// 目标 PID 已无法打开（进程已退出，或已被更高完整性进程接管）。
    ProcessGone,
    /// 实时镜像名与扫描时记录的名称不一致 → PID 极可能已被系统复用。
    ImageChanged,
    /// 重新枚举监听表：该端口此刻**不是**由目标 PID 持有（已释放或已易主）。
    PortReleased,
}

impl StaleCheck {
    /// 面向用户 / 审计的中文说明。
    pub fn describe(self) -> &'static str {
        match self {
            Self::ProcessGone => "目标进程已退出",
            Self::ImageChanged => "目标进程身份已变化（PID 可能被复用）",
            Self::PortReleased => "该端口此刻已不由目标进程持有",
        }
    }
}

impl std::fmt::Display for StaleCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.describe())
    }
}

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
    /// **系统关键进程闸门**（v0.6.1 · S2）：目标 PID / 端口命中系统保护清单
    /// （内核态 PID ≤ 4、`svchost.exe` / `lsass.exe` / `services.exe` 等系统镜像、
    /// 135/139/445 等系统保留端口），终止动作**未被发起**。
    ///
    /// 该拒绝与 UI 的「显示系统服务与高位端口」选项**无关**——列表可显示不等于
    /// 可终止；命中即拒绝，不存在任何可绕过的配置开关。
    ProtectedProcess {
        /// 被拒绝终止的目标 PID。
        pid: u32,
        /// 命中原因（人类可读，供审计 / Toast 回显）。
        reason: &'static str,
    },
    /// **目标已失效**（v0.6.1 · S3）：终止前的新鲜度核验未通过，动作**未被发起**。
    ///
    /// 缓存中的 `pid` 从扫描到点击之间可能已退出、已被系统复用、或已不再持有该
    /// 端口；此时继续终止就会误杀无关进程。命中即中止并要求用户刷新列表。
    StaleTarget {
        /// 目标 PID。
        pid: u32,
        /// 未通过的具体核验项。
        check: StaleCheck,
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
            PortError::OpenProcessFailed { code, .. } | PortError::TerminateFailed { code, .. } => {
                *code == ERROR_ACCESS_DENIED.0
            }
            _ => false,
        }
    }

    /// 是否被**系统关键进程闸门**拒绝（S2）。此类拒绝是**设计内**的确定性结论，
    /// 不是环境 / 权限故障——提权后重试同样会被拒绝。
    pub fn is_protected(&self) -> bool {
        matches!(self, PortError::ProtectedProcess { .. })
    }

    /// 是否为**目标已失效**（S3）：缓存过期导致动作被中止，刷新列表后重试即可。
    pub fn is_stale(&self) -> bool {
        matches!(self, PortError::StaleTarget { .. })
    }

    /// 错误码（无码变体返回 `None`）。
    pub fn code(&self) -> Option<u32> {
        match self {
            PortError::ScanFailed { code, .. }
            | PortError::OpenProcessFailed { code, .. }
            | PortError::TerminateFailed { code, .. } => Some(*code),
            PortError::ProtectedProcess { .. }
            | PortError::StaleTarget { .. }
            | PortError::UnsupportedPlatform => None,
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
            PortError::ProtectedProcess { pid, reason } => write!(
                f,
                "已拒绝终止系统关键进程 (PID: {pid})：{reason}（系统进程受保护，本工具不对其执行终止）"
            ),
            PortError::StaleTarget { pid, check } => write!(
                f,
                "已中止终止 (PID: {pid})：{check}，请刷新列表后重试"
            ),
            PortError::UnsupportedPlatform => write!(f, "端口猎手仅支持 Windows 平台"),
        }
    }
}

impl std::error::Error for PortError {}

/// 身份复核的纯逻辑（S3）：实时镜像名 `identity_name` 与扫描缓存名
/// `expected_name` 是否指向同一进程。
///
/// - `identity_name == UNKNOWN_PROCESS` → [`StaleCheck::ProcessGone`]（进程已退出 /
///   已被更高完整性进程接管，无法作为终止依据）；
/// - `expected_name` 为空或同为 `UNKNOWN_PROCESS` → **跳过比对**（缓存里本就没有
///   可用的名字，交由端口新鲜度与操作系统权限模型兜底）；
/// - 两者不一致（忽略 ASCII 大小写）→ [`StaleCheck::ImageChanged`]（PID 被复用）。
///
/// 抽成纯函数是为了让「PID 复用」这一关键回归点可被离线单测覆盖：真实的
/// 「扫描后结束进程、再点释放」难以在测试中稳定构造，而其**判定语义**可以。
fn check_identity(pid: u32, identity_name: &str, expected_name: &str) -> Result<(), PortError> {
    if identity_name == scanner::UNKNOWN_PROCESS {
        return Err(PortError::StaleTarget {
            pid,
            check: StaleCheck::ProcessGone,
        });
    }
    if !expected_name.is_empty()
        && expected_name != scanner::UNKNOWN_PROCESS
        && !identity_name.eq_ignore_ascii_case(expected_name)
    {
        return Err(PortError::StaleTarget {
            pid,
            check: StaleCheck::ImageChanged,
        });
    }
    Ok(())
}

/// 端口新鲜度复核的纯逻辑（S3）：重新枚举得到的端口持有者 `owner` 是否即目标。
///
/// `None`（此刻无持有者）与「持有者是别的 PID」都判 [`StaleCheck::PortReleased`]。
fn check_port_freshness(pid: u32, owner: Option<u32>) -> Result<(), PortError> {
    match owner {
        Some(owner_pid) if owner_pid == pid => Ok(()),
        _ => Err(PortError::StaleTarget {
            pid,
            check: StaleCheck::PortReleased,
        }),
    }
}

/// 系统关键进程闸门（S2）：判定该目标是否**绝不允许**被本工具终止。
///
/// 判定顺序（任一命中即拒绝）：
/// 1. **内核态**：`pid ≤ 4`（Idle / System）；
/// 2. **实时镜像名 + 端口**：`scanner::is_system_service_entry` —— 覆盖
///    `svchost.exe` / `lsass.exe` / `services.exe` / `spoolsv.exe` / `dwm.exe` /
///    `csrss.exe` / `wininit.exe` / `smss.exe`（大小写不敏感）与系统保留端口
///    （135/137/138/139/445/1900/5353/5355/5357）。
///
/// `process_name` 由调用方传入**终止时刻**的实时查询结果；查询失败时为
/// `<unknown>`，此时仅靠 PID 与端口仍能挡住内核态与保留端口目标。
pub fn is_protected_target(pid: u32, process_name: &str, port: u16) -> bool {
    scanner::is_kernel_owner(pid) || scanner::is_system_service_entry(pid, process_name, port)
}

/// 命中闸门时的原因文本（审计 / 错误回显用）。
fn protected_reason(pid: u32, process_name: &str, port: u16) -> &'static str {
    if scanner::is_kernel_owner(pid) {
        return "目标为内核态系统进程（PID ≤ 4）";
    }
    if scanner::is_system_service_entry(pid, process_name, port) {
        if scanner::SYSTEM_RESERVED_PORTS.contains(&port) {
            return "目标占用系统保留端口";
        }
        return "目标为 Windows 系统服务镜像";
    }
    "目标命中系统关键进程保护清单"
}

/// 低层动作：终止指定 PID 的进程（Windows：`OpenProcess(PROCESS_TERMINATE)` +
/// `TerminateProcess(handle, 1)`），使端口随之释放。
///
/// # 三道闸门（v0.6.1；终止动作之前，任一不过即中止）
/// 1. **系统关键进程闸门**（S2）：[`is_protected_target`] 命中即拒绝，与 UI 的
///    显示选项完全解耦；
/// 2. **身份复核**（S3）：实时查询目标镜像名，与 `expected_name`（扫描缓存中的
///    展示名）比对——不一致说明 PID 已被复用；
/// 3. **端口新鲜度复核**（S3）：重新枚举监听表确认该 PID **此刻仍持有**该端口。
///
/// 三道闸门共用**一次**实时身份查询，成本为一次 `OpenProcess` + 一次监听表枚举
/// （毫秒级，且调用方本就经 `spawn_blocking` 执行）。
///
/// - `expected_name`：扫描缓存里的进程名；传 `""` 或
///   [`scanner::UNKNOWN_PROCESS`] 时跳过第 2 道（无从比对，由第 3 道兜底）；
/// - `port` 参与系统保留端口闸门与第 3 道核验；`protocol` 参与第 3 道核验；
/// - 成功返回 `Ok(())`；任何失败返回结构化 [`PortError`]；
/// - 不含 Toast / 审计——反馈职责见 [`kill_process_with_events`] 与装配层。
pub fn kill_process_and_release_port(
    pid: u32,
    port: u16,
    protocol: &str,
    expected_name: &str,
) -> Result<(), PortError> {
    #[cfg(windows)]
    {
        // —— 第 1 道：系统关键进程闸门（S2）——
        // 实时取镜像名（而非 UI 送来的陈旧缓存名）：既堵住「显示系统服务后一键
        // 误杀 lsass」的路径，也为第 2 道身份复核提供事实依据。
        let (identity_name, _identity_path) = scanner::query_process_identity(pid);
        if is_protected_target(pid, &identity_name, port) {
            let reason = protected_reason(pid, &identity_name, port);
            tracing::warn!(
                target: "port_hunter",
                "拒绝终止系统关键进程：PID {pid}（{identity_name}），端口 {port}，原因：{reason}"
            );
            return Err(PortError::ProtectedProcess { pid, reason });
        }

        // —— 第 2 道：身份复核（S3，消除 PID 复用 TOCTOU）——
        if let Err(err) = check_identity(pid, &identity_name, expected_name) {
            tracing::warn!(
                target: "port_hunter",
                "中止终止：PID {pid} 身份复核未通过（实时 '{identity_name}' / 缓存 '{expected_name}'）: {err}"
            );
            return Err(err);
        }

        // —— 第 3 道：端口新鲜度复核（S3）——
        // 枚举失败（探测本身出错）时不阻断用户操作：宁可让第 1/2 道与操作系统
        // 权限模型兜底，也不因一次探测故障把正常释放变成"点了没反应"。
        match scanner::owner_of_port(port, protocol) {
            Ok(owner) => {
                if let Err(err) = check_port_freshness(pid, owner) {
                    tracing::warn!(
                        target: "port_hunter",
                        "中止终止：端口 {port}/{protocol} 此刻的持有者为 {owner:?}（目标 PID {pid}）"
                    );
                    return Err(err);
                }
            }
            Err(err) => tracing::warn!(
                target: "port_hunter",
                "端口新鲜度核验探测失败（放行第 3 道，交由前两道与系统权限兜底）: {err}"
            ),
        }

        // SAFETY: OpenProcess 以 PROCESS_TERMINATE 打开句柄；失败返回错误码。
        let handle = match unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
            Ok(handle) => handle,
            Err(err) => {
                return Err(PortError::OpenProcessFailed {
                    pid,
                    // Error::code() 为 HRESULT（0x8007xxxx 形态）；低 16 位即
                    // Win32 原始错误码（HRESULT::from_win32 编码契约）。
                    code: (err.code().0 as u32) & 0xFFFF,
                });
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
        let _ = (pid, port, protocol, expected_name);
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
/// - 系统关键进程闸门命中（[`PortError::ProtectedProcess`]）与目标失效
///   （[`PortError::StaleTarget`]）：**不发布 Toast**——两者都不是"操作失败"而是
///   "动作被前置拦下"，且 `StaleTarget` 的可读提示由装配层按刷新语义给出，避免
///   两条反馈互相干扰；
/// - 其他错误：不发布 Toast，但**必留一条 `warn` 日志**（M9①：消除"失败静默"，
///   不把可诊断性完全押在装配层的提示上）。
///
/// `bus` 为 `None` 时仅执行低层动作（无 Toast），供测试 / 无总线场景使用。
pub fn kill_process_with_events(
    bus: Option<&crate::bus::EventBus>,
    pid: u32,
    port: u16,
    protocol: &str,
    process_name: &str,
) -> Result<(), PortError> {
    // 扫描缓存里的展示名同时充当「终止前身份复核」（S3）的比对基准。
    let outcome = kill_process_and_release_port(pid, port, protocol, process_name);
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
        Err(err) => {
            // 含 ProtectedProcess / StaleTarget：Toast 由装配层统一给，这里保证
            // 「每条失败路径至少留下一条日志」。
            tracing::warn!(
                target: "port_hunter",
                "释放端口 {port}/{protocol} 未执行（PID {pid}，进程 {process_name}）: {err}"
            );
        }
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
        assert!(
            denied_open.is_access_denied(),
            "错误码 5 应判定为 UIPI 拦截"
        );
        assert_eq!(denied_open.code(), Some(5));
        assert_eq!(
            denied_open.win32_summary(),
            "Win32 错误码 5 (ERROR_ACCESS_DENIED)"
        );
        assert!(
            denied_open.to_string().contains("PID: 42"),
            "错误文本应回显 PID"
        );

        let denied_term = PortError::TerminateFailed { pid: 7, code: 5 };
        assert!(denied_term.is_access_denied());

        let other = PortError::OpenProcessFailed { pid: 1, code: 87 };
        assert!(!other.is_access_denied(), "87 不是 access denied");
        assert_eq!(
            other.win32_summary(),
            "Win32 错误码 87 (ERROR_INVALID_PARAMETER)"
        );

        let scan = PortError::ScanFailed {
            context: "GetExtendedTcpTable 枚举",
            code: 122,
        };
        assert!(!scan.is_access_denied());
        assert!(scan.to_string().contains("GetExtendedTcpTable"));
    }

    /// 未知 PID 的终止必然失败且错误可读（Windows 集成）；非 Windows 平台
    /// 直接返回 UnsupportedPlatform。
    #[cfg(windows)]
    #[test]
    fn killing_nonexistent_process_fails_gracefully() {
        // u32::MAX 恒不存在：S3 的身份复核会在 `OpenProcess` 之前先判「已退出」。
        let err = kill_process_and_release_port(u32::MAX, 8080, "TCP", "ghost.exe")
            .expect_err("不存在的 PID 应报错而非 panic");
        assert!(err.is_stale(), "不存在的 PID 应被新鲜度核验拦下: {err}");
        assert!(!err.to_string().is_empty());
        // 无 Win32 错误码（动作根本没走到 OpenProcess）。
        assert_eq!(err.code(), None);
    }

    /// 不存在的总线句柄（None）不发布事件仍返回底层结果（低层组合可测）。
    #[test]
    fn kill_without_bus_still_reports_low_level_result() {
        // u32::MAX 恒为不存在的 PID：低层必然失败（S3 判「已退出」）；总线为
        // None 时结果不被总线逻辑改写（契约验证点）。
        let err = kill_process_with_events(None, u32::MAX, 8080, "TCP", "ghost.exe").unwrap_err();
        #[cfg(not(windows))]
        assert_eq!(err, PortError::UnsupportedPlatform);
        #[cfg(windows)]
        assert!(err.is_stale(), "Windows 下应为目标失效，实际: {err}");
    }

    // ---- S3：目标新鲜度核验（v0.6.1 整改，消除 PID 复用 TOCTOU） ----

    /// `StaleCheck` 三类原因都必须自解释（文案进入审计与 Toast）。
    #[test]
    fn stale_check_descriptions_are_self_explanatory() {
        for (check, needle) in [
            (StaleCheck::ProcessGone, "已退出"),
            (StaleCheck::ImageChanged, "复用"),
            (StaleCheck::PortReleased, "持有"),
        ] {
            let text = check.to_string();
            assert!(
                text.contains(needle),
                "{check:?} 文案应含「{needle}」: {text}"
            );
        }
    }

    /// 失效错误模型：无 Win32 码、非 UIPI、非闸门拒绝，文案给出「刷新后重试」的动作。
    #[test]
    fn stale_target_error_model_is_actionable() {
        let err = PortError::StaleTarget {
            pid: 4242,
            check: StaleCheck::ImageChanged,
        };
        assert!(err.is_stale());
        assert!(!err.is_protected(), "失效与闸门拒绝是两类语义");
        assert!(!err.is_access_denied(), "失效不是 UIPI 问题");
        assert_eq!(err.code(), None);
        let text = err.to_string();
        assert!(text.contains("4242"), "应回显 PID: {text}");
        assert!(text.contains("刷新"), "应给出可执行动作: {text}");
    }

    /// 身份复核：镜像名不一致（PID 复用）时**不得**发起终止——用当前进程自身
    /// 冒充"已被复用的 PID"（缓存名写成别的进程），断言命中 `ImageChanged`，
    /// 且失败类型是失效而非 Win32 错误（说明根本没走到 `OpenProcess`）。
    ///
    /// 该用例是**确定性**的：名称不一致这条分支不依赖任何运行时探测结果，因此
    /// 绝不会意外走到真实终止调用（测试进程中"自己杀自己"）。
    #[cfg(windows)]
    #[test]
    fn identity_recheck_blocks_reused_pid_before_terminate() {
        // "not-this.exe" 既不是系统黑名单镜像，也绝不会等于当前测试进程名。
        let err = kill_process_and_release_port(std::process::id(), 5150, "TCP", "not-this.exe")
            .expect_err("镜像名不一致必须被拦下");
        match err {
            PortError::StaleTarget { check, .. } => assert_eq!(
                check,
                StaleCheck::ImageChanged,
                "实时镜像名与缓存名不一致应判 ImageChanged"
            ),
            other => panic!("应判定为目标失效，实际: {other}"),
        }
        assert_eq!(err.code(), None, "拦下时不应产生 Win32 错误码");
    }

    /// 身份复核的**判定语义**全覆盖（离线）：进程消失 / 名字不一致 / 缓存无名
    /// 时跳过比对 / 忽略大小写一致。
    #[test]
    fn identity_recheck_decision_table() {
        let pid = 4242;
        // 实时身份查询失败 → 视为进程已退出。
        match check_identity(pid, scanner::UNKNOWN_PROCESS, "node.exe") {
            Err(PortError::StaleTarget { check, .. }) => assert_eq!(check, StaleCheck::ProcessGone),
            other => panic!("实时身份缺失应判 ProcessGone，实际: {other:?}"),
        }
        // 名称不一致 → PID 复用。
        match check_identity(pid, "node.exe", "java.exe") {
            Err(PortError::StaleTarget { check, .. }) => {
                assert_eq!(check, StaleCheck::ImageChanged)
            }
            other => panic!("名称不一致应判 ImageChanged，实际: {other:?}"),
        }
        // 名称一致（含大小写差异）→ 放行。
        assert!(check_identity(pid, "Node.EXE", "node.exe").is_ok());
        assert!(check_identity(pid, "node.exe", "node.exe").is_ok());
        // 缓存无名（`<unknown>` / 空串）→ 跳过该道，交给端口新鲜度兜底。
        assert!(check_identity(pid, "node.exe", "").is_ok());
        assert!(check_identity(pid, "node.exe", scanner::UNKNOWN_PROCESS).is_ok());
    }

    /// 端口新鲜度复核的**判定语义**全覆盖（离线）：持有者即目标才放行，
    /// 「此刻无持有者」与「持有者是别的进程」一律判 `PortReleased`。
    #[test]
    fn port_freshness_decision_table() {
        let pid = 4242u32;
        assert!(
            check_port_freshness(pid, Some(pid)).is_ok(),
            "持有者即目标应放行"
        );
        for owner in [None, Some(999u32), Some(0u32)] {
            match check_port_freshness(pid, owner) {
                Err(PortError::StaleTarget { check, .. }) => {
                    assert_eq!(
                        check,
                        StaleCheck::PortReleased,
                        "持有者 {owner:?} 应判端口已释放"
                    )
                }
                other => panic!("持有者 {owner:?} 应被拦下，实际: {other:?}"),
            }
        }
    }

    // ---- S2：系统关键进程闸门（v0.6.1 整改） ----

    /// 闸门判定的**纯逻辑**覆盖：内核态、系统镜像、保留端口三类目标一律命中，
    /// 且判定与 UI 的「显示系统服务」选项无关（本函数不接收该开关）。
    #[test]
    fn protected_target_covers_kernel_images_and_reserved_ports() {
        // 内核态（PID ≤ 4）：与镜像名 / 端口无关。
        for pid in [0u32, 4] {
            assert!(
                is_protected_target(pid, "anything.exe", 8080),
                "PID {pid} 内核态目标必须被闸门拒绝"
            );
        }
        // 系统服务镜像（大小写不敏感）。
        for name in [
            "svchost.exe",
            "LSASS.EXE",
            "services.exe",
            "spoolsv.exe",
            "dwm.exe",
            "csrss.exe",
            "wininit.exe",
            "smss.exe",
        ] {
            assert!(
                is_protected_target(888, name, 49153),
                "系统镜像 {name} 必须被闸门拒绝"
            );
        }
        // 系统保留端口（占用者即使是未知镜像也必须拒绝）。
        for port in [135u16, 137, 138, 139, 445, 1900, 5353, 5355, 5357] {
            assert!(
                is_protected_target(1234, scanner::UNKNOWN_PROCESS, port),
                "保留端口 {port} 的目标必须被闸门拒绝"
            );
        }
        // 普通开发进程 + 开发端口：放行（不得把闸门做成"谁都杀不了"）。
        assert!(!is_protected_target(1234, "node.exe", 5173));
        assert!(!is_protected_target(1234, "java.exe", 8080));
        assert!(!is_protected_target(1234, "vite.exe", 3000));
    }

    /// 端到端：闸门命中时**低层终止动作完全未被发起**——以本进程自身（存活且
    /// 非系统镜像）验证"放行"路径仍能走到 `OpenProcess`（返回权限类错误码而非
    /// `ProtectedProcess`），而 `svchost` 类目标在 `OpenProcess` 之前即被拒绝。
    #[cfg(windows)]
    #[test]
    fn gate_rejects_system_targets_without_touching_terminate() {
        // 保留端口 445：即使是当前进程自身（必然存活）也必须被闸门挡下，
        // 证明拒绝发生在 OpenProcess 之前（否则会拿到 access_denied/其他错误码）。
        let self_pid = std::process::id();
        let err = kill_process_and_release_port(self_pid, 445, "TCP", "anything.exe")
            .expect_err("保留端口目标必须被闸门拒绝");
        assert!(
            err.is_protected(),
            "必须是闸门拒绝而非 Win32 错误，实际: {err}"
        );
        assert!(!err.is_access_denied(), "闸门拒绝不应被误判为 UIPI");
        assert!(err.to_string().contains("系统关键进程"));
    }

    /// 闸门错误模型：无 Win32 错误码、非 UIPI、文案点名受保护，且带 PID。
    #[test]
    fn protected_process_error_model_is_self_describing() {
        let err = PortError::ProtectedProcess {
            pid: 888,
            reason: "目标为 Windows 系统服务镜像",
        };
        assert!(err.is_protected());
        assert!(!err.is_access_denied(), "闸门拒绝与 UIPI 是两类语义");
        assert_eq!(err.code(), None, "闸门拒绝没有 Win32 错误码");
        let text = err.to_string();
        assert!(text.contains("888"), "文案应回显 PID: {text}");
        assert!(text.contains("系统服务镜像"), "文案应回显原因: {text}");
    }

    /// 闸门拒绝**不发布任何 Toast**（避免与装配层的通用失败提示重复）。
    #[test]
    fn gate_rejection_publishes_no_toast() {
        let bus = crate::bus::EventBus::new(8);
        let mut rx = bus.subscribe();
        #[cfg(windows)]
        let outcome =
            kill_process_with_events(Some(&bus), std::process::id(), 445, "TCP", "svchost.exe");
        #[cfg(not(windows))]
        let outcome = kill_process_with_events(Some(&bus), 888, 445, "TCP", "svchost.exe");
        assert!(outcome.is_err(), "闸门命中必然失败");
        assert!(
            rx.try_recv().is_err(),
            "闸门拒绝不得经总线发布 Toast（反馈由装配层统一负责）"
        );
    }
}
