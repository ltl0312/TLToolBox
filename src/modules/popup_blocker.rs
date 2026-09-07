//! # 生产级 Win32 弹窗拦截模块
//!
//! ## 与 Tokio 的隔离（本模块的第一性原则）
//!
//! 使用 `WINEVENT_OUTOFCONTEXT` 跨进程监听系统级 UI 事件时，操作系统会把事件路由到
//! **安装钩子的线程所拥有的消息队列**，并由该线程运行的 Win32 消息泵
//! （`GetMessageW` / `DispatchMessageW`）派发回调。Tokio 工作线程是无栈协程调度载体，
//! 本身不运行标准 Win32 消息循环，因此在 Tokio 协程中直接挂接 / 轮询 WinEvent 回调
//! **永远不会被触发**。
//!
//! 本模块因此将 `SetWinEventHook` 与消息泵整体隔离到一条由 `std::thread` 派生的
//! **专用操作系统原生线程**（命名 `win32-popup-hook-pump`）中运行；Tokio 侧只负责
//! 生命周期编排（启停请求的收发与结果确认），绝不触碰 Win32 事件监听。
//!
//! ## 平滑卸载协议
//!
//! 卸载链路必须保证 `UnhookWinEvent`（要求与 `SetWinEventHook` 处于同一线程）一定执行，
//! 杜绝系统级事件钩子与消息泵线程泄漏：
//!
//! 1. `stop()` 先 `cancel()` [`CancellationToken`] 广播停机意图；
//! 2. 再向泵线程定向投递 `PostThreadMessageW(WM_QUIT)`，唤醒阻塞在 `GetMessageW`
//!    中的泵线程（退出码 0，循环终止）；
//! 3. 泵线程退出循环后**在同一线程上**调用 `UnhookWinEvent`，随后线程自然结束；
//! 4. `stop()` 通过 Join 该原生线程并施加超时，确保卸载动作**已经发生**才向调用方返回。
//!
//! ## 竞态防护
//!
//! `PostThreadMessageW` 要求目标线程**已经建立消息队列**，否则调用失败导致停机信号
//! 丢失、泵线程永久挂起。为此泵线程在回报自身线程 ID 之前，先以
//! `PeekMessageW(PM_NOREMOVE)` 强制建立队列，再与父侧完成一次性握手；握手成功后队列
//! 必然存在，任何后续投递的 `WM_QUIT` 都不会丢失。
//!
//! ## 动态黑名单与热更新（阶段三：配置驱动的规则存储）
//!
//! 黑名单关键词不再硬编码于本模块，而是以 [`crate::config::AppConfig::popup_blacklist`]
//! 为唯一事实源，由装配层在注册时经 [`PopupBlockerModule::with_rules`] 注入；运行期亦可
//! 调用 [`PopupBlockerModule::update_rules`] **热更新**——不需要重启原生消息泵线程，
//! 因为回调并非读取某个启动期常量，而是每条事件现场读取所属实例的当前规则快照。
//!
//! 由于 `SetWinEventHook` 的回调是 `unsafe extern "system" fn`（与实例无关联的静态
//! 函数，无法捕获 `&self`），本模块通过一张 **进程级钩子句柄注册表**
//! （`OnceLock<RwLock<HashMap<hook 句柄, Arc<HookRouting>>>>`，仅在 Windows 平台存在）
//! 把回调收到的 `HWINEVENTHOOK` 路由回它所属的 [`PopupBlockerModule`] 实例——不同实例
//! 各自持有独立的规则存储与截图留痕状态，互不串扰。泵线程在装钩成功后注册、卸载钩子前注销。
//!
//! 并发模型遵循两条铁律：
//! - **规则存储为 copy-on-write**：[`RuleStore`] 内部是 `std::sync::RwLock<Arc<RuleSet>>`，
//!   `update_rules` 以写锁**整体替换**不可变快照（快照生成时一次性完成大小写归一与
//!   去重，杜绝回调内逐条重复归一）；读取方只短暂持读锁克隆 `Arc`，随即在**锁外**
//!   完成全部字符串匹配；
//! - **回调内绝不持有重锁**：`win_event_proc` 在锁内只做「注册表查表 + 克隆 `Arc`」
//!   两个指针级操作，匹配与窗口查询均在无锁路径上执行，泵线程 / 桌面 UI 不会因
//!   规则更新或并发读取而挂起（更新与查表互斥窗口为微秒级）。
//!
//! ## 弹窗拦截截图留痕（v0.3.2）
//!
//! 命中黑名单的弹窗在 `PostMessageW(WM_CLOSE)` 关闭**之前**，把窗口画面截取为 PNG
//! 留存至配置的截图目录（[`crate::config::AppConfig::popup_screenshot_dir`]，缺省
//! exe 同级 `logs/popup_screenshots`），供用户回看“最近拦截了什么”。
//!
//! 线程拓扑（与 WinEvent 回调的第一性原则一致——回调内绝不执行重量级 / 阻塞操作）：
//!
//! 1. **回调侧只投递**：`win_event_proc` 命中后经无界通道把 `(hwnd, 标题, 类名)`
//!    交给**专用截图 worker 线程**（`popup-screenshot-worker`）即返回，随后立即
//!    投递 `WM_CLOSE`——截图耗时（`PrintWindow` 同步渲染 + GDI+ 编码落盘）绝不
//!    阻塞钩子消息泵；
//! 2. **worker 侧串行落盘**：worker 逐条执行
//!    [`crate::platform::capture_window_png`]（`PrintWindow` + GDI+ 原生编码，
//!    零第三方图像库），成功后把留痕记录（文件名 / 标题 / 类名 / 时间 / 绝对路径）
//!    压入**进程内环形记录表**（上限 [`MAX_CAPTURE_RECORDS`] 条，新覆盖旧）并
//!    自增 [`watch`] 版本号——UI 订阅该版本号即可在弹窗开着时实时刷新留痕列表；
//! 3. **停机**：`stop()` 关闭通道（worker 的 `blocking_recv` 返回 `None` 退出）
//!    并按 [`PUMP_JOIN_TIMEOUT`] 超时 Join，保证截图线程与钩子线程都不泄漏。
//!
//! 截图失败（窗口已销毁 / GDI+ 不可用等）仅记录告警，**绝不**影响 `WM_CLOSE`
//! 拦截动作本身（截图是拦截的附属留痕，不是前置条件）。

use super::{ModuleError, ToolModule};
use async_trait::async_trait;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError, RwLock as StdRwLock};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[cfg(windows)]
use std::collections::HashMap;
#[cfg(windows)]
use std::sync::OnceLock;
#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
use tokio::sync::mpsc as tokio_mpsc;
#[cfg(windows)]
use tokio::sync::oneshot;

