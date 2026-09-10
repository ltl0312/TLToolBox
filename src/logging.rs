//! # 日志子系统：tracing → 按天滚动文件落盘（Release 无控制台场景的故障排查通道）
//!
//! TLToolBox 的 release 构建是**无控制台 GUI 子系统**（见 `main.rs` 顶部的
//! `windows_subsystem`），进程不附带任何可见的“黑框”控制台。为使故障排查在
//! 无控制台前提下仍可进行，本模块把 [`tracing`] 输出按天滚动写入**可执行文件同级
//! 目录下的 `logs/`**（经 [`crate::config::resolve_app_path`] 锚定，与进程 CWD
//! 彻底解耦——注册表 Run 键自启时 CWD 可能落在 `C:\Windows\System32`，若按相对
//! 路径写日志将落错位置甚至因无写权限而失败；锚定 exe 目录后从任意工作目录拉起
//! 都稳定写到程序安装目录旁）。
//!
//! v0.3.2 起日志目录可经配置自定义（[`crate::config::AppConfig::app_log_dir`]）：
//! [`init_in_dir`] 接受调用方解析出的**有效日志目录**（缺省回退 exe 同级
//! `logs/`），[`init`] 保持指向默认目录的便捷入口。同目录下还落盘用户操作审计
//! 日志 `logs/app_audit.log`（见下方「审计日志」小节）。
//!
//! # 装配拓扑（多目标分发，`tracing_subscriber::layer`）
//!
//! 以 `tracing_subscriber::registry()` 挂载两层 `fmt::Layer`，编译期按构建模式分支：
//!
//! - **文件层（两种构建模式均启用）**：`tracing-appender` 的每日滚动文件 + 非阻塞
//!   写线程（`NonBlocking`）。文件名形如 `tltoolbox.2026-09-04.log`
//!   （`<前缀>.<UTC 日期>.log`，前缀/后缀经 `RollingFileAppender` builder 指定）；
//!   每天 UTC 零点轮转新文件，并借 builder 的 `max_log_files` 在滚动时自动清理
//!   过期文件（保留最近 [`MAX_LOG_FILES`] 份，即磁盘“清理机制”）。层级别为
//!   DEBUG——文件是面向故障排查的完整记录；
//! - **控制台层（仅 debug 构建装配）**：`fmt::Layer` 写 `stdout`，级别 INFO，与
//!   历史控制台行为保持一致（开发期观察）。release 构建该层整体不参与编译，
//!   保证不产生任何控制台 I/O。
//!
//! 说明：`NonBlocking` 把日志事件交给**专用写线程**串行落盘——写文件绝不停留在
//! 调用线程（UI 线程 / Tokio 工作线程）上，也不会因多线程并发写同一文件而相互
//! 穿插；缓冲通道按行数上限 128_000（crate 默认）工作，极端积压时按 lossy 语义
//! 丢弃新事件以优先保证调用线程不被阻塞（本应用为低频事件型常驻工具，实际不会
//! 触顶）。
//!
//! # 刷盘保证（WorkerGuard 生命周期）
//!
//! [`init`] / [`init_in_dir`] 返回的 [`LoggingGuard`] 持有非阻塞写线程的
//! [`WorkerGuard`]：其 `Drop` 会向写线程发送关闭信号并等待其把**尚未落盘的事件
//! 全部写完后**才返回（内部最多等待约 1 秒）。因此调用方（`main`）必须把守卫
//! 绑定到与进程同寿的变量上（`let _log_guard = logging::init();`），使**任何退出
//! 路径**（正常收尾、单实例二次启动提前 `return`、panic 展开）都会在进程结束前
//! 触发完整刷盘。
//!
//! # 审计日志（用户操作留痕，v0.3.2）
//!
//! [`AuditSink`] 把用户在 GUI 中的全部关键操作（模块开关、黑名单增删、开机自启
//! 切换、UAC 提权、检查更新、路径更改等）以**高精度时间戳**（微秒，UTC ISO8601）
//! 逐行追加写入 `<应用日志目录>/app_audit.log`（[`AUDIT_LOG_FILE`]）。写入经
//! 无界异步通道交给常驻任务串行落盘（调用方绝不阻塞），格式：
//!
//! ```text
//! [2026-09-04T12:34:56.123456Z] [模块开关] popup_blocker -> 开启 -> 成功
//! ```
//!
//! # 优雅降级
//!
//! 日志目录创建失败（如 exe 目录只读、被占用）时[`init`] **绝不 panic**：自动降级
//! 为「debug：仅控制台 / release：不落盘」，并以 `eprintln!` 提示（release 无
//! 控制台时该提示不可见，属预期）。应用本体不受影响照常启动。

