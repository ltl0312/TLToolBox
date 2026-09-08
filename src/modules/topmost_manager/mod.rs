//! # 全局窗口置顶守护器（v0.4.0 · 第五个常驻守护模块）
//!
//! 基于 Win32 原生 API（零重型第三方依赖）实现**轻量可视化窗口置顶管理系统**：
//! 支持 1~9 级优先级（1 级最顶层），经 [`enum_windows`] 轻量枚举候选窗口并产出
//! 纯文本列表（**严禁加载窗口图标**——图标位图会把 UI 图形显存预算打爆），再交由
//! 链式 Z-Order 引擎（[`engine`]）逐级锚定守护。
//!
//! # 模块模型
//!
//! - **运行态（守护）**：`start()` 派生专用原生泵线程（`win32-topmost-pump`）安装
//!   [`SetWinEventHook`]（`EVENT_SYSTEM_FOREGROUND`）。系统每次前台窗口切换都向
//!   泵线程投递 WinEvent 回调：回调只记录“被激活的窗口”并（重新）武装一枚 15ms
//!   一次性 `SetTimer` 防抖，随后立即返回（回调内绝无重活）。泵线程收到 `WM_TIMER`
//!   时执行**前台抢占纠偏**——若用户激活了受管置顶窗口，系统已把它顶到绝对顶层，
//!   守护按 [`engine::plan_shield_refresh`] 把其前方的 1 / 2 级窗口沿链序重刷
//!   （全程 `SWP_NOACTIVATE`，**绝不抢占用户焦点**）。无事时泵线程阻塞于
//!   `GetMessageW`，CPU 占用为零；
//! - **停止态**：`stop()` 卸载钩子 / 计时器并 Join 泵线程。已置顶窗口的
//!   `WS_EX_TOPMOST` 属性由操作系统在会话期间持续保持——停止只冻结“守护”，
//!   不撤销用户已建立的置顶（避免全量关闭误伤用户手工置顶）；
//! - **窗口级操作与运行态正交**：弹窗内的置顶开关 / 优先级步进器（[`apply_pin`] /
//!   [`apply_unpin`] / [`set_priority`](Self::set_priority)）在模块运行与否时都可
//!   执行——直接 `SetWindowPos` 即时生效并持久化规则记忆。
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
//! # 规则记忆与恢复
//!
//! 每次置顶 / 解除 / 改级落定后，模块把当前受管条目序列化为
//! [`PinnedRule`](crate::config::PinnedRule) 列表（进程名 + 标题原文 + 优先级 +
//! enabled），经单写者通道交给装配层持久化（`[topmost_manager]` 节）。
//! `start()` 时按装配期注入的规则恢复：重新枚举窗口 → 进程名（大小写不敏感）+
//! 标题**子串**双向匹配 → 恢复置顶与优先级。
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
        DispatchMessageW, GetMessageW, KillTimer, PeekMessageW, PostThreadMessageW, SetTimer,
        TranslateMessage, EVENT_SYSTEM_FOREGROUND, MSG, PM_NOREMOVE, WINEVENT_OUTOFCONTEXT,
        WINEVENT_SKIPOWNPROCESS, WM_QUIT, WM_TIMER,
    },
};

