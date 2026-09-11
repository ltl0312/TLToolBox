//! # 剪贴板纯文本净化模块（阶段三原生落地 · 第三个常驻守护模块）
//!
//! 后台监听系统剪贴板的每一次内容变动：当复制内容**同时**携带纯文本与富文本载荷
//! （浏览器/网页复制携带 `HTML Format`、Office 类应用复制携带 `Rich Text Format`、
//! 即时通讯/聊天工具复制携带内嵌样式）时，自动把剪贴板清空并仅写回 `CF_UNICODETEXT`
//! 纯文本——剥离开 HTML、RTF、位图等全部富文本格式，使后续粘贴总是得到干净文本。
//!
//! ## 与 Tokio 的隔离（同弹窗拦截模块的第一性原则）
//!
//! `WM_CLIPBOARDUPDATE` 与 `WM_DRAWCLIPBOARD` 一样，本质是 **Win32 窗口消息**：
//! 经 [`AddClipboardFormatListener`] 注册监听后，系统把每次剪贴板变动的通知
//! **投递（posted）到监听窗口所属线程的消息队列**，必须由该线程运行的 Win32 消息泵
//! （`GetMessageW`）取出。Tokio 工作线程是无栈协程调度载体，本身不运行标准 Win32
//! 消息循环，因此在 Tokio 协程中直接创建监听窗口 / 轮询该消息**永远不会被触发**。
//!
//! 本模块因此把「消息窗口创建 + `AddClipboardFormatListener` 注册 + Win32 消息泵」整体
//! 隔离到一条由 `std::thread` 派生的**专用操作系统原生线程**（命名
//! `win32-clipboard-purifier-pump`）中运行；Tokio 侧只负责生命周期编排（启停请求的
//! 收发与结果确认），绝不触碰 Win32 剪贴板监听。
//!
//! ## 为什么采用系统预定义 `STATIC` 类的纯消息窗口（`HWND_MESSAGE`）
//!
//! 与弹窗拦截模块必须自绘窗口类不同，剪贴板监听窗口**不需要任何窗口过程逻辑**——
//! `WM_CLIPBOARDUPDATE` 是投递到消息队列的异步通知（见微软文档对
//! `AddClipboardFormatListener` 的表述 *“it is posted a WM_CLIPBOARDUPDATE message”*），
//! 泵线程在 `GetMessageW` 取消息处**直接拦截**即可，无需派发、无需自定义 `WNDPROC`：
//!
//! 1. 若自定义注册窗口类（`RegisterClassW` + `WNDCLASSW`），windows crate 0.58 要求
//!    额外启用 `Win32_Graphics_Gdi` 特性（`WNDCLASSW` 含 GDI 句柄字段）——为一个纯
//!    消息窗口拖入整套 GDI 绑定，与「feature 精确挑选、避免全量编译膨胀」的原则相悖；
//! 2. 因此直接以系统预定义类 **`STATIC`** 创建窗口：`CreateWindowExW` 对系统类可传
//!    `hInstance = NULL`，无需注册任何类、无需额外特性；`STATIC` 过程对投递的消息
//!    一律走 `DefWindowProcW` 默认丢弃——反正我们不派发任何消息；
//! 3. 窗口以 `HWND_MESSAGE` 为父句柄 → **纯消息窗口**：不可见、不占任务栏、不参与
//!    Alt-Tab，仅作为系统投递 `WM_CLIPBOARDUPDATE` 的载体，常驻开销趋近于零。
//!
//! 由于净化逻辑全部内联在泵循环（实例私有线程的栈上），本模块**不需要**弹窗拦截
//! 模块那种「句柄 → 实例」的进程级静态注册表：防自循环标记等状态天然为线程局部，
//! 不同实例互不串扰。
//!
//! ## 平滑卸载协议
//!
//! 卸载链路必须保证 `RemoveClipboardFormatListener` 与 `DestroyWindow`（都要求与
//! `AddClipboardFormatListener` / `CreateWindowExW` 处于同一线程）一定执行，杜绝监听
//! 窗口与消息泵线程泄漏：
//!
//! 1. `stop()` 先 `cancel()` [`CancellationToken`] 广播停机意图；
//! 2. 再向泵线程定向投递 `PostThreadMessageW(WM_QUIT)`，唤醒阻塞在 `GetMessageW`
//!    中的泵线程（退出码 0，循环终止）；
//! 3. 泵线程退出循环后**在同一线程上**注销剪贴板格式监听并销毁消息窗口，随后线程
//!    自然结束；
//! 4. `stop()` 通过 Join 该原生线程并施加超时，确保卸载动作**已经发生**才向调用方返回。
//!
//! ## 竞态防护
//!
//! `PostThreadMessageW` 要求目标线程**已经建立消息队列**，否则调用失败导致停机信号
//! 丢失、泵线程永久挂起。为此泵线程在回报自身线程 ID 之前，先以
//! `PeekMessageW(PM_NOREMOVE)` 强制建立队列，再与父侧完成一次性握手；握手成功后队列
//! 必然存在，任何后续投递的 `WM_QUIT` 都不会丢失。
//!
//! ## 净化判定与数据安全
//!
//! 判定（纯函数，见 [`should_purge`]）：剪贴板**同时**包含 `CF_UNICODETEXT` 与任一
//! 富文本格式（`HTML Format` / `Rich Text Format` / `Rich Text Format Without
//! Objects`，后三者是需经 `RegisterClipboardFormatW` 解析的注册格式 ID，非固定值）
//! 才触发净化；纯文本复制、图片复制、文件复制均不受打扰。
//!
//! 净化在 **单次持有剪贴板锁**（`OpenClipboard` … `CloseClipboard`）内完成：
//!
//! 1. `GetClipboardData(CF_UNICODETEXT)` 读取纯文本（`GlobalLock` 只读视图 + 首个
//!    NUL 截断，不触碰原句柄的所有权）；
//! 2. **在 `EmptyClipboard` 之前**用 `GlobalAlloc(GMEM_MOVEABLE)` 预分配并写入新文本
//!    块——即使后续分配失败，原剪贴板数据也毫发无损；
//! 3. `EmptyClipboard()` 清空全部既有格式（连同 CF_HTML / RTF / 位图一并剥离），
//!    再 `SetClipboardData(CF_UNICODETEXT, 新块)` 仅写回纯文本（成功后句柄所有权移交
//!    系统；失败则 `GlobalFree` 自释，绝不泄漏）。
//!
//! ## 防自循环保护
//!
//! 写回动作同样构成一次“剪贴板内容变动”，系统会向监听列表（含本窗口）再投递一条
//! `WM_CLIPBOARDUPDATE` 回声。若不加防护，净化模块会反复处理自己的写回（尽管净化后
//! 内容已不再满足判定而自然收敛，仍应显式阻断以杜绝无谓重入与日志风暴）。为此泵循环
//! 内维护一枚 [`EchoGuard`] 标记：**写回成功即布防**，紧随其后的下一条更新若命中已
//! 布防的标记则判定为自我回声、直接跳过并解除布防。纯文本复制等“无需净化”的更新
//! 不触碰标记，因此布防后若先收到的是外部应用的正常复制事件，也只会多跳过一条
//! 无净化必要的更新，最坏情形为延迟一次净化——可接受的尽力而为语义。
//!
//! ## 失败语义（与状态机一致性）
//!
//! - `start` 建窗 / 注册监听失败 → 保持停止态并上报错误（UI 开关自然回滚）；
//! - 运行期 `OpenClipboard` 遭遇并发锁争用（其他进程正持有剪贴板）→ 以递增
//!   退避重试至多 5 次（初次 + 4 次重试，总等待上限 225ms）平滑降级，仍失败
//!   才放弃本次；单次净化失败（延迟渲染提供方无响应等）→ 仅记录日志并放弃
//!   本次，不 panic、不影响后续事件——剪贴板监听天然是“尽力而为”型服务；
//! - `stop` 仅负责信号与 Join，不触碰剪贴板，不存在半途状态。
//!
//! # Safety 说明
//!
//! 全部 Win32 FFI 调用位于 `unsafe extern` 边界内按文档签名调用；剪贴板句柄的锁定 /
//! 解锁严格配对，`SetClipboardData` 移交所有权后不再 `GlobalFree`，分配失败路径上的
//! 自释不重复。`GlobalLock` 返回指针仅在持锁且未解锁的生命周期内解引用读取，读取
//! 边界以 `GlobalSize` 与首个 NUL 双重约束，不存在越界访问路径。

