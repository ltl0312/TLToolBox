//! # 单实例守护：具名互斥锁 + 广播唤醒（Windows 常驻安全机制）
//!
//! TLToolBox 定位为「后台托盘静默常驻」型应用：若用户再次双击 exe（或系统重复
//! 自启），会拉起重复进程，造成托盘双图标以及 Win32 钩子、剪贴板监听器资源的
//! 重复抢占。本模块在**启动最早阶段**（配置 / 模块 / UI 装配之前）提供两道防线：
//!
//! 1. **唯一性互斥**：[`CreateMutexW`] 创建会话级具名互斥
//!    `Local\TLToolBox_SingleInstance_Mutex`；紧接着 [`GetLastError`] 若返回
//!    [`ERROR_ALREADY_EXISTS`]，说明已有实例持有同名互斥——本进程即“第二实例”；
//! 2. **唤醒既有实例**：第二实例经 [`RegisterWindowMessageW`] 注册系统级广播消息
//!    `TLTOOLBOX_WAKEUP_EXISTING_INSTANCE`，向 [`HWND_BROADCAST`] 投递
//!    [`PostMessageW`] 请求既有实例把静默隐藏的主窗口还原前置；随后本进程立即
//!    退出（装配层 `main` 记录日志后 `return Ok(())`）。
//!
//! # 接收端（主实例）
//!
//! 主实例在托盘线程的 Win32 原生消息泵中监听同一注册消息编号（见
//! [`crate::tray`]），收到后向事件总线发布
//! [`AppEvent::TrayAction(TrayAction::ShowWindow)`](crate::bus::TrayAction)——与托盘
//! 菜单「显示主窗口」/ 双击共用同一条唤醒管线。注册消息编号**系统内唯一且跨进程
//! 一致**（同一字符串恒映射到同一编号），监听方只需比对 [`register_wakeup_message`]
//! 返回的编号即可，无需关心谁先注册。
//!
//! # 互斥句柄生命周期
//!
//! 主实例取得的句柄由 [`SingleInstanceGuard`] 持有，随 `main` 作用域退出（进程
//! 平滑收尾）时 `Drop` → [`CloseHandle`] 释放；进程异常终止时系统亦自动回收。
//! 第二实例检测成功后**必须立即退出**——若它也持有同名句柄，互斥将因“仍有打开
//! 句柄”而失效（这正是第二实例在获取句柄后立即 `CloseHandle` 的原因）。
//!
//! # 提权交接期（v0.6.1 · S6 整改）
//!
//! 提权重启由旧实例经 `ShellExecuteW("runas")` 拉起新实例，而旧实例尚持有互斥、
//! 要等平滑收尾才释放——于是新实例启动时必然看到 `ERROR_ALREADY_EXISTS`。这**不是
//! 重复启动而是预期交接**，故新实例不得走「第二实例立即退出」路径。
//!
//! 旧实现对该分支直接把守卫降级为 `None`。旧实例退出后**没有任何进程持有互斥**，
//! 整个提权执行期间单实例保证被打破：此时再次双击 exe 的新副本会被判定为 `Primary`
//! 并常驻，导致托盘多图标、WinEvent 钩子 / 剪贴板监听 / 终端日志钩子重复注册、
//! 模块状态互相覆盖、日志文件跨进程竞争。
//!
//! 现由 [`acquire_after_handoff`] 承担交接：以退避重试**持续询位**（不广播唤醒
//! 消息——交接期不是"用户重复启动"），直到真正成为 `Primary` 并重新持有守卫；
//! 超时才降级为 `Secondary` 并由调用方如实告警。
//!
//! # 非 Windows 平台
//!
//! 具名互斥是 Windows 原生能力；非 Windows 目标上 [`acquire`] 恒返回
//! [`SingleInstanceOutcome::Primary`]（占位守卫），保证装配代码跨平台可编译，
//! 单实例语义仅限 Windows 生效。
//!
//! [`CreateMutexW`]: https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-createmutexw
//! [`GetLastError`]: https://learn.microsoft.com/en-us/windows/win32/api/errhandlingapi/nf-errhandlingapi-getlasterror
//! [`ERROR_ALREADY_EXISTS`]: https://learn.microsoft.com/en-us/windows/win32/debug/system-error-codes--0-499-
//! [`RegisterWindowMessageW`]: https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-registerwindowmessagew
//! [`HWND_BROADCAST`]: https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-postmessagew
//! [`PostMessageW`]: https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-postmessagew
//! [`CloseHandle`]: https://learn.microsoft.com/en-us/windows/win32/api/handleapi/nf-handleapi-closehandle

