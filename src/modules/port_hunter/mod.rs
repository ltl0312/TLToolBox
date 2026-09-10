//! # 本地开发端口猎手（v0.5.0 · 第六个模块卡片：即开即用工具）
//!
//! 解决本地开发中频繁出现的端口占用冲突（8080 / 3000 / 5173 被占用）：基于
//! Windows 原生网络 API（iphlpapi）实现**轻量、毫秒级、零噪音**的本地开发端口
//! 枚举与一键释放。
//!
//! # 模块定位（与常驻守护模块的差异）
//!
//! 本模块为**即开即用工具**（无常驻后台开关）：卡片以「即开即用」态呈现（不渲染
//! 物理开关，齿轮即入口），`start` / `stop` 为幂等空操作、`is_running` 恒为
//! `false`——它不派生任何原生线程 / 钩子，资源占用仅在弹窗打开扫描时发生。
//!
//! # 组成
//!
//! - [`scanner`]：Win32 枚举（`GetExtendedTcpTable` / `GetExtendedUdpTable`）+
//!   四重降噪过滤纯函数（状态 / 动态端口 / 会话隔离 / 系统黑名单）+ 进程元数据
//!   提取（`OpenProcess` + `QueryFullProcessImageNameW` + `ProcessIdToSessionId`）；
//! - [`killer`]：进程安全终止（`OpenProcess(PROCESS_TERMINATE)` +
//!   `TerminateProcess`）、[`PortError`] 错误模型与 UIPI 防御（`ERROR_ACCESS_DENIED`
//!   → Toast「需要管理员权限，请通过顶部盾牌提权运行」）、成功 Toast；
//! - [`logger`]：模块专属明细日志（`{log_dir}/port_hunter.log`，流式追加）——
//!   与全局审计日志（`app_audit.log`）构成「全局审计 + 模块明细」双轨日志体系。
//!
//! # 并发模型
//!
//! - 扫描（同步 Win32 调用）由装配层经 `spawn_blocking` 移出 UI 线程，结果写入
//!   内部 `Mutex<Vec<PortEntry>>` 缓存（UI 渲染期即时搜索复用，见
//!   [`scanner::filter_port_rows`]）；
//! - 终止动作同样经 `spawn_blocking` 执行；Toast 经按钮事件总线
//!   （`AppEvent::ToastRequested`）由总线 → UI 桥接层展示（与窗口置顶模块同路径）。

pub mod killer;
pub mod logger;
pub mod scanner;

pub use killer::PortError;
pub use scanner::PortEntry;

use crate::bus::EventBus;
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use tokio::sync::Mutex as AsyncMutex;

/// 一次扫描的可观测量（供 UI 状态条与模块日志消费）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOutcome {
    /// TCP 原始 LISTEN 条目数。
    pub tcp_listeners: usize,
    /// UDP 原始绑定条目数。
    pub udp_listeners: usize,
    /// 扫描完成后缓存的总条目数（会话隔离后；展示期过滤前的全量）。
    pub cached: usize,
}

/// 一次终止动作的结果报告（模块层统一生产，装配层消费写审计 / 弹反馈）。
#[derive(Debug, Clone)]
pub struct KillReport {
    /// 终止进程名（扫描缓存回读；缓存无该 PID 时为 `<unknown>`）。
    pub process_name: String,
    /// 底层动作耗时（毫秒，仅包络 Win32 调用）。
    pub elapsed_ms: u128,
    /// 结果：`Ok(())` 成功；`Err(error)` 失败（含 UIPI 判定所需的错误码）。
    pub outcome: Result<(), PortError>,
}

impl KillReport {
    /// 是否成功释放。
    pub fn is_success(&self) -> bool {
        self.outcome.is_ok()
    }
}

#[derive(Clone)]
pub struct PortHunterModule {
    inner: Arc<PortHunterInner>,
}

/// 模块内部状态。
struct PortHunterInner {
    /// 串行化生命周期变迁（本模块为空操作，保留契约形状）。
    lifecycle: AsyncMutex<()>,
    /// 运行标志（本模块恒为 `false`——即开即用工具无常驻后台态）。
    running: AtomicBool,
    /// 最近一次扫描的展示缓存（状态 + 会话降噪后；渲染期再经阶段 2/4 + 搜索过滤）。
    cached_rows: StdMutex<Vec<PortEntry>>,
    /// 模块专属明细日志器（`{log_dir}/port_hunter.log`）。
    logger: logger::PortHunterLogger,
    /// 可选事件总线（终止后的 Toast 出口；装配期绑定后不变）。
    bus: StdMutex<Option<EventBus>>,
}

