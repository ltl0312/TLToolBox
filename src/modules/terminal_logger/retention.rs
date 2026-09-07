//! # 终端日志过期清理守护（v0.3.1 加固）
//!
//! 终端交互日志子系统把每次会话的日志落盘到 `logs/terminals/`（及显式配置的
//! 自定义目录）下的 `powershell/`、`bash/`、`cmd/` 等子目录，文件名形如
//! `yyyy-MM-dd_HH-mm-ss_pid<PID>.log`、`<ts>_pid<PID>.state.log` 与
//! `cmd_HHMMSS_随机.log`。若不做任何回收，日志碎文件会随会话次数无限积压，
//! 长期运行后占据大量磁盘空间。
//!
//! 本模块提供**过期清理守护**的核心逻辑：
//!
//! - [`is_managed_log_file`]：受管文件判定——以 `.log` 结尾即视为会话日志
//!   （`.state.log` 亦以 `.log` 结尾，天然覆盖两类文件）；
//! - [`retention_cutoff`]：由保留天数与当前时刻计算过期截止点（负数溢出夹取到
//!   `UNIX_EPOCH`，杜绝 `SystemTime` 下溢 panic）；
//! - [`cleanup_expired_logs_sync`]：**递归**扫描日志根目录，删除修改时间早于
//!   截止点的 `.log` / `.state.log` 文件——日志按子目录组织，递归保证全部子目录
//!   （`powershell/`、`bash/`、`cmd/`、未来新增目录）都被覆盖；
//! - [`cleanup_expired_logs`]：异步包装（`spawn_blocking`），供模块启动 / 定时
//!   任务在不阻塞 Tokio 工作线程的前提下调用。
//!
//! ## 扫描语义（保守、容错、无权限擦除风险）
//!
//! - 只删除**普通文件**；目录、符号链接及其它条目一律不动；
//! - 只删除文件名以 `.log` 结尾的文件——非受管文件（`notes.txt`、`config.toml`
//!   等）即使过期也绝不触碰；
//! - 目录不存在 / 不可读：返回零报告（模块首次启动时目录往往尚未创建）；
//! - 单条目失败（元数据不可读、删除被拒等）只计数 `skipped` 并继续，绝不中断
//!   整轮扫描；
//! - 删除基于 `metadata.modified()`（文件系统 mtime），不基于文件名时间戳——
//!   即使文件名被外部工具改写过，过期判定依旧准确。

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// 清理守护的定时触发周期：**每日一次**（与日志按天滚动的节奏一致；启动时
/// 先立即执行一轮，此后每 `CLEANUP_INTERVAL` 触发一轮）。
pub const CLEANUP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// 单轮清理的结果报告（供调用方日志 / 诊断展示）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupReport {
    /// 扫描到的受管日志文件总数（`.log` / `.state.log`）。
    pub scanned: usize,
    /// 实际删除的过期文件数。
    pub removed: usize,
    /// 遍历中遇到但无法处理的条目数（元数据不可读、删除被拒、目录不可枚举等）。
    pub skipped: usize,
}

/// 是否为本子系统管理的日志文件。
///
/// 判定规则：文件名以 `.log` 结尾。PowerShell 转录文件
/// `<ts>_pid<PID>.log` 与状态流 `<ts>_pid<PID>.state.log`、bash 会话日志
/// `<ts>_<pid>.log`、cmd 会话日志 `cmd_HHMMSS_随机.log` 均以此结尾；
/// `.state.log` 的 `.log` 后缀天然被覆盖，无需单独分支。
pub fn is_managed_log_file(name: &str) -> bool {
    name.ends_with(".log")
}

/// 由保留天数与参考时刻计算过期截止点：`now - retention_days 天`。
///
/// `retention_days` 为 0 时截止点为 `now` 本身（修改时间早于当前时刻的文件
/// 即过期）；天数极大导致下溢时夹取到 `UNIX_EPOCH`——`SystemTime` 无负值，
/// 不夹取将在此处 panic。
pub fn retention_cutoff(retention_days: u32, now: SystemTime) -> SystemTime {
    let span = std::time::Duration::from_secs(u64::from(retention_days) * 86_400);
    now.checked_sub(span).unwrap_or(std::time::UNIX_EPOCH)
}