use std::fmt;
#[cfg(windows)]
use std::time::Duration;

/// 会话级具名互斥体名称（唯一实例令牌）。
///
/// `Local\` 前缀 → **会话级**命名空间：无需提权、随登录会话自然隔离（同一用户的
/// 不同会话可各自常驻一份实例，符合桌面工具预期）。第二个实例以同名调用
/// [`CreateMutexW`] 时，系统返回既有互斥的句柄副本并把上次错误码置为
/// [`ERROR_ALREADY_EXISTS`]——据此判别谁是主实例。
#[cfg(windows)]
pub const INSTANCE_MUTEX_NAME: &str = r"Local\TLToolBox_SingleInstance_Mutex";

/// 唤醒广播消息字符串（`RegisterWindowMessageW` 注册）。
///
/// 注册后的消息编号落在 `0xC000..=0xFFFF` 区间，**系统内唯一**：第二实例向
/// `HWND_BROADCAST` 广播时使用与主实例完全相同的编号（同一字符串跨进程映射一致）。
pub const WAKEUP_MESSAGE_NAME: &str = "TLTOOLBOX_WAKEUP_EXISTING_INSTANCE";

/// 提权交接期等待互斥易手的上限（S6）：超过即放弃守护、如实告警。
///
/// 15s 是「旧实例平滑收尾（逆序停模块 + 关托盘 + 刷日志）」的宽裕上界；正常
/// 情况下旧实例在数百毫秒内释放互斥，本等待几乎瞬时命中。
#[cfg(windows)]
pub const HANDOFF_WAIT: Duration = Duration::from_secs(15);

/// 交接期询位重试间隔（100ms；上限内至多约 150 次轮询）。
#[cfg(windows)]
const HANDOFF_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// 单实例检查结果。
#[derive(Debug)]
pub enum SingleInstanceOutcome {
    /// 本进程是**唯一实例**：持有互斥句柄守卫至进程退出
    /// （[`SingleInstanceGuard`] 的 `Drop` 负责 `CloseHandle`）。
    Primary(SingleInstanceGuard),
    /// 已存在运行中的实例：唤醒广播已尝试投递（`wakeup_delivered = true` 表示
    /// `PostMessageW(HWND_BROADCAST)` 投递成功）。调用方应记录日志后立即退出。
    Secondary {
        /// 唤醒消息是否成功广播给既有实例。
        wakeup_delivered: bool,
    },
}

/// 单实例互斥句柄守卫：持有 `CreateMutexW` 返回的句柄，`Drop` 时自动释放。
///
/// 句柄以 `usize`（指针宽度整数）保存而非原生 `HANDLE`：原始句柄包装内部是指针、
/// 不实现 `Send`/`Sync`，整数化后守卫可安全跨线程持有——主实例的守卫由异步
/// `main` 的局部变量持有至进程收尾，无需任何跨线程访问（仅 `Drop` 关闭一次）。
pub struct SingleInstanceGuard {
    #[cfg(windows)]
    handle: usize,
    #[cfg(not(windows))]
    _placeholder: (),
}

impl SingleInstanceGuard {
    /// 由 `CreateMutexW` 成功返回的句柄构造守卫（仅 Windows）。
    #[cfg(windows)]
    fn from_handle(handle: usize) -> Self {
        Self { handle }
    }

    /// 非 Windows 占位守卫（单实例语义仅 Windows 生效）。
    #[cfg(not(windows))]
    fn placeholder() -> Self {
        Self { _placeholder: () }
    }
}

impl fmt::Debug for SingleInstanceGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SingleInstanceGuard")
            .finish_non_exhaustive()
    }
}

impl Drop for SingleInstanceGuard {
    #[cfg(windows)]
    fn drop(&mut self) {
        imp::close_handle(self.handle);
    }

    #[cfg(not(windows))]
    fn drop(&mut self) {}
}

/// 单实例守护错误。
#[derive(Debug)]
pub enum SingleInstanceError {
    /// `CreateMutexW` 调用硬失败（非 `ERROR_ALREADY_EXISTS` 的错误路径，
    /// 如名称非法 / 访问被拒）。此时无法判定唯一性，调用方应降级为无守护运行。
    CreateMutex { reason: String },
}

