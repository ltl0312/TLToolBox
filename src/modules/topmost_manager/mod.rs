//! # 全局窗口置顶守护器（v0.4.1 · 第五个常驻守护模块）
//!
//! 基于 Win32 原生 API（零重型第三方依赖）实现**轻量可视化窗口置顶管理系统**：
//! 支持 1~9 级优先级（1 级最顶层），经 [`enum_windows`] 轻量枚举候选窗口并产出
//! 纯文本列表（**严禁加载窗口图标**——图标位图会把 UI 图形显存预算打爆），再交由
//! 链式 Z-Order 引擎（[`engine`]）逐级锚定守护。
//!
//! # 模块模型
//!
//! - **运行态（守护）**：`start()` 派生专用原生泵线程（`win32-topmost-pump`）安装
//!   [`SetWinEventHook`]（事件区间 `EVENT_SYSTEM_FOREGROUND` ~
//!   `EVENT_SYSTEM_MINIMIZESTART`，v0.4.1 并入最小化监听）。系统每次前台窗口
//!   切换都向泵线程投递 WinEvent 回调：回调只记录“被激活的窗口”并（重新）武装
//!   一枚 15ms 一次性 `SetTimer` 防抖（v0.4.1 起受管窗口被最小化时改投递
//!   `PostMessageW(WM_APP)` 定制消息，泵线程立即执行自动解置顶），随后立即返回
//!   （回调内绝无重活）。泵线程收到 `WM_TIMER` 时执行**前台抢占纠偏**——若用户
//!   激活了受管置顶窗口，系统已把它顶到绝对顶层，守护按
//!   [`engine::plan_shield_refresh`] 把其前方的 1 / 2 级窗口沿链序重刷（全程
//!   `SWP_NOACTIVATE`，**绝不抢占用户焦点**）。无事时泵线程阻塞于 `GetMessageW`，
//!   CPU 占用为零；
//! - **停止态**：`stop()` 卸载钩子 / 计时器并 Join 泵线程。已置顶窗口的
//!   `WS_EX_TOPMOST` 属性由操作系统在会话期间持续保持——停止只冻结“守护”，
//!   不撤销用户已建立的置顶（避免全量关闭误伤用户手工置顶）；
//! - **窗口级操作与运行态正交**：弹窗内的置顶开关 / 优先级步进器（[`apply_pin`] /
//!   [`apply_unpin`] / [`set_priority`](Self::set_priority)）在模块运行与否时都可
//!   执行——直接 `SetWindowPos` 即时生效并持久化规则记忆。UI 侧（v0.4.1）在模块
//!   「已停止」时以警示条 + 控件禁用联锁，避免用户在停止态无效操作。
//!
//! # 并发模型（延续既有模块铁律）
//!
//! - 受管条目收敛在 [`TopmostState`]：写入为短临界 std Mutex（条目数个位到数十，
//!   纯向量操作微秒级），读取克隆快照后在锁外完成全部 Win32 调用与比较；
//! - 泵线程（Windows 专属）经 `CancellationToken` + `WM_QUIT` 握手停机：`stop()`
//!   先 cancel 广播意图，再向已建消息队列的泵线程定向投递 `WM_QUIT` 唤醒
//!   `GetMessageW`，超时 Join 确保 `UnhookWinEvent` 在安装线程执行；
//! - 前台纠偏与 UI 操作并发收敛：纠偏只对“已受管窗口”重定位、不增删条目；
//!   窗口消亡清理以 `IsWindow` 前置校验 + 惰性清扫完成。
//!
//! # 规则记忆与优先级记忆（v0.4.1 强化）
//!
//! 每次置顶 / 解除 / 改级落定后，模块把当前受管条目序列化为
//! [`PinnedRule`](crate::config::PinnedRule) 列表（进程名 + 标题原文 + 优先级 +
//! enabled），经单写者通道交给装配层持久化（`[topmost_manager]` 节）。
//! **优先级记忆**：用户对某进程窗口设定过的优先级（1~9）以 `process_name` 为主键
//! 记入 `memorized` 映射并在落盘时合并为 `enabled = false` 的记忆规则——解除置顶
//! 不抹除记忆；下次枚举 / 重启后同一进程的未受管窗口自动回填该历史优先级
//! （[`TopmostManagerModule::remembered_priority`]）。
//! `start()` 时按装配期注入的规则恢复：重新枚举窗口 → 进程名（大小写不敏感）+
//! 标题**子串**双向匹配 → 恢复置顶与优先级。
//!
//! # 最小化自动解置顶（v0.4.1）
//!
//! 泵线程经 `EVENT_SYSTEM_MINIMIZESTART` 感知受管窗口被最小化：立即对该窗口执行
//! [`apply_unpin`]（先移除受管条目 → `SetWindowPos(HWND_NOTOPMOST)` → 剩余链条
//! 重排，杜绝 15ms 纠偏定时器恢复尸体）并经事件总线发布 Toast
//! “窗口已最小化，自动取消置顶”。
//!
//! # UIPI 拦截与审计
//!
//! `SetWindowPos` 返回 `ERROR_ACCESS_DENIED`（目标窗口完整性级别高于本进程）时，
//! 引擎上报 [`engine::Win32Error::AccessDenied`]：UI 触发的操作以方法返回值上抛
//! （装配层写审计 + 弹 Toast“目标窗口具备高特权，请提权运行 TLToolBox”）；守护
//! 线程触发的纠偏失败经可选的 [`EventBus`] 发布 [`AppEvent::ToastRequested`]。

pub mod engine;
pub mod enum_windows;

use crate::bus::{AppEvent, EventBus};
use crate::config::PinnedRule;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

#[cfg(windows)]
use std::time::Duration;
#[cfg(windows)]
use tokio::sync::oneshot;

#[cfg(windows)]
use windows::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    System::Threading::GetCurrentThreadId,
    UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK},
    UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, KillTimer, PeekMessageW, PostMessageW, PostThreadMessageW,
        SetTimer, TranslateMessage, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZESTART, MSG,
        PM_NOREMOVE, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_QUIT, WM_TIMER,
    },
};

/// 前置优先级默认值（用户新建置顶 / UI 行未指定时使用；1~9，1 最顶层）。
pub const DEFAULT_PRIORITY: u8 = 3;
/// 前台抢占纠偏的防抖窗口（15ms，落在任务书 10~20ms 区间；合并同一次用户操作的
/// 连续前台事件为一次纠偏）。
#[cfg(windows)]
const FOREGROUND_DEBOUNCE_MS: u32 = 15;
/// 防抖一次性计时器 ID（`SetTimer` / `WM_TIMER` 载荷；'TL' 占位）。
#[cfg(windows)]
const SHIELD_TIMER_ID: usize = 0x544C;
/// 「用户手动取消置顶」兜底对账计时器 ID（v0.6.1 · M4①；与防抖计时器不同 ID）。
#[cfg(windows)]
const UNPIN_RECONCILE_TIMER_ID: usize = 0x544D;
/// 兜底对账周期（8s）：用户取消置顶后，最迟在此时长内被识别并持久化。
///
/// 取值权衡：前台切换纠偏已覆盖绝大多数场景，本计时器只是"用户取消后长时间不切
/// 前台"的兜底；8s 的唤醒成本可忽略（一次只读的 `GetWindowLongPtrW` 查询）。
#[cfg(windows)]
const UNPIN_RECONCILE_MS: u32 = 8_000;
/// 停机协议中 Join 原生泵线程的等待上限（与弹窗拦截模块同一纪律）。
#[cfg(windows)]
const PUMP_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
/// 泵线程定制的「受管窗口被最小化 → 自动解置顶」消息（v0.4.1）。
///
/// 落在 `WM_APP`（0x8000 ~ 0xBFFF）应用保留区，绝无系统冲突；WinEvent 回调经
/// `PostMessageW(None, …)` 投递到泵线程队列，消息泵收到后执行解置顶（回调内
/// 不做任何重活）。
#[cfg(windows)]
const MSG_MINIMIZE_UNPIN: u32 = 0x8000; // = WM_APP

// ---------------------------------------------------------------------------
// 进程内活动置顶窗口条目（模块核心数据形态）
// ---------------------------------------------------------------------------

/// 内存中激活的置顶窗口条目。
///
/// 与持久化 [`PinnedRule`] 互转：`hwnd` / `title` 为进程内实时字段；落盘只保留
/// （进程名, 标题原文, 优先级, enabled），启动恢复以枚举窗口 + 匹配回到本条形态。
#[derive(Debug, Clone)]
pub struct ActivePinnedWindow {
    /// 窗口句柄裸值（还原见 [`enum_windows::to_hwnd`]）。
    pub hwnd: isize,
    /// 所属进程的可执行文件名（`notepad.exe`）。
    pub process_name: String,
    /// 窗口标题（置顶时刻采集；随 UI 刷新更新）。
    pub title: String,
    /// 置顶优先级（1~9，1 位于最顶层）。
    pub priority: u8,
    /// 置顶开关（本模块只维护置顶中的条目，恒为 `true`；字段保留以对齐配置结构）。
    pub enabled: bool,
}