use super::{ModuleError, ToolModule};
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

#[cfg(windows)]
use std::time::Duration;
#[cfg(windows)]
use tokio::sync::oneshot;
#[cfg(windows)]
use windows::core::w;
#[cfg(windows)]
use windows::Win32::{
    Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, WPARAM},
    System::DataExchange::{
        AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
        IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
        RemoveClipboardFormatListener, SetClipboardData,
    },
    System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE},
    System::Threading::GetCurrentThreadId,
    UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetMessageW, PeekMessageW, PostThreadMessageW,
        HWND_MESSAGE, MSG, PM_NOREMOVE, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLIPBOARDUPDATE, WM_QUIT,
    },
};

/// 停机协议中 Join 原生泵线程的等待上限。
///
/// 收到 `WM_QUIT` 后泵线程应立即退出；仅在极端场景（`GetClipboardData` 正等待某个
/// 采用延迟渲染的剪贴板提供方应答的系统内部窗口）下可能略慢，故设置宽松上限。
#[cfg(windows)]
const PUMP_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// `OpenClipboard` 的重试配置（v0.3.1 加固）。
///
/// 剪贴板是**系统级互斥资源**：其他进程（正在进行的拖放、延迟渲染、另一款
/// 剪贴板工具）持有期间 `OpenClipboard` 立即返回失败。并发锁争用属正常竞争
/// 而非故障——本模块以小步递增退避重试数次，对“剪贴板被瞬态占用”做到平滑
/// 降级，避免单次失败即放弃本次净化。
///
/// - 总尝试次数 **5**（初次 + 4 次重试，符合 3~5 次重试的加固要求）；
/// - 退避序列 15ms / 30ms / 60ms / 120ms，重试总等待上限 **225ms**——远小于
///   [`PUMP_JOIN_TIMEOUT`]，且绝大多数毫秒级瞬态争用在第一、二次重试内即让出；
/// - 等待期间泵线程不取消息（`WM_QUIT` 的响应至多延后 225ms），仍满足停机
///   协议的宽松超时窗口。
#[cfg(windows)]
const OPEN_CLIPBOARD_MAX_ATTEMPTS: usize = 5;

/// 各次重试的退避时长（毫秒）：下标 = 第几次重试（0 起）。
#[cfg(windows)]
const OPEN_CLIPBOARD_BACKOFF_MS: [u64; 4] = [15, 30, 60, 120];

/// 第 `retry_index` 次重试（0 起）应等待的退避时长；重试序列耗尽返回 `None`。
#[cfg(windows)]
fn clipboard_retry_backoff(retry_index: usize) -> Option<Duration> {
    OPEN_CLIPBOARD_BACKOFF_MS
        .get(retry_index)
        .map(|&ms| Duration::from_millis(ms))
}

