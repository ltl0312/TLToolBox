//! # 终端交互日志子系统（基础层 → 装配层 → ToolModule 生命周期接入）
//!
//! 目标：为交互式 Shell 会话（PowerShell / bash / cmd）注入会话日志钩子，把
//! 用户在终端里执行的命令与屏幕输出沉淀为本地文件，供后续回看 / 审计使用。
//!
//! 本子系统分阶段推进，各层职责如下：
//!
//! - [`anchor`]：通用**无损文本锚点插拔引擎**——以带 TAG 的成对标记行在
//!   Shell 配置文件中圈出 TLToolBox 管理的区块，支持注入 / 更新 / 彻底移除，
//!   且不破坏文件其余任何用户内容（详见模块内文档）；
//! - [`ps_bash`]：**PowerShell / Bash 挂载器**——经锚点引擎向 PowerShell——经锚点引擎向 PowerShell
//!   5.1 / 7.x 两套引擎、每引擎 `$PROFILE` 的 CurrentUserCurrentHost 与
//!   AllHosts（[`PowerShellHook`]，**静默** `Start-Transcript` 转录 + 提示符
//!   状态记录）以及 bash `.bashrc` 与 `.bash_profile`（[`BashHook`]，
//!   `PROMPT_COMMAND` + `history 1` 命令记录）注入会话日志钩子，支持幂等
//!   安装 / 无损卸载（详见模块内文档）；
//! - [`cmd`]：**CMD 会话记录**——cmd.exe 无原生每命令钩子（PROMPT 仅文本、
//!   doskey 宏仅按名触发、AutoRun 仅启动时执行），故走「`HKCU\...\Command
//!   Processor` 的 AutoRun 注册表钩子 + exe 同级 `scripts\tltb_cmd_capture.bat`
//!   **原生非侵入**会话记录」：不包裹 / 不接管 cmd，仅借 AutoRun 写会话头、
//!   借 doskey `exit` 宏在会话结束时把 `doskey /history` 键入命令清单与
//!   会话尾落盘（[`CmdHook`]，详见模块内文档）；
//! - [`retention`]：**日志过期清理守护**——`start` 时立即（并以每日为周期）
//!   递归扫描日志根目录（`powershell/` `bash/` `cmd/` 及任意子目录），删除
//!   超出保留期限（[`crate::config::AppConfig::terminal_log_retention_days`]，
//!   缺省 14 天）的 `.log` / `.state.log` 会话日志，防止日志碎文件无限积压
//!   （详见模块内文档）；
//! - 配置侧：[`crate::config::AppConfig`] 的 `terminal_log_dir` /
//!   `enabled_shells` 字段（缺省键回退默认值，见
//!   [`crate::config::AppConfig::effective_terminal_log_dir`]）。
//!
//! # 装配层（本文件）：ToolModule 化的第四个常驻守护模块
//!
//! 本文件是终端日志子系统的**装配层**，把上述三个挂载器组合为标准的
//! [`ToolModule`]（`crate::modules::ToolModule`）常驻守护模块：
//!
//! - [`HookManager`]：统一调度三只挂载器（[`PowerShellHook`] / [`BashHook`] /
//!   [`CmdHook`]）的 `install` 与 `uninstall`。构造时按配置
//!   `enabled_shells` 名单**过滤**要管理的 Shell（`powershell` / `bash` /
//!   `cmd`），未启用或无法定位目标（如无主目录、无 `powershell.exe`）的
//!   挂载器被剔除，其余照常装配；`install_all` 任一步失败时**回滚**已成功
//!   安装的钩子（尽力而为），保证「启动失败 → 系统侧无任何残留」的原子语义；
//! - [`TerminalLoggerModule`]：装配层对外暴露的模块本体。`start` 时从运行期
//!   配置**解析最终日志存储目录**（[`AppConfig::effective_terminal_log_dir`]，
//!   含缺省回退到 exe 同级 `logs/terminals`），随后按 `enabled_shells`
//!   名单调用 `HookManager::install_all`；`stop` 调用 `uninstall_all`；
//!   `is_running` 由**内存原子标志与注册表状态**联合判定——cmd 钩子启用时，
//!   除内存运行标志外还要求 AutoRun 注册表值仍持有本模块的 `call` 片段
//!   （钩子被外部清理后如实反映停止态，见 [`TerminalLoggerModule::is_running`]）。
//!
//! ## 生命周期与失败语义（与其它常驻模块一致）
//!
//! - `start` / `stop` 均以 `&self` 共享借用实现，经内部 `AsyncMutex` 串行化
//!   并发变迁，重复调用幂等（已在运行态的 `start`、已停止态的 `stop` 直接
//!   短路返回）；
//! - `start` 失败（如目标 `.bashrc` 不可解码、exe 路径含 `%` / `!` 无法写入
//!   AutoRun）→ 保持停止态并上报，先前已安装的钩子被回滚卸载；
//! - `stop` 卸载失败 → 保持运行态并上报（与
//!   [`KeepAwakeModule`](crate::modules::keep_awake::KeepAwakeModule) 的失败
//!   语义一致：状态机与系统事实不产生「显示已停止、钩子仍生效」的分叉），
//!   下次 `stop` 可重试；
//! - 钩子在 `start` 期**惰性装配**（按需定位目标、探测真实 `$PROFILE`），
//!   模块注册本身不做任何系统探测或改写——用户不开启本模块就不会产生
//!   `powershell.exe` 探测等额外开销。

pub mod anchor;
pub mod cmd;
pub mod ps_bash;
pub mod retention;

pub use anchor::{block_end_marker, block_start_marker, inject_block, remove_block};
pub use cmd::CmdHook;
pub use ps_bash::{BashHook, PowerShellHook};

use super::{ModuleError, ToolModule};
use crate::config::AppConfig;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex as SyncMutex;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Shell ID 常量（与配置 `enabled_shells` 名单的取值约定）
// ---------------------------------------------------------------------------