impl ActivePinnedWindow {
    /// 收敛为一条持久化规则（置顶 / 解除 / 改级后调用，供装配层落盘）。
    pub fn to_rule(&self) -> PinnedRule {
        PinnedRule {
            process_name: self.process_name.clone(),
            // 标题模式以窗口标题**原文**落盘；恢复时按子串语义放宽匹配（标题
            // 前后缀变化如路径前缀追加仍可命中）。
            title_pattern: self.title.clone(),
            priority: engine::clamp_priority(self.priority),
            enabled: self.enabled,
        }
    }
}

// ---------------------------------------------------------------------------
// 状态容器（受管条目 + 守护失败一次性标志）
// ---------------------------------------------------------------------------

/// 「用户已手动取消置顶」的移除判定（纯逻辑，可离线单测）。
///
/// - 窗口**存活**但不再置顶 → 移除（用户经原生菜单取消了置顶；M4①）；
/// - 仍置顶 → 保留；
/// - 窗口已消亡 → **不移除**（由 `sweep_dead` 统一负责，避免两处清理互相掩盖）。
fn should_drop_user_unpinned(alive: bool, topmost: bool) -> bool {
    alive && !topmost
}

/// 模块并发状态：受管条目（短临界 std Mutex 保护）。
struct TopmostState {
    /// 受管条目向量，保持**插入顺序**；任何“按优先级排序的链序 / UI 顺序”都经
    /// [`TopmostState::sorted_snapshot`] 派生——排序键为（优先级升序, 插入序稳定），
    /// 同优先级内先置顶者恒在更上层，语义确定且可测。
    entries: StdMutex<Vec<ActivePinnedWindow>>,
    /// 最近一次链式守护是否出现失败（含 UIPI 拦截）；拉取即清（一次性告警）。
    guard_failure: StdMutex<bool>,
}

impl Default for TopmostState {
    fn default() -> Self {
        Self {
            entries: StdMutex::new(Vec::new()),
            guard_failure: StdMutex::new(false),
        }
    }
}

impl TopmostState {
    /// 受管条目快照（插入序拷贝，锁外使用）。
    fn snapshot(&self) -> Vec<ActivePinnedWindow> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// 按（优先级升序, 插入序稳定）排序的链序快照（链式应用 / UI 列表权威顺序）。
    fn sorted_snapshot(&self) -> Vec<ActivePinnedWindow> {
        let mut entries = self.snapshot();
        entries.sort_by_key(|entry| engine::clamp_priority(entry.priority));
        entries
    }

    fn find(&self, hwnd: isize) -> Option<ActivePinnedWindow> {
        self.snapshot().into_iter().find(|entry| entry.hwnd == hwnd)
    }

    /// 追加或原位更新一条受管条目（新条目追加在向量末尾 = 同优先级组内最下）。
    fn upsert(&self, entry: ActivePinnedWindow) {
        let mut guard = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        match guard
            .iter_mut()
            .find(|existing| existing.hwnd == entry.hwnd)
        {
            Some(existing) => *existing = entry,
            None => guard.push(entry),
        }
    }

    /// 移除一条受管条目（解除置顶 / 窗口消亡清理）。
    fn remove(&self, hwnd: isize) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|entry| entry.hwnd != hwnd);
    }

    /// 批量移除已消亡窗口的条目；返回是否发生清理。
    fn sweep_dead(&self) -> bool {
        let mut guard = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let before = guard.len();
        guard.retain(|entry| engine::is_window_alive(entry.hwnd));
        guard.len() != before
    }

    /// 批量移除**用户已手动取消置顶**的窗口条目（v0.6.1 · M4①）。
    ///
    /// 判定：窗口仍存活（消亡条目归 [`Self::sweep_dead`] 管），但
    /// [`engine::is_topmost`] 已为假——说明用户在 Windows 原生窗口菜单里取消过置顶。
    /// 返回被移除的 `(hwnd, 进程名)` 列表（供审计 / Toast 回显）。
    ///
    /// 若不清除，下一次前台切换事件的纠偏路径会把该窗口重新置顶：用户**无法真正
    /// 关闭**置顶，且 UI 仍标记 `topmost = true` 而系统实际已非置顶——界面在撒谎。
    fn sweep_user_unpinned(&self) -> Vec<(isize, String)> {
        let mut guard = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let mut removed = Vec::new();
        guard.retain(|entry| {
            let alive = engine::is_window_alive(entry.hwnd);
            let still_topmost = alive && engine::is_topmost(entry.hwnd);
            if should_drop_user_unpinned(alive, still_topmost) {
                removed.push((entry.hwnd, entry.process_name.clone()));
                return false;
            }
            // 保留：仍存活且仍置顶的条目；消亡条目交给 sweep_dead 处理（本函数不动）。
            true
        });
        removed
    }

    fn note_guard_failure(&self) {
        *self
            .guard_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
    }

    /// 拉取并清除守护失败标志（一次性告警）。
    fn take_guard_failure(&self) -> bool {
        std::mem::take(
            &mut *self
                .guard_failure
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }
}

// ---------------------------------------------------------------------------
// 模块本体
// ---------------------------------------------------------------------------

/// Win32 全局窗口置顶守护模块。
///
/// 生命周期由内部可变性管理（`AtomicBool` + std Mutex + 异步锁），对调度器 / UI
/// 仅暴露共享引用接口，满足 [`super::ToolModule`] 的 `Send + Sync`。
#[derive(Clone)]
pub struct TopmostManagerModule {
    inner: Arc<TopmostManagerInner>,
}

/// 模块内部并发状态。
struct TopmostManagerInner {
    /// 串行化 `start` / `stop` 生命周期变迁。
    lifecycle: AsyncMutex<()>,
    /// 快速查询的运行标志。
    running: AtomicBool,
    /// 活动运行上下文（泵线程 + 停机令牌；仅运行期存在）。
    active: StdMutex<Option<ActiveRun>>,
    /// 受管条目状态容器。
    state: Arc<TopmostState>,
    /// 启动恢复用的置顶规则记忆（装配期注入；每次 `start` 重新恢复）。
    startup_rules: StdMutex<Vec<PinnedRule>>,
    /// 进程级优先级记忆（v0.4.1：`process_name` → 历史设定优先级 1~9）。
    ///
    /// 置顶 / 改级落定时写入，解除置顶**不**抹除；落盘时对「无受管窗口的进程」
    /// 合并为 `enabled = false` 的记忆规则（见 [`TopmostManagerInner::rules_snapshot`]），
    /// 下次枚举 / 重启后同一进程的未受管窗口自动回填该历史优先级。
    memorized: StdMutex<HashMap<String, u8>>,
    /// 规则持久化单写者通道发送端（置顶 / 解除 / 改级后投递最新规则列表）。
    rules_tx: StdMutex<Option<tokio::sync::mpsc::UnboundedSender<Vec<PinnedRule>>>>,
    /// 可选事件总线（守护线程的 UIPI 失败 → `ToastRequested`；构造期装配后不变）。
    bus: StdMutex<Option<EventBus>>,
}

/// 单次运行上下文（Windows：泵线程；非 Windows：虚拟占位）。
struct ActiveRun {
    cancel: CancellationToken,
    #[cfg(windows)]
    thread: Option<std::thread::JoinHandle<()>>,
    #[cfg(windows)]
    thread_id: u32,
    #[cfg(not(windows))]
    _virtual: (),
}

impl TopmostManagerModule {
    /// 以装配期注入的置顶规则记忆构造模块。
    ///
    /// v0.4.1：`pinned_rules` 中的**全部**规则（含 `enabled = false` 的记忆规则）
    /// 同时播种进 `memorized`（进程 → 优先级），供未受管窗口的优先级回填。
    pub fn with_rules(rules: impl IntoIterator<Item = PinnedRule>) -> Self {
        let rules: Vec<PinnedRule> = rules.into_iter().collect();
        // 同进程多条规则时后者覆盖前者（collect 语义：后写先得，迭代序稳定）。
        // v0.6.2（L9）：键统一小写（与运行期写入 / 查询一致）。
        let memorized: HashMap<String, u8> = rules
            .iter()
            .map(|rule| {
                (
                    rule.process_name.to_ascii_lowercase(),
                    engine::clamp_priority(rule.priority),
                )
            })
            .collect();
        Self {
            inner: Arc::new(TopmostManagerInner {
                lifecycle: AsyncMutex::new(()),
                running: AtomicBool::new(false),
                active: StdMutex::new(None),
                state: Arc::new(TopmostState::default()),
                startup_rules: StdMutex::new(rules),
                memorized: StdMutex::new(memorized),
                rules_tx: StdMutex::new(None),
                bus: StdMutex::new(None),
            }),
        }
    }

