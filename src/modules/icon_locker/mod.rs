//! # 桌面图标布局锁（v0.6.0 · 第七个模块卡片）
//!
//! 通过**纯原生 STA COM**（严禁跨进程内存注入）经 `IShellWindows` → `IShellBrowser`
//! → `IFolderView` 抓取桌面图标绝对坐标（`GetItemPosition`，以 DisplayName 为键），
//! 保存为绑定显示器拓扑指纹（[`daemon::topology_fingerprint`]）的方案；还原时批量
//! `SelectAndPositionItems` 瞬移。`start()` 派生专用守护线程（[`daemon::DisplayChangeWatchdog`]）
//! 监听 `WM_DISPLAYCHANGE`，以 **1500ms 可重置防抖**在拓扑稳定后自动还原最近一次
//! 保存 / 还原的方案；失败时经事件总线弹出 Toast 提示用户排查桌面右键「自动排列图标」。
//!
//! # 线程模型（延续既有模块铁律）
//!
//! - 全部 COM 操作（抓取 / 还原）都是同步 Win32 调用，**严禁阻塞 Slint UI 线程**：
//!   装配层一律经 [`explorer::spawn_com_thread`] 在**全新的独立 OS 线程**内执行
//!   （线程内 `CoInitializeEx(COINIT_APARTMENTTHREADED)` → 操作 →
//!   `CoUninitialize()`，`RPC_E_CHANGED_MODE` 容错；结果经 oneshot 通道异步回传
//!   ——Tokio 工作线程池永不接触 Shell COM，避免 `spawn_blocking` 阻塞池线程的
//!   COM 公寓污染）；
//! - 守护线程回调只做「发送一条空信号」的短促动作，真正的还原经
//!   `explorer::spawn_com_thread` 在独立 OS 线程执行；
//! - 模块内部状态全部收敛在 [`ModuleState`]（短临界 std Mutex + 原子标志），
//!   `ToolModule` 生命周期永不在锁内跨 `await`。

pub mod daemon;
pub mod explorer;

use crate::bus::{AppEvent, EventBus};
use crate::config::IconLayoutProfile;
use crate::modules::{ModuleError, ToolModule};
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};

/// 模块内部并发状态（短临界 std Mutex 保护；读取先克隆快照再在锁外执行 COM）。
struct ModuleState {
    /// 已保存的布局方案（保持插入顺序；UI 列表按同一顺序展示）。
    profiles: StdMutex<Vec<IconLayoutProfile>>,
    /// 最近一次保存 / 还原的方案 ID（WM_DISPLAYCHANGE 自动还原的目标）。
    active_profile: StdMutex<Option<String>>,
    /// 拓扑变化后自动还原开关（与配置 `icon_locker.auto_restore` 同源）。
    auto_restore: AtomicBool,
    /// 事件总线（守护线程失败提示 Toast 的出口；缺省为 None）。
    bus: StdMutex<Option<EventBus>>,
}

fn lock_ok<T>(guard: std::sync::LockResult<T>) -> T {
    guard.unwrap_or_else(PoisonError::into_inner)
}

/// 从状态中选取自动还原目标：优先最近一次活动方案，其次最新保存的方案。
fn pick_auto_restore_profile(state: &ModuleState) -> Option<IconLayoutProfile> {
    let profiles = lock_ok(state.profiles.lock());
    let active = lock_ok(state.active_profile.lock());
    active
        .as_ref()
        .and_then(|id| profiles.iter().find(|p| p.id == *id).cloned())
        .or_else(|| profiles.last().cloned())
}

/// 向事件总线投递一条 Toast（守护线程失败提示用；无总线时静默）。
fn toast(state: &ModuleState, message: impl Into<String>) {
    // 单次短临界读取总线句柄：存在即发布（publish 为同步非阻塞广播）。
    if let Some(bus) = lock_ok(state.bus.lock()).as_ref() {
        bus.publish(AppEvent::ToastRequested(message.into()));
    }
}

/// 拓扑指纹匹配判定（v0.6.2 · P2-12 纯函数，供单测）。
///
/// 方案**只有**在「保存时的显示器拓扑 == 当前拓扑」时才允许自动还原——这正是
/// 拓扑指纹字段的设计意图。旧实现从不校验：显示器拓扑变化后会把旧坐标刷到
/// 当前桌面，**误移动用户图标**（指纹沦为死数据）。
///
/// 空指纹（异常 / 手工构造的方案）视为**不匹配**——宁可少还原一次，也不把
/// 来源不明的坐标应用到当前桌面。
fn profile_matches_topology(profile_fingerprint: &str, current: &str) -> bool {
    !profile_fingerprint.is_empty() && profile_fingerprint == current
}