/// 纯文本剪贴板格式 `CF_UNICODETEXT`（13）。
///
/// windows crate 0.58 把 `CF_*` 标准格式常量（`CLIPBOARD_FORMAT` 包装）放在
/// `Win32::System::Ole` 模块下——仅为读取一个固定 ABI 常量而启用整套 Ole 特性
/// 得不偿失。13 是 WinUser.h 明文规定的稳定值，此处直接以 `u32` 声明并注明出处。
#[cfg(windows)]
const CF_UNICODETEXT: u32 = 13;

/// 需要净化的剪贴板内容是否**同时**含有纯文本与任一富文本格式。
///
/// 纯函数（跨平台可测）：净化只发生在「复制自富文本来源」时——纯文本复制
/// （仅 CF_UNICODETEXT）、图片 / 文件复制（无 CF_UNICODETEXT）一律放行。
fn should_purge(has_unicode_text: bool, has_html: bool, has_rich_text: bool) -> bool {
    has_unicode_text && (has_html || has_rich_text)
}

/// 防自循环回声标记。
///
/// 语义：一次净化写回会在系统队列中产生紧随其后的 `WM_CLIPBOARDUPDATE` 回声。
/// 写回前先 [`arm_after_rewrite`](Self::arm_after_rewrite) 布防；泵循环每消费一条
/// 剪贴板更新先询问 [`on_clipboard_update`](Self::on_clipboard_update)——返回 `true`
/// 表示命中自我回声（跳过本次并解除布防），`false` 表示外部真实事件（正常处理）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EchoGuard {
    armed: bool,
}

impl EchoGuard {
    /// 构造一枚未布防的标记。
    const fn new() -> Self {
        Self { armed: false }
    }

    /// 收到一次剪贴板更新：若标记已布防，说明这是自己写回引发的回声——
    /// 消费（解除）标记并返回 `true` 指示调用方跳过本次处理。
    fn on_clipboard_update(&mut self) -> bool {
        if self.armed {
            self.armed = false;
            true
        } else {
            false
        }
    }

    /// 本次处理确认发生了写回 → 布防，使紧随其后的下一条更新被当作回声跳过。
    fn arm_after_rewrite(&mut self) {
        self.armed = true;
    }
}

/// 剪贴板纯文本净化模块。
///
/// 生命周期完全由内部可变性管理（`AtomicBool` 运行标志 + 异步锁串行化变迁），
/// 仅向调度器 / UI 暴露共享引用接口，天然满足 [`ToolModule`] 的 `Send + Sync` 契约。
#[derive(Clone)]
pub struct ClipboardPurifierModule {
    inner: Arc<ClipboardPurifierInner>,
}

/// 模块内部并发状态。
struct ClipboardPurifierInner {
    /// 串行化 `start` / `stop` 生命周期变迁，杜绝并发启停互相穿插。
    lifecycle: AsyncMutex<()>,
    /// 快速查询的运行标志（无锁读取路径，供 UI 高频轮询）。
    running: AtomicBool,
    /// 当前活动运行上下文（一次运行仅对应一条泵线程）。
    active: StdMutex<Option<ActiveRun>>,
}

/// 单次运行（泵线程）的运行时上下文。
struct ActiveRun {
    /// 停机广播令牌：`cancel()` 即请求退出。
    cancel: CancellationToken,
    #[cfg(windows)]
    /// 泵线程线程 ID（供 `PostThreadMessageW(WM_QUIT)` 定向投递）。
    thread_id: u32,
    #[cfg(windows)]
    /// 泵线程 Join 句柄（`stop()` 借此确认监听窗口已销毁、线程已退出）。
    thread: std::thread::JoinHandle<()>,
}