    /// 绑定事件总线（守护线程的 UIPI 失败提示出口；`None` = 不弹 Toast）。
    pub fn with_bus(self, bus: Option<EventBus>) -> Self {
        if let Some(bus) = bus {
            // 总线在模块构造时装配一次（不可变字段）。
            self.inner
                .bus
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .replace(bus);
        }
        self
    }

    /// 装配规则持久化通道（单写者；置顶 / 解除 / 改级后投递最新 [`PinnedRule`] 列表）。
    pub fn attach_rule_persister(&self, tx: tokio::sync::mpsc::UnboundedSender<Vec<PinnedRule>>) {
        *self
            .inner
            .rules_tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(tx);
    }

    /// 当前受管条目快照（插入序；调试 / 测试用）。
    pub fn pinned(&self) -> Vec<ActivePinnedWindow> {
        self.inner.state.snapshot()
    }

    /// 读取某进程的历史优先级记忆（v0.4.1，`process_name` 大小写敏感主键）。
    ///
    /// 记忆来源为 `with_rules` 播种的 `pinned_rules`（含 `enabled = false` 记忆
    /// 规则）与运行期每次置顶 / 改级的落定写入；装配层在枚举弹窗行时对未受管
    /// 窗口调用本方法回填历史设定值（受管窗口直接取条目的实时优先级）。
    pub fn remembered_priority(&self, process_name: &str) -> Option<u8> {
        // v0.6.2（L9）：键统一小写——写入侧（`memorize_priority` / `with_rules`）
        // 同样小写化，保证与规则匹配的大小写不敏感语义一致。
        let key = process_name.to_ascii_lowercase();
        self.inner
            .memorized
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .copied()
    }

    /// 拉取并清除守护失败标志（装配层轮询消费，触发一次性告警 Toast）。
    pub fn take_guard_failure(&self) -> bool {
        self.inner.state.take_guard_failure()
    }

    /// 枚举当前全部候选窗口（Win32 同步调用；UI 线程经 `spawn_blocking` 使用）。
    pub fn enumerate_candidates() -> Vec<enum_windows::WindowInfo> {
        enum_windows::enumerate_top_level_windows()
    }

    // -------------------------------------------------------------------
    // 窗口级实时操作（与运行态正交；主装配层在其上写审计 + Toast 反馈）
    // -------------------------------------------------------------------

    /// 把窗口置顶并纳入受管（链式重刷使新窗口落在正确链位）。
    ///
    /// `priority` 越界自动夹紧 1~9；成功后把（进程名 → 优先级）写入记忆并持久化
    /// 规则。错误上抛：
    /// - [`engine::Win32Error::AccessDenied`]：UIPI 拦截（提示用户提权）；
    /// - [`engine::Win32Error::InvalidWindow`]：窗口已销毁。
    pub fn apply_pin(&self, hwnd: isize, priority: u8) -> Result<(), engine::Win32Error> {
        let priority = engine::clamp_priority(priority);
        // 1) 前置校验 + 立即应用置顶（失败上抛：窗口失效 / UIPI 拦截 / 其他）。
        engine::set_topmost(hwnd)?;

        // 2) 采集实时元数据并入状态（进程名 / 标题以置顶时刻为准）。
        let (process_name, title) =
            window_identity(hwnd).unwrap_or_else(|| ("<unknown.exe>".to_string(), String::new()));
        // 3) 写入进程级优先级记忆（v0.4.1：解除置顶后仍可回填 / 持久化）。
        self.memorize_priority(&process_name, priority);
        self.upsert_entry(ActivePinnedWindow {
            hwnd,
            process_name,
            title,
            priority,
            enabled: true,
        });

        // 4) 沿整条优先级链重刷，保证新窗口落在正确的链位（含未运行态——
        //    链式重刷等价于 set_topmost + 锚定关系，即时生效）。
        self.refresh_chain();
        Ok(())
    }

    /// 取消窗口置顶（v0.4.1 根治「取消失效 / 死尸复活」的时序纪律）。
    ///
    /// 执行顺序**不可颠倒**：
    /// 1. 先把 HWND 从内存受管活跃集合（`active_pinned` / 规则列表）**彻底移除**
    ///    （含持久化）——即使随后 Win32 调用失败（窗口已消亡），该窗口也绝不再
    ///    滞留受管集，15ms 纠偏定时器 /
    ///    前台抢占纠偏拿到的永远是移除后的快照，无法复活死尸；
    /// 2. 调用 `SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0,
    ///    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE)` 归还普通层（引擎
    ///    [`engine::set_notopmost`] 前置 `IsWindow` 校验）；
    /// 3. **无论结果如何**都强制重刷剩余链条：移除该窗口后，其余受管窗口的相对
    ///    锚定关系必须重新收敛（原「无需整链重刷」的假设在窗口消亡 / 最小化
    ///    自动解置顶等路径上不成立）。
    ///
    /// 优先级记忆不受影响（解除置顶不抹除进程级历史设定）。
    pub fn apply_unpin(&self, hwnd: isize) -> Result<(), engine::Win32Error> {
        // 1) 先移除（不可前置 Win32 校验：窗口已消亡也必须清出受管集）。
        self.remove_entry(hwnd);
        // 2) Win32 层取消置顶（窗口已消亡 → InvalidWindow，供调用方提示）。
        let outcome = engine::set_notopmost(hwnd);
        // 3) 强制剩余链条重排（见函数文档）。
        self.refresh_chain();
        outcome
    }

    /// 修改窗口优先级：更新条目标记 + 写入进程级记忆后沿新优先级整链重刷
    /// （免去先解后置的闪烁）。
    pub fn set_priority(&self, hwnd: isize, priority: u8) -> Result<(), engine::Win32Error> {
        let priority = engine::clamp_priority(priority);
        let Some(entry) = self.inner.state.find(hwnd) else {
            return Err(engine::Win32Error::InvalidWindow);
        };
        if !engine::is_window_alive(hwnd) {
            self.remove_entry(hwnd);
            return Err(engine::Win32Error::InvalidWindow);
        }
        let mut updated = entry.clone();
        updated.priority = priority;
        // v0.4.1：进程级优先级记忆紧随改级落定（解除置顶后仍可回填）。
        self.memorize_priority(&updated.process_name, priority);
        self.upsert_entry(updated);
        self.refresh_chain();
        Ok(())
    }

    // -------------------------------------------------------------------
    // 内部工具
    // -------------------------------------------------------------------

    fn upsert_entry(&self, entry: ActivePinnedWindow) {
        self.inner.state.upsert(entry);
        self.persist_rules();
    }

    fn remove_entry(&self, hwnd: isize) {
        self.inner.state.remove(hwnd);
        self.persist_rules();
    }

