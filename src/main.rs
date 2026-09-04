// 消除控制台黑框：release 构建（not(debug_assertions)）下把子系统声明为
// Windows GUI（/SUBSYSTEM:WINDOWS），进程不再附带控制台窗口；debug 构建
// 保持默认控制台子系统，便于开发期观察 tracing 输出。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! # TLToolBox 主装配与主循环绑定（可执行入口 · 单栏工具箱界面）
//!
//! 自大模型 Agent 工作站转型为**轻量级原生 Windows 工具箱**后，本文件是纯本地
//! 装配点：把注册表开机自启（[`autostart`](tltoolbox::autostart)）、事件总线、
//! 模块调度器与 [`ui/app.slint`](https://slint.dev) 界面（经 `build.rs` 编译、
//! `slint::include_modules!()` 引入）串成完整的常驻桌面应用。UI 为 480 × 560
//! 的**单栏「桌面实用工具箱」**：顶部全局控制栏（标题/小版本号/开机自启/
//! 一键全开全关）+ ScrollView 流式模块卡片列表（名称 / 描述 / 运行状态徽标 /
//! 物理开关）；双栏时代的内嵌运行日志控制台已退役，日志改由 `tracing` 承载。
//! 装配顺序与职责：
//!
//! 1. **静默启动识别**：解析命令行 `--silent`（系统经注册表 Run 键拉起本程序时
//!    附加，见 [`autostart::SILENT_ARG`](tltoolbox::autostart::SILENT_ARG)）；
//!    常驻层据此抑制前台打扰，托盘最小化行为由常驻层消费；
//! 2. **配置加载**：`ConfigManager::default().load()` 负责首次运行自动落盘默认
//!    `config/tltoolbox.toml`，随后异步加载（自启标志 / 托盘行为 / 自动启动模块列表）；
//! 3. **注册表自启同步**：配置为准——`auto_start_windows` 与实际注册表
//!    `HKCU\...\CurrentVersion\Run` 状态不一致时立即收敛（配置开而缺失/路径漂移
//!    → 补写「当前 exe --silent」；配置关而残留 → 删除）。同步失败仅告警降级，
//!    不阻断启动（优雅同步）；
//! 4. **总线装配**：`EventBus`（`tokio::sync::broadcast`，容量 256）注入模块管理器
//!    作为广播出口；
//! 5. **模块注册**：注册内置常驻守护模块（[`PopupBlockerModule`] 弹窗拦截、
//!    [`KeepAwakeModule`] 系统防休眠、[`ClipboardPurifierModule`] 剪贴板纯文本净化）；
//! 6. **自动启动模块**：按配置对 `auto_start_modules` 执行 `toggle(id, true)`——
//!    **先于 UI 装配**，启动期广播事件因尚无订阅者而被总线按设计丢弃，随后以调度
//!    层**真实状态快照**填充 UI 初始模型，保证“界面即事实”；
//! 7. **UI 实例化与数据注入**：`MainWindow::new()` 后一次性写入模块列表
//!    `VecModel`，并注入开机自启生效状态（注册表为准）与应用版本号；`VecModel`
//!    与全部 Slint 模型一样**非 `Send`**，自创建后终生驻留 UI 主线程；
//! 8. **总线 → UI 桥**：常驻 Tokio 任务订阅 `EventBus`，把模块状态落定事件经
//!    [`slint::invoke_from_event_loop`] **重定向到 UI 主线程的消息循环**再刷新
//!    模型。`VecModel`/`ModelRc` 从不跨线程移动：跨线程载荷只有
//!    `Weak<MainWindow>`（Slint 官方保证 `Send`）与调度器 `Arc` 等 `Send` 数据，
//!    模型改写一律发生在事件循环闭包（UI 线程）内部；
//! 9. **回调绑定**：模块开关 `toggle_module` 派发异步启停；全局「开机自启」
//!    `toggle_autostart` 写注册表并持久化配置后回读收敛；「全部启用 / 全部停用」
//!    `toggle_all_modules` 复用托盘的全量切换路径——三者落定后的真实状态均回流
//!    UI（失败场景自然回滚开关）；
//! 10. **托盘装配与生命周期控制**（桌面常驻核心）：`tray::spawn` 派生**独立托盘
//!     线程**（Win32 消息泵），把菜单 / 双击指令发布为 [`AppEvent::TrayAction`]；
//!     关闭按钮按 `minimize_to_tray` 配置**隐藏到托盘**（`CloseRequestResponse`）；
//!     `--silent` 静默自启且托盘就绪时主窗口保持隐藏（不调用 `show()`）；独立“生命周期控制器”订阅总线——双击 /
//!     “显示主窗口”经 `invoke_from_event_loop` 在 UI 线程还原窗口，“全部模块：开启/
//!     关闭”逐模块 toggle，“退出程序”调度 `slint::quit_event_loop` 进入收尾；模块
//!     状态事件回写托盘菜单文案（`TrayControl::sync_all_modules`，非阻塞投递）。

