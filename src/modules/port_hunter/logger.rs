//! # 端口猎手 · 模块专属明细日志（v0.5.0）
//!
//! 与全局审计日志（`app_audit.log`，见 [`crate::logging::AuditSink`]）互补的**模块
//! 专用细粒度事件流**：全局审计只记录「释放端口」这一关键危险操作；本日志则以
//! 更高分辨率记录端口猎手的各类内部事件——扫描时刻与监听端口计数、用户搜索
//! 关键词、终止进程耗时、详细的 Win32 错误码等，便于开发排查端口冲突全链路。
//!
//! # 写入机制（流式追加）
//!
//! - 目标文件：`<有效日志目录>/port_hunter.log`（目录按 `[port_hunter].log_dir`
//!   或默认 exe 同级 `logs/port_hunter` 解析，见
//!   [`AppConfig::effective_port_hunter_log_dir`](crate::config::AppConfig::effective_port_hunter_log_dir)）；
//! - **写入前自动创建目标目录**：日志器构造时 `create_dir_all`，失败仅降级告警
//!   （不 panic——日志不可用绝不阻断端口猎手主功能）；
//! - 每次调用以 `OpenOptions::create(true).append(true)` **流式追加**一行，单次
//!   写入原子性由文件追加语义保证，多线程由内部 `Mutex` 串行化；
//! - 时间戳为 **UTC 微秒 ISO8601**（与全局审计日志同一精度约定）+ 事件类别标签，
//!   格式：`[<UTC 时间戳>] [<事件类别>] <明细> <-> <结果>`。
//!
//! # 事件类别
//!
//! - `SCAN`：一次监听端口枚举完成（记录按协议拆分计数与展示过滤选项）；
//! - `SEARCH`：用户在弹窗搜索框输入的关键词快照（含解析出的显式端口号）；
//! - `KILL`：一次释放端口（终止进程）的成败与耗时（含 Win32 错误码明细）。
//!
//! # 优雅降级
//!
//! 目录创建或写入失败一律经 `tracing::warn` 告警后**继续**——端口猎手本体
//! （枚举 / 过滤 / 终止）的运行不依赖日志的成败。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 模块专属日志文件名（位于 [`PORT_HUNTER_LOG_DIR`] 解析出的有效目录下）。
pub const PORT_HUNTER_LOG_FILE: &str = "port_hunter.log";

/// 日志行时间戳格式（UTC 微秒，与全局审计日志同一形态）。
const LINE_TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.6fZ";

/// 端口猎手模块专属日志器：目标文件流式追加写入。
///
/// 内部以 `Mutex` 串行化并发写入（单条 append 为原子追加），`Clone` 为浅拷贝
/// 共享同一文件路径与锁，可跨线程安全使用。
#[derive(Clone)]
pub struct PortHunterLogger {
    /// 绝对目标文件路径（构造时由日志目录解析）。
    file_path: PathBuf,
    /// 串行化追加写入的互斥锁（跨克隆句柄共享）。
    lock: Arc<Mutex<()>>,
}

use std::sync::Arc;