#[cfg(windows)]
use windows::Win32::{
    Foundation::{HMODULE, HWND, LPARAM, WPARAM},
    UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK},
    UI::WindowsAndMessaging::{
        DispatchMessageW, GetClassNameW, GetMessageW, GetWindowTextW, PeekMessageW, PostMessageW,
        PostThreadMessageW, TranslateMessage, EVENT_OBJECT_CREATE, MSG, PM_NOREMOVE,
        WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_CLOSE, WM_QUIT,
    },
};

#[cfg(windows)]
use windows::Win32::System::Threading::GetCurrentThreadId;

/// 停机协议中 Join 原生泵线程的等待上限。
///
/// 收到 `WM_QUIT` 后泵线程应立即退出；仅在极端场景（回调正阻塞于跨进程取窗口文本的
/// 系统内部超时窗口）下可能略慢，故设置宽松上限。
#[cfg(windows)]
const PUMP_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// 动态黑名单规则存储（跨平台核心：Windows 钩子回调与单元测试共用同一判定逻辑）
// ---------------------------------------------------------------------------

/// 单条黑名单关键词的“已编译”形态。
#[derive(Debug, Clone, PartialEq, Eq)]
struct CompiledPattern {
    /// 归一化后的原始关键词（去首尾空白；保留原大小写，供日志与 `current_rules` 回读）。
    raw: String,
    /// 小写归一后的匹配针：判定时与同样小写化的窗口标题 / 类名做子串匹配。
    needle: String,
}

/// 不可变黑名单快照。
///
/// 快照在写入（[`RuleStore::set`]）时**一次性编译**完成（归一化 + 去重 + 小写化），
/// 之后为纯只读共享，多个消息泵线程 / 回调可无锁并发执行 [`RuleSet::matches`]。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RuleSet {
    patterns: Vec<CompiledPattern>,
}

impl RuleSet {
    /// 从原始关键词列表编译快照：
    /// 1) 逐条去首尾空白，空白串直接剔除；
    /// 2) 按**小写归一针**去重（同一关键词的大小写变体视作同一条目，保留首现形态）。
    fn compile(raw_rules: impl IntoIterator<Item = String>) -> Self {
        let mut seen = std::collections::HashSet::new();
        let patterns = raw_rules
            .into_iter()
            .filter_map(|rule| {
                let raw = rule.trim().to_string();
                if raw.is_empty() {
                    return None; // 空白条目无匹配意义
                }
                let needle = raw.to_lowercase();
                if !seen.insert(needle.clone()) {
                    return None; // 与既有条目同义（大小写不敏感去重）
                }
                Some(CompiledPattern { raw, needle })
            })
            .collect();
        Self { patterns }
    }

    /// 是否不含任何关键词（空黑名单 = 不拦截任何窗口；匹配路径零分配短路）。
    fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// 回读当前关键词（归一化后的原始文本，顺序与去重后一致）。
    fn raw_keywords(&self) -> Vec<String> {
        self.patterns
            .iter()
            .map(|pattern| pattern.raw.clone())
            .collect()
    }

    /// 判定窗口标题 / 类名是否命中任意黑名单关键词。
    ///
    /// 匹配语义：**子串匹配 + 忽略大小写**（Unicode 小写归一；中文等无大小写之分的
    /// 字符不受影响，仍为逐字子串匹配）。标题与类名各自小写化**一次**后统一比对，
    /// 避免按关键词逐条重复归一化。
    fn matches(&self, title: &str, class_name: &str) -> bool {
        if self.is_empty() {
            return false;
        }
        let title = title.to_lowercase();
        let class_name = class_name.to_lowercase();
        self.patterns
            .iter()
            .any(|pattern| title.contains(&pattern.needle) || class_name.contains(&pattern.needle))
    }
}

/// 实例级动态规则存储（copy-on-write 并发模型）。
///
/// - **读路径**（钩子回调 / 单元测试）：[`RuleStore::snapshot`] 仅在极短读锁内克隆
///   `Arc<RuleSet>` 即释放锁，全部字符串匹配都在**锁外**完成——回调绝不持锁扫描；
/// - **写路径**（[`RuleStore::set`]，即 `update_rules`）：以写锁**整体替换**快照，
///   发布-订阅语义。正在运行的原生消息泵线程无需任何干预，下一条事件即按新规则
///   判定，热更新即时生效。
#[derive(Debug)]
struct RuleStore {
    current: StdRwLock<Arc<RuleSet>>,
}

impl RuleStore {
    fn new(rules: impl IntoIterator<Item = String>) -> Self {
        Self {
            current: StdRwLock::new(Arc::new(RuleSet::compile(rules))),
        }
    }

    /// 整体替换规则快照（热更新入口）。
    fn set(&self, rules: impl IntoIterator<Item = String>) {
        let compiled = Arc::new(RuleSet::compile(rules));
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = compiled;
    }

    /// 取当前快照（读锁仅覆盖一次 `Arc` 克隆）。
    fn snapshot(&self) -> Arc<RuleSet> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[cfg(windows)]
/// 进程级钩子句柄路由：`HWINEVENTHOOK 原始指针值 -> 所属实例的路由信息`。
///
/// `SetWinEventHook` 回调是静态 `unsafe extern "system" fn`，系统只回传
/// `HWINEVENTHOOK` 等载荷、不携带任何用户指针——回调收到事件时必须反查本表才能
/// 路由回安装该钩子的 [`PopupBlockerModule`] 实例（不同实例各持独立的规则存储与
/// 截图留痕状态，互不串扰）。键取句柄的裸指针值（`usize`）：句柄类型自身未实现
/// `Hash`。
///
/// 生命周期与钩子安装 / 卸载严格对齐：泵线程在 `SetWinEventHook` 成功**之后**注册、
/// 在 `UnhookWinEvent` **之前**注销；句柄被操作系统复用时不会命中残留映射。
static HOOK_ROUTING_REGISTRY: OnceLock<StdRwLock<HashMap<usize, Arc<HookRouting>>>> =
    OnceLock::new();

/// 单个钩子句柄对应的实例路由信息（规则存储 + 可选截图留痕状态）。
#[cfg(windows)]
struct HookRouting {
    /// 所属实例的动态黑名单规则存储（回调的匹配事实源）。
    rules: Arc<RuleStore>,
    /// 所属实例的弹窗截图留痕状态（`None` = 未启用截图留痕）。
    screenshots: Option<Arc<ScreenshotState>>,
}

#[cfg(windows)]
fn hook_registry() -> &'static StdRwLock<HashMap<usize, Arc<HookRouting>>> {
    HOOK_ROUTING_REGISTRY.get_or_init(|| StdRwLock::new(HashMap::new()))
}

#[cfg(windows)]
fn hook_key(hook: HWINEVENTHOOK) -> usize {
    hook.0 as usize
}

#[cfg(windows)]
fn register_hook_routing(hook: HWINEVENTHOOK, routing: Arc<HookRouting>) {
    hook_registry()
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(hook_key(hook), routing);
}

