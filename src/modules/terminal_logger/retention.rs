//! # 终端日志过期清理守护（v0.3.1 加固 · v0.6.1 收敛清理范围）
//!
//! 终端交互日志子系统把每次会话的日志落盘到 `logs/terminals/`（及显式配置的
//! 自定义目录）下的 `powershell/`、`bash/`、`cmd/` 三个子目录，文件名形如
//! `yyyy-MM-dd_HH-mm-ss_pid<PID>.log`、`<ts>_pid<PID>.state.log` 与
//! `cmd_HHMMSS…_<随机>.log`。若不做任何回收，日志碎文件会随会话次数无限积压，
//! 长期运行后占据大量磁盘空间。
//!
//! 本模块提供**过期清理守护**的核心逻辑：
//!
//! - [`is_managed_log_file`]：受管文件判定——`.log` 结尾 **且** 文件名匹配本子
//!   系统真实的两种命名形态（见该函数文档），二者必须同时成立；
//! - [`is_managed_subdir`]：受管子目录判定（`powershell` / `bash` / `cmd`）；
//! - [`retention_cutoff`]：由保留天数与当前时刻计算过期截止点（负数溢出夹取到
//!   `UNIX_EPOCH`，杜绝 `SystemTime` 下溢 panic）；
//! - [`cleanup_expired_logs_sync`]：**限定范围**扫描日志根目录（根 + 三个受管
//!   子目录，深度上限 [`MAX_SCAN_DEPTH`]），删除修改时间早于截止点的受管文件；
//! - [`cleanup_expired_logs`]：异步包装（`spawn_blocking`），供模块启动 / 定时
//!   任务在不阻塞 Tokio 工作线程的前提下调用。
//!
//! ## 扫描范围（v0.6.1 · M7 整改：从"递归删一切"收敛为"只在已知布局内清理"）
//!
//! 旧实现**递归**遍历 `log_base` 下**任意**目录，且受管判定只有 `.log` 后缀一项
//! ——文档声称"只扫 powershell/bash/cmd 三个子目录"，代码却无此限制。用户若把
//! 终端日志目录误配到含其他 `.log` 的项目根目录（自定义目录由用户自由选择），
//! 清理守护会**删除用户数据**。
//!
//! 现收敛为三道限制，全部为白名单式（不在名单内即绝不触碰）：
//!
//! 1. **目录范围**：只遍历根目录本身与 [`MANAGED_SUBDIRS`] 中的三个子目录，
//!    深度上限 [`MAX_SCAN_DEPTH`]（=1）——其他目录（含用户自建目录）**不进入**；
//! 2. **文件形态**：文件名必须匹配 `<yyyy-MM-dd_HH-mm-ss>…` 或 `cmd_…` 两种
//!    会话日志命名（见 [`is_managed_log_file`]）；
//! 3. **只删普通文件**：目录、符号链接及其它条目一律不动。
//!
//! ## 其余扫描语义（保守、容错、无权限擦除风险）
//!
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

/// 受管子目录白名单：三个 Shell 会话日志目录（与
/// [`super::SHELL_POWERSHELL`] / [`super::SHELL_BASH`] / [`super::SHELL_CMD`] 同源）。
///
/// 白名单式：不在名单内的目录**永不进入**（v0.6.1 · M7）。
pub const MANAGED_SUBDIRS: &[&str] = &["powershell", "bash", "cmd"];

/// 扫描深度上限：根目录（深度 0）+ 受管子目录（深度 1）。
///
/// 限制深度的作用与目录白名单叠加——即使某个受管子目录下又被用户塞进了子目录，
/// 也不会被继续深入遍历。
pub const MAX_SCAN_DEPTH: usize = 1;

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