impl fmt::Display for SingleInstanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateMutex { reason } => {
                write!(f, "无法创建单实例互斥锁: {reason}")
            }
        }
    }
}

impl std::error::Error for SingleInstanceError {}

/// 尝试成为唯一实例（在配置 / 模块 / UI 装配之前调用）。
///
/// - Windows：创建会话级具名互斥；若 [`GetLastError`] 报告
///   [`ERROR_ALREADY_EXISTS`]，则关闭本进程拿到的句柄副本、广播唤醒消息并返回
///   [`SingleInstanceOutcome::Secondary`]；
/// - 首个实例持有句柄守卫（返回 [`SingleInstanceOutcome::Primary`]）；
/// - 非 Windows：恒返回 `Primary`（占位守卫，单实例语义不生效）。
pub fn acquire() -> Result<SingleInstanceOutcome, SingleInstanceError> {
    #[cfg(windows)]
    {
        imp::acquire_named(INSTANCE_MUTEX_NAME)
    }
    #[cfg(not(windows))]
    {
        tracing::debug!(
            target: "single_instance",
            "非 Windows 平台：单实例守护退化为空操作，本进程按主实例运行"
        );
        Ok(SingleInstanceOutcome::Primary(
            SingleInstanceGuard::placeholder(),
        ))
    }
}

/// 提权交接期：等待旧（低权限）实例释放互斥后**重新持有**唯一性守卫（S6）。
///
/// # 语义
/// 由 `main` 在「命令行含 `RESTART_MARKER_ARG` 且初次获取判定为 `Secondary`」时
/// 调用——该组合意味着本进程是旧实例经 UAC 拉起的后继，旧实例尚在平滑收尾。
///
/// - 以 [`HANDOFF_RETRY_INTERVAL`] 为间隔询位，直到成为 `Primary`（立即返回并交出
///   守卫，单实例保证**全程连续**）；
/// - **不广播唤醒消息**：交接期不是"用户重复启动"，向旧实例广播只会让它把正在
///   收尾的主窗口再弹一次（且每次询位都会弹）；
/// - 超过 `timeout` 仍未取得（旧实例卡住 / 未退出）→ 返回 `Secondary`，
///   由调用方如实告警并降级——不静默失败、不阻塞应用启动。
///
/// 非 Windows 平台恒返回 `Primary`（占位守卫，单实例语义不生效）。
pub async fn acquire_after_handoff(
    timeout: Duration,
) -> Result<SingleInstanceOutcome, SingleInstanceError> {
    #[cfg(windows)]
    {
        acquire_after_handoff_named(INSTANCE_MUTEX_NAME, timeout).await
    }
    #[cfg(not(windows))]
    {
        let _ = timeout;
        Ok(SingleInstanceOutcome::Primary(
            SingleInstanceGuard::placeholder(),
        ))
    }
}

/// [`acquire_after_handoff`] 的可注入名称实现（测试用唯一互斥名直接驱动语义）。
#[cfg(windows)]
async fn acquire_after_handoff_named(
    name: &str,
    timeout: Duration,
) -> Result<SingleInstanceOutcome, SingleInstanceError> {
    let deadline = std::time::Instant::now() + timeout;
    let mut attempts: u32 = 0;
    loop {
        attempts += 1;
        match imp::probe_named(name)? {
            Some(guard) => {
                tracing::info!(
                    target: "single_instance",
                    attempts,
                    "提权交接完成：旧实例已释放互斥，本（高权限）实例重新持有单实例守护"
                );
                return Ok(SingleInstanceOutcome::Primary(guard));
            }
            None => {
                if std::time::Instant::now() >= deadline {
                    tracing::warn!(
                        target: "single_instance",
                        attempts,
                        "提权交接等待超时（{}ms）：未能取得单实例互斥，本次降级为无守护运行",
                        timeout.as_millis()
                    );
                    return Ok(SingleInstanceOutcome::Secondary {
                        wakeup_delivered: false,
                    });
                }
                tokio::time::sleep(HANDOFF_RETRY_INTERVAL).await;
            }
        }
    }
}