    /// 写入进程级优先级记忆（v0.4.1；不触发持久化，由随后的 persist 一并落盘）。
    ///
    /// v0.6.2（L9）：键统一小写，与 [`Self::remembered_priority`] 的读取语义一致。
    fn memorize_priority(&self, process_name: &str, priority: u8) {
        self.inner
            .memorized
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                process_name.to_ascii_lowercase(),
                engine::clamp_priority(priority),
            );
    }

    /// 由「受管条目 + 进程级记忆」收敛出完整规则列表（v0.4.1 语义）。
    ///
    /// - 受管条目 → `enabled = true` 规则（标题原文落盘，供启动恢复子串匹配）；
    /// - 无受管窗口的进程 → `enabled = false` 的**记忆规则**（空标题模式 = 仅进程
    ///   名主键；启动恢复跳过，仅承担优先级回填——见 [`Self::remembered_priority`]）。
    fn rules_snapshot(&self) -> Vec<PinnedRule> {
        let managed = self.inner.state.snapshot();
        let memorized = self
            .inner
            .memorized
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut rules: Vec<PinnedRule> = managed.iter().map(ActivePinnedWindow::to_rule).collect();
        let managed_processes: std::collections::HashSet<&str> = managed
            .iter()
            .map(|entry| entry.process_name.as_str())
            .collect();
        for (process_name, priority) in memorized {
            if !managed_processes.contains(process_name.as_str()) {
                rules.push(PinnedRule {
                    process_name,
                    title_pattern: String::new(),
                    priority: engine::clamp_priority(priority),
                    enabled: false,
                });
            }
        }
        rules
    }

    /// 把最新规则列表投递给持久化通道（若有装配；单写者通道保证最后写入为
    /// 最后一次操作）。
    fn persist_rules(&self) {
        let rules = self.rules_snapshot();
        let tx = self
            .inner
            .rules_tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(tx) = tx {
            // v0.6.2（L10）：持久化失败不再静默——发送端被丢弃 / 接收端已关闭时
            // 至少留痕，否则"置顶规则未落盘"这一事实用户无从得知（下次启动丢失）。
            if tx.send(rules).is_err() {
                tracing::warn!(
                    target: "topmost_manager",
                    "置顶规则持久化失败：单写者通道已关闭（规则仅保留在内存态）"
                );
            }
        }
    }

    /// 把 UIPI 拦截失败转成 Toast 请求（守护线程路径；UI 路径由调用方直接 Toast）。
    fn report_denied(&self, hwnds: &[isize]) {
        let bus = self
            .inner
            .bus
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(bus) = bus {
            let names: Vec<String> = hwnds
                .iter()
                .take(2)
                .map(|hwnd| format!("0x{hwnd:X}"))
                .collect();
            bus.publish(AppEvent::ToastRequested(format!(
                "窗口置顶被系统拒绝（目标窗口具备高特权），请提权运行 TLToolBox（{}）",
                names.join("、")
            )));
        }
    }

    /// 沿整条优先级链重刷全部受管窗口的 Z-Order（含消亡清理 / UIPI 失败上报）。
    fn refresh_chain(&self) {
        let stats = chain_stats_from_entries(&self.inner.state.sorted_snapshot());
        self.handle_chain_stats(&stats);
    }

    /// 前台抢占纠偏：用户激活受管窗口后，重刷其前方 1 / 2 级窗口（泵线程调用）。
    ///
    /// # M4①：纠偏前的「用户已取消置顶」对账
    /// 用户可经 Windows 原生窗口菜单自行取消置顶。若不先对账，本方法会立刻把窗口
    /// **重新置顶**（用户无法真正关闭），且 UI 仍标记为已置顶（界面撒谎）。因此每次
    /// 纠偏前先扫一遍受管条目：窗口存活但 `WS_EX_TOPMOST` 已不置位者，判定为「用户
    /// 手动取消」→ 惰性移除条目 + 持久化规则快照（配置与 UI 随即收敛到事实）。
    fn shield_refresh(&self, activated: isize) {
        self.reconcile_user_unpin();

        let entries = self.inner.state.sorted_snapshot();
        let chain = to_chain_entries(&entries);
        let Some(plan) = engine::plan_shield_refresh(&chain, activated) else {
            return;
        };
        let stats = engine::apply_plan(&plan);
        self.handle_chain_stats(&stats);
    }

    /// 受管条目与系统真实置顶状态的惰性对账（M4①）。
    ///
    /// 返回本次移除的条目数；发生移除时持久化规则并写审计 / 日志，让「配置事实源」
    /// 与「UI 列表」同一步收敛（否则下次启动会按旧规则重新置顶）。
    fn reconcile_user_unpin(&self) -> usize {
        let removed = self.inner.state.sweep_user_unpinned();
        if removed.is_empty() {
            return 0;
        }
        self.persist_rules();
        for (hwnd, process_name) in &removed {
            tracing::info!(
                target: "topmost_manager",
                "检测到用户已手动取消置顶（HWND: 0x{hwnd:X} 进程: {process_name}），已移除受管条目并持久化规则"
            );
        }
        removed.len()
    }

    /// 统一次链式重刷结果的统计处理：消亡清理 + 失败上报。
    fn handle_chain_stats(&self, stats: &engine::ChainApplyStats) {
        if stats.invalid > 0 {
            // 引擎已跳过失效窗口：从状态中清除，防止条目滞留。
            if self.inner.state.sweep_dead() {
                self.persist_rules();
            }
        }
        if stats.denied > 0 {
            self.inner.state.note_guard_failure();
            self.report_denied(&stats.denied_hwnds);
        }
        if stats.failed > 0 {
            self.inner.state.note_guard_failure();
        }
    }

    /// 受管窗口被最小化 → 立即自动解除置顶（v0.4.1；泵线程经
    /// `EVENT_SYSTEM_MINIMIZESTART` 投递 `MSG_MINIMIZE_UNPIN` 后调用）。
    ///
    /// 只干预**受管**窗口（非受管的最小化不产生任何动作）；解置顶走
    /// [`Self::apply_unpin`]（先移除受管条目 → `SetWindowPos(HWND_NOTOPMOST)` →
    /// 剩余链条重排），随后经事件总线发布 Toast“窗口已最小化，自动取消置顶”。
    /// 窗口在解置顶前已消亡（`InvalidWindow`）同样视为解除完成——条目已被移除。
    fn handle_minimized(&self, hwnd: isize) {
        let Some(entry) = self.inner.state.find(hwnd) else {
            return; // 非受管窗口：不干预
        };
        let process_name = entry.process_name.clone();
        match self.apply_unpin(hwnd) {
            Ok(()) | Err(engine::Win32Error::InvalidWindow) => {
                self.notify_minimized_unpin(&process_name);
            }
            Err(other) => {
                tracing::warn!(
                    target: "topmost_manager",
                    "最小化自动解置顶失败（HWND: 0x{hwnd:X} 进程: {process_name}）: {other}"
                );
            }
        }
    }

    /// 经事件总线发布「最小化自动取消置顶」Toast（泵线程路径；UI 由
    /// 总线 → UI 桥接层展示）。
    fn notify_minimized_unpin(&self, process_name: &str) {
        let bus = self
            .inner
            .bus
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(bus) = bus {
            bus.publish(AppEvent::ToastRequested(format!(
                "窗口已最小化，自动取消置顶（{process_name}）"
            )));
        }
        tracing::info!(
            target: "topmost_manager",
            "受管窗口 {process_name} 已最小化，自动取消置顶"
        );
    }

    /// 启动恢复：按装配期注入的规则记忆重新枚举并置顶。
    fn restore_from_rules(&self) {
        let rules = self
            .inner
            .startup_rules
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if rules.is_empty() {
            return;
        }
        let candidates = enum_windows::enumerate_top_level_windows();
        for rule in &rules {
            if !rule.enabled {
                continue;
            }
            for window in &candidates {
                if window.topmost {
                    continue; // 已被其它工具 / 本模块置顶：跳过重复置顶
                }
                if rule_matches_window_info(rule, window) {
                    if engine::set_topmost(window.hwnd).is_ok() {
                        self.upsert_entry(ActivePinnedWindow {
                            hwnd: window.hwnd,
                            process_name: window.process_name.clone(),
                            title: window.title.clone(),
                            priority: rule.priority,
                            enabled: true,
                        });
                    }
                    break; // 一条规则至多恢复一个窗口
                }
            }
        }
    }

    // -------------------------------------------------------------------
    // 生命周期实现
    // -------------------------------------------------------------------

    /// Windows 实现：派生专用原生泵线程安装前台事件钩子。
    #[cfg(windows)]
    async fn start_native(&self) -> Result<(), super::ModuleError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        if self.inner.running.load(Ordering::Acquire) {
            return Ok(()); // 已在运行：幂等
        }

        // 先清一遍已消亡条目（恢复置顶前）。
        let _ = self.inner.state.sweep_dead();

        let cancel = CancellationToken::new();
        let (ready_tx, ready_rx) = oneshot::channel::<Result<u32, String>>();

        let pump_inner = Arc::clone(&self.inner);
        let thread = std::thread::Builder::new()
            .name("win32-topmost-pump".to_string())
            .spawn(move || pump_thread_main(ready_tx, pump_inner))
            .map_err(|e| -> super::ModuleError { Box::new(e) })?;

        // 等待原生线程完成“建队列 + 装钩子”握手；失败回收线程并上报。
        let thread_id = match ready_rx.await {
            Ok(Ok(thread_id)) => thread_id,
            Ok(Err(reason)) => {
                let _ = thread.join();
                return Err(reason.into());
            }
            Err(_) => {
                let _ = thread.join();
                return Err("窗口置顶模块启动握手被中断".into());
            }
        };

        // 钩子就绪后恢复规则记忆并整链重刷一次（让运行态与状态容器收敛）。
        self.restore_from_rules();
        self.refresh_chain();

        self.inner.running.store(true, Ordering::Release);
        *self
            .inner
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(ActiveRun {
            cancel,
            thread_id,
            thread: Some(thread),
        });

        tracing::info!(
            target: "topmost_manager",
            "全局窗口置顶守护已启动（泵线程 {thread_id}，受管窗口 {} 个）",
            self.inner.state.snapshot().len()
        );
        Ok(())
    }

    /// 非 Windows 兜底实现：仅维持虚拟生命周期（供跨平台编译与调度联调）。
    #[cfg(not(windows))]
    async fn start_virtual(&self) -> Result<(), super::ModuleError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        if self.inner.running.load(Ordering::Acquire) {
            return Ok(()); // 幂等
        }
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        tokio::spawn(async move {
            tracing::warn!(target: "topmost_manager", "非 Windows 平台：窗口置顶模块仅维持虚拟生命周期");
            token.cancelled().await;
        });
        self.inner.running.store(true, Ordering::Release);
        *self
            .inner
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(ActiveRun {
            cancel,
            _virtual: (),
        });
        Ok(())
    }

    /// 停止模块（Windows / 非 Windows 共用）。
    async fn stop_impl(&self) -> Result<(), super::ModuleError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        if !self.inner.running.load(Ordering::Acquire) {
            return Ok(()); // 幂等
        }

        let Some(mut run) = self
            .inner
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        else {
            self.inner.running.store(false, Ordering::Release);
            return Ok(());
        };

        // 1) 广播停机意图。
        run.cancel.cancel();

        #[cfg(windows)]
        {
            // 2) 向泵线程消息队列定向投递 WM_QUIT，唤醒阻塞中的 GetMessageW。
            // SAFETY: WM_QUIT 为已定义消息常量，投递失败仅返回错误码，无 UB。
            let _ = unsafe { PostThreadMessageW(run.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };

            // 3) 超时 Join：确保 UnhookWinEvent / KillTimer 已在安装线程执行完毕。
            //    （`take()` 而非移动：超时路径要把 `run` 放回 `active` 槽位。）
            if let Some(thread) = run.thread.take() {
                let join_result = tokio::time::timeout(
                    PUMP_JOIN_TIMEOUT,
                    tokio::task::spawn_blocking(move || thread.join()),
                )
                .await;
                match join_result {
                    Ok(Ok(Ok(()))) => {
                        tracing::debug!(target: "topmost_manager", "泵线程已退出，前台事件钩子已卸载");
                    }
                    Ok(Ok(Err(panic))) => {
                        let payload = panic
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "未知 panic 载荷".to_string());
                        tracing::error!(target: "topmost_manager", "泵线程异常终止: {payload}");
                        self.inner.running.store(false, Ordering::Release);
                        return Err(format!("窗口置顶泵线程异常终止: {payload}").into());
                    }
                    Ok(Err(task_err)) => {
                        tracing::error!(target: "topmost_manager", "Join 阻塞任务异常终止: {task_err}");
                        // Join 任务自身异常（理论不可达）：同样按"停止未完成"处理，
                        // 不翻转 running（M4②）。
                        return Err(format!("窗口置顶泵线程收尾任务异常: {task_err}").into());
                    }
                    Err(_elapsed) => {
                        // M4②：超时后**不得**置 `running = false`。旧实现在此仅记
                        // 日志便继续把标志清零，而旧泵线程尚未退出、WinEvent 钩子
                        // 与对账计时器仍在生效——再次 `start` 会安装第二套
                        // `SetWinEventHook` + 计时器，造成钩子泄漏与重复纠偏。
                        // 现保持「停止中」语义：返回错误并保留 `running = true`，
                        // 由调用方提示重试；`active` 槽位已被 `take()` 清空，但
                        // 旧线程仍在运行，故把句柄**放回**以维持可再次停止。
                        tracing::error!(
                            target: "topmost_manager",
                            "泵线程未在 {PUMP_JOIN_TIMEOUT:?} 内退出：保持运行标志（停止未完成），可重试停止"
                        );
                        *self
                            .inner
                            .active
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner) = Some(run);
                        return Err(format!(
                            "窗口置顶守护停止超时（泵线程 {PUMP_JOIN_TIMEOUT:?} 内未退出），请稍后重试"
                        )
                        .into());
                    }
                }
            }
        }

        self.inner.running.store(false, Ordering::Release);
        tracing::info!(
            target: "topmost_manager",
            "全局窗口置顶守护已停止（已置顶窗口的置顶属性由系统持续保持，不主动撤销）"
        );
        Ok(())
    }
}