impl ClipboardPurifierModule {
    /// 构造一个尚未启动的剪贴板纯文本净化模块。
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ClipboardPurifierInner {
                lifecycle: AsyncMutex::new(()),
                running: AtomicBool::new(false),
                active: StdMutex::new(None),
            }),
        }
    }

    /// 执行一次剪贴板净化（Windows 泵线程内调用）。
    ///
    /// 返回 `true` 当且仅当本次确实发生了「清空 + 仅写回 CF_UNICODETEXT」的写回——
    /// 调用方（泵循环）据此布防 [`EchoGuard`]，吞掉紧随其后的自我回声。
    ///
    /// 流程严格持有剪贴板锁（`OpenClipboard` … `CloseClipboard`）：
    ///
    /// 1. 判定：需同时含 CF_UNICODETEXT 与任一注册富文本格式，否则原样放行；
    /// 2. 读取纯文本：`GetClipboardData` 取得只读句柄 → `GlobalLock` 建立只读视图 →
    ///    `GlobalSize` 界定读取边界，截断于首个 NUL（CF_UNICODETEXT 为 NUL 终结串）；
    ///    空文本不净化（避免把仅有富文本载荷的复制改写成“空白文本”造成内容损失）；
    /// 3. **先分配后清空**：以 `GMEM_MOVEABLE` 预分配新块并写入文本，任意分配失败
    ///    均发生在 `EmptyClipboard` 之前，原数据毫发无损；
    /// 4. `EmptyClipboard` 清空全部既有格式 → `SetClipboardData` 仅写回 CF_UNICODETEXT
    ///    （成功后句柄所有权移交系统；失败则 `GlobalFree` 自释后原样上报）。
    ///
    /// # Safety
    ///
    /// 仅允许在**持剪贴板打开权**的泵线程上下文内调用（本函数自行 Open / Close，
    /// 但调用方不得同时在别处持有剪贴板）；`GlobalLock` 返回指针只在解锁前按
    /// `GlobalSize` 界定的只读区间解引用，写入目标块由本函数独占分配、大小精确。
    #[cfg(windows)]
    unsafe fn purge_rich_text_once(
        listener: HWND,
        html_format: u32,
        rtf_format: u32,
        rtf_wo_format: u32,
    ) -> bool {
        // 1) 打开剪贴板（携带本监听窗口句柄）。失败 = 其他进程正持有剪贴板
        //    （正在进行的拖放 / 长渲染 / 其它剪贴板工具），属正常竞争：以递增
        //    退避重试至多 [`OPEN_CLIPBOARD_MAX_ATTEMPTS`] 次（初次 + 4 次重试）
        //    平滑降级；重试耗尽后记录日志并放弃本次，等待下一条更新事件。
        let mut open_error: Option<windows::core::Error> = None;
        for attempt in 0..OPEN_CLIPBOARD_MAX_ATTEMPTS {
            match OpenClipboard(listener) {
                Ok(()) => {
                    open_error = None;
                    break;
                }
                Err(err) => {
                    open_error = Some(err);
                    if let Some(backoff) = clipboard_retry_backoff(attempt) {
                        std::thread::sleep(backoff);
                    }
                }
            }
        }
        if let Some(err) = open_error {
            tracing::debug!(
                target: "clipboard_purifier",
                "OpenClipboard 在 {OPEN_CLIPBOARD_MAX_ATTEMPTS} 次尝试内未获锁（剪贴板被其他进程长期占用），放弃本次净化: {err}"
            );
            return false;
        }

        // 2) 判定是否值得净化（已持锁，判定结果与后续读写在同一次锁定内原子一致）。
        //    IsClipboardFormatAvailable 以 Result 表达可用性：Ok = 格式存在。
        let has_text = IsClipboardFormatAvailable(CF_UNICODETEXT).is_ok();
        let has_html = html_format != 0 && IsClipboardFormatAvailable(html_format).is_ok();
        let has_rtf = (rtf_format != 0 && IsClipboardFormatAvailable(rtf_format).is_ok())
            || (rtf_wo_format != 0 && IsClipboardFormatAvailable(rtf_wo_format).is_ok());
        if !should_purge(has_text, has_html, has_rtf) {
            let _ = CloseClipboard();
            return false;
        }

        // 3) 读取 CF_UNICODETEXT 纯文本。
        let data = match GetClipboardData(CF_UNICODETEXT) {
            Ok(data) => data,
            Err(err) => {
                tracing::debug!(target: "clipboard_purifier", "GetClipboardData(CF_UNICODETEXT) 失败: {err}");
                let _ = CloseClipboard();
                return false;
            }
        };
        // 剪贴板返回的 HANDLE 按文档即 HGLOBAL（系统共享内存块）：读取方不得释放。
        let global = HGLOBAL(data.0);
        let locked = GlobalLock(global);
        if locked.is_null() {
            tracing::debug!(target: "clipboard_purifier", "GlobalLock 失败：无法读取剪贴板文本");
            let _ = CloseClipboard();
            return false;
        }
        // 以 GlobalSize（字节）界定只读视界，截断于首个 NUL：CF_UNICODETEXT 是
        // NUL 终结的 UTF-16 串，串尾之外的填充字节一律不进入净化结果。
        let size_bytes = GlobalSize(global);
        let max_units = size_bytes / 2;
        let view = std::slice::from_raw_parts(locked.cast::<u16>(), max_units);
        let end = view.iter().position(|&unit| unit == 0).unwrap_or(max_units);
        let text: Vec<u16> = view[..end].to_vec();
        let _ = GlobalUnlock(global);

        if text.is_empty() {
            // 空纯文本：不净化。避免把「仅携带富文本/图片载荷、无实际文字」的复制
            // 改写成空剪贴板造成不可恢复的内容损失。
            let _ = CloseClipboard();
            return false;
        }

        // 4) 预分配新块并写入文本（在 EmptyClipboard 之前完成：分配失败不影响原数据）。
        //    v0.6.2（L14）：改用 `checked_mul().ok_or_else(..)` ——旧实现 `.expect(..)`
        //    位于泵循环调用链上，溢出 panic 会让泵线程静默死亡（`running` 仍为真，
        //    UI 显示运行中却不再净化）。溢出按"放弃本次净化"处理，语义等同分配失败。
        let byte_len = match (text.len() + 1).checked_mul(2) {
            Some(len) => len,
            None => {
                tracing::warn!(
                    target: "clipboard_purifier",
                    "剪贴板文本长度溢出 usize（len={}），放弃本次净化",
                    text.len()
                );
                let _ = CloseClipboard();
                return false;
            }
        };
        let new_global = match GlobalAlloc(GMEM_MOVEABLE, byte_len) {
            Ok(block) => block,
            Err(err) => {
                tracing::warn!(target: "clipboard_purifier", "GlobalAlloc 分配净化文本块失败: {err}");
                let _ = CloseClipboard();
                return false;
            }
        };
        let new_locked = GlobalLock(new_global);
        if new_locked.is_null() {
            tracing::warn!(target: "clipboard_purifier", "GlobalLock 锁定新文本块失败");
            let _ = GlobalFree(new_global);
            let _ = CloseClipboard();
            return false;
        }
        let dest = new_locked.cast::<u16>();
        std::ptr::copy_nonoverlapping(text.as_ptr(), dest, text.len());
        dest.add(text.len()).write(0u16); // NUL 终结
        let _ = GlobalUnlock(new_global);

        // 5) 清空全部既有格式（剥离 CF_HTML / RTF / 位图等），仅写回 CF_UNICODETEXT。
        if EmptyClipboard().is_err() {
            tracing::warn!(target: "clipboard_purifier", "EmptyClipboard 失败：放弃本次净化");
            let _ = GlobalFree(new_global);
            let _ = CloseClipboard();
            return false;
        }
        if SetClipboardData(CF_UNICODETEXT, HANDLE(new_global.0)).is_err() {
            // SetClipboardData 失败 = 系统未接管该块，所有权仍在己方 → 必须自释。
            tracing::warn!(target: "clipboard_purifier", "SetClipboardData 写回纯文本失败");
            let _ = GlobalFree(new_global);
            let _ = CloseClipboard();
            return false;
        }
        let _ = CloseClipboard();

        // 6) 记录净化结果（仅日志：长度 + 截断预览）。
        let unit_len = text.len();
        let preview_units = text.len().min(40);
        let preview = String::from_utf16_lossy(&text[..preview_units]);
        let truncated = text.len() > preview_units;
        tracing::info!(
            target: "clipboard_purifier",
            "已剥离富文本格式（CF_HTML/RTF 等），剪贴板仅保留纯文本（{} 个 UTF-16 码元）: \"{preview}{}\"",
            unit_len,
            if truncated { "…" } else { "" }
        );
        true
    }

    /// 泵线程主体：运行于专用操作系统原生线程，承载纯消息窗口的创建与 Win32 消息泵。
    ///
    /// 所有净化相关状态（[`EchoGuard`] 标记、注册格式 ID）都是本函数栈上局部变量，
    /// 无需任何进程级注册表——`WM_CLIPBOARDUPDATE` 在 `GetMessageW` 处被直接拦截，
    /// 净化逻辑内联执行，从不派发消息（`STATIC` 类窗口过程无需参与）。
    ///
    /// 线程退出前必须在同线程完成 `RemoveClipboardFormatListener` + `DestroyWindow`。
    #[cfg(windows)]
    fn pump_thread_main(ready_tx: oneshot::Sender<Result<u32, String>>) {
        // SAFETY:
        // - 本函数整体运行在由 `std::thread::Builder::spawn` 派生的专用原生线程中；
        // - `PeekMessageW` 仅用于建立本线程消息队列（取不到消息也无副作用）；
        // - `CreateWindowExW` 以系统预定义类 STATIC + HWND_MESSAGE 父句柄创建纯消息
        //   窗口：系统类已由 user32 预注册，hInstance 传 NULL 合法，无需 RegisterClassW；
        // - `AddClipboardFormatListener` 把该窗口加入系统剪贴板格式监听列表，系统此后
        //   以投递（posted）方式向本线程队列发送 WM_CLIPBOARDUPDATE，由 GetMessageW
        //   直接取出拦截，无需 DispatchMessageW / 自定义窗口过程；
        // - 退出路径在同线程注销监听并销毁窗口，满足窗口创建/销毁的线程亲和约束。
        unsafe {
            // 1) 强制建立本线程消息队列。
            //    `PostThreadMessageW` 只接受“已建队列”的线程；先建队再回报线程 ID，
            //    从根上消除“WM_QUIT 早于队列建立而投递失败”的竞态。
            let mut seed = MSG::default();
            let _ = PeekMessageW(&mut seed, None, 0, 0, PM_NOREMOVE);

            // 2) 创建纯消息窗口（系统预定义 STATIC 类承载，父句柄 HWND_MESSAGE）。
            let listener = match CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("TLToolBoxClipboardPurifierWindow"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                None, // hMenu：消息窗口无菜单
                None, // hInstance：系统预定义类允许 NULL
                None, // lpParam
            ) {
                Ok(hwnd) => hwnd,
                Err(err) => {
                    let _ = ready_tx.send(Err(format!(
                        "CreateWindowExW 创建纯消息窗口失败（可能处于无桌面会话）: {err}"
                    )));
                    return;
                }
            };

            // 3) 注册为系统剪贴板格式监听者。失败则回收窗口后上报。
            if let Err(err) = AddClipboardFormatListener(listener) {
                let _ = DestroyWindow(listener);
                let _ = ready_tx.send(Err(format!(
                    "AddClipboardFormatListener 注册剪贴板监听失败: {err}"
                )));
                return;
            }

            // 4) 解析注册富文本格式 ID（返回 0 = 注册失败，仅该格式失去探测能力，
            //    不阻断整体运行）。CF_HTML / RTF 不是预定义常量，需经
            //    RegisterClipboardFormatW 取回各会话内的稳定注册值。
            //    v0.6.2（L15）：注册失败补一条 `warn`——旧实现静默吞掉，用户只会
            //    感到"富文本复制没被净化"，无从排查。
            let html_format = RegisterClipboardFormatW(w!("HTML Format"));
            let rtf_format = RegisterClipboardFormatW(w!("Rich Text Format"));
            let rtf_wo_format = RegisterClipboardFormatW(w!("Rich Text Format Without Objects"));
            for (name, id) in [
                ("HTML Format", html_format),
                ("Rich Text Format", rtf_format),
                ("Rich Text Format Without Objects", rtf_wo_format),
            ] {
                if id == 0 {
                    tracing::warn!(
                        target: "clipboard_purifier",
                        "注册剪贴板格式 '{name}' 失败（返回 0）：该格式的探测能力不可用，净化仍对纯文本生效"
                    );
                }
            }

            let thread_id = GetCurrentThreadId();

            // 5) 与父侧握手。若父侧已放弃等待（oneshot 关闭 / future 被取消），
            //    立即注销监听并销毁窗口后退出，绝不遗留无主监听窗口。
            if ready_tx.send(Ok(thread_id)).is_err() {
                let _ = RemoveClipboardFormatListener(listener);
                let _ = DestroyWindow(listener);
                return;
            }

            tracing::info!(
                target: "clipboard_purifier",
                "剪贴板格式监听已就绪（纯消息窗口 HWND=0x{:X}，泵线程 {thread_id}），进入消息循环",
                listener.0 as usize
            );

            // 6) Win32 消息泵：拦截 WM_CLIPBOARDUPDATE 就地净化；GetMessageW 取到
            //    WM_QUIT 时返回 FALSE（0），循环退出。
            //    防自循环：写回成功即布防 EchoGuard，紧随其后的自我回声被跳过。
            let mut guard = EchoGuard::new();
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                match msg.message {
                    WM_CLIPBOARDUPDATE => {
                        if guard.on_clipboard_update() {
                            // 自我回声：本次更新正是我们写回纯文本引发的，直接放行。
                            continue;
                        }
                        if Self::purge_rich_text_once(
                            listener,
                            html_format,
                            rtf_format,
                            rtf_wo_format,
                        ) {
                            // 确实发生了写回 → 布防，吞掉紧随其后的系统回声。
                            guard.arm_after_rewrite();
                        }
                    }
                    WM_QUIT => break, // 防御：正常 WM_QUIT 已令 GetMessageW 返回 FALSE
                    _ => {
                        // 本线程队列中理论不存在其他消息（纯消息窗口不可见、不派发、
                        // 无人向它投递业务消息）；即便出现也无需处理，直接丢弃。
                    }
                }
            }

            // 7) 泵退出后在同一线程注销监听并销毁窗口，随后线程自然结束。
            //    顺序不可颠倒：先注销再销毁——监听列表在窗口销毁前显式清理。
            let _ = RemoveClipboardFormatListener(listener);
            let _ = DestroyWindow(listener);
            tracing::info!(target: "clipboard_purifier", "剪贴板格式监听已注销，泵线程退出");
        }
    }

    /// Windows 实现：派生专用原生线程运行剪贴板监听消息泵。
    #[cfg(windows)]
    async fn start_native(&self) -> Result<(), ModuleError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        if self.inner.running.load(Ordering::Acquire) {
            return Ok(()); // 已在运行：幂等
        }

        let cancel = CancellationToken::new();
        let (ready_tx, ready_rx) = oneshot::channel::<Result<u32, String>>();

        // 派生专用操作系统原生线程承载监听窗口与消息泵（严禁放置于 Tokio 协程中）。
        let thread = std::thread::Builder::new()
            .name("win32-clipboard-purifier-pump".to_string())
            .spawn(move || Self::pump_thread_main(ready_tx))
            .map_err(|e| -> ModuleError { Box::new(e) })?;

        // 等待原生线程完成“建队列 + 建窗 + 注册监听”握手，失败则回收线程并上报。
        let thread_id = match ready_rx.await {
            Ok(Ok(thread_id)) => thread_id,
            Ok(Err(reason)) => {
                // 线程已自行退出；Join 仅回收句柄，随后向调用方暴露失败原因。
                let _ = thread.join();
                return Err(reason.into());
            }
            Err(_) => {
                // 通道关闭：泵线程检测到父侧放弃后已自行清理退出，此处回收句柄。
                let _ = thread.join();
                return Err("剪贴板净化模块启动握手被中断".into());
            }
        };

        self.inner.running.store(true, Ordering::Release);
        *self.active_lock()? = Some(ActiveRun {
            cancel,
            thread_id,
            thread,
        });

        tracing::info!(target: "clipboard_purifier", "剪贴板纯文本净化模块已启动（泵线程 {thread_id}）");
        Ok(())
    }

    /// 非 Windows 兜底实现：仅维持虚拟生命周期，供跨平台编译与调度联调。
    #[cfg(not(windows))]
    async fn start_virtual(&self) -> Result<(), ModuleError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        if self.inner.running.load(Ordering::Acquire) {
            return Ok(()); // 已在运行：幂等
        }
        self.inner.running.store(true, Ordering::Release);
        tracing::warn!(
            target: "clipboard_purifier",
            "当前系统非 Windows 平台：剪贴板纯文本净化模块仅维持虚拟生命周期"
        );
        Ok(())
    }

    /// 取得活动上下文锁（将“锁中毒”这类异常状态显式上报为模块错误）。
    fn active_lock(&self) -> Result<std::sync::MutexGuard<'_, Option<ActiveRun>>, ModuleError> {
        self.inner
            .active
            .lock()
            .map_err(|_| -> ModuleError { "clipboard_purifier: 活动上下文锁中毒".into() })
    }

    /// 停止模块的公共实现（Windows / 非 Windows 共用，内部以 cfg 区分）。
    async fn stop_impl(&self) -> Result<(), ModuleError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        if !self.inner.running.load(Ordering::Acquire) {
            return Ok(()); // 未在运行：幂等
        }

        let run = self.active_lock()?.take();
        let Some(run) = run else {
            // running 与 active 不一致（理论不可达）：保守复位后返回。
            self.inner.running.store(false, Ordering::Release);
            return Ok(());
        };

        // 1) 广播停机意图（Drop 兜底路径与未来的外部取消均复用该令牌）。
        run.cancel.cancel();

        #[cfg(windows)]
        {
            // 2) 向泵线程消息队列定向投递 WM_QUIT，唤醒阻塞中的 GetMessageW。
            //    队列在握手阶段已建立，此处投递必然成功。
            // SAFETY: WM_QUIT 为已定义消息常量，参数类型匹配；投递失败仅返回错误码，无 UB。
            let _ = unsafe { PostThreadMessageW(run.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };

            // 3) Join 泵线程并施加超时：确保 RemoveClipboardFormatListener 与
            //    DestroyWindow 已在创建线程上执行完毕。
            //    三层 Result 展开：timeout → spawn_blocking JoinHandle → 线程 join。
            let join_result = tokio::time::timeout(
                PUMP_JOIN_TIMEOUT,
                tokio::task::spawn_blocking(move || run.thread.join()),
            )
            .await;

            match join_result {
                Ok(Ok(Ok(()))) => {
                    tracing::debug!(target: "clipboard_purifier", "泵线程已退出，监听窗口已销毁");
                }
                Ok(Ok(Err(panic))) => {
                    // std 线程 join 的 panic 载荷为 Box<dyn Any + Send>，需 downcast 取可读文本。
                    let payload = panic
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "未知 panic 载荷".to_string());
                    tracing::error!(target: "clipboard_purifier", "泵线程异常终止: {payload}");
                    self.inner.running.store(false, Ordering::Release);
                    return Err(format!("剪贴板净化泵线程异常终止: {payload}").into());
                }
                Ok(Err(task_err)) => {
                    tracing::error!(target: "clipboard_purifier", "Join 阻塞任务异常终止: {task_err}");
                    self.inner.running.store(false, Ordering::Release);
                    return Err(format!("剪贴板净化 Join 阻塞任务异常终止: {task_err}").into());
                }
                Err(_elapsed) => {
                    // 极端场景（GetClipboardData 正等待延迟渲染提供方应答的系统窗口内）：
                    // 泵线程转入分离式收尾——WM_QUIT 已投递，监听注销最终仍会在该线程执行。
                    tracing::error!(
                        target: "clipboard_purifier",
                        "泵线程未在 {PUMP_JOIN_TIMEOUT:?} 内退出，已转入分离式收尾"
                    );
                }
            }
        }

        self.inner.running.store(false, Ordering::Release);
        Ok(())
    }
}