#[cfg(windows)]
fn unregister_hook_routing(hook: HWINEVENTHOOK) {
    hook_registry()
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&hook_key(hook));
}

#[cfg(windows)]
fn lookup_hook_routing(hook: HWINEVENTHOOK) -> Option<Arc<HookRouting>> {
    hook_registry()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&hook_key(hook))
        .cloned()
}

// ---------------------------------------------------------------------------
// 弹窗拦截截图留痕（v0.3.2：命中 → 截图 PNG → 记录 → UI 预览）
// ---------------------------------------------------------------------------

/// 单条弹窗拦截留痕记录（文件名 / 标题 / 类名 / 时间 / 绝对路径）。
///
/// 由截图 worker 线程在成功落盘后写入进程内记录表；UI 装配层经
/// [`PopupBlockerModule::captures`] 拉取构建列表模型，选中条目时按 `path`
/// 加载图片预览。
#[derive(Debug, Clone)]
pub struct CaptureRecord {
    /// 截图文件名（`popup_<YYYYMMDD_HHMMSS>_<序号>.png`）。
    pub file_name: String,
    /// 被拦截弹窗的窗口标题（拦截时刻读取，可能含不可打印字符）。
    pub title: String,
    /// 被拦截弹窗的窗口类名。
    pub class_name: String,
    /// 拦截时刻的本地时间文本（`YYYY-MM-DD HH:MM:SS`）。
    pub captured_at: String,
    /// 截图文件的绝对路径（UI 预览按此加载）。
    pub path: PathBuf,
}

/// 进程内最多保留的留痕记录条数（新覆盖旧，防止无限增长）。
const MAX_CAPTURE_RECORDS: usize = 50;

/// 弹窗截图任务载荷（回调侧投递、worker 侧消费；`hwnd` 以裸指针值传递以
/// 满足通道的 `Send` 约束——Win32 句柄类型自身不实现 `Send`）。
#[cfg(windows)]
struct CaptureJob {
    hwnd: usize,
    title: String,
    class_name: String,
}

/// 截图留痕的共享状态（模块、钩子路由、worker 三方共享）。
///
/// 停机协议：`stop()` 把 `tx` 置为 `None`（关闭通道）→ worker 的
/// `blocking_recv` 返回 `None` 自然退出；模块 / 路由各自持有的 `Arc` 释放后
/// 状态整体回收。记录表与版本号在启停周期之间**持续保留**（重启模块不清空
/// 历史留痕）。
struct ScreenshotState {
    /// 截图落盘目录（装配期由配置解析，运行期不变）。
    dir: PathBuf,
    /// 文件名序号（同毫秒去重，进程内单调递增）。
    seq: Arc<AtomicU64>,
    /// 进程内留痕记录表（新记录在前，上限 [`MAX_CAPTURE_RECORDS`]）。
    records: Arc<StdMutex<VecDeque<CaptureRecord>>>,
    /// 留痕版本号（每次成功截图自增；UI 经 [`watch`] 订阅实时刷新）。
    version_tx: watch::Sender<u64>,
    /// 截图任务通道发送端（`None` = worker 未运行 / 停机中）。
    #[cfg(windows)]
    tx: StdMutex<Option<tokio_mpsc::UnboundedSender<CaptureJob>>>,
    /// 非 Windows 占位（截图能力仅 Windows 存在）。
    #[cfg(not(windows))]
    _tx: (),
}

impl ScreenshotState {
    /// 投递一条截图任务（回调侧调用：仅一次无界通道发送，绝不阻塞）。
    #[cfg(windows)]
    fn submit(&self, hwnd: usize, title: &str, class_name: &str) {
        let tx = self
            .tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(tx) = tx else {
            return; // worker 未运行 / 停机中：静默跳过截图
        };
        let _ = tx.send(CaptureJob {
            hwnd,
            title: title.to_string(),
            class_name: class_name.to_string(),
        });
    }

    /// 取当前留痕记录快照（新→旧；仅短暂持锁克隆，随即锁外返回）。
    fn snapshot(&self) -> Vec<CaptureRecord> {
        let guard = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        guard.iter().cloned().collect()
    }
}

/// 截图 worker 线程主体：逐条消费截图任务并串行落盘（见模块文档的线程拓扑）。
///
/// 仅持有通道接收端与各共享部件（`Arc` 克隆），**不**持有整个
/// [`ScreenshotState`]——否则停机时 `Arc` 引用会让通道发送端永不析构、worker
/// 无法退出。
#[cfg(windows)]
fn screenshot_worker(
    mut rx: tokio_mpsc::UnboundedReceiver<CaptureJob>,
    dir: PathBuf,
    seq: Arc<AtomicU64>,
    records: Arc<StdMutex<VecDeque<CaptureRecord>>>,
    version_tx: watch::Sender<u64>,
) {
    while let Some(job) = rx.blocking_recv() {
        let seq_no = seq.fetch_add(1, Ordering::Relaxed);
        let now = chrono::Local::now();
        let file_name = format!("popup_{}_{:06}.png", now.format("%Y%m%d_%H%M%S"), seq_no);
        let path = dir.join(&file_name);

        // 截图失败仅告警：拦截动作（WM_CLOSE）不依赖截图结果。
        match crate::platform::capture_window_png(job.hwnd, &path) {
            Ok(()) => {
                // 先记录成功日志（借用），随后 title / class_name 移入 record。
                tracing::info!(
                    target: "popup_blocker",
                    "弹窗截图已留存（标题=\"{}\", 类名=\"{}\", '{}'）",
                    job.title,
                    job.class_name,
                    path.display()
                );
                let record = CaptureRecord {
                    captured_at: now.format("%Y-%m-%d %H:%M:%S").to_string(),
                    file_name,
                    title: job.title,
                    class_name: job.class_name,
                    path,
                };
                {
                    let mut guard = records.lock().unwrap_or_else(PoisonError::into_inner);
                    guard.push_front(record);
                    guard.truncate(MAX_CAPTURE_RECORDS);
                }
                // 版本号自增 → UI 的 watch 订阅被唤醒（含 0 → 1 的首次投递）。
                let _ = version_tx.send(version_tx.borrow().wrapping_add(1));
            }
            Err(err) => {
                tracing::warn!(
                    target: "popup_blocker",
                    "弹窗截图失败（标题=\"{}\", 目标 '{}'）: {err}",
                    job.title,
                    path.display()
                );
            }
        }
    }
    tracing::debug!(target: "popup_blocker", "截图 worker 已退出（通道关闭）");
}

/// WinEvent 系统回调的裸函数指针类型（与 `WINEVENTPROC` 载荷一致）。
///
/// 显式声明以便对关联函数做 `as` 转换，规避泛型推断歧义。
#[cfg(windows)]
type WinEventCallback = unsafe extern "system" fn(HWINEVENTHOOK, u32, HWND, i32, i32, u32, u32);