/// `enabled_shells` 名单中 PowerShell 的 ID（对应 [`PowerShellHook`]）。
pub const SHELL_POWERSHELL: &str = "powershell";

/// `enabled_shells` 名单中 bash 的 ID（对应 [`BashHook`]）。
pub const SHELL_BASH: &str = "bash";

/// `enabled_shells` 名单中 cmd 的 ID（对应 [`CmdHook`]）。
pub const SHELL_CMD: &str = "cmd";

// ---------------------------------------------------------------------------
// HookManager：统一调度 PowerShell / Bash / CMD 三只挂载器
// ---------------------------------------------------------------------------

/// 终端日志三只挂载器的统一调度器。
///
/// 按配置 `enabled_shells` 名单持有「启用中的 Shell」对应的挂载器
/// （`None` = 未启用该 Shell，或启用但无法定位目标而被剔除）：
///
/// - [`PowerShellHook`]（`powershell`）：向 PowerShell 5.1 / 7.x `$PROFILE`
///   注入**静默** `Start-Transcript` 转录 + 提示符状态记录区块；
/// - [`BashHook`]（`bash`）：向 `$HOME/.bashrc` 与 `$HOME/.bash_profile`
///   注入 `PROMPT_COMMAND` + `history 1` 命令记录区块；
/// - [`CmdHook`]（`cmd`）：生成捕获脚本并挂载 cmd AutoRun 注册表钩子。
///
/// [`install_all`](Self::install_all) / [`uninstall_all`](Self::uninstall_all)
/// 按固定顺序（PowerShell → bash → cmd）逐只调度；install 失败时回滚已成功
/// 安装的钩子（保证「启动失败 → 系统侧无残留」的原子语义），uninstall
/// 失败时继续尝试其余钩子后聚合上报。对未持有挂载器的 Shell（未启用 /
/// 已剔除），相应步骤为空操作。
#[derive(Debug, Clone)]
pub struct HookManager {
    /// PowerShell 转录挂载器（`enabled_shells` 含 `powershell` 且可定位时持有）。
    powershell: Option<PowerShellHook>,
    /// bash 转录挂载器（`enabled_shells` 含 `bash` 且可定位时持有）。
    bash: Option<BashHook>,
    /// cmd AutoRun 挂载器（`enabled_shells` 含 `cmd` 时持有）。
    cmd: Option<CmdHook>,
}

impl HookManager {
    /// 按 `enabled_shells` 名单装配调度器（名称大小写不敏感）。
    ///
    /// 逐个 Shell 定位目标挂载器：定位失败（如 bash 无主目录、PowerShell
    /// 无法确定 `$PROFILE`）仅**告警剔除**该 Shell，不影响其余 Shell 装配——
    /// 日志钩子绝不能让模块因个别 Shell 环境缺失而整体不可用。名单中的未知
    /// Shell（`wt` / `fish` 等，暂无对应挂载器）同样告警忽略。
    pub fn for_enabled_shells(enabled_shells: &[String]) -> Self {
        let mut powershell = None;
        let mut bash = None;
        let mut cmd = None;

        for shell in enabled_shells {
            match shell.to_ascii_lowercase().as_str() {
                SHELL_POWERSHELL if powershell.is_none() => match PowerShellHook::new() {
                    Ok(hook) => powershell = Some(hook),
                    Err(err) => tracing::warn!(
                        target: "terminal_logger",
                        "PowerShell 钩子定位失败（本次跳过 powershell）: {err}"
                    ),
                },
                SHELL_BASH if bash.is_none() => match BashHook::new() {
                    Ok(hook) => bash = Some(hook),
                    Err(err) => tracing::warn!(
                        target: "terminal_logger",
                        "bash 钩子定位失败（本次跳过 bash）: {err}"
                    ),
                },
                SHELL_CMD if cmd.is_none() => match CmdHook::new() {
                    Ok(hook) => cmd = Some(hook),
                    Err(err) => tracing::warn!(
                        target: "terminal_logger",
                        "cmd 钩子定位失败（本次跳过 cmd）: {err}"
                    ),
                },
                SHELL_POWERSHELL | SHELL_BASH | SHELL_CMD => {
                    // 名单重复：该 Shell 已装配，忽略重复条目。
                }
                other => tracing::warn!(
                    target: "terminal_logger",
                    "enabled_shells 含暂不支持的 Shell「{other}」，已忽略"
                ),
            }
        }

        Self {
            powershell,
            bash,
            cmd,
        }
    }

    /// 当前调度器是否未持有任何挂载器（`enabled_shells` 为空 / 全部剔除）。
    pub fn is_empty(&self) -> bool {
        self.powershell.is_none() && self.bash.is_none() && self.cmd.is_none()
    }

    /// cmd 挂载器是否在调度范围内（`enabled_shells` 含 `cmd` 且已装配）。
    pub fn cmd_active(&self) -> bool {
        self.cmd.is_some()
    }

    /// 统一安装：依次把启用的 PowerShell / bash / cmd 钩子挂到 `log_base`
    /// 指定的日志根目录下。任一步失败 → 尽力回滚已成功安装的钩子后上报，
    /// 保证「启动失败 → 系统侧无残留」的原子语义。
    pub fn install_all(&self, log_base: &Path) -> Result<(), ModuleError> {
        if let Some(hook) = &self.powershell {
            if let Err(err) = hook.install(log_base) {
                self.rollback_after_failure("PowerShell");
                return Err(format!("PowerShell 转录钩子安装失败: {err}").into());
            }
        }
        if let Some(hook) = &self.bash {
            if let Err(err) = hook.install(log_base) {
                self.rollback_after_failure("bash");
                return Err(format!("bash 记录钩子安装失败: {err}").into());
            }
        }
        if let Some(hook) = &self.cmd {
            if let Err(err) = hook.install(log_base) {
                self.rollback_after_failure("cmd");
                return Err(format!("cmd AutoRun 钩子安装失败: {err}").into());
            }
        }
        Ok(())
    }