/// 是否为本子系统管理的日志文件（v0.6.1 · M7：后缀 + 命名形态双重判定）。
///
/// 仅"以 `.log` 结尾"是不够的——那会把用户目录里任意 `.log` 都纳入删除候选。
/// 必须同时匹配本子系统真实产出的两种命名形态：
///
/// 1. **时间戳前缀**：`<yyyy-MM-dd_HH-mm-ss>_…`（PowerShell 转录
///    `yyyy-MM-dd_HH-mm-ss_pid<PID>.log`、状态流 `…_pid<PID>.state.log`、
///    bash 会话 `<ts>_<pid>.log`）；
/// 2. **cmd 前缀**：`cmd_…`（`cmd_HHMMSS…_<随机>.log`，由
///    `%TIME%` 与 `%RANDOM%` 拼接，长度不定，故只固定前缀）。
pub fn is_managed_log_file(name: &str) -> bool {
    if !name.ends_with(".log") {
        return false;
    }
    has_session_timestamp_prefix(name) || name.starts_with("cmd_")
}

/// 文件名是否以 `<yyyy-MM-dd_HH-mm-ss>_` 开头（会话日志的时间戳前缀）。
///
/// 逐位校验而非引入正则依赖：该前缀是本子系统自行生成的固定形态，逐位比对既精确
/// 又零分配。
fn has_session_timestamp_prefix(name: &str) -> bool {
    const PREFIX_LEN: usize = 20; // "YYYY-MM-DD_HH-MM-SS_"
    const DIGIT_POSITIONS: [usize; 14] = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18];

    let bytes = name.as_bytes();
    // 前缀之后至少还要有 1 个字符（如 PID 段），否则不是会话日志名。
    if bytes.len() <= PREFIX_LEN {
        return false;
    }
    if !DIGIT_POSITIONS.iter().all(|i| bytes[*i].is_ascii_digit()) {
        return false;
    }
    bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'_'
        && bytes[13] == b'-'
        && bytes[16] == b'-'
        && bytes[19] == b'_'
}

