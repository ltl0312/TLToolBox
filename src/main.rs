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
//! 一键全开全关 + 「关于」「设置」入口）+ ScrollView 流式模块卡片列表（名称 /
//! 描述 / 运行状态徽标 / 物理开关）；带设置项的模块（弹窗拦截、终端交互日志）
//! 卡片上另有齿轮按钮：
//! 弹窗拦截点击弹出**「黑名单规则管理 + 拦截留痕」弹窗**（根层级 Overlay：查看 /
//! 新增 / 删除拦截关键词，经 `update_rules` 热更新并持久化到配置，全程无需手写
//! TOML；v0.3.2 起附带**拦截截图留痕区**——每次命中黑名单关闭弹窗前截图 PNG
//! 留存，弹窗内可浏览最近拦截记录与图片预览）；
//! 终端交互日志点击弹出**「终端日志记录 - 存储管理」弹窗**（根层级 Overlay 之二：
//! 展示当前生效的日志存储目录绝对路径，可一键在文件资源管理器中打开或**更改 /
//! 恢复默认**——目录不存在时自动创建，齿轮分派见 8.4.1、弹窗回调接线见 8.4.5）；
//! **「全局设置」弹窗**（v0.3.2，主界面「设置」按钮）：三类目录（应用日志 /
//! 终端交互日志 / 弹窗截图）的当前生效绝对路径展示 + 原生文件夹选择器切换 /
//! 资源管理器打开 / 恢复默认；**「关于」弹窗**（v0.3.2，主界面「关于」按钮）：
//! 图标 / 版本号 / 开源地址（点击经 `ShellExecuteW` 打开浏览器）/ 检查更新
//! （WinHttp 原生请求 GitHub Releases 接口，比对 `tag_name` 与当前版本，零网络
//! 库依赖；发现新版本时展示琥珀色下载入口）。
//! 双栏时代的内嵌运行日志控制台已退役，日志改由 `tracing` 承载（见
//! [`tltoolbox::logging`]）：debug 构建双写 控制台 + 按天滚动文件；release
//! 构建（无控制台黑框）仅写文件。**v0.3.2 起日志目录可经配置自定义**
//! （`app_log_dir`，缺省回退 exe 同级 `logs/`），因此装配顺序调整为
//! 「单实例 → 配置加载 → 日志装配」；同目录另落盘**用户操作审计日志**
//! `logs/app_audit.log`（[`AuditSink`](tltoolbox::logging::AuditSink)，高精度
//! 时间戳 + 模块开关 / 黑名单 / 自启 / 提权 / 检查更新 / 路径更改的操作留痕）。
//! 守卫句柄（`WorkerGuard`）由本函数持有至退出，保证任何退出路径都先完整刷盘
//! 再结束进程。
//! 装配顺序与职责：
//!
//! 0. **启动前置 · 单实例守护**（在配置 / 模块 / UI 装配之前执行，见函数体 0.2）：
//!    经 Windows 会话级具名互斥（[`single_instance`](tltoolbox::single_instance)，
//!    `CreateMutexW` + `GetLastError == ERROR_ALREADY_EXISTS` 探测）拦截二次启动——
//!    检测到既有实例时向 `HWND_BROADCAST` 广播唤醒消息并**立即退出本进程**；主
//!    实例的托盘消息泵监听该消息，收到后发布 [`AppEvent::TrayAction`] 的
//!    `ShowWindow`，把静默常驻的主窗口还原前置。互斥句柄由守卫持有至进程收尾；
//! 1. **静默启动识别**：解析命令行 `--silent`（系统经注册表 Run 键拉起本程序时
//!    附加，见 [`autostart::SILENT_ARG`](tltoolbox::autostart::SILENT_ARG)）；
//!    常驻层据此抑制前台打扰，托盘最小化行为由常驻层消费；
//! 2. **配置加载**：`ConfigManager::default().load()` 负责首次运行自动落盘默认
//!    配置——默认路径已锚定到**可执行文件同级目录**下的
//!    `config/tltoolbox.toml`（见
//!    [`config::resolve_app_path`](tltoolbox::config::resolve_app_path)，规避
//!    Run 键自启时 CWD = `System32` 导致的找不到 / 无权限问题），随后异步加载
//!    （自启标志 / 托盘行为 / 自动启动模块列表 / 三类日志与截图目录）。加载失败
//!    时以默认日志目录兜底装配后上报错误（保证失败原因可落盘）；成功则按
//!    `effective_app_log_dir` 装配 tracing 与审计日志；
//! 3. **日志与审计装配**（v0.3.2）：`logging::init_in_dir(&effective_app_log_dir)`
//!    决定按天滚动文件落盘目录；`AuditSink` 在同一目录写用户操作审计
//!    `app_audit.log`（`audit` 句柄被后续全部 UI 回调与生命周期控制器持有）；
//! 4. **注册表自启同步**：配置为准——`auto_start_windows` 与实际注册表
//!    `HKCU\...\CurrentVersion\Run` 状态不一致时立即收敛（配置开而缺失/路径漂移
//!    → 补写「当前 exe --silent」；配置关而残留 → 删除）。同步失败仅告警降级，
//!    不阻断启动（优雅同步）；
//! 5. **总线装配**：`EventBus`（`tokio::sync::broadcast`，容量 256）注入模块管理器
//!    作为广播出口；
//! 6. **模块注册**：注册内置常驻守护模块（[`PopupBlockerModule`] 弹窗拦截——含
//!    截图留痕目录注入、[`KeepAwakeModule`] 系统防休眠、[`ClipboardPurifierModule`]
//!    剪贴板纯文本净化、[`TerminalLoggerModule`] 终端交互日志）；
//! 7. **自动启动模块**：按配置对 `auto_start_modules` 执行 `toggle(id, true)`——
//!    **先于 UI 装配**，启动期广播事件因尚无订阅者而被总线按设计丢弃，随后以调度
//!    层**真实状态快照**填充 UI 初始模型，保证“界面即事实”；
//! 8. **UI 实例化与数据注入**：`MainWindow::new()` 后一次性写入模块列表
//!    `VecModel`，并注入开机自启生效状态（注册表为准）、应用版本号
//!    （`env!("CARGO_PKG_VERSION")` 动态注入 `app_version`，UI 侧零硬编码）与
//!    三类目录展示文本 / 初始留痕列表（v0.3.2）；`VecModel`
//!    与全部 Slint 模型一样**非 `Send`**，自创建后终生驻留 UI 主线程；
//! 9. **总线 → UI 桥**：常驻 Tokio 任务订阅 `EventBus`，把模块状态落定事件经
//!    [`slint::invoke_from_event_loop`] **重定向到 UI 主线程的消息循环**再刷新
//!    模型。`VecModel`/`ModelRc` 从不跨线程移动：跨线程载荷只有
//!    `Weak<MainWindow>`（Slint 官方保证 `Send`）与调度器 `Arc` 等 `Send` 数据，
//!    模型改写一律发生在事件循环闭包（UI 线程）内部；
//! 10. **回调绑定**：模块开关 `toggle_module` 派发异步启停；全局「开机自启」
//!     `toggle_autostart` 写注册表并持久化配置后回读收敛；「全部启用 / 全部停用」
//!     `toggle_all_modules` 复用托盘的全量切换路径——三者落定后的真实状态均回流
//!     UI（失败场景自然回滚开关）；四类动作在**成功落定 / 生效**后另经统一 `show_toast`
//!     弹出底部 Toast 轻量反馈气泡（2.5s 自动淡出 / 点击即关，见函数体 8.0）；弹窗拦截额外注册**规则管理 + 留痕闭环**：齿轮点击
//!     拉取 `current_rules` 灌入弹窗；新增 / 删除同步热更新模块内 RuleStore、
//!     经单写者通道异步落盘 `popup_blacklist`、再回读归一化结果刷新弹窗列表——
//!     规则事实源始终是模块内存态，配置与 UI 均自其收敛（详见函数体 8.4）；
//!     拦截留痕列表经版本号 [`watch`](tokio::sync::watch) 订阅实时刷新，选中行
//!     经 `Image::load_from_path` 加载 PNG 预览（8.7）；「关于」/「全局设置」弹窗
//!     回调见 8.5 / 8.6（检查更新、目录浏览选择）。全部关键用户操作统一写审计日志；
//! 11. **托盘装配与生命周期控制**（桌面常驻核心）：`tray::spawn` 派生**独立托盘
//!     线程**（Win32 消息泵），把菜单 / 双击指令发布为 [`AppEvent::TrayAction`]；
//!     `minimize_to_tray && 托盘就绪` 时构成**常驻闭环**：关闭按钮回调返回
//!     `CloseRequestResponse::KeepWindowShown` 拒绝关闭，并经
//!     `invoke_from_event_loop` 延后执行 `hide()` + [`platform::empty_working_set`]
//!     （物理内存工作集压制——常驻期从 ~54MB 骤降至 ~10MB 以内）；事件循环改
//!     用 `slint::run_event_loop_until_quit`——slint 的 keepalive 语义是「最后
//!     一个可见窗口 / 托盘图标消失即退出」，普通 `run_event_loop` 会在窗口隐藏
//!     的瞬间终止进程，`until_quit` 形态让窗口隐藏 / 显示切换都不收尾，只有
//!     托盘「退出程序」的 `quit_event_loop` 才进入平滑收尾（该形态同时使
//!     `--silent` 静默常驻成立）；
//!     `--silent` 静默自启且托盘就绪时主窗口保持隐藏（不调用 `show()`）；独立“生命周期控制器”订阅总线——双击 /
//!     “显示主窗口”经 `invoke_from_event_loop` 在 UI 线程还原窗口，“全部模块：开启/
//!     关闭”逐模块 toggle，“退出程序”调度 `slint::quit_event_loop` 进入收尾；模块
//!     状态事件回写托盘菜单文案（`TrayControl::sync_all_modules`，非阻塞投递）。
//! 12. **权限提权整合**（UIPI 突围）：启动早期 0.3 读取一次 [`platform::is_elevated`]
//!     快照——**未提权**时 UI 顶部渲染盾牌提权按钮、托盘菜单渲染「以管理员身份重启」；
//!     **已提权**时标题旁渲染“管理员 (Admin)”翡翠徽标、两处入口均隐藏。入口经
//!     [`trigger_admin_restart`]（单飞 + `spawn_blocking`）执行
//!     [`platform::restart_as_admin`]：`ShellExecuteW("runas")` 拉起提权副本后，本
//!     进程调度 `quit_event_loop` 平滑收尾退出；新副本凭 `RESTART_MARKER_ARG` 跳过
//!     单实例「第二实例退出」路径（见 0.2 特例）完成接管。

slint::include_modules!();

use slint::{
    CloseRequestResponse, ComponentHandle, Image as SlintImage, Model, ModelRc, SharedString,
    VecModel,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tltoolbox::autostart;
use tltoolbox::bus::{AppEvent, EventBus, TrayAction};
use tltoolbox::config::{AppConfig, ConfigManager, PinnedRule};
use tltoolbox::logging::{self, AuditSink};
use tltoolbox::manager::{ModuleManager, SharedManager};
use tltoolbox::modules::clipboard_purifier::ClipboardPurifierModule;
use tltoolbox::modules::icon_locker::explorer;
use tltoolbox::modules::icon_locker::IconLockerModule;
use tltoolbox::modules::keep_awake::KeepAwakeModule;
use tltoolbox::modules::popup_blocker::{CaptureRecord, PopupBlockerModule};
use tltoolbox::modules::port_hunter::{PortEntry, PortHunterModule};
use tltoolbox::modules::terminal_logger::TerminalLoggerModule;
use tltoolbox::modules::topmost_manager::TopmostManagerModule;
use tltoolbox::modules::ToolModule;
use tltoolbox::platform;
use tltoolbox::single_instance;
use tltoolbox::tray::{self, TrayControl};
use tltoolbox::update;
use tokio::sync::broadcast;
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// UI 模型辅助（仅允许在 UI 主线程执行）
// ---------------------------------------------------------------------------

/// 模块是否在卡片上提供「设置齿轮」（点击弹出模块级配置面板入口）。
///
/// 仅当模块装配层为其实现了设置面板入口回调时才返回 `true`。当前：
/// - `popup_blocker`（弹窗拦截）：齿轮点击打开「黑名单规则管理」弹窗；
/// - `terminal_logger`（终端交互日志）：齿轮点击打开「终端日志记录 - 存储管理」
///   弹窗（展示当前生效的日志存储目录，可一键在文件资源管理器中打开，见函数体
///   8.4.1 / 8.4.5 的按模块 ID 分派与回调接线）；
/// - `topmost_manager`（全局窗口置顶，v0.4.0）：齿轮点击打开「管理窗口」弹窗
///   （候选窗口列表 + 优先级步进器 + 置顶开关，见函数体 8.8 的分派与回调接线）；
/// - `port_hunter`（端口占用管理，v0.5.0）：齿轮点击打开「端口占用管理」弹窗
///   （监听端口列表 + 搜索过滤 + 一键释放 + 两个选项开关，见函数体 8.9）。
///   其余模块（如 keep_awake）不渲染齿轮。
///   未来新增带设置面板的模块时在此扩展。
fn module_has_settings(id: &str) -> bool {
    matches!(
        id,
        "popup_blocker" | "terminal_logger" | "topmost_manager" | "port_hunter" | "icon_locker"
    )
}

/// 模块卡片是否提供「常驻后台开关」（v0.5.0）。
///
/// 端口占用管理为**即开即用工具**（无常驻后台语义）：卡片不渲染物理开关、
/// 状态列显示「即开即用」，全部功能经齿轮弹窗进入；「全部启用 / 全部停用」与
/// 卡片拨动对其一律跳过（`start` / `stop` 本为空操作，双保险）。
fn module_is_toggleable(id: &str) -> bool {
    id != "port_hunter"
}

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
            has_settings: module_has_settings(meta.id),
            toggleable: module_is_toggleable(meta.id),
        })
        .collect()
}

/// 【UI 线程内】以调度层真实状态整体重建模块列表模型。
///
/// 采用 `set_vec`（模型 reset 语义）而非逐行更新：reset 会让 `for` 中继重建行元素、
/// 使 Switch 的 `checked: item.enabled` 绑定重新求值，从而无论用户点击是否已改写
/// Switch 内部状态，视觉开关最终都会收敛到底层模块的真实运行态（含启动失败回滚）。
/// 模块数量极少且 toggle 为低频事件，重建成本可忽略。
///
/// v0.4.1 附加职责：把 `topmost_manager` 的真实运行态同步到
/// `topmost_module_enabled`（窗口置顶弹窗的警示条 / 控件禁用联锁，见 app.slint）——
/// 模块状态事件驱动本函数，运行态落定即联锁收敛，无需单独事件通道。
fn refresh_modules_model(ui: &MainWindow, manager: &ModuleManager) {
    let items = module_items_from_manager(manager);
    let model: ModelRc<ModuleItem> = ui.get_modules();
    if let Some(vec_model) = model.as_any().downcast_ref::<VecModel<ModuleItem>>() {
        vec_model.set_vec(items);
    }
    ui.set_topmost_module_enabled(
        manager
            .get_module("topmost_manager")
            .map(|module| module.is_running())
            .unwrap_or(false),
    );
}

/// 【UI 线程内】整体替换弹窗规则列表模型（`popup_rules` in-property）。
///
/// 采用 `set_popup_rules`（模型 reset 语义）而非逐行更新：规则增删后行号必须与
/// 弹窗展示顺序严格一致，reset 会令 `for` 中继重建行元素并把（删除按钮）回调里的
/// `index` 重新绑定到新行号上。规则数量级为个位数到数十条，重建成本可忽略。
fn set_popup_rules_model(ui: &MainWindow, rules: Vec<String>) {
    let model = VecModel::from(
        rules
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<SharedString>>(),
    );
    ui.set_popup_rules(ModelRc::new(model));
}

