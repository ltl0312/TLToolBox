//! # TLToolBox · 模块生命周期 ↔ 事件总线集成测试（架构转型后）
//!
//! 本文件位于 `tests/`（集成测试），以**库目标** `tltoolbox` 为被测对象，从 crate
//! 外部按公开 API 契约验证跨模块装配的正确性——与各库内部的单元测试（可访问私有
//! 实现）互补，覆盖端到端链路：
//!
//! 1. **模块生命周期变更事件的广播机制**：真实模块（[`PopupBlockerModule`]，Windows
//!    下派生专用原生线程安装 Win32 事件钩子）经共享调度器启停后，事件总线必须收到
//!    携带模块**最终真实状态**的 [`AppEvent::ModuleStatusChanged`]，且顺序与事实一致；
//! 2. **幂等请求的状态同步**：重复请求同一目标状态不重复触发生命周期操作，但调度器
//!    仍广播一次当前状态（UI 与底层事实强制对齐）；
//! 3. **错误边界**：未注册模块 ID 返回错误且不产生任何总线事件。
//!
//! 转型说明：原 `agent_integration_tests.rs` 中的 Agent 工具（`ToggleModuleTool` /
//! `SystemDiagnosticTool`）链路随 agent 层整体退役，本文件保留与产品无关的
//! 纯本地调度契约。隔离策略：全程不触达 Slint UI，仅依赖 `tokio` 运行时，可离线、
//! 无图形会话地由 `cargo test` 执行。

use std::sync::Arc;
use std::time::Duration;
use tltoolbox::bus::{AppEvent, EventBus};
use tltoolbox::manager::{ModuleManager, SharedManager};
use tltoolbox::modules::popup_blocker::PopupBlockerModule;
use tltoolbox::modules::ToolModule;
use tokio::sync::broadcast;

/// 总线状态事件接收的超时窗口（真实模块启停为本地原语操作，正常应毫秒级送达）。
const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

/// 集成测试默认总线容量（远大于事件数，杜绝 Lagged 干扰断言）。
const TEST_BUS_CAPACITY: usize = 64;

/// 夹具：新建事件总线，注册一个**真实** `popup_blocker` 模块并包装为共享调度器。
///
/// 返回（总线、共享调度器、模块句柄）三元组；总线保留引用以便测试额外订阅。
fn fixture() -> (EventBus, SharedManager, Arc<PopupBlockerModule>) {
    let bus = EventBus::new(TEST_BUS_CAPACITY);
    let mut mgr = ModuleManager::new(bus.clone());
    let module = Arc::new(PopupBlockerModule::new());
    mgr.register(module.clone());
    (bus, Arc::new(mgr), module)
}

/// 在超时窗口内从总线读取下一条事件并断言其为模块状态广播，返回 (id, is_running)。
async fn expect_module_status_event(rx: &mut broadcast::Receiver<AppEvent>) -> (String, bool) {
    let event = tokio::time::timeout(EVENT_TIMEOUT, rx.recv())
        .await
        .expect("等待总线事件超时：模块状态事件未送达")
        .expect("总线接收错误（通道可能已关闭）");
    match event {
        AppEvent::ModuleStatusChanged { id, is_running } => (id, is_running),
        other => panic!("收到意外事件变体: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. 模块生命周期变更事件的广播机制
// ---------------------------------------------------------------------------

/// 生命周期变更事件与事实一致：启动广播 true、停止广播 false，且均携带真实模块 ID。
#[tokio::test]
async fn module_lifecycle_broadcasts_authoritative_state_events() {
    let (_bus, shared, module) = fixture();
    let mut rx = _bus.subscribe();
    assert!(!module.is_running(), "新模块应处于停止态");

    // 启动：toggle 返回 changed=true，模块真实进入运行态，总线广播 (popup_blocker, true)。
    let changed = shared
        .toggle("popup_blocker", true)
        .await
        .expect("启动模块失败");
    assert!(changed, "首次启动应报告真实变迁");
    assert!(module.is_running(), "启动后模块应处于运行态");
    let (id, is_running) = expect_module_status_event(&mut rx).await;
    assert_eq!(id, "popup_blocker", "事件应携带模块真实 ID");
    assert!(is_running, "启动事件应广播运行态 true");

    // 停止：同理广播 (popup_blocker, false)，且模块真实回到停止态。
    let changed = shared
        .toggle("popup_blocker", false)
        .await
        .expect("停止模块失败");
    assert!(changed, "停止应报告真实变迁");
    assert!(!module.is_running(), "停止后模块应处于停止态");
    let (id, is_running) = expect_module_status_event(&mut rx).await;
    assert_eq!(id, "popup_blocker");
    assert!(!is_running, "停止事件应广播停止态 false");
}

/// 幂等请求不重复触发生命周期操作，但调度器仍广播一次当前状态（强制 UI 与事实对齐）。
#[tokio::test]
async fn idempotent_toggle_still_syncs_bus_with_current_state() {
    let (_bus, shared, module) = fixture();
    let mut rx = _bus.subscribe();

    shared
        .toggle("popup_blocker", true)
        .await
        .expect("启动失败");
    let (_id, _running) = expect_module_status_event(&mut rx).await;

    // 重复请求启动：模块已在运行 → changed=false（不二次启动），但事件照发。
    let changed = shared
        .toggle("popup_blocker", true)
        .await
        .expect("幂等请求应成功");
    assert!(!changed, "状态未变时不得报告 changed");
    assert!(module.is_running(), "模块应保持运行态");
    let (id, is_running) = expect_module_status_event(&mut rx).await;
    assert_eq!(id, "popup_blocker");
    assert!(is_running, "幂等同步事件应广播当前真实状态");
}

// ---------------------------------------------------------------------------
// 2. 错误边界
// ---------------------------------------------------------------------------

/// 未知模块 ID：调度器返回错误且**不**广播任何事件（避免 UI 收到无意义状态）。
#[tokio::test]
async fn unknown_module_toggle_errors_without_broadcast() {
    let (_bus, shared, _module) = fixture();
    let mut rx = _bus.subscribe();

    let err = shared
        .toggle("ghost_module", true)
        .await
        .expect_err("未注册模块应报错");
    assert!(
        err.to_string().contains("ghost_module"),
        "错误信息应点名模块 ID: {err}"
    );
    assert!(
        matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
        "未知模块不得产生任何总线事件"
    );
}