    /// 统一卸载：依次移除启用的 PowerShell / bash / cmd 钩子。
    ///
    /// 所有钩子都会得到卸载尝试（单个失败不阻断其余）；至少一个失败时聚合
    /// 上报首个错误（卸载本身幂等，调用方可重试）。
    pub fn uninstall_all(&self) -> Result<(), ModuleError> {
        let mut first_error: Option<String> = None;

        if let Some(hook) = &self.powershell {
            if let Err(err) = hook.uninstall() {
                first_error.get_or_insert_with(|| format!("PowerShell 转录钩子卸载失败: {err}"));
            }
        }
        if let Some(hook) = &self.bash {
            if let Err(err) = hook.uninstall() {
                first_error.get_or_insert_with(|| format!("bash 记录钩子卸载失败: {err}"));
            }
        }
        if let Some(hook) = &self.cmd {
            if let Err(err) = hook.uninstall() {
                first_error.get_or_insert_with(|| format!("cmd AutoRun 钩子卸载失败: {err}"));
            }
        }

        match first_error {
            Some(message) => Err(message.into()),
            None => Ok(()),
        }
    }

    /// 安装中途失败后的回滚：尽力卸载本调度器范围内全部钩子（未安装者为
    /// 幂等空操作），保证「安装失败 → 不残留部分钩子」的原子语义。
    fn rollback_after_failure(&self, failed_shell: &str) {
        tracing::warn!(
            target: "terminal_logger",
            "{failed_shell} 钩子安装失败，正在回滚已成功安装的钩子"
        );
        if let Err(err) = self.uninstall_all() {
            tracing::warn!(
                target: "terminal_logger",
                "安装失败回滚不完整（建议重试卸载以清理残留）: {err}"
            );
        }
    }
}

/// 【目录安全创建】按**有效日志根目录**（[`AppConfig::effective_terminal_log_dir`]）
/// 与已装配的 Shell 挂载器预建写入目录：
///
/// - 根目录（会话日志的公共父目录）恒建；
/// - 各已装配 Shell 的子目录（`powershell/` / `bash/` / `cmd/`）按挂载器有无创建；
/// - 无任何挂载器（`enabled_shells` 为空 / 全部剔除）时不创建任何目录——没有
///   写入方就不产生空目录。
///
/// 注入脚本运行期亦会自建目录（PS: `New-Item -Force` / bash: `mkdir -p`），
/// 此处预建使**首次写入**不依赖 Shell 侧行为；创建失败仅告警（返回的错误不
/// 上抛），目录不可写绝不阻断模块启动。返回实际尝试创建的目标目录列表。
fn ensure_log_dirs(log_base: &Path, hooks: &HookManager) -> Vec<PathBuf> {
    if hooks.is_empty() {
        return Vec::new();
    }
    let mut dirs = vec![log_base.to_path_buf()];
    for (sub, active) in [
        ("powershell", hooks.powershell.is_some()),
        ("bash", hooks.bash.is_some()),
        ("cmd", hooks.cmd.is_some()),
    ] {
        if active {
            dirs.push(log_base.join(sub));
        }
    }
    for dir in &dirs {
        if let Err(err) = std::fs::create_dir_all(dir) {
            tracing::warn!(
                target: "terminal_logger",
                "终端日志目录创建失败（本次写入将由 Shell 侧钩子重试自建）: '{}' -> {err}",
                dir.display()
            );
        }
    }
    dirs
}

// ---------------------------------------------------------------------------
// TerminalLoggerModule：ToolModule 化的常驻守护模块本体
// ---------------------------------------------------------------------------
/// 钩子卸载失败后的**自动重试次数**（v0.6.2 · M10）。
///
/// 卸载承担回滚系统改写的职责（移除 Shell 配置注入块 / 注册表 AutoRun 片段），
/// 失败会在用户系统上留下残留。最常见的失败形态是目标文件被编辑器 / 杀毒进程
/// 瞬态占用（`ERROR_SHARING_VIOLATION` 一类），短退避重试即可解决——因此重试
/// 收益远高于成本。
const UNINSTALL_MAX_ATTEMPTS: usize = 3;

/// 相邻两次卸载重试之间的退避间隔。
const UNINSTALL_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(150);

/// 终端交互日志模块（第四个常驻守护模块）。/// 生命周期完全由内部可变性管理（原子运行标志 + 异步锁串行化变迁 + 最近
/// 一次成功安装的钩子快照），对外仅暴露共享引用接口，天然满足 [`ToolModule`]
/// 的 `Send + Sync` 契约。钩子在 `start` 期按运行期配置**惰性装配**——
/// 模块注册与构造不做任何系统探测或改写。
#[derive(Clone)]
pub struct TerminalLoggerModule {
    inner: Arc<TerminalLoggerInner>,
}

/// 模块内部并发状态。
struct TerminalLoggerInner {
    /// 运行期配置句柄（与 `crate::main` 的 `runtime_config` 同源）：
    /// `enabled_shells` 决定装配哪些 Shell 钩子，`terminal_log_dir` 经
    /// [`AppConfig::effective_terminal_log_dir`] 在每次 `start` 解析最终日志目录。
    config: Arc<AsyncMutex<AppConfig>>,
    /// 最近一次成功 `start` 安装的钩子集合（`stop` 复用以精确卸载「当时
    /// 安装的钩子」，不因期间配置变化而漏卸；`None` = 尚未成功启动）。
    hooks: SyncMutex<Option<HookManager>>,
    /// 串行化 `start` / `stop` 生命周期变迁，杜绝并发启停互相穿插。
    lifecycle: AsyncMutex<()>,
    /// 快速查询的运行标志（无锁读取路径，供 UI 高频轮询）。
    running: AtomicBool,
    /// 本次运行是否包含 cmd AutoRun 钩子（`is_running` 的注册表核验开关）。
    cmd_hook_active: AtomicBool,
    /// 日志过期清理守护任务的取消令牌（`start` 派生、`stop` 取消；
    /// `None` = 守护未在运行）。修复式重装 / 重复启动时先取消旧令牌，
    /// 杜绝双守护任务并存。
    retention_guard: SyncMutex<Option<CancellationToken>>,
    /// 最近一次「钩子卸载失败」的原因（v0.6.2 · M10）。
    ///
    /// `Some` = 存在**已知的系统侧残留**（Shell 配置文件注入块 / 注册表
    /// AutoRun 片段），UI 据此给出「需修复」提示；成功卸载 / 强制清理后清空。
    /// 无此记录时，一次失败的卸载会表现为"点了没反应"——残留悄悄留在用户
    /// 的 Shell 配置里，下次启动还可能重复注入。
    last_stop_error: SyncMutex<Option<String>>,
}