/// 【任意线程可调用】把最新的规则列表异步刷入 UI 的 `popup_rules` 模型。
///
/// 经 `slint::invoke_from_event_loop` 排队到 UI 线程执行。为何不直接在回调里同步
/// 改写：删除回调由「被删除行自身的按钮」触发，同步重建模型会销毁正在派发事件的
/// 行元素；排队到下一轮事件循环即可让当前事件干净收尾后再重建列表。
fn deliver_popup_rules_refresh(ui_weak: &slint::Weak<MainWindow>, rules: Vec<String>) {
    let weak = ui_weak.clone();
    let queued = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            set_popup_rules_model(&ui, rules);
        }
    });
    if queued.is_err() {
        tracing::warn!(target: "main", "无法投递规则列表刷新：UI 事件循环已不可用");
    }
}

// ---------------------------------------------------------------------------
// 全局设置 · 日志 / 截图目录自定义（v0.3.2：展示 + 浏览切换 + 恢复默认）
// ---------------------------------------------------------------------------

/// 【UI 线程内】把三类目录的**当前生效绝对路径**刷入主设置弹窗的展示属性。
///
/// 经各 `effective_*` 访问器统一解析（缺省回退 exe 同级默认布局），保证展示的
/// 是消费方实际使用的目录。
fn refresh_settings_displays(ui: &MainWindow, cfg: &AppConfig) {
    ui.set_app_log_dir_display(SharedString::from(
        cfg.effective_app_log_dir().to_string_lossy().into_owned(),
    ));
    ui.set_terminal_log_dir_display(SharedString::from(
        cfg.effective_terminal_log_dir()
            .to_string_lossy()
            .into_owned(),
    ));
    ui.set_screenshot_dir_display(SharedString::from(
        cfg.effective_popup_screenshot_dir()
            .to_string_lossy()
            .into_owned(),
    ));
}

// ---------------------------------------------------------------------------
// 弹窗拦截留痕（v0.3.2：截图列表模型 + 预览图加载）
// ---------------------------------------------------------------------------

/// 【UI 线程内】把留痕记录快照重建为 `captures` 列表模型。
///
/// 采用 `set_captures`（reset 语义）：新截图落盘后整体重建，行号与
/// `capture_selected` 的 index 严格对应（与规则列表同一约定）。
fn set_captures_model(ui: &MainWindow, records: Vec<CaptureRecord>) {
    let items = records
        .into_iter()
        .map(|record| CaptureItem {
            file_name: SharedString::from(record.file_name),
            title: SharedString::from(record.title),
            captured_at: SharedString::from(record.captured_at),
            path: SharedString::from(record.path.to_string_lossy().into_owned()),
        })
        .collect::<Vec<CaptureItem>>();
    ui.set_captures(ModelRc::new(VecModel::from(items)));
}

/// 【任意线程可调用】把最新留痕记录异步刷入 UI（经事件循环排队到 UI 线程）。
fn deliver_captures_refresh(ui_weak: &slint::Weak<MainWindow>, records: Vec<CaptureRecord>) {
    let weak = ui_weak.clone();
    let queued = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            set_captures_model(&ui, records);
        }
    });
    if queued.is_err() {
        tracing::warn!(target: "main", "无法投递留痕列表刷新：UI 事件循环已不可用");
    }
}

/// 【UI 线程内】按列表行号加载留痕预览图（`Image::load_from_path` 原生解码 PNG）。
fn load_capture_preview(ui: &MainWindow, index: usize) {
    let Some(item) = ui.get_captures().row_data(index) else {
        return;
    };
    let path = item.path.to_string();
    match SlintImage::load_from_path(std::path::Path::new(&path)) {
        Ok(image) => ui.set_capture_preview(image),
        Err(err) => {
            // 文件可能被外部删除：清空预览并告警（不 panic）。
            ui.set_capture_preview(SlintImage::default());
            tracing::warn!(target: "main", "加载留痕预览失败（'{}'）: {err}", path);
        }
    }
    ui.set_capture_preview_caption(SharedString::from(format!(
        "{} · {}",
        item.file_name, item.title
    )));
}

// ---------------------------------------------------------------------------
// 全局窗口置顶 · 管理弹窗（v0.4.0：候选窗口枚举 + 置顶状态合并 + 操作回调）
// ---------------------------------------------------------------------------

/// 最近一次枚举的**全量**候选窗口行（供搜索过滤复用；UI 模型只展示过滤子集）。
///
/// Slint 的 `TopmostWindowItem` 各字段（`SharedString` / `bool` / `int`）均
/// `Send + Sync`，可安全驻留本静态量；跨线程载荷遵守既有铁律——模型写入只发生在
/// UI 线程闭包内，本静态仅作为“全量快照缓存”被读取。
static TOPMOST_FULL_ROWS: OnceLock<std::sync::Mutex<Vec<TopmostWindowItem>>> = OnceLock::new();

/// 全量快照缓存访问器。
fn topmost_full_rows() -> &'static std::sync::Mutex<Vec<TopmostWindowItem>> {
    TOPMOST_FULL_ROWS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// 由枚举窗口 + 模块受管条目 + 优先级记忆合并出弹窗行。
///
/// `pinned` 为模块当前受管条目；命中受管的窗口行带优先级 / 置顶态，未受管的
/// 行按 v0.4.1 的**优先级记忆**回填（该进程在 `pinned_rules` 中留有历史设定 →
/// 显示记忆值；无记忆 → 默认优先级 [`DEFAULT_PRIORITY`](tltoolbox::modules::topmost_manager::DEFAULT_PRIORITY)
/// 并保持 `pinned = false`（系统 `WS_EX_TOPMOST` 仅是展示指示，不与受管挂钩）。
/// `hwnd_value` 为操作回路的稳定键（HWND 裸值截断到 32 位，落在 USER 句柄表内）。
fn topmost_row_from(
    window: &tltoolbox::modules::topmost_manager::enum_windows::WindowInfo,
    pinned: &[tltoolbox::modules::topmost_manager::ActivePinnedWindow],
    module: &TopmostManagerModule,
) -> TopmostWindowItem {
    let entry = pinned.iter().find(|p| p.hwnd == window.hwnd);
    // 优先级事实源：受管条目 > 进程级记忆（v0.4.1）> 默认值。
    let priority = entry
        .map(|e| e.priority as i32)
        .or_else(|| {
            module
                .remembered_priority(&window.process_name)
                .map(|p| p as i32)
        })
        .unwrap_or(tltoolbox::modules::topmost_manager::DEFAULT_PRIORITY as i32);
    TopmostWindowItem {
        hwnd: SharedString::from(format!("0x{:X}", window.hwnd)),
        hwnd_value: window.hwnd as i32,
        process_name: SharedString::from(window.process_name.clone()),
        title: SharedString::from(window.title.clone()),
        topmost: window.topmost,
        priority,
        pinned: entry.is_some(),
    }
}

/// 按搜索词过滤全量候选行（进程名 / 标题 / HWND 文本三路子串，忽略大小写）。
fn filter_topmost_rows(rows: &[TopmostWindowItem], search: &str) -> Vec<TopmostWindowItem> {
    let needle = search.trim().to_lowercase();
    if needle.is_empty() {
        return rows.to_vec();
    }
    rows.iter()
        .filter(|row| {
            row.process_name.to_lowercase().contains(&needle)
                || row.title.to_lowercase().contains(&needle)
                || row.hwnd.to_lowercase().contains(&needle)
        })
        .cloned()
        .collect()
}

/// 【任意线程可调用】重建全量候选行快照并刷新弹窗模型（枚举在阻塞线程执行）。
fn refresh_topmost_rows(ui_weak: &slint::Weak<MainWindow>, module: Arc<TopmostManagerModule>) {
    let weak = ui_weak.clone();
    tokio::spawn(async move {
        // 枚举（EnumWindows + 逐窗口元数据查询）为同步 Win32 调用，移出运行时。
        let windows = tokio::task::spawn_blocking(TopmostManagerModule::enumerate_candidates)
            .await
            .unwrap_or_default();
        let pinned = module.pinned();
        let rows: Vec<TopmostWindowItem> = windows
            .iter()
            .map(|window| topmost_row_from(window, &pinned, &module))
            .collect();
        *topmost_full_rows()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = rows.clone();

        // 在 UI 线程读取当前搜索词并交付过滤结果（跨线程载荷仅 Weak + Vec）。
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                let search = ui.get_topmost_search().to_string();
                ui.set_topmost_windows(ModelRc::new(VecModel::from(filter_topmost_rows(
                    &rows, &search,
                ))));
            }
        });
    });
}

/// 【UI 线程内】按当前搜索词从全量快照重刷弹窗模型（搜索框输入即时过滤）。
fn apply_topmost_search(ui: &MainWindow) {
    let search = ui.get_topmost_search().to_string();
    let rows = topmost_full_rows()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    ui.set_topmost_windows(ModelRc::new(VecModel::from(filter_topmost_rows(
        &rows, &search,
    ))));
}

/// 置顶 / 解除置顶 / 改级操作的回调载体：执行模块操作 → 审计 → Toast → 刷新列表。
///
/// v0.4.1 起 `hwnd` 由 Slint 行控件的 `hwnd_value`（HWND 裸值截断）**直接**传入，
/// 不再经十六进制文本解析——搜索过滤 / 列表重排后仍是 HWND 绝对匹配的真实窗口。
fn handle_topmost_pin(
    ui_weak: &slint::Weak<MainWindow>,
    module: Arc<TopmostManagerModule>,
    audit: AuditSink,
    hwnd: isize,
    priority: i32,
    pinned: bool,
) {
    let result = if pinned {
        module.apply_pin(hwnd, priority as u8)
    } else {
        module.apply_unpin(hwnd)
    };
    match result {
        Ok(()) => {
            // 审计细节（进程名 / 生效优先级）从模块受管条目回读，保证与事实一致。
            let process = module
                .pinned()
                .iter()
                .find(|p| p.hwnd == hwnd)
                .map(|p| p.process_name.clone())
                .unwrap_or_else(|| "<unknown.exe>".to_string());
            if pinned {
                let effective = module
                    .pinned()
                    .iter()
                    .find(|p| p.hwnd == hwnd)
                    .map(|p| p.priority)
                    .unwrap_or(priority as u8);
                audit.record(
                    "TOPMOST",
                    format!("开启窗口置顶 HWND: 0x{hwnd:X} 进程: {process} 优先级: {effective}"),
                    "成功",
                );
                show_toast(ui_weak, &format!("已置顶：{process}（优先级 {effective}）"));
            } else {
                audit.record(
                    "TOPMOST",
                    format!("解除窗口置顶 HWND: 0x{hwnd:X} 进程: {process}"),
                    "成功",
                );
                show_toast(ui_weak, &format!("已解除置顶：{process}"));
            }
        }
        Err(engine_err) => {
            let (audit_result, toast_msg) = match &engine_err {
                tltoolbox::modules::topmost_manager::engine::Win32Error::AccessDenied => (
                    "失败: UIPI 拦截（目标窗口特权更高）".to_string(),
                    "目标窗口具备高特权，请提权运行 TLToolBox".to_string(),
                ),
                tltoolbox::modules::topmost_manager::engine::Win32Error::InvalidWindow => (
                    "失败: 窗口已关闭".to_string(),
                    "窗口已关闭或句柄失效".to_string(),
                ),
                other => (format!("失败: {other}"), format!("置顶操作失败：{other}")),
            };
            audit.record(
                "TOPMOST",
                format!(
                    "{} HWND: 0x{hwnd:X}",
                    if pinned {
                        "开启窗口置顶"
                    } else {
                        "解除窗口置顶"
                    }
                ),
                audit_result,
            );
            show_toast(ui_weak, &toast_msg);
        }
    }
    // 操作已落定：重拉全量快照收敛弹窗（开关 / 优先级与底层事实对齐）。
    refresh_topmost_rows(ui_weak, module);
}

/// 优先级步进器回调：仅对受管（已置顶）窗口生效。
fn handle_topmost_priority(
    ui_weak: &slint::Weak<MainWindow>,
    module: Arc<TopmostManagerModule>,
    audit: AuditSink,
    hwnd: isize,
    priority: i32,
) {
    match module.set_priority(hwnd, priority as u8) {
        Ok(()) => {
            let process = module
                .pinned()
                .iter()
                .find(|p| p.hwnd == hwnd)
                .map(|p| p.process_name.clone())
                .unwrap_or_else(|| "<unknown.exe>".to_string());
            audit.record(
                "TOPMOST",
                format!("调整窗口置顶优先级 HWND: 0x{hwnd:X} 进程: {process} 优先级: {priority}"),
                "成功",
            );
            show_toast(
                ui_weak,
                &format!("已调整 {process} 置顶优先级为 {priority}"),
            );
        }
        Err(engine_err) => {
            let (audit_result, toast_msg) = match &engine_err {
                tltoolbox::modules::topmost_manager::engine::Win32Error::AccessDenied => (
                    "失败: UIPI 拦截（目标窗口特权更高）".to_string(),
                    "目标窗口具备高特权，请提权运行 TLToolBox".to_string(),
                ),
                tltoolbox::modules::topmost_manager::engine::Win32Error::InvalidWindow => (
                    "失败: 窗口已关闭".to_string(),
                    "窗口已关闭或句柄失效".to_string(),
                ),
                other => (format!("失败: {other}"), format!("调整优先级失败：{other}")),
            };
            audit.record(
                "TOPMOST",
                format!("调整窗口置顶优先级 HWND: 0x{hwnd:X} -> {priority}"),
                audit_result,
            );
            show_toast(ui_weak, &toast_msg);
        }
    }
    refresh_topmost_rows(ui_weak, module);
}

// ---------------------------------------------------------------------------
// 端口占用管理 · UI 桥接（v0.5.0：扫描 / 过滤 / 释放 / 选项持久化）
// ---------------------------------------------------------------------------

/// 把模块侧 [`PortEntry`] 转换为 UI 列表行（`PortEntryItem`）。
fn port_entry_to_item(entry: &PortEntry) -> PortEntryItem {
    PortEntryItem {
        protocol: SharedString::from(entry.protocol.clone()),
        local_port: entry.local_port as i32,
        local_addr: SharedString::from(entry.local_addr.clone()),
        pid: entry.pid as i32,
        process_name: SharedString::from(entry.process_name.clone()),
        process_path: SharedString::from(entry.process_path.clone()),
    }
}

/// 【UI 线程内】按当前搜索词 / 显示选项重刷弹窗列表与状态条。
///
/// v0.5.1 语义：`show_system_ports` 在扫描期已物理阻断系统端口 / PID ≤ 4 /
/// 多 IP 重复行（`scanner` 原生表项遍历循环内完成，缓存是干净形态）；此处
/// 只做**搜索词即时收敛**（纯客户端过滤，零重扫）。
fn refresh_port_hunter_display(ui: &MainWindow, module: &PortHunterModule) {
    let search = ui.get_port_hunter_search().to_string();
    let show_system = ui.get_port_show_system_ports();
    let rows = module.cached_rows();
    // 扫描缓存已是物理阻断后的干净形态，搜索过滤直接套用（无需系统端口 /
    // 内核态二次剔除——它们从未进入缓存）。
    let filtered =
        tltoolbox::modules::port_hunter::scanner::filter_port_rows(&rows, &search, show_system);
    let items: Vec<PortEntryItem> = filtered.iter().map(port_entry_to_item).collect();
    ui.set_port_hunter_entries(ModelRc::new(VecModel::from(items)));
    // v0.5.1 副标题：仅在复选框**真实勾选**（bool == true）时追加「（显示系统
    // 服务与高位端口）」；未勾选时展示有效过滤后条数（如「共 12 个监听端口 ·
    // 当前展示 12 个」）。复选框驱动 scan 重扫：勾选前缓存不含系统端口，勾选后
    // 扫描把系统端口纳入——展示数量随 bool 真实联动，绝不出现「未勾选仍 106」。
    let suffix = if show_system {
        "（显示系统服务与高位端口）"
    } else {
        ""
    };
    let status = format!(
        "共 {} 个监听端口 · 当前展示 {} 个{}",
        rows.len(),
        filtered.len(),
        suffix
    );
    ui.set_port_hunter_status(SharedString::from(status));
}