use std::io;
use std::path::{Path, PathBuf};

use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

/// 日志目录名（相对**可执行文件所在目录**，见 [`log_directory`]）。
pub const DEFAULT_LOG_DIR: &str = "logs";

/// 日志文件名的前缀（builder 会以 `<前缀>.<日期>.<后缀>` 拼出完整文件名）。
pub const DEFAULT_LOG_PREFIX: &str = "tltoolbox";

/// 日志文件名的后缀（产出 `tltoolbox.2026-09-04.log` 形态）。
pub const DEFAULT_LOG_SUFFIX: &str = "log";

/// 用户操作审计日志的文件名（位于应用日志目录下，见 [`AuditSink`]）。
pub const AUDIT_LOG_FILE: &str = "app_audit.log";

/// 磁盘上最多保留的日志文件份数（含当日文件）。
///
/// 每日轮转出新文件时，`RollingFileAppender` 会按创建时间删除最旧的
/// `tltoolbox.<日期>.log`，使存量文件不超过本上限——即日志的自动清理机制
/// （crate 文档建议“需要保留 m 份时设 m+1”，此处 15 份≈保留约两周排查窗口）。
pub const MAX_LOG_FILES: usize = 15;

/// 文件层（滚动日志）记录的最高级别：DEBUG——文件是故障排查的完整记录。
const FILE_MAX_LEVEL: tracing::Level = tracing::Level::DEBUG;

/// 控制台层（仅 debug 构建装配）记录的最高级别：INFO，与历史控制台行为一致。
#[cfg(debug_assertions)]
const CONSOLE_MAX_LEVEL: tracing::Level = tracing::Level::INFO;

/// 日志守卫：持有非阻塞写线程的 [`WorkerGuard`]，进程退出时触发完整刷盘。
///
/// 由 `main()` 以 `let _log_guard = logging::init();` 绑定到进程同寿变量上；
/// 任何退出路径（含提前 `return`）都会在作用域结束时 `Drop`，从而把缓冲事件
/// 全部写盘后才结束进程。
#[derive(Debug)]
pub struct LoggingGuard {
    /// 文件写线程守卫；日志目录不可用（降级运行）时为 `None`。
    _worker: Option<WorkerGuard>,
}

impl LoggingGuard {
    /// 文件日志通道是否可用（目录创建 / 文件打开成功）。
    ///
    /// 供装配层在启动日志中如实上报“文件输出已启用/降级”，便于用户察觉
    /// exe 目录只读等异常部署。
    pub fn is_file_logging_active(&self) -> bool {
        self._worker.is_some()
    }
}

/// 解析日志目录的绝对路径：`<exe 所在目录>/logs`。
///
/// 复用 [`crate::config::resolve_app_path`] 的 exe 锚定规则（绝对路径原样返回、
/// 相对路径拼到 `current_exe()` 父目录下），保证自启 / 双击等任意 CWD 下日志
/// 都落在程序安装目录旁的 `logs/`。
pub fn log_directory() -> PathBuf {
    crate::config::resolve_app_path(Path::new(DEFAULT_LOG_DIR))
}