/// 是否为受管的 Shell 日志子目录（白名单，大小写不敏感）。
pub fn is_managed_subdir(name: &str) -> bool {
    MANAGED_SUBDIRS
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
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

/// 在日志根目录内**限定范围**扫描，删除修改时间早于截止点的受管日志文件。
///
/// 同步实现（`std::fs`），供单元测试与 [`cleanup_expired_logs`] 的阻塞池包装
/// 直接调用；目录不存在 / 不可读时返回零报告。
///
/// 扫描范围见模块文档「扫描范围」一节：根目录（深度 0）+ 三个受管子目录
/// （深度 1，[`MAX_SCAN_DEPTH`]），其余目录一律不进入。
pub fn cleanup_expired_logs_sync(dir: &Path, retention_days: u32) -> CleanupReport {
    let cutoff = retention_cutoff(retention_days, SystemTime::now());
    let mut report = CleanupReport::default();
    scan_dir(dir, 0, &cutoff, &mut report);
    report
}

/// 遍历单个目录（白名单 + 深度受限）的清理主体。
///
/// `depth`：根目录为 0；只有「深度未达上限」**且**「目录名在
/// [`MANAGED_SUBDIRS`] 白名单内」时才继续下探——这两条同时成立是 M7 的核心。
fn scan_dir(dir: &Path, depth: usize, cutoff: &SystemTime, report: &mut CleanupReport) {
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

        let name = entry.file_name().to_string_lossy().into_owned();
        if file_type.is_dir() {
            // 白名单 + 深度上限：仅 powershell/bash/cmd 且未达上限时下探。
            // 其余目录（用户自建目录、深层嵌套目录）**不进入**——这是"绝不误删
            // 用户数据"的关键闸门。
            if depth < MAX_SCAN_DEPTH && is_managed_subdir(&name) {
                scan_dir(&entry.path(), depth + 1, cutoff, report);
            }
        } else if file_type.is_file() && is_managed_log_file(&name) {
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
                Ok(_) => {}                    // 未过期：保留
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
        std::env::temp_dir().join(format!(
            "tltoolbox-ret-{tag}-{}-{nanos}",
            std::process::id()
        ))
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

    // ---- 受管文件判定（v0.6.1 · M7：后缀 + 命名形态双重判定） ----

    #[test]
    fn managed_log_file_filter_covers_log_and_state_log_only() {
        // 两类受管会话日志：.log 与 .state.log（时间戳前缀形态）。
        assert!(is_managed_log_file("2026-01-01_10-00-00_pid123.log"));
        assert!(is_managed_log_file("2026-01-01_10-00-00_pid123.state.log"));
        // bash 会话日志（时间戳 + PID）。
        assert!(is_managed_log_file("2026-01-01_08-00-00_42.log"));
        // cmd 会话日志（cmd_ 前缀）。
        assert!(is_managed_log_file("cmd_101523_42.log"));
        assert!(is_managed_log_file("cmd_10152342_12345.log"));
        // 非受管文件：即使后缀像日志也不得误删。
        assert!(!is_managed_log_file("config.toml"));
        assert!(!is_managed_log_file("notes.txt"));
        assert!(!is_managed_log_file(".log.tmp"), "非 .log 结尾不得视为受管");
        assert!(!is_managed_log_file("log"));
        assert!(!is_managed_log_file(""));
    }

    /// M7 的核心：**只有**本子系统真实产出的命名形态才算受管。
    ///
    /// 旧实现"以 `.log` 结尾即受管"会把用户目录里任意 `.log` 纳入删除候选；
    /// 下面这些形似日志但并非本子系统产出的文件必须全部被拒。
    #[test]
    fn managed_log_file_rejects_user_files_that_merely_end_with_log() {
        for user_file in [
            "notes.log",                    // 普通笔记
            "build.log",                    // 构建输出
            "app-2026-01-01.log",           // 日期不在前缀位置
            "2026-01-01_misc.log",          // 时间戳不完整（时分秒位置非数字）
            "2026-01-01_10-00-00.log",      // 时间戳后无 `_` 分隔（缺 PID 段）
            "26-01-01_10-00-00_pid7.log",   // 年份仅两位
            "2026/01/01_10-00-00_pid7.log", // 分隔符错误
            "session.log",
        ] {
            assert!(
                !is_managed_log_file(user_file),
                "非本子系统命名形态不得视为受管: {user_file}"
            );
        }
    }

    #[test]
    fn managed_subdir_whitelist_is_case_insensitive_and_closed() {
        for shell in ["powershell", "bash", "cmd", "PowerShell", "CMD"] {
            assert!(is_managed_subdir(shell), "{shell} 应在白名单内");
        }
        for other in ["scripts", "nested", "deep", "logs", ""] {
            assert!(!is_managed_subdir(other), "{other} 不在白名单内");
        }
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

    /// 构造一棵模拟日志树（三类 Shell 子目录 + 用户自建目录 + 深层嵌套 + 根目录
    /// 文件 + 非受管同后缀文件），旧文件 mtime 改写为 30 天前、新文件保持当前时刻。
    ///
    /// v0.6.1（M7）：树里刻意放入"用户数据"（`notes/` 目录、根目录的 `build.log`），
    /// 用于验证清理**绝不越界**。
    fn build_log_tree(root: &Path) -> Vec<PathBuf> {
        let old = SystemTime::now() - Duration::from_secs(30 * 86_400);
        // (相对路径, 是否 30 天前的旧文件)
        let files: [(&str, bool); 12] = [
            // —— 受管范围：三个 Shell 子目录 ——
            ("powershell/2026-01-01_10-00-00_pid123.log", true),
            ("powershell/2026-01-01_10-00-00_pid123.state.log", true),
            ("powershell/2026-06-01_09-00-00_pid456.log", false),
            ("bash/2026-01-01_08-00-00_42.log", true),
            ("bash/notes.txt", true),
            ("cmd/cmd_10152342_999.log", true),
            ("cmd/cmd_10152342_1000.log", false),
            // —— 根目录：合法的会话日志（受管，深度 0）与用户文件（不受管） ——
            ("2026-01-01_06-00-00_99.log", true),
            ("build.log", true),
            // —— 越界内容：用户自建目录 + 深层嵌套（M7 后一律不进入） ——
            ("notes/2026-01-01_05-00-00_pid8.log", true),
            ("nested/deep/2026-01-01_07-00-00_pid7.log", true),
            ("powershell/archive/2026-01-01_04-00-00_pid9.log", true),
        ];
        let mut created = Vec::new();
        for (rel, is_old) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "# fake session log\n").unwrap();
            if is_old {
                set_modified(&path, old);
            }
            created.push(path);
        }
        created
    }

    /// 核心契约：只清理「受管目录 + 受管命名 + 已过期」三者同时成立的文件。
    #[test]
    fn cleanup_removes_expired_logs_in_managed_scope_and_keeps_everything_else() {
        let root = unique_temp("tree");
        let created = build_log_tree(&root);

        let report = cleanup_expired_logs_sync(&root, 14);

        // 受管文件共 9 个：powershell 3 + bash 1 + cmd 2 + 根目录 1 + 越界 2
        //（越界者不被扫描，故 scanned 只计 7）。
        assert_eq!(
            report.scanned, 7,
            "只应扫描根目录与三个受管子目录内的受管文件: {report:?}"
        );
        // 过期且在被扫描范围内的：powershell 2（转录 + 状态流）+ bash 1 + cmd 1 + 根 1 = 5。
        assert_eq!(report.removed, 5, "过期受管日志应被清理: {report:?}");
        assert_eq!(report.skipped, 0);

        let survivor = |name: &str| created.iter().find(|p| p.ends_with(name)).unwrap();
        // —— 未过期：保留 ——
        assert!(
            survivor("2026-06-01_09-00-00_pid456.log").exists(),
            "未过期日志应保留"
        );
        assert!(
            survivor("cmd_10152342_1000.log").exists(),
            "未过期 cmd 日志应保留"
        );
        assert!(survivor("notes.txt").exists(), "非受管文件不得删除");
        // —— 已过期：删除 ——
        for removed in [
            "2026-01-01_10-00-00_pid123.log",
            "2026-01-01_10-00-00_pid123.state.log",
            "2026-01-01_08-00-00_42.log",
            "cmd_10152342_999.log",
            "2026-01-01_06-00-00_99.log",
        ] {
            assert!(!survivor(removed).exists(), "{removed} 应被清理");
        }

        // ---- M7 的关键断言：越界内容一律不受影响 ----
        for preserved in [
            "notes/2026-01-01_05-00-00_pid8.log",       // 用户自建目录
            "nested/deep/2026-01-01_07-00-00_pid7.log", // 深层嵌套
            "powershell/archive/2026-01-01_04-00-00_pid9.log", // 受管子目录之下再下一层
            "build.log",                                // 根目录内但命名不受管
        ] {
            assert!(
                survivor(preserved).exists(),
                "越界内容不得被删除（M7）：{preserved}"
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cleanup_with_retention_zero_removes_all_managed_logs_in_scope() {
        let root = unique_temp("zero");
        let created = build_log_tree(&root);

        let report = cleanup_expired_logs_sync(&root, 0);
        assert_eq!(report.removed, 7, "保留 0 天 = 范围内的受管日志全部过期");
        // 越界文件仍然一个不动（用完整文件名匹配：Path::ends_with 按整段比较）。
        for preserved in [
            "notes/2026-01-01_05-00-00_pid8.log",
            "nested/deep/2026-01-01_07-00-00_pid7.log",
            "powershell/archive/2026-01-01_04-00-00_pid9.log",
        ] {
            let path = created
                .iter()
                .find(|p| p.ends_with(preserved))
                .unwrap_or_else(|| panic!("测试树应包含 {preserved}"));
            assert!(path.exists(), "保留 0 天时越界文件仍不得删除: {preserved}");
        }

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
        assert_eq!(report.removed, 5, "异步入口应产生与同步核心一致的清理结果");

        let _ = std::fs::remove_dir_all(&root);
        let _ = created;
    }
}