slint::include_modules!();

use slint::{CloseRequestResponse, ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::Mutex;
use tltoolbox::autostart;
use tltoolbox::bus::{AppEvent, EventBus, TrayAction};
use tltoolbox::config::ConfigManager;
use tltoolbox::manager::{ModuleManager, SharedManager};
use tltoolbox::modules::clipboard_purifier::ClipboardPurifierModule;
use tltoolbox::modules::keep_awake::KeepAwakeModule;
use tltoolbox::modules::popup_blocker::PopupBlockerModule;
use tltoolbox::tray::{self, TrayControl};

// ---------------------------------------------------------------------------
// UI 模型辅助（仅允许在 UI 主线程执行）
// ---------------------------------------------------------------------------

/// 把调度层元数据快照转换为 UI 模块列表条目。
fn module_items_from_manager(manager: &ModuleManager) -> Vec<ModuleItem> {
    manager
        .get_metadata_list()
        .into_iter()
        .map(|meta| ModuleItem {
            id: SharedString::from(meta.id),
            name: SharedString::from(meta.display_name),
            desc: SharedString::from(meta.description),
            enabled: meta.running,
        })
        .collect()
}

/// 【UI 线程内】以调度层真实状态整体重建模块列表模型。
///
/// 采用 `set_vec`（模型 reset 语义）而非逐行更新：reset 会让 `for` 中继重建行元素、
/// 使 Switch 的 `checked: item.enabled` 绑定重新求值，从而无论用户点击是否已改写
/// Switch 内部状态，视觉开关最终都会收敛到底层模块的真实运行态（含启动失败回滚）。
/// 模块数量极少且 toggle 为低频事件，重建成本可忽略。
fn refresh_modules_model(ui: &MainWindow, manager: &ModuleManager) {
    let items = module_items_from_manager(manager);
    let model: ModelRc<ModuleItem> = ui.get_modules();
    if let Some(vec_model) = model.as_any().downcast_ref::<VecModel<ModuleItem>>() {
        vec_model.set_vec(items);
    }
}

// ---------------------------------------------------------------------------
// 总线 → UI 投递（跨线程边界；模型改写严格发生在 UI 线程闭包内）
// ---------------------------------------------------------------------------

/// 把“模块状态已变更”事件以 UI 线程闭包形式投递，闭包内重拉调度层真实状态刷新列表。
///
/// 返回 `false` 表示事件循环已不可用（窗口关闭 / 后端退出），调用方应终止转发。
fn deliver_module_sync(ui_weak: &slint::Weak<MainWindow>, manager: SharedManager) -> bool {
    let weak = ui_weak.clone();
    let queued = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            refresh_modules_model(&ui, &manager);
        }
    });
    queued.is_ok()
}

/// 常驻总线 → UI 转发任务：订阅 [`EventBus`]，把模块状态落定事件转发到 UI 主线程。
///
/// 跨线程载荷仅含 `Weak<MainWindow>`（Slint 保证 `Send`）与调度器 `Arc`（`Send`）；
/// 真正的模型改写（`refresh_modules_model`）只会在事件循环闭包——即 UI 线程——内执行，
/// 杜绝把 `VecModel` / `ModelRc` 移出 UI 线程。托盘指令与日志事件不属本任务职责，
/// 分别由生命周期控制器 / tracing 承载。
async fn forward_module_events(
    mut rx: broadcast::Receiver<AppEvent>,
    manager: SharedManager,
    ui_weak: slint::Weak<MainWindow>,
) {
    loop {
        match rx.recv().await {
            Err(broadcast::error::RecvError::Closed) => {
                // 总线已随应用收尾关闭：退出转发任务。
                break;
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(target: "main", "事件总线消费滞后，已跳过 {skipped} 条事件");
            }
            Ok(AppEvent::ModuleStatusChanged { .. }) => {
                // 状态事件实时性优先：立即重拉真实状态刷新模块列表（含开关回滚）。
                if !deliver_module_sync(&ui_weak, Arc::clone(&manager)) {
                    break;
                }
            }
            Ok(_) => {
                // 托盘指令（TrayAction）与日志（AppLogAppended）不属本任务职责。
            }
        }
    }
    tracing::debug!(target: "main", "总线 → UI 转发任务已退出");
}