/// 装配全局日志订阅者（进程内**仅可调用一次**，见 [`init_to`] 的说明）。
///
/// 返回的 [`LoggingGuard`] 必须由调用方持有到进程退出：
///
/// ```no_run
/// let _log_guard = tltoolbox::logging::init();
/// ```
///
/// # 降级语义
/// 日志目录不可用时绝不 panic：debug 构建退化为仅控制台输出；release 构建退化为
/// 不落盘（[`LoggingGuard::is_file_logging_active`] 返回 `false` 可自检）。
pub fn init() -> LoggingGuard {
    init_in_dir(&log_directory())
}

/// 在**指定目录**装配全局日志订阅者（进程内**仅可调用一次**，见 [`init_to`]
/// 的说明）。
///
/// v0.3.2 起全局日志目录可经配置自定义：`main` 先加载配置，再把
/// [`AppConfig::effective_app_log_dir`](crate::config::AppConfig::effective_app_log_dir)
/// 解析出的**有效目录**传入本函数（缺省回退 exe 同级 `logs/`），使 tracing
/// 按天滚动文件与用户操作审计日志（[`AuditSink`]）落在同一自定义目录下。
/// 返回的 [`LoggingGuard`] 必须由调用方持有到进程退出（语义与 [`init`] 一致）。
pub fn init_in_dir(directory: &Path) -> LoggingGuard {
    init_to(directory)
}

/// 在指定目录装配日志订阅者（内部实现；`init()` 指向默认 exe 锚定目录）。
///
/// # 全局单例约束
/// 底层调用 `tracing_subscriber` 的全局 `init()`，同一进程内重复调用会 panic。
/// 本函数仅供 `main()` 入口调用一次；测试如需驱动完整链路，必须在测试进程中
/// 保持“只初始化一次”的纪律（见 `tests` 中的说明）。
fn init_to(directory: &Path) -> LoggingGuard {
    match build_file_writer(directory) {
        Ok((non_blocking, worker_guard)) => {
            // ---- 文件层：非阻塞写线程 → 按天滚动文件（两种构建模式均启用）。
            //      级别过滤用 Layer::with_filter 实现（fmt::Layer 本身没有
            //      with_max_level——那是 SubscriberBuilder 的方法）；文件内禁用
            //      ANSI 转义序列，便于直接阅读 / 检索。 ----
            let file_layer = fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false)
                .with_filter(LevelFilter::from_level(FILE_MAX_LEVEL));
            let subscriber = tracing_subscriber::registry().with(file_layer);

            // ---- 控制台层（仅 debug 构建装配）：release 不参与编译，零控制台 I/O。
            //      级别 INFO 与历史控制台行为一致；ANSI 颜色仅在真实终端启用。 ----
            #[cfg(debug_assertions)]
            let subscriber = {
                let console_layer = fmt::layer()
                    .with_writer(io::stdout)
                    .with_filter(LevelFilter::from_level(CONSOLE_MAX_LEVEL));
                subscriber.with(console_layer)
            };

            subscriber.init();
            LoggingGuard {
                _worker: Some(worker_guard),
            }
        }
        Err(err) => {
            eprintln!(
                "[tltoolbox] 日志目录初始化失败（{}），本次运行{}日志文件输出: {err}",
                directory.display(),
                if cfg!(debug_assertions) {
                    "降级为仅控制台、无"
                } else {
                    "无"
                }
            );
            // 降级路径仍保证 debug 构建有控制台输出（不静默吞掉全部日志）；
            // release 构建无控制台亦无文件——进程内日志为空操作，属预期降级。
            #[cfg(debug_assertions)]
            {
                let console_layer = fmt::layer()
                    .with_writer(io::stdout)
                    .with_filter(LevelFilter::from_level(CONSOLE_MAX_LEVEL));
                tracing_subscriber::registry().with(console_layer).init();
            }
            LoggingGuard { _worker: None }
        }
    }
}