/// 【任意线程可调用】执行一次端口猎手扫描并交付 UI（阻塞枚举移出运行时）。
///
/// 流程：读取运行期配置的显示选项 → `spawn_blocking` 执行同步 Win32 枚举
/// （[`PortHunterModule::scan`]，结果写入模块缓存）→ UI 线程按缓存重刷列表与
/// 状态条（搜索词即时套用）。失败路径写审计 + Toast，列表保持上次缓存不变。
fn scan_port_hunter(
    ui_weak: &slint::Weak<MainWindow>,
    module: Arc<PortHunterModule>,
    runtime_config: Arc<Mutex<AppConfig>>,
    audit: AuditSink,
) {
    let weak = ui_weak.clone();
    let module_scan = Arc::clone(&module);
    let module_ui = Arc::clone(&module);
    tokio::spawn(async move {
        let show_system = {
            let cfg = runtime_config.lock().await;
            cfg.port_hunter.show_system_ports
        };
        // 同步 Win32 调用整体移出 Tokio 工作线程（毫秒级，但保持纪律）。
        let result = tokio::task::spawn_blocking(move || module_scan.scan(show_system)).await;

        match result {
            Ok(Ok(_outcome)) => {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        // 扫描成功：缓存已在阻塞线程更新，按缓存重刷展示。
                        refresh_port_hunter_display(&ui, &module_ui);
                    }
                });
            }
            Ok(Err(err)) => {
                audit.record("PORT_HUNTER", "扫描监听端口", format!("失败: {err}"));
                show_toast(&weak, &format!("端口扫描失败：{err}"));
            }
            Err(err) => {
                audit.record("PORT_HUNTER", "扫描监听端口", format!("任务异常: {err}"));
                show_toast(&weak, "端口扫描任务异常，请重试");
            }
        }
    });
}

/// 打开「端口占用管理」弹窗：读取运行期配置的两个选项开关 → 注入 UI →
/// 展示弹窗 → 立即扫描一次（打开即是最新数据）。
fn open_port_hunter_modal(
    ui_weak: &slint::Weak<MainWindow>,
    module: Arc<PortHunterModule>,
    runtime_config: Arc<Mutex<AppConfig>>,
    audit: AuditSink,
) {
    let weak = ui_weak.clone();
    tokio::spawn(async move {
        let (confirm, show_system) = {
            let cfg = runtime_config.lock().await;
            (
                cfg.port_hunter.confirm_before_kill,
                cfg.port_hunter.show_system_ports,
            )
        };
        let weak_ui = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak_ui.upgrade() {
                ui.set_port_confirm_before_kill(confirm);
                ui.set_port_show_system_ports(show_system);
                ui.set_port_hunter_search(SharedString::from(""));
                ui.set_show_port_hunter_modal(true);
            }
        });
        scan_port_hunter(&weak, module, runtime_config, audit);
    });
}

/// 端口猎手弹窗回调的共享上下文（汇聚配置句柄 / 模块 / 审计 / UI 弱引用，
/// 避免回调与持久化函数出现超长参数列表）。
struct PortHunterCtx {
    /// UI 弱引用（事件循环投递入口）。
    ui: slint::Weak<MainWindow>,
    /// 端口猎手模块句柄（扫描 / 终止 / 缓存）。
    module: Arc<PortHunterModule>,
    /// 配置管理器（原子落盘）。
    config_mgr: Arc<ConfigManager>,
    /// 运行期配置锁（选项读写）。
    runtime_config: Arc<Mutex<AppConfig>>,
    /// 全局审计日志。
    audit: AuditSink,
}

impl PortHunterCtx {
    /// 触发一次扫描并刷新弹窗列表（打开 / 刷新 / 选项重过滤共用）。
    fn scan(&self) {
        scan_port_hunter(
            &self.ui,
            Arc::clone(&self.module),
            Arc::clone(&self.runtime_config),
            self.audit.clone(),
        );
    }

    /// 持久化端口猎手的一个布尔选项（`confirm` / `system`）到配置节并落盘。
    ///
    /// `rescan` 为 `true` 时落盘成功后立即重新扫描刷新列表（「显示系统服务与
    /// 高位端口」切换的即时重过滤语义）；失败仅告警——内存态已改，下次启动按
    /// 配置收敛。
    fn persist_option(&self, field: &'static str, value: bool, rescan: bool) {
        let weak = self.ui.clone();
        let config_mgr = Arc::clone(&self.config_mgr);
        let runtime_config = Arc::clone(&self.runtime_config);
        let audit = self.audit.clone();
        let module = Arc::clone(&self.module);
        tokio::spawn(async move {
            let snapshot = {
                let mut cfg = runtime_config.lock().await;
                match field {
                    "confirm" => cfg.port_hunter.confirm_before_kill = value,
                    "system" => cfg.port_hunter.show_system_ports = value,
                    other => {
                        tracing::warn!(target: "main", "未知的端口猎手选项字段: {other}");
                        return;
                    }
                }
                cfg.clone()
            };
            // v0.5.1 状态写回 UI：此前只写配置不写回 UI 属性，CheckBox 的
            // `checked: root.show-system-ports` 单向绑定会在弹窗重开时把视觉
            // 拉回旧值——「显示系统服务与高位端口」复选框失效 + 状态脱节。
            // 写回后，复选框视觉 / 配置 / 过滤结果（refresh_port_hunter_display
            // 读取的正是本属性）三方收敛；排队的顺序保证本写回先于下方 rescan
            // 的展示刷新执行。
            let sync_weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = sync_weak.upgrade() {
                    match field {
                        "confirm" => ui.set_port_confirm_before_kill(value),
                        "system" => ui.set_port_show_system_ports(value),
                        _ => {}
                    }
                }
            });
            let label = if field == "confirm" {
                "二次确认"
            } else {
                "显示系统服务与高位端口"
            };
            match config_mgr.save(&snapshot).await {
                Ok(()) => {
                    audit.record(
                        "PORT_HUNTER",
                        format!(
                            "配置选项: {label} -> {}",
                            if value { "开启" } else { "关闭" }
                        ),
                        "成功",
                    );
                    show_toast(
                        &weak,
                        &format!("{}已{}", label, if value { "开启" } else { "关闭" }),
                    );
                }
                Err(err) => {
                    audit.record(
                        "PORT_HUNTER",
                        format!(
                            "配置选项: {label} -> {}",
                            if value { "开启" } else { "关闭" }
                        ),
                        format!("保存失败: {err}"),
                    );
                    tracing::error!(target: "main", "端口猎手选项落盘失败（{field}={value}）: {err}");
                    show_toast(&weak, "配置保存失败，请重试");
                }
            }
            if rescan {
                scan_port_hunter(&weak, module, runtime_config, audit);
            }
        });
    }
}

/// 【任意线程可调用】执行一次「一键释放」（终止占用进程）。
///
/// 线程模型：Win32 终止动作经 `spawn_blocking` 移出运行时（毫秒级）；成败均写
/// 全局审计日志（`[PORT_HUNTER]` 类别，含端口 / 协议 / 进程 / PID / 结果）与
/// 模块明细日志（在 [`PortHunterModule::kill`] 内部）；成功 / UIPI 拦截的 Toast
/// 由 killer 经事件总线发布；其他失败在此补一条通用失败 Toast；随后立即重扫
/// 列表收敛（进程可能已被外部终止）。
fn handle_port_kill(
    ui_weak: &slint::Weak<MainWindow>,
    module: Arc<PortHunterModule>,
    runtime_config: Arc<Mutex<AppConfig>>,
    audit: AuditSink,
    pid: i32,
    port: i32,
    protocol: String,
) {
    let weak = ui_weak.clone();
    let module_kill = Arc::clone(&module);
    let module_after = Arc::clone(&module);
    tokio::spawn(async move {
        let pid_u = pid as u32;
        let port_u = port as u16;
        let protocol_text = protocol.clone();
        let report =
            tokio::task::spawn_blocking(move || module_kill.kill(pid_u, port_u, &protocol_text))
                .await;

        match report {
            Ok(report) => {
                let detail = format!(
                    "释放端口: {port_u} ({}), 终止进程: {} (PID: {pid_u})",
                    protocol, report.process_name
                );
                if report.is_success() {
                    audit.record("PORT_HUNTER", detail, "成功");
                } else {
                    let err = report.outcome.as_ref().expect_err("失败路径必有错误");
                    let result_text = if err.is_access_denied() {
                        "失败: 需要管理员权限（UIPI 拦截）".to_string()
                    } else {
                        format!("失败: {err}")
                    };
                    audit.record("PORT_HUNTER", detail, result_text);
                    // UIPI 拦截的 Toast 已由 killer 经总线发布；此处只补其他失败。
                    if !err.is_access_denied() {
                        show_toast(&weak, &format!("释放端口 {port_u} 失败：{err}"));
                    }
                }
            }
            Err(err) => {
                audit.record(
                    "PORT_HUNTER",
                    format!("释放端口: {port_u} ({protocol})"),
                    format!("任务异常: {err}"),
                );
                show_toast(&weak, "释放端口任务异常，请重试");
            }
        }
        // 操作落定：重扫列表收敛（含进程已被外部终止的残留行清理）。
        scan_port_hunter(&weak, module_after, runtime_config, audit);
    });
}

// ---------------------------------------------------------------------------
// 目录自定义操作（主设置 / 模块设置弹窗共用：更改 / 打开 / 恢复默认）
// ---------------------------------------------------------------------------

/// 目录自定义操作的类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirSettingAction {
    /// 「更改…」：Win32 原生文件夹选择对话框（取消 = 保持原配置不变）。
    Browse,
    /// 「打开」：确保目录存在后在文件资源管理器中打开。
    Open,
    /// 「恢复默认」：配置字段置 `None`（回退 exe 同级默认布局）。
    Reset,
}

/// 目录键 → 配置字段写入（返回是否识别该键）。
///
/// 键约定与 UI 回调一致：`app_log` / `terminal_log` / `popup_screenshot`。
fn set_dir_field(cfg: &mut AppConfig, key: &str, value: Option<PathBuf>) -> bool {
    match key {
        "app_log" => {
            cfg.app_log_dir = value;
            true
        }
        "terminal_log" => {
            cfg.terminal_log_dir = value;
            true
        }
        "popup_screenshot" => {
            cfg.popup_screenshot_dir = value;
            true
        }
        _ => false,
    }
}

/// 目录键 → 文件夹选择对话框标题。
fn dir_setting_title(key: &str) -> String {
    match key {
        "app_log" => "选择应用日志目录（程序运行日志 + 用户操作审计）".to_string(),
        "terminal_log" => "选择终端交互日志目录（PowerShell / bash / cmd 会话记录）".to_string(),
        _ => "选择弹窗截图目录（拦截留痕 PNG）".to_string(),
    }
}

/// 目录键 → 当前生效目录（经 effective_* 解析，绝对路径）。
fn effective_dir_for_key(cfg: &AppConfig, key: &str) -> PathBuf {
    match key {
        "app_log" => cfg.effective_app_log_dir(),
        "terminal_log" => cfg.effective_terminal_log_dir(),
        _ => cfg.effective_popup_screenshot_dir(),
    }
}

/// 【任意线程可调用】执行一次目录自定义操作，落定后回刷 UI 展示并写审计日志。
///
/// 线程模型：浏览选择（`SHBrowseForFolderW` 模态阻塞）与资源管理器打开（进程
/// spawn）均经 `spawn_blocking` 移出 UI 线程与 Tokio 工作线程；配置字段更新 +
/// 原子落盘在 Tokio 任务内串行执行（ConfigManager 自带写锁）。
fn handle_dir_setting(
    ui_weak: &slint::Weak<MainWindow>,
    config_mgr: Arc<ConfigManager>,
    runtime_config: Arc<Mutex<AppConfig>>,
    audit: AuditSink,
    key: String,
    action: DirSettingAction,
) {
    let weak = ui_weak.clone();
    tokio::spawn(async move {
        match action {
            DirSettingAction::Open => {
                // 「打开」：解析当前生效目录 → 资源管理器打开（目录不存在自动创建）。
                let dir = {
                    let cfg = runtime_config.lock().await;
                    effective_dir_for_key(&cfg, &key)
                };
                open_dir_in_explorer(&weak, dir);
            }
            DirSettingAction::Browse => {
                // 「更改…」：原生文件夹选择；取消（None）静默保持原配置。
                let title = dir_setting_title(&key);
                let chosen =
                    match tokio::task::spawn_blocking(move || platform::browse_for_folder(&title))
                        .await
                    {
                        Ok(Ok(Some(dir))) => dir,
                        Ok(Ok(None)) => return, // 用户取消：不产生任何变更
                        Ok(Err(err)) => {
                            audit.record("路径更改", &key, format!("选择目录失败: {err}"));
                            show_toast(&weak, &format!("选择目录失败：{err}"));
                            return;
                        }
                        Err(_err) => {
                            audit.record("路径更改", &key, "选择目录任务异常");
                            show_toast(&weak, "选择目录任务异常，请重试");
                            return;
                        }
                    };
                apply_dir_field_change(
                    weak.clone(),
                    &config_mgr,
                    &runtime_config,
                    &audit,
                    &key,
                    Some(chosen),
                )
                .await;
            }
            DirSettingAction::Reset => {
                // 「恢复默认」：字段置 None → 回退 exe 同级默认布局。
                apply_dir_field_change(
                    weak.clone(),
                    &config_mgr,
                    &runtime_config,
                    &audit,
                    &key,
                    None,
                )
                .await;
            }
        }
    });
}

/// 把目录字段的新值写入运行期配置并原子落盘，随后回刷 UI 展示 + 审计 + Toast。
async fn apply_dir_field_change(
    weak: slint::Weak<MainWindow>,
    config_mgr: &ConfigManager,
    runtime_config: &Mutex<AppConfig>,
    audit: &AuditSink,
    key: &str,
    value: Option<PathBuf>,
) {
    // 1) 更新内存配置并克隆快照（锁内不跨 await）。
    let snapshot = {
        let mut cfg = runtime_config.lock().await;
        if !set_dir_field(&mut cfg, key, value.clone()) {
            tracing::warn!(target: "main", "未知的目录配置键: {key}");
            return;
        }
        cfg.clone()
    };

    // 2) 原子落盘；成功 → 回刷展示 + 审计 + Toast；失败 → 审计失败原因（内存态已改，
    //    下次启动仍按旧配置——由失败提示引导用户重试）。
    match config_mgr.save(&snapshot).await {
        Ok(()) => {
            let action_text = match value.as_deref() {
                Some(dir) => format!("{key} -> {}", dir.display()),
                None => format!("{key} -> 恢复默认（exe 同级默认布局）"),
            };
            audit.record("路径更改", action_text, "成功");
            let message = match value.as_deref() {
                Some(dir) if key == "popup_screenshot" => {
                    format!(
                        "截图目录已更新（弹窗拦截下次启动时生效）：{}",
                        dir.display()
                    )
                }
                Some(dir) => format!("目录已更新：{}", dir.display()),
                None => "已恢复默认目录".to_string(),
            };
            // 三类展示文本一律以**最新快照**的 effective_* 解析（任一目录变更后
            // 其它目录的展示同步收敛，避免展示陈旧路径）。
            let (app_dir, term_dir, shot_dir) = (
                snapshot
                    .effective_app_log_dir()
                    .to_string_lossy()
                    .into_owned(),
                snapshot
                    .effective_terminal_log_dir()
                    .to_string_lossy()
                    .into_owned(),
                snapshot
                    .effective_popup_screenshot_dir()
                    .to_string_lossy()
                    .into_owned(),
            );
            let weak_display = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_display.upgrade() {
                    ui.set_app_log_dir_display(SharedString::from(app_dir));
                    ui.set_terminal_log_dir_display(SharedString::from(term_dir));
                    ui.set_screenshot_dir_display(SharedString::from(shot_dir));
                }
            });
            show_toast(&weak, &message);
        }
        Err(err) => {
            audit.record("路径更改", key, format!("保存失败: {err}"));
            tracing::error!(target: "main", "目录配置落盘失败（{key}）: {err}");
            show_toast(&weak, "目录配置保存失败，请重试");
        }
    }
}