/// Win32 弹窗拦截模块。
///
/// 生命周期完全由内部可变性管理（`AtomicBool` + 内部锁），对调度器 / UI 仅暴露
/// 共享引用接口，天然满足 [`ToolModule`] 的 `Send + Sync` 契约。
#[derive(Clone)]
pub struct PopupBlockerModule {
    inner: Arc<PopupBlockerInner>,
}

/// 模块内部并发状态。
struct PopupBlockerInner {
    /// 串行化 `start` / `stop` 生命周期变迁，杜绝并发启停互相穿插。
    lifecycle: AsyncMutex<()>,
    /// 快速查询的运行标志（无锁读取路径，供 UI 高频轮询）。
    running: AtomicBool,
    /// 当前活动运行上下文（一次运行仅对应一条泵线程）。
    active: StdMutex<Option<ActiveRun>>,
    /// 动态黑名单规则存储（copy-on-write，见 [`RuleStore`]）：实例级事实源，
    /// 由装配层注入（`with_rules`），运行期可热更新（`update_rules`），
    /// 钩子回调经全局句柄注册表路由回本存储。
    rules: Arc<RuleStore>,
    /// 弹窗拦截截图留痕状态（`None` = 未启用，见 [`PopupBlockerModule::with_rules`]）。
    screenshots: Option<Arc<ScreenshotState>>,
}

/// 单次运行（泵线程 + 可选截图 worker）的运行时上下文。
struct ActiveRun {
    /// 停机广播令牌：`cancel()` 即请求退出。
    cancel: CancellationToken,
    #[cfg(windows)]
    /// 泵线程线程 ID（供 `PostThreadMessageW(WM_QUIT)` 定向投递）。
    thread_id: u32,
    #[cfg(windows)]
    /// 泵线程 Join 句柄（`stop()` 借此确认 `UnhookWinEvent` 已执行完毕）。
    thread: std::thread::JoinHandle<()>,
    #[cfg(windows)]
    /// 截图 worker Join 句柄（`stop()` 关闭通道后借此确认 worker 已退出）。
    screenshot_thread: Option<std::thread::JoinHandle<()>>,
}

impl PopupBlockerModule {
    /// 构造一个尚未启动、**规则为空**的弹窗拦截模块。
    ///
    /// 空黑名单语义 = 不拦截任何窗口（deny-list 为空的保守默认）。常规装配路径应使用
    /// [`Self::with_rules`] 注入配置中的黑名单；`new()` 供测试与延迟配置场景使用。
    pub fn new() -> Self {
        Self::with_rules(Vec::<String>::new())
    }

    /// 以初始黑名单关键词构造模块（装配层注入 [`crate::config::AppConfig::popup_blacklist`]）。
    ///
    /// 未启用弹窗截图留痕（等价于 [`Self::with_rules_and_screenshot_dir`] 传 `None`）。
    pub fn with_rules(rules: impl IntoIterator<Item = String>) -> Self {
        Self::with_rules_and_screenshot_dir(rules, None)
    }

    /// 以初始黑名单关键词与截图留痕目录构造模块。
    ///
    /// `screenshot_dir = Some(dir)` 时启用弹窗截图留痕：命中黑名单的窗口在
    /// `WM_CLOSE` 前经专用 worker 截取 PNG 至该目录（目录由调用方按
    /// [`AppConfig::effective_popup_screenshot_dir`](crate::config::AppConfig::effective_popup_screenshot_dir)
    /// 解析为绝对路径）；`None` 时拦截动作照常、不产生截图。
    pub fn with_rules_and_screenshot_dir(
        rules: impl IntoIterator<Item = String>,
        screenshot_dir: Option<PathBuf>,
    ) -> Self {
        let screenshots = screenshot_dir.map(|dir| {
            Arc::new(ScreenshotState {
                dir,
                seq: Arc::new(AtomicU64::new(0)),
                records: Arc::new(StdMutex::new(VecDeque::new())),
                version_tx: watch::channel(0).0,
                #[cfg(windows)]
                tx: StdMutex::new(None),
                #[cfg(not(windows))]
                _tx: (),
            })
        });
        Self {
            inner: Arc::new(PopupBlockerInner {
                lifecycle: AsyncMutex::new(()),
                running: AtomicBool::new(false),
                active: StdMutex::new(None),
                rules: Arc::new(RuleStore::new(rules)),
                screenshots,
            }),
        }
    }

    /// 回读当前生效的截图留痕记录（新→旧，上限 [`MAX_CAPTURE_RECORDS`] 条；
    /// 未启用截图留痕时返回空列表）。
    pub fn captures(&self) -> Vec<CaptureRecord> {
        match &self.inner.screenshots {
            Some(state) => state.snapshot(),
            None => Vec::new(),
        }
    }

    /// 订阅留痕版本号：每次成功截图落盘后自增。
    ///
    /// UI 装配层以 `changed().await` 循环消费；未启用截图留痕时返回的接收端
    /// 立即报 `Err`（调用方应退出刷新循环）。
    pub fn captures_watch(&self) -> watch::Receiver<u64> {
        match &self.inner.screenshots {
            Some(state) => state.version_tx.subscribe(),
            None => watch::channel(0).1,
        }
    }

    /// 当前生效的截图目录（未启用时返回 `None`，供 UI 展示与「打开目录」入口）。
    pub fn screenshot_dir(&self) -> Option<PathBuf> {
        self.inner.screenshots.as_ref().map(|state| state.dir.clone())
    }

    /// 热更新黑名单：整体替换规则快照（发布-订阅语义）。
    ///
    /// - 同步方法、微秒级完成，任意线程（含 Tokio 工作线程）可调用；
    /// - **无需重启**正在运行的原生消息泵线程：回调每条事件现场读取当前快照，
    ///   更新落定后下一条事件即按新规则判定；
    /// - 传入的关键词会先经 [`RuleSet::compile`] 归一化（去空白、剔除空串、去重）。
    pub fn update_rules(&self, rules: impl IntoIterator<Item = String>) {
        self.inner.rules.set(rules);
        tracing::debug!(
            target: "popup_blocker",
            "黑名单已热更新（{} 条关键词，无需重启消息泵线程）",
            self.inner.rules.snapshot().patterns.len()
        );
    }

    /// 回读当前生效的黑名单（归一化后的原始关键词，供 UI / 诊断展示）。
    pub fn current_rules(&self) -> Vec<String> {
        self.inner.rules.snapshot().raw_keywords()
    }