/// 构建「非阻塞写线程 → 按天滚动文件」写出管线。
///
/// 预建目录后经 builder 构造 [`RollingFileAppender`]（`Rotation::DAILY` +
/// `filename_prefix`/`filename_suffix` → `tltoolbox.<YYYY-MM-DD>.log`；
/// `max_log_files` 提供滚动期自动清理）。文件层接入 `NonBlocking` 专用写线程，
/// 返回其 [`WorkerGuard`] 供进程退出刷盘。
fn build_file_writer(directory: &Path) -> io::Result<(NonBlocking, WorkerGuard)> {
    // 1) 目录预检：不可创建（父级只读 / 路径被文件占用等）→ 立即返回可读错误。
    std::fs::create_dir_all(directory).map_err(|source| {
        io::Error::new(
            source.kind(),
            format!("无法创建日志目录 '{}': {source}", directory.display()),
        )
    })?;

    // 2) builder 构造（返回 Result，绝不 panic；内部仍会 create_dir_all + 打开当日文件）。
    let sample_name = format!(
        "{}.{}.{}",
        DEFAULT_LOG_PREFIX, "YYYY-MM-DD", DEFAULT_LOG_SUFFIX
    );
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(DEFAULT_LOG_PREFIX)
        .filename_suffix(DEFAULT_LOG_SUFFIX)
        .max_log_files(MAX_LOG_FILES)
        .build(directory)
        .map_err(|err| {
            io::Error::other(format!(
                "无法打开日志文件 '{sample_name}'（目录: {}）: {err}",
                directory.display()
            ))
        })?;

    // 3) 接入专用写线程（默认 128_000 行缓冲、lossy 语义：极端积压时丢弃新事件
    //    而非阻塞调用线程；本应用低频日志实际不会触顶）。
    Ok(tracing_appender::non_blocking(appender))
}

// ---------------------------------------------------------------------------
// 用户操作审计日志（v0.3.2：GUI 全部关键操作的高精度留痕）
// ---------------------------------------------------------------------------

/// 审计日志单条记录的常规格式（见 [`AuditSink::record`]）。
///
/// `[<UTC 微秒时间戳>] [<操作类别>] <操作详情> -> <操作结果>`
const AUDIT_LINE_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.6fZ";

/// 用户操作审计日志发送端句柄（轻量克隆，跨线程可共享）。
///
/// 经无界异步通道把格式化好的记录行交给常驻落盘任务串行追加写入
/// `<应用日志目录>/app_audit.log`——调用方（UI 回调 / Tokio 任务）发送即返回、
/// 绝不阻塞；写入顺序与发送顺序严格一致（单消费者 FIFO）。
///
/// 生命周期：发送端被 UI 回调句柄持有到进程收尾；通道随最后一份发送端析构
/// 而关闭，落盘任务自然退出（最后一次 `record` 发送后至多残留一条在途记录，
/// 对低频审计语义可接受）。
#[derive(Clone)]
pub struct AuditSink {
    tx: tokio::sync::mpsc::UnboundedSender<String>,
}