// ---------------------------------------------------------------------------
// 检查更新（v0.3.2：WinHttp 原生请求 GitHub Releases）
// ---------------------------------------------------------------------------

/// 【任意线程可调用】触发一次「检查更新」：WinHttp 请求 GitHub Releases 接口，
/// 与当前版本（`CARGO_PKG_VERSION`）比对后回写 UI 状态文本 / 下载入口并弹 Toast。
///
/// 网络请求为同步 WinHttp 管线，整体经 `spawn_blocking` 移出运行时（配合
/// `WinHttpSetTimeouts` 收紧超时，不会长期挂起）；结果一律回写
/// `update_status` / `update_available` / `latest_version`，并写入审计日志。
fn check_for_updates(ui_weak: &slint::Weak<MainWindow>, audit: &AuditSink) {
    let weak = ui_weak.clone();
    let audit = audit.clone();
    tokio::spawn(async move {
        audit.record("检查更新", update::RELEASES_API_URL, "进行中");
        let outcome = tokio::task::spawn_blocking(update::fetch_latest_tag).await;
        let current = env!("CARGO_PKG_VERSION");

        let (status_text, available, latest) = match outcome {
            Ok(Ok(Some(tag))) => {
                let version = tag.trim_start_matches('v');
                if update::is_newer_version(current, version) {
                    (format!("发现新版本 {tag}"), true, tag.clone())
                } else {
                    (format!("已是最新版本（v{current}）"), false, String::new())
                }
            }
            Ok(Ok(None)) => (
                "无法获取版本信息（可能无网络或接口限流）".to_string(),
                false,
                String::new(),
            ),
            Ok(Err(err)) => (format!("检查更新失败：{err}"), false, String::new()),
            Err(err) => (format!("检查更新任务异常：{err}"), false, String::new()),
        };
        audit.record(
            "检查更新",
            &status_text,
            if available {
                "发现新版本"
            } else {
                "完成"
            },
        );

        let status_for_toast = status_text.clone();
        let weak_ui = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak_ui.upgrade() {
                ui.set_update_status(SharedString::from(status_text));
                ui.set_update_available(available);
                ui.set_latest_version(SharedString::from(latest));
            }
        });
        show_toast(&weak, &status_for_toast);
    });
}

// ---------------------------------------------------------------------------
// 终端交互日志 · 存储管理设置弹窗（UI 回调 → 异步读取配置 → 回 UI 线程展示）
// ---------------------------------------------------------------------------

/// 打开「终端日志记录 - 存储管理」设置弹窗（卡片齿轮 `terminal_logger` 分派与
/// `open_terminal_modal` 预留回调的共用入口）。
///
/// 线程模型：回调运行于 UI 线程，而读取配置需跨 `tokio::sync::Mutex`（异步锁），
/// 同步阻塞加锁会卡住事件循环，故整体经 [`tokio::spawn`] 移出 UI 线程；解析出
/// **当前生效**的日志根目录绝对路径（[`AppConfig::effective_terminal_log_dir`]：
/// 显式目录按绝对 / exe 锚定语义解析，缺省回退 exe 同级 `logs/terminals`）后，
/// 再经 [`slint::invoke_from_event_loop`] 排队回 UI 线程：写入
/// `terminal_log_dir_display` 文本并亮起 `show_terminal_modal`。本函数只读解析
/// 路径，不创建目录——目录的按需创建发生在「资源管理器打开」动作里。
fn open_terminal_settings(
    ui_weak: &slint::Weak<MainWindow>,
    runtime_config: Arc<Mutex<AppConfig>>,
) {
    let weak = ui_weak.clone();
    tokio::spawn(async move {
        // 锁内仅解析路径快照（不跨 await 持锁）；effective_* 恒产出绝对路径。
        let log_dir = {
            let cfg = runtime_config.lock().await;
            cfg.effective_terminal_log_dir()
        };
        let display = log_dir.to_string_lossy().into_owned();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_terminal_log_dir_display(SharedString::from(display));
                ui.set_show_terminal_modal(true);
            }
        });
    });
}

/// 「在文件资源管理器中打开指定目录」请求的单飞守卫。
///
/// explorer 进程 spawn 本身瞬时返回，但连点会在极短窗口内堆叠多个资源管理器
/// 窗口；请求置位后闭锁，直到本次打开落定（成功或失败）再复位。
static EXPLORER_OPEN_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// 【任意线程可调用】在文件资源管理器中打开指定目录（目录不存在自动创建）。
///
/// 实际打开动作委托给 [`platform::open_folder`]（v0.4.0 缺陷修复：`explorer`
/// 收到磁盘上不存在的目录路径会**静默回退打开「文档」文件夹**——由该函数先做
/// 路径绝对化 / 分隔符归一化 / `create_dir_all` 落盘创建后再 spawn）；
/// 成功经 `show_toast` 反馈「已在资源管理器中打开目录」。
///
/// 线程模型（杜绝界面卡死）：目录创建与进程 spawn 均为同步 IO / 进程操作，
/// 全部经 [`tokio::task::spawn_blocking`] 移出 UI 线程与 Tokio 工作线程；
/// spawn 返回的 `Child` 随即丢弃且**从不 wait**——explorer 是 GUI 进程，
/// 创建成功即独立存活、立即返回，绝不在任何线程上阻塞等待其退出。
/// 终端日志目录（模块设置弹窗）与三类目录（主设置弹窗）共用本入口。
fn open_dir_in_explorer(ui_weak: &slint::Weak<MainWindow>, dir: PathBuf) {
    if EXPLORER_OPEN_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return; // 已有打开请求在途：静默忽略重复触发，避免堆叠资源管理器窗口。
    }
    let weak = ui_weak.clone();
    let dir_text = dir.to_string_lossy().into_owned();
    tokio::spawn(async move {
        let outcome = tokio::task::spawn_blocking(move || platform::open_folder(&dir)).await;
        // 落定后复位单飞守卫：成功 / 失败都允许用户再次发起新一轮打开。
        EXPLORER_OPEN_IN_FLIGHT.store(false, Ordering::SeqCst);
        match outcome {
            Ok(Ok(())) => show_toast(&weak, "已在资源管理器中打开目录"),
            Ok(Err(err)) => {
                tracing::warn!(
                    target: "main",
                    "资源管理器打开目录失败（目录 '{dir_text}'）: {err}"
                );
                show_toast(&weak, "打开目录失败，请稍后重试");
            }
            Err(err) => {
                tracing::error!(target: "main", "资源管理器打开目录任务异常: {err}");
                show_toast(&weak, "打开目录失败，请稍后重试");
            }
        }
    });
}

/// 【UI 回调入口】打开终端交互日志的**当前生效目录**（模块设置弹窗主按钮）。
fn open_terminal_log_dir_in_explorer(
    ui_weak: &slint::Weak<MainWindow>,
    runtime_config: Arc<Mutex<AppConfig>>,
) {
    let weak = ui_weak.clone();
    tokio::spawn(async move {
        let log_dir = {
            let cfg = runtime_config.lock().await;
            cfg.effective_terminal_log_dir()
        };
        open_dir_in_explorer(&weak, log_dir);
    });
}

// ---------------------------------------------------------------------------
// Toast 轻量操作反馈（统一入口：任意线程可调用；事件循环内显隐 + Tokio 定时收回）
// ---------------------------------------------------------------------------

/// Toast 展示时长：2.5s 后由延迟任务自动收回（收回即触发 Slint 侧 150ms 淡出动画）。
const TOAST_DISPLAY_DURATION: std::time::Duration = std::time::Duration::from_millis(2500);

/// Toast 世代号：每次触发自增，供「定时收回」任务判别自己是否已被更新的触发取代。
///
/// 为什么需要：连续快速操作会先后展示多条 Toast（如增删规则、全量启停）。若无世代
/// 判别，前一条 Toast 的 2.5s 收回定时器到期时会无差别地把**后一条**正在展示的
/// Toast 提前关掉。世代号在触发时前进，旧任务的收回回调醒来后发现世代号已过即放弃，
/// 让新 Toast 完整展示自己的时长。
static TOAST_EPOCH: AtomicU64 = AtomicU64::new(0);

/// 【任意线程可调用】在窗口底部中央展示一条 Toast（轻量操作反馈气泡）。
///
/// 职责与线程模型（与本文件其它 UI 投递一致，见 [`deliver_module_sync`]）：
///   1. 经 [`slint::invoke_from_event_loop`] 把「写入文案 + `show_toast = true`」
///      排队到 UI 线程 —— 跨线程载荷仅 `Weak<MainWindow>`（Slint 保证 `Send`）
///      与 `SharedString`，模型 / 窗口对象从不跨线程移动；
///   2. 另起 Tokio 延迟任务：`TOAST_DISPLAY_DURATION`（2.5s）后再次经事件循环
///      复位 `show_toast = false`，Slint 侧随之播放 150ms 淡出 / 下沉离场过渡。
///
/// 提前关闭：点击 Toast 胶囊由 Slint 侧直接复位 `show_toast`（不占用本函数）；其后的
/// 定时收回对已隐藏状态是幂等空操作。调用方须在动作**成功落定 / 生效后**才调用本函数，
/// 失败路径仅告警回滚，不弹 Toast。
fn show_toast(ui_weak: &slint::Weak<MainWindow>, message: &str) {
    // 世代号先行自增：本次触发即宣告所有更早的「定时收回」任务过期。
    let epoch = TOAST_EPOCH.fetch_add(1, Ordering::Relaxed) + 1;

    // 转拥有态：事件循环闭包要求 'static，文案须随闭包移动而非借用调用方栈。
    let message = SharedString::from(message);

    // 1) 展示：写入文案并亮起胶囊（排队到 UI 线程执行）。
    let show_weak = ui_weak.clone();
    let queued = slint::invoke_from_event_loop(move || {
        if let Some(ui) = show_weak.upgrade() {
            ui.set_toast_message(message);
            ui.set_show_toast(true);
        }
    });
    if queued.is_err() {
        tracing::warn!(target: "main", "无法投递 Toast 展示：UI 事件循环已不可用");
        return;
    }

    // 2) 自动收回：2.5s 后复位显隐；若期间已有更新的 Toast 触发（世代号前进）则放弃，
    //    避免把新提示提前关掉。事件循环已退出时投递失败静默忽略（进程正在收尾）。
    let hide_weak = ui_weak.clone();
    tokio::spawn(async move {
        tokio::time::sleep(TOAST_DISPLAY_DURATION).await;
        if TOAST_EPOCH.load(Ordering::Relaxed) != epoch {
            return;
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = hide_weak.upgrade() {
                ui.set_show_toast(false);
            }
        });
    });
}

// ---------------------------------------------------------------------------
// 提权重启（突破 UIPI 的核心入口：UI 盾牌按钮与托盘菜单共用）
// ---------------------------------------------------------------------------

/// 「以管理员身份重启」请求的单飞守卫。
///
/// [`platform::restart_as_admin`] 会同步阻塞在 UAC 确认框上；若不闭锁，用户
/// 在确认框停留期间的连点会堆叠出多个 UAC 提示。首次请求置位后即闭锁，直到
/// 请求**落定**：成功 → 进程即将退出（无需复位）；失败（用户取消等）→ 复位，
/// 允许用户稍后再次发起新一轮提权请求。
static ADMIN_RESTART_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// 【任意线程可调用】以管理员身份重启当前实例（UI 盾牌按钮 / 托盘菜单共用入口）。
///
/// 流程：
/// 1. 单飞检查：已有一份提权重启请求在途（UAC 确认框打开中）时静默忽略重复触发；
/// 2. [`tokio::task::spawn_blocking`] 执行 [`platform::restart_as_admin`]——同步
///    Win32 调用移出 UI 线程与 Tokio 工作线程，UAC 确认期间应用其余部分照常响应；
/// 3. 成功（`ShellExecuteW("runas")` 已拉起提权实例）→ 调度 `slint::quit_event_loop`：
///    主线程沿既有平滑收尾路径（逆序停模块 → 关托盘 → 释放单实例互斥）退出旧进程；
///    新实例凭 [`platform::RESTART_MARKER_ARG`] 完成单实例握手接管；
/// 4. 失败（用户取消 UAC / 账户无权提权 / 系统错误）→ 复位单飞闭锁并给出告警 +
///    Toast 反馈，应用原样继续运行，用户可再次发起。
///
/// 每次请求（成功 / 失败）均写入用户操作审计日志（`app_audit.log`）。
fn trigger_admin_restart(ui_weak: &slint::Weak<MainWindow>, audit: &AuditSink) {
    if ADMIN_RESTART_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        tracing::debug!(target: "main", "忽略重复的提权重启请求（已有请求在途）");
        return;
    }
    let weak = ui_weak.clone();
    let audit = audit.clone();
    tokio::spawn(async move {
        match tokio::task::spawn_blocking(platform::restart_as_admin).await {
            Ok(Ok(())) => {
                audit.record("UAC 提权", "以管理员身份重启", "成功（新实例已拉起）");
                tracing::info!(target: "main", "提权实例已确认启动，调度旧进程平滑收尾");
                let _ = slint::invoke_from_event_loop(|| {
                    let _ = slint::quit_event_loop();
                });
            }
            Ok(Err(err)) => {
                // 最常见的失败 = 用户在 UAC 确认框选择“否”/直接取消（错误码 1223）；
                // 此时应用必须原样存活，仅向用户反馈原因，并复位闭锁允许重试。
                ADMIN_RESTART_IN_FLIGHT.store(false, Ordering::SeqCst);
                audit.record("UAC 提权", "以管理员身份重启", format!("失败: {err}"));
                tracing::warn!(target: "main", "以管理员身份重启失败（应用继续运行）: {err}");
                show_toast(&weak, &format!("以管理员身份重启失败：{err}"));
            }
            Err(err) => {
                ADMIN_RESTART_IN_FLIGHT.store(false, Ordering::SeqCst);
                audit.record("UAC 提权", "以管理员身份重启", "任务异常");
                tracing::error!(target: "main", "提权重启阻塞任务执行异常: {err}");
                show_toast(&weak, "以管理员身份重启任务异常，请重试");
            }
        }
    });
}

// ---------------------------------------------------------------------------
// 配置持久化（弹窗规则的黑名单落盘：单写者串行化，保证最终写入为最后一次操作）
// ---------------------------------------------------------------------------

/// 配置持久化通道的发送端句柄：投递一份「最新黑名单」即触发一次异步落盘。
///
/// 落盘由一个常驻任务串行消费（先进先出），因此即便用户在弹窗里快速连续增删，
/// 最后一次操作对应的黑名单也必然最后写盘——避免并发 `save` 的 last-write-wins
/// 乱序把较早快照覆盖到较新状态上。发送端被 UI 回调持有（进程生命周期内不关闭），
/// 任务在通道关闭（进程收尾）时自然退出。
type ConfigBlacklistSender = tokio::sync::mpsc::UnboundedSender<Vec<String>>;