/// 置顶条目优先级默认值（用户新建置顶 / UI 行未指定时使用；1~9，1 最顶层）。
pub const DEFAULT_PRIORITY: u8 = 3;
/// 前台抢占纠偏的防抖窗口（15ms，落在任务书 10~20ms 区间；合并同一次用户操作的
/// 连续前台事件为一次纠偏）。
#[cfg(windows)]
const FOREGROUND_DEBOUNCE_MS: u32 = 15;
/// 防抖一次性计时器 ID（`SetTimer` / `WM_TIMER` 载荷；'TL' 占位）。
#[cfg(windows)]
const SHIELD_TIMER_ID: usize = 0x544C;
/// 停机协议中 Join 原生泵线程的等待上限（与弹窗拦截模块同一纪律）。
#[cfg(windows)]
const PUMP_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

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
        self.snapshot()
            .into_iter()
            .find(|entry| entry.hwnd == hwnd)
    }

    /// 追加或原位更新一条受管条目（新条目追加在向量末尾 = 同优先级组内最下）。
    fn upsert(&self, entry: ActivePinnedWindow) {
        let mut guard = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        match guard.iter_mut().find(|existing| existing.hwnd == entry.hwnd) {
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

    fn note_guard_failure(&self) {
        *self
            .guard_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
    }

    /// 拉取并清除守护失败标志（一次性告警）。
    fn take_guard_failure(&self) -> bool {
        std::mem::take(&mut *self.guard_failure.lock().unwrap_or_else(PoisonError::into_inner))
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
    pub fn with_rules(rules: impl IntoIterator<Item = PinnedRule>) -> Self {
        Self {
            inner: Arc::new(TopmostManagerInner {
                lifecycle: AsyncMutex::new(()),
                running: AtomicBool::new(false),
                active: StdMutex::new(None),
                state: Arc::new(TopmostState::default()),
                startup_rules: StdMutex::new(rules.into_iter().collect()),
                rules_tx: StdMutex::new(None),
                bus: StdMutex::new(None),
            }),
        }
    }

    /// 绑定事件总线（守护线程的 UIPI 失败提示出口；`None` = 不弹 Toast）。
    pub fn with_bus(self, bus: Option<EventBus>) -> Self {
        if let Some(bus) = bus {
            // 总线在模块构造时装配一次（不可变字段）。
            self.inner.bus.lock().unwrap_or_else(PoisonError::into_inner).replace(bus);
        }
        self
    }

    /// 装配规则持久化通道（单写者；置顶 / 解除 / 改级后投递最新 [`PinnedRule`] 列表）。
    pub fn attach_rule_persister(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<Vec<PinnedRule>>,
    ) {
        *self.inner.rules_tx.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
    }

    /// 当前受管条目快照（插入序；调试 / 测试用）。
    pub fn pinned(&self) -> Vec<ActivePinnedWindow> {
        self.inner.state.snapshot()
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
    /// `priority` 越界自动夹紧 1~9；成功后持久化规则记忆。错误上抛：
    /// - [`engine::Win32Error::AccessDenied`]：UIPI 拦截（提示用户提权）；
    /// - [`engine::Win32Error::InvalidWindow`]：窗口已销毁。
    pub fn apply_pin(&self, hwnd: isize, priority: u8) -> Result<(), engine::Win32Error> {
        let priority = engine::clamp_priority(priority);
        // 1) 前置校验 + 立即应用置顶（失败上抛：窗口失效 / UIPI 拦截 / 其他）。
        engine::set_topmost(hwnd)?;

        // 2) 采集实时元数据并入状态（进程名 / 标题以置顶时刻为准）。
        let (process_name, title) = window_identity(hwnd)
            .unwrap_or_else(|| ("<unknown.exe>".to_string(), String::new()));
        self.upsert_entry(ActivePinnedWindow {
            hwnd,
            process_name,
            title,
            priority,
            enabled: true,
        });

        // 3) 沿整条优先级链重刷，保证新窗口落在正确的链位（含未运行态——
        //    链式重刷等价于 set_topmost + 锚定关系，即时生效）。
        self.refresh_chain();
        Ok(())
    }

    /// 取消窗口置顶并移除受管条目（持久化规则随之更新）。
    pub fn apply_unpin(&self, hwnd: isize) -> Result<(), engine::Win32Error> {
        engine::set_notopmost(hwnd)?;
        self.remove_entry(hwnd);
        // 其余受管窗口保持既有置顶（系统锚定关系未破坏），无需整链重刷。
        Ok(())
    }

    /// 修改窗口优先级：更新条目标记后沿新优先级整链重刷（免去先解后置的闪烁）。
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

    /// 把当前受管条目序列化为规则列表并投递给持久化通道（若有装配）。
    fn persist_rules(&self) {
        let rules: Vec<PinnedRule> = self
            .inner
            .state
            .snapshot()
            .iter()
            .map(ActivePinnedWindow::to_rule)
            .collect();
        let tx = self
            .inner
            .rules_tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(tx) = tx {
            let _ = tx.send(rules);
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
    fn shield_refresh(&self, activated: isize) {
        let entries = self.inner.state.sorted_snapshot();
        let chain = to_chain_entries(&entries);
        let Some(plan) = engine::plan_shield_refresh(&chain, activated) else {
            return;
        };
        let stats = engine::apply_plan(&plan);
        self.handle_chain_stats(&stats);
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

        let run = self
            .inner
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(run) = run else {
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
            if let Some(thread) = run.thread {
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
                    }
                    Err(_elapsed) => {
                        tracing::error!(
                            target: "topmost_manager",
                            "泵线程未在 {PUMP_JOIN_TIMEOUT:?} 内退出，已转入分离式收尾"
                        );
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
    // 双向子串：窗口标题可能随会话上下文增删前后缀。
    title.contains(&needle) || needle.contains(&title)
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
// 全部发生在同一条泵线程上，`Cell` 即线程安全）。
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
        let hook = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            callback_module,
            Some(foreground_proc as ForegroundCallback),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        if hook.0.is_null() {
            let _ = ready_tx.send(Err("SetWinEventHook 安装失败（前台事件钩子）".to_string()));
            return;
        }

        let thread_id = GetCurrentThreadId();
        if ready_tx.send(Ok(thread_id)).is_err() {
            let _ = UnhookWinEvent(hook);
            return;
        }
        tracing::info!(target: "topmost_manager", "前台事件钩子已就绪（泵线程 {thread_id}）");

        // 消息泵：WM_QUIT → 退出；WM_TIMER(防抖) → 前台抢占纠偏；其余消息
        //（含 WinEvent 回调的派发）走 Translate + Dispatch。
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_QUIT {
                break;
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
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }

        // 泵退出：先 KillTimer 再卸载钩子（同一线程）。
        let _ = KillTimer(None, SHIELD_TIMER_ID);
        let _ = UnhookWinEvent(hook);
        tracing::info!(target: "topmost_manager", "前台事件钩子已安全卸载，泵线程退出");
    }
}

/// 前台事件回调的裸函数指针类型（与 `WINEVENTPROC` 载荷一致）。
#[cfg(windows)]
type ForegroundCallback = unsafe extern "system" fn(HWINEVENTHOOK, u32, HWND, i32, i32, u32, u32);

/// WinEvent 回调：记录“被激活的窗口”并武装防抖计时器（泵线程派发）。
///
/// # Safety / 约束
/// - 布局与 `WINEVENTPROC` 一致；运行于本模块泵线程的系统回调上下文；
/// - 回调内严禁重活 / 异步操作：只做一次线程局部写 + 武装计时器；
/// - `SetTimer(None, …)` 要求调用线程已建消息队列（泵线程满足）。
#[cfg(windows)]
unsafe extern "system" fn foreground_proc(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    if hwnd.0.is_null() {
        return;
    }
    // 记录激活窗口（本线程 Cell；泵线程在防抖到期后消费）。
    RECENT_FOREGROUND.with(|cell| cell.set(hwnd.0 as usize));
    // (重新)武装 15ms 一次性防抖计时器：连续前台事件自动合并为一次纠偏。
    let _ = SetTimer(None, SHIELD_TIMER_ID, FOREGROUND_DEBOUNCE_MS, None);
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

    /// 受管条目与配置节双向互转的端到端往返（结构体序列化测试之一）。
    #[test]
    fn pinned_snapshot_roundtrips_through_config_section() {
        let managed = [entry(0x1, "a.exe", "标题 A", 2), entry(0x2, "b.exe", "标题 B", 9)];
        let rules: Vec<PinnedRule> = managed.iter().map(ActivePinnedWindow::to_rule).collect();
        let cfg = TopmostManagerConfig {
            enabled: true,
            pinned_rules: rules,
        };
        let toml_text = toml::to_string_pretty(&cfg).expect("序列化应成功");
        let parsed: TopmostManagerConfig =
            toml::from_str(&toml_text).expect("反序列化应成功");
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
        assert_eq!(order, vec![0x10, 0x12, 0x20, 0x30], "优先级升序 + 插入序稳定");

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

        let disabled = PinnedRule { enabled: false, ..rule };
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
}