/// 执行自动还原（在独立 OS 线程内调用，见 [`explorer::spawn_com_thread`]；只依赖
/// 状态快照，不触碰 UI）。
///
/// # 指纹校验（v0.6.2 · P2-12）
/// 自动还原由 `WM_DISPLAYCHANGE` 触发，此时拓扑**可能**已变化。仅当活动（或最新）
/// 方案的拓扑指纹与当前一致时才执行还原；不一致则跳过并 Toast 告知用户——图标
/// 保持现状，绝不把旧坐标刷到不同的桌面上。手动还原（`restore_profile`）是用户的
/// 显式请求，不受此限制。
fn restore_active(state: &ModuleState) -> Result<(), ModuleError> {
    let profile = pick_auto_restore_profile(state).ok_or_else(|| -> ModuleError {
        "尚无已保存的布局方案，无法自动还原".into()
    })?;
    let current = daemon::current_topology_fingerprint();
    if !profile_matches_topology(&profile.topology_fingerprint, &current) {
        tracing::info!(
            target: "icon_locker",
            saved = %profile.topology_fingerprint,
            current = %current,
            "显示器拓扑与方案 \"{}\" 不匹配，跳过自动还原（图标保持现状）",
            profile.name
        );
        toast(
            state,
            format!(
                "显示器拓扑与方案「{}」不一致，未自动还原（方案对应 {}）",
                profile.name,
                if profile.topology_fingerprint.is_empty() {
                    "的拓扑信息缺失"
                } else {
                    "其他显示器布局"
                }
            ),
        );
        return Ok(());
    }
    explorer::restore_layout(&explorer::DesktopLayout {
        positions: profile.icon_positions.clone(),
    })?;
    *lock_ok(state.active_profile.lock()) = Some(profile.id.clone());
    tracing::info!(
        target: "icon_locker",
        "显示器拓扑自动还原完成（方案 \"{}\"，{} 个图标）",
        profile.name,
        profile.icon_positions.len()
    );
    Ok(())
}

/// 桌面图标布局锁模块。
pub struct IconLockerModule {
    running: Arc<AtomicBool>,
    state: Arc<ModuleState>,
    /// 原生守护线程句柄（WM_DISPLAYCHANGE 消息泵）。
    watchdog: StdMutex<Option<daemon::DisplayChangeWatchdog>>,
    /// 守护信号 → 自动还原的 Tokio 任务句柄。
    watchdog_task: StdMutex<Option<tokio::task::JoinHandle<()>>>,
}

impl IconLockerModule {
    pub fn new(profiles: Vec<IconLayoutProfile>) -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            state: Arc::new(ModuleState {
                profiles: StdMutex::new(profiles),
                active_profile: StdMutex::new(None),
                auto_restore: AtomicBool::new(true),
                bus: StdMutex::new(None),
            }),
            watchdog: StdMutex::new(None),
            watchdog_task: StdMutex::new(None),
        }
    }

    /// 装配期注入自动还原开关初值（来自配置 `icon_locker.auto_restore`）。
    pub fn with_auto_restore(self, auto_restore: bool) -> Self {
        self.state
            .auto_restore
            .store(auto_restore, Ordering::SeqCst);
        self
    }

    /// 装配期注入事件总线（守护线程失败 Toast 出口）。
    pub fn with_bus(self, bus: Option<EventBus>) -> Self {
        *lock_ok(self.state.bus.lock()) = bus;
        self
    }

    /// 当前已保存方案快照（UI 列表数据源）。
    pub fn profiles(&self) -> Vec<IconLayoutProfile> {
        lock_ok(self.state.profiles.lock()).clone()
    }

    /// 抓取当前桌面布局并保存为一份新方案（COM 操作，调用方须经
    /// [`explorer::spawn_com_thread`] 在独立 OS 线程内执行）。
    pub fn capture_profile(
        &self,
        name: impl Into<String>,
    ) -> Result<IconLayoutProfile, ModuleError> {
        let layout = explorer::capture_layout()?;
        let profile = IconLayoutProfile {
            id: format!("profile-{}", chrono::Utc::now().timestamp_millis()),
            name: name.into(),
            topology_fingerprint: daemon::current_topology_fingerprint(),
            icon_positions: layout.positions,
        };
        lock_ok(self.state.profiles.lock()).push(profile.clone());
        *lock_ok(self.state.active_profile.lock()) = Some(profile.id.clone());
        Ok(profile)
    }

    /// 按 ID 还原一份方案并设为活动（COM 操作，调用方须经
    /// [`explorer::spawn_com_thread`] 在独立 OS 线程内执行）。
    pub fn restore_profile(&self, id: &str) -> Result<(), ModuleError> {
        let profile = lock_ok(self.state.profiles.lock())
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .ok_or_else(|| -> ModuleError { format!("未找到布局方案: {id}").into() })?;
        explorer::restore_layout(&explorer::DesktopLayout {
            positions: profile.icon_positions.clone(),
        })?;
        *lock_ok(self.state.active_profile.lock()) = Some(profile.id.clone());
        Ok(())
    }

    /// 删除一份方案（返回是否确实删除；活动方案被删后自动还原回退到最新方案）。
    pub fn remove_profile(&self, id: &str) -> bool {
        let removed = {
            let mut profiles = lock_ok(self.state.profiles.lock());
            let old_len = profiles.len();
            profiles.retain(|p| p.id != id);
            old_len != profiles.len()
        };
        if removed {
            let mut active = lock_ok(self.state.active_profile.lock());
            if active.as_deref() == Some(id) {
                *active = None;
            }
        }
        removed
    }

    /// 自动还原开关当前值。
    pub fn auto_restore(&self) -> bool {
        self.state.auto_restore.load(Ordering::SeqCst)
    }

    /// 设置自动还原开关（UI 复选，随后装配层持久化到配置）。
    pub fn set_auto_restore(&self, enabled: bool) {
        self.state.auto_restore.store(enabled, Ordering::SeqCst);
    }
}

