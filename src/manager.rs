//! # 模块调度管理器（阶段二）
//!
//! [`ModuleManager`] 是阶段一 [`ToolModule`](crate::modules::ToolModule) 契约的调度层：
//! 持有全部已注册守护模块的 `Arc` 句柄，并把生命周期操作与阶段二的事件总线直接组合——
//! 每次 toggle 落定后**主动广播** [`AppEvent::ModuleStatusChanged`]，携带模块的
//! **最终真实状态**，供 UI / Agent 层据此刷新与回滚。
//!
//! # 并发与死锁模型
//!
//! - **调度器自身零锁**：注册完成后管理器以 `&self` 共享（`Arc<ModuleManager>`）；
//!   `toggle` 只读遍历注册表并调度模块，**不持有任何锁跨越 `await`**。
//! - **变迁串行化下沉到模块内部**：同一模块的并发启停由各模块内部的并发原语
//!   （如阶段一 `popup_blocker` 的生命周期互斥锁）串行收敛；不同模块的启停天然并行。
//! - **广播在操作落定之后**：`start` / `stop` 返回后（无论成败）才读取真实状态并发布，
//!   因此总线事件永不早于事实——UI 看到的事件即模块的最终状态，杜绝虚假状态。
//! - **失败也广播**：启动失败时模块保持停止态，管理器同样发布 `is_running = false`
//!   事件让 UI 回滚开关，随后才把错误上抛给调用方。
//!
//! 全局不存在任何互斥锁或锁序依赖，因此不存在死锁路径。

use crate::bus::{AppEvent, EventBus};
use crate::modules::{ModuleError, ToolModule};
use std::collections::HashMap;
use std::sync::Arc;

/// 模块元数据快照（供 UI 列表一次性拉取）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleMeta {
    /// 模块唯一标识符（如 `"popup_blocker"`）。
    pub id: &'static str,
    /// 模块人类可读名称。
    pub display_name: &'static str,
    /// 模块功能描述。
    pub description: &'static str,
    /// 模块当前运行状态。
    pub running: bool,
}

/// 模块调度管理器。
///
/// 生命周期：
/// 1. 构造后经 `register` 完成启动期注册（独占借用仅在装配阶段使用，不进入运行期共享路径）；
/// 2. 包装为 [`SharedManager`]（`Arc`）后，运行期全部操作走 `&self` 共享接口。
pub struct ModuleManager {
    /// 模块注册表：`id -> 模块实例`。键为 `&'static str` 以零开销索引。
    modules: HashMap<&'static str, Arc<dyn ToolModule>>,
    /// 事件总线：生命周期状态变更的广播出口。
    bus: EventBus,
}

impl ModuleManager {
    /// 绑定事件总线构造管理器。
    pub fn new(bus: EventBus) -> Self {
        Self {
            modules: HashMap::new(),
            bus,
        }
    }

    /// 注册一个模块实例（装配期调用）。
    ///
    /// 同 ID 重复注册以后者覆盖前者。
    pub fn register(&mut self, module: Arc<dyn ToolModule>) {
        self.modules.insert(module.id(), module);
    }

    /// 按 ID 取得模块句柄（供 UI / 调度层直接调用模块级操作）。
    pub fn get_module(&self, id: &str) -> Option<Arc<dyn ToolModule>> {
        self.modules.get(id).cloned()
    }

    /// 切换模块运行状态（启动 / 停止），并在操作落定后向总线广播真实状态。
    ///
    /// - `enable = true` 请求启动，`false` 请求停止；
    /// - 请求状态与当前状态一致时为幂等空操作（`Ok(false)`），但**仍会广播**一次
    ///   当前状态，使 UI 与底层事实强制对齐；
    /// - 模块不存在时返回错误且**不**广播；
    /// - 模块启停失败时：先广播其真实（未变更）状态，再向上抛出错误。
    ///
    /// 返回本次是否实际触发了状态变迁。
    pub async fn toggle(&self, id: &str, enable: bool) -> Result<bool, ModuleError> {
        let module = self
            .modules
            .get(id)
            .ok_or_else(|| -> ModuleError { format!("未注册的模块: {id}").into() })?;

        // 仅在状态确需变迁时调用模块生命周期操作；模块自身幂等，双保险。
        let changed = module.is_running() != enable;
        let result = if changed {
            if enable {
                module.start().await
            } else {
                module.stop().await
            }
        } else {
            Ok(())
        };

        // 操作落定后读取真实状态并广播（无论成败）——事件反映事实，绝不反映请求意图。
        let actual_running = module.is_running();
        self.bus.publish(AppEvent::ModuleStatusChanged {
            id: id.to_string(),
            is_running: actual_running,
        });

        result?;
        Ok(changed)
    }

