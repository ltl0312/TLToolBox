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
//! （`OnceLock<RwLock<HashMap<hook 句柄, Arc<RuleStore>>>>`，仅在 Windows 平台存在）
//! 把回调收到的 `HWINEVENTHOOK` 路由回它所属的 [`PopupBlockerModule`] 实例——不同实例
//! 各自持有独立的规则存储，互不串扰。泵线程在装钩成功后注册、卸载钩子前注销。
//!
//! 并发模型遵循两条铁律：
//! - **规则存储为 copy-on-write**：[`RuleStore`] 内部是 `std::sync::RwLock<Arc<RuleSet>>`，
//!   `update_rules` 以写锁**整体替换**不可变快照（快照生成时一次性完成大小写归一与
//!   去重，杜绝回调内逐条重复归一）；读取方只短暂持读锁克隆 `Arc`，随即在**锁外**
//!   完成全部字符串匹配；
//! - **回调内绝不持有重锁**：`win_event_proc` 在锁内只做「注册表查表 + 克隆 `Arc`」
//!   两个指针级操作，匹配与窗口查询均在无锁路径上执行，泵线程 / 桌面 UI 不会因
//!   规则更新或并发读取而挂起（更新与查表互斥窗口为微秒级）。

use super::{ModuleError, ToolModule};
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError, RwLock as StdRwLock};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

#[cfg(windows)]
use std::collections::HashMap;
#[cfg(windows)]
use std::sync::OnceLock;
#[cfg(windows)]
use std::time::Duration;

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
/// 进程级钩子句柄注册表：`HWINEVENTHOOK 原始指针值 -> 所属实例的规则存储`。
///
/// `SetWinEventHook` 回调是静态 `unsafe extern "system" fn`，系统只回传
/// `HWINEVENTHOOK` 等载荷、不携带任何用户指针——回调收到事件时必须反查本表才能
/// 路由回安装该钩子的 [`PopupBlockerModule`] 实例（不同实例各持独立规则存储，
/// 互不串扰）。键取句柄的裸指针值（`usize`）：句柄类型自身未实现 `Hash`。
///
/// 生命周期与钩子安装 / 卸载严格对齐：泵线程在 `SetWinEventHook` 成功**之后**注册、
/// 在 `UnhookWinEvent` **之前**注销；句柄被操作系统复用时不会命中残留映射。
static HOOK_RULE_REGISTRY: OnceLock<StdRwLock<HashMap<usize, Arc<RuleStore>>>> = OnceLock::new();

#[cfg(windows)]
fn hook_registry() -> &'static StdRwLock<HashMap<usize, Arc<RuleStore>>> {
    HOOK_RULE_REGISTRY.get_or_init(|| StdRwLock::new(HashMap::new()))
}

#[cfg(windows)]
fn hook_key(hook: HWINEVENTHOOK) -> usize {
    hook.0 as usize
}

#[cfg(windows)]
fn register_hook_rules(hook: HWINEVENTHOOK, store: Arc<RuleStore>) {
    hook_registry()
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(hook_key(hook), store);
}

#[cfg(windows)]
fn unregister_hook_rules(hook: HWINEVENTHOOK) {
    hook_registry()
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&hook_key(hook));
}

#[cfg(windows)]
fn lookup_hook_rules(hook: HWINEVENTHOOK) -> Option<Arc<RuleStore>> {
    hook_registry()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&hook_key(hook))
        .cloned()
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
}

/// 单次运行（泵线程）的运行时上下文。
struct ActiveRun {
    /// 停机广播令牌：`cancel()` 即请求退出。
    cancel: CancellationToken,
    #[cfg(windows)]
    /// 泵线程线程 ID（供 `PostThreadMessageW(WM_QUIT)` 定向投递）。
    thread_id: u32,
    #[cfg(windows)]
    /// 泵线程 Join 句柄（`stop()` 借此确认 `UnhookWinEvent` 已执行完毕）。
    thread: std::thread::JoinHandle<()>,
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
    pub fn with_rules(rules: impl IntoIterator<Item = String>) -> Self {
        Self {
            inner: Arc::new(PopupBlockerInner {
                lifecycle: AsyncMutex::new(()),
                running: AtomicBool::new(false),
                active: StdMutex::new(None),
                rules: Arc::new(RuleStore::new(rules)),
            }),
        }
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
    /// - 黑名单不再硬编码：回调经句柄注册表（[`lookup_hook_rules`]）路由回所属实例
    ///   的规则存储并取快照——锁内只做查表与 `Arc` 克隆两个指针级操作，窗口文本
    ///   查询与字符串匹配全部在**无锁路径**上执行，规则热更新无需重启泵线程。
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
        let Some(snapshot) = lookup_hook_rules(hook).map(|store| store.snapshot()) else {
            return; // 钩子已注销 / 注册表未就绪：忽略该事件
        };
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
            // 关闭指令为异步消息投递，不等待目标窗口处理，杜绝在系统回调中阻塞。
            let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }

    /// 泵线程主体：运行于专用操作系统原生线程，承载钩子安装与标准 Win32 消息泵。
    ///
    /// `rules` 为所属实例的规则存储（`Arc` 克隆传入，线程独立持有）：装钩成功后立即
    /// 注册「钩子句柄 → 规则存储」路由，卸载钩子前注销——回调由此路由回所属实例。
    ///
    /// 线程退出前必须完成 `UnhookWinEvent`（钩子只能由安装线程卸载）。
    #[cfg(windows)]
    fn pump_thread_main(ready_tx: oneshot::Sender<Result<u32, String>>, rules: Arc<RuleStore>) {
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

            // 2.5) 注册「钩子句柄 → 实例规则存储」路由。必须在握手与消息泵之前完成：
            //      回调一经派发即可正确定位所属实例的当前规则。
            register_hook_rules(hook, rules);

            let thread_id = GetCurrentThreadId();

            // 3) 与父侧握手。若父侧已放弃等待（oneshot 关闭 / future 被取消），
            //    立即注销路由并卸载钩子后退出，绝不遗留无主事件钩子。
            if ready_tx.send(Ok(thread_id)).is_err() {
                unregister_hook_rules(hook);
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
            unregister_hook_rules(hook);
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

        // 所属实例的规则存储克隆进泵线程：装钩成功后注册句柄路由，回调据此
        // 读取动态黑名单；规则热更新只写存储、不触碰泵线程。
        let rules = Arc::clone(&self.inner.rules);

        // 派生专用操作系统原生线程承载钩子与消息泵（严禁放置于 Tokio 协程中）。
        let thread = std::thread::Builder::new()
            .name("win32-popup-hook-pump".to_string())
            .spawn(move || Self::pump_thread_main(ready_tx, rules))
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
            // 2) 向泵线程消息队列定向投递 WM_QUIT，唤醒阻塞中的 GetMessageW。
            //    队列在握手阶段已建立，此处投递必然成功。
            // SAFETY: WM_QUIT 为已定义消息常量，参数类型匹配；投递失败仅返回错误码，无 UB。
            let _ = unsafe { PostThreadMessageW(run.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };

            // 3) Join 泵线程并施加超时：确保 UnhookWinEvent 已在安装线程上执行完毕。
            //    三层 Result 展开：timeout → spawn_blocking JoinHandle → 线程 join。
            let join_result = tokio::time::timeout(
                PUMP_JOIN_TIMEOUT,
                tokio::task::spawn_blocking(move || run.thread.join()),
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
}