impl AuditSink {
    /// 在指定目录装配审计落盘任务。
    ///
    /// 目录不存在时自动创建（与 tracing 文件日志同目录，即
    /// [`AppConfig::effective_app_log_dir`](crate::config::AppConfig::effective_app_log_dir)
    /// 的取值；创建失败不 panic——审计降级为静默空操作，应用本体不受影响）。
    pub fn new(directory: PathBuf) -> Self {
        let file_path = directory.join(AUDIT_LOG_FILE);
        if std::fs::create_dir_all(&directory).is_err() {
            tracing::warn!(
                target: "logging",
                "审计日志目录创建失败（本次运行审计不落盘）: '{}'",
                directory.display()
            );
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let mut reported = false;
            while let Some(line) = rx.recv().await {
                let text = line + "\n";
                let write_result = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&file_path)
                    .and_then(|mut file| {
                        use std::io::Write;
                        file.write_all(text.as_bytes())
                    });
                if let Err(err) = write_result {
                    // 首次失败告警一次即可（后续持续失败不刷屏）。
                    if !reported {
                        reported = true;
                        tracing::warn!(
                            target: "logging",
                            "审计日志写入失败: {} -> {err}",
                            file_path.display()
                        );
                    }
                }
            }
        });
        Self { tx }
    }

    /// 追加一条审计记录（**高精度时间戳**由本方法在调用时刻生成，微秒级 UTC）。
    ///
    /// - `action`：操作类别（如 `模块开关`、`黑名单`、`开机自启`、`UAC 提权`、
    ///   `检查更新`、`路径更改`）；
    /// - `detail`：操作详情（对象与目标状态，如 `popup_blocker -> 开启`）；
    /// - `result`：操作结果（如 `成功` / `失败: <原因>`，由调用方如实上报）。
    ///
    /// 三段文本均接受任何 `AsRef<str>` 载荷（`&str` / `String` / `&String`，
    /// 供调用方可选移动或借用）。任意线程可调用；发送失败（通道已关闭，进程
    /// 收尾中）时静默忽略。
    pub fn record<A, D, R>(&self, action: A, detail: D, result: R)
    where
        A: AsRef<str>,
        D: AsRef<str>,
        R: AsRef<str>,
    {
        let timestamp = chrono::Utc::now().format(AUDIT_LINE_FORMAT);
        let action = action.as_ref();
        let detail = detail.as_ref();
        let result = result.as_ref();
        let line = format!("[{timestamp}] [{action}] {detail} -> {result}");
        let _ = self.tx.send(line);
    }
}