    /// WinEvent 事件回调：由泵线程在 `DispatchMessageW` 派发阶段被系统调用。
    ///
    /// # Safety / 约束
    /// - 必须与 `WINEVENTPROC` 布局一致（`unsafe extern "system"`）；
    /// - 运行于专用原生线程的系统回调上下文，**严禁**在其中执行任何异步 / Tokio
    ///   操作，也禁止可能 `panic` / 跨 FFI 边界展开的代码；
    /// - 黑名单不再硬编码：回调经句柄注册表（[`lookup_hook_routing`]）路由回所属实例
    ///   的规则存储与截图状态——锁内只做查表与 `Arc` 克隆两个指针级操作，窗口文本
    ///   查询与字符串匹配全部在**无锁路径**上执行，规则热更新无需重启泵线程；
    /// - 命中后**只投递不执行**：截图任务经无界通道交给专用 worker（`PrintWindow`
    ///   等重量级操作绝不在回调内执行），随后立即异步投递 `WM_CLOSE`。
    #[cfg(windows)]
    unsafe extern "system" fn win_event_proc(
        hook: HWINEVENTHOOK,
        event: u32,
        hwnd: HWND,
        id_object: i32,
        id_child: i32,
        _event_thread: u32,
        _event_time: u32,
    ) {
        // OBJID_WINDOW == 0 && CHILDID_SELF == 0：只关心窗口本体（而非子元素/子对象）的创建。
        if event != EVENT_OBJECT_CREATE || hwnd.0.is_null() || id_object != 0 || id_child != 0 {
            return;
        }

        // 快照为空（空黑名单 = 不拦截）时零分配短路：连窗口文本都无需读取。
        let Some(routing) = lookup_hook_routing(hook) else {
            return; // 钩子已注销 / 注册表未就绪：忽略该事件
        };
        let snapshot = routing.rules.snapshot();
        if snapshot.is_empty() {
            return;
        }

        // SAFETY: 以下 Win32 查询均为进程内只读调用，hwnd 可能已失效（窗口刚销毁），
        // 但 GetClassNameW / GetWindowTextW 对失效句柄返回 0，不会引发访问违规。
        unsafe {
            let mut class_buf = [0u16; 256];
            let mut title_buf = [0u16; 256];
            let class_len = GetClassNameW(hwnd, &mut class_buf).max(0) as usize;
            let title_len = GetWindowTextW(hwnd, &mut title_buf).max(0) as usize;

            let class_name = String::from_utf16_lossy(&class_buf[..class_len.min(class_buf.len())]);
            let title = String::from_utf16_lossy(&title_buf[..title_len.min(title_buf.len())]);

            // 快照匹配：子串匹配 + 忽略大小写（大小写归一在快照写入时已完成）。
            if !snapshot.matches(&title, &class_name) {
                return;
            }

            tracing::warn!(
                target: "popup_blocker",
                "捕获目标弹窗: 标题=\"{title}\", 类名=\"{class_name}\", HWND=0x{:X}; 下发 WM_CLOSE 关闭指令",
                hwnd.0 as usize,
            );
            // 截图留痕（可选）：把任务投递给专用 worker 后立即继续——PrintWindow
            // 与 PNG 编码绝不阻塞钩子消息泵。
            if let Some(screenshots) = &routing.screenshots {
                screenshots.submit(hwnd.0 as usize, &title, &class_name);
            }
            // 关闭指令为**异步消息投递**（PostMessageW），不等待目标窗口处理。
            // 严禁改用同步阻塞式 SendMessageW：WinEvent 回调运行在系统回调上下文
            // 中，同步等待目标窗口响应会阻塞本进程消息泵，且受 UIPI 限制向高权限
            // 窗口同步发送会直接失败——异步投递 + 不等待是防卡死钩子消息泵的唯一
            // 正确形态（SendMessageTimeoutW 亦可用，但此处无需任何应答，Post 最优）。
            let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }

    /// 泵线程主体：运行于专用操作系统原生线程，承载钩子安装与标准 Win32 消息泵。
    ///
    /// `routing` 为所属实例的路由信息（规则存储 + 截图状态，`Arc` 克隆传入，
    /// 线程独立持有）：装钩成功后立即注册「钩子句柄 → 路由」映射，卸载钩子前
    /// 注销——回调由此路由回所属实例。
    ///
    /// 线程退出前必须完成 `UnhookWinEvent`（钩子只能由安装线程卸载）。
    #[cfg(windows)]
    fn pump_thread_main(ready_tx: oneshot::Sender<Result<u32, String>>, routing: Arc<HookRouting>) {
        // SAFETY:
        // - 本函数整体运行在由 `std::thread::Builder::spawn` 派生的专用原生线程中；
        // - `PeekMessageW` 仅用于建立本线程消息队列（取不到消息也无副作用）；
        // - `SetWinEventHook`（WINEVENT_OUTOFCONTEXT）将回调挂载到本线程消息队列，
        //   由紧随其后的 `GetMessageW` / `DispatchMessageW` 标准消息泵承载派发；
        // - 退出路径在同线程调用 `UnhookWinEvent`，满足钩子卸载的线程亲和约束。
        unsafe {
            // 1) 强制建立本线程消息队列。
            //    `PostThreadMessageW` 只接受“已建队列”的线程；先建队再回报线程 ID，
            //    从根上消除“WM_QUIT 早于队列建立而投递失败”的竞态。
            let mut seed = MSG::default();
            let _ = PeekMessageW(&mut seed, None, 0, 0, PM_NOREMOVE);

            // 2) 在本线程安装系统级 UI 事件钩子（监听 EVENT_OBJECT_CREATE）。
            //    回调内置于本进程代码，无需按模块句柄定位 DLL，故传 NULL。
            let callback_module: Option<&HMODULE> = None;
            let hook = SetWinEventHook(
                EVENT_OBJECT_CREATE,
                EVENT_OBJECT_CREATE,
                callback_module,
                Some(Self::win_event_proc as WinEventCallback),
                0, // idProcess = 0：监听所有进程
                0, // idThread  = 0：监听所有线程
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );

            if hook.0.is_null() {
                let _ = ready_tx.send(Err(
                    "SetWinEventHook 安装失败：句柄为空（可能缺少必要权限或处于无桌面会话）"
                        .to_string(),
                ));
                return;
            }

            // 2.5) 注册「钩子句柄 → 实例路由」映射。必须在握手与消息泵之前完成：
            //      回调一经派发即可正确定位所属实例的当前规则与截图状态。
            register_hook_routing(hook, routing);

            let thread_id = GetCurrentThreadId();

            // 3) 与父侧握手。若父侧已放弃等待（oneshot 关闭 / future 被取消），
            //    立即注销路由并卸载钩子后退出，绝不遗留无主事件钩子。
            if ready_tx.send(Ok(thread_id)).is_err() {
                unregister_hook_routing(hook);
                let _ = UnhookWinEvent(hook);
                return;
            }

            tracing::info!(
                target: "popup_blocker",
                "Win32 原生事件钩子已就绪（泵线程 {thread_id}），进入消息循环"
            );

            // 4) 标准 Win32 消息泵：GetMessageW 取到 WM_QUIT 时返回 FALSE（0），循环退出。
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                if msg.message == WM_QUIT {
                    // 显式防御：正常情况下 WM_QUIT 已令 GetMessageW 返回 FALSE。
                    break;
                }
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageW(&msg);
            }

            // 5) 泵退出后在同一线程注销路由并安全卸载钩子，随后线程自然结束。
            //    顺序不可颠倒：先注销再 UnhookWinEvent——句柄在卸载后才可能被操作系统
            //    复用给新钩子，先注销可杜绝“复用句柄命中残留映射”的竞态。
            unregister_hook_routing(hook);
            let _ = UnhookWinEvent(hook);
            tracing::info!(target: "popup_blocker", "Win32 原生事件钩子已安全卸载，泵线程退出");
        }
    }