impl TerminalLoggerModule {
    /// 以运行期配置句柄构造一个尚未启动的终端日志模块。
    ///
    /// 配置句柄与装配层共享（`Arc` 克隆），因此运行期对 `enabled_shells` /
    /// `terminal_log_dir` 的修改会在下一次 `start` 生效。
    pub fn new(config: Arc<AsyncMutex<AppConfig>>) -> Self {
        Self {
            inner: Arc::new(TerminalLoggerInner {
                config,
                hooks: SyncMutex::new(None),
                lifecycle: AsyncMutex::new(()),
                running: AtomicBool::new(false),
                cmd_hook_active: AtomicBool::new(false),
                retention_guard: SyncMutex::new(None),
                last_stop_error: SyncMutex::new(None),
            }),
        }
    }

    /// 注册表一致性核验：cmd 钩子启用时，AutoRun 注册表值必须仍持有本模块
    /// 的 `call "…tltb_cmd_capture.bat"` 片段（[`CmdHook::is_installed`]）。
    ///
    /// cmd 钩子未启用 → 无注册表事实可核验，直接放行。探测失败（无法定位
    /// exe / 注册表不可读）采取宽松语义返回 `true`——只有「明确读到 AutoRun
    /// 已不含本模块片段」才判定外部清理，避免误报。
    fn registry_state_ok(&self) -> bool {
        if !self.inner.cmd_hook_active.load(Ordering::Acquire) {
            return true;
        }
        CmdHook::new()
            .map(|hook| hook.is_installed())
            .unwrap_or(true)
    }

    /// 取最近一次成功安装的钩子快照（供 `stop` 精确卸载「当时安装的钩子」）。
    ///
    /// 快照缺失（尚未成功启动 / 已被取走）时按当前配置重建兜底——`stop` 是
    /// 幂等操作，卸载本身对未安装钩子是空操作，重建目标仅用于尽力清理。
    async fn current_hooks(&self) -> HookManager {
        let hooks = self
            .inner
            .hooks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        match hooks {
            Some(hooks) => hooks,
            None => {
                let enabled_shells = {
                    let cfg = self.inner.config.lock().await;
                    cfg.enabled_shells.clone()
                };
                HookManager::for_enabled_shells(&enabled_shells)
            }
        }
    }

    /// 派生日志过期清理守护任务：**立即**扫一轮过期日志，此后按
    /// [`retention::CLEANUP_INTERVAL`]（每日）定时清理。
    ///
    /// - 先取消既有守护令牌再派生新任务：修复式重装（运行标志为 true 但
    ///   AutoRun 片段被外部清理后重新 `start`）与重复启动场景下，保证任意
    ///   时刻至多存在一个守护任务，杜绝双守护并存；
    /// - 守护任务在 `select!` 的定时等待点响应取消令牌（`stop` 成功时调用），
    ///   最迟一个周期内退出，不阻塞任何调用方、不遗留后台任务；
    /// - 目录尚不存在（模块首次启动前）时扫描返回零报告，属正常语义。
    fn spawn_retention_guard(&self, log_base: PathBuf, retention_days: u32) {
        // 取消既有守护（若有）：修复式重装 / 重复启动不产生双守护任务。
        if let Some(old) = self
            .inner
            .retention_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            old.cancel();
        }