/// 取得（或注册）唤醒广播消息编号；返回 `0` 表示注册失败（极罕见）。
///
/// 供接收端（托盘消息泵）在泵循环中比对 `MSG.message`：编号与第二实例广播时
/// 使用的一致（同一字符串系统内唯一映射），无需关注注册先后。
#[cfg(windows)]
pub fn register_wakeup_message() -> u32 {
    imp::register_wakeup()
}

/// 非 Windows 兜底：无广播唤醒能力，恒返回 `0`（无监听方会使用该值）。
#[cfg(not(windows))]
pub fn register_wakeup_message() -> u32 {
    0
}

// ---------------------------------------------------------------------------
// Windows 原生实现（CreateMutexW / GetLastError / PostMessageW 等）
// ---------------------------------------------------------------------------

/// Windows 平台实现：具名互斥获取、唤醒广播注册与投递、句柄关闭。
#[cfg(windows)]
mod imp {
    use super::*;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, BOOL, ERROR_ALREADY_EXISTS, HANDLE, LPARAM, WPARAM,
    };
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::Win32::UI::WindowsAndMessaging::{
        PostMessageW, RegisterWindowMessageW, HWND_BROADCAST,
    };

    /// 字符串 → NUL 结尾的 UTF-16LE 单元序列（Win32 字符串 API 的载荷形态）。
    fn to_wide_units(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 以指定名称尝试获取单实例互斥（主入口）。
    ///
    /// `binitialowner = false`：只借互斥的**存在性**做唯一性判别，从不真正占用
    /// （不调用 `ReleaseMutex`），句柄语义退化为“该名称是否已被本进程 / 其他进程
    /// 持有”。
    pub(super) fn acquire_named(name: &str) -> Result<SingleInstanceOutcome, SingleInstanceError> {
        match probe_named(name)? {
            Some(guard) => Ok(SingleInstanceOutcome::Primary(guard)),
            None => {
                tracing::info!(
                    target: "single_instance",
                    "检测到既有实例持有互斥 '{name}'：广播唤醒消息并转入退出路径"
                );
                let delivered = notify_existing_instance();
                Ok(SingleInstanceOutcome::Secondary {
                    wakeup_delivered: delivered,
                })
            }
        }
    }

    /// 单次询位：互斥可独占时返回守卫（`Some`），否则关闭本进程拿到的副本并返回
    /// `None`（S6 的交接重试循环与 [`acquire_named`] 共用本原语）。
    ///
    /// **不产生任何广播副作用**——交接期由 [`super::acquire_after_handoff`] 高频
    /// 轮询本函数，若在此广播会让旧实例的收尾窗口被反复弹出。
    pub(super) fn probe_named(
        name: &str,
    ) -> Result<Option<SingleInstanceGuard>, SingleInstanceError> {
        let wide = to_wide_units(name);
        // SAFETY: wide 为 NUL 结尾的 UTF-16 缓冲，其指针在本调用期间存活；
        // SECURITY_ATTRIBUTES 传 None（默认安全描述符，会话级命名空间无需提权）；
        // binitialowner = BOOL(0)（windows-0.58 的 BOOL 参数需显式传 BOOL 包装值，
        // 原生 bool 不满足其 Param 约束）；lpname 传 PCWSTR 值（Option 包装不满足
        // windows-0.58 的 Param<PCWSTR> 约束；本路径恒提供非空名称）。
        let handle =
            unsafe { CreateMutexW(None, BOOL(0), PCWSTR(wide.as_ptr())) }.map_err(|err| {
                SingleInstanceError::CreateMutex {
                    reason: err.to_string(),
                }
            })?;

        // CreateMutexW 对“已存在的同名互斥”仍返回有效句柄，只是把上次错误码
        // 置为 ERROR_ALREADY_EXISTS——须紧接调用 GetLastError 读取（包装层内部
        // 均为纯 Rust 逻辑，未穿插其他 FFI 调用，错误码仍有效）。
        let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;

        if already_exists {
            // 关闭本进程拿到的句柄副本（不释放既有实例的互斥；若保留到进程退出，
            // 反而会因“仍存打开句柄”而破坏互斥的唯一性判定）。
            close_handle(handle.0 as usize);
            return Ok(None);
        }

        Ok(Some(SingleInstanceGuard::from_handle(handle.0 as usize)))
    }

    /// 注册唤醒广播消息并返回其系统内唯一编号；`0` = 注册失败。
    pub(super) fn register_wakeup() -> u32 {
        let wide = to_wide_units(WAKEUP_MESSAGE_NAME);
        // SAFETY: wide 为 NUL 结尾的 UTF-16 缓冲，其指针在本调用期间存活。
        unsafe { RegisterWindowMessageW(PCWSTR(wide.as_ptr())) }
    }

    /// 向 `HWND_BROADCAST` 投递唤醒消息（第二实例路径）。
    ///
    /// 返回是否投递成功。广播会送达**所有顶层窗口**（含托盘线程创建的隐藏
    /// 窗口）；主实例的托盘消息泵识别该编号后发布
    /// [`AppEvent::TrayAction(TrayAction::ShowWindow)`](crate::bus::TrayAction)。
    fn notify_existing_instance() -> bool {
        let message_id = register_wakeup();
        if message_id == 0 {
            tracing::error!(
                target: "single_instance",
                "唤醒消息注册失败（编号 0），无法通知既有实例显示主窗口"
            );
            return false;
        }
        // SAFETY: HWND_BROADCAST 向全部顶层窗口投递；wparam/lparam 恒为 0
        // （无载荷，唤醒仅凭消息编号本身）。
        match unsafe { PostMessageW(HWND_BROADCAST, message_id, WPARAM(0), LPARAM(0)) } {
            Ok(()) => {
                tracing::info!(
                    target: "single_instance",
                    message_id,
                    "唤醒广播已投递（HWND_BROADCAST）"
                );
                true
            }
            Err(err) => {
                tracing::warn!(
                    target: "single_instance",
                    "唤醒广播投递失败（既有实例可能无法被唤醒显示）: {err}"
                );
                false
            }
        }
    }

    /// 关闭互斥句柄（守卫 `Drop` 调用；幂等，句柄为 0 时跳过）。
    pub(super) fn close_handle(handle: usize) {
        if handle != 0 {
            // SAFETY: handle 来自 CreateMutexW 的成功返回且仅关闭一次
            // （守卫 Drop 或第二实例的副本清理路径各自独占）。
            unsafe {
                let _ = CloseHandle(HANDLE(handle as *mut core::ffi::c_void));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造本进程内唯一的测试互斥名（避免并行测试线程互相干扰）。
    #[cfg(windows)]
    fn unique_test_name(tag: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("系统时钟应晚于 UNIX 纪元")
            .as_nanos();
        format!(
            r"Local\TLToolBox_Test_{}_{}_{}",
            std::process::id(),
            tag,
            nanos
        )
    }

    /// Windows：同名二次获取应判别为 Secondary；主守卫释放后可再次成为 Primary。
    ///
    /// 二次获取路径会向 HWND_BROADCAST 广播唤醒消息——测试环境中无监听方，
    /// 广播被各顶层窗口忽略，无可见副作用。
    #[cfg(windows)]
    #[test]
    fn second_acquisition_of_same_name_is_secondary_until_released() {
        let name = unique_test_name("mutex");
        let name_owned = name.clone();

        // 1) 首次获取 → Primary，句柄守卫存活。
        let primary = imp::acquire_named(&name).expect("首次获取应成功");
        let guard = match primary {
            SingleInstanceOutcome::Primary(guard) => guard,
            other => panic!("首次获取应为主实例，实际: {other:?}"),
        };

        // 2) 守卫仍在 → 同名的二次获取应判别为 Secondary（既有实例仍存活）。
        let second = imp::acquire_named(&name_owned).expect("二次获取应成功");
        match second {
            SingleInstanceOutcome::Secondary { .. } => {}
            other => panic!("应判别为第二实例，实际: {other:?}"),
        }

        // 3) 释放守卫（互斥名被回收）→ 再次获取应重新成为 Primary。
        drop(guard);
        let again = imp::acquire_named(&name_owned).expect("重取应成功");
        match again {
            SingleInstanceOutcome::Primary(_guard2) => {
                // _guard2 在此作用域末尾 Drop：测试自清理。
            }
            other => panic!("守卫释放后应可重新成为主实例，实际: {other:?}"),
        }
    }

    /// Windows：注册唤醒消息编号稳定（同串两次注册一致）且非 0。
    #[cfg(windows)]
    #[test]
    fn wakeup_message_registration_is_stable_and_nonzero() {
        let first = super::register_wakeup_message();
        let second = super::register_wakeup_message();
        assert!(first != 0, "注册的广播消息编号不应为 0");
        assert_eq!(first, second, "同一字符串重复注册应返回同一编号");
        // 注册编号位于系统保留的 0xC000..=0xFFFF 区间。
        assert!(
            (0xC000..=0xFFFF).contains(&first),
            "RegisterWindowMessageW 编号应落在 0xC000..=0xFFFF: {first:#x}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn instance_mutex_name_is_session_scoped() {
        assert!(
            INSTANCE_MUTEX_NAME.starts_with(r"Local\"),
            "互斥名应使用会话级 Local\\ 命名空间: {INSTANCE_MUTEX_NAME}"
        );
        assert!(
            INSTANCE_MUTEX_NAME.contains("TLToolBox"),
            "互斥名应含应用标识避免与其他程序冲突: {INSTANCE_MUTEX_NAME}"
        );
    }

    /// 非 Windows：单实例语义不生效，恒为主实例（占位守卫）。
    #[cfg(not(windows))]
    #[test]
    fn non_windows_always_primary() {
        match super::acquire().expect("非 Windows 获取应成功") {
            SingleInstanceOutcome::Primary(_guard) => {}
            other => panic!("非 Windows 平台应恒为主实例，实际: {other:?}"),
        }
    }

    // ---- S6：提权交接期重新持有互斥（v0.6.1 整改） ----

    /// 交接契约：旧实例未释放时按超时降级为 `Secondary`；旧实例释放后**重新取得**
    /// `Primary` 并持有守卫——这正是「提权期间单实例保证不出现空窗」的直接验证。
    #[cfg(windows)]
    #[tokio::test]
    async fn handoff_reacquires_mutex_once_previous_instance_releases() {
        let name = unique_test_name("handoff");

        // 模拟旧实例：直接询位并持住守卫。
        let holder = imp::probe_named(&name)
            .expect("询位应成功")
            .expect("首次询位应取得互斥");

        // 1) 旧实例仍持有：等待至超时必须放弃（返回 Secondary），且不得 panic / 挂死。
        let started = std::time::Instant::now();
        match acquire_after_handoff_named(&name, Duration::from_millis(300))
            .await
            .expect("交接等待不应硬失败")
        {
            SingleInstanceOutcome::Secondary { .. } => {}
            other => panic!("旧实例未释放时不应取得互斥，实际: {other:?}"),
        }
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "应完整等待到超时点，实际 {:?}",
            started.elapsed()
        );

        // 2) 旧实例退出（释放守卫）→ 交接必须重新取得 Primary，空窗期结束。
        drop(holder);
        match acquire_after_handoff_named(&name, Duration::from_secs(5))
            .await
            .expect("交接等待不应硬失败")
        {
            SingleInstanceOutcome::Primary(_guard) => {
                // 守卫在此作用域末尾 Drop：测试自清理。
            }
            other => panic!("旧实例释放后应重新取得互斥，实际: {other:?}"),
        }
    }

    /// 交接期间**不得**取得互斥的另一面：交接成功后同名再次获取仍应判别为
    /// `Secondary`——证明重新持有的守卫是真实有效的，而非"看起来拿到了"。
    #[cfg(windows)]
    #[tokio::test]
    async fn handoff_guard_actually_holds_uniqueness() {
        let name = unique_test_name("handoff-hold");
        let guard = match acquire_after_handoff_named(&name, Duration::from_secs(5))
            .await
            .expect("空闲互斥应立即取得")
        {
            SingleInstanceOutcome::Primary(guard) => guard,
            other => panic!("空闲互斥应立即可用，实际: {other:?}"),
        };
        assert!(
            imp::probe_named(&name).expect("询位应成功").is_none(),
            "交接取得的守卫必须真实占位（其他人询位应为 None）"
        );
        drop(guard);
    }

    /// 交接等待上限必须是有界的短常量（否则提权后启动会被旧实例拖住）。
    #[cfg(windows)]
    #[test]
    fn handoff_wait_is_bounded() {
        assert!(
            HANDOFF_WAIT <= Duration::from_secs(30),
            "交接等待上限过长会明显拖慢提权后的启动"
        );
        assert!(!HANDOFF_WAIT.is_zero(), "上限不得为 0（否则永不等待）");
    }
}