// ---------------------------------------------------------------------------
// 托盘生命周期控制（托盘指令 → UI / 调度层动作）
// ---------------------------------------------------------------------------

/// 判断“全部模块”当前是否**全部处于运行态**（聚合开关，供托盘菜单文案）。
///
/// 无任何已注册模块时返回 `false`（“全部开启”对空集无意义）。
fn all_modules_enabled(manager: &ModuleManager) -> bool {
    let metas = manager.get_metadata_list();
    !metas.is_empty() && metas.iter().all(|meta| meta.running)
}

/// 【任意线程可调用】请求显示主窗口：经 `invoke_from_event_loop` 排队到
/// UI 线程执行（还原最小化 + show），杜绝跨线程触碰 Slint 窗口对象。
fn request_show_main_window(ui_weak: &slint::Weak<MainWindow>) {
    let weak = ui_weak.clone();
    let queued = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            // 若窗口曾被最小化到任务栏，先还原再显示（show 幂等）。
            ui.window().set_minimized(false);
            let _ = ui.show();
        }
    });
    if queued.is_err() {
        tracing::warn!(target: "main", "无法投递「显示主窗口」：UI 事件循环已不可用");
    }
}

/// 把全部模块统一切换到 `enable` 指定状态（托盘“全部模块：开启/关闭”、
/// UI「全部启用 / 全部停用」共用）。
///
/// 顺序逐模块 toggle：调度器在每次落定后广播真实状态（驱动 UI 开关与托盘
/// 菜单文案回流）；单模块失败仅告警并继续，不中断整体操作。
async fn set_all_modules(manager: &SharedManager, enable: bool) {
    for meta in manager.get_metadata_list() {
        if meta.running == enable {
            continue; // 已处于目标状态，幂等跳过
        }
        if let Err(err) = manager.toggle(meta.id, enable).await {
            tracing::warn!(
                target: "main",
                "全量切换：模块 {} -> {} 失败: {err}",
                meta.id,
                enable
            );
        }
    }
}

/// 常驻“生命周期控制器”：独立订阅 [`EventBus`]，把托盘指令翻译为
/// UI / 调度层动作，并把模块真实状态回写到托盘菜单文案。
///
/// # 线程安全模型（与 [`crate::tray`] 呼应，杜绝死锁）
///
/// - 本任务运行于 Tokio 工作线程，全程**无阻塞等待**：UI 操作一律经
///   `slint::invoke_from_event_loop` 排队到 UI 线程；模块启停经异步
///   `manager.toggle`（调度器零跨 `await` 锁）；托盘回写经非阻塞投递
///   （[`TrayControl::sync_all_modules`]），不等待托盘线程应答；
/// - 收到“退出程序”后调度 `slint::quit_event_loop`（由 UI 线程执行），
///   随即退出本任务——后续收尾（停模块、关托盘）由主线程接管。
async fn lifecycle_controller(
    mut rx: broadcast::Receiver<AppEvent>,
    manager: SharedManager,
    ui_weak: slint::Weak<MainWindow>,
    tray_control: Option<TrayControl>,
) {
    let mut quit_scheduled = false;
    loop {
        match rx.recv().await {
            Err(broadcast::error::RecvError::Closed) => break,
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(target: "main", "生命周期控制器消费滞后，已跳过 {skipped} 条事件");
            }
            Ok(AppEvent::ModuleStatusChanged { .. }) => {
                // 任一模块状态落定 → 聚合状态可能变化 → 回写托盘菜单文案。
                if let Some(tray) = &tray_control {
                    tray.sync_all_modules(all_modules_enabled(&manager));
                }
            }
            Ok(AppEvent::TrayAction(action)) => match action {
                TrayAction::ShowWindow => request_show_main_window(&ui_weak),
                TrayAction::ToggleAllModules(enable) => {
                    let mgr = Arc::clone(&manager);
                    tokio::spawn(async move {
                        set_all_modules(&mgr, enable).await;
                    });
                }
                TrayAction::ExitApp => {
                    tracing::info!(target: "main", "托盘「退出程序」触发，调度应用平滑收尾");
                    quit_scheduled = true;
                    let _ = slint::invoke_from_event_loop(|| {
                        let _ = slint::quit_event_loop();
                    });
                }
            },
            Ok(AppEvent::AppLogAppended { .. }) => {
                // 双栏日志控制台已退役：运行日志由 tracing 承载，此处忽略。
            }
        }
        if quit_scheduled {
            break;
        }
    }
    tracing::debug!(target: "main", "生命周期控制器已退出");
}