impl PortHunterModule {
    /// 以日志目录构造模块（目录在日志器内按需自动创建）。
    ///
    /// `log_dir` 应为装配层解析好的**有效目录**（
    /// [`AppConfig::effective_port_hunter_log_dir`](crate::config::AppConfig::effective_port_hunter_log_dir)）。
    pub fn new(log_dir: std::path::PathBuf) -> Self {
        Self {
            inner: Arc::new(PortHunterInner {
                lifecycle: AsyncMutex::new(()),
                running: AtomicBool::new(false),
                cached_rows: StdMutex::new(Vec::new()),
                logger: logger::PortHunterLogger::new(log_dir),
                bus: StdMutex::new(None),
            }),
        }
    }

    /// 绑定事件总线（终止成功 / UIPI 拦截的 Toast 出口，装配期调用一次）。
    pub fn attach_bus(&self, bus: Option<EventBus>) {
        if let Some(bus) = bus {
            self.inner
                .bus
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .replace(bus);
        }
    }

    /// 当前事件总线句柄（引用，短临界读取）。
    fn bus_ref(&self) -> Option<EventBus> {
        self.inner
            .bus
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// 执行一次同步扫描：Win32 枚举 + 会话隔离降噪，结果写入缓存并返回观测值。
    ///
    /// `show_system_ports` 为本次扫描携带的显示选项（仅用于模块日志记录；
    /// 实际渲染期过滤由装配层以当前配置调用 [`scanner::filter_port_rows`]）。
    ///
    /// # 线程模型
    /// 同步 Win32 调用（毫秒级）；UI 回调须经
    /// [`tokio::task::spawn_blocking`](tokio::task::spawn_blocking) 使用本方法，
    /// 避免阻塞事件循环与 Tokio 工作线程。
    pub fn scan(&self, show_system_ports: bool) -> Result<ScanOutcome, PortError> {
        // v0.5.1 重构：show_system_ports 直接贯穿到原生表项遍历循环——物理阻断
        //（PID ≤ 4 / 系统保留端口 / 多 IP 重复行）在 scanner 循环内完成，缓存从
        // 诞生起就是「干净」形态，绝不把过滤推给 UI 层。
        let report = scanner::scan_and_collect(show_system_ports)?;
        let cached = report.entries.clone();
        {
            let mut guard = self
                .inner
                .cached_rows
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            *guard = cached;
        }
        self.inner.logger.log_scan(
            report.tcp_listeners,
            report.udp_listeners,
            report.entries.len(),
            show_system_ports,
        );
        Ok(ScanOutcome {
            tcp_listeners: report.tcp_listeners,
            udp_listeners: report.udp_listeners,
            cached: report.entries.len(),
        })
    }

    /// 当前展示缓存快照（状态 + 会话降噪；渲染期过滤由
    /// [`scanner::filter_port_rows`] 在 UI 线程完成）。
    pub fn cached_rows(&self) -> Vec<PortEntry> {
        self.inner
            .cached_rows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// 从展示缓存回读 PID 对应的进程名（终止反馈回显用；未命中 → `<unknown>`）。
    pub fn process_name_of(&self, pid: u32) -> String {
        self.cached_rows()
            .iter()
            .find(|entry| entry.pid == pid)
            .map(|entry| entry.process_name.clone())
            .unwrap_or_else(|| scanner::UNKNOWN_PROCESS.to_string())
    }

    /// 记录一次用户搜索关键词到模块日志（含显式端口豁免提示）。
    pub fn log_search(&self, keyword: &str) {
        let explicit = scanner::parse_explicit_port(keyword);
        self.inner.logger.log_search(keyword, explicit);
    }

    /// 执行一次「释放端口（终止进程）」：低层 Win32 动作 + 事件总线 Toast +
    /// 模块明细日志，返回 [`KillReport`] 供装配层写审计。
    ///
    /// # 线程模型
    /// 同步 Win32 调用；UI 回调须经 `spawn_blocking` 使用（与 [`Self::scan`] 一致）。
    pub fn kill(&self, pid: u32, port: u16, protocol: &str) -> KillReport {
        let process_name = self.process_name_of(pid);
        let started = std::time::Instant::now();
        let outcome = killer::kill_process_with_events(
            self.bus_ref().as_ref(),
            pid,
            port,
            protocol,
            &process_name,
        );
        let elapsed_ms = started.elapsed().as_millis();

        match &outcome {
            Ok(()) => {
                self.inner
                    .logger
                    .log_kill_success(port, protocol, &process_name, pid, elapsed_ms)
            }
            Err(error) => self.inner.logger.log_kill_failure(
                port,
                protocol,
                &process_name,
                pid,
                &error.to_string(),
            ),
        }
        KillReport {
            process_name,
            elapsed_ms,
            outcome,
        }
    }
}

impl Default for PortHunterModule {
    fn default() -> Self {
        Self::new(std::path::PathBuf::from("logs/port_hunter"))
    }
}

#[async_trait]
impl super::ToolModule for PortHunterModule {
    fn id(&self) -> &'static str {
        "port_hunter"
    }

    fn display_name(&self) -> &'static str {
        "端口占用管理"
    }

    fn description(&self) -> &'static str {
        "本地开发端口监听定位与一键释放"
    }

    /// 即开即用工具：启动为空操作（不派生任何后台资源）。
    async fn start(&self) -> Result<(), super::ModuleError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        if self.inner.running.load(Ordering::Acquire) {
            return Ok(());
        }
        // 本模块无后台运行态：不置 running（保持 false），仅记录启动意图。
        tracing::debug!(target: "port_hunter", "端口猎手为即开即用工具，start 为空操作");
        Ok(())
    }

    /// 即开即用工具：停止为空操作（无资源可释放）。
    async fn stop(&self) -> Result<(), super::ModuleError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        tracing::debug!(target: "port_hunter", "端口猎手无后台资源，stop 为空操作");
        Ok(())
    }

    /// 恒为 `false`：即开即用工具没有「常驻运行」语义（UI 以「即开即用」标签呈现）。
    fn is_running(&self) -> bool {
        self.inner.running.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::ToolModule;

    /// 元数据契约：ID / 展示名 / 描述与装配层与 UI 的约定一致。
    #[test]
    fn metadata_identity_matches_registry_contract() {
        let module = PortHunterModule::default();
        assert_eq!(module.id(), "port_hunter");
        assert_eq!(module.display_name(), "端口占用管理");
        assert_eq!(module.description(), "本地开发端口监听定位与一键释放");
        assert!(!module.is_running(), "即开即用工具不应有运行态");
    }

    /// 生命周期空操作：start / stop 幂等成功且不产生运行态（不派生后台资源）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_noop_is_idempotent_and_never_running() {
        let module = PortHunterModule::default();
        for _ in 0..3 {
            module.start().await.expect("start 应幂等成功");
            assert!(!module.is_running(), "即开即用工具不得进入运行态");
            module.start().await.expect("重复 start 应幂等");
            module.stop().await.expect("stop 应幂等成功");
            module.stop().await.expect("重复 stop 应幂等");
        }
    }

    /// 日志器实例唯一性（模块构造即建立日志目标目录）由 logger 单测覆盖；
    /// 此处验证缓存默认空、未知 PID 回退 `<unknown>`。
    #[test]
    fn cache_starts_empty_and_unknown_pid_falls_back() {
        let module = PortHunterModule::default();
        assert!(module.cached_rows().is_empty(), "新模块缓存应为空");
        assert_eq!(module.process_name_of(999_999), scanner::UNKNOWN_PROCESS);
    }

    /// 展示缓存写入 / 回读契约：注入行后 process_name_of 精确回读，未命中回退。
    #[test]
    fn cached_rows_roundtrip_and_pid_lookup() {
        let module = PortHunterModule::default();
        {
            let mut guard = module
                .inner
                .cached_rows
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            *guard = vec![scanner::PortEntry {
                protocol: "TCP".to_string(),
                local_port: 8080,
                local_addr: "127.0.0.1".to_string(),
                pid: 4242,
                process_name: "node.exe".to_string(),
                process_path: "C:\\dev\\node.exe".to_string(),
            }];
        }
        assert_eq!(module.process_name_of(4242), "node.exe");
        assert_eq!(module.process_name_of(1), scanner::UNKNOWN_PROCESS);
    }

    /// KILL 报告结构：未命中 PID 的终止必然失败，且报告携带回退进程名与耗时。
    #[test]
    fn kill_report_never_panics_and_carries_context() {
        let module = PortHunterModule::default();
        let report = module.kill(u32::MAX, 8080, "TCP");
        assert!(!report.is_success(), "不存在的 PID 必然失败");
        assert_eq!(report.process_name, scanner::UNKNOWN_PROCESS);
        #[cfg(windows)]
        assert!(
            report.outcome.as_ref().unwrap_err().code().is_some(),
            "Windows 下应有错误码"
        );
        #[cfg(not(windows))]
        assert_eq!(
            *report.outcome.as_ref().unwrap_err(),
            PortError::UnsupportedPlatform,
            "非 Windows 平台应为 UnsupportedPlatform"
        );
        // 耗时必然 >= 0（u128），此处仅验证结构字段存在。
        let _ = report.elapsed_ms;
    }
}