    /// 拉取全部模块元数据（按 ID 排序，保证 UI 列表顺序稳定）。
    pub fn get_metadata_list(&self) -> Vec<ModuleMeta> {
        let mut metas: Vec<ModuleMeta> = self
            .modules
            .values()
            .map(|module| ModuleMeta {
                id: module.id(),
                display_name: module.display_name(),
                description: module.description(),
                running: module.is_running(),
            })
            .collect();
        metas.sort_by_key(|meta| meta.id);
        metas
    }
}

/// 共享调度器句柄：装配完成后在 UI / 总线转发等任务间共享的统一入口。
pub type SharedManager = Arc<ModuleManager>;

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// 测试桩模块：以真实模块相同的“内部锁 + 原子标志”结构实现契约，
    /// 允许注入启动失败、统计生命周期调用次数。
    struct MockModule {
        id: &'static str,
        inner: Arc<MockInner>,
    }

    struct MockInner {
        /// 串行化启停变迁（与生产模块同构的并发模型）。
        lifecycle: Mutex<()>,
        running: AtomicBool,
        fail_next_start: AtomicBool,
        start_calls: AtomicUsize,
        stop_calls: AtomicUsize,
    }

    impl MockModule {
        fn new(id: &'static str) -> (Arc<Self>, Arc<dyn ToolModule>) {
            let module = Arc::new(MockModule {
                id,
                inner: Arc::new(MockInner {
                    lifecycle: Mutex::new(()),
                    running: AtomicBool::new(false),
                    fail_next_start: AtomicBool::new(false),
                    start_calls: AtomicUsize::new(0),
                    stop_calls: AtomicUsize::new(0),
                }),
            });
            let erased: Arc<dyn ToolModule> = module.clone();
            (module, erased)
        }

        fn fail_next_start(&self) {
            self.inner.fail_next_start.store(true, Ordering::SeqCst);
        }

        fn start_calls(&self) -> usize {
            self.inner.start_calls.load(Ordering::SeqCst)
        }

        fn stop_calls(&self) -> usize {
            self.inner.stop_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ToolModule for MockModule {
        fn id(&self) -> &'static str {
            self.id
        }

        fn display_name(&self) -> &'static str {
            "测试桩模块"
        }

        fn description(&self) -> &'static str {
            "manager 单元测试用最小模块实现"
        }

        async fn start(&self) -> Result<(), ModuleError> {
            let _guard = self.inner.lifecycle.lock().await;
            if self.inner.running.load(Ordering::Acquire) {
                return Ok(()); // 幂等
            }
            self.inner.start_calls.fetch_add(1, Ordering::SeqCst);
            if self.inner.fail_next_start.swap(false, Ordering::SeqCst) {
                return Err("测试注入的启动失败".into());
            }
            self.inner.running.store(true, Ordering::Release);
            Ok(())
        }

        async fn stop(&self) -> Result<(), ModuleError> {
            let _guard = self.inner.lifecycle.lock().await;
            if !self.inner.running.load(Ordering::Acquire) {
                return Ok(()); // 幂等
            }
            self.inner.stop_calls.fetch_add(1, Ordering::SeqCst);
            self.inner.running.store(false, Ordering::Release);
            Ok(())
        }

        fn is_running(&self) -> bool {
            self.inner.running.load(Ordering::Acquire)
        }
    }

    /// 构造含一个 mock 模块、并暴露订阅端的测试夹具。
    fn fixture() -> (EventBus, SharedManager, Arc<MockModule>) {
        let bus = EventBus::new(16);
        let mut mgr = ModuleManager::new(bus.clone());
        let (raw, erased) = MockModule::new("mock");
        mgr.register(erased);
        (bus, Arc::new(mgr), raw)
    }

    #[tokio::test]
    async fn register_and_metadata_list() {
        let bus = EventBus::new(16);
        let mut mgr = ModuleManager::new(bus);
        let (_raw_a, erased_a) = MockModule::new("mock_a");
        let (_raw_b, erased_b) = MockModule::new("mock_b");
        mgr.register(erased_a);
        mgr.register(erased_b);

        let metas = mgr.get_metadata_list();
        assert_eq!(metas.len(), 2);
        assert!(metas.iter().all(|m| !m.running));
        // 按 id 稳定排序。
        assert!(metas.windows(2).all(|w| w[0].id <= w[1].id));

        assert!(mgr.get_module("mock_a").is_some());
        assert!(mgr.get_module("ghost").is_none());
    }

    #[tokio::test]
    async fn toggle_enable_broadcasts_true_and_transitions() {
        let (bus, shared, raw) = fixture();
        let mut rx = bus.subscribe();
        assert!(!raw.is_running());

        let changed = shared.toggle("mock", true).await.expect("启动应成功");
        assert!(changed, "首次启动应实际触发变迁");
        assert!(raw.is_running());

        let event = rx.recv().await.expect("总线应收到事件");
        match event {
            AppEvent::ModuleStatusChanged { id, is_running } => {
                assert_eq!(id, "mock");
                assert!(is_running);
            }
            other => panic!("收到意外事件: {other:?}"),
        }
    }

    #[tokio::test]
    async fn toggle_disable_broadcasts_false_and_transitions() {
        let (bus, shared, raw) = fixture();
        let mut rx = bus.subscribe();

        shared.toggle("mock", true).await.unwrap();
        let _ = rx.recv().await.unwrap(); // 消费启动事件

        let changed = shared.toggle("mock", false).await.expect("停止应成功");
        assert!(changed);
        assert!(!raw.is_running());

        match rx.recv().await.unwrap() {
            AppEvent::ModuleStatusChanged { is_running, .. } => assert!(!is_running),
            other => panic!("收到意外事件: {other:?}"),
        }
    }

    #[tokio::test]
    async fn toggle_noop_is_idempotent_but_still_syncs_bus() {
        let (bus, shared, raw) = fixture();
        let mut rx = bus.subscribe();

        shared.toggle("mock", true).await.unwrap();
        let _ = rx.recv().await.unwrap();

        // 重复请求相同状态：不触发二次启动，但广播一次当前状态（强制对齐）。
        let changed = shared.toggle("mock", true).await.expect("幂等请求应成功");
        assert!(!changed, "状态未变不应报告 changed");
        assert_eq!(raw.start_calls(), 1, "不应重复调用 start");

        match rx.recv().await.unwrap() {
            AppEvent::ModuleStatusChanged { is_running, .. } => assert!(is_running),
            other => panic!("收到意外事件: {other:?}"),
        }
    }

    #[tokio::test]
    async fn toggle_unknown_module_errors_without_broadcast() {
        let (bus, shared, _raw) = fixture();
        let mut rx = bus.subscribe();

        let err = shared
            .toggle("ghost", true)
            .await
            .expect_err("未知模块应报错");
        assert!(
            err.to_string().contains("ghost"),
            "错误信息应包含模块 ID: {err}"
        );
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "未知模块不应产生任何总线事件"
        );
    }

    #[tokio::test]
    async fn start_failure_broadcasts_false_then_errors() {
        let (bus, shared, raw) = fixture();
        let mut rx = bus.subscribe();
        raw.fail_next_start();

        let err = shared
            .toggle("mock", true)
            .await
            .expect_err("注入的失败应上抛");
        assert!(err.to_string().contains("注入"), "错误信息: {err}");
        assert!(!raw.is_running(), "失败后应保持停止态");

        // 失败也必须广播真实状态，供 UI 回滚开关。
        match rx.recv().await.unwrap() {
            AppEvent::ModuleStatusChanged { is_running, .. } => assert!(!is_running),
            other => panic!("收到意外事件: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_toggles_serialize_per_module() {
        let (_bus, shared, raw) = fixture();

        // 4 路并发启动：内部生命周期锁应保证 start 实际只执行一次，且全部收敛到运行态。
        let mut starters = Vec::new();
        for _ in 0..4 {
            let mgr = Arc::clone(&shared);
            starters.push(tokio::spawn(async move { mgr.toggle("mock", true).await }));
        }
        for handle in starters {
            handle.await.expect("任务应正常结束").expect("启动应成功");
        }
        assert!(raw.is_running());
        assert_eq!(raw.start_calls(), 1, "并发启动只应实际执行一次");

        // 4 路并发停止：同理只实际停止一次，最终一致收敛到停止态。
        let mut stoppers = Vec::new();
        for _ in 0..4 {
            let mgr = Arc::clone(&shared);
            stoppers.push(tokio::spawn(async move { mgr.toggle("mock", false).await }));
        }
        for handle in stoppers {
            handle.await.expect("任务应正常结束").expect("停止应成功");
        }
        assert!(!raw.is_running());
        assert_eq!(raw.stop_calls(), 1, "并发停止只应实际执行一次");
    }
}