// ---------------------------------------------------------------------------
// 主入口
// ---------------------------------------------------------------------------

/// 应用错误统一别名。
type AppError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() -> Result<(), AppError> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    // ---- 0. 静默启动识别：注册表 Run 键自启时携带 --silent（常驻层据此抑制打扰）。 ----
    let silent_launch = std::env::args().any(|arg| arg == autostart::SILENT_ARG);
    if silent_launch {
        tracing::info!(target: "main", "检测到 --silent：本次由系统开机自启拉起");
    }

    // ---- 1. 配置加载：首次运行自动落盘默认 TOML，随后异步加载。 ----
    let config_mgr = Arc::new(ConfigManager::default());
    let app_config = config_mgr.load().await.map_err(|err| -> AppError {
        tracing::error!(target: "main", "配置加载失败: {err}");
        err.into()
    })?;
    tracing::info!(
        target: "main",
        path = %config_mgr.path().display(),
        auto_start_windows = app_config.auto_start_windows,
        minimize_to_tray = app_config.minimize_to_tray,
        "配置已加载（自动启动模块: {:?}）",
        app_config.auto_start_modules
    );

    // 运行期配置句柄：UI「开机自启」切换后在此更新并落盘（配置为准）。
    let runtime_config = Arc::new(Mutex::new(app_config.clone()));

    // ---- 2. 注册表自启同步（配置为准，优雅降级）：auto_start_windows 与
    //       HKCU\...\CurrentVersion\Run 实际状态不一致时立即收敛。 ----
    if let Err(err) = autostart::synchronize_autostart(app_config.auto_start_windows) {
        tracing::warn!(
            target: "main",
            "注册表自启状态同步失败（不阻断启动，下次运行将重试）: {err}"
        );
    } else {
        tracing::info!(
            target: "main",
            "注册表自启状态已与配置收敛（auto_start_windows = {}）",
            app_config.auto_start_windows
        );
    }

    // ---- 3. 事件总线：注入模块调度器作为广播出口。 ----
    let event_bus = EventBus::default();

    // ---- 4. 模块管理器装配：先注册全部内置常驻守护模块。 ----
    //      弹窗拦截模块的初始黑名单取自配置 `popup_blacklist`（含默认广告关键词），
    //      运行期可经 `PopupBlockerModule::update_rules` 热更新而无需重启原生泵线程。
    let mut module_mgr = ModuleManager::new(event_bus.clone());
    let popup_rules_count = app_config.popup_blacklist.len();
    module_mgr.register(Arc::new(PopupBlockerModule::with_rules(
        app_config.popup_blacklist.clone(),
    )));
    tracing::info!(
        target: "main",
        "弹窗拦截黑名单已注入 PopupBlockerModule（{} 条关键词，运行期可热更新）",
        popup_rules_count
    );
    //      系统防休眠模块：无参构造即可注册。注册即出现在 UI 模块列表，但**不**进入
    //      默认自动启动列表——阻止系统睡眠属「显式开启才合理」的电源行为改变，避免
    //      首次安装即静默改写用户机器的空闲休眠策略（由用户在 UI / 托盘手动开启）。
    module_mgr.register(Arc::new(KeepAwakeModule::new()));
    //      剪贴板纯文本净化模块：无参构造即可注册。同样**不**进入默认自动启动列表——
    //      剪贴板行为改写（自动剥离富文本格式）属用户预期敏感的操作，显式开启才合理。
    module_mgr.register(Arc::new(ClipboardPurifierModule::new()));
    let shared_mgr: SharedManager = Arc::new(module_mgr);
    let registered_modules: Vec<&str> = shared_mgr
        .get_metadata_list()
        .iter()
        .map(|meta| meta.id)
        .collect();
    tracing::info!(
        target: "main",
        "模块注册完成（{} 个）: {}",
        registered_modules.len(),
        registered_modules.join(", ")
    );

    // ---- 5. 自动启动模块：先于 UI 装配执行，保证初始 UI 快照即真实状态。 ----
    //      此时总线尚无订阅者，启动期事件按 bus 模块的设计语义被丢弃——真实状态随后
    //      经 get_metadata_list() 快照注入 UI，无需依赖事件回放。
    for module_id in &app_config.auto_start_modules {
        if shared_mgr.get_module(module_id).is_none() {
            tracing::warn!(target: "main", "自动启动列表含未注册模块: {module_id}，已跳过");
            continue;
        }
        match shared_mgr.toggle(module_id, true).await {
            Ok(_) => tracing::info!(target: "main", "自动启动模块: {module_id}"),
            Err(err) => tracing::error!(target: "main", "自动启动模块 {module_id} 失败: {err}"),
        }
    }

    // ---- 6. UI 实例化与初始数据注入（全部模型驻留 UI 主线程）。 ----
    let ui = MainWindow::new().map_err(|err| -> AppError {
        format!("Slint 界面初始化失败（当前会话可能缺少可用的图形环境）: {err}").into()
    })?;

    ui.set_modules(ModelRc::new(VecModel::from(module_items_from_manager(&shared_mgr))));
    ui.set_autostart_enabled(autostart::is_autostart_enabled());
    ui.set_app_version(SharedString::from(env!("CARGO_PKG_VERSION")));
    tracing::info!(
        target: "main",
        "UI 已实例化，初始注入 {} 个模块",
        ui.get_modules().row_count()
    );

    // ---- 7. 总线 → UI 桥：订阅总线并启动常驻转发任务（模块状态 → UI 刷新）。 ----
    let bus_rx = event_bus.subscribe();
    let forwarder_ui = ui.as_weak();
    let forwarder_mgr = Arc::clone(&shared_mgr);
    tokio::spawn(async move {
        forward_module_events(bus_rx, forwarder_mgr, forwarder_ui).await;
    });
    tracing::info!(target: "main", "总线 → UI 转发任务已启动");

    // ---- 8. 回调绑定：UI 控件 → 异步动作；落定后的真实状态回流刷新（含失败回滚）。 ----
    //      回调在 UI 线程触发；实际动作派发到 Tokio 任务执行，形成
    //      “请求 → 事实 → 视图”闭环。

    // 8.1 模块开关拨动 → 异步调度模块启停。
    let manager_for_toggle = Arc::clone(&shared_mgr);
    ui.on_toggle_module(move |id, enable| {
        let manager = Arc::clone(&manager_for_toggle);
        let id_text = id.to_string();
        tokio::spawn(async move {
            if let Err(err) = manager.toggle(&id_text, enable).await {
                tracing::error!(target: "main", "切换模块 {id_text} -> {enable} 失败: {err}");
                // 失败时调度器已广播真实（未变更）状态驱动 UI 回滚开关。
            }
        });
    });

    // 8.2 全局「开机自启」拨动 → 写注册表 + 持久化配置 + UI 回读收敛。
    let autostart_mgr = Arc::clone(&config_mgr);
    let autostart_cfg = Arc::clone(&runtime_config);
    let autostart_ui = ui.as_weak();
    ui.on_toggle_autostart(move |enable| {
        let mgr = Arc::clone(&autostart_mgr);
        let cfg_lock = Arc::clone(&autostart_cfg);
        let weak = autostart_ui.clone();
        tokio::spawn(async move {
            // 1) 注册表镜像（同步 API 走 spawn_blocking，不占用 UI / 运行时工作线程）。
            let applied = match tokio::task::spawn_blocking(move || autostart::set_autostart(enable)).await {
                Ok(Ok(())) => true,
                Ok(Err(err)) => {
                    tracing::error!(target: "main", "开机自启写入失败: {err}");
                    false
                }
                Err(err) => {
                    tracing::error!(target: "main", "开机自启任务执行失败: {err}");
                    false
                }
            };
            // 2) 写入成功 → 把用户意图持久化到配置（配置为准：下次启动据此收敛注册表）。
            if applied {
                let snapshot = {
                    let mut cfg = cfg_lock.lock().await;
                    cfg.auto_start_windows = enable;
                    cfg.clone()
                };
                if let Err(err) = mgr.save(&snapshot).await {
                    tracing::error!(target: "main", "自启意图写入配置失败: {err}");
                }
            }
            // 3) 回读注册表真实状态刷新开关：失败路径自然回弹为原状态。
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_autostart_enabled(autostart::is_autostart_enabled());
                }
            });
        });
    });

    // 8.3 「全部启用 / 全部停用」→ 与托盘全量切换共用同一实现。
    let all_mgr = Arc::clone(&shared_mgr);
    ui.on_toggle_all_modules(move |enable| {
        let mgr = Arc::clone(&all_mgr);
        tokio::spawn(async move {
            set_all_modules(&mgr, enable).await;
        });
    });

    // ---- 9. 托盘装配与生命周期控制（桌面常驻核心机制）。 ----
    let tray_handle = match tray::spawn(event_bus.clone(), all_modules_enabled(&shared_mgr)) {
        Ok(handle) => {
            tracing::info!(
                target: "main",
                "系统托盘已就绪（右键菜单：显示主窗口 / 全部模块 / 退出程序）"
            );
            Some(handle)
        }
        Err(err) => {
            tracing::warn!(target: "main", "系统托盘不可用，降级为无托盘运行: {err}");
            None
        }
    };

    // 关闭按钮按配置“隐藏到托盘”而非退出进程（窗口对象保留，可再次显示）。
    if app_config.minimize_to_tray {
        ui.window().on_close_requested(|| {
            tracing::debug!(target: "main", "窗口关闭请求 → 隐藏到系统托盘（进程常驻）");
            CloseRequestResponse::HideWindow
        });
    }

    // 生命周期控制器：独立订阅总线，消费托盘指令并回写托盘菜单文案。
    let lc_rx = event_bus.subscribe();
    let lc_ui = ui.as_weak();
    let lc_mgr = Arc::clone(&shared_mgr);
    let lc_tray = tray_handle.as_ref().map(|handle| handle.control());
    tokio::spawn(async move {
        lifecycle_controller(lc_rx, lc_mgr, lc_ui, lc_tray).await;
    });
    tracing::info!(target: "main", "生命周期控制器已启动");

    // ---- 10. 启动 UI 事件循环（显式区分“显示主窗口”与“后台静默”两条路径）。 ----
    //
    // 显示决策：静默自启（--silent，注册表 Run 键拉起）**且**托盘已就绪时，
    // 不调用 `MainWindow::show()`——主窗口保持隐藏、进程静默常驻，唤醒入口
    // 由托盘图标的「显示主窗口」菜单 / 双击承载；其余情形（普通启动，或静默
    // 启动但托盘初始化失败）显式 `show()` 拉起界面，避免生成不可达的“隐形进程”。
    let keep_window_hidden = silent_launch && tray_handle.is_some();
    if keep_window_hidden {
        tracing::info!(target: "main", "静默启动：主窗口保持隐藏，由系统托盘接管常驻");
    } else {
        ui.show().map_err(|err| -> AppError {
            format!("Slint 主窗口显示失败: {err}").into()
        })?;
        tracing::info!(target: "main", "主窗口已显示，TLToolBox 进入前台运行");
    }

    tracing::info!(target: "main", "TLToolBox 启动完毕，进入事件循环");
    slint::run_event_loop().map_err(|err| -> AppError {
        format!("Slint 事件循环运行失败: {err}").into()
    })?;
    tracing::info!(target: "main", "UI 事件循环已退出，开始平滑收尾");

    // ---- 11. 收尾：先逆序停止全部仍在运行的模块（平滑卸载 Win32 钩子等
    //           资源），再关闭托盘线程释放图标。 ----
    for meta in shared_mgr.get_metadata_list() {
        if meta.running {
            match shared_mgr.toggle(meta.id, false).await {
                Ok(_) => tracing::info!(target: "main", "收尾：模块 {0} 已停止", meta.id),
                Err(err) => tracing::error!(target: "main", "收尾：停止模块 {0} 失败: {err}", meta.id),
            }
        }
    }

    if let Some(handle) = tray_handle {
        tracing::info!(target: "main", "正在关闭系统托盘（释放图标资源）");
        handle.shutdown();
    }

    tracing::info!("TLToolBox 已退出，全部常驻模块已平滑停机");
    Ok(())
}
