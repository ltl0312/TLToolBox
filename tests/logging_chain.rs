//! # 双轨日志链路探测（任务 2 验收 · 实测样本产区）
//!
//! 以库目标 `tltoolbox` 的公开 API 端到端验证「全局追踪 + 模块明细」双轨链路：
//!
//! 1. **全局追踪轨**：`logging::init_in_dir` 装配按天滚动文件 → `tracing::info!`
//!    落盘 `<dir>/tltoolbox.<UTC 日期>.log`（`WorkerGuard` Drop 刷盘）；
//! 2. **全局审计轨**：`AuditSink` 高精度时间戳记录落盘 `<dir>/app_audit.log`；
//! 3. **模块明细轨**：`PortHunterLogger`（端口猎手专属日志）落盘
//!    `<dir>/port_hunter.log`，且构造时自动创建目标目录（effective_log_dir 语义）。
//!
//! 每条轨的**真实落盘行**经 `println!` 输出（`cargo test -- --nocapture` 可见），
//! 供“各模块日志落盘的实测样本”验收直接引用。目录创建语义的完整覆盖见
//! terminal_logger 的 `ensure_log_dirs` 单元测试以及 logging / logger 的既有单测，
//! 本文件只做链路级冒烟探测，避免与库内单测重复。

use std::path::{Path, PathBuf};
use std::time::Duration;
use tltoolbox::logging::{self, AuditSink};
use tltoolbox::modules::port_hunter::logger::PortHunterLogger;

/// 本次测试独有的临时目录（并发 / 并行进程互不干扰）。
fn unique_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("系统时钟应晚于 UNIX 纪元")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "tltoolbox-log-chain-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

/// 期望的当日滚动文件名（与 tracing-appender 的 UTC 零点轮转约定一致）。
fn expected_today_file(dir: &Path) -> PathBuf {
    let today = chrono::Utc::now().format("%Y-%m-%d");
    dir.join(format!("tltoolbox.{today}.log"))
}

/// 轮询等待文件出现目标子串（异步落盘任务需有限等待窗口）。
fn wait_for_content(file: &Path, needle: &str, timeout: Duration) -> String {
    let deadline = std::time::Instant::now() + timeout;
    let mut last = String::new();
    while std::time::Instant::now() < deadline {
        if let Ok(content) = std::fs::read_to_string(file) {
            if content.contains(needle) {
                return content;
            }
            last = content;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "文件应在 {timeout:?} 内出现 {needle:?}，实际内容: {last:?}（文件: {}）",
        file.display()
    );
}

/// 双轨日志链路端到端探测：追踪 / 审计 / 端口猎手明细三轨各落一行并打印样本。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dual_track_logging_chain_probe() {
    let dir = unique_dir("probe");
    assert!(!dir.exists(), "前置条件：探测目录尚不存在");

    // ---- 轨 1：tracing 按天滚动文件（全局装配只允许本测试进程一次） ----
    let guard = logging::init_in_dir(&dir);
    assert!(guard.is_file_logging_active(), "探测目录下文件日志应可用");
    tracing::info!(
        target: "probe",
        module = "tracing",
        "双轨日志链路探测：tracing 按天滚动文件轨正常"
    );
    drop(guard); // WorkerGuard 刷盘
    let trace_content = std::fs::read_to_string(expected_today_file(&dir))
        .expect("tracing 当日滚动文件应已生成且可读");
    let trace_line = trace_content
        .lines()
        .find(|line| line.contains("双轨日志链路探测"))
        .expect("tracing 日志行应完整落盘");
    println!("[样本·轨1] tracing 按天滚动文件: {trace_line}");

    // ---- 轨 2：全局审计日志（AuditSink，高精度 UTC 微秒时间戳） ----
    let audit_sink = AuditSink::new(dir.clone());
    audit_sink.record("应用启动", "双轨日志链路探测（审计轨）", "成功");
    drop(audit_sink); // 关闭发送端，落盘任务排空后退出
    let audit_content = wait_for_content(
        &dir.join("app_audit.log"),
        "双轨日志链路探测（审计轨）",
        Duration::from_secs(3),
    );
    let audit_line = audit_content
        .lines()
        .find(|line| line.contains("双轨日志链路探测（审计轨）"))
        .expect("审计行应完整落盘");
    println!("[样本·轨2] 全局审计日志: {audit_line}");

    // ---- 轨 3：端口猎手模块明细日志（构造即自动创建目标目录） ----
    let ph_dir = dir.join("nested").join("port_hunter");
    assert!(!ph_dir.exists(), "前置条件：模块日志目录尚不存在");
    let ph_logger = PortHunterLogger::new(&ph_dir);
    assert!(ph_dir.is_dir(), "PortHunterLogger 构造应自动创建目标目录");
    ph_logger.log_scan(12, 3, 9, false);
    ph_logger.log_kill_success(5173, "TCP", "node.exe", 12345, 2);
    let ph_content =
        std::fs::read_to_string(ph_logger.file_path()).expect("端口猎手模块日志应已生成且可读");
    for line in ph_content.lines() {
        println!("[样本·轨3] 端口猎手模块明细日志: {line}");
    }

    println!("[样例汇总] 生效日志根目录: {}", dir.display());
    println!(
        "            追踪文件: {}",
        expected_today_file(&dir).display()
    );
    println!(
        "            审计文件: {}",
        dir.join("app_audit.log").display()
    );
    println!(
        "            模块明细文件: {}",
        ph_logger.file_path().display()
    );

    // 留档供报告引用：把样本复制到工作区 logs/ 下（幂等，仅追加汇总）。
    let _ = std::fs::create_dir_all("logs");
    let _ = std::fs::write("logs/log-chain-probe-summary.txt", {
        format!(
            "[样本·轨1] {trace_line}\n[样本·轨2] {audit_line}\n[样本·轨3] {ph_summary}\n",
            ph_summary = ph_content.lines().collect::<Vec<_>>().join("\n[样本·轨3] ")
        )
    });

    let _ = std::fs::remove_dir_all(&dir);
}