impl PortHunterLogger {
    /// 以指定日志目录构造日志器（**自动创建目标目录**；失败仅降级告警）。
    ///
    /// `dir` 应为消费方（装配层）解析好的**有效目录**（绝对路径）；写入目标为
    /// `dir/port_hunter.log`。目录创建失败不 panic——后续写入尝试失败时以
    /// `tracing::warn` 单次上报。
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        if let Err(err) = std::fs::create_dir_all(&dir) {
            tracing::warn!(
                target: "port_hunter",
                "端口猎手日志目录创建失败（本次运行模块日志不落盘）: '{}' -> {err}",
                dir.display()
            );
        }
        Self {
            file_path: dir.join(PORT_HUNTER_LOG_FILE),
            lock: Arc::new(Mutex::new(())),
        }
    }

    /// 当前日志文件绝对路径（供 UI 展示与测试定位）。
    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    /// 追加一条带 UTC 微秒时间戳 + 事件类别标签的日志行（格式见模块文档）。
    ///
    /// `category` 为事件类别（`SCAN` / `SEARCH` / `KILL`），`detail` 为明细，
    /// `result` 为结果（如 `成功` / `失败: Win32 错误码 5`）。任意线程可调用；
    /// 写入失败（目录被删 / 磁盘错误）仅告警，绝不 panic。
    pub fn log_line(&self, category: &str, detail: &str, result: &str) {
        let timestamp = chrono::Utc::now().format(LINE_TIMESTAMP_FORMAT);
        let line = format!("[{timestamp}] [{category}] {detail} -> {result}\n");
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let write_result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file_path)
            .and_then(|mut file| file.write_all(line.as_bytes()));
        if let Err(err) = write_result {
            tracing::warn!(
                target: "port_hunter",
                "模块日志写入失败: '{}' -> {err}",
                self.file_path.display()
            );
        }
    }

    /// SCAN 事件：一次监听端口枚举的完成明细。
    ///
    /// - `tcp_listeners` / `udp_listeners`：两协议下系统原始枚举出的 LISTEN 条目数；
    /// - `kept`：经「会话隔离」等扫描期降噪后保留并缓存的条目数；
    /// - `show_system_ports`：本次扫描携带的显示选项（决定后续渲染期两阶段过滤）。
    pub fn log_scan(&self, tcp_listeners: usize, udp_listeners: usize, kept: usize, show_system_ports: bool) {
        self.log_line(
            "SCAN",
            &format!(
                "扫描监听端口: TCP {tcp_listeners} 条, UDP {udp_listeners} 条, 会话隔离后缓存 {kept} 条, show_system_ports={show_system_ports}"
            ),
            "完成",
        );
    }

    /// SEARCH 事件：用户在弹窗搜索框输入的关键词快照（含解析出的显式端口号）。
    pub fn log_search(&self, keyword: &str, explicit_port: Option<u16>) {
        let port_hint = match explicit_port {
            Some(port) => format!("（解析出显式端口号 {port}，动态端口过滤已豁免）"),
            None => String::new(),
        };
        self.log_line("SEARCH", &format!("搜索关键词 \"{keyword}\"{port_hint}"), "已应用");
    }

    /// KILL 事件：一次成功释放端口（终止进程）的耗时明细。
    pub fn log_kill_success(
        &self,
        port: u16,
        protocol: &str,
        process_name: &str,
        pid: u32,
        elapsed_ms: u128,
    ) {
        self.log_line(
            "KILL",
            &format!("释放端口 {port} ({protocol}), 终止进程 {process_name} (PID: {pid})"),
            &format!("成功（耗时 {elapsed_ms}ms）"),
        );
    }

    /// KILL 事件：一次失败释放端口（终止进程）的明细（含 Win32 错误码）。
    pub fn log_kill_failure(
        &self,
        port: u16,
        protocol: &str,
        process_name: &str,
        pid: u32,
        detail: &str,
    ) {
        self.log_line(
            "KILL",
            &format!("释放端口 {port} ({protocol}), 终止进程 {process_name} (PID: {pid})"),
            &format!("失败: {detail}"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生成本次测试独有的临时目录（并发测试互不干扰）。
    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("系统时钟应晚于 UNIX 纪元")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "tltoolbox-port-hunter-logger-{tag}-{}-{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// 日志器创建后自动建目录、文件写入目标正确（`{dir}/port_hunter.log`）。
    #[test]
    fn logger_creates_missing_directory_and_targets_correct_file() {
        let base = temp_dir("mkdir");
        let dir = base.join("nested").join("logs");
        assert!(!dir.exists(), "前置条件：目录尚不存在");

        let logger = PortHunterLogger::new(&dir);
        assert!(dir.is_dir(), "构造后目标目录应被自动创建");
        assert_eq!(
            logger.file_path(),
            dir.join(PORT_HUNTER_LOG_FILE),
            "日志文件应落在 {}/port_hunter.log",
            dir.display()
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 三类事件（SCAN / SEARCH / KILL 成功 / KILL 失败）逐行追加落盘，内容含
    /// UTC 微秒时间戳、事件类别与明细 / 结果；文件以追加模式累积（两轮写入都在）。
    #[test]
    fn typed_events_append_lines_with_timestamps_and_details() {
        let dir = temp_dir("append");
        let logger = PortHunterLogger::new(&dir);

        logger.log_scan(8, 3, 9, false);
        logger.log_search("5173", Some(5173));
        logger.log_kill_success(5173, "TCP", "node.exe", 12345, 2);
        logger.log_kill_failure(8080, "TCP", "java.exe", 54321, "Win32 错误码 5 (ERROR_ACCESS_DENIED)");

        let content = std::fs::read_to_string(logger.file_path()).expect("日志文件应可读");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 4, "四次事件应产生四行日志");
        assert!(
            lines[0].contains("[SCAN]") && lines[0].contains("TCP 8 条, UDP 3 条"),
            "SCAN 行应含协议计数，实际: {}",
            lines[0]
        );
        assert!(
            lines[1].contains("[SEARCH]") && lines[1].contains("5173") && lines[1].contains("已应用"),
            "SEARCH 行应含关键词与显式端口提示，实际: {}",
            lines[1]
        );
        assert!(
            lines[2].contains("[KILL]")
                && lines[2].contains("释放端口 5173 (TCP)")
                && lines[2].contains("node.exe (PID: 12345)")
                && lines[2].contains("成功（耗时 2ms）"),
            "KILL 成功行应含端口/进程/耗时，实际: {}",
            lines[2]
        );
        assert!(
            lines[3].contains("[KILL]")
                && lines[3].contains("失败: Win32 错误码 5 (ERROR_ACCESS_DENIED)"),
            "KILL 失败行应含详细错误码，实际: {}",
            lines[3]
        );

        // 每行行首都应携带 UTC 微秒时间戳（RFC3339 形态）。
        for line in &lines {
            let timestamp = line.split(']').next().expect("行首应为时间戳").trim_start_matches('[');
            let parsed = chrono::DateTime::parse_from_rfc3339(timestamp);
            assert!(parsed.is_ok(), "时间戳应可被 RFC3339 解析，实际: {timestamp:?}");
        }

        // 追加语义：再次写入后原内容保留、新行追加在后。
        logger.log_search("node", None);
        let content = std::fs::read_to_string(logger.file_path()).expect("日志文件应可读");
        assert_eq!(content.lines().count(), 5, "追加写入应保留既有行");
        assert!(content.lines().last().unwrap().contains("[SEARCH]"));
        assert!(!content.lines().last().unwrap().contains("5173"), "无显式端口时不应带豁免提示");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 并发写入串行化：多线程同时记录事件，行数不丢、行序不串（追加原子性）。
    #[test]
    fn concurrent_appends_are_serialized_without_line_loss() {
        let dir = temp_dir("concurrent");
        let logger = PortHunterLogger::new(&dir);

        let mut handles = Vec::new();
        for i in 0..8 {
            let logger = logger.clone();
            handles.push(std::thread::spawn(move || {
                for j in 0..25 {
                    logger.log_search(&format!("t{i}-{j}"), None);
                }
            }));
        }
        for handle in handles {
            handle.join().expect("写入线程应正常结束");
        }

        let content = std::fs::read_to_string(logger.file_path()).expect("日志文件应可读");
        assert_eq!(content.lines().count(), 8 * 25, "并发写入不得丢行");
        for i in 0..8 {
            let mut seen = 0usize;
            for line in content.lines() {
                if line.contains(&format!("t{i}-")) {
                    seen += 1;
                }
            }
            assert_eq!(seen, 25, "线程 {i} 的 25 行应全部落盘");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}