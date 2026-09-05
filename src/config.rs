//! # 配置引擎：Serde TOML 持久化（阶段二 · 架构转型后）
//!
//! 应用级配置以结构化 TOML 落盘（高可读性、可手改、可纳入版本管理），由
//! [`ConfigManager`] 提供**异步**的读取与写入能力：
//!
//! - 原子写盘：先写同目录临时文件再 `rename` 覆盖，杜绝进程崩溃 / 断电导致的半截文件；
//! - 写入串行化：内部写锁保证并发 `save` 不互相穿插（仍为 last-write-wins 语义）；
//! - 字段级向前兼容：旧版配置缺少新增字段时按各字段默认值补齐，而不是整体解析失败；
//! - 坏配置保护：TOML 解析失败时返回 [`ConfigError::Parse`]，**绝不**静默覆盖用户数据。
//!
//! # 常驻行为与注册表同步（架构转型后）
//!
//! [`AppConfig`] 的 `auto_start_windows` 是“是否跟随系统开机自启”的**唯一事实源**。
//! 本模块保持纯净——`load` / `save` 只读写 TOML 文件，不触碰注册表；实际的自启
//! 镜像由 [`crate::autostart`] 完成：装配层在 **`ConfigManager::load()` 返回后
//! 立即**调用 [`crate::autostart::synchronize_autostart`]，把配置字段与
//! `HKCU\...\CurrentVersion\Run` 收敛（配置开启而注册表缺失/路径漂移 → 补写；
//! 配置关闭而注册表残留 → 删除；已一致 → 空操作）。同步失败仅告警降级，
//! 不阻断启动（“优雅同步”）。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::sync::Mutex;

/// 默认配置文件相对路径（相对**可执行文件所在目录**，见 [`resolve_app_path`]）。
pub const DEFAULT_CONFIG_PATH: &str = "config/tltoolbox.toml";

/// 终端日志目录的默认相对路径（相对 exe 同级目录；`terminal_log_dir` 缺省时使用）。
pub const DEFAULT_TERMINAL_LOG_DIR: &str = "logs/terminals";

/// 把应用资源相对路径解析为「以可执行文件目录为基准」的绝对路径。
///
/// # 背景（路径锚定的必要性）
/// 经 Windows 注册表 Run 键自启 / 用户双击拉起时，进程工作目录（CWD）可能落在
/// `C:\Windows\System32` 或任意目录；若直接使用相对路径 `config/tltoolbox.toml`
/// 读写配置，将指向错误位置、甚至因 `System32` 无写权限而落盘失败。因此应用
/// 资源的基准目录统一取 `std::env::current_exe()` 所在目录（exe 同级），与 CWD
/// 彻底解耦——无论从何处启动，配置始终锚定在程序自己的安装目录旁。
///
/// # 解析规则
/// - `sub_path` 本身为绝对路径 → 原样返回（显式绝对路径优先，不改写调用方意图）；
/// - 否则优先取 `current_exe()` 的父目录作为基准拼接；
/// - `current_exe()` 不可用（极罕见，如句柄异常）→ 兜底原样返回相对路径，
///   退化为「相对 CWD」的历史行为，保证调用方总能拿到一个可用路径。
///
/// # 测试自由度
/// 本函数只影响 [`ConfigManager::default`] 等**默认**入口；需要相对路径 / 临时
/// 路径的单元测试一律经 [`ConfigManager::new`] 显式指定路径，锚定逻辑不干扰测试。
pub fn resolve_app_path(sub_path: &Path) -> PathBuf {
    if sub_path.is_absolute() {
        return sub_path.to_path_buf();
    }
    match std::env::current_exe() {
        Ok(exe) => exe
            .parent()
            .map(|exe_dir| exe_dir.join(sub_path))
            .unwrap_or_else(|| sub_path.to_path_buf()),
        Err(_) => sub_path.to_path_buf(),
    }
}

/// 应用级配置结构。
///
/// 每个字段均带 `#[serde(default = ...)]`：解析时若文件中缺失该键，将回退到
/// 对应的默认值而非报错，使旧版本配置文件在新增配置项后仍可无缝加载。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    /// 应用启动后应自动拉起（`toggle(id, true)`）的模块 ID 列表，按序启动。
    #[serde(default = "AppConfig::default_auto_start_modules")]
    pub auto_start_modules: Vec<String>,
    /// 弹窗拦截黑名单关键词（弹窗拦截模块的唯一事实源）。
    ///
    /// 装配层（`crate::main`）在模块注册时将其注入
    /// [`PopupBlockerModule`](crate::modules::popup_blocker::PopupBlockerModule)；
    /// 匹配语义为「窗口标题 / 类名的子串匹配、忽略大小写」。条目会先经模块侧
    /// 归一化（去首尾空白、剔除空串、去重）再参与判定。运行期改写本字段并调用
    /// 模块的 `update_rules` 即可热更新，无需重启原生消息泵线程。
    #[serde(default = "AppConfig::default_popup_blacklist")]
    pub popup_blacklist: Vec<String>,
    /// 是否跟随系统开机自启（写入注册表 Run 键，见 [`crate::autostart`]）。
    ///
    /// 配置为准：启动装配层在配置加载后据此同步注册表实际状态。
    #[serde(default = "AppConfig::default_auto_start_windows")]
    pub auto_start_windows: bool,
    /// 点击窗口关闭按钮时是否仅最小化到托盘而非退出进程（常驻行为；托盘
    /// 生命周期由后续常驻层消费本字段）。
    #[serde(default = "AppConfig::default_minimize_to_tray")]
    pub minimize_to_tray: bool,
    /// 终端交互日志的落盘目录（终端日志模块配置；`None` → 消费方回退为
    /// exe 同级 `logs/terminals`，见 [`DEFAULT_TERMINAL_LOG_DIR`]）。
    ///
    /// `None`（缺省 / 旧配置无此键）即采用默认布局；显式给出时按
    /// [`resolve_app_path`] 语义锚定：绝对路径原样使用，相对路径以 exe 同级
    /// 目录为基准拼接。消费方必须经 [`AppConfig::effective_terminal_log_dir`]
    /// 取最终目录，禁止直接读取本字段绕开回退逻辑。
    #[serde(
        default = "AppConfig::default_terminal_log_dir",
        skip_serializing_if = "Option::is_none"
    )]
    pub terminal_log_dir: Option<PathBuf>,
    /// 启用终端会话日志的 Shell 名单（模块 ID → 仅向本名单内的 Shell 注入钩子；
    /// 实际注入目标与实现策略由终端日志模块后续阶段决定）。
    ///
    /// 默认启用传统三件套：`powershell` / `cmd` / `bash`。
    #[serde(default = "AppConfig::default_enabled_shells")]
    pub enabled_shells: Vec<String>,
    /// 各模块的自定义参数（模块 ID → 配置值），供模块级扩展配置使用。
    #[serde(default)]
    pub module_custom_params: HashMap<String, String>,
}

impl AppConfig {
    fn default_auto_start_modules() -> Vec<String> {
        vec!["popup_blocker".to_string()]
    }

    /// 默认弹窗拦截黑名单：保留传统内置的常见广告 / 流氓进程窗口关键词。
    fn default_popup_blacklist() -> Vec<String> {
        vec![
            "广告".to_string(),
            "Flash Helper Service".to_string(),
            "Update Notice".to_string(),
            "推广弹窗".to_string(),
        ]
    }

    fn default_auto_start_windows() -> bool {
        false
    }

    fn default_minimize_to_tray() -> bool {
        true
    }

    fn default_terminal_log_dir() -> Option<PathBuf> {
        None
    }

    fn default_enabled_shells() -> Vec<String> {
        vec!["powershell".into(), "cmd".into(), "bash".into()]
    }

    /// 终端日志目录的**有效路径**（消费方唯一入口）。
    ///
    /// `terminal_log_dir` 为 `None`（缺省 / 旧配置）时回退到可执行文件同级目录
    /// 下的 `logs/terminals`（与配置 `config/`、应用日志 `logs/` 同锚定基准，
    /// 见 [`resolve_app_path`] 与 [`DEFAULT_TERMINAL_LOG_DIR`]）；显式给出时按
    /// [`resolve_app_path`] 语义处理绝对 / 相对路径（相对路径同样锚定 exe 目录，
    /// 与 CWD 解耦，自启 / 双击拉起均指向同一目录）。
    pub fn effective_terminal_log_dir(&self) -> PathBuf {
        match &self.terminal_log_dir {
            Some(dir) => resolve_app_path(dir),
            None => resolve_app_path(Path::new(DEFAULT_TERMINAL_LOG_DIR)),
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            auto_start_modules: Self::default_auto_start_modules(),
            popup_blacklist: Self::default_popup_blacklist(),
            auto_start_windows: Self::default_auto_start_windows(),
            minimize_to_tray: Self::default_minimize_to_tray(),
            terminal_log_dir: Self::default_terminal_log_dir(),
            enabled_shells: Self::default_enabled_shells(),
            module_custom_params: HashMap::new(),
        }
    }
}