impl Default for TopmostManagerModule {
    fn default() -> Self {
        Self::with_rules(Vec::<PinnedRule>::new())
    }
}

#[async_trait]
impl super::ToolModule for TopmostManagerModule {
    fn id(&self) -> &'static str {
        "topmost_manager"
    }

    fn display_name(&self) -> &'static str {
        "全局窗口置顶"
    }

    fn description(&self) -> &'static str {
        "多窗口优先级分级置顶守护"
    }

    async fn start(&self) -> Result<(), super::ModuleError> {
        #[cfg(windows)]
        {
            self.start_native().await
        }
        #[cfg(not(windows))]
        {
            self.start_virtual().await
        }
    }

    async fn stop(&self) -> Result<(), super::ModuleError> {
        self.stop_impl().await
    }

    fn is_running(&self) -> bool {
        self.inner.running.load(Ordering::Acquire)
    }
}

// ---------------------------------------------------------------------------
// 跨平台纯函数（可单测）
// ---------------------------------------------------------------------------

/// 采集窗口的（进程名, 标题）身份信息（Windows 平台；失败回退占位）。
fn window_identity(hwnd: isize) -> Option<(String, String)> {
    #[cfg(windows)]
    {
        let raw = enum_windows::to_hwnd(hwnd);
        let title = unsafe { enum_windows::window_title_raw(raw) };
        let process = unsafe { enum_windows::process_name_raw(raw) };
        Some((process, title))
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        None
    }
}

/// 按一条规则匹配窗口（进程名大小写不敏感 + 标题双向子串）。
///
/// # 纯函数（无 FFI），供恢复与单测共用
pub fn rule_matches_window(
    process_name: &str,
    title_pattern: &str,
    enabled: bool,
    window_process: &str,
    window_title: &str,
) -> bool {
    if !enabled {
        return false;
    }
    if !window_process.eq_ignore_ascii_case(process_name) {
        return false;
    }
    let needle = title_pattern.trim().to_lowercase();
    let title = window_title.trim().to_lowercase();
    if needle.is_empty() {
        // 空标题模式 = 仅按进程名匹配（标题不参与判定）。
        return true;
    }
    if title.is_empty() {
        // 空标题窗口在枚举期已被过滤；此处防御性拒绝（无法子串匹配）。
        return false;
    }
    // 正向子串：窗口标题可能随会话上下文增删前后缀（`- 已保存`、`* 未保存` 等）。
    if title.contains(&needle) {
        return true;
    }
    // v0.6.2（L11）：反向匹配从「双向任意子串」收紧为**前缀关系**且要求标题具备
    // 最低信息量（≥ 4 字符）。旧实现 `needle.contains(&title)` 会让标题极短（如
    // "OK"、"1"）的窗口命中一条毫不相关的长模式——恢复阶段据此置顶错误窗口。
    // 前缀关系仍保留原意图（记忆的模式含当前标题为前缀的更短形态）。
    const MIN_REVERSE_TITLE_CHARS: usize = 4;
    title.chars().count() >= MIN_REVERSE_TITLE_CHARS && needle.starts_with(&title)
}

/// 进程名 + 标题（标题允许空）是否构成有效恢复匹配。
#[cfg(windows)]
fn rule_matches_window_info(rule: &PinnedRule, window: &enum_windows::WindowInfo) -> bool {
    rule_matches_window(
        &rule.process_name,
        &rule.title_pattern,
        rule.enabled,
        &window.process_name,
        &window.title,
    )
}

/// 由受管条目快照构造链式应用条目（插入序索引为同优先级内的稳定次序）。
fn to_chain_entries(entries: &[ActivePinnedWindow]) -> Vec<engine::ChainEntry> {
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| engine::ChainEntry {
            hwnd: entry.hwnd,
            priority: entry.priority,
            seq: index as u64,
        })
        .collect()
}

/// 按当前受管条目执行整链重刷并返回统计（供模块方法直接消费）。
fn chain_stats_from_entries(entries: &[ActivePinnedWindow]) -> engine::ChainApplyStats {
    engine::apply_chain(&to_chain_entries(entries))
}

// ---------------------------------------------------------------------------
// Windows 泵线程（钩子 + 消息泵）
// ---------------------------------------------------------------------------