impl Default for ClipboardPurifierModule {
    fn default() -> Self {
        Self::new()
    }
}

/// 兜底清理：即使调用方遗忘 `stop()`，实例消亡时也要唤醒泵线程，确保
/// 监听注销与窗口销毁最终在创建线程上执行（JoinHandle 随 `run` 析构而分离，
/// 不阻塞当前线程）。
impl Drop for ClipboardPurifierInner {
    fn drop(&mut self) {
        let run = match self.active.try_lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => return, // 极端的并发消亡场景：放弃兜底，交由进程退出统一回收。
        };
        let Some(run) = run else { return };

        run.cancel.cancel();
        #[cfg(windows)]
        unsafe {
            let _ = PostThreadMessageW(run.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        // `run` 在此析构：Windows 平台下 JoinHandle 被丢弃 = 线程分离，
        // 泵线程退出 GetMessageW 循环后自行注销监听并销毁窗口。
    }
}

#[async_trait]
impl ToolModule for ClipboardPurifierModule {
    fn id(&self) -> &'static str {
        "clipboard_purifier"
    }

    fn display_name(&self) -> &'static str {
        "剪贴板纯文本净化"
    }

    fn description(&self) -> &'static str {
        "监听系统剪贴板，自动剥离 HTML 与富文本格式，仅保留纯文本"
    }

    async fn start(&self) -> Result<(), ModuleError> {
        #[cfg(windows)]
        {
            self.start_native().await
        }
        #[cfg(not(windows))]
        {
            self.start_virtual().await
        }
    }

    async fn stop(&self) -> Result<(), ModuleError> {
        self.stop_impl().await
    }

    fn is_running(&self) -> bool {
        self.inner.running.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 元数据契约：注册表键 / 展示名 / 描述必须与装配层与 UI 的约定一致。
    #[test]
    fn metadata_identity_matches_registry_contract() {
        let module = ClipboardPurifierModule::new();
        assert_eq!(module.id(), "clipboard_purifier");
        assert_eq!(module.display_name(), "剪贴板纯文本净化");
        assert_eq!(
            module.description(),
            "监听系统剪贴板，自动剥离 HTML 与富文本格式，仅保留纯文本"
        );
        assert!(!module.is_running(), "新模块应处于停止态");
    }

    /// 生命周期幂等性验证：重复 `start` / `stop` 不得产生窗口 / 线程泄漏，
    /// 状态机多轮收敛且无错乱。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_start_stop_is_idempotent_and_converges() {
        let module = ClipboardPurifierModule::new();
        assert!(!module.is_running());

        for cycle in 1..=3 {
            module.start().await.expect("启动应成功");
            assert!(module.is_running(), "第 {cycle} 轮启动后应处于运行态");

            module.start().await.expect("重复启动应幂等成功");
            assert!(module.is_running(), "重复启动不得破坏运行态");

            module.stop().await.expect("停止应成功");
            assert!(!module.is_running(), "第 {cycle} 轮停止后应退出运行态");

            module.stop().await.expect("重复停止应幂等成功");
            assert!(!module.is_running(), "重复停止不得破坏停止态");
        }
    }

    /// 状态流转验证：新建 → 启动 →（重复启动）→ 停止 →（重复停止）→ 再启动，
    /// 每一步后 `is_running` 与目标状态严格一致。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn state_transitions_follow_start_stop_semantics() {
        let module = ClipboardPurifierModule::new();

        // 停止态下 stop 为无操作（幂等），状态保持不变。
        module.stop().await.expect("停止态下重复停止应幂等成功");
        assert!(!module.is_running());

        // 启动 → 运行态。
        module.start().await.expect("首次启动应成功");
        assert!(module.is_running());

        // 停止 → 停止态。
        module.stop().await.expect("停止应成功");
        assert!(!module.is_running());

        // 再次启动 → 再次运行态（模块可反复启停）。
        module.start().await.expect("再次启动应成功");
        assert!(module.is_running());

        // 收尾停止。
        module.stop().await.expect("收尾停止应成功");
        assert!(!module.is_running());
    }

    /// 并发调度验证：多个任务同时发起 start / stop，生命周期锁应保证串行收敛，
    /// 全程只存在一条泵线程，最终状态一致且无死锁。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_lifecycle_operations_are_serialized() {
        let module = ClipboardPurifierModule::new();

        let mut starters = Vec::new();
        for _ in 0..4 {
            let m = module.clone();
            starters.push(tokio::spawn(async move { m.start().await }));
        }
        for handle in starters {
            handle
                .await
                .expect("并发 start 任务应正常结束")
                .expect("并发 start 应成功");
        }
        assert!(module.is_running(), "并发启动后应收敛到运行态");

        let mut stoppers = Vec::new();
        for _ in 0..4 {
            let m = module.clone();
            stoppers.push(tokio::spawn(async move { m.stop().await }));
        }
        for handle in stoppers {
            handle
                .await
                .expect("并发 stop 任务应正常结束")
                .expect("并发 stop 应成功");
        }
        assert!(!module.is_running(), "并发停止后应收敛到停止态");
    }

    /// §6.2 #7（v0.6.2 · P2-15）：**反复启停压测**——50 轮 start / stop，每轮都
    /// 断言 `is_running()` 与真实资源状态一致。长驻工具的模块生命周期必须经得起
    /// 用户长时间反复开关：状态漂移、句柄泄漏（泵线程残留）都会在此显形。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repeated_start_stop_cycles_converge() {
        let module = ClipboardPurifierModule::new();
        for cycle in 0..50 {
            module
                .start()
                .await
                .unwrap_or_else(|err| panic!("第 {cycle} 轮 start 应成功: {err}"));
            assert!(module.is_running(), "第 {cycle} 轮启动后应为运行态");
            module
                .stop()
                .await
                .unwrap_or_else(|err| panic!("第 {cycle} 轮 stop 应成功: {err}"));
            assert!(!module.is_running(), "第 {cycle} 轮停止后应为停止态");
        }
    }

    // -----------------------------------------------------------------------
    // 净化判定与防自循环核心（跨平台纯逻辑，不依赖 Win32 剪贴板）
    // -----------------------------------------------------------------------

    /// 判定矩阵：只有“纯文本 + 至少一种富文本格式”同时存在才触发净化；
    /// 纯文本复制、无文本复制、图片 / 文件复制一律放行。
    #[test]
    fn should_purge_requires_text_plus_rich_format() {
        // 富文本来源：HTML / RTF / 两者齐备，均应净化。
        assert!(
            should_purge(true, true, false),
            "CF_UNICODETEXT + CF_HTML 应净化"
        );
        assert!(
            should_purge(true, false, true),
            "CF_UNICODETEXT + RTF 应净化"
        );
        assert!(should_purge(true, true, true), "全部格式齐备应净化");
        // 纯文本复制：无富文本格式 → 放行。
        assert!(!should_purge(true, false, false), "仅纯文本不得净化");
        // 无纯文本载荷：图片 / 文件 / 仅富文本复制 → 放行（净化无可写回的文本）。
        assert!(!should_purge(false, true, false), "仅 HTML 无文本不得净化");
        assert!(!should_purge(false, false, true), "仅 RTF 无文本不得净化");
        assert!(!should_purge(false, false, false), "空剪贴板不得净化");
    }

    /// [`EchoGuard`] 语义：未布防时更新一律放行；布防后仅**紧随其后的一条**更新
    /// 被判定为自我回声（跳过并解除布防），更后续的更新恢复放行。
    #[test]
    fn echo_guard_skips_exactly_the_next_update_after_arming() {
        let mut guard = EchoGuard::new();

        // 未布防：外部事件 → 放行（返回 false = 应正常处理）。
        assert!(!guard.on_clipboard_update());
        assert!(!guard.on_clipboard_update());

        // 写回成功 → 布防。
        guard.arm_after_rewrite();
        assert!(guard.armed);

        // 紧随其后的一条更新是自我回声 → 跳过并解除布防。
        assert!(
            guard.on_clipboard_update(),
            "布防后的下一条更新应被当作回声跳过"
        );
        assert!(!guard.armed, "回声被消费后标记应解除");

        // 解除后再布防 → 重新生效（模块可反复启停净化）。
        guard.arm_after_rewrite();
        assert!(guard.on_clipboard_update());
        assert!(!guard.on_clipboard_update(), "连续更新只应跳过一条");
    }

    /// 未触发净化（写回未发生）时不得布防：仅“真实写回”才产生回声。
    #[test]
    fn echo_guard_arms_only_after_actual_rewrite() {
        let mut guard = EchoGuard::new();
        // 模拟一条无需净化的更新（如纯文本复制）：不调用 arm_after_rewrite。
        assert!(!guard.on_clipboard_update());
        assert!(!guard.armed, "无写回则不得布防");
        // 后续外部事件仍正常放行，不产生误跳过。
        assert!(!guard.on_clipboard_update());
    }

    // -----------------------------------------------------------------------
    // OpenClipboard 重试退避（v0.3.1 加固）
    // -----------------------------------------------------------------------

    /// 退避序列契约：前四次重试依次 15 / 30 / 60 / 120ms，序列耗尽后返回
    /// `None`——配合 [`OPEN_CLIPBOARD_MAX_ATTEMPTS`]（5 次尝试）保证重试
    /// 总等待上限 225ms，且绝不无限重试。
    #[cfg(windows)]
    #[test]
    fn open_clipboard_retry_backoff_follows_sequence_then_exhausts() {
        assert_eq!(
            clipboard_retry_backoff(0),
            Some(Duration::from_millis(15)),
            "第 1 次重试退避 15ms"
        );
        assert_eq!(clipboard_retry_backoff(1), Some(Duration::from_millis(30)));
        assert_eq!(clipboard_retry_backoff(2), Some(Duration::from_millis(60)));
        assert_eq!(clipboard_retry_backoff(3), Some(Duration::from_millis(120)));
        assert_eq!(
            clipboard_retry_backoff(4),
            None,
            "重试序列耗尽（第 5 次尝试后）不再退避"
        );
        assert_eq!(clipboard_retry_backoff(usize::MAX), None);
    }
}