/// 配置读写错误（携带失败路径与底层原因，便于 UI / 日志直接展示）。
#[derive(Debug)]
pub enum ConfigError {
    /// 文件系统层错误（读取 / 建目录 / 写临时文件 / 原子替换失败等）。
    Io {
        /// 触发错误的文件路径。
        path: PathBuf,
        /// 底层 IO 错误。
        source: std::io::Error,
    },
    /// 配置内容不是合法 TOML，或与 `AppConfig` 结构不兼容。
    Parse {
        /// 被解析的配置文件路径。
        path: PathBuf,
        /// 底层 TOML 解析错误。
        source: toml::de::Error,
    },
    /// 配置序列化为 TOML 失败（理论上仅当出现非字符串键等极端情况）。
    Serialize {
        /// 正在写入的配置文件路径。
        path: PathBuf,
        /// 底层 TOML 序列化错误。
        source: toml::ser::Error,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io { path, source } => {
                write!(f, "配置文件 '{}' 访问失败: {source}", path.display())
            }
            ConfigError::Parse { path, source } => {
                write!(f, "配置文件 '{}' 解析失败: {source}", path.display())
            }
            ConfigError::Serialize { path, source } => {
                write!(f, "配置序列化失败（目标 '{}'）: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io { source, .. } => Some(source),
            ConfigError::Parse { source, .. } => Some(source),
            ConfigError::Serialize { source, .. } => Some(source),
        }
    }
}

/// 配置管理器：负责单个配置文件的异步加载与原子持久化。
///
/// 全部方法均为 `&self` 异步调用，内部状态仅含目标路径与写锁，可在任务间以
/// `Arc<ConfigManager>` 安全共享。
pub struct ConfigManager {
    /// 配置文件路径。
    file_path: PathBuf,
    /// 串行化并发 `save` 的写锁（防止两份写入互相穿插产生撕裂文件）。
    write_lock: Mutex<()>,
}

impl ConfigManager {
    /// 指向指定路径构造配置管理器。
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        Self {
            file_path: path.as_ref().to_path_buf(),
            write_lock: Mutex::new(()),
        }
    }

    /// 当前配置文件路径（供日志与 UI 展示）。
    pub fn path(&self) -> &Path {
        &self.file_path
    }

    /// 异步加载配置。
    ///
    /// 语义约定：
    /// - 文件**不存在**（首次运行）→ 生成并落盘默认配置后返回之（启动引导）；
    /// - 文件**存在但解析失败** → 返回 [`ConfigError::Parse`]，保留原文件内容不动，
    ///   由调用方决定是上报错误还是回退内存默认值；
    /// - 其他 IO 错误 → 原样上抛。
    ///
    /// 注意：本方法只读 TOML 文件。若调用方需要“加载后同步注册表自启状态”，
    /// 应在返回值到手后调用 [`crate::autostart::synchronize_autostart`]
    /// （见模块文档与 `crate::main` 的装配示例）。
    pub async fn load(&self) -> Result<AppConfig, ConfigError> {
        match fs::read_to_string(&self.file_path).await {
            Ok(content) => toml::from_str(&content).map_err(|source| ConfigError::Parse {
                path: self.file_path.clone(),
                source,
            }),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                // 首次运行引导：落盘默认配置。落盘失败不阻断启动——以内存默认值继续，
                // 等待后续某次 save 成功时再补写。
                let default_cfg = AppConfig::default();
                if let Err(save_err) = self.save(&default_cfg).await {
                    tracing::warn!(
                        target: "config",
                        "无法自动创建默认配置文件 '{}': {save_err}（本次以内存默认值运行）",
                        self.file_path.display()
                    );
                }
                Ok(default_cfg)
            }
            Err(source) => Err(ConfigError::Io {
                path: self.file_path.clone(),
                source,
            }),
        }
    }

    /// 异步保存配置：临时文件 + 原子替换，保证任意时刻磁盘上都存在一份完整配置。
    ///
    /// 写锁仅在本次写入期间持有，且不存在嵌套加锁，无死锁隐患。
    pub async fn save(&self, config: &AppConfig) -> Result<(), ConfigError> {
        let _write_guard = self.write_lock.lock().await;

        let content = toml::to_string_pretty(config).map_err(|source| ConfigError::Serialize {
            path: self.file_path.clone(),
            source,
        })?;

        // 确保父目录存在（默认路径为 exe 同级目录下的 `config/tltoolbox.toml`，
        // 首次写入时 config 目录尚未创建；经 resolve_app_path 锚定后此目录必然可写）。
        if let Some(parent) = self.file_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .await
                    .map_err(|source| ConfigError::Io {
                        path: parent.to_path_buf(),
                        source,
                    })?;
            }
        }

        // 1) 写入同目录临时文件（保证与目标文件处于同一文件系统，rename 才可能原子）。
        let tmp_path = self.tmp_path();
        if let Err(source) = fs::write(&tmp_path, content.as_bytes()).await {
            return Err(ConfigError::Io {
                path: tmp_path,
                source,
            });
        }

        // 2) 原子替换目标文件。失败时尽力清理临时文件，避免残留。
        if let Err(source) = fs::rename(&tmp_path, &self.file_path).await {
            let _ = fs::remove_file(&tmp_path).await;
            return Err(ConfigError::Io {
                path: self.file_path.clone(),
                source,
            });
        }

        tracing::debug!(
            target: "config",
            "配置已落盘: '{}'",
            self.file_path.display()
        );
        Ok(())
    }

    /// 与目标文件同目录的临时文件路径（`<文件名>.tmp`）。
    fn tmp_path(&self) -> PathBuf {
        let file_name = self
            .file_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.file_path.with_file_name(format!("{file_name}.tmp"))
    }
}

impl Default for ConfigManager {
    /// 指向**可执行文件同级目录**下的默认配置 `config/tltoolbox.toml`。
    ///
    /// 经 [`resolve_app_path`] 把默认相对路径锚定到 exe 目录：注册表 Run 键自启
    /// （CWD = `System32`）、资源管理器双击等任意工作目录下，都能稳定定位到
    /// 安装目录旁的 `config/tltoolbox.toml` 并具备写入权限。
    fn default() -> Self {
        Self::new(resolve_app_path(Path::new(DEFAULT_CONFIG_PATH)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在系统临时目录构造一个本次测试独有的配置文件路径。
    fn temp_cfg_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("系统时钟应晚于 UNIX 纪元")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "tltoolbox-{tag}-{}-{nanos}.toml",
            std::process::id()
        ))
    }

    async fn remove_if_exists(path: &Path) {
        let _ = fs::remove_file(path).await;
    }

    // ---- 路径锚定（resolve_app_path / default()）：系统级隐患「Run 键自启时
    //      CWD 为 System32」的回归防线 ----

    #[test]
    fn resolve_app_path_keeps_explicit_absolute_path() {
        // 绝对路径必须原样返回（显式指定优先，不得被重定向到 exe 目录）。
        let abs = std::env::temp_dir().join("tltoolbox-explicit-absolute.toml");
        assert_eq!(
            resolve_app_path(&abs),
            abs,
            "绝对路径应原样返回而不做 exe 目录拼接"
        );
    }

    #[test]
    fn resolve_app_path_anchors_relative_subpath_to_exe_dir() {
        let resolved = resolve_app_path(Path::new(DEFAULT_CONFIG_PATH));
        assert!(
            resolved.is_absolute(),
            "exe 目录锚定应产出绝对路径，实际: {}",
            resolved.display()
        );
        assert!(
            resolved.ends_with(Path::new(DEFAULT_CONFIG_PATH)),
            "路径应以 config/tltoolbox.toml 收尾，实际: {}",
            resolved.display()
        );

        // 锚定基准必须是「当前进程可执行文件目录」，而非进程工作目录（CWD）——
        // 这正是注册表 Run 键自启场景（CWD = System32）下避免写错位置的关键。
        let exe_path = std::env::current_exe().expect("current_exe 应可用");
        let exe_dir = exe_path.parent().expect("exe 必有父目录");
        let expected = exe_dir.join("config");
        assert_eq!(
            resolved.parent().map(|p| p.to_path_buf()),
            Some(expected),
            "基准目录应为 exe 同级目录下的 config/，实际: {}",
            resolved.display()
        );
    }

    #[test]
    fn default_config_manager_anchors_to_exe_dir() {
        // ConfigManager::default() 是 main.rs 的装配入口：其路径必须锚定 exe 目录，
        // 保证自启 / 双击等任意 CWD 下首次运行都能自动落盘并读回同一份配置。
        let mgr = ConfigManager::default();
        assert!(
            mgr.path().is_absolute(),
            "默认配置路径应为绝对路径（exe 目录锚定），实际: {}",
            mgr.path().display()
        );
        assert!(
            mgr.path().ends_with(Path::new(DEFAULT_CONFIG_PATH)),
            "默认配置路径应指向 config/tltoolbox.toml，实际: {}",
            mgr.path().display()
        );
    }

    // ---- 配置内容与持久化语义 ----

    #[test]
    fn default_config_has_expected_values() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.auto_start_modules, vec!["popup_blocker".to_string()]);
        assert!(
            !cfg.auto_start_windows,
            "开机自启默认应为关闭（显式开启才写注册表）"
        );
        assert!(
            cfg.minimize_to_tray,
            "关闭按钮最小化到托盘默认应开启（桌面常驻定位）"
        );
        assert!(cfg.module_custom_params.is_empty());

        // 弹窗黑名单默认值 = 传统内置的常见广告 / 流氓窗口关键词。
        let expected: Vec<String> = ["广告", "Flash Helper Service", "Update Notice", "推广弹窗"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            cfg.popup_blacklist, expected,
            "黑名单默认值应保留原有关键词"
        );

        // 终端日志子系统（基础层）默认值：日志目录不显式指定（消费方回退
        // exe 同级 logs/terminals），Shell 名单 = 传统三件套。
        assert_eq!(
            cfg.enabled_shells,
            vec![
                "powershell".to_string(),
                "cmd".to_string(),
                "bash".to_string()
            ],
            "默认启用 Shell 名单应为 powershell / cmd / bash"
        );
        assert_eq!(
            cfg.terminal_log_dir, None,
            "终端日志目录默认不应显式指定（None → exe 同级 logs/terminals）"
        );
    }

    #[test]
    fn terminal_log_dir_default_falls_back_to_exe_adjacent_logs() {
        // None 时 effective_terminal_log_dir 必须落在 exe 同级 logs/terminals，
        // 与配置文件（exe 同级 config/）保持同一锚定基准。
        let cfg = AppConfig::default();
        assert_eq!(cfg.terminal_log_dir, None, "前置条件：默认无显式目录");
        let effective = cfg.effective_terminal_log_dir();
        assert!(
            effective.is_absolute(),
            "回退目录应为绝对路径（exe 锚定），实际: {}",
            effective.display()
        );
        assert!(
            effective.ends_with(Path::new(DEFAULT_TERMINAL_LOG_DIR)),
            "回退目录应以 logs/terminals 收尾，实际: {}",
            effective.display()
        );
    }

    #[test]
    fn explicit_terminal_log_dir_resolves_absolute_and_relative() {
        // 显式绝对路径：原样使用（不改写调用方意图）。
        let mut cfg = AppConfig::default();
        let abs = std::env::temp_dir().join("tltoolbox-terminal-logs");
        cfg.terminal_log_dir = Some(abs.clone());
        assert_eq!(
            cfg.effective_terminal_log_dir(),
            abs,
            "显式绝对路径应原样返回"
        );

        // 显式相对路径：按 resolve_app_path 语义锚定到 exe 目录（与 CWD 解耦）。
        cfg.terminal_log_dir = Some(PathBuf::from("data/terminal-logs"));
        let effective = cfg.effective_terminal_log_dir();
        assert!(
            effective.is_absolute(),
            "相对路径应被锚定为绝对路径，实际: {}",
            effective.display()
        );
        assert!(
            effective.ends_with(Path::new("data").join("terminal-logs")),
            "锚定结果应以 data/terminal-logs 收尾，实际: {}",
            effective.display()
        );
    }

    #[tokio::test]
    async fn save_then_load_roundtrips() {
        let path = temp_cfg_path("roundtrip");
        remove_if_exists(&path).await;

        let mgr = ConfigManager::new(&path);
        let mut cfg = AppConfig::default();
        cfg.auto_start_modules = vec!["popup_blocker".into(), "fake_module".into()];
        cfg.auto_start_windows = true;
        cfg.minimize_to_tray = false;
        cfg.popup_blacklist = vec!["弹窗测试关键词".into(), "Popup Test Ad".into()];
        // 终端日志字段显式给出（含 Some 目录的序列化 / 反序列化路径）。
        cfg.terminal_log_dir = Some(PathBuf::from("terminal-log-roundtrip"));
        cfg.enabled_shells = vec!["powershell".into(), "wt".into()];
        cfg.module_custom_params
            .insert("popup_blocker".into(), "aggressive".into());

        mgr.save(&cfg).await.expect("保存应成功");
        let loaded = mgr.load().await.expect("加载应成功");
        assert_eq!(loaded, cfg, "往返读写应保持一致");

        remove_if_exists(&path).await;
    }

    #[tokio::test]
    async fn load_on_missing_file_bootstraps_default_config() {
        let path = temp_cfg_path("bootstrap");
        remove_if_exists(&path).await;

        let mgr = ConfigManager::new(&path);
        let cfg = mgr.load().await.expect("文件缺失时应引导默认配置而非报错");
        assert_eq!(cfg, AppConfig::default());
        assert!(path.exists(), "引导后默认配置应已落盘");

        remove_if_exists(&path).await;
    }

    #[tokio::test]
    async fn malformed_toml_returns_parse_error_and_keeps_file() {
        let path = temp_cfg_path("malformed");
        remove_if_exists(&path).await;
        fs::write(&path, "这不是合法 TOML = [").await.unwrap();
        let original = fs::read_to_string(&path).await.unwrap();

        let mgr = ConfigManager::new(&path);
        let err = mgr.load().await.expect_err("坏配置应返回 Parse 错误");
        assert!(
            matches!(err, ConfigError::Parse { .. }),
            "应为 Parse 变体，实际: {err:?}"
        );

        // 坏文件必须原样保留，禁止静默覆盖。
        let after = fs::read_to_string(&path).await.unwrap();
        assert_eq!(after, original, "解析失败时不得改动用户文件");

        remove_if_exists(&path).await;
    }

    #[tokio::test]
    async fn partial_toml_fills_missing_fields_with_defaults() {
        let path = temp_cfg_path("partial");
        remove_if_exists(&path).await;
        // 精简配置只声明 auto_start_modules：常驻字段应回退默认值，解析不得失败。
        fs::write(&path, "auto_start_modules = [\"popup_blocker\"]\n")
            .await
            .unwrap();

        let mgr = ConfigManager::new(&path);
        let cfg = mgr.load().await.expect("缺字段应回退默认值而非报错");
        assert_eq!(cfg.auto_start_modules, vec!["popup_blocker".to_string()]);
        assert_eq!(
            cfg.popup_blacklist,
            AppConfig::default().popup_blacklist,
            "黑名单缺省时应回退默认关键词"
        );
        assert_eq!(
            cfg.auto_start_windows,
            AppConfig::default().auto_start_windows
        );
        assert_eq!(cfg.minimize_to_tray, AppConfig::default().minimize_to_tray);
        assert!(cfg.module_custom_params.is_empty());
        assert_eq!(
            cfg.enabled_shells,
            AppConfig::default().enabled_shells,
            "缺省时 Shell 名单应回退默认三件套"
        );
        assert_eq!(
            cfg.terminal_log_dir, None,
            "缺省时日志目录应为 None（消费方回退 exe 同级 logs/terminals）"
        );

        remove_if_exists(&path).await;
    }

    #[tokio::test]
    async fn terminal_log_fields_parse_when_present() {
        let path = temp_cfg_path("terminal-log");
        remove_if_exists(&path).await;
        // 显式声明终端日志字段（相对目录 + 自定义 Shell 名单）：应原样解析。
        fs::write(
            &path,
            "terminal_log_dir = \"data/terminal-logs\"\n\
             enabled_shells = [\"powershell\", \"wsl\"]\n",
        )
        .await
        .unwrap();

        let mgr = ConfigManager::new(&path);
        let cfg = mgr.load().await.expect("含终端日志字段的配置应正常解析");
        assert_eq!(
            cfg.terminal_log_dir,
            Some(PathBuf::from("data/terminal-logs")),
            "显式目录应原样读入（相对路径由 effective 访问器消费时再锚定）"
        );
        assert_eq!(
            cfg.enabled_shells,
            vec!["powershell".to_string(), "wsl".to_string()],
            "自定义 Shell 名单应逐字读入"
        );
        // 其余键缺省仍回退默认值，互不影响。
        assert_eq!(
            cfg.auto_start_modules,
            AppConfig::default().auto_start_modules
        );

        remove_if_exists(&path).await;
    }

    #[tokio::test]
    async fn popup_blacklist_field_parses_and_roundtrips() {
        let path = temp_cfg_path("blacklist");
        remove_if_exists(&path).await;

        let mgr = ConfigManager::new(&path);
        let mut cfg = AppConfig::default();
        cfg.popup_blacklist = vec![
            "购物返利".into(),
            "Flash Helper Service".into(),
            "Update Notice".into(),
        ];
        mgr.save(&cfg).await.expect("保存应成功");

        let loaded = mgr.load().await.expect("加载应成功");
        assert_eq!(
            loaded.popup_blacklist, cfg.popup_blacklist,
            "黑名单应逐字往返一致"
        );

        remove_if_exists(&path).await;
    }

    #[tokio::test]
    async fn legacy_agent_config_with_llm_keys_is_tolerated_and_dropped_on_save() {
        let path = temp_cfg_path("legacy-agent");
        remove_if_exists(&path).await;
        // 旧 Agent 版配置文件含 llm 键：转型后应被容忍（未知键忽略），
        // 常驻字段回退默认值；且再次序列化时 llm 键不得复现。
        fs::write(
            &path,
            "auto_start_modules = [\"popup_blocker\"]\n\
             llm_api_base = \"http://127.0.0.1:11434/v1\"\n\
             llm_model = \"qwen2.5:7b\"\n",
        )
        .await
        .unwrap();

        let mgr = ConfigManager::new(&path);
        let cfg = mgr.load().await.expect("旧版 llm 键应被忽略而非报错");
        assert_eq!(cfg.auto_start_modules, vec!["popup_blocker".to_string()]);
        assert_eq!(
            cfg.popup_blacklist,
            AppConfig::default().popup_blacklist,
            "旧版配置缺黑名单键时应回退默认关键词"
        );
        assert_eq!(
            cfg.auto_start_windows,
            AppConfig::default().auto_start_windows
        );
        assert_eq!(cfg.minimize_to_tray, AppConfig::default().minimize_to_tray);
        assert_eq!(
            cfg.enabled_shells,
            AppConfig::default().enabled_shells,
            "旧版配置缺 Shell 名单键时应回退默认三件套"
        );
        assert_eq!(
            cfg.terminal_log_dir, None,
            "旧版配置缺日志目录键时应为 None（不阻断解析）"
        );

        let serialized = toml::to_string_pretty(&cfg).expect("重新序列化应成功");
        assert!(
            !serialized.contains("llm_api_base") && !serialized.contains("llm_model"),
            "转型后配置不得再携带 llm 字段: {serialized}"
        );
        assert!(
            !serialized.contains("terminal_log_dir"),
            "None 的日志目录键不应序列化落盘: {serialized}"
        );

        remove_if_exists(&path).await;
    }

    #[tokio::test]
    async fn unknown_keys_are_tolerated() {
        let path = temp_cfg_path("unknown");
        remove_if_exists(&path).await;
        // 结构未声明未来版本可能新增的键：应被忽略，不得整体报错。
        fs::write(
            &path,
            "auto_start_windows = true\nfuture_section = { a = 1 }\n",
        )
        .await
        .unwrap();

        let mgr = ConfigManager::new(&path);
        let cfg = mgr.load().await.expect("未知键应被容忍");
        assert!(cfg.auto_start_windows, "已声明键仍应正常解析");

        remove_if_exists(&path).await;
    }
}