/// 递归扫描 `dir` 下全部普通文件，删除修改时间早于截止点的受管日志文件。
///
/// 同步实现（`std::fs`），供单元测试与 [`cleanup_expired_logs`] 的阻塞池包装
/// 直接调用；目录不存在 / 不可读时返回零报告。
pub fn cleanup_expired_logs_sync(dir: &Path, retention_days: u32) -> CleanupReport {
    let cutoff = retention_cutoff(retention_days, SystemTime::now());
    let mut report = CleanupReport::default();
    scan_dir(dir, &cutoff, &mut report);
    report
}

/// 递归遍历单个目录（含子目录）的清理主体。
fn scan_dir(dir: &Path, cutoff: &SystemTime, report: &mut CleanupReport) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // 目录不存在（模块首次启动前）或不可枚举：无可扫描条目，静默返回。
        Err(_) => return,
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                report.skipped += 1;
                continue;
            }
        };

        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => {
                report.skipped += 1;
                continue;
            }
        };

        if file_type.is_dir() {
            // 会话日志按 powershell/ bash/ cmd/ 子目录组织：递归覆盖全部子目录。
            scan_dir(&entry.path(), cutoff, report);
        } else if file_type.is_file() && is_managed_log_file(&entry.file_name().to_string_lossy()) {
            report.scanned += 1;
            match entry.metadata().and_then(|meta| meta.modified()) {
                Ok(modified) if modified < *cutoff => {
                    // 修改时间早于截止点 = 已超出保留期限：删除。
                    match std::fs::remove_file(entry.path()) {
                        Ok(()) => report.removed += 1,
                        // 删除被拒（只读属性 / 被占用等）：计数跳过，不中断扫描。
                        Err(_) => report.skipped += 1,
                    }
                }
                Ok(_) => {}   // 未过期：保留
                Err(_) => report.skipped += 1, // mtime 不可读：保守跳过
            }
        }
        // 符号链接及其它条目类型：一律不处理（杜绝经链接误删外部文件）。
    }
}