    /// Windows 实现：派生专用原生线程运行钩子消息泵。
    #[cfg(windows)]
    async fn start_native(&self) -> Result<(), ModuleError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        if self.inner.running.load(Ordering::Acquire) {
            return Ok(()); // 已在运行：幂等
        }

        let cancel = CancellationToken::new();
        let (ready_tx, ready_rx) = oneshot::channel::<Result<u32, String>>();

        // 所属实例的路由信息（规则存储 + 截图状态）：泵线程装钩成功后注册句柄路由，
        // 回调据此读取动态黑名单与投递截图任务；规则热更新只写存储、不触碰泵线程。
        let routing = Arc::new(HookRouting {
            rules: Arc::clone(&self.inner.rules),
            screenshots: self.inner.screenshots.clone(),
        });

        // 截图 worker 先行启动（若启用截图留痕）：回调一经派发即可投递任务。
        // worker 退出语义见 ScreenshotState 文档——stop() 置 tx 为 None 后
        // blocking_recv 返回 None，worker 自然结束。
        let screenshot_thread = if let Some(state) = &self.inner.screenshots {
            // 截图目录预建（GDI+ 编码要求父目录存在；创建失败仅告警——每次
            // 截图仍会重试 create_dir_all，避免一次性故障永久禁用留痕）。
            if let Err(err) = std::fs::create_dir_all(&state.dir) {
                tracing::warn!(
                    target: "popup_blocker",
                    "截图目录创建失败: '{}' -> {err}",
                    state.dir.display()
                );
            }
            let (tx, rx) = tokio_mpsc::unbounded_channel::<CaptureJob>();
            *state.tx.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
            // 先取走闭包所需的全部拥有态部件（不得捕获借用 self 的 state 引用：
            // std::thread::spawn 要求 'static 闭包）。
            let dir = state.dir.clone();
            let seq = Arc::clone(&state.seq);
            let records = Arc::clone(&state.records);
            let version_tx = state.version_tx.clone();
            let worker = std::thread::Builder::new()
                .name("popup-screenshot-worker".to_string())
                .spawn(move || screenshot_worker(rx, dir, seq, records, version_tx))
                .map_err(|e| -> ModuleError { Box::new(e) })?;
            Some(worker)
        } else {
            None
        };

        // 派生专用操作系统原生线程承载钩子与消息泵（严禁放置于 Tokio 协程中）。
        let thread = std::thread::Builder::new()
            .name("win32-popup-hook-pump".to_string())
            .spawn(move || Self::pump_thread_main(ready_tx, routing))
            .map_err(|e| -> ModuleError { Box::new(e) })?;

        // 等待原生线程完成“建队列 + 装钩子”握手，安装失败则回收线程并上报。
        let thread_id = match ready_rx.await {
            Ok(Ok(thread_id)) => thread_id,
            Ok(Err(reason)) => {
                // 线程已自行退出；Join 仅回收句柄，随后向调用方暴露失败原因。
                let _ = thread.join();
                return Err(reason.into());
            }
            Err(_) => {
                // 通道关闭：泵线程检测到父侧放弃后已自行卸载退出，此处回收句柄。
                let _ = thread.join();
                return Err("弹窗拦截模块启动握手被中断".into());
            }
        };

        self.inner.running.store(true, Ordering::Release);
        *self.active_lock()? = Some(ActiveRun {
            cancel,
            thread_id,
            thread,
            screenshot_thread,
        });

        tracing::info!(target: "popup_blocker", "弹窗拦截模块已启动（泵线程 {thread_id}）");
        Ok(())
    }

    /// 非 Windows 兜底实现：仅维持虚拟生命周期，供跨平台编译与调度联调。
    #[cfg(not(windows))]
    async fn start_virtual(&self) -> Result<(), ModuleError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        if self.inner.running.load(Ordering::Acquire) {
            return Ok(()); // 已在运行：幂等
        }

        let cancel = CancellationToken::new();
        let token = cancel.clone();
        tokio::spawn(async move {
            tracing::warn!(
                target: "popup_blocker",
                "当前系统非 Windows 平台：弹窗拦截模块仅维持虚拟生命周期"
            );
            token.cancelled().await;
            tracing::debug!(target: "popup_blocker", "虚拟生命周期已结束");
        });

        self.inner.running.store(true, Ordering::Release);
        *self.active_lock()? = Some(ActiveRun { cancel });

        tracing::info!(target: "popup_blocker", "弹窗拦截模块已启动（虚拟生命周期）");
        Ok(())
    }

    /// 取得活动上下文锁（将“锁中毒”这类异常状态显式上报为模块错误）。
    fn active_lock(&self) -> Result<std::sync::MutexGuard<'_, Option<ActiveRun>>, ModuleError> {
        self.inner
            .active
            .lock()
            .map_err(|_| -> ModuleError { "popup_blocker: 活动上下文锁中毒".into() })
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
            // 解构运行上下文：thread_id / thread 进入泵线程 Join，screenshot_thread
            // 进入截图 worker Join（两者互不依赖，顺序执行）。
            let ActiveRun {
                cancel: _,
                thread_id,
                thread,
                screenshot_thread,
            } = run;

            // 2) 向泵线程消息队列定向投递 WM_QUIT，唤醒阻塞中的 GetMessageW。
            //    队列在握手阶段已建立，此处投递必然成功。
            // SAFETY: WM_QUIT 为已定义消息常量，参数类型匹配；投递失败仅返回错误码，无 UB。
            let _ = unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };

            // 3) Join 泵线程并施加超时：确保 UnhookWinEvent 已在安装线程上执行完毕
            //    （钩子路由随之注销，路由内持有的 Arc<ScreenshotState> 释放）。
            //    三层 Result 展开：timeout → spawn_blocking JoinHandle → 线程 join。
            let join_result = tokio::time::timeout(
                PUMP_JOIN_TIMEOUT,
                tokio::task::spawn_blocking(move || thread.join()),
            )
            .await;

            match join_result {
                Ok(Ok(Ok(()))) => {
                    tracing::debug!(target: "popup_blocker", "泵线程已退出，事件钩子已卸载");
                }
                Ok(Ok(Err(panic))) => {
                    // std 线程 join 的 panic 载荷为 Box<dyn Any + Send>，需 downcast 取可读文本。
                    let payload = panic
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "未知 panic 载荷".to_string());
                    tracing::error!(target: "popup_blocker", "泵线程异常终止: {payload}");
                    self.inner.running.store(false, Ordering::Release);
                    return Err(format!("弹窗拦截泵线程异常终止: {payload}").into());
                }
                Ok(Err(task_err)) => {
                    tracing::error!(target: "popup_blocker", "Join 阻塞任务异常终止: {task_err}");
                    self.inner.running.store(false, Ordering::Release);
                    return Err(format!("弹窗拦截 Join 阻塞任务异常终止: {task_err}").into());
                }
                Err(_elapsed) => {
                    // 极端场景（回调阻塞于跨进程窗口查询的系统超时窗口内）：泵线程转入
                    // 分离式收尾——WM_QUIT 已投递，UnhookWinEvent 最终仍会在该线程执行。
                    tracing::error!(
                        target: "popup_blocker",
                        "泵线程未在 {PUMP_JOIN_TIMEOUT:?} 内退出，已转入分离式收尾"
                    );
                }
            }

            // 4) 关闭截图通道（worker 的 blocking_recv 收到 None 后退出）并 Join worker。
            if let Some(state) = &self.inner.screenshots {
                *state.tx.lock().unwrap_or_else(PoisonError::into_inner) = None;
            }
            if let Some(worker) = screenshot_thread {
                match tokio::time::timeout(
                    PUMP_JOIN_TIMEOUT,
                    tokio::task::spawn_blocking(move || worker.join()),
                )
                .await
                {
                    Ok(Ok(Ok(()))) => {
                        tracing::debug!(target: "popup_blocker", "截图 worker 已退出");
                    }
                    Ok(Ok(Err(panic))) => {
                        let payload = panic
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "未知 panic 载荷".to_string());
                        tracing::error!(target: "popup_blocker", "截图 worker 异常终止: {payload}");
                    }
                    Ok(Err(task_err)) => {
                        tracing::error!(target: "popup_blocker", "截图 worker Join 任务异常: {task_err}");
                    }
                    Err(_elapsed) => {
                        tracing::error!(
                            target: "popup_blocker",
                            "截图 worker 未在 {PUMP_JOIN_TIMEOUT:?} 内退出，已转入分离式收尾"
                        );
                    }
                }
            }
        }

        self.inner.running.store(false, Ordering::Release);
        Ok(())
    }
}

impl Default for PopupBlockerModule {
    fn default() -> Self {
        Self::new()
    }
}

/// 兜底清理：即使调用方遗忘 `stop()`，实例消亡时也要唤醒泵线程，确保
/// `UnhookWinEvent` 最终在安装线程上执行（JoinHandle 随 `run` 析构而分离，不阻塞当前线程）。
impl Drop for PopupBlockerInner {
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
        // 泵线程退出 GetMessageW 循环后自行执行 UnhookWinEvent。
    }
}

#[async_trait]
impl ToolModule for PopupBlockerModule {
    fn id(&self) -> &'static str {
        "popup_blocker"
    }

    fn display_name(&self) -> &'static str {
        "桌面弹窗拦截"
    }

    fn description(&self) -> &'static str {
        "通过 Win32 原生事件钩子毫秒级拦截广告弹窗与流氓进程窗口"
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

    /// 生命周期幂等性验证：重复 `start` / `stop` 不得产生线程 / 钩子泄漏。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_start_stop_is_idempotent_and_converges() {
        let module = PopupBlockerModule::new();
        assert!(!module.is_running());

        for cycle in 1..=3 {
            module.start().await.expect("启动应成功");
            assert!(module.is_running(), "第 {cycle} 轮启动后应处于运行态");

            module.start().await.expect("重复启动应幂等成功");
            assert!(module.is_running());

            module.stop().await.expect("停止应成功");
            assert!(!module.is_running(), "第 {cycle} 轮停止后应退出运行态");

            module.stop().await.expect("重复停止应幂等成功");
            assert!(!module.is_running());
        }
    }

    /// 并发调度验证：多个任务同时发起 start / stop，生命周期锁应保证串行收敛，
    /// 全程只存在一条泵线程，最终状态一致且无死锁。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_lifecycle_operations_are_serialized() {
        let module = PopupBlockerModule::new();

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
        assert!(module.is_running());

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
        assert!(!module.is_running());
    }

    // -----------------------------------------------------------------------
    // 动态黑名单规则核心（跨平台纯逻辑，不依赖 Win32）
    // -----------------------------------------------------------------------

    /// 默认关键词的命中语义：子串匹配 + 忽略大小写；标题与类名任一命中即算命中。
    #[test]
    fn rule_set_matches_substring_case_insensitively() {
        let rules = RuleSet::compile(
            ["广告", "Flash Helper Service", "Update Notice", "推广弹窗"]
                .into_iter()
                .map(String::from),
        );

        // 中文逐字子串命中（标题 / 类名任一命中均可）。
        assert!(rules.matches("今日推广弹窗已拦截", "SomeClass"));
        assert!(rules.matches("普通标题", "广告专用窗口类"));
        // 拉丁关键词忽略大小写命中。
        assert!(rules.matches("FLASH HELPER SERVICE 正在运行", "#32770"));
        assert!(rules.matches("请查看 update notice", "Chrome_WidgetWin_1"));
        // 完全无关的窗口不得误伤。
        assert!(!rules.matches("Steam 下载中", "Chrome_WidgetWin_1"));
        assert!(!rules.matches("", ""));
    }

    /// 空黑名单（等价的 deny-list 空集）不得命中任何窗口——含空标题 / 类名输入。
    #[test]
    fn empty_rule_set_blocks_nothing() {
        let rules = RuleSet::default();
        assert!(rules.is_empty());
        assert!(!rules.matches("广告", ""));
        assert!(!rules.matches("", "Flash Helper Service"));
        assert!(!rules.matches("", ""));
    }

    /// 归一化契约：去首尾空白、剔除空白串、按小写针大小写不敏感去重（保留首现形态）。
    #[test]
    fn rule_set_compile_normalizes_trims_and_dedups() {
        let rules = RuleSet::compile(
            [
                "  广告  ", // 去空白后与下方 "广告" 重复
                "广告",
                "",    // 空串剔除
                "   ", // 纯空白剔除
                "Flash Helper Service",
                "flash helper service", // 与上一条同义（忽略大小写）→ 剔除
            ]
            .into_iter()
            .map(String::from),
        );

        assert_eq!(
            rules.raw_keywords(),
            vec!["广告".to_string(), "Flash Helper Service".to_string()],
            "应保留首现形态并按序去重"
        );
        assert!(!rules.is_empty());
    }

    /// [`RuleStore`] 的 copy-on-write 语义：`set` 整体替换快照后，新针即时生效、
    /// 旧针即时失效；已被快照引用的旧 `Arc` 不受影响（隔离性）。
    #[test]
    fn rule_store_set_swaps_snapshot_immediately() {
        let store = RuleStore::new(["广告"].into_iter().map(String::from));

        let before = store.snapshot();
        assert!(before.matches("xx广告xx", ""));
        assert!(!before.matches("推广弹窗", ""));

        // 热替换：增删（增 "推广弹窗"、删 "广告"）在同一份快照中原子生效。
        store.set(["推广弹窗", "Update Notice"].into_iter().map(String::from));

        let after = store.snapshot();
        assert!(after.matches("请查看 Update Notice", ""));
        assert!(after.matches("推广弹窗", ""));
        assert!(!after.matches("xx广告xx", ""), "删除的关键词应立即失效");

        // 旧快照 Arc 仍是不可变历史版本，语义不受后续写入影响。
        assert!(before.matches("xx广告xx", ""));

        // 清空 → 恢复“不拦截任何窗口”。
        store.set(Vec::<String>::new());
        assert!(store.snapshot().is_empty());
        assert!(!store.snapshot().matches("Update Notice", ""));
    }

    /// 模块级热更新：**运行中**调用 `update_rules` 不打断生命周期、无需重启泵线程，
    /// 新黑名单对后续判定即时生效；`current_rules` 回读归一化结果。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_rules_takes_effect_while_running_without_restart() {
        let module = PopupBlockerModule::with_rules(["广告"].into_iter().map(String::from));
        assert_eq!(module.current_rules(), vec!["广告".to_string()]);

        module.start().await.expect("启动应成功");
        assert!(module.is_running());

        // 运行态热更新：增删关键词，不调用 stop / start。
        module.update_rules(
            ["  Flash Helper Service  ", "推广弹窗", "广告", "广告"]
                .into_iter()
                .map(String::from),
        );

        // 生命周期未被热更新打断。
        assert!(module.is_running(), "热更新不得影响泵线程运行状态");

        // 归一化 + 去重后的回读结果（"广告" 重复条目仅保留一个）。
        assert_eq!(
            module.current_rules(),
            vec![
                "Flash Helper Service".to_string(),
                "推广弹窗".to_string(),
                "广告".to_string(),
            ]
        );

        // 匹配判定读取的是更新后的快照（即时生效性）。
        let snapshot = module.inner.rules.snapshot();
        assert!(snapshot.matches("FLASH HELPER SERVICE", ""));
        assert!(snapshot.matches("xx推广弹窗xx", ""));
        assert!(
            !snapshot.matches("普通窗口内容", "Edit"),
            "无关窗口不得误伤"
        );
        assert!(!snapshot.matches("", ""));

        module.stop().await.expect("停止应成功");
        assert!(!module.is_running());
    }

    // -----------------------------------------------------------------------
    // 弹窗拦截截图留痕（v0.3.2：记录表 / 版本号 / 目录注入契约）
    // -----------------------------------------------------------------------

    /// 构造留痕测试状态（与 `with_rules_and_screenshot_dir` 同构）。
    fn test_screenshot_state() -> Arc<ScreenshotState> {
        Arc::new(ScreenshotState {
            dir: std::env::temp_dir(),
            seq: Arc::new(AtomicU64::new(0)),
            records: Arc::new(StdMutex::new(VecDeque::new())),
            version_tx: watch::channel(0).0,
            #[cfg(windows)]
            tx: StdMutex::new(None),
            #[cfg(not(windows))]
            _tx: (),
        })
    }

    fn sample_record(name: &str) -> CaptureRecord {
        CaptureRecord {
            file_name: name.to_string(),
            title: format!("标题-{name}"),
            class_name: "SampleClass".to_string(),
            captured_at: "2026-09-04 12:00:00".to_string(),
            path: std::env::temp_dir().join(name),
        }
    }

    /// 记录表契约：初始为空、`push_front` 新记录在前、快照为独立克隆。
    #[test]
    fn screenshot_state_snapshot_is_newest_first() {
        let state = test_screenshot_state();
        assert!(state.snapshot().is_empty(), "初始留痕应为空");

        // 模拟 worker 成功路径的写入（push_front 新记录在前）。
        {
            let mut records = state.records.lock().unwrap();
            records.push_front(sample_record("a.png"));
            records.push_front(sample_record("b.png"));
            records.push_front(sample_record("c.png"));
        }
        let snapshot = state.snapshot();
        assert_eq!(snapshot.len(), 3);
        assert_eq!(snapshot[0].file_name, "c.png", "最新记录应在最前");
        assert_eq!(snapshot[2].file_name, "a.png", "最早记录应在最后");
    }

    /// 记录表上限：超过 [`MAX_CAPTURE_RECORDS`] 时截断（worker 同款 truncate 语义）。
    #[test]
    fn screenshot_state_records_are_capped_at_max() {
        let state = test_screenshot_state();
        {
            let mut records = state.records.lock().unwrap();
            for i in 0..(MAX_CAPTURE_RECORDS + 10) {
                records.push_front(sample_record(&format!("{i}.png")));
                records.truncate(MAX_CAPTURE_RECORDS);
            }
        }
        assert_eq!(
            state.snapshot().len(),
            MAX_CAPTURE_RECORDS,
            "留痕记录应被截断至上限"
        );
    }

    /// 版本号订阅契约：成功截图后发送端自增，`watch` 订阅者被唤醒。
    #[tokio::test]
    async fn screenshot_state_version_channel_wakes_subscriber() {
        let state = test_screenshot_state();
        let mut rx = state.version_tx.subscribe();
        assert_eq!(*rx.borrow(), 0, "初始版本号应为 0");

        let _ = state.version_tx.send(1);
        assert!(rx.changed().await.is_ok(), "发送端自增后订阅者应被唤醒");
        assert_eq!(*rx.borrow(), 1);
    }

    /// 截图目录注入契约：`with_rules_and_screenshot_dir(Some(dir))` 可回读；
    /// `new()` / `with_rules` 不启用截图留痕（`screenshot_dir` 为 None、记录为空）。
    #[test]
    fn screenshot_dir_contract_reflects_constructor() {
        let enabled = PopupBlockerModule::with_rules_and_screenshot_dir(
            ["广告"].into_iter().map(String::from),
            Some(PathBuf::from("C:/shots")),
        );
        assert_eq!(enabled.screenshot_dir(), Some(PathBuf::from("C:/shots")));
        assert_eq!(enabled.current_rules(), vec!["广告".to_string()]);
        assert!(enabled.captures().is_empty(), "尚未拦截时留痕应为空");
        // 版本号订阅在启用时应可正常取得（初始 0）。
        assert_eq!(*enabled.captures_watch().borrow(), 0);

        let disabled = PopupBlockerModule::new();
        assert_eq!(disabled.screenshot_dir(), None, "未启用时目录应为 None");
        assert!(disabled.captures().is_empty());
    }
}