// 最近一次前台事件激活的窗口（回调写入、泵线程在 `WM_TIMER` 防抖到期后消费；
// 全部发生在同一条泵线程上，`Cell` 即线程安全）。最小化事件不走本槽位——它经
// `PostMessageW` 定制消息直达泵线程（见 [`MSG_MINIMIZE_UNPIN`]）。
#[cfg(windows)]
thread_local! {
    static RECENT_FOREGROUND: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// 泵线程主体：装钩 + 消息泵（运行于专用原生线程；`inner` 为模块内部状态，
/// 由闭包捕获随线程存活）。
///
/// # WinEvent 第一性原则（与 popup_blocker 一致）
/// `SetWinEventHook` 回调依赖线程消息队列，钩子与消息泵须整体隔离到专用原生
/// 线程；Tokio 侧只做生命周期编排。
#[cfg(windows)]
fn pump_thread_main(
    ready_tx: oneshot::Sender<Result<u32, String>>,
    inner: Arc<TopmostManagerInner>,
) {
    // SAFETY:
    // - 本函数整体运行在 std::thread 派生的专用原生线程；
    // - PeekMessageW 仅用于建立线程消息队列（取不到消息也无副作用）；
    // - SetWinEventHook(WINEVENT_OUTOFCONTEXT) 把回调挂到本线程队列，由紧随其后
    //   的 GetMessageW / DispatchMessageW 标准消息泵承载派发；
    // - 退出路径在同线程卸载钩子 / 计时器，满足线程亲和约束。
    unsafe {
        let mut seed = MSG::default();
        let _ = PeekMessageW(&mut seed, None, 0, 0, PM_NOREMOVE);

        let callback_module: Option<&windows::Win32::Foundation::HMODULE> = None;
        // v0.4.1：事件区间扩为 FOREGROUND ~ MINIMIZESTART（0x0003 ~ 0x0016），
        // 同一钩子同时承载「前台抢占纠偏」与「最小化自动解置顶」两条事件流。
        let hook = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_MINIMIZESTART,
            callback_module,
            Some(win_event_proc as WinEventCallback),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        if hook.0.is_null() {
            let _ = ready_tx.send(Err(
                "SetWinEventHook 安装失败（前台 / 最小化事件钩子）".to_string()
            ));
            return;
        }

        let thread_id = GetCurrentThreadId();
        if ready_tx.send(Ok(thread_id)).is_err() {
            let _ = UnhookWinEvent(hook);
            return;
        }
        tracing::info!(target: "topmost_manager", "前台 / 最小化事件钩子已就绪（泵线程 {thread_id}）");

        // v0.6.1（M4①）兜底对账计时器：前台纠偏只在切换前台时触发，若用户取消
        // 置顶后长时间不切前台，UI 会一直显示"已置顶"。以固定周期做一次 Z-Order
        // 对账，保证「用户手动取消」最迟在一个周期内被识别并持久化。
        let _ = SetTimer(None, UNPIN_RECONCILE_TIMER_ID, UNPIN_RECONCILE_MS, None);

        // 消息泵：WM_QUIT → 退出；WM_TIMER(防抖) → 前台抢占纠偏；
        // WM_TIMER(对账) → 用户手动取消置顶的兜底检测；MSG_MINIMIZE_UNPIN →
        // 受管窗口最小化自动解置顶；其余消息（含 WinEvent 回调的派发）走
        // Translate + Dispatch。
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_QUIT {
                break;
            }
            if msg.message == WM_TIMER && msg.wParam.0 == UNPIN_RECONCILE_TIMER_ID {
                // 周期性对账：识别用户经原生菜单取消置顶的窗口并移除条目（M4①）。
                let module = TopmostManagerModule {
                    inner: Arc::clone(&inner),
                };
                module.reconcile_user_unpin();
                continue;
            }
            if msg.message == WM_TIMER && msg.wParam.0 == SHIELD_TIMER_ID {
                // 防抖到期：读取回调记录的“最近激活窗口”并执行纠偏。
                let activated = RECENT_FOREGROUND.with(|cell| cell.replace(0));
                let _ = KillTimer(None, SHIELD_TIMER_ID);
                if activated != 0 {
                    // 纠偏运行在泵线程上（绝不在 WinEvent 回调内做重活）。
                    let module = TopmostManagerModule {
                        inner: Arc::clone(&inner),
                    };
                    module.shield_refresh(activated as isize);
                }
                continue;
            }
            if msg.message == MSG_MINIMIZE_UNPIN {
                // 受管窗口被最小化 → 立即自动解置顶（消息由 WinEvent 回调经
                // PostMessageW(None, …) 投递；wParam 携带窗口句柄）。
                let hwnd = msg.wParam.0 as isize;
                let module = TopmostManagerModule {
                    inner: Arc::clone(&inner),
                };
                module.handle_minimized(hwnd);
                continue;
            }
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }

        // 泵退出：先 KillTimer（两个计时器）再卸载钩子（同一线程）。
        let _ = KillTimer(None, SHIELD_TIMER_ID);
        let _ = KillTimer(None, UNPIN_RECONCILE_TIMER_ID);
        let _ = UnhookWinEvent(hook);
        tracing::info!(target: "topmost_manager", "前台 / 最小化事件钩子已安全卸载，泵线程退出");
    }
}

/// WinEvent 回调的裸函数指针类型（与 `WINEVENTPROC` 载荷一致）。
#[cfg(windows)]
type WinEventCallback = unsafe extern "system" fn(HWINEVENTHOOK, u32, HWND, i32, i32, u32, u32);

/// WinEvent 回调（v0.4.1 双事件分派：前台激活 / 最小化开始）。
///
/// - `EVENT_SYSTEM_FOREGROUND`：记录“被激活的窗口”并（重新）武装 15ms 一次性
///   防抖计时器（泵线程在 `WM_TIMER` 到期后执行前台抢占纠偏）；
/// - `EVENT_SYSTEM_MINIMIZESTART`：经 `PostMessageW(None, MSG_MINIMIZE_UNPIN, …)`
///   把“受管窗口被最小化”投递给泵线程消息队列——回调内不执行任何解置顶逻辑，
///   由消息泵立即处理（最小化的即时性由消息队列保证，无额外防抖延迟）。
///
/// # Safety / 约束
/// - 布局与 `WINEVENTPROC` 一致；运行于本模块泵线程的系统回调上下文；
/// - 回调内严禁重活 / 异步操作：前台路径只做一次线程局部写 + 武装计时器，
///   最小化路径只做一次 `PostMessageW` 投递；
/// - `SetTimer(None, …)` / `PostMessageW(None, …)` 均要求调用线程已建消息队列
///   （泵线程满足）；
/// - **panic 边界（v0.6.1 · S5）**：回调体整体置于
///   [`crate::ffi_guard::guard_ffi`] 内。回调内的 `tracing!` 宏、线程局部访问与
///   Win32 调用包装均可 panic，而 panic 一旦跨 `extern "system"` 展开会让整个
///   常驻进程直接 abort（前台钩子 / 最小化钩子 / 托盘全部随之失效且无任何反馈）。
///   截停后本次事件被放弃——丢失一次前台纠偏机会，进程与钩子保持存活。
#[cfg(windows)]
unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    let _ = crate::ffi_guard::guard_ffi("topmost_manager::win_event_proc", || {
        if hwnd.0.is_null() {
            return;
        }
        if event == EVENT_SYSTEM_FOREGROUND {
            // 记录激活窗口（本线程 Cell；泵线程在防抖到期后消费）。
            RECENT_FOREGROUND.with(|cell| cell.set(hwnd.0 as usize));
            // (重新)武装 15ms 一次性防抖计时器：连续前台事件自动合并为一次纠偏。
            let _ = SetTimer(None, SHIELD_TIMER_ID, FOREGROUND_DEBOUNCE_MS, None);
        } else if event == EVENT_SYSTEM_MINIMIZESTART {
            // 最小化开始：把事件翻译成泵线程定制消息（wParam 携带窗口句柄）。
            // SAFETY: PostMessageW(None, …) 把消息投递到调用线程（泵线程）自己的
            // 队列；失败仅返回错误码，无未定义行为。
            let _ = PostMessageW(None, MSG_MINIMIZE_UNPIN, WPARAM(hwnd.0 as usize), LPARAM(0));
        }
    });
}