/// 派生配置持久化任务：接收「最新黑名单」→ 更新运行期配置并异步原子落盘。
fn spawn_blacklist_persister(
    config_mgr: Arc<ConfigManager>,
    runtime_config: Arc<Mutex<AppConfig>>,
) -> ConfigBlacklistSender {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<String>>();
    tokio::spawn(async move {
        while let Some(blacklist) = rx.recv().await {
            // 1) 更新内存中的运行期配置（加锁后整份克隆快照，随即释放锁）。
            let snapshot = {
                let mut cfg = runtime_config.lock().await;
                cfg.popup_blacklist = blacklist;
                cfg.clone()
            };
            // 2) 异步原子写盘（ConfigManager::save 自带写锁串行化）。
            if let Err(err) = config_mgr.save(&snapshot).await {
                tracing::error!(target: "main", "黑名单规则写入配置失败: {err}");
            } else {
                tracing::debug!(target: "main", "黑名单规则已持久化到 tltoolbox.toml");
            }
        }
    });
    tx
}

/// 窗口置顶规则持久化通道的发送端句柄：投递一份「最新置顶规则列表」即触发
/// 一次异步落盘（v0.4.0，与黑名单持久化同构的单写者模式）。
///
/// 由 [`TopmostManagerModule`] 在每次置顶 / 解除 / 改级后投递最新快照；常驻任务
/// 串行消费，保证最后一次操作的规则必然最后写盘。
type TopmostRulesSender = tokio::sync::mpsc::UnboundedSender<Vec<PinnedRule>>;

/// 派生窗口置顶规则持久化任务：接收「最新规则列表」→ 更新配置节 → 原子落盘。
fn spawn_topmost_rules_persister(
    config_mgr: Arc<ConfigManager>,
    runtime_config: Arc<Mutex<AppConfig>>,
) -> TopmostRulesSender {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<PinnedRule>>();
    tokio::spawn(async move {
        while let Some(rules) = rx.recv().await {
            let snapshot = {
                let mut cfg = runtime_config.lock().await;
                cfg.topmost_manager.pinned_rules = rules;
                cfg.clone()
            };
            if let Err(err) = config_mgr.save(&snapshot).await {
                tracing::error!(target: "main", "窗口置顶规则写入配置失败: {err}");
            } else {
                tracing::debug!(target: "main", "窗口置顶规则已持久化到 tltoolbox.toml");
            }
        }
    });
    tx
}

// ---------------------------------------------------------------------------
// 桌面图标布局锁 · 弹窗助手（v0.6.0：指纹采集 / 方案模型重建 / 配置持久化）
// ---------------------------------------------------------------------------

/// 【UI 线程内】把模块方案快照重建为 `icon_locker_profiles` 模型并刷新状态条。
fn set_icon_locker_profiles_model(ui: &MainWindow, module: &IconLockerModule) {
    let profiles = module.profiles();
    let items = profiles
        .iter()
        .map(|profile| IconLockerProfileItem {
            id: SharedString::from(profile.id.clone()),
            name: SharedString::from(profile.name.clone()),
            icon_count: profile.icon_positions.len() as i32,
            fingerprint: SharedString::from(profile.topology_fingerprint.clone()),
        })
        .collect::<Vec<_>>();
    ui.set_icon_locker_profiles(ModelRc::new(VecModel::from(items)));
    ui.set_icon_locker_status(SharedString::from(format!(
        "共 {} 个布局方案",
        profiles.len()
    )));
}

/// 打开「桌面图标布局锁」设置弹窗。
///
/// 线程模型：环境指纹采集（`EnumDisplayMonitors`）虽是轻量 Win32 枚举，仍按既有
/// 纪律经 `spawn_blocking` 移出 UI 线程（COM / 系统枚举严禁阻塞 Slint 事件循环）；
/// 落定后经 `invoke_from_event_loop` 回 UI 线程填充指纹、方案模型与自动还原开关，
/// 再展示弹窗。
fn open_icon_locker_modal(ui_weak: &slint::Weak<MainWindow>, module: Arc<IconLockerModule>) {
    let weak = ui_weak.clone();
    tokio::spawn(async move {
        let fingerprint = tokio::task::spawn_blocking(
            tltoolbox::modules::icon_locker::daemon::current_topology_fingerprint,
        )
        .await
        .unwrap_or_default();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_icon_locker_fingerprint(SharedString::from(fingerprint));
                set_icon_locker_profiles_model(&ui, &module);
                ui.set_icon_locker_auto_restore(module.auto_restore());
                ui.set_show_icon_locker_modal(true);
            }
        });
    });
}