        let token = CancellationToken::new();
        let task_token = token.clone();
        tokio::spawn(async move {
            // 消费 interval 的首个「立即」tick，随后进入「先清理、再等待」循环：
            // 模块启动即执行第一轮扫描，此后每个周期清理一次。
            let mut interval = tokio::time::interval(retention::CLEANUP_INTERVAL);
            interval.tick().await;
            loop {
                let report =
                    retention::cleanup_expired_logs(log_base.clone(), retention_days).await;
                tracing::info!(
                    target: "terminal_logger",
                    dir = %log_base.display(),
                    retention_days,
                    scanned = report.scanned,
                    removed = report.removed,
                    skipped = report.skipped,
                    "日志过期清理完成（保留 {retention_days} 天）"
                );
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = task_token.cancelled() => break,
                }
            }
            tracing::debug!(target: "terminal_logger", "日志过期清理守护已退出");
        });

        *self
            .inner
            .retention_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(token);
    }
    /// 带自动重试的钩子卸载（v0.6.2 · M10）。
    ///
    /// 最多尝试 [`UNINSTALL_MAX_ATTEMPTS`] 次，相邻两次间隔
    /// [`UNINSTALL_RETRY_BACKOFF`]——覆盖"目标文件被编辑器 / 杀毒进程瞬态占用"
    /// 这一最常见的失败形态。全部尝试都失败时返回**最后一次**错误（每次失败都
    /// 照常 `warn` 留痕，保证失败路径可诊断）。
    async fn uninstall_with_retry(&self, hooks: &HookManager) -> Result<(), ModuleError> {
        let mut last_err: Option<ModuleError> = None;
        for attempt in 1..=UNINSTALL_MAX_ATTEMPTS {
            // v0.6.2（L6）：同步文件 / 注册表 IO 移出 Tokio 工作线程。
            let attempt_hooks = hooks.clone();
            let result = tokio::task::spawn_blocking(move || attempt_hooks.uninstall_all()).await;
            match result {
                Ok(Ok(())) => {
                    if attempt > 1 {
                        tracing::info!(
                            target: "terminal_logger",
                            attempt,
                            "钩子卸载在第 {attempt} 次尝试成功"
                        );
                    }
                    return Ok(());
                }
                Ok(Err(err)) => {
                    tracing::warn!(
                        target: "terminal_logger",
                        attempt,
                        total = UNINSTALL_MAX_ATTEMPTS,
                        "钩子卸载失败（第 {attempt}/{UNINSTALL_MAX_ATTEMPTS} 次）: {err}"
                    );
                    last_err = Some(err);
                }
                Err(join_err) => {
                    tracing::error!(
                        target: "terminal_logger",
                        "钩子卸载阻塞任务异常终止: {join_err}"
                    );
                    last_err = Some(format!("卸载任务异常终止: {join_err}").into());
                }
            }
            if attempt < UNINSTALL_MAX_ATTEMPTS {
                tokio::time::sleep(UNINSTALL_RETRY_BACKOFF).await;
            }
        }
        Err(last_err.unwrap_or_else(|| "钩子卸载失败（无错误详情）".into()))
    }

    /// 最近一次「钩子卸载失败」的原因；`None` = 无已知残留。
    ///
    /// 供装配层写审计 / UI 提示「需修复」。
    pub fn last_stop_error(&self) -> Option<String> {
        self.inner
            .last_stop_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 是否需要修复（v0.6.2 · M10，UI「需修复」提示的判定来源）。
    ///
    /// 两种形态都意味着**系统侧存在与模块状态不一致的残留**：
    /// 1. 最近一次卸载失败（Shell 配置注入块 / 注册表 AutoRun 片段仍在）；
    /// 2. 运行标志为真但 [`Self::is_running`] 为假（cmd AutoRun 片段被外部清理）。
    pub fn needs_repair(&self) -> bool {
        if self.last_stop_error().is_some() {
            return true;
        }
        self.inner.running.load(Ordering::Acquire) && !self.is_running()
    }

    /// **强制清理**终端日志钩子（v0.6.2 · M10 手动修复入口）。
    ///
    /// 与 [`ToolModule::stop`] 的差异：不依赖运行标志与「最近一次成功安装」的
    /// 快照，而是按**当前配置**重新装配全部 Shell 挂载器并逐一卸载——专为清理
    /// 「上次卸载失败的残留」或「外部注入的历史残留」设计，对未残留者同样是
    /// 幂等空操作。成功后清空残留记录。
    ///
    /// # 线程模型
    /// 同步文件 / 注册表 IO 经 `spawn_blocking` 移出 Tokio 工作线程（与
    /// [`crate::modules::terminal_logger`] 的装配纪律一致）。
    pub async fn force_cleanup_hooks(&self) -> Result<(), ModuleError> {
        let _guard = self.inner.lifecycle.lock().await;
        // 按当前配置装配挂载器（与 start 同一套定位逻辑，保证能找到残留所在的
        // 目标文件 / 注册表值）。
        let enabled_shells = {
            let cfg = self.inner.config.lock().await;
            cfg.enabled_shells.clone()
        };
        let hooks = HookManager::for_enabled_shells(&enabled_shells);
        let attempt = hooks.clone();

        let outcome = tokio::task::spawn_blocking(move || attempt.uninstall_all())
            .await
            .map_err(|err| -> ModuleError {
                format!("强制清理任务异常终止: {err}").into()
            })?;

        match outcome {
            Ok(()) => {
                *self
                    .inner
                    .last_stop_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
                tracing::info!(
                    target: "terminal_logger",
                    "终端日志钩子强制清理完成：Shell 配置与注册表均无本模块残留"
                );
                Ok(())
            }
            Err(err) => {
                *self
                    .inner
                    .last_stop_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(err.to_string());
                Err(err)
            }
        }
    }
}