// ---------------------------------------------------------------------------
// 单元测试（跨平台纯逻辑：状态排序 / 规则匹配 / 条目序列化 / 链规划）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TopmostManagerConfig;
    use crate::modules::ToolModule;

    fn entry(hwnd: isize, process: &str, title: &str, priority: u8) -> ActivePinnedWindow {
        ActivePinnedWindow {
            hwnd,
            process_name: process.to_string(),
            title: title.to_string(),
            priority,
            enabled: true,
        }
    }

    /// 条目序列化契约：ActivePinnedWindow → PinnedRule 逐字段保真（含优先级夹紧）。
    #[test]
    fn active_entry_serializes_to_rule() {
        let window = entry(0x1234, "notepad.exe", "无标题 - 记事本", 1);
        let rule = window.to_rule();
        assert_eq!(rule.process_name, "notepad.exe");
        assert_eq!(rule.title_pattern, "无标题 - 记事本");
        assert_eq!(rule.priority, 1);
        assert!(rule.enabled);

        // 越界优先级序列化时夹紧。
        let window = entry(0x5678, "cmd.exe", "命令提示符", 42);
        assert_eq!(window.to_rule().priority, engine::PRIORITY_MAX);
        let window = entry(0x5678, "cmd.exe", "命令提示符", 0);
        assert_eq!(window.to_rule().priority, engine::PRIORITY_MIN);
    }

    // ---- M4①：用户手动取消置顶的检测（v0.6.1 整改） ----

    /// 移除判定的真值表：只有「窗口存活 + 已不置顶」才判为用户手动取消。
    #[test]
    fn user_unpin_detection_truth_table() {
        assert!(
            should_drop_user_unpinned(true, false),
            "窗口存活但已非置顶 = 用户手动取消 → 必须移除条目（否则会被重新置顶）"
        );
        assert!(
            !should_drop_user_unpinned(true, true),
            "仍处于置顶状态 → 保留"
        );
        assert!(
            !should_drop_user_unpinned(false, false),
            "窗口已消亡 → 交由 sweep_dead 统一清理，本判定不越权"
        );
        assert!(
            !should_drop_user_unpinned(false, true),
            "不可能组合（句柄失效时 is_topmost 恒为 false）→ 保守保留"
        );
    }

    /// 状态容器的对账入口：空条目集合下不得产生任何移除（幂等、无副作用）。
    ///
    /// 真实窗口的「用户取消」端到端验证由 `engine::is_topmost` 的真实窗口测试覆盖
    /// （见 `engine` 模块单测）；此处只锁住容器层契约。
    #[test]
    fn reconcile_on_empty_state_is_noop() {
        let module = TopmostManagerModule::with_rules(Vec::new());
        assert_eq!(module.reconcile_user_unpin(), 0, "空状态对账应无移除");
        assert_eq!(module.reconcile_user_unpin(), 0, "重复对账应幂等");
        assert!(module.pinned().is_empty());
    }

    /// 受管条目与配置节双向互转的端到端往返（结构体序列化测试之一）。
    #[test]
    fn pinned_snapshot_roundtrips_through_config_section() {
        let managed = [
            entry(0x1, "a.exe", "标题 A", 2),
            entry(0x2, "b.exe", "标题 B", 9),
        ];
        let rules: Vec<PinnedRule> = managed.iter().map(ActivePinnedWindow::to_rule).collect();
        let cfg = TopmostManagerConfig {
            enabled: true,
            pinned_rules: rules,
        };
        let toml_text = toml::to_string_pretty(&cfg).expect("序列化应成功");
        let parsed: TopmostManagerConfig = toml::from_str(&toml_text).expect("反序列化应成功");
        assert_eq!(parsed.pinned_rules.len(), 2);
        assert_eq!(parsed.pinned_rules[0].process_name, "a.exe");
        assert_eq!(parsed.pinned_rules[0].priority, 2);
        assert_eq!(parsed.pinned_rules[1].title_pattern, "标题 B");
        assert!(parsed.enabled);
    }

    /// 规则匹配：进程名大小写不敏感；标题双向子串（前后缀变化仍命中）。
    #[test]
    fn rule_matching_semantics() {
        // 进程名大小写不敏感。
        assert!(rule_matches_window(
            "notepad.exe",
            "无标题",
            true,
            "NOTEPAD.EXE",
            "无标题 - 记事本"
        ));
        // 标题子串：窗口标题含规则模式。
        assert!(rule_matches_window(
            "notepad.exe",
            "记事本",
            true,
            "notepad.exe",
            "文档1 - 记事本"
        ));
        // 标题子串（反向）：规则模式含窗口标题（窗口标题变短仍命中）。
        assert!(rule_matches_window(
            "chrome.exe",
            "新建标签页 - Google Chrome",
            true,
            "chrome.exe",
            "新建标签页"
        ));
        // 进程名不匹配。
        assert!(!rule_matches_window(
            "notepad.exe",
            "记事本",
            true,
            "chrome.exe",
            "记事本"
        ));
        // disabled 规则不匹配。
        assert!(!rule_matches_window(
            "notepad.exe",
            "记事本",
            false,
            "notepad.exe",
            "记事本"
        ));
        // 空标题模式 = 仅按进程名匹配（任何标题都命中）。
        assert!(rule_matches_window(
            "calc.exe",
            "  ",
            true,
            "calc.exe",
            "计算器"
        ));
    }

    /// 状态容器排序：sorted_snapshot 按（优先级升序, 插入序稳定）。
    #[test]
    fn sorted_snapshot_orders_by_priority_then_insertion() {
        let state = TopmostState::default();
        state.upsert(entry(0x30, "c.exe", "C", 3));
        state.upsert(entry(0x10, "a.exe", "A", 1));
        state.upsert(entry(0x12, "a2.exe", "A2", 1));
        state.upsert(entry(0x20, "b.exe", "B", 2));

        let sorted = state.sorted_snapshot();
        let order: Vec<isize> = sorted.iter().map(|e| e.hwnd).collect();
        assert_eq!(
            order,
            vec![0x10, 0x12, 0x20, 0x30],
            "优先级升序 + 插入序稳定"
        );

        // 同优先级改级：upsert 原位更新保持位置（find 命中更新）。
        state.upsert(ActivePinnedWindow {
            hwnd: 0x10,
            priority: 5,
            ..entry(0x10, "a.exe", "A", 1)
        });
        let sorted = state.sorted_snapshot();
        let order: Vec<isize> = sorted.iter().map(|e| e.hwnd).collect();
        assert_eq!(order, vec![0x12, 0x20, 0x30, 0x10], "改级后按新优先级重排");
    }

    /// 受管条目的增删与查询语义。
    #[test]
    fn state_upsert_find_remove() {
        let state = TopmostState::default();
        assert!(state.find(0x1).is_none());
        state.upsert(entry(0x1, "a.exe", "A", 1));
        state.upsert(entry(0x2, "b.exe", "B", 2));
        assert_eq!(state.find(0x2).unwrap().process_name, "b.exe");

        // upsert 同句柄 = 原位更新（不重复追加）。
        state.upsert(ActivePinnedWindow {
            title: "A2".into(),
            ..entry(0x1, "a.exe", "A", 1)
        });
        assert_eq!(state.snapshot().len(), 2, "同句柄 upsert 不得追加新条目");

        state.remove(0x1);
        assert!(state.find(0x1).is_none());
        assert_eq!(state.snapshot().len(), 1);
    }

    /// 条目 → 引擎链条目的映射：seq 使用插入序索引（同优先级内先置顶者在上）。
    #[test]
    fn chain_entries_preserve_insertion_order_within_priority() {
        let entries = vec![entry(0x10, "a.exe", "A", 1), entry(0x20, "b.exe", "B", 1)];
        let chain = to_chain_entries(&entries);
        assert_eq!(chain[0].hwnd, 0x10);
        assert_eq!(chain[0].seq, 0);
        assert_eq!(chain[1].seq, 1);
        assert_eq!(chain.len(), 2);
    }

    /// 链序（引擎重排）正确反映优先级。
    #[test]
    fn sorted_snapshot_feeds_engine_ordering() {
        let state = TopmostState::default();
        state.upsert(entry(0x50, "e.exe", "E", 5));
        state.upsert(entry(0x10, "a.exe", "A", 1));
        state.upsert(entry(0x90, "i.exe", "I", 9));
        let chain = to_chain_entries(&state.sorted_snapshot());
        let order: Vec<isize> = chain.iter().map(|e| e.hwnd).collect();
        assert_eq!(order, vec![0x10, 0x50, 0x90], "1 级最前、9 级最后");
    }

    /// 配置记忆恢复的规则过滤（enabled=false 直接跳过；进程名 / 标题模式匹配）。
    #[cfg(windows)]
    #[test]
    fn restore_matching_uses_rule_and_window_info() {
        let window = enum_windows::WindowInfo {
            hwnd: 0x77,
            process_name: "Code.exe".into(),
            title: "main.rs - TLToolBox - Visual Studio Code".into(),
            class_name: "Chrome_WidgetWin_1".into(),
            topmost: false,
        };
        let rule = PinnedRule {
            process_name: "code.exe".into(),
            title_pattern: "TLToolBox".into(),
            priority: 2,
            enabled: true,
        };
        assert!(rule_matches_window_info(&rule, &window));
        assert_eq!(rule.priority, 2);

        let disabled = PinnedRule {
            enabled: false,
            ..rule
        };
        assert!(!rule_matches_window_info(&disabled, &window));
    }

    // -----------------------------------------------------------------------
    // 模块生命周期契约（ToolModule 元数据 / 启停幂等性；与 popup_blocker 同纪律）
    // -----------------------------------------------------------------------

    /// 元数据契约：注册表键 / 展示名 / 描述必须与装配层与 UI 的约定一致。
    #[test]
    fn metadata_identity_matches_registry_contract() {
        let module = TopmostManagerModule::default();
        assert_eq!(module.id(), "topmost_manager");
        assert_eq!(module.display_name(), "全局窗口置顶");
        assert_eq!(module.description(), "多窗口优先级分级置顶守护");
        assert!(!module.is_running(), "新模块应处于停止态");
    }

    /// 生命周期幂等性验证：重复 `start` / `stop` 不得重复派生泵线程 /
    /// 重复卸载钩子，状态机多轮收敛且无错乱。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_start_stop_is_idempotent_and_converges() {
        let module = TopmostManagerModule::default();
        assert!(!module.is_running());

        for cycle in 1..=2 {
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

    /// 停止态下操作不受影响：未运行时可正常构造（不触碰任何原生线程）。
    #[test]
    fn module_can_operate_while_stopped() {
        let module = TopmostManagerModule::default();
        // 未运行时窗口级操作应返回明确错误（窗口不存在的判定与运行态一致），
        // 而非 panic 或静默成功——由装配层据此提示“请先开启守护”。
        let err = module.apply_pin(0xDEAD_BEEF, 5);
        assert!(
            matches!(err, Err(engine::Win32Error::InvalidWindow)),
            "失效句柄应返回 InvalidWindow，实际: {err:?}"
        );
        let err = module.apply_unpin(0xDEAD_BEEF);
        assert!(
            matches!(err, Err(engine::Win32Error::InvalidWindow)),
            "解除不存在的窗口应返回 InvalidWindow，实际: {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // v0.4.1 修复回归防线：apply_unpin 时序纪律 / 优先级记忆 / 最小化自动解置顶
    // -----------------------------------------------------------------------

    /// 死尸复活根治：`apply_unpin` 即使遇到 Win32 失败（窗口已消亡）也必须已把
    /// 条目从受管集移除——15ms 纠偏定时器 / 前台抢占纠偏拿到的永远是移除后的
    /// 快照，无法把已取消的窗口重新置顶。
    #[test]
    fn apply_unpin_removes_entry_even_when_win32_fails() {
        let module = TopmostManagerModule::default();
        // 直接向状态容器注入一条“受管窗口”（绕过 FFI，模拟已置顶条目）。
        module
            .inner
            .state
            .upsert(entry(0x1234, "notepad.exe", "文档 - 记事本", 2));
        assert_eq!(module.pinned().len(), 1);

        // 伪句柄 0x1234 在 IsWindow 下必然失效 → set_notopmost 返回 InvalidWindow；
        // 但移除必须先于该失败完成，返回错误的同时受管集必须已清空。
        let err = module.apply_unpin(0x1234);
        assert!(
            matches!(err, Err(engine::Win32Error::InvalidWindow)),
            "伪句柄应返回 InvalidWindow，实际: {err:?}"
        );
        assert!(
            module.pinned().is_empty(),
            "即使 Win32 失败，受管条目也必须已彻底移除（防死尸复活）"
        );
    }

    /// 优先级记忆种子：`with_rules` 注入的规则（含 `enabled = false` 记忆规则）
    /// 全部播种进 `memorized`，供未受管窗口回填历史优先级。
    #[test]
    fn remembered_priority_is_seeded_from_rules() {
        let rules = vec![
            PinnedRule {
                process_name: "notepad.exe".into(),
                title_pattern: "无标题".into(),
                priority: 5,
                enabled: true,
            },
            // enabled = false 的记忆规则同样参与回填（v0.4.1 核心语义）。
            PinnedRule {
                process_name: "chrome.exe".into(),
                title_pattern: String::new(),
                priority: 8,
                enabled: false,
            },
        ];
        let module = TopmostManagerModule::with_rules(rules);
        assert_eq!(module.remembered_priority("notepad.exe"), Some(5));
        assert_eq!(module.remembered_priority("chrome.exe"), Some(8));
        // 无记忆进程无匹配；v0.6.2（L9）：主键已统一小写，大小写变体同样命中。
        assert_eq!(module.remembered_priority("calc.exe"), None);
        assert_eq!(
            module.remembered_priority("NOTEPAD.EXE"),
            Some(5),
            "记忆键应为大小写不敏感（与规则匹配语义一致）"
        );
    }

    /// 优先级记忆持久化形态：受管条目收敛为 `enabled = true` 规则，无受管窗口
    /// 的进程收敛为 `enabled = false` 记忆规则（空标题模式 = 仅进程主键）。
    #[test]
    fn rules_snapshot_merges_memory_only_rules() {
        let rules = vec![PinnedRule {
            process_name: "chrome.exe".into(),
            title_pattern: String::new(),
            priority: 8,
            enabled: false,
        }];
        let module = TopmostManagerModule::with_rules(rules);
        // 未受管时的快照 = 纯记忆规则。
        let snapshot = module.rules_snapshot();
        assert_eq!(snapshot.len(), 1);
        assert!(!snapshot[0].enabled, "记忆规则应保持 enabled = false");
        assert_eq!(snapshot[0].process_name, "chrome.exe");
        assert_eq!(snapshot[0].priority, 8);
        assert!(snapshot[0].title_pattern.is_empty(), "记忆规则用空标题模式");

        // 置顶 notepad 后：受管规则 + chrome 的记忆规则并存（记忆不因其它窗口
        // 的操作丢失）。
        let pinned_entry = entry(0x5678, "notepad.exe", "文档 - 记事本", 3);
        module.inner.state.upsert(pinned_entry.clone());
        let snapshot = module.rules_snapshot();
        assert_eq!(snapshot.len(), 2, "受管规则 + 记忆规则");
        let managed_rule = snapshot
            .iter()
            .find(|rule| rule.process_name == "notepad.exe")
            .expect("应有 notepad 的受管规则");
        assert!(managed_rule.enabled);
        assert_eq!(managed_rule.priority, 3);
        assert!(snapshot
            .iter()
            .any(|rule| rule.process_name == "chrome.exe" && !rule.enabled && rule.priority == 8));
    }

    /// 解除置顶不抹除进程级优先级记忆（用户下次打开弹窗 / 重启应用仍可回填）。
    #[test]
    fn apply_unpin_preserves_memorized_priority() {
        let rules = vec![PinnedRule {
            process_name: "notepad.exe".into(),
            title_pattern: String::new(),
            priority: 4,
            enabled: false,
        }];
        let module = TopmostManagerModule::with_rules(rules);
        module
            .inner
            .state
            .upsert(entry(0x1111, "notepad.exe", "文档 - 记事本", 4));
        assert_eq!(module.remembered_priority("notepad.exe"), Some(4));

        let _ = module.apply_unpin(0x1111); // 伪句柄 → InvalidWindow，但移除已完成
        assert!(module.pinned().is_empty());
        assert_eq!(
            module.remembered_priority("notepad.exe"),
            Some(4),
            "解除置顶不得抹除优先级记忆"
        );
        // 快照仍保留该进程的记忆规则，下次枚举可回填。
        let snapshot = module.rules_snapshot();
        assert!(snapshot
            .iter()
            .any(|rule| rule.process_name == "notepad.exe" && !rule.enabled && rule.priority == 4));
    }

    /// 最小化自动解置顶：受管窗口被最小化 → 条目移除 + 事件总线收到 Toast。
    #[test]
    fn handle_minimized_unpins_managed_window_and_toasts() {
        let bus = EventBus::new(4);
        let mut rx = bus.subscribe();
        let module = TopmostManagerModule::default().with_bus(Some(bus));
        module
            .inner
            .state
            .upsert(entry(0x2222, "calc.exe", "计算器", 1));

        // 直接以守护线程的入口调用（伪句柄在 IsWindow 下失效 → InvalidWindow，
        // 与“窗口在解置顶前已消亡”同语义，同样视为解除完成）。
        module.handle_minimized(0x2222);

        assert!(module.pinned().is_empty(), "最小化受管窗口后条目必须被移除");
        match rx.try_recv() {
            Ok(AppEvent::ToastRequested(message)) => {
                assert!(
                    message.contains("窗口已最小化，自动取消置顶"),
                    "Toast 文案应含最小化解置顶语义，实际: {message}"
                );
            }
            other => panic!("应收到最小化解置顶 Toast，实际: {other:?}"),
        }
    }

    /// 最小化自动解置顶：非受管窗口的最小化不产生任何动作（无 Toast、状态不变）。
    #[test]
    fn handle_minimized_ignores_unmanaged_window() {
        let bus = EventBus::new(4);
        let mut rx = bus.subscribe();
        let module = TopmostManagerModule::default().with_bus(Some(bus));
        module.handle_minimized(0x9999);
        assert!(module.pinned().is_empty());
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "非受管窗口最小化不应发布任何事件"
        );
    }
}