/// 审计日志文件的绝对路径（供 UI 展示与测试直接定位）。
pub fn audit_log_path(directory: &Path) -> PathBuf {
    directory.join(AUDIT_LOG_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    /// 生成本次测试独有的临时子目录（确保并发测试互不干扰）。
    fn temp_subdir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("系统时钟应晚于 UNIX 纪元")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "tltoolbox-logging-{tag}-{}-{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 期望的当日文件名：`tltoolbox.<UTC 日期>.log`（与 rolling 内部命名一致；
    /// tracing-appender 以 UTC 零点为每日轮转边界）。
    fn expected_today_filename() -> String {
        let today = chrono::Utc::now().format("%Y-%m-%d");
        format!("{DEFAULT_LOG_PREFIX}.{today}.{DEFAULT_LOG_SUFFIX}")
    }

    // ---- 路径锚定：与 config::resolve_app_path 同一规则，绝不受 CWD 影响 ----

    #[test]
    fn resolve_log_directory_anchors_to_exe_dir() {
        let resolved = log_directory();
        assert!(
            resolved.is_absolute(),
            "exe 目录锚定应产出绝对路径，实际: {}",
            resolved.display()
        );
        assert_eq!(
            resolved
                .file_name()
                .map(|n| n.to_string_lossy().into_owned()),
            Some(DEFAULT_LOG_DIR.to_string()),
            "末级目录应为 logs/，实际: {}",
            resolved.display()
        );
        // 锚定基准必须是 exe 目录而非 CWD（Run 键自启场景 CWD = System32 的防线）。
        let exe_dir = std::env::current_exe()
            .expect("current_exe 应可用")
            .parent()
            .expect("exe 必有父目录")
            .to_path_buf();
        assert_eq!(
            resolved.parent().map(|p| p.to_path_buf()),
            Some(exe_dir),
            "日志目录应为 exe 同级 logs/，实际: {}",
            resolved.display()
        );
    }

    // ---- 滚动命名 + 守卫 Drop 刷盘（writer 管线级验证，不触碰全局 subscriber） ----

    #[test]
    fn daily_writer_produces_dated_file_and_flushes_on_guard_drop() {
        let dir = temp_subdir("writer");
        let (mut non_blocking, guard) = build_file_writer(&dir).expect("writer 构建应成功");

        // 1) 直接向非阻塞写线程投递一行文本。
        writeln!(non_blocking, "writer-flush-sentinel").expect("写入非阻塞通道应成功");

        // 2) Drop 守卫：写线程应把缓冲内容全部落盘后才结束（WorkerGuard 语义）。
        drop(guard);

        // 3) 当日文件必须存在且内容完整（含末尾换行）。
        let file = dir.join(expected_today_filename());
        assert!(
            file.is_file(),
            "应生成按天命名文件 '{}'，目录内容: {:?}",
            file.display(),
            std::fs::read_dir(&dir)
                .map(|d| d
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect::<Vec<_>>())
                .unwrap_or_default()
        );
        let content = std::fs::read_to_string(&file).expect("日志文件应可读");
        assert!(
            content.contains("writer-flush-sentinel"),
            "守卫 Drop 后缓冲内容应已刷入文件，实际内容: {content:?}"
        );

        cleanup(&dir);
    }

    // ---- 优雅降级：日志目录不可用绝不 panic ----

    #[test]
    fn file_writer_fails_gracefully_on_unusable_directory() {
        // 让“目录路径”穿过一个真实文件：create_dir_all 必然失败。
        let base = temp_subdir("bad-dir");
        std::fs::create_dir_all(&base).unwrap();
        let blocker = base.join("i-am-a-file");
        std::fs::write(&blocker, b"x").unwrap();

        let err = build_file_writer(&blocker.join("logs"))
            .expect_err("目录路径被文件占用时应返回错误而非 panic");
        assert!(!err.to_string().is_empty(), "错误信息不应为空");

        cleanup(&base);
    }

    // ---- 端到端：init_to → tracing::info! → 守卫 Drop → 文件含完整日志行 ----
    // 注意：本测试会安装**进程级全局 subscriber**，测试进程中必须仅此一处执行
    // init_to（同进程内 subscriber 只能安装一次）。其余测试均为 writer/路径级，
    // 不触碰全局状态。

    #[test]
    fn init_then_guard_drop_persists_log_line_to_daily_file() {
        let dir = temp_subdir("e2e");
        let guard = init_to(&dir);

        assert!(guard.is_file_logging_active(), "临时目录下文件日志应可用");
        tracing::info!(target: "logging_test", "e2e-guard-flush-sentinel");
        drop(guard);

        let file = dir.join(expected_today_filename());
        let content = std::fs::read_to_string(&file).expect("日志文件应已生成且可读");
        assert!(
            content.contains("e2e-guard-flush-sentinel"),
            "全局 subscriber 发出的日志行应在守卫 Drop 后完整落盘，实际内容: {content:?}"
        );

        cleanup(&dir);
    }

    // -----------------------------------------------------------------------
    // 用户操作审计日志（AuditSink，v0.3.2）
    // -----------------------------------------------------------------------

    /// 轮询等待审计文件出现目标子串（落盘任务异步写入，需有限等待窗口）。
    fn wait_for_audit_line(dir: &Path, needle: &str, timeout: std::time::Duration) -> String {
        let file = audit_log_path(dir);
        let deadline = std::time::Instant::now() + timeout;
        let mut last = String::new();
        while std::time::Instant::now() < deadline {
            if let Ok(content) = std::fs::read_to_string(&file) {
                if content.contains(needle) {
                    return content;
                }
                last = content;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!(
            "审计文件应在 {timeout:?} 内出现 {needle:?}，实际内容: {last:?}（文件: {}）",
            file.display()
        );
    }

    /// 审计记录携带高精度时间戳（微秒），按 动作/详情/结果 三段落盘。
    ///
    /// 多线程运行时：`AuditSink` 的落盘任务独立运行于工作线程，测试线程轮询
    /// 等待（current-thread 运行时会因测试线程 sleep 阻塞而永远等不到落盘）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn audit_sink_writes_timestamped_records() {
        let dir = temp_subdir("audit");
        let sink = AuditSink::new(dir.clone());

        sink.record("模块开关", "popup_blocker -> 开启", "成功");
        sink.record("检查更新", "found v9.9.9", "发现新版本");
        drop(sink); // 关闭发送端，落盘任务在排空后退出

        // 轮询等待两条记录各自出现（逐条 write_all 落盘；先等第二条再断言，
        // 避免“第一条刚出现、第二条尚未写盘”的瞬态竞态）。
        let _ = wait_for_audit_line(
            &dir,
            "popup_blocker -> 开启 -> 成功",
            Duration::from_secs(3),
        );
        let content = wait_for_audit_line(&dir, "found v9.9.9", Duration::from_secs(3));
        assert!(
            content.contains("[模块开关] popup_blocker -> 开启 -> 成功"),
            "审计行应含动作/详情/结果三段，实际: {content:?}"
        );
        assert!(
            content.contains("[检查更新] found v9.9.9 -> 发现新版本"),
            "第二条记录应完整落盘，实际: {content:?}"
        );

        // 时间戳形态：[YYYY-MM-DDTHH:MM:SS.ffffffZ]，微秒级高精度（RFC3339 子集）。
        let first_line = content.lines().next().expect("至少一行");
        let timestamp_part = first_line
            .split(']')
            .next()
            .expect("行首应为时间戳")
            .trim_start_matches('[');
        assert!(
            timestamp_part.len() >= 24,
            "时间戳应为微秒级 ISO8601 形态，实际: {timestamp_part:?}"
        );
        // RFC3339 解析（'Z' 结尾 + 小数秒）：chrono 的 parse_from_str 对 'Z' 与
        // %.6f 各有坑（%z 不接受 'Z'、%.Nf 的消费规则），统一走 RFC3339 校验。
        let parsed = chrono::DateTime::parse_from_rfc3339(timestamp_part);
        assert!(
            parsed.is_ok(),
            "时间戳应可被 RFC3339 解析，实际: {timestamp_part:?}（{parsed:?}）"
        );
        // 精度断言：小数秒至少 3 位（毫秒级即达标的高精度下限，µs 形态恒满足）。
        let frac_digits = timestamp_part
            .split('.')
            .nth(1)
            .map(|frac| frac.trim_end_matches('Z').len())
            .unwrap_or(0);
        assert!(
            frac_digits >= 3,
            "时间戳应含毫秒以上精度，实际小数秒位数: {frac_digits}（{timestamp_part:?}）"
        );

        cleanup(&dir);
    }

    /// 审计目录不存在时自动创建（AuditSink::new 的目录引导语义）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn audit_sink_creates_missing_directory() {
        let base = temp_subdir("audit-dir");
        let dir = base.join("nested").join("audit");
        assert!(!dir.exists(), "前置条件：目录尚不存在");

        let sink = AuditSink::new(dir.clone());
        sink.record("路径更改", "app_log_dir", "成功");
        drop(sink);

        let content = wait_for_audit_line(&dir, "路径更改", Duration::from_secs(3));
        assert!(content.contains("[路径更改] app_log_dir -> 成功"));
        assert!(audit_log_path(&dir).is_file(), "审计文件应已创建");

        cleanup(&base);
    }

    /// 审计文件名常量契约：`app_audit.log`（供 UI 展示 / 文档引用）。
    #[test]
    fn audit_file_name_contract() {
        assert_eq!(AUDIT_LOG_FILE, "app_audit.log");
        assert_eq!(
            audit_log_path(Path::new("C:/dir")),
            PathBuf::from("C:/dir/app_audit.log")
        );
    }
}