#[async_trait]
impl ToolModule for IconLockerModule {
    fn id(&self) -> &'static str {
        "icon_locker"
    }

    fn display_name(&self) -> &'static str {
        "桌面图标布局锁"
    }

    fn description(&self) -> &'static str {
        "保存并自动还原多显示器桌面图标布局"
    }

    async fn start(&self) -> Result<(), ModuleError> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Ok(()); // 幂等：已在运行态。
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();

        // 1) 派生原生守护线程；回调仅转发一条防抖信号，绝不执行重活。
        let watchdog = match daemon::DisplayChangeWatchdog::spawn(move || {
            let _ = tx.send(());
        }) {
            Ok(watchdog) => watchdog,
            Err(err) => {
                self.running.store(false, Ordering::SeqCst);
                return Err(format!("显示器守护线程启动失败: {err}").into());
            }
        };
        *lock_ok(self.watchdog.lock()) = Some(watchdog);

        // 2) 防抖信号 → 独立 OS 线程自动还原（COM 远离 UI 线程与 Tokio 工作线程池：
        //    spawn_com_thread 在线程内完成 CoInitializeEx(STA) / CoUninitialize 配对，
        //    结果经 oneshot 通道异步回传，见 explorer::spawn_com_thread）。
        let state = Arc::clone(&self.state);
        let running = Arc::clone(&self.running);
        let task = tokio::spawn(async move {
            while rx.recv().await.is_some() {
                if !running.load(Ordering::SeqCst) {
                    continue;
                }
                if !state.auto_restore.load(Ordering::SeqCst) {
                    tracing::debug!(target: "icon_locker", "显示器拓扑已变化但自动还原开关关闭，跳过");
                    continue;
                }
                let state2 = Arc::clone(&state);
                let (com_tx, com_rx) = tokio::sync::oneshot::channel();
                if let Err(err) = explorer::spawn_com_thread(
                    "tlt-icon-locker-autorestore",
                    move || restore_active(&state2),
                    com_tx,
                ) {
                    tracing::error!(target: "icon_locker", "自动还原线程启动失败: {err}");
                    continue;
                }
                match com_rx.await {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        tracing::warn!(
                            target: "icon_locker",
                            "显示器拓扑变化自动还原失败: {err}（若图标被系统弹回，请排查桌面右键「自动排列图标」）"
                        );
                        toast(
                            &state,
                            "图标布局自动还原失败：若图标被系统弹回，请排查桌面右键「自动排列图标」",
                        );
                    }
                    Err(_) => {
                        tracing::error!(target: "icon_locker", "自动还原结果通道中断（独立 COM 线程异常退出）");
                    }
                }
            }
        });
        *lock_ok(self.watchdog_task.lock()) = Some(task);
        tracing::info!(target: "icon_locker", "桌面图标布局锁已启动（WM_DISPLAYCHANGE 防抖守护就绪）");
        Ok(())
    }

    async fn stop(&self) -> Result<(), ModuleError> {
        if !self.running.swap(false, Ordering::SeqCst) {
            return Ok(()); // 幂等：已在停止态。
        }
        // 停止守护线程（WM_CLOSE → Join）。
        if let Some(watchdog) = lock_ok(self.watchdog.lock()).take() {
            drop(watchdog);
        }
        // 取消防抖信号消费任务。
        if let Some(task) = lock_ok(self.watchdog_task.lock()).take() {
            task.abort();
        }
        tracing::info!(target: "icon_locker", "桌面图标布局锁已停止（防抖守护已退出）");
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IconCoordinate;
    use std::collections::HashMap;

    fn sample_profile(id: &str, name: &str) -> IconLayoutProfile {
        IconLayoutProfile {
            id: id.into(),
            name: name.into(),
            topology_fingerprint: "P:0,0,2560x1440".into(),
            icon_positions: HashMap::from([(
                "此电脑".to_string(),
                IconCoordinate { x: 10, y: 20 },
            )]),
        }
    }

    #[test]
    fn profile_lifecycle_is_keyed_by_id() {
        let module = IconLockerModule::new(vec![sample_profile("a", "A")]);
        assert!(module.remove_profile("a"));
        assert!(!module.remove_profile("a"));
    }

    #[test]
    fn auto_restore_defaults_on_and_settable() {
        let module = IconLockerModule::new(Vec::new());
        assert!(module.auto_restore(), "自动还原默认应开启");
        module.set_auto_restore(false);
        assert!(!module.auto_restore());
    }

    #[test]
    fn pick_auto_restore_prefers_active_then_newest() {
        let state = ModuleState {
            profiles: StdMutex::new(vec![
                sample_profile("old", "旧方案"),
                sample_profile("new", "新方案"),
            ]),
            active_profile: StdMutex::new(Some("old".into())),
            auto_restore: AtomicBool::new(true),
            bus: StdMutex::new(None),
        };
        assert_eq!(
            pick_auto_restore_profile(&state).map(|p| p.id),
            Some("old".into()),
            "优先还原最近活动方案"
        );
        *lock_ok(state.active_profile.lock()) = None;
        assert_eq!(
            pick_auto_restore_profile(&state).map(|p| p.id),
            Some("new".into()),
            "无活动方案时回退到最新保存"
        );
    }

    #[test]
    fn restore_without_profile_is_a_clear_error() {
        let state = ModuleState {
            profiles: StdMutex::new(Vec::new()),
            active_profile: StdMutex::new(None),
            auto_restore: AtomicBool::new(true),
            bus: StdMutex::new(None),
        };
        let err = restore_active(&state).expect_err("无方案时应报错而非静默成功");
        assert!(err.to_string().contains("尚无已保存的布局方案"));
    }

    // ---- P2-12：拓扑指纹参与还原校验（v0.6.2） ----

    /// 指纹判定真值表：仅「非空且逐字节相等」才允许自动还原。
    ///
    /// 这是"旧坐标刷到不同桌面"误伤问题的最后一道闸门——审计报告指出指纹字段
    /// 在旧实现中从不参与校验，沦为死数据。
    #[test]
    fn topology_fingerprint_must_match_exactly() {
        let saved = "P:0,0,2560x1440|S:-1920,0,1920x1080";
        assert!(
            profile_matches_topology(saved, saved),
            "拓扑一致（如外接屏重新插回）应允许还原"
        );
        assert!(
            !profile_matches_topology(saved, "P:0,0,1920x1080"),
            "拓扑不一致（换了显示器 / 改了分辨率）必须跳过还原"
        );
        assert!(
            !profile_matches_topology("", "P:0,0,1920x1080"),
            "空指纹视为来源不明，不得应用"
        );
        assert!(!profile_matches_topology("", ""), "双方皆空同样视为不匹配");
        // 大小写 / 空白差异即视为不同拓扑（指纹由本工具生成，不存在这些变体）。
        assert!(!profile_matches_topology(
            "P:0,0,2560x1440",
            "p:0,0,2560x1440"
        ));
    }

    /// 指纹不匹配时自动还原**不执行 COM 还原**且按设计成功返回（跳过 ≠ 失败）。
    #[test]
    fn restore_active_skips_on_topology_mismatch() {
        // 用一个必然不等于当前拓扑的指纹构造方案。
        let mut profile = sample_profile("mismatched", "外接屏方案");
        profile.topology_fingerprint = "S:99999,99999,1x1".into();
        let state = ModuleState {
            profiles: StdMutex::new(vec![profile]),
            active_profile: StdMutex::new(Some("mismatched".into())),
            auto_restore: AtomicBool::new(true),
            bus: StdMutex::new(None),
        };
        // 跳过路径返回 Ok（"按设计不还原"），不产生错误、不触碰 COM。
        let outcome = restore_active(&state);
        assert!(
            outcome.is_ok(),
            "拓扑不匹配应跳过而非报错，实际: {outcome:?}"
        );
    }
}