#[async_trait]
impl ToolModule for TerminalLoggerModule {
    fn id(&self) -> &'static str {
        "terminal_logger"
    }

    fn display_name(&self) -> &'static str {
        "终端交互日志"
    }

    fn description(&self) -> &'static str {
        "自动记录 CMD、PowerShell、Bash 终端的全部输入与屏幕输出内容"
    }

    async fn start(&self) -> Result<(), ModuleError> {
        let _guard = self.inner.lifecycle.lock().await;

        // 已在运行且注册表侧钩子仍在（含外部清理后的一致性检查）→ 幂等短路。
        if self.inner.running.load(Ordering::Acquire) && self.registry_state_ok() {
            return Ok(());
        }
        // 运行标志为 true 但 AutoRun 片段已被外部清理 → 不短路，走修复式重装。

        // 1) 从运行期配置解析最终日志存储目录、保留期限与启用的 Shell 名单。
        let (log_base, retention_days, enabled_shells) = {
            let cfg = self.inner.config.lock().await;
            (
                cfg.effective_terminal_log_dir(),
                cfg.effective_terminal_log_retention_days(),
                cfg.enabled_shells.clone(),
            )
        };

        // 2) 按名单装配挂载器（未启用 / 无法定位的 Shell 被剔除）。
        let hooks = HookManager::for_enabled_shells(&enabled_shells);

        // 2.5) 目录安全创建（effective_log_dir 语义）：首次写入前保证日志根目录
        //      与各已装配 Shell 的子目录存在，见 [`ensure_log_dirs`]。
        ensure_log_dirs(&log_base, &hooks);

        // 3) 统一安装钩子（未启用任何 Shell 时为空操作）。
        //    v0.6.2（L6）：同步文件 IO（读配置文件 / 写注入块）经 `spawn_blocking`
        //    移出 Tokio 工作线程，与模块卸载路径的纪律一致。
        if hooks.is_empty() {
            tracing::info!(
                target: "terminal_logger",
                log_base = %log_base.display(),
                "终端交互日志启动：enabled_shells 为空，本次未安装任何会话钩子"
            );
        } else {
            let attempt = hooks.clone();
            let install_dir = log_base.clone();
            tokio::task::spawn_blocking(move || attempt.install_all(&install_dir))
                .await
                .map_err(|err| -> ModuleError {
                    format!("钩子安装任务异常终止: {err}").into()
                })??;
        }

        // 4) 落定状态：快照钩子集（供 stop 精确卸载）+ 运行标志 + cmd 核验开关。
        *self
            .inner
            .hooks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(hooks.clone());
        self.inner
            .cmd_hook_active
            .store(hooks.cmd_active(), Ordering::Release);
        self.inner.running.store(true, Ordering::Release);

        // 5) 启动日志过期清理守护：立即扫一轮过期日志，此后按固定周期（每日）
        //    定时清理，防止 `logs/terminals/` 下会话日志碎文件无限积压。
        self.spawn_retention_guard(log_base.clone(), retention_days);

        tracing::info!(
            target: "terminal_logger",
            log_base = %log_base.display(),
            enabled_shells = ?enabled_shells,
            retention_days,
            "终端交互日志已启动（会话日志将写入 '{}' 下的 powershell/ bash/ cmd/ 子目录，保留 {retention_days} 天）",
            log_base.display()
        );
        Ok(())
    }

    async fn stop(&self) -> Result<(), ModuleError> {
        let _guard = self.inner.lifecycle.lock().await;
        if !self.inner.running.load(Ordering::Acquire) {
            return Ok(()); // 未在运行：幂等短路
        }

        // 卸载「最近一次成功 start 安装的钩子」；快照缺失（异常状态）时按当前
        // 配置重建兜底，尽力清理。
        let hooks = self.current_hooks().await;
        match self.uninstall_with_retry(&hooks).await {
            Ok(()) => {
                *self
                    .inner
                    .last_stop_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            }
            Err(err) => {
                // 卸载失败（已自动重试仍失败）：部分钩子**确定残留**。归还快照供
                // 重试、保持运行标志（状态机与系统事实一致，避免「UI 显示已停止、
                // 钩子仍生效」的虚假状态），并记录残留原因供 UI 提示「需修复」
                // 与手动强制清理（v0.6.2 · M10）。
                *self
                    .inner
                    .hooks
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(hooks);
                *self
                    .inner
                    .last_stop_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(err.to_string());
                return Err(err);
            }
        }

        self.inner.cmd_hook_active.store(false, Ordering::Release);
        self.inner.running.store(false, Ordering::Release);
        // 取消日志过期清理守护（仅成功卸载后）：守护任务在下一个周期等待点退出。
        if let Some(token) = self
            .inner
            .retention_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            token.cancel();
        }
        tracing::info!(target: "terminal_logger", "终端交互日志已停止：全部会话钩子已卸载，日志清理守护已停止");
        Ok(())
    }

    /// 运行状态判定：**内存原子标志 + 注册表状态**联合判断。
    /// 1. 内存运行标志为 `false` → 停止态；
    /// 2. 内存标志为 `true` 且本次运行含 cmd 钩子（`cmd_hook_active`）→ 额外
    ///    核验 AutoRun 注册表值仍持有本模块的 `call` 片段（[`Self::registry_state_ok`]）：
    ///    片段被外部清理（用户手改注册表 / 其它工具接管 AutoRun 等）后如实
    ///    返回 `false`，UI 与调度层据此显示停止态；下一次 `start` 检测到该
    ///    不一致会自动走修复式重装；
    /// 3. 未含 cmd 钩子（仅 PowerShell / bash 文件钩子）→ 无注册表事实可
    ///    核验，内存标志即权威。
    fn is_running(&self) -> bool {
        if !self.inner.running.load(Ordering::Acquire) {
            return false;
        }
        self.registry_state_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::terminal_logger::anchor::{block_end_marker, block_start_marker};
    use crate::modules::terminal_logger::ps_bash::{BASH_TAG, PS_TAG};
    use std::path::PathBuf;

    /// 系统临时目录下本次测试独有的目录路径（并行测试互不干扰）。
    fn unique_temp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("系统时钟应晚于 UNIX 纪元")
            .as_nanos();
        std::env::temp_dir().join(format!("tltoolbox-tl-{tag}-{}-{nanos}", std::process::id()))
    }

    /// 构造一个以显式运行期配置包裹的模块（默认目录 / 空 Shell 名单，
    /// 供无副作用测试使用）。
    fn module_with(
        enabled_shells: Vec<String>,
        terminal_log_dir: Option<PathBuf>,
    ) -> TerminalLoggerModule {
        let config = AppConfig {
            terminal_log_dir,
            enabled_shells,
            ..AppConfig::default()
        };
        TerminalLoggerModule::new(Arc::new(AsyncMutex::new(config)))
    }

    // ---- 元数据契约 ----

    #[test]
    fn metadata_identity_matches_registry_contract() {
        let module = module_with(vec![], None);
        assert_eq!(module.id(), "terminal_logger");
        assert_eq!(module.display_name(), "终端交互日志");
        assert_eq!(
            module.description(),
            "自动记录 CMD、PowerShell、Bash 终端的全部输入与屏幕输出内容"
        );
        assert!(!module.is_running(), "新模块应处于停止态");
    }

    // ---- 生命周期冒烟：串行启停（零系统副作用） ----

    /// 模块生命周期串行冒烟：多轮 start / stop + 幂等重复调用，状态机每步
    /// 与目标状态一致。本测试的 `enabled_shells` 为空——不触碰任何真实用户
    /// Shell 配置文件 / 注册表，纯验证模块生命周期契约（真实文件侧钩子的
    /// 安装往返由 [`hook_manager_installs_and_uninstalls_temp_targets`] 覆盖）。
    #[tokio::test]
    async fn module_serial_start_stop_smoke_is_idempotent() {
        let module = module_with(vec![], None);
        assert!(!module.is_running());

        for cycle in 1..=3 {
            module.start().await.expect("启动应成功");
            assert!(module.is_running(), "第 {cycle} 轮启动后应处于运行态");

            module.start().await.expect("重复启动应幂等成功");
            assert!(module.is_running(), "重复启动不得破坏运行态");

            module.stop().await.expect("停止应成功");
            assert!(!module.is_running(), "第 {cycle} 轮停止后应退出运行态");

            module.stop().await.expect("重复停止应幂等成功");
            assert!(!module.is_running(), "重复停止不得破坏停止态");
        }
    }

    // ---- 日志过期清理守护（v0.3.1 加固）----

    /// 清理守护随模块生命周期启停：`start` 后守护令牌就位（立即扫一轮 +
    /// 每日定时），`stop` 后取消并清空；多轮启停不遗留旧守护任务。
    /// （实际的文件过期删除语义由 `retention` 模块的单测覆盖，此处只验证
    /// 守护任务的装配 / 取消接线。）
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retention_guard_follows_module_lifecycle() {
        // enabled_shells 为空：不触碰真实 Shell 配置 / 注册表，仅验证守护接线。
        let module = module_with(vec![], Some(PathBuf::from("unused-retention-dir")));

        for cycle in 1..=2 {
            module.start().await.expect("启动应成功");
            assert!(
                module.inner.retention_guard.lock().unwrap().is_some(),
                "第 {cycle} 轮：启动后日志清理守护应就位"
            );
            assert!(module.is_running());

            module.stop().await.expect("停止应成功");
            assert!(
                module.inner.retention_guard.lock().unwrap().is_none(),
                "第 {cycle} 轮：停止后日志清理守护应已取消"
            );
            assert!(!module.is_running());
        }
    }

    /// 修复式重装（运行中再次 `start`）不得产生双守护任务：令牌被整体替换，
    /// 旧守护在下一个周期点退出。多次重复 `start` 后仅保留一枚活跃令牌。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repeated_start_replaces_retention_guard_without_duplication() {
        let module = module_with(vec![], None);

        module.start().await.expect("首次启动应成功");
        for _ in 0..3 {
            // 运行中重复 start：幂等短路（running && registry_state_ok）……
            module.start().await.expect("重复启动应幂等成功");
            // ……不新增守护任务（令牌仍是同一枚）。
            module
                .inner
                .retention_guard
                .lock()
                .unwrap()
                .as_ref()
                .expect("守护令牌应保持就位");
        }
        module.stop().await.expect("停止应成功");
        assert!(module.inner.retention_guard.lock().unwrap().is_none());
    }

    // ---- 调度器过滤与装配 ----

    #[test]
    fn hook_manager_dispatch_respects_enabled_shells() {
        // 仅启用 cmd：只装配 CmdHook（构造仅读 current_exe，无任何系统副作用）。
        let only_cmd = HookManager::for_enabled_shells(&[SHELL_CMD.to_string()]);
        assert!(only_cmd.cmd.is_some(), "cmd 应在调度范围内");
        assert!(only_cmd.powershell.is_none());
        assert!(only_cmd.bash.is_none());
        assert!(only_cmd.cmd_active());

        // 名单为空 → 无任何钩子。
        assert!(HookManager::for_enabled_shells(&[]).is_empty());

        // 未知 Shell（wt / fish 等）→ 全部剔除，不影响整体可用性。
        let unknown = HookManager::for_enabled_shells(&["fish".into(), "wt".into()]);
        assert!(unknown.is_empty());
    }

    // ---- 目录安全创建（effective_log_dir 语义：首次写入前保证目录存在） ----

    /// 预建语义：根目录恒建、仅已装配 Shell 的子目录被创建、无挂载器时零创建。
    /// 全程使用临时目录 + `Hook::at` 显式目标，零真实环境副作用。
    #[test]
    fn ensure_log_dirs_creates_root_and_active_shell_subdirs_only() {
        let root = unique_temp("ensure-dirs");
        let log_base = root.join("logs").join("terminals");
        assert!(!log_base.exists(), "前置条件：日志根目录尚不存在");

        // 仅装配 PowerShell + bash（无 cmd）→ 只建这两个子目录。
        let hooks = HookManager {
            powershell: Some(PowerShellHook::at(root.join("profile.ps1"))),
            bash: Some(BashHook::at(root.join(".bashrc"))),
            cmd: None,
        };
        let created = ensure_log_dirs(&log_base, &hooks);
        assert!(log_base.is_dir(), "有效日志根目录应被创建");
        assert!(
            log_base.join("powershell").is_dir(),
            "powershell 子目录应被创建"
        );
        assert!(log_base.join("bash").is_dir(), "bash 子目录应被创建");
        assert!(
            !log_base.join("cmd").exists(),
            "未装配的 Shell 不应产生空子目录"
        );
        assert_eq!(created.len(), 3, "根目录 + 两个已装配 Shell 子目录");

        // 重复调用幂等（已存在目录不报错、结果一致）。
        assert_eq!(ensure_log_dirs(&log_base, &hooks).len(), 3);

        // 无任何挂载器（enabled_shells 为空）→ 零创建，也不产生根目录。
        let bare_root = root.join("bare");
        let nothing = ensure_log_dirs(&bare_root, &HookManager::for_enabled_shells(&[]));
        assert!(nothing.is_empty(), "无写入方时不应创建任何目录");
        assert!(!bare_root.exists(), "空名单下连根目录也不应被创建");

        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- 文件侧钩子的安装 / 卸载往返（显式临时路径，零真实环境副作用） ----

    /// PowerShell / bash 钩子经 HookManager 统装的真实文件往返：
    /// install_all 创建两份配置并注入区块 → 重复 install_all 幂等 →
    /// uninstall_all 还原（整文件即区块者删除文件）→ 重复卸载空操作。
    #[tokio::test]
    async fn hook_manager_installs_and_uninstalls_temp_targets() {
        let root = unique_temp("mgr-roundtrip");
        let profile = root.join("ps").join("Microsoft.PowerShell_profile.ps1");
        let bashrc = root.join("bash").join(".bashrc");
        let log_base = root.join("logs").join("terminals");

        let hooks = HookManager {
            powershell: Some(PowerShellHook::at(&profile)),
            bash: Some(BashHook::at(&bashrc)),
            cmd: None,
        };

        // 1) 安装：两份目标文件被创建并注入对应 TAG 区块。
        hooks.install_all(&log_base).expect("安装应成功");
        assert!(profile.exists(), "PowerShell 配置文件应被创建");
        assert!(bashrc.exists(), ".bashrc 应被创建");
        let ps_once = std::fs::read_to_string(&profile).unwrap();
        let bash_once = std::fs::read_to_string(&bashrc).unwrap();
        assert!(ps_once.contains(&block_start_marker(PS_TAG)));
        assert!(ps_once.contains(&block_end_marker(PS_TAG)));
        assert!(ps_once.contains("Start-Transcript"));
        assert!(bash_once.contains(&block_start_marker(BASH_TAG)));
        assert!(
            bash_once.contains("PROMPT_COMMAND"),
            "bash 钩子应为 PROMPT_COMMAND + history 方案"
        );

        // 2) 幂等：同参数重复安装逐字节不变。
        hooks.install_all(&log_base).expect("重复安装应幂等成功");
        assert_eq!(
            std::fs::read_to_string(&profile).unwrap(),
            ps_once,
            "重复安装不得改动已注入内容"
        );
        assert_eq!(
            std::fs::read_to_string(&bashrc).unwrap(),
            bash_once,
            "重复安装不得改动已注入内容"
        );

        // 3) 卸载：整文件即区块 → 删除文件（还原安装前的「不存在」）。
        hooks.uninstall_all().expect("卸载应成功");
        assert!(!profile.exists(), "整文件即本区块时卸载应删除文件");
        assert!(!bashrc.exists(), "整文件即本区块时卸载应删除文件");

        // 4) 已卸载状态下再次卸载为空操作。
        hooks.uninstall_all().expect("重复卸载应为空操作");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 安装失败回滚：bash 目标文件为不可解码的 UTF-16（解码层明确报错）时，
    /// install_all 必须先成功安装 PowerShell 钩子、再在 bash 一步失败，并
    /// 回滚已安装的 PowerShell 钩子——调用方得到错误且系统侧无残留。
    #[tokio::test]
    async fn install_all_rolls_back_earlier_hooks_on_failure() {
        let root = unique_temp("mgr-rollback");
        let profile = root.join("profile.ps1");
        let bashrc = root.join(".bashrc");
        let log_base = root.join("logs").join("terminals");

        // 预置一份 UTF-16LE（带 BOM）的 .bashrc：bash 档案按严格 UTF-8 读入，
        // 解码必然失败（与 ps_bash 模块既有测试同一失败注入路径）。
        std::fs::create_dir_all(&root).unwrap();
        let user_text = "# 用户内容\r\nWrite-Host 'é'\r\n";
        let mut original_bytes = vec![0xFF, 0xFE];
        for unit in user_text.encode_utf16() {
            original_bytes.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(&bashrc, &original_bytes).unwrap();

        let hooks = HookManager {
            powershell: Some(PowerShellHook::at(&profile)),
            bash: Some(BashHook::at(&bashrc)),
            cmd: None,
        };

        let err = hooks
            .install_all(&log_base)
            .expect_err("bash 解码失败应使 install_all 报错");
        assert!(
            err.to_string().contains("bash"),
            "错误应点名失败的 Shell: {err}"
        );
        assert!(
            !profile.exists(),
            "回滚应卸载先前成功安装的 PowerShell 钩子（文件被删除）"
        );
        assert_eq!(
            std::fs::read(&bashrc).unwrap(),
            original_bytes,
            "失败目标文件不得被触碰"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- M10：卸载重试 / 残留状态 / 强制清理（v0.6.2 整改） ----

    /// 从未启动的模块：无残留、无需修复，强制清理为幂等空操作。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn force_cleanup_on_never_started_module_is_noop() {
        let module = module_with(Vec::new(), None);
        assert!(!module.needs_repair(), "从未启动的模块不应报告「需修复」");
        assert!(module.last_stop_error().is_none());
        module
            .force_cleanup_hooks()
            .await
            .expect("无残留时强制清理应成功");
        assert!(!module.needs_repair(), "清理后仍不应报告「需修复」");
        assert!(module.last_stop_error().is_none());
    }

    /// 空残留状态下强制清理的幂等性：连续两次均成功且不产生任何状态翻转。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn force_cleanup_is_idempotent() {
        let module = module_with(vec!["powershell".to_string(), "bash".to_string()], None);
        for round in 1..=2 {
            module
                .force_cleanup_hooks()
                .await
                .unwrap_or_else(|err| panic!("第 {round} 次强制清理应成功: {err}"));
            assert!(!module.needs_repair(), "第 {round} 次清理后不应报告需修复");
            // 强制清理是幂等的清理动作，**不得**翻转运行标志。
            assert!(!module.is_running(), "强制清理不得把模块置为运行态");
        }
    }

    /// 重试常量必须有界（避免卸载失败时长时间挂住 stop 路径）。
    #[test]
    fn uninstall_retry_constants_are_bounded() {
        assert!(
            (2..=5).contains(&UNINSTALL_MAX_ATTEMPTS),
            "重试次数过少失去意义、过多拖慢停止: {UNINSTALL_MAX_ATTEMPTS}"
        );
        assert!(
            UNINSTALL_RETRY_BACKOFF <= std::time::Duration::from_millis(500),
            "退避过长会明显拖慢模块停止"
        );
    }
}