/// 把模块当前方案列表 + 自动还原开关持久化到配置 `[icon_locker]` 节。
///
/// 方案列表的事实源是模块内存态（与启动时注入的配置同源），落盘仅是镜像收敛；
/// 失败仅告警（下一次成功操作 / 重启加载时自愈）。
async fn persist_icon_locker_config(
    config_mgr: &Arc<ConfigManager>,
    runtime_config: &Arc<Mutex<AppConfig>>,
    module: &IconLockerModule,
) {
    let snapshot = {
        let mut cfg = runtime_config.lock().await;
        cfg.icon_locker.profiles = module.profiles();
        cfg.icon_locker.auto_restore = module.auto_restore();
        cfg.clone()
    };
    if let Err(err) = config_mgr.save(&snapshot).await {
        tracing::error!(target: "main", "图标布局锁配置落盘失败: {err}");
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
            Ok(AppEvent::ToastRequested(message)) => {
                // 后台守护线程（如窗口置顶模块的泵线程遭遇 UIPI 拦截）请求的
                // Toast：转发到统一 Toast 入口（show_toast 在 UI 线程展示）。
                show_toast(&ui_weak, &message);
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
/// v0.5.0：跳过无常驻后台开关的模块（`port_hunter` 即开即用工具——全部切换
/// 不应对其产生任何副作用）。
async fn set_all_modules(manager: &SharedManager, enable: bool) {
    for meta in manager.get_metadata_list() {
        if !module_is_toggleable(meta.id) {
            continue; // 即开即用工具：不参与全量启停
        }
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
    audit: AuditSink,
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
                    audit.record(
                        "全部模块",
                        if enable {
                            "托盘：全部启用"
                        } else {
                            "托盘：全部停用"
                        },
                        "进行中",
                    );
                    let mgr = Arc::clone(&manager);
                    let audit = audit.clone();
                    tokio::spawn(async move {
                        set_all_modules(&mgr, enable).await;
                        audit.record(
                            "全部模块",
                            if enable {
                                "托盘：全部启用"
                            } else {
                                "托盘：全部停用"
                            },
                            "完成",
                        );
                    });
                }
                TrayAction::ExitApp => {
                    tracing::info!(target: "main", "托盘「退出程序」触发，调度应用平滑收尾");
                    quit_scheduled = true;
                    let _ = slint::invoke_from_event_loop(|| {
                        let _ = slint::quit_event_loop();
                    });
                }
                TrayAction::RestartAsAdmin => {
                    // 托盘「以管理员身份重启」（仅未提权菜单渲染该项）：与 UI 盾牌
                    // 按钮共用 trigger_admin_restart——成功则经平滑收尾退出旧进程，
                    // 提权副本凭 RESTART_MARKER_ARG 完成单实例握手接管。
                    trigger_admin_restart(&ui_weak, &audit);
                }
            },
            Ok(AppEvent::AppLogAppended { .. }) => {
                // 双栏日志控制台已退役：运行日志由 tracing 承载，此处忽略。
            }
            Ok(AppEvent::ToastRequested(_)) => {
                // 后台守护线程请求的 Toast 由总线 → UI 转发任务处置
                // （forward_module_events），生命周期控制器不重复处理。
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
    // ---- 0. 启动前置（先于日志装配）：静默启动识别（0.1）与单实例守护（0.2）。 ----
    //      v0.3.2 起日志目录经配置自定义（app_log_dir），须先加载配置才能确定日志
    //      落盘位置，故「日志装配 → 单实例 → 配置」的旧顺序调整为
    //      「单实例 → 配置 → 日志装配」：继续执行的路径在日志就绪后统一补记早期日志，
    //      唯一不受影响的例外是「第二实例立即退出」路径（该路径本就不落盘日志）。

    //      0.1 静默启动识别：注册表 Run 键自启时携带 --silent（常驻层据此抑制打扰）。
    let silent_launch = std::env::args().any(|arg| arg == autostart::SILENT_ARG);

    //      0.2 单实例守护：最先于一切有副作用的装配（配置落盘 / 模块启动 / UI /
    //          托盘 / 钩子注册）执行——用户再次双击 exe 或系统重复自启时，检测到
    //          既有实例持有会话级具名互斥（CreateMutexW + ERROR_ALREADY_EXISTS）：
    //          a) 向 HWND_BROADCAST 广播唤醒消息（既有实例托盘消息泵收到后发布
    //             TrayAction::ShowWindow，还原静默主窗口）；
    //          b) 立即退出本进程（return Ok(())，无任何资源被二次注册）。
    //          特例——提权重启握手期（命令行含 RESTART_MARKER_ARG）：新（高权限）
    //          实例由旧实例的 ShellExecuteW("runas") 拉起，而旧实例尚持有互斥、要等
    //          平滑收尾才释放；此时“检测到既有实例”不是重复启动而是预期的交接，
    //          故跳过 b) 的退出路径、降级为无互斥继续装配（旧实例必然随即退出）。
    let restarting_elevated = std::env::args().any(|arg| arg == platform::RESTART_MARKER_ARG);
    let (instance_guard, instance_log) = match single_instance::acquire() {
        Ok(single_instance::SingleInstanceOutcome::Primary(guard)) => (
            Some(guard),
            "单实例守护已就绪：本进程为唯一实例，互斥句柄持有至退出".to_string(),
        ),
        Ok(single_instance::SingleInstanceOutcome::Secondary { .. }) if restarting_elevated => (
            None,
            "提权重启握手期：既有实例即将退出并释放互斥，本实例跳过「第二实例退出」路径继续装配"
                .to_string(),
        ),
        Ok(single_instance::SingleInstanceOutcome::Secondary { wakeup_delivered }) => {
            // 日志尚未装配：该路径立即退出进程（唤醒广播已投递给既有实例）。
            eprintln!(
                "[tltoolbox] 检测到 TLToolBox 已在运行（唤醒广播投递: {wakeup_delivered}），本进程立即退出"
            );
            return Ok(());
        }
        Err(err) => (
            None,
            format!("单实例互斥创建失败，本次降级为允许并行运行: {err}"),
        ),
    };
    let _instance_guard = instance_guard;

    // ---- 1. 配置加载（先于日志装配）：首次运行自动落盘默认 TOML，随后异步加载。
    //      加载失败 → 先以**默认日志目录**（exe 同级 logs/）装配日志，把失败原因
    //      落盘后上报错误；加载成功 → effective_app_log_dir 决定后续日志 / 审计
    //      的落盘目录。 ----
    let config_mgr = Arc::new(ConfigManager::default());
    let app_config = match config_mgr.load().await {
        Ok(cfg) => cfg,
        Err(err) => {
            // 兜底日志装配（默认目录）：保证配置加载失败原因也能写入日志文件。
            let _fallback_guard = logging::init();
            tracing::error!(target: "main", "配置加载失败: {err}");
            return Err(err.into());
        }
    };

    // ---- 2. 日志装配（v0.3.2：目录经配置自定义，缺省回退 exe 同级 logs/）。 ----
    //      debug 构建双写 控制台 + 按天滚动文件；release（无控制台黑框）仅写文件。
    //      返回的 WorkerGuard 由 `_log_guard` 持有到本函数作用域结束——正常收尾、
    //      panic 展开等任何退出路径都会先触发完整刷盘。
    let app_log_dir = app_config.effective_app_log_dir();
    let _log_guard = logging::init_in_dir(&app_log_dir);
    tracing::info!(
        target: "main",
        log_dir = %app_log_dir.display(),
        file_logging = _log_guard.is_file_logging_active(),
        "日志子系统已就绪：debug 双写控制台/文件，release 仅文件（按天滚动，保留 {} 份；目录经配置自定义）",
        logging::MAX_LOG_FILES
    );
    tracing::info!(target: "main", "{instance_log}");
    if silent_launch {
        tracing::info!(target: "main", "检测到 --silent：本次由系统开机自启拉起");
    }
    if restarting_elevated {
        tracing::info!(
            target: "main",
            "检测到 {0}：本进程为提权重启的后继实例",
            platform::RESTART_MARKER_ARG
        );
    }

    // ---- 2.5 用户操作审计（v0.3.2）：与 tracing 日志同目录落盘 app_audit.log。 ----
    //      高精度时间戳（微秒 UTC）由 AuditSink 在记录时刻生成；发送端被下述各
    //      UI 回调句柄持有至进程收尾（落盘任务串行消费，调用方绝不阻塞）。
    let audit = logging::AuditSink::new(app_config.effective_app_log_dir());
    audit.record(
        "应用启动",
        format!("v{}", env!("CARGO_PKG_VERSION")),
        "成功",
    );

    tracing::info!(
        target: "main",
        path = %config_mgr.path().display(),
        auto_start_windows = app_config.auto_start_windows,
        minimize_to_tray = app_config.minimize_to_tray,
        app_log_dir = %app_config.effective_app_log_dir().display(),
        terminal_log_dir = %app_config.effective_terminal_log_dir().display(),
        popup_screenshot_dir = %app_config.effective_popup_screenshot_dir().display(),
        "配置已加载（自动启动模块: {:?}）",
        app_config.auto_start_modules
    );

    // ---- 3. 提权状态快照：读取一次（进程生命周期内不会漂移），供托盘菜单与 UI
    //          决定「以管理员身份重启」入口的展示形态——未提权展示入口（UIPI 限制
    //          下拦截高权限弹窗可能失败），已提权展示“管理员”徽标并隐藏入口。 ----
    let elevated = platform::is_elevated();
    tracing::info!(
        target: "main",
        elevated,
        "管理员权限状态：{}（提权后可拦截高完整性进程弹窗）",
        if elevated { "已提权" } else { "未提权" }
    );

    // 运行期配置句柄：UI「开机自启」切换 / 目录自定义（v0.3.2）后在此更新并落盘
    // （配置为准）。
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
    //      同时保留强类型句柄（`Arc<PopupBlockerModule>`）：规则管理弹窗的回调需要
    //      直接读取 / 热更新黑名单（`current_rules` / `update_rules` 为模块专用方法，
    //      不在 [`ToolModule`](tltoolbox::modules::ToolModule) 契约上）。
    let mut module_mgr = ModuleManager::new(event_bus.clone());
    // 弹窗拦截模块：注入黑名单（配置为准）+ 截图留痕目录（v0.3.2，缺省回退
    // exe 同级 logs/popup_screenshots）。目录由配置解析为绝对路径后固化在模块内，
    // 运行期经「主设置」更改后于模块**下次启动**时生效。
    let popup_blocker = Arc::new(PopupBlockerModule::with_rules_and_screenshot_dir(
        app_config.popup_blacklist.clone(),
        Some(app_config.effective_popup_screenshot_dir()),
    ));
    let popup_rules_count = app_config.popup_blacklist.len();
    // 显式升级为 trait 对象：先克隆具体类型再经 unsize 强转，避免推理歧义。
    let popup_module: Arc<dyn ToolModule> = popup_blocker.clone();
    module_mgr.register(popup_module);
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
    //      终端交互日志模块：以运行期配置句柄装配（与上面 `runtime_config` 同源）——
    //      `start` 时经 `effective_terminal_log_dir` 解析最终日志目录，并按
    //      `enabled_shells` 名单向 PowerShell / bash / CMD 注入会话钩子。钩子全部在
    //      start 期惰性装配，模块注册本身零系统探测 / 改写。同样**不**进入默认自动
    //      启动列表——向用户 Shell 配置文件与注册表 AutoRun 注入钩子属「显式开启才
    //      合理」的系统改写（默认名单仅弹窗拦截，见 AppConfig::default_auto_start_modules）。
    module_mgr.register(Arc::new(TerminalLoggerModule::new(Arc::clone(
        &runtime_config,
    ))));
    //      全局窗口置顶守护模块（v0.4.0）：以配置节装配——置顶规则记忆
    //      （`pinned_rules`，启动恢复）+ 事件总线（守护线程 UIPI 失败 →
    //      ToastRequested 提示提权）。同样**不**进入默认自动启动列表；但
    //      `topmost_manager.enabled = true` 会在自动启动段被视作启动请求
    //      （与 auto_start_modules 等价，见下方 5. 自动启动段）。
    let topmost_manager = Arc::new(
        TopmostManagerModule::with_rules(app_config.topmost_manager.pinned_rules.clone())
            .with_bus(Some(event_bus.clone())),
    );
    topmost_manager.attach_rule_persister(spawn_topmost_rules_persister(
        Arc::clone(&config_mgr),
        Arc::clone(&runtime_config),
    ));
    module_mgr.register(topmost_manager.clone());
    //      本地开发端口猎手（v0.5.0）：即开即用工具——不注册后台常驻轮询，以
    //      有效日志目录（缺省 exe 同级 logs/port_hunter）装配模块专属明细日志器，
    //      事件总线供「一键释放」的成功 / UIPI 拦截 Toast 出口。**不**进入
    //      auto_start_modules 默认列表（无后台运行态，见 module_is_toggleable）。
    let port_hunter = Arc::new(PortHunterModule::new(
        app_config.effective_port_hunter_log_dir(),
    ));
    port_hunter.attach_bus(Some(event_bus.clone()));
    module_mgr.register(port_hunter.clone());
    let icon_locker = Arc::new(
        IconLockerModule::new(app_config.icon_locker.profiles.clone())
            .with_auto_restore(app_config.icon_locker.auto_restore)
            .with_bus(Some(event_bus.clone())),
    );
    module_mgr.register(icon_locker.clone());
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
    //      v0.4.0：`topmost_manager.enabled = true`（配置节）视作自动启动请求，
    //      与 auto_start_modules 等价（窗口置顶守护 = 显式开启才合理，不写入
    //      auto_start_modules 默认列表；但用户一旦开启，重启后应保持守护）。
    let mut auto_start_ids: Vec<String> = app_config.auto_start_modules.clone();
    if app_config.topmost_manager.enabled
        && !auto_start_ids.iter().any(|id| id == "topmost_manager")
    {
        auto_start_ids.push("topmost_manager".to_string());
    }
    if app_config.icon_locker.enabled && !auto_start_ids.iter().any(|id| id == "icon_locker") {
        auto_start_ids.push("icon_locker".to_string());
    }
    for module_id in &auto_start_ids {
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

    ui.set_modules(ModelRc::new(VecModel::from(module_items_from_manager(
        &shared_mgr,
    ))));
    // v0.4.1：窗口置顶弹窗的模块联锁初始值——以自动启动段落定后的真实运行态注入
    //（此后由 refresh_modules_model 随模块状态事件持续收敛）。
    ui.set_topmost_module_enabled(topmost_manager.is_running());
    ui.set_autostart_enabled(autostart::is_autostart_enabled());
    ui.set_app_version(SharedString::from(env!("CARGO_PKG_VERSION")));
    // 提权状态驱动 UI 展示形态：elevated = true → 标题旁「管理员 (Admin)」翡翠徽标、
    // 隐藏提权按钮；false → 渲染醒目的盾牌提权按钮（见 ui/app.slint）。
    ui.set_elevated(elevated);
    // v0.3.2 初始数据注入：
    // - 主设置弹窗：三类目录的当前生效绝对路径；
    // - 弹窗拦截留痕：进程内留痕记录（启动后拦截到的截图列表，新→旧）；
    // - 关于弹窗：检查更新状态为空（未检查过），下载入口不渲染。
    refresh_settings_displays(&ui, &app_config);
    set_captures_model(&ui, popup_blocker.captures());
    ui.set_update_status(SharedString::from(""));
    ui.set_update_available(false);
    // v0.5.0 端口猎手初始选项状态（弹窗打开前保持与配置一致；打开时再重读）。
    ui.set_port_confirm_before_kill(app_config.port_hunter.confirm_before_kill);
    ui.set_port_show_system_ports(app_config.port_hunter.show_system_ports);
    ui.set_port_hunter_search(SharedString::from(""));
    ui.set_port_hunter_entries(ModelRc::new(VecModel::from(Vec::<PortEntryItem>::new())));
    ui.set_port_hunter_status(SharedString::from("打开弹窗后自动扫描本地监听端口"));
    // v0.6.0 桌面图标布局锁初始状态（弹窗打开时再重读 / 重算，见 8.4.7）。
    ui.set_icon_locker_fingerprint(SharedString::from(""));
    ui.set_icon_locker_profiles(ModelRc::new(VecModel::from(
        Vec::<IconLockerProfileItem>::new(),
    )));
    ui.set_icon_locker_auto_restore(app_config.icon_locker.auto_restore);
    ui.set_icon_locker_profile_name(SharedString::from(""));
    ui.set_icon_locker_status(SharedString::from("尚未保存布局方案"));
    tracing::info!(
        target: "main",
        "UI 已实例化，初始注入 {} 个模块（提权展示形态: {}）",
        ui.get_modules().row_count(),
        if elevated { "管理员徽标" } else { "提权按钮" }
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

    // 8.0 Toast 触发回调：Slint 侧 `trigger_toast(string)` 与统一 `show_toast` 同入口。
    //     Rust 内部动作落定后直接调用 show_toast（见下述 8.2 / 8.3 / 8.4.x）；本绑定
    //     仅为把 UI 声明的回调面接通，供未来 UI 内任意元素请求一条 Toast。
    let toast_ui = ui.as_weak();
    ui.on_trigger_toast(move |msg| show_toast(&toast_ui, msg.as_str()));

    // 8.0.1 盾牌提权按钮（仅未提权形态渲染）→ 与托盘菜单共用提权重启入口。
    //       点击后进入 UAC 确认；成功则旧进程平滑收尾退出、提权副本接管。
    //       （请求进 / 出均写入审计日志，见 trigger_admin_restart。）
    let elevate_ui = ui.as_weak();
    let elevate_audit = audit.clone();
    ui.on_request_elevate(move || trigger_admin_restart(&elevate_ui, &elevate_audit));

    // 8.1 模块开关拨动 → 异步调度模块启停（成功 / 失败均写入审计日志）。
    //     v0.4.0 特例：`topmost_manager` 模块的启停镜像到配置节
    //     `topmost_manager.enabled`（配置为准：下次启动据此自动拉起守护）。
    let manager_for_toggle = Arc::clone(&shared_mgr);
    let toggle_audit = audit.clone();
    let toggle_cfg = Arc::clone(&config_mgr);
    let toggle_runtime = Arc::clone(&runtime_config);
    ui.on_toggle_module(move |id, enable| {
        // v0.5.0：即开即用工具（port_hunter）无常驻开关，卡片不渲染物理开关；
        // 此处防御性拦截（全量切换等路径的兜底），避免产生无意义的状态广播。
        if !module_is_toggleable(id.as_str()) {
            return;
        }
        let manager = Arc::clone(&manager_for_toggle);
        let audit = toggle_audit.clone();
        let cfg_mgr = Arc::clone(&toggle_cfg);
        let cfg_lock = Arc::clone(&toggle_runtime);
        let id_text = id.to_string();
        let action_text = format!("{id_text} -> {}", if enable { "开启" } else { "关闭" });
        tokio::spawn(async move {
            match manager.toggle(&id_text, enable).await {
                Ok(_) => {
                    audit.record("模块开关", action_text, "成功");
                    if id_text == "topmost_manager" {
                        // 配置镜像持久化（失败仅告警：模块运行态不受影响）。
                        let snapshot = {
                            let mut cfg = cfg_lock.lock().await;
                            cfg.topmost_manager.enabled = enable;
                            cfg.clone()
                        };
                        if let Err(err) = cfg_mgr.save(&snapshot).await {
                            tracing::warn!(
                                target: "main",
                                "窗口置顶启停状态写入配置失败（下次启动将按配置收敛）: {err}"
                            );
                        }
                    } else if id_text == "icon_locker" {
                        let snapshot = {
                            let mut cfg = cfg_lock.lock().await;
                            cfg.icon_locker.enabled = enable;
                            cfg.clone()
                        };
                        if let Err(err) = cfg_mgr.save(&snapshot).await {
                            tracing::warn!(target: "main", "图标布局锁启停状态写入配置失败: {err}");
                        }
                    }
                }
                Err(err) => {
                    audit.record("模块开关", action_text, format!("失败: {err}"));
                    tracing::error!(target: "main", "切换模块 {id_text} -> {enable} 失败: {err}");
                    // 失败时调度器已广播真实（未变更）状态驱动 UI 回滚开关。
                }
            }
        });
    });

    // 8.2 全局「开机自启」拨动 → 写注册表 + 持久化配置 + UI 回读收敛（审计留痕）。
    let autostart_mgr = Arc::clone(&config_mgr);
    let autostart_cfg = Arc::clone(&runtime_config);
    let autostart_ui = ui.as_weak();
    let autostart_audit = audit.clone();
    ui.on_toggle_autostart(move |enable| {
        let mgr = Arc::clone(&autostart_mgr);
        let cfg_lock = Arc::clone(&autostart_cfg);
        let weak = autostart_ui.clone();
        let audit = autostart_audit.clone();
        let action_text = if enable {
            "开机自启 -> 开启"
        } else {
            "开机自启 -> 关闭"
        };
        tokio::spawn(async move {
            // 1) 注册表镜像（同步 API 走 spawn_blocking，不占用 UI / 运行时工作线程）。
            let applied =
                match tokio::task::spawn_blocking(move || autostart::set_autostart(enable)).await {
                    Ok(Ok(())) => true,
                    Ok(Err(err)) => {
                        audit.record("开机自启", action_text, format!("失败: {err}"));
                        tracing::error!(target: "main", "开机自启写入失败: {err}");
                        false
                    }
                    Err(err) => {
                        audit.record("开机自启", action_text, "任务异常");
                        tracing::error!(target: "main", "开机自启任务执行失败: {err}");
                        false
                    }
                };
            // 2) 写入成功 → 把用户意图持久化到配置（配置为准：下次启动据此收敛注册表），
            //    并弹出 Toast 反馈（已开启 / 已关闭开机自启）。
            if applied {
                let snapshot = {
                    let mut cfg = cfg_lock.lock().await;
                    cfg.auto_start_windows = enable;
                    cfg.clone()
                };
                if let Err(err) = mgr.save(&snapshot).await {
                    audit.record("开机自启", action_text, format!("配置持久化失败: {err}"));
                    tracing::error!(target: "main", "自启意图写入配置失败: {err}");
                } else {
                    audit.record("开机自启", action_text, "成功");
                }
                show_toast(
                    &weak,
                    if enable {
                        "已开启开机自启"
                    } else {
                        "已关闭开机自启"
                    },
                );
            }
            // 3) 回读注册表真实状态刷新开关：失败路径自然回弹为原状态。
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_autostart_enabled(autostart::is_autostart_enabled());
                }
            });
        });
    });

    // 8.3 「全部启用 / 全部停用」→ 与托盘全量切换共用同一实现；批量切换完成后弹出
    //     Toast 反馈（文案与目标状态一致；个别模块失败仅告警不阻断，故仍按目标态提示）。
    let all_mgr = Arc::clone(&shared_mgr);
    let all_ui = ui.as_weak();
    let all_audit = audit.clone();
    ui.on_toggle_all_modules(move |enable| {
        let mgr = Arc::clone(&all_mgr);
        let weak = all_ui.clone();
        let audit = all_audit.clone();
        let action_text = if enable {
            "全部模块 -> 启用"
        } else {
            "全部模块 -> 停用"
        };
        tokio::spawn(async move {
            audit.record("全部模块", action_text, "进行中");
            set_all_modules(&mgr, enable).await;
            audit.record("全部模块", action_text, "完成");
            show_toast(
                &weak,
                if enable {
                    "已全部启动"
                } else {
                    "已全部停止"
                },
            );
        });
    });

    // 8.4 弹窗拦截 · 黑名单规则管理闭环（卡片齿轮 → 规则弹窗查看 / 新增 / 删除）。
    //     规则的事实源始终是 PopupBlockerModule 内部的 RuleStore（写入即归一化 +
    //     去重 + 热更新生效），配置（tltoolbox.toml）与 UI 列表都从它回读收敛，
    //     三者永不产生分支状态。持久化经单写者通道串行落盘（见
    //     spawn_blacklist_persister），避免连续操作写盘乱序。
    let blacklist_persister =
        spawn_blacklist_persister(Arc::clone(&config_mgr), Arc::clone(&runtime_config));

    // 8.4.1 模块卡片齿轮点击 → 按模块 ID 分派设置面板入口（卡片渲染条件见
    //      module_has_settings）。当前：
    //      - popup_blocker：从模块读取最新规则 + 拦截留痕灌入 UI 模型并展示
    //        「规则管理 + 留痕」弹窗（留痕列表随版本号订阅实时刷新，见 8.4.6）；
    //      - terminal_logger：读取当前生效的日志根目录绝对路径写入
    //        terminal_log_dir_display，并展示「终端日志记录 - 存储管理」弹窗
    //        （与 on_open_terminal_modal 共用 open_terminal_settings 入口，见 8.4.5）；
    //      - topmost_manager（v0.4.0）：重置搜索词 → 全量枚举 + 合并置顶状态刷入
    //        topmost_windows 模型 → 展示「管理窗口」弹窗（见 8.8）。
    let open_blocker = Arc::clone(&popup_blocker);
    let open_cfg = Arc::clone(&runtime_config);
    let open_ui = ui.as_weak();
    let open_topmost_module = Arc::clone(&topmost_manager);
    let open_port_module = Arc::clone(&port_hunter);
    let open_icon_module = Arc::clone(&icon_locker);
    let open_port_audit = audit.clone();
    ui.on_open_module_settings(move |module_id| {
        let weak = open_ui.clone();
        match module_id.as_str() {
            "popup_blocker" => {
                if let Some(ui) = weak.upgrade() {
                    set_popup_rules_model(&ui, open_blocker.current_rules());
                    // 打开弹窗时同步拉取最新留痕记录（含进程启动以来已拦截的截图）。
                    set_captures_model(&ui, open_blocker.captures());
                    ui.set_show_rules_modal(true);
                }
            }
            "terminal_logger" => {
                open_terminal_settings(&weak, Arc::clone(&open_cfg));
            }
            "topmost_manager" => {
                if let Some(ui) = weak.upgrade() {
                    ui.set_topmost_search(SharedString::from(""));
                    // v0.4.1：打开弹窗瞬间再同步一次模块运行态联锁（警示条 /
                    // 控件禁用以打开时刻的真实状态为准，不等总线事件回流）。
                    ui.set_topmost_module_enabled(open_topmost_module.is_running());
                }
                // 全量枚举 + 置顶状态合并（阻塞线程枚举 → UI 线程交付模型）。
                refresh_topmost_rows(&weak, Arc::clone(&open_topmost_module));
                if let Some(ui) = weak.upgrade() {
                    ui.set_show_topmost_modal(true);
                }
            }
            "icon_locker" => {
                open_icon_locker_modal(&weak, Arc::clone(&open_icon_module));
            }
            "port_hunter" => {
                // 读取配置选项 → 展示弹窗 → 立即扫描（v0.5.0，见 8.9 的接线）。
                open_port_hunter_modal(
                    &weak,
                    Arc::clone(&open_port_module),
                    Arc::clone(&open_cfg),
                    open_port_audit.clone(),
                );
            }
            other => {
                tracing::warn!(target: "main", "模块 {other} 尚无设置面板实现（齿轮点击忽略）");
            }
        }
    });

    let close_icon_ui = ui.as_weak();
    ui.on_close_icon_locker_modal(move || {
        if let Some(ui) = close_icon_ui.upgrade() {
            ui.set_show_icon_locker_modal(false);
        }
    });

    // 8.4.7 桌面图标布局锁 · 弹窗回调闭环（v0.6.0）：
    //      - icon_locker_save(name)：抓取当前布局 → 保存方案 → 持久化 → 刷新列表；
    //      - icon_locker_restore(id)：按 ID 批量还原（COM 经独立 OS 线程，绝不
    //        阻塞 UI 线程与 Tokio 工作线程池）；失败 / 被系统弹回时 Toast 提示排查
    //        「自动排列图标」；
    //      - icon_locker_delete(id)：删除方案 → 持久化 → 刷新列表；
    //      - icon_locker_auto_restore_changed(checked)：切换自动还原开关并持久化。
    //      COM 抓取 / 还原统一走 explorer::spawn_com_thread：std::thread::spawn 拉
    //      **全新 OS 线程**，线程内 CoInitializeEx(COINIT_APARTMENTTHREADED) → 操作
    //      → CoUninitialize（RPC_E_CHANGED_MODE 容错），结果经 tokio::sync::oneshot
    //      异步回传——Tokio 工作线程池永不接触 Shell COM（避免 spawn_blocking 阻塞
    //      池线程的 COM 公寓污染）；UI 模型改写仅在 invoke_from_event_loop 闭包内
    //      执行（与既有模块同一跨线程纪律）。
    let save_icon_module = Arc::clone(&icon_locker);
    let save_icon_cfg = Arc::clone(&config_mgr);
    let save_icon_runtime = Arc::clone(&runtime_config);
    let save_icon_ui = ui.as_weak();
    let save_icon_audit = audit.clone();
    ui.on_icon_locker_save(move |name| {
        let weak = save_icon_ui.clone();
        let module = Arc::clone(&save_icon_module);
        let cfg_mgr = Arc::clone(&save_icon_cfg);
        let cfg_lock = Arc::clone(&save_icon_runtime);
        let audit = save_icon_audit.clone();
        let trimmed = name.trim().to_string();
        let profile_name = if trimmed.is_empty() {
            let seconds = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("布局-{seconds}")
        } else {
            trimmed
        };
        // COM 抓取在**全新独立 OS 线程**执行（绝不占用 Tokio 阻塞池线程）：线程内
        // CoInitializeEx(COINIT_APARTMENTTHREADED)（RPC_E_CHANGED_MODE 容错）→
        // capture_profile → CoUninitialize，结果经 oneshot 通道异步回传（见
        // explorer::spawn_com_thread）。
        let module_for_capture = Arc::clone(&module);
        let (com_tx, com_rx) = tokio::sync::oneshot::channel();
        let spawn_result = explorer::spawn_com_thread(
            "tlt-icon-locker-save",
            move || module_for_capture.capture_profile(profile_name),
            com_tx,
        );
        tokio::spawn(async move {
            if let Err(err) = spawn_result {
                audit.record("图标布局", "保存方案", format!("独立线程启动失败: {err}"));
                show_toast(&weak, &format!("保存布局失败：无法启动 COM 线程（{err}）"));
                return;
            }
            match com_rx.await {
                Ok(Ok(profile)) => {
                    persist_icon_locker_config(&cfg_mgr, &cfg_lock, &module).await;
                    audit.record(
                        "图标布局",
                        format!(
                            "保存方案 \"{}\"（{} 个图标）",
                            profile.name,
                            profile.icon_positions.len()
                        ),
                        "成功",
                    );
                    let weak_inner = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak_inner.upgrade() {
                            set_icon_locker_profiles_model(&ui, &module);
                            ui.set_icon_locker_profile_name(SharedString::from(""));
                        }
                    });
                    show_toast(&weak, "已保存桌面图标布局");
                }
                Ok(Err(err)) => {
                    audit.record("图标布局", "保存方案", format!("失败: {err}"));
                    show_toast(&weak, &format!("保存布局失败：{err}"));
                }
                Err(_) => {
                    audit.record("图标布局", "保存方案", "任务异常（COM 结果通道中断）");
                    show_toast(&weak, "保存布局任务异常，请重试");
                }
            }
        });
    });

    let restore_icon_module = Arc::clone(&icon_locker);
    let restore_icon_ui = ui.as_weak();
    let restore_icon_audit = audit.clone();
    ui.on_icon_locker_restore(move |id| {
        let weak = restore_icon_ui.clone();
        let module = Arc::clone(&restore_icon_module);
        let audit = restore_icon_audit.clone();
        let id_text = id.to_string();
        // 与保存同一 COM 线程纪律：独立 OS 线程 + STA 生命周期 + oneshot 回传
        // （explorer::spawn_com_thread），Tokio 工作线程池不接触 Shell COM。
        let module_for_restore = Arc::clone(&module);
        let id_for_task = id_text.clone();
        let (com_tx, com_rx) = tokio::sync::oneshot::channel();
        let spawn_result = explorer::spawn_com_thread(
            "tlt-icon-locker-restore",
            move || module_for_restore.restore_profile(&id_for_task),
            com_tx,
        );
        tokio::spawn(async move {
            if let Err(err) = spawn_result {
                audit.record(
                    "图标布局",
                    format!("还原方案 {id_text}"),
                    format!("独立线程启动失败: {err}"),
                );
                show_toast(&weak, &format!("还原布局失败：无法启动 COM 线程（{err}）"));
                return;
            }
            match com_rx.await {
                Ok(Ok(())) => {
                    audit.record("图标布局", format!("还原方案 {id_text}"), "成功");
                    show_toast(
                        &weak,
                        "已还原桌面图标布局（若图标被系统弹回，请排查桌面右键「自动排列图标」）",
                    );
                }
                Ok(Err(err)) => {
                    audit.record("图标布局", format!("还原方案 {id_text}"), format!("失败: {err}"));
                    show_toast(
                        &weak,
                        &format!("还原布局失败：{err}（若图标被系统弹回，请排查桌面右键「自动排列图标」）"),
                    );
                }
                Err(_) => {
                    audit.record(
                        "图标布局",
                        format!("还原方案 {id_text}"),
                        "任务异常（COM 结果通道中断）",
                    );
                    show_toast(&weak, "还原布局任务异常，请重试");
                }
            }
        });
    });

    let delete_icon_module = Arc::clone(&icon_locker);
    let delete_icon_cfg = Arc::clone(&config_mgr);
    let delete_icon_runtime = Arc::clone(&runtime_config);
    let delete_icon_ui = ui.as_weak();
    let delete_icon_audit = audit.clone();
    ui.on_icon_locker_delete(move |id| {
        let weak = delete_icon_ui.clone();
        let module = Arc::clone(&delete_icon_module);
        let cfg_mgr = Arc::clone(&delete_icon_cfg);
        let cfg_lock = Arc::clone(&delete_icon_runtime);
        let audit = delete_icon_audit.clone();
        let id_text = id.to_string();
        tokio::spawn(async move {
            let removed = module.remove_profile(&id_text);
            if removed {
                persist_icon_locker_config(&cfg_mgr, &cfg_lock, &module).await;
                audit.record("图标布局", format!("删除方案 {id_text}"), "成功");
                let weak_inner = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak_inner.upgrade() {
                        set_icon_locker_profiles_model(&ui, &module);
                    }
                });
                show_toast(&weak, "已删除布局方案");
            } else {
                audit.record("图标布局", format!("删除方案 {id_text}"), "未找到");
                show_toast(&weak, "未找到该布局方案");
            }
        });
    });

    let auto_icon_module = Arc::clone(&icon_locker);
    let auto_icon_cfg = Arc::clone(&config_mgr);
    let auto_icon_runtime = Arc::clone(&runtime_config);
    let auto_icon_ui = ui.as_weak();
    let auto_icon_audit = audit.clone();
    ui.on_icon_locker_auto_restore_changed(move |checked| {
        let weak = auto_icon_ui.clone();
        let module = Arc::clone(&auto_icon_module);
        let cfg_mgr = Arc::clone(&auto_icon_cfg);
        let cfg_lock = Arc::clone(&auto_icon_runtime);
        let audit = auto_icon_audit.clone();
        tokio::spawn(async move {
            module.set_auto_restore(checked);
            persist_icon_locker_config(&cfg_mgr, &cfg_lock, &module).await;
            audit.record(
                "图标布局",
                format!("自动还原 -> {}", if checked { "开启" } else { "关闭" }),
                "成功",
            );
            show_toast(
                &weak,
                if checked {
                    "已开启显示器拓扑变化自动还原"
                } else {
                    "已关闭自动还原"
                },
            );
        });
    });

    let close_ui = ui.as_weak();
    ui.on_close_rules_modal(move || {
        if let Some(ui) = close_ui.upgrade() {
            ui.set_show_rules_modal(false);
        }
    });

    // 8.4.3 新增规则：热更新 RuleStore → 顺序持久化 → 回读归一化结果刷新 UI。
    //       （空白输入由 Slint 侧按钮禁用 + 此处 trim 双保险；去重由 RuleStore
    //         统一完成，重复关键词在此静默合并、UI 列表不变。）
    let add_blocker = Arc::clone(&popup_blocker);
    let add_persist = blacklist_persister.clone();
    let add_ui = ui.as_weak();
    let add_audit = audit.clone();
    ui.on_add_rule(move |text| {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let mut rules = add_blocker.current_rules();
        rules.push(trimmed.to_string());
        add_blocker.update_rules(rules);
        let normalized = add_blocker.current_rules();
        let _ = add_persist.send(normalized.clone());
        deliver_popup_rules_refresh(&add_ui, normalized);
        add_audit.record("黑名单", format!("添加关键词 \"{trimmed}\""), "成功");
        // 生效反馈：RuleStore 已热更新并立即生效（拦截泵按新黑名单匹配）、持久化已排队落盘。
        show_toast(&add_ui, "拦截规则已添加并生效");
    });

    // 8.4.4 删除规则（按弹窗行号；行号与 current_rules 顺序一致）。
    //
    // 采用「文本匹配优先、行号兜底」的策略：以用户**当前看到的那一行**的文字为准，
    // 从 RuleStore 中找到同一条目删除。即便上一条操作的 UI 刷新仍在排队（列表尚未
    // 重排、行号短暂滞后），点击也只会删掉用户目之所及的那条规则，绝不错删。
    let remove_blocker = Arc::clone(&popup_blocker);
    let remove_persist = blacklist_persister.clone();
    let remove_ui = ui.as_weak();
    let remove_audit = audit.clone();
    ui.on_remove_rule(move |index| {
        let idx = index as usize;
        // 1) 读当前可见行的文字（UI 线程内同步读取模型是安全的）。
        let visible_text = remove_ui
            .upgrade()
            .and_then(|ui| ui.get_popup_rules().row_data(idx))
            .map(|text| text.to_string());

        let mut rules = remove_blocker.current_rules();
        let remove_at = visible_text
            .as_deref()
            .and_then(|text| rules.iter().position(|rule| rule.as_str() == text))
            .unwrap_or(idx); // 文本匹配失败（异常状态）→ 退化为按行号删除
        if remove_at >= rules.len() {
            tracing::warn!(target: "main", "删除规则越界：index={index}，当前共 {} 条", rules.len());
            return;
        }
        let removed = rules.remove(remove_at);
        remove_blocker.update_rules(rules);
        let normalized = remove_blocker.current_rules();
        let _ = remove_persist.send(normalized.clone());
        remove_audit.record("黑名单", format!("删除关键词 \"{removed}\""), "成功");
        tracing::info!(target: "main", "已删除拦截关键词 \"{removed}\"（剩余 {} 条）", normalized.len());
        deliver_popup_rules_refresh(&remove_ui, normalized);
        // 生效反馈：RuleStore 已热更新并立即生效、持久化已排队落盘。
        show_toast(&remove_ui, "拦截规则已删除");
    });

    // 8.4.5 终端交互日志 · 存储管理弹窗回调接线：
    //      a) open_terminal_modal：Slint 声明面的预留打开入口（与 8.4.1 的
    //         terminal_logger 齿轮分派共用 open_terminal_settings，供未来 UI 内
    //         任意元素直接请求打开该弹窗）；
    //      b) close_terminal_modal：弹窗右上角「×」/ 点击遮罩空白 → 复位显隐；
    //      c) open_log_dir_in_explorer：「在文件资源管理器中打开日志目录」主按钮
    //         → 保证目录存在后以 explorer 打开并 Toast 反馈（见 helper 文档）。
    let open_term_cfg = Arc::clone(&runtime_config);
    let open_term_ui = ui.as_weak();
    ui.on_open_terminal_modal(move || {
        open_terminal_settings(&open_term_ui, Arc::clone(&open_term_cfg));
    });

    let close_term_ui = ui.as_weak();
    ui.on_close_terminal_modal(move || {
        if let Some(ui) = close_term_ui.upgrade() {
            ui.set_show_terminal_modal(false);
        }
    });

    let explorer_cfg = Arc::clone(&runtime_config);
    let explorer_ui = ui.as_weak();
    ui.on_open_log_dir_in_explorer(move || {
        open_terminal_log_dir_in_explorer(&explorer_ui, Arc::clone(&explorer_cfg));
    });

    // 8.5 「关于」弹窗（v0.3.2）：显隐 / 开源地址（ShellExecuteW 打开浏览器）/
    //     检查更新（WinHttp 原生请求 GitHub Releases）。
    let about_ui = ui.as_weak();
    ui.on_open_about_modal(move || {
        if let Some(ui) = about_ui.upgrade() {
            ui.set_show_about_modal(true);
        }
    });

    let about_close_ui = ui.as_weak();
    ui.on_close_about_modal(move || {
        if let Some(ui) = about_close_ui.upgrade() {
            ui.set_show_about_modal(false);
        }
    });

    // 8.5.1 打开开源地址（点击「关于」里的链接行）→ 系统默认浏览器。
    let repo_ui = ui.as_weak();
    let repo_audit = audit.clone();
    ui.on_open_repo_url(move || {
        let weak = repo_ui.clone();
        let audit = repo_audit.clone();
        tokio::spawn(async move {
            match tokio::task::spawn_blocking(move || platform::open_url(update::REPO_URL)).await {
                Ok(Ok(())) => audit.record("打开开源地址", update::REPO_URL, "成功"),
                Ok(Err(err)) => {
                    audit.record("打开开源地址", update::REPO_URL, format!("失败: {err}"));
                    show_toast(&weak, &format!("打开开源地址失败：{err}"));
                }
                Err(_err) => {
                    audit.record("打开开源地址", update::REPO_URL, "任务异常");
                    show_toast(&weak, "打开开源地址任务异常，请重试");
                }
            }
        });
    });

    // 8.5.2 打开更新下载页（发现新版本后点击下载入口）→ 系统默认浏览器。
    let download_ui = ui.as_weak();
    let download_audit = audit.clone();
    ui.on_open_download_url(move || {
        let weak = download_ui.clone();
        let audit = download_audit.clone();
        tokio::spawn(async move {
            match tokio::task::spawn_blocking(move || platform::open_url(update::RELEASES_PAGE_URL))
                .await
            {
                Ok(Ok(())) => audit.record("打开下载页", update::RELEASES_PAGE_URL, "成功"),
                Ok(Err(err)) => {
                    audit.record(
                        "打开下载页",
                        update::RELEASES_PAGE_URL,
                        format!("失败: {err}"),
                    );
                    show_toast(&weak, &format!("打开下载页失败：{err}"));
                }
                Err(_err) => {
                    audit.record("打开下载页", update::RELEASES_PAGE_URL, "任务异常");
                    show_toast(&weak, "打开下载页任务异常，请重试");
                }
            }
        });
    });

    // 8.5.3 检查更新：WinHttp 请求 GitHub Releases 接口并与当前版本比对。
    let check_ui = ui.as_weak();
    let check_audit = audit.clone();
    ui.on_check_for_updates(move || check_for_updates(&check_ui, &check_audit));

    // 8.6 主设置弹窗（v0.3.2：三类目录自定义）：
    //     - 打开前回刷展示（读取运行期配置的 effective_* 绝对路径）；
    //     - 更改 / 打开 / 恢复默认统一经 handle_dir_setting 处理（见 helper 文档）。
    let settings_ui = ui.as_weak();
    let settings_cfg = Arc::clone(&runtime_config);
    ui.on_open_settings_modal(move || {
        let weak = settings_ui.clone();
        let cfg_lock = Arc::clone(&settings_cfg);
        tokio::spawn(async move {
            let cfg = cfg_lock.lock().await.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    refresh_settings_displays(&ui, &cfg);
                    ui.set_show_settings_modal(true);
                }
            });
        });
    });

    let settings_close_ui = ui.as_weak();
    ui.on_close_settings_modal(move || {
        if let Some(ui) = settings_close_ui.upgrade() {
            ui.set_show_settings_modal(false);
        }
    });

    // 8.6.1 目录操作三连：更改（原生文件夹选择）/ 打开（资源管理器）/ 恢复默认。
    let browse_cfg = Arc::clone(&config_mgr);
    let browse_runtime = Arc::clone(&runtime_config);
    let browse_ui = ui.as_weak();
    let browse_audit = audit.clone();
    ui.on_browse_setting_dir(move |key| {
        handle_dir_setting(
            &browse_ui,
            Arc::clone(&browse_cfg),
            Arc::clone(&browse_runtime),
            browse_audit.clone(),
            key.to_string(),
            DirSettingAction::Browse,
        );
    });

    let open_cfg = Arc::clone(&config_mgr);
    let open_runtime = Arc::clone(&runtime_config);
    let open_ui = ui.as_weak();
    let open_audit = audit.clone();
    ui.on_open_setting_dir(move |key| {
        handle_dir_setting(
            &open_ui,
            Arc::clone(&open_cfg),
            Arc::clone(&open_runtime),
            open_audit.clone(),
            key.to_string(),
            DirSettingAction::Open,
        );
    });

    let reset_cfg = Arc::clone(&config_mgr);
    let reset_runtime = Arc::clone(&runtime_config);
    let reset_ui = ui.as_weak();
    let reset_audit = audit.clone();
    ui.on_reset_setting_dir(move |key| {
        handle_dir_setting(
            &reset_ui,
            Arc::clone(&reset_cfg),
            Arc::clone(&reset_runtime),
            reset_audit.clone(),
            key.to_string(),
            DirSettingAction::Reset,
        );
    });

    // 8.7 弹窗拦截留痕（v0.3.2）：
    //     - capture_selected：选中留痕行 → 加载 PNG 预览图（Image::load_from_path）；
    //     - open_capture_dir：资源管理器打开当前截图目录（目录不存在自动创建）。
    let preview_ui = ui.as_weak();
    ui.on_capture_selected(move |index| {
        if let Some(ui) = preview_ui.upgrade() {
            load_capture_preview(&ui, index as usize);
        }
    });

    let capture_dir_ui = ui.as_weak();
    let capture_dir_cfg = Arc::clone(&runtime_config);
    ui.on_open_capture_dir(move || {
        let weak = capture_dir_ui.clone();
        let cfg_lock = Arc::clone(&capture_dir_cfg);
        tokio::spawn(async move {
            let dir = {
                let cfg = cfg_lock.lock().await;
                cfg.effective_popup_screenshot_dir()
            };
            open_dir_in_explorer(&weak, dir);
        });
    });

    // 8.7.1 留痕版本号订阅：每次截图成功落盘即刷新 UI 的 captures 模型（弹窗开着时
    //      实时出现新记录；闭着时下一次打开自动拉取最新列表）。发送端随进程收尾
    //      关闭，changed() 返回 Err 后本任务自然退出。
    let mut capture_version = popup_blocker.captures_watch();
    let capture_ui = ui.as_weak();
    let capture_blocker = Arc::clone(&popup_blocker);
    tokio::spawn(async move {
        while capture_version.changed().await.is_ok() {
            let records = capture_blocker.captures();
            deliver_captures_refresh(&capture_ui, records);
        }
    });

    // 8.8 全局窗口置顶 · 管理弹窗回调接线（v0.4.0；v0.4.1 事件契约修订）：
    //     - close_topmost_modal：弹窗右上角「×」/ 点击遮罩空白 → 复位显隐；
    //     - refresh_topmost：顶栏「⟳ 刷新」→ 重新全量枚举 + 合并置顶状态；
    //     - topmost_search_changed：搜索框实时输入 → 按当前搜索词重刷模型；
    //     - topmost_toggle_pin(hwnd_value, pinned)：行开关 → 立即应用 / 解除置顶
    //       （hwnd_value 为 HWND 裸值截断，**绝不携带行索引**——搜索过滤后仍按
    //       HWND 绝对匹配真实窗口；审计 + Toast + 刷新列表）；
    //     - topmost_set_priority(hwnd_value, priority)：优先级步进器 → 改级
    //       （含非受管行上步进器未启用的防御：仅对已置顶窗口生效）。
    let close_tm_ui = ui.as_weak();
    ui.on_close_topmost_modal(move || {
        if let Some(ui) = close_tm_ui.upgrade() {
            ui.set_show_topmost_modal(false);
        }
    });

    let refresh_tm_ui = ui.as_weak();
    let refresh_tm_module = Arc::clone(&topmost_manager);
    ui.on_refresh_topmost(move || {
        refresh_topmost_rows(&refresh_tm_ui, Arc::clone(&refresh_tm_module));
    });

    let search_tm_ui = ui.as_weak();
    ui.on_topmost_search_changed(move |_text| {
        if let Some(ui) = search_tm_ui.upgrade() {
            apply_topmost_search(&ui);
        }
    });

    let pin_tm_ui = ui.as_weak();
    let pin_tm_module = Arc::clone(&topmost_manager);
    let pin_tm_audit = audit.clone();
    ui.on_topmost_toggle_pin(move |hwnd_value, pinned| {
        // 置顶时优先级取行内当前值（模型按 hwnd_value 绝对匹配；未命中回退默认）。
        // 行内步进器显示的优先级即为置顶生效值，模型 reset 后仍按句柄找得到。
        let priority = pin_tm_ui
            .upgrade()
            .and_then(|ui| {
                ui.get_topmost_windows()
                    .iter()
                    .find(|row| row.hwnd_value == hwnd_value)
                    .map(|row| row.priority)
            })
            .unwrap_or(tltoolbox::modules::topmost_manager::DEFAULT_PRIORITY as i32);
        handle_topmost_pin(
            &pin_tm_ui,
            Arc::clone(&pin_tm_module),
            pin_tm_audit.clone(),
            hwnd_value as isize,
            priority,
            pinned,
        );
    });

    let prio_tm_ui = ui.as_weak();
    let prio_tm_module = Arc::clone(&topmost_manager);
    let prio_tm_audit = audit.clone();
    ui.on_topmost_set_priority(move |hwnd_value, priority| {
        handle_topmost_priority(
            &prio_tm_ui,
            Arc::clone(&prio_tm_module),
            prio_tm_audit.clone(),
            hwnd_value as isize,
            priority,
        );
    });

    // 8.9 端口占用管理（v0.5.0）弹窗回调接线：
    //     - close_port_hunter_modal：弹窗右上角「×」/ 点击遮罩空白 → 复位显隐；
    //     - refresh_port_hunter：顶栏「刷新」→ 重新扫描监听端口并重刷列表；
    //     - port_hunter_search_changed：搜索框实时输入 → 客户端即时过滤（阶段
    //       2/4 + 搜索匹配全部走纯函数，零重新枚举）+ 关键词写入模块明细日志
    //       （含显式端口豁免提示）；
    //     - port_confirm_toggled：复选「二次确认」→ 即时保存配置（不重扫）；
    //     - port_system_toggled：复选「显示系统服务与高位端口」→ 即时保存配置
    //       并**立即重新扫描**使显隐选项生效；
    //     - port_kill_requested(pid, port, protocol)：「一键释放」/ 行内确认 →
    //       终止进程（审计 + Toast + 重扫收敛，见 handle_port_kill）。
    let close_ph_ui = ui.as_weak();
    ui.on_close_port_hunter_modal(move || {
        if let Some(ui) = close_ph_ui.upgrade() {
            ui.set_show_port_hunter_modal(false);
        }
    });

    // 端口猎手弹窗共享上下文：供刷新（scan）与选项切换（即时保存 + 可选重扫）复用。
    let ph_ctx = Arc::new(PortHunterCtx {
        ui: ui.as_weak(),
        module: Arc::clone(&port_hunter),
        config_mgr: Arc::clone(&config_mgr),
        runtime_config: Arc::clone(&runtime_config),
        audit: audit.clone(),
    });

    let refresh_ph_ctx = Arc::clone(&ph_ctx);
    ui.on_refresh_port_hunter(move || {
        refresh_ph_ctx.scan();
    });

    let search_ph_ui = ui.as_weak();
    let search_ph_module = Arc::clone(&port_hunter);
    ui.on_port_hunter_search_changed(move |text| {
        // 关键词留痕（含显式端口豁免提示）：仅记录非空输入。
        if !text.trim().is_empty() {
            search_ph_module.log_search(text.as_str());
        }
        if let Some(ui) = search_ph_ui.upgrade() {
            refresh_port_hunter_display(&ui, &search_ph_module);
        }
    });

    let confirm_ph_ctx = Arc::clone(&ph_ctx);
    ui.on_port_confirm_toggled(move |checked| {
        // v0.5.1 调试输出：验证 CheckBox 状态变更到达 Rust 侧（真机排查复选框
        // 失效用；debug 构建打印到终端，release 走下方 tracing 落盘日志）。
        println!("[PORT] Checkbox toggled, new state: {}", checked);
        tracing::info!(target: "main", "[PORT] 二次确认 CheckBox -> {checked}");
        confirm_ph_ctx.persist_option("confirm", checked, false);
    });

    let system_ph_ctx = Arc::clone(&ph_ctx);
    ui.on_port_system_toggled(move |checked| {
        // v0.5.1 调试输出（用户要求）：复选框每次切换都必须能在启动日志中看到
        // 新状态；该值经双向绑定同步回 UI 属性并驱动下方 rescan 的物理重扫。
        println!("[PORT] Checkbox toggled, new state: {}", checked);
        tracing::info!(
            target: "main",
            "[PORT] 显示系统服务与高位端口 CheckBox -> {checked}"
        );
        system_ph_ctx.persist_option("system", checked, true);
    });

    let kill_ph_ui = ui.as_weak();
    let kill_ph_module = Arc::clone(&port_hunter);
    let kill_ph_runtime = Arc::clone(&runtime_config);
    let kill_ph_audit = audit.clone();
    ui.on_port_kill_requested(move |pid, port, protocol| {
        handle_port_kill(
            &kill_ph_ui,
            Arc::clone(&kill_ph_module),
            Arc::clone(&kill_ph_runtime),
            kill_ph_audit.clone(),
            pid,
            port,
            protocol.to_string(),
        );
    });

    // ---- 9. 托盘装配与生命周期控制（桌面常驻核心机制）。 ----
    //      第三个参数 elevated 决定托盘菜单是否渲染「以管理员身份重启」：
    //      未提权渲染（UIPI 突围入口），已提权隐藏。
    let tray_handle = match tray::spawn(
        event_bus.clone(),
        all_modules_enabled(&shared_mgr),
        elevated,
    ) {
        Ok(handle) => {
            tracing::info!(
                target: "main",
                "系统托盘已就绪（右键菜单：显示主窗口 / 全部模块 / {}退出程序）",
                if elevated { "" } else { "以管理员身份重启 / " }
            );
            Some(handle)
        }
        Err(err) => {
            tracing::warn!(target: "main", "系统托盘不可用，降级为无托盘运行: {err}");
            None
        }
    };

    // 托盘常驻闭环是否成立：配置开启**且**托盘图标就绪。成立时关闭按钮
    // “隐藏到托盘”且事件循环用 run_event_loop_until_quit（窗口隐藏 / 关闭都
    // 不终止进程——slint 的 keepalive 语义是「最后一个可见窗口 / 托盘图标消失
    // 即退出」；只有托盘「退出程序」/ 提权重启的 quit_event_loop 才收尾）。
    // 不成立时（配置关闭 / 托盘初始化失败）不挂常驻钩子：关闭窗口 = 退出进程。
    let tray_resident = app_config.minimize_to_tray && tray_handle.is_some();
    if tray_resident {
        // 关闭按钮“隐藏到托盘”（窗口对象保留，可经托盘再次显示）：
        // 回调返回 KeepWindowShown 拒绝系统关闭，随后经 invoke_from_event_loop
        // 把 hide() + 内存压制（EmptyWorkingSet）排到**下一轮**事件循环执行。
        // 经验约束（slint 1.17 + winit 实测）：普通 run_event_loop 在关闭回调
        // 内同步 hide() 会释放窗口 keepalive、让循环立即退出；把隐藏延后到
        // 关闭处理完全落定（且循环为 run_event_loop_until_quit 形态）后才执行，
        // 进程才能保持常驻。
        let ui_weak = ui.as_weak();
        ui.window().on_close_requested(move || {
            let hide_ui = ui_weak.clone();
            let queued = slint::invoke_from_event_loop(move || {
                if let Some(ui) = hide_ui.upgrade() {
                    let _ = ui.hide();
                }
                // 窗口已隐藏：常驻期的窗口对象无需热页面，压制物理内存工作集
                //（尽力而为；失败仅告警，绝不阻断常驻）。
                if let Err(err) = platform::empty_working_set() {
                    tracing::warn!(target: "main", "隐藏进托盘后内存工作集压制失败: {err}");
                }
            });
            if queued.is_err() {
                tracing::warn!(target: "main", "关闭请求后无法排队隐藏动作：事件循环已不可用");
            }
            tracing::debug!(target: "main", "窗口关闭请求 → 已排队隐藏到系统托盘（进程常驻）");
            CloseRequestResponse::KeepWindowShown
        });
    }

    // 生命周期控制器：独立订阅总线，消费托盘指令并回写托盘菜单文案。
    let lc_rx = event_bus.subscribe();
    let lc_ui = ui.as_weak();
    let lc_mgr = Arc::clone(&shared_mgr);
    let lc_tray = tray_handle.as_ref().map(|handle| handle.control());
    let lc_audit = audit.clone();
    tokio::spawn(async move {
        lifecycle_controller(lc_rx, lc_mgr, lc_ui, lc_tray, lc_audit).await;
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
        ui.show()
            .map_err(|err| -> AppError { format!("Slint 主窗口显示失败: {err}").into() })?;
        tracing::info!(target: "main", "主窗口已显示，TLToolBox 进入前台运行");
    }

    tracing::info!(target: "main", "TLToolBox 启动完毕，进入事件循环");
    if tray_resident {
        // 常驻闭环：窗口隐藏 / 显示切换都不终止循环，仅 quit_event_loop 收尾
        // （托盘「退出程序」/ 提权重启）。这一形态同时修正了 slint 默认循环
        // 「最后一个可见窗口消失即退出」对托盘应用的误伤（含 --silent 静默
        // 常驻：窗口从未显示也可长期驻留）。
        slint::run_event_loop_until_quit().map_err(|err| -> AppError {
            format!("Slint 事件循环运行失败: {err}").into()
        })?;
    } else {
        // 常规形态（minimize_to_tray 关闭 / 托盘不可用）：关闭最后窗口即退出。
        slint::run_event_loop().map_err(|err| -> AppError {
            format!("Slint 事件循环运行失败: {err}").into()
        })?;
    }
    tracing::info!(target: "main", "UI 事件循环已退出，开始平滑收尾");

    // ---- 11. 收尾：先逆序停止全部仍在运行的模块（平滑卸载 Win32 钩子等
    //           资源），再关闭托盘线程释放图标。 ----
    for meta in shared_mgr.get_metadata_list() {
        if meta.running {
            match shared_mgr.toggle(meta.id, false).await {
                Ok(_) => tracing::info!(target: "main", "收尾：模块 {0} 已停止", meta.id),
                Err(err) => {
                    tracing::error!(target: "main", "收尾：停止模块 {0} 失败: {err}", meta.id)
                }
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