/// 异步清理入口：经阻塞池执行同步扫描，不阻塞 Tokio 工作线程。
///
/// `dir` 按值传入（`spawn_blocking` 要求 `'static`）；返回单轮清理报告。
/// 阻塞任务异常终止（理论不可达）时返回零报告并告警。
pub async fn cleanup_expired_logs(dir: PathBuf, retention_days: u32) -> CleanupReport {
    match tokio::task::spawn_blocking(move || cleanup_expired_logs_sync(&dir, retention_days)).await
    {
        Ok(report) => report,
        Err(err) => {
            tracing::warn!(
                target: "terminal_logger",
                "日志过期清理阻塞任务异常终止: {err}"
            );
            CleanupReport::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 系统临时目录下本次测试独有的目录路径（并行测试互不干扰）。
    fn unique_temp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("系统时钟应晚于 UNIX 纪元")
            .as_nanos();
        std::env::temp_dir().join(format!("tltoolbox-ret-{tag}-{}-{nanos}", std::process::id()))
    }

    /// 把文件的修改时间改写为指定时刻（`File::set_modified`，模拟旧日志的 mtime）。
    ///
    /// Windows 上改写文件时间需要 `FILE_WRITE_ATTRIBUTES` 访问权：`File::open`
    /// 只授予读访问会被拒绝（AccessDenied），故以写权限打开。
    fn set_modified(path: &Path, time: SystemTime) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("以写权限打开文件以改写 mtime");
        file.set_modified(time)
            .expect("改写 mtime 应成功（测试专属临时文件）");
    }

    // ---- 受管文件判定 ----

    #[test]
    fn managed_log_file_filter_covers_log_and_state_log_only() {
        // 两类受管会话日志：.log 与 .state.log。
        assert!(is_managed_log_file("2026-01-01_10-00-00_pid123.log"));
        assert!(is_managed_log_file("2026-01-01_10-00-00_pid123.state.log"));
        assert!(is_managed_log_file("cmd_101523_42.log"));
        // 非受管文件：即使形似日志也不得误删。
        assert!(!is_managed_log_file("config.toml"));
        assert!(!is_managed_log_file("notes.txt"));
        assert!(!is_managed_log_file(".log.tmp"), "非 .log 结尾不得视为受管");
        assert!(!is_managed_log_file("log"));
        assert!(!is_managed_log_file(""));
    }

    // ---- 截止点计算 ----

    #[test]
    fn cutoff_subtracts_days_and_saturates_on_underflow() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let cutoff = retention_cutoff(14, now);
        assert_eq!(
            cutoff,
            now - Duration::from_secs(14 * 86_400),
            "14 天保留期限应精确前移 14 天"
        );
        // 0 天 = 截止点即当前时刻。
        assert_eq!(retention_cutoff(0, now), now);
        // 极大天数（u32 上限）导致下溢时夹取 UNIX_EPOCH，绝不 panic。
        assert_eq!(retention_cutoff(u32::MAX, now), SystemTime::UNIX_EPOCH);
    }

    // ---- 递归扫描与过期删除 ----

    /// 构造一棵模拟日志树（含三类 Shell 子目录 + 嵌套子目录 + 非受管文件），
    /// 旧文件 mtime 改写为 30 天前、新文件保持当前时刻。
    fn build_log_tree(root: &Path) -> Vec<PathBuf> {
        let old = SystemTime::now() - Duration::from_secs(30 * 86_400);
        let files = [
            "powershell/2026-01-01_10-00-00_pid123.log",
            "powershell/2026-01-01_10-00-00_pid123.state.log",
            "powershell/2026-06-01_09-00-00_pid456.log",
            "bash/2026-01-01_08-00-00_42.log",
            "bash/notes.txt",
            "cmd/cmd_101523_42.log",
            "nested/deep/2026-01-01_07-00-00_pid7.log",
        ];
        let mut created = Vec::new();
        for rel in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "# fake session log\n").unwrap();
            if rel.contains("2026-01-01") {
                set_modified(&path, old); // 30 天前的旧文件
            }
            created.push(path);
        }
        created
    }

    #[test]
    fn cleanup_removes_expired_logs_recursively_and_keeps_others() {
        let root = unique_temp("tree");
        let created = build_log_tree(&root);

        let report = cleanup_expired_logs_sync(&root, 14);

        // 6 个受管 .log / .state.log 文件全部被扫描到。
        assert_eq!(report.scanned, 6, "应扫描到全部受管日志: {report:?}");
        // 4 个 30 天前的旧文件（含 .state.log 与嵌套子目录）应被删除。
        assert_eq!(report.removed, 4, "全部过期会话日志应被清理: {report:?}");
        assert_eq!(report.skipped, 0);

        let survivor = |name: &str| created.iter().find(|p| p.ends_with(name)).unwrap();
        // 新文件（2026-06-01）保留。
        assert!(
            survivor("2026-06-01_09-00-00_pid456.log").exists(),
            "未过期日志应保留"
        );
        // cmd 会话日志（文件名不含旧日期标记，mtime 为当前时刻）保留。
        assert!(survivor("cmd_101523_42.log").exists(), "未过期 cmd 日志应保留");
        // 旧文件全部删除。
        assert!(
            !survivor("2026-01-01_10-00-00_pid123.log").exists(),
            "过期转录日志应删除"
        );
        assert!(
            !survivor("2026-01-01_10-00-00_pid123.state.log").exists(),
            "过期状态流应删除"
        );
        assert!(
            !survivor("2026-01-01_08-00-00_42.log").exists(),
            "过期的 bash 会话日志应删除"
        );
        assert!(
            !survivor("2026-01-01_07-00-00_pid7.log").exists(),
            "嵌套子目录中的过期日志应删除"
        );
        // 非受管文件（即使过期）保留。
        assert!(survivor("notes.txt").exists(), "非受管文件不得删除");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cleanup_with_retention_zero_removes_all_managed_logs() {
        let root = unique_temp("zero");
        let created = build_log_tree(&root);

        let report = cleanup_expired_logs_sync(&root, 0);
        assert_eq!(report.removed, 6, "保留 0 天 = 全部受管日志过期");
        assert!(created.iter().filter(|p| p.ends_with(".log")).all(|p| !p.exists()));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cleanup_missing_or_unreadable_dir_yields_zero_report() {
        // 不存在的目录：零报告、不 panic、不报错。
        let ghost = unique_temp("ghost");
        assert_eq!(
            cleanup_expired_logs_sync(&ghost, 14),
            CleanupReport::default(),
            "目录缺失应返回空报告"
        );
    }

    #[tokio::test]
    async fn async_wrapper_delegates_to_sync_core() {
        let root = unique_temp("async");
        let created = build_log_tree(&root);

        let report = cleanup_expired_logs(root.clone(), 14).await;
        assert_eq!(report.removed, 4, "异步入口应产生与同步核心一致的清理结果");

        let _ = std::fs::remove_dir_all(&root);
        let _ = created;
    }
}