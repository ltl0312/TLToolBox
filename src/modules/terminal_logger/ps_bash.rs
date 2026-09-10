//! # PowerShell / Bash 会话日志挂载器（terminal_logger · 装配层第一弹）
//!
//! 本模块在用户的 Shell 启动脚本（Windows PowerShell 5.1 / 7.x 的 `$PROFILE`
//! 家族与 bash 的 `.bashrc` / `.bash_profile`）中，借助 [`super::anchor`] 的
//! 无损锚点引擎圈入「TLToolBox 管理区块」，使每个新建的**交互式**会话自动把
//! 命令输入、屏幕输出与执行状态沉淀为本地日志文件：
//!
//! - **PowerShell（[`PowerShellHook`]）**：向 **PowerShell 5.1 与 7.x 两套引擎**
//!   的配置目录注入转录钩子（[`detect_powershell_profiles`]，跟随 OneDrive
//!   重定向后的 Documents）——每个引擎目录下同时写入
//!   `Microsoft.PowerShell_profile.ps1`（CurrentUserCurrentHost，覆盖控制台 /
//!   VS Code 集成终端）与 `profile.ps1`（CurrentUserAllHosts，覆盖 ISE 等全部
//!   宿主）。钩子负载（[`powershell_transcript_payload`]）：
//!   1. `$env:TLTB_PS_LOGGED` 防递归（同进程双文件重复 dot-source / 派生
//!      子进程重读配置都不二次启动）；
//!   2. **惰性 `Start-Transcript`**：首条命令的提示符刷新时才启动转录（而非
//!      配置加载即启动）——非交互式 PowerShell 调用永不产生日志文件；转录
//!      必须**静默**（`| Out-Null`）：Start-Transcript 的多行启动横幅若打到
//!      控制台会破坏 PSReadLine 的行缓冲同步，造成首屏提示符需多敲一次回车
//!      （本次修复的核心）；
//!   3. **提示符刷新前记录上一条命令状态**：重写 `prompt` 函数（保存并委托
//!      既有实现，不破坏用户 / 其他工具的自定义提示符）——**先**抓上一条
//!      命令的 `$LASTEXITCODE` 与 `$?`（转录启动会改写 `$?`，必须最先抓），
//!      **再**静默启动转录，然后把状态追加进**独立的会话状态流**
//!      `<ts>_pid<PID>.state.log`（活跃的 `Start-Transcript` 独占锁定转录
//!      文件、无法同文件追加——转录文件与状态流成对存在，UTF-8 编码）；
//!      每次会话（新进程）生成独立日志
//!      `<log_base>/powershell/yyyy-MM-dd_HH-mm-ss_pid<PID>.log`。
//! - **Bash（[`BashHook`]）**：同时向 `~/.bash_profile`（登录 shell，Git Bash
//!   默认形态）与 `~/.bashrc`（交互非登录 shell）注入**命令记录钩子**。Windows
//!   的 Git Bash 通常**没有** `script(1)`，故不依赖屏幕包装，改为 bash 原生
//!   **PROMPT_COMMAND + `history`**（[`bash_transcript_payload`]）：
//!   1. `TLTB_BASH_LOGGED` 防递归（双文件只生效一次，嵌套 shell 不重复挂）；
//!   2. 仅真实交互终端（`[ -t 1 ]`）且含 `BASH_VERSION` 时装配；
//!   3. **PROMPT_COMMAND** 在每次提示符刷新前经 `history 1` 抓取上一条用户
//!      **实际键入**的命令（`history` 只记录用户输入行，不含 PROMPT_COMMAND
//!      自身——旧 DEBUG trap 方案会把 `__tltb_on_prompt` 函数名误记为命令，
//!      本次已彻底移除 DEBUG trap），以
//!      `[时间戳] [exit=…] 命令` 格式追加到
//!      `<log_base>/bash/<ts>_<pid>.log`（按历史行号去重：空回车 / 提示符
//!      重绘不重复落盘；首帧只做行号对齐，不记录 HISTFILE 预载的上一会话
//!      尾部）；`PROMPT_COMMAND` 若已被用户占用则链式保留，不覆盖既有钩子。
//!
//! # 目标文件与路径解析（多引擎 / OneDrive / 双入口）
//!
//! - PowerShell：先经 Windows 自带的 `powershell.exe`（5.1）查询
//!   `$PROFILE.CurrentUserCurrentHost`——PowerShell 自行解析**已知文件夹**，
//!   因此 Documents 被 OneDrive 重定向时仍得到真实路径；随后从该路径反推
//!   用户 Documents 根，为 **5.1（`WindowsPowerShell` 目录）与 7.x
//!   （`PowerShell` 目录）两套引擎**各生成一对目标（CurrentUserCurrentHost 的
//!   `Microsoft.PowerShell_profile.ps1` + AllHosts 的 `profile.ps1`，7.x 未安装
//!   时目录尚不存在也无妨——挂载自动创建，未来安装即生效）；探测失败（进程
//!   缺失 / 非零退出 / 输出不可解析）时回退到 `$HOME\Documents\<引擎目录>`
//!   （Windows 主目录取 `$HOME`，其次 `USERPROFILE`；MSYS / Git Bash 形态的
//!   `$HOME`（`/c/Users/…`）会被归一为盘符路径）；
//! - Bash：`$HOME/.bashrc` 与 `$HOME/.bash_profile`（Git Bash 的 `$HOME` 即
//!   Windows 用户主目录，两文件同时是登录 / 非登录交互 shell 的启动入口；
//!   登录 shell 优先读 `.bash_profile`，非登录读 `.bashrc`）。WSL 的配置位于
//!   发行版虚拟磁盘内部、无法经 Windows 侧 `$HOME` 直接定位，属已知边界
//!   （见 [`bash_posix_log_root`]）。
//!
//! # 注入脚本的路径形态
//!
//! 两个注入脚本各自内嵌一条 `log_base` 路径。PowerShell 脚本按 **Windows
//! 原生形态**（`D:\…`）嵌入单引号字符串（`'` 翻倍转义）——PowerShell 原生
//! 消费该路径。Bash 脚本运行在 MSYS / Git Bash 里，Windows 盘符路径中的
//! 反斜杠在 bash 双引号语境下存在转义歧义（`\\`、`\$(…)` 等），故注入前先由
//! [`bash_posix_log_root`] 归一为 MSYS POSIX 形态（`C:\x` → `/c/x`），再对
//! `\` `"` `$` `` ` `` 做双引号转义，保证内嵌路径在任何目录名（空格、`$`、
//! 反引号等）下都逐字可靠。
//!
//! # 编码策略（无损前提）
//!
//! 锚点引擎工作在文本行模型上，本模块负责「字节 ↔ 文本」的无损翻译：
//!
//! - **PowerShell 配置文件**按自动识别读入：UTF-8（带 / 不带 BOM）与 UTF-16LE /
//!   UTF-16BE（带 BOM，PowerShell 5.1 历史遗留的常见形态）均可处理，写回时
//!   沿用原编码（BOM 原样保留）；**新建**文件采用 UTF-8 BOM（PS 5.1 对无 BOM
//!   文件按 ANSI 解释，带 BOM 才保证按 UTF-8 解析）；
//! - **bash 启动脚本**严格按 UTF-8 读写；若既有文件首字符带 UTF-8 BOM
//!   （bash 会把 BOM 当命令报错），挂载 / 卸载时自动剥离疗愈。其余编码（如
//!   UTF-16）无法安全编辑 → 明确报错、绝不覆写。
//!
//! # 安全与降级
//!
//! - 写入一律走「同目录临时文件 + 原子改名」，任意时刻磁盘上都是完整文件；
//! - 挂载幂等：同参数重复 `install` 逐字节不变（不触碰 mtime）；`uninstall`
//!   对从未注入过的文件是空操作，对「整文件即本区块」的文件删除之（还原注入
//!   前的「文件不存在」状态）；
//! - 多目标挂载 / 卸载：`install` / `uninstall` 遍历全部目标文件，单个失败
//!   不阻断其余（聚合上报首个错误）；
//! - 记录失败永不打扰用户会话：PowerShell 侧 try/catch 静默、bash 侧每条
//!   写日志命令自带 `2>/dev/null || true`，绝不让日志钩子挂掉交互 shell。

use crate::modules::terminal_logger::anchor::{inject_block, remove_block};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// 常量与 TAG
// ---------------------------------------------------------------------------

/// PowerShell 挂载区块的锚点 TAG（标记行 `# >>> TLToolBox <TAG> >>>`）。
///
/// 沿用历史值 `ps-transcript`：升级前注入的旧区块也能被本版本原位更新 /
/// 卸载（锚点 TAG 变更是使旧区块成为孤儿的首要原因）。
pub const PS_TAG: &str = "ps-transcript";

/// Bash 挂载区块的锚点 TAG。
///
/// 沿用历史值 `bash-script`（理由同上：保证旧注入可被本版本原位更新 / 清理）。
pub const BASH_TAG: &str = "bash-script";

/// PowerShell 防递归环境变量名（注入脚本据此短路重复转录）。
pub const PS_RECURSION_GUARD_ENV: &str = "TLTB_PS_LOGGED";

/// Bash 防递归环境变量名（`PROMPT_COMMAND` 记录钩子据此短路重复挂载：双入口
/// 文件只生效一次，嵌套 shell 不重复装配）。
pub const BASH_RECURSION_GUARD_ENV: &str = "TLTB_BASH_LOGGED";

/// PowerShell 控制台宿主（CurrentUserCurrentHost）配置文件固定文件名。
pub const PS_PROFILE_FILE_NAME: &str = "Microsoft.PowerShell_profile.ps1";

/// PowerShell 全部宿主（CurrentUserAllHosts）配置文件固定文件名（`profile.ps1`）。
pub const PS_ALL_HOSTS_FILE_NAME: &str = "profile.ps1";

/// PowerShell 5.1 引擎的配置子目录名（Documents 下）。
pub const PS_ENGINE_DIR_51: &str = "WindowsPowerShell";

/// PowerShell 7.x 引擎的配置子目录名（Documents 下）。
pub const PS_ENGINE_DIR_7: &str = "PowerShell";

/// PowerShell 5.1 引擎可执行文件名（Windows 自带）。
pub const PS_ENGINE_EXE_51: &str = "powershell.exe";

/// PowerShell 7.x 引擎可执行文件名（需安装）。
pub const PS_ENGINE_EXE_7: &str = "pwsh.exe";

/// bash 交互（非登录）启动脚本固定文件名。
pub const BASH_RC_FILE_NAME: &str = ".bashrc";

/// bash 登录 shell 启动脚本固定文件名。
pub const BASH_PROFILE_FILE_NAME: &str = ".bash_profile";

/// 注入脚本模板中的 log_base 占位符（拆分为 head/tail 后插入，模板内的
/// 字面量即使在用户路径中复现也不会被二次替换）。
const LOG_BASE_PLACEHOLDER: &str = "__LOG_BASE__";

// ---------------------------------------------------------------------------
// 错误模型
// ---------------------------------------------------------------------------

/// 挂载器操作错误（携带失败路径，便于 UI / 日志直接展示）。
#[derive(Debug)]
pub enum TerminalHookError {
    /// 无法确定用户主目录（`HOME` / `USERPROFILE` 均缺失或非绝对路径）。
    HomeUnavailable,
    /// 读取目标文件失败（文件不存在不算错误——按空内容 / 空操作处理）。
    Read {
        /// 触发失败的文件路径。
        path: PathBuf,
        /// 底层 IO 错误。
        source: io::Error,
    },
    /// 目标文件字节无法按支持编码解码（解码失败而非读取失败）。
    Decode {
        /// 触发失败的文件路径。
        path: PathBuf,
        /// 面向用户的编码说明（如「必须为 UTF-8 纯文本」）。
        hint: &'static str,
    },
    /// 建目录 / 写临时文件 / 原子替换失败。
    Write {
        /// 触发失败的文件路径。
        path: PathBuf,
        /// 底层 IO 错误。
        source: io::Error,
    },
}

impl fmt::Display for TerminalHookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HomeUnavailable => {
                write!(
                    f,
                    "无法确定用户主目录（HOME / USERPROFILE 均缺失或非绝对路径）"
                )
            }
            Self::Read { path, source } => write!(f, "读取失败 '{}': {source}", path.display()),
            Self::Decode { path, hint } => write!(f, "无法解码 '{}': {hint}", path.display()),
            Self::Write { path, source } => write!(f, "写入失败 '{}': {source}", path.display()),
        }
    }
}

impl std::error::Error for TerminalHookError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } | Self::Write { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// 挂载器操作的统一结果别名。
pub type TerminalHookResult<T> = Result<T, TerminalHookError>;

// ---------------------------------------------------------------------------
// 主目录与目标文件路径解析（纯函数层，跨平台可单测）
// ---------------------------------------------------------------------------

/// 解析用户主目录：优先 `HOME`，其次 `USERPROFILE`；只接受绝对路径。
///
/// Windows 上 `HOME` 可能以 MSYS / Git Bash 形态存在（`/c/Users/…`，Rust 的
/// Windows 文件 API 无法直接消费），先经 [`normalize_msys_home`] 归一为盘符
/// 路径；`USERPROFILE` 本身即原生 Windows 路径，原样可用。
pub fn default_home() -> Option<PathBuf> {
    for key in ["HOME", "USERPROFILE"] {
        if let Some(home) = std::env::var_os(key) {
            if home.is_empty() {
                continue;
            }
            let home = normalize_msys_home(PathBuf::from(home));
            if home.is_absolute() {
                return Some(home);
            }
        }
    }
    None
}

/// 把 MSYS / Git Bash 形态的 `$HOME`（`/<盘符>/…`，如 `/c/Users/x`）归一为
/// Windows 盘符路径（`C:\Users\x`）；其余形态（原生 Windows 路径、POSIX
/// 路径、UNC、相对路径）原样返回。
#[cfg(windows)]
fn normalize_msys_home(home: PathBuf) -> PathBuf {
    let text = home.to_string_lossy();
    let bytes = text.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b'/' {
        let drive = (bytes[1] as char).to_ascii_uppercase();
        let rest = text[3..].replace('/', "\\");
        return PathBuf::from(format!("{drive}:\\{rest}"));
    }
    home
}

/// 非 Windows 平台无需归一（`$HOME` 本身就是 POSIX 形态）。
#[cfg(not(windows))]
fn normalize_msys_home(home: PathBuf) -> PathBuf {
    home
}

/// Windows PowerShell **5.1** 配置文件的「直接定位」路径：`<home>\Documents\
/// WindowsPowerShell\Microsoft.PowerShell_profile.ps1`。
///
/// 注意：Documents 被 OneDrive 重定向时该路径并非真实位置，确定性探测请用
/// [`detect_powershell_profiles`]（先经 powershell.exe / pwsh.exe 查询）。
pub fn powershell_profile_path(home: &Path) -> PathBuf {
    home.join("Documents")
        .join(PS_ENGINE_DIR_51)
        .join(PS_PROFILE_FILE_NAME)
}

/// bash 交互（非登录）启动脚本路径：`<home>/.bashrc`。
pub fn bashrc_path(home: &Path) -> PathBuf {
    home.join(BASH_RC_FILE_NAME)
}

/// bash 登录 shell 启动脚本路径：`<home>/.bash_profile`。
pub fn bash_profile_path(home: &Path) -> PathBuf {
    home.join(BASH_PROFILE_FILE_NAME)
}

/// 某引擎配置目录下的一对目标文件：`CurrentUserCurrentHost`（引擎同名
/// `Microsoft.PowerShell_profile.ps1`）与 `CurrentUserAllHosts`（`profile.ps1`）。
fn engine_profile_targets(engine_dir: &Path) -> Vec<PathBuf> {
    vec![
        engine_dir.join(PS_PROFILE_FILE_NAME),
        engine_dir.join(PS_ALL_HOSTS_FILE_NAME),
    ]
}

/// 从 PowerShell 5.1 探测路径反推 Documents 根（`…\Documents\WindowsPowerShell\
/// Microsoft.PowerShell_profile.ps1` 上溯两级即 `…\Documents`），供两套引擎
/// 统一挂载（两引擎同属一个用户 Documents）。
fn documents_of(profile_path: &Path) -> Option<&Path> {
    profile_path
        .parent() // …\WindowsPowerShell
        .and_then(Path::parent) // …\Documents
}

/// 探测 PowerShell **5.1 与 7.x 两套引擎**的全部当前用户配置文件路径
/// （每引擎 `CurrentUserCurrentHost` + `CurrentUserAllHosts` 各一）。
///
/// 解析策略：
/// 1. 先经 `powershell.exe -NoProfile -NonInteractive` 查询 5.1 的
///    `$PROFILE.CurrentUserCurrentHost`（跟随 OneDrive 重定向，输出强制 UTF-8
///    解码；查询以 `CREATE_NO_WINDOW` 隐藏控制台窗口）——成功则以其
///    Documents 根为基准，为两套引擎（`WindowsPowerShell` / `PowerShell`）
///    生成目标对（PowerShell 7 未安装时其目录尚不存在也无妨——挂载会自动
///    创建，未来安装后钩子即生效）；
/// 2. 探测失败（进程缺失 / 非零退出 / 输出不可解析）→ 回退
///    `HOME` / `USERPROFILE` 直接定位（5.1 与 7.x 目录各一对）；
/// 3. 主目录也不可得 → [`TerminalHookError::HomeUnavailable`]。
pub fn detect_powershell_profiles() -> TerminalHookResult<Vec<PathBuf>> {
    if let Some(docs) = probe_powershell_profiles()
        .as_ref()
        .and_then(|paths| paths.first())
        .and_then(|first| documents_of(first))
    {
        let mut targets = Vec::new();
        for engine_dir in [PS_ENGINE_DIR_51, PS_ENGINE_DIR_7] {
            targets.extend(engine_profile_targets(&docs.join(engine_dir)));
        }
        return Ok(targets);
    }
    let home = default_home().ok_or(TerminalHookError::HomeUnavailable)?;
    let documents = home.join("Documents");
    let mut targets = Vec::new();
    for engine_dir in [PS_ENGINE_DIR_51, PS_ENGINE_DIR_7] {
        targets.extend(engine_profile_targets(&documents.join(engine_dir)));
    }
    Ok(targets)
}

/// 经 powershell.exe 探测 5.1 引擎的 `$PROFILE.CurrentUserCurrentHost`（数组首
/// 元素即探测结果）；失败返回 `None`。
#[cfg(windows)]
fn probe_powershell_profiles() -> Option<Vec<PathBuf>> {
    use std::os::windows::process::CommandExt;

    /// `CREATE_NO_WINDOW`：GUI 进程派生控制台程序时不闪烁黑色窗口。
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    /// 强制子进程 stdout 按 UTF-8 输出（默认 OEM/ANSI 会损坏非 ASCII 用户名）。
    const QUERY: &str = "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; [Console]::Write($PROFILE.CurrentUserCurrentHost)";

    let out = std::process::Command::new(PS_ENGINE_EXE_51)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            QUERY,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    path.is_absolute().then_some(vec![path])
}

/// 非 Windows 平台无法探测（该配置本就是 Windows 专属）。
#[cfg(not(windows))]
fn probe_powershell_profiles() -> Option<Vec<PathBuf>> {
    None
}

// ---------------------------------------------------------------------------
// 路径形态转换与注入转义（纯函数层）
// ---------------------------------------------------------------------------

/// Windows 盘符路径（`C:\…` / `c:/…`）→ MSYS / Git Bash POSIX 形态（`/c/…`）。
///
/// bash 双引号语境下反斜杠存在转义歧义（`\\`、`\$`、`` \` `` 等），而 `/c/…`
/// 形态不含任何反斜杠，是 Git Bash 消费 Windows 路径的可靠载体。非盘符前缀
/// 路径（POSIX `/…`、UNC `\\…`、相对路径）原样返回——WSL 内如能读到本配置
/// （例如 `.bashrc` 经挂载点在发行版间共享），POSIX 形态的 `log_base` 可直接
/// 使用；Windows 侧定位的 WSL `.bashrc` 属已知边界（见模块文档）。
pub fn bash_posix_log_root(log_base: &Path) -> String {
    let text = log_base.to_string_lossy();
    let bytes = text.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        let drive = (bytes[0] as char).to_ascii_lowercase();
        let rest = text[3..].replace('\\', "/");
        return format!("/{drive}/{rest}");
    }
    text.into_owned()
}

/// PowerShell 单引号字符串转义：`'` 翻倍（`''`）；其余字符（含 `\`、`$`）在
/// 单引号内均为字面量，无需处理。
fn ps_single_quote_escape(raw: &str) -> String {
    raw.replace('\'', "''")
}

/// bash 双引号字符串转义：`\` `"` `$` `` ` `` 前置反斜杠；其余字符（含空格、
/// 单引号）在双引号内均为字面量。
fn bash_double_quote_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 8);
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' => out.push_str("\\$"),
            '`' => out.push_str("\\`"),
            _ => out.push(ch),
        }
    }
    out
}

/// 把 `value` 填入模板的占位符：先按占位符切分再拼接，模板中的占位符字面量
/// 即使在 `value` 内复现也不会被二次替换（与 `str::replace` 的语义差异点）。
fn fill_placeholder(template: &str, token: &str, value: &str) -> String {
    let (head, tail) = template
        .split_once(token)
        .unwrap_or_else(|| panic!("注入模板缺少占位符 `{token}`"));
    format!("{head}{value}{tail}")
}

// ---------------------------------------------------------------------------
// 注入脚本生成（纯函数层：路径与注入生成的单测锚点）
// ---------------------------------------------------------------------------

/// PowerShell 注入负载模板。占位符 `__LOG_BASE__` 将替换为单引号转义后的
/// Windows 原生 `log_base`。
///
/// 交互修复要点（与旧版的差异）：
/// 1. `Start-Transcript` **必须静默**：经 `| Out-Null` 吞掉其多行启动横幅
///    （PS 5.1 实测 Out-Null 即完全抑制）——横幅若打到控制台会破坏 PSReadLine
///    的行缓冲同步，表现为首屏提示符前需要多敲一次回车；
/// 2. **先抓上一条命令状态、后启动转录**：prompt 函数开头立即读取
///    `$LASTEXITCODE` / `$?`（转录启动本身会改写 `$?`，先启动转录会记录到
///    转录的状态而非用户命令的状态），再静默启动转录；
/// 3. **状态写入独立的会话状态流** `<ts>_pid<PID>.state.log`：活跃的
///    `Start-Transcript` 会**独占锁定**转录文件（PS 5.1 实测任何二次打开写入
///    都报「正由另一进程使用」），逐命令退出状态只能追加到独立状态流文件，
///    转录文件只承载命令 / 输出的逐字转录；两者同目录、同名前缀成对存在。
const PS_PAYLOAD_TEMPLATE: &str = r##"# TLToolBox: PowerShell session recorder hook (delete this whole block to disable)
if ($null -eq $env:TLTB_PS_LOGGED) {
    $env:TLTB_PS_LOGGED = '1'
    try {
        $script:tltbLogRoot = Join-Path '__LOG_BASE__' 'powershell'
        New-Item -ItemType Directory -Path $script:tltbLogRoot -Force -ErrorAction Stop | Out-Null
        $script:tltbLogFile = Join-Path $script:tltbLogRoot ('{0}_pid{1}.log' -f (Get-Date -Format 'yyyy-MM-dd_HH-mm-ss'), $PID)
        # 会话「状态流」文件：转录文件被 Start-Transcript 独占锁定（PS 5.1 实测
        # 转录期间任何二次打开该文件的写入都报「正由另一进程使用」），逐命令的
        # 退出状态因此写入独立的 <ts>_pid<PID>.state.log。
        $script:tltbStateFile = [System.IO.Path]::ChangeExtension($script:tltbLogFile, '.state.log')
        $script:tltbTranscriptOn = $false
        $script:tltbPrevPrompt = $function:prompt
        function global:prompt {
            # 1) 抓取上一条命令状态必须先于任何其它语句（Start-Transcript 会改写
            #    $?）——否则记录到的是转录自身的状态，而非用户上一条命令的状态。
            try {
                $script:tltbCode = $global:LASTEXITCODE
                if ($null -eq $script:tltbCode) { $script:tltbCode = -1 }
                $script:tltbOk = $?
            } catch {
                # Never let a status read disturb the interactive session.
            }
            # 2) 惰性转录：提示符真正出现（= 交互会话）时才启动 Start-Transcript，
            #    非交互式 PowerShell 调用永不产生会话日志文件。横幅经 Out-Null
            #    静默——严禁打到控制台破坏 PSReadLine 的行缓冲（否则首屏提示符
            #    需多敲一次回车）。
            if (-not $script:tltbTranscriptOn) {
                try {
                    Start-Transcript -Path $script:tltbLogFile -Append -IncludeInvocationHeader -ErrorAction Stop | Out-Null
                    $script:tltbTranscriptOn = $true
                } catch {
                    # Never let a transcript failure disturb the interactive session.
                }
            }
            # 3) 调用既有 prompt 逻辑前，仅把上一条命令的退出状态（$LASTEXITCODE
            #    与 $?）追加进会话状态流（<ts>_pid<PID>.state.log）——转录文件被
            #    活跃的 Start-Transcript 独占锁定，不可作为追加目标（PS 5.1 实测）。
            #    UTF-8 落盘；失败静默降级。
            try {
                Add-Content -Path $script:tltbStateFile -Encoding UTF8 -Value ('[TLTB:exit={0},ok={1}] {2}' -f $script:tltbCode, $script:tltbOk, (Get-Date -Format 'yyyy-MM-dd HH:mm:ss.fff')) -ErrorAction Stop
            } catch {
                # Never let a status write disturb the interactive session.
            }
            # 4) 委托既有 prompt 实现（用户 / 其它工具自定义时保留其形态，否则
            #    走系统默认提示符）——prompt 函数不向控制台输出任何多余文本。
            if ($null -ne $script:tltbPrevPrompt) {
                & $script:tltbPrevPrompt
            } else {
                "PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) "
            }
        }
    } catch {
        # Never let a recorder failure disturb the interactive session.
    }
}"##;

/// 生成 PowerShell 会话记录钩子的负载（锚点区块内部内容，不含标记行）。
///
/// 语义：`$env:TLTB_PS_LOGGED` 未设置（防递归）→ 置位 → 确定日志根目录与
/// 会话转录文件（时间戳 + PID）→ 保存既有 `prompt` 实现 → 重写 `prompt`：
/// 先抓上一条命令的 `$LASTEXITCODE` / `$?`（转录启动会改写 `$?`，必须先抓），
/// 再**静默** `Start-Transcript -Append -IncludeInvocationHeader | Out-Null`
/// （多行启动横幅若打到控制台会破坏 PSReadLine 行缓冲 → 首屏需多敲回车；
/// Out-Null 实测（PS 5.1）完全抑制横幅）；随后把状态以
/// `[TLTB:exit=…,ok=…] <时间戳>` 追加进**独立会话状态流**
/// `<ts>_pid<PID>.state.log`（活跃的转录独占锁定转录文件，无法同文件追加，
/// 见模板文档）；最后委托既有 prompt 实现。任何失败 try/catch 静默，绝不
/// 打扰会话。
pub fn powershell_transcript_payload(log_base: &Path) -> String {
    debug_assert!(
        !log_base.to_string_lossy().contains(['\n', '\r']),
        "log_base 不得包含换行"
    );
    let base = ps_single_quote_escape(&log_base.to_string_lossy());
    fill_placeholder(PS_PAYLOAD_TEMPLATE, LOG_BASE_PLACEHOLDER, &base)
}

/// Bash 注入负载模板。占位符 `__LOG_BASE__` 将替换为 MSYS POSIX 形态且经双
/// 引号转义的 `log_base`。
///
/// 记录方案（本次重构）：**PROMPT_COMMAND + `history 1`**，不再使用 DEBUG trap。
/// 旧版 DEBUG trap 方案有两个缺陷：`PROMPT_COMMAND` 以命令形式执行自身函数
/// 名时 DEBUG trap 会把 `__tltb_on_prompt` 当成「待记录命令」覆盖掉真实命令
/// （自身函数名错误落盘）；且 `$BASH_COMMAND` 抓的是简单命令而非用户整行
/// 输入。现方案在每次提示符刷新时用
/// `history 1 | sed 's/^[[:space:]]*[0-9]*[[:space:]]*//'` 抓**上一条用户实际
/// 键入的命令**（历史按行记录、不含 PROMPT_COMMAND 自身），格式化写
/// `[时间戳] [exit=$rc] 命令内容`；按历史行号去重，提示符空刷新（空回车 /
/// 重绘）不重复落盘；首个提示符仅完成行号对齐（跳过从 HISTFILE 预载的上一
/// 会话尾部，避免把历史遗留命令当成新命令）。
const BASH_PAYLOAD_TEMPLATE: &str = r##"# TLToolBox: bash session recorder hook (delete this whole block to disable)
if [ -z "$TLTB_BASH_LOGGED" ]; then
    export TLTB_BASH_LOGGED=1
    if [ -t 1 ] && [ -n "$BASH_VERSION" ]; then
        __tltb_log_dir="__LOG_BASE__/bash"
        mkdir -p "$__tltb_log_dir" 2>/dev/null || true
        export TLTB_BASH_LOG="$__tltb_log_dir/$(date +%Y-%m-%d_%H-%M-%S)_$$.log"
        __tltb_write() {
            printf '%s\n' "$*" >> "$TLTB_BASH_LOG" 2>/dev/null || true
        }
        __tltb_on_prompt() {
            local rc=$?
            local hist_line hist_num hist_cmd
            hist_line=$(history 1 2>/dev/null) || hist_line=''
            # 首帧仅对齐历史行号：既不记录从 HISTFILE 预载的上一会话尾部，也
            # 不为空历史误写任何行。
            if [ -z "${__TLTB_LAST_HISTNUM:-}" ]; then
                if [ -n "$hist_line" ]; then
                    hist_num=$(printf '%s\n' "$hist_line" | sed 's/^[[:space:]]*\([0-9][0-9]*\)[[:space:]].*/\1/')
                    case "$hist_num" in
                        ''|*[!0-9]*) __TLTB_LAST_HISTNUM=0 ;;
                        *) __TLTB_LAST_HISTNUM=$hist_num ;;
                    esac
                else
                    __TLTB_LAST_HISTNUM=0
                fi
                return 0
            fi
            [ -n "$hist_line" ] || return 0
            hist_num=$(printf '%s\n' "$hist_line" | sed 's/^[[:space:]]*\([0-9][0-9]*\)[[:space:]].*/\1/')
            case "$hist_num" in
                ''|*[!0-9]*) return 0 ;;
            esac
            # 提示符空刷新（空回车 / 重绘）不产生新历史条目：行号不变即跳过，
            # 避免把上一条命令重复落盘。
            if [ "$hist_num" = "$__TLTB_LAST_HISTNUM" ]; then
                return 0
            fi
            __TLTB_LAST_HISTNUM=$hist_num
            # history 1 即上一条用户实际键入的命令（bash 不把 PROMPT_COMMAND
            # 自身计入历史，因此本函数名永远不会出现在日志里）。
            hist_cmd=$(printf '%s\n' "$hist_line" | sed 's/^[[:space:]]*[0-9][0-9]*[[:space:]]*//')
            [ -n "$hist_cmd" ] || return 0
            __tltb_write "[$(date '+%Y-%m-%d %H:%M:%S')] [exit=$rc] $hist_cmd"
        }
        if [ -n "$PROMPT_COMMAND" ]; then
            PROMPT_COMMAND="__tltb_on_prompt; $PROMPT_COMMAND"
        else
            PROMPT_COMMAND=__tltb_on_prompt
        fi
    fi
fi"##;

/// 生成 bash 会话记录钩子的负载（锚点区块内部内容，不含标记行）。
///
/// 语义（Windows Git Bash 常缺 `script(1)`，故不依赖屏幕包装）：`TLTB_BASH_LOGGED`
/// 未设（防递归：`.bash_profile` / `.bashrc` 双入口只装配一次、嵌套 shell 不
/// 重复挂）→ 置位 → 仅真实交互终端（`[ -t 1 ]`，bash 一定在）→ 预建日志目录、
/// 以「时间戳 + PID」命名本次会话日志并导出 `TLTB_BASH_LOG` →
/// **PROMPT_COMMAND** 在每次提示符刷新前经 `history 1` 抓上一条实际键入命令，
/// 以 `[时间戳] [exit=…] 命令` 格式落盘（行号去重 + 首帧对齐，无 DEBUG trap、
/// 函数自身名永不落盘）；`PROMPT_COMMAND` 已被用户占用时链式保留
/// （`__tltb_on_prompt; <既有>`）。每条写日志命令自带 `2>/dev/null || true`——
/// 记录失败永不打扰交互 shell。
pub fn bash_transcript_payload(log_base: &Path) -> String {
    debug_assert!(
        !log_base.to_string_lossy().contains(['\n', '\r']),
        "log_base 不得包含换行"
    );
    let root = bash_double_quote_escape(&bash_posix_log_root(log_base));
    fill_placeholder(BASH_PAYLOAD_TEMPLATE, LOG_BASE_PLACEHOLDER, &root)
}

/// 由既有配置内容生成「注入后」内容（纯函数：PowerShell 挂载的确定性核心）。
pub fn ps_injected_content(current: &str, log_base: &Path) -> String {
    inject_block(current, PS_TAG, &powershell_transcript_payload(log_base))
}

/// 由既有配置内容生成「卸载后」内容（纯函数：PowerShell 卸载的确定性核心）。
pub fn ps_removed_content(current: &str) -> String {
    remove_block(current, PS_TAG)
}

/// 由既有配置内容生成「注入后」内容（纯函数：Bash 挂载的确定性核心）。
pub fn bash_injected_content(current: &str, log_base: &Path) -> String {
    inject_block(current, BASH_TAG, &bash_transcript_payload(log_base))
}

/// 由既有配置内容生成「卸载后」内容（纯函数：Bash 卸载的确定性核心）。
pub fn bash_removed_content(current: &str) -> String {
    remove_block(current, BASH_TAG)
}

// ---------------------------------------------------------------------------
// 文件读写：编码识别与原子落盘
// ---------------------------------------------------------------------------

/// 既有文件的编码识别策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadEncoding {
    /// 严格按 UTF-8 解码（bash `.bashrc`；带 UTF-8 BOM 时 BOM 作为内容首字符
    /// 保留，由调用方按需疗愈）。
    StrictUtf8,
    /// 自动识别：UTF-8（带 / 不带 BOM）与 UTF-16LE / UTF-16BE（带 BOM），
    /// BOM 一律剥离出内容、由编码标记在写回时原样还原（PowerShell 配置）。
    AutoBom,
}

/// 目标文件实际采用的文本编码（读入时识别，写回时还原）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextEncoding {
    /// 无 BOM 的 UTF-8。
    Utf8,
    /// 带 UTF-8 BOM（EF BB BF）。
    Utf8Bom,
    /// UTF-16LE（BOM FF FE）。
    Utf16Le,
    /// UTF-16BE（BOM FE FF）。
    Utf16Be,
}

/// 解码：字节 → (文本, 编码标记)。失败返回面向用户的静态提示（路径由调用方
/// 补入错误上下文）。
fn decode_text(bytes: &[u8], read: ReadEncoding) -> Result<(String, TextEncoding), &'static str> {
    match read {
        ReadEncoding::StrictUtf8 => {
            let text = std::str::from_utf8(bytes)
                .map_err(|_| "bash 配置文件必须为 UTF-8 纯文本（不支持 UTF-16 等编码）")?;
            Ok((text.to_owned(), TextEncoding::Utf8))
        }
        ReadEncoding::AutoBom => {
            if bytes.starts_with(&[0xFF, 0xFE]) {
                let text = decode_utf16(bytes, true)
                    .ok_or("UTF-16LE 内容解码失败（BOM 后的字节不是合法 UTF-16）")?;
                Ok((text, TextEncoding::Utf16Le))
            } else if bytes.starts_with(&[0xFE, 0xFF]) {
                let text = decode_utf16(bytes, false)
                    .ok_or("UTF-16BE 内容解码失败（BOM 后的字节不是合法 UTF-16）")?;
                Ok((text, TextEncoding::Utf16Be))
            } else if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
                let text = std::str::from_utf8(&bytes[3..])
                    .map_err(|_| "UTF-8（带 BOM）内容解码失败")?
                    .to_owned();
                Ok((text, TextEncoding::Utf8Bom))
            } else {
                let text = std::str::from_utf8(bytes)
                    .map_err(|_| "既非 UTF-8 也非带 BOM 的 UTF-16，无法安全编辑")?
                    .to_owned();
                Ok((text, TextEncoding::Utf8))
            }
        }
    }
}

/// UTF-16 → UTF-8：`bytes` 须以对应 BOM 开头（调用方已按前缀分派）。
fn decode_utf16(bytes: &[u8], little_endian: bool) -> Option<String> {
    let body = bytes.get(2..)?;
    if body.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            }
        })
        .collect();
    String::from_utf16(&units).ok()
}

/// 编码：文本 + 编码标记 → 字节（BOM 按标记还原）。
fn encode_text(text: &str, encoding: TextEncoding) -> Vec<u8> {
    match encoding {
        TextEncoding::Utf8 => text.as_bytes().to_vec(),
        TextEncoding::Utf8Bom => {
            let mut out = vec![0xEF, 0xBB, 0xBF];
            out.extend_from_slice(text.as_bytes());
            out
        }
        TextEncoding::Utf16Le => encode_utf16(text, true),
        TextEncoding::Utf16Be => encode_utf16(text, false),
    }
}

/// UTF-8 → UTF-16（带 BOM）。
fn encode_utf16(text: &str, little_endian: bool) -> Vec<u8> {
    let mut out = if little_endian {
        vec![0xFF, 0xFE]
    } else {
        vec![0xFE, 0xFF]
    };
    for unit in text.encode_utf16() {
        let bytes = if little_endian {
            unit.to_le_bytes()
        } else {
            unit.to_be_bytes()
        };
        out.extend_from_slice(&bytes);
    }
    out
}

/// 读取目标文件为文本：不存在 → `Ok(None)`；存在 → 解码并按读入编码打标。
fn load_text(
    path: &Path,
    read: ReadEncoding,
) -> TerminalHookResult<Option<(String, TextEncoding)>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(TerminalHookError::Read {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let (text, encoding) = decode_text(&bytes, read).map_err(|hint| TerminalHookError::Decode {
        path: path.to_path_buf(),
        hint,
    })?;
    Ok(Some((text, encoding)))
}

/// 原子落盘：建父目录（若缺）→ 写同目录临时文件 → 原子改名替换。替换失败时
/// 尽力清理临时文件，避免残留半截文件。
fn write_text_atomic(path: &Path, text: &str, encoding: TextEncoding) -> TerminalHookResult<()> {
    let bytes = encode_text(text, encoding);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|source| TerminalHookError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("系统时钟应晚于 UNIX 纪元")
        .as_nanos();
    let tmp = path.with_file_name(format!(
        ".{file_name}.tltb{}{nanos}.tmp",
        std::process::id()
    ));
    std::fs::write(&tmp, &bytes).map_err(|source| TerminalHookError::Write {
        path: tmp.clone(),
        source,
    })?;
    if let Err(source) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(TerminalHookError::Write {
            path: path.to_path_buf(),
            source,
        });
    }
    Ok(())
}

/// 目标配置文件的「读写档案」：编码识别策略、新建文件的默认编码、是否疗愈
/// 首字符 UTF-8 BOM。
#[derive(Debug, Clone, Copy)]
struct FileProfile {
    read: ReadEncoding,
    new_file: TextEncoding,
    strip_utf8_bom: bool,
}

/// PowerShell 配置文件档案：自动编码识别；新建用 UTF-8 BOM（PS 5.1 无 BOM
/// 判定 UTF-8）；BOM 已在解码层剥离，无需二次疗愈。
const PS_PROFILE_FILE: FileProfile = FileProfile {
    read: ReadEncoding::AutoBom,
    new_file: TextEncoding::Utf8Bom,
    strip_utf8_bom: false,
};

/// bash `.bashrc` 档案：严格 UTF-8；新建无 BOM；首字符 BOM（bash 会把 BOM 当
/// 命令报错）在挂载 / 卸载时剥离疗愈。
const BASH_RC_FILE: FileProfile = FileProfile {
    read: ReadEncoding::StrictUtf8,
    new_file: TextEncoding::Utf8,
    strip_utf8_bom: true,
};

/// 按档案剥离文本首字符 UTF-8 BOM（bash 疗愈用）。
fn strip_leading_bom(text: String, profile: FileProfile) -> String {
    if profile.strip_utf8_bom && text.starts_with('\u{feff}') {
        let mut text = text;
        text.drain(..'\u{feff}'.len_utf8());
        text
    } else {
        text
    }
}

/// 通用挂载：读既有内容（缺省为空）→ 剥离 BOM（按档案）→ 锚点注入 → 内容
/// 变化才原子落盘（幂等：同参数重复调用不触碰文件）。
fn mount_block(
    path: &Path,
    tag: &str,
    payload: &str,
    profile: FileProfile,
) -> TerminalHookResult<()> {
    let (current, encoding) = match load_text(path, profile.read)? {
        Some(found) => found,
        None => (String::new(), profile.new_file),
    };
    let current = strip_leading_bom(current, profile);
    let next = strip_leading_bom(inject_block(&current, tag, payload), profile);
    if next == current {
        return Ok(());
    }
    write_text_atomic(path, &next, encoding)
}

/// 通用卸载：文件不存在 → 空操作；锚点移除后内容未变（从未注入过）→ 空
/// 操作；内容为空（整文件即本区块）→ 删除文件，还原注入前的「不存在」；
/// 否则原子落盘。
fn unmount_block(path: &Path, tag: &str, profile: FileProfile) -> TerminalHookResult<()> {
    let Some((current, encoding)) = load_text(path, profile.read)? else {
        return Ok(());
    };
    let current = strip_leading_bom(current, profile);
    let next = strip_leading_bom(remove_block(&current, tag), profile);
    if next == current {
        return Ok(());
    }
    if next.is_empty() {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(TerminalHookError::Write {
                path: path.to_path_buf(),
                source,
            }),
        }
    } else {
        write_text_atomic(path, &next, encoding)
    }
}

// ---------------------------------------------------------------------------
// 挂载器（面向装配层的公共入口）
// ---------------------------------------------------------------------------

/// PowerShell 会话记录挂载器（**5.1 与 7.x 双引擎、每引擎双文件**）。
///
/// 目标文件由 [`detect_powershell_profiles`] 定位：每引擎目录下的
/// `Microsoft.PowerShell_profile.ps1`（CurrentUserCurrentHost）与 `profile.ps1`
/// （CurrentUserAllHosts）。`install` 向**每个**目标注入 TAG 为 [`PS_TAG`] 的
/// 锚点区块（父目录自动创建），`uninstall` 原样移除全部目标中的区块——单
/// 目标失败不阻断其余（聚合上报）。
#[derive(Debug, Clone)]
pub struct PowerShellHook {
    /// 待挂载的配置文件路径集合（`at` 单文件形态 = 单元素）。
    profiles: Vec<PathBuf>,
}

impl PowerShellHook {
    /// 定位全部 PowerShell 目标配置文件（5.1 + 7.x × CurrentUserCurrentHost +
    /// AllHosts，见 [`detect_powershell_profiles`]）。
    pub fn new() -> TerminalHookResult<Self> {
        Ok(Self {
            profiles: detect_powershell_profiles()?,
        })
    }

    /// 显式指定单个目标配置文件（测试注入固定路径时使用）。
    pub fn at(profile_path: impl Into<PathBuf>) -> Self {
        Self {
            profiles: vec![profile_path.into()],
        }
    }

    /// 显式指定多个目标配置文件（测试多引擎 / 多宿主往返时使用）。
    pub fn at_all(profile_paths: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            profiles: profile_paths.into_iter().collect(),
        }
    }

    /// 当前挂载器的全部目标配置文件路径。
    pub fn targets(&self) -> &[PathBuf] {
        &self.profiles
    }

    /// 单文件形态下的目标路径（`at` 构造时）；多目标（`new` 构造）返回首个。
    pub fn profile_path(&self) -> &Path {
        &self.profiles[0]
    }

    /// 注入 PowerShell 会话记录钩子到全部目标（幂等：同 `log_base` 重复调用
    /// 逐字节不变；单目标失败不阻断其余，聚合上报首个错误）。
    pub fn install(&self, log_base: &Path) -> TerminalHookResult<()> {
        let payload = powershell_transcript_payload(log_base);
        let mut first_error: Option<TerminalHookError> = None;
        for profile in &self.profiles {
            if let Err(err) = mount_block(profile, PS_TAG, &payload, PS_PROFILE_FILE) {
                tracing::warn!(
                    target: "terminal_logger",
                    "PowerShell 钩子挂载失败 '{}': {err}",
                    profile.display()
                );
                first_error.get_or_insert(err);
            }
        }
        if let Some(err) = first_error {
            return Err(err);
        }
        tracing::debug!(
            target: "terminal_logger",
            "PowerShell 会话记录钩子已挂载到 {} 个配置文件（log_base: '{}'）",
            self.profiles.len(),
            log_base.display()
        );
        Ok(())
    }

    /// 移除 PowerShell 会话记录钩子（未注入过 / 文件不存在均为空操作）。
    pub fn uninstall(&self) -> TerminalHookResult<()> {
        let mut first_error: Option<TerminalHookError> = None;
        for profile in &self.profiles {
            if let Err(err) = unmount_block(profile, PS_TAG, PS_PROFILE_FILE) {
                tracing::warn!(
                    target: "terminal_logger",
                    "PowerShell 钩子卸载失败 '{}': {err}",
                    profile.display()
                );
                first_error.get_or_insert(err);
            }
        }
        if let Some(err) = first_error {
            return Err(err);
        }
        tracing::debug!(
            target: "terminal_logger",
            "PowerShell 会话记录钩子已卸载（{} 个配置文件）",
            self.profiles.len()
        );
        Ok(())
    }
}

/// Bash（Git Bash / WSL）会话记录挂载器（**`.bashrc` + `.bash_profile` 双入口**）。
///
/// Git Bash 默认以**登录 shell** 启动（读 `~/.bash_profile`），也常从既有会话
/// 里再开非登录交互 shell（读 `~/.bashrc`）——双入口都注入 TAG 为 [`BASH_TAG`]
/// 的锚点区块，钩子负载内的 `TLTB_BASH_LOGGED` 防递归保证同会话只装配一次。
/// `at` 单文件形态仅用于测试（`bash_profile` 目标为 `None`）。
#[derive(Debug, Clone)]
pub struct BashHook {
    /// `~/.bashrc`（交互非登录 shell）。
    rc: PathBuf,
    /// `~/.bash_profile`（登录 shell；`None` = 测试单文件形态，不触碰）。
    profile: Option<PathBuf>,
}

impl BashHook {
    /// 定位 `$HOME/.bashrc` 与 `$HOME/.bash_profile`（`HOME` → `USERPROFILE`，
    /// 含 MSYS 形态归一）。
    pub fn new() -> TerminalHookResult<Self> {
        let home = default_home().ok_or(TerminalHookError::HomeUnavailable)?;
        Ok(Self {
            rc: bashrc_path(&home),
            profile: Some(bash_profile_path(&home)),
        })
    }

    /// 显式指定单个目标 `.bashrc`（测试注入固定路径时使用；不触碰
    /// `.bash_profile`）。
    pub fn at(bashrc_path: impl Into<PathBuf>) -> Self {
        Self {
            rc: bashrc_path.into(),
            profile: None,
        }
    }

    /// 显式指定 `.bashrc` 与 `.bash_profile` 双目标（测试双入口往返时使用）。
    pub fn at_pair(bashrc_path: impl Into<PathBuf>, profile_path: impl Into<PathBuf>) -> Self {
        Self {
            rc: bashrc_path.into(),
            profile: Some(profile_path.into()),
        }
    }

    /// 当前挂载器的全部目标文件路径（`.bashrc` 恒在；`new` 形态含
    /// `.bash_profile`）。
    pub fn targets(&self) -> Vec<&Path> {
        let mut targets = vec![self.rc.as_path()];
        if let Some(profile) = &self.profile {
            targets.push(profile.as_path());
        }
        targets
    }

    /// 当前挂载器指向的目标 `.bashrc` 路径。
    pub fn bashrc_path(&self) -> &Path {
        &self.rc
    }

    /// 注入 bash 会话记录钩子到全部目标（幂等；单目标失败不阻断其余）。
    pub fn install(&self, log_base: &Path) -> TerminalHookResult<()> {
        let payload = bash_transcript_payload(log_base);
        let mut first_error: Option<TerminalHookError> = None;
        for target in self.targets() {
            if let Err(err) = mount_block(target, BASH_TAG, &payload, BASH_RC_FILE) {
                tracing::warn!(
                    target: "terminal_logger",
                    "bash 钩子挂载失败 '{}': {err}",
                    target.display()
                );
                first_error.get_or_insert(err);
            }
        }
        if let Some(err) = first_error {
            return Err(err);
        }
        tracing::debug!(
            target: "terminal_logger",
            "bash 会话记录钩子已挂载到 {} 个启动脚本（log_base: '{}'）",
            self.targets().len(),
            log_base.display()
        );
        Ok(())
    }

    /// 移除 bash 会话记录钩子（未注入过 / 文件不存在均为空操作）。
    pub fn uninstall(&self) -> TerminalHookResult<()> {
        let mut first_error: Option<TerminalHookError> = None;
        for target in self.targets() {
            if let Err(err) = unmount_block(target, BASH_TAG, BASH_RC_FILE) {
                tracing::warn!(
                    target: "terminal_logger",
                    "bash 钩子卸载失败 '{}': {err}",
                    target.display()
                );
                first_error.get_or_insert(err);
            }
        }
        if let Some(err) = first_error {
            return Err(err);
        }
        tracing::debug!(
            target: "terminal_logger",
            "bash 会话记录钩子已卸载（{} 个启动脚本）",
            self.targets().len()
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::terminal_logger::anchor::{block_end_marker, block_start_marker};

    /// 统计内容中某 TAG 起始标记的个数。
    fn count_blocks(content: &str, tag: &str) -> usize {
        content.matches(&block_start_marker(tag)).count()
    }

    /// 系统临时目录下本次测试独有的目录路径（并行测试互不干扰）。
    fn unique_temp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("系统时钟应晚于 UNIX 纪元")
            .as_nanos();
        std::env::temp_dir().join(format!("tltoolbox-{tag}-{}-{nanos}", std::process::id()))
    }

    // ---- 路径解析 ----

    #[test]
    fn powershell_profile_path_joins_documents_windows_powershell() {
        let home = Path::new(r"C:\Users\测试用户");
        assert_eq!(
            powershell_profile_path(home),
            PathBuf::from(
                r"C:\Users\测试用户\Documents\WindowsPowerShell\Microsoft.PowerShell_profile.ps1"
            )
        );
    }

    #[test]
    fn bashrc_path_joins_home() {
        let home = Path::new(r"C:\Users\alice");
        assert_eq!(bashrc_path(home), PathBuf::from(r"C:\Users\alice\.bashrc"));
    }

    #[test]
    fn bash_profile_path_joins_home() {
        let home = Path::new(r"C:\Users\alice");
        assert_eq!(
            bash_profile_path(home),
            PathBuf::from(r"C:\Users\alice\.bash_profile")
        );
    }

    #[cfg(windows)]
    #[test]
    fn msys_home_is_normalized_to_drive_path() {
        // Git Bash 的 $HOME 常为 /c/Users/… 形态，Windows 文件 API 不能直接消费。
        assert_eq!(
            normalize_msys_home(PathBuf::from("/c/Users/alice")),
            PathBuf::from(r"C:\Users\alice")
        );
        assert_eq!(
            normalize_msys_home(PathBuf::from("/d/Tools")),
            PathBuf::from(r"D:\Tools")
        );
        // 原生 Windows 路径与纯 POSIX 路径不受影响。
        assert_eq!(
            normalize_msys_home(PathBuf::from(r"C:\Users\alice")),
            PathBuf::from(r"C:\Users\alice")
        );
        assert_eq!(
            normalize_msys_home(PathBuf::from("/home/alice")),
            PathBuf::from("/home/alice")
        );
    }

    #[test]
    fn default_home_is_absolute_when_environment_provides_one() {
        if std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .is_none()
        {
            return; // 无任何主目录环境变量时无从断言，跳过。
        }
        let home = default_home().expect("存在 HOME / USERPROFILE 时应可解析");
        assert!(
            home.is_absolute(),
            "主目录必须是绝对路径，实际 {}",
            home.display()
        );
    }

    #[cfg(windows)]
    #[test]
    fn detect_powershell_profiles_covers_both_engines_and_both_hosts() {
        // powershell.exe 探测成功 / 回退直接定位，两条路径都必须产出两套引擎
        // 目录（WindowsPowerShell / PowerShell）下的「CurrentUserCurrentHost +
        // AllHosts」四个目标，且全部为绝对路径。
        let targets = detect_powershell_profiles().expect("Windows 上应可解析 $PROFILE");
        assert_eq!(targets.len(), 4, "应为 2 引擎 × 2 宿主: {targets:?}");
        for target in &targets {
            assert!(
                target.is_absolute(),
                "探测路径应为绝对路径: {}",
                target.display()
            );
            assert!(
                target.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name == PS_PROFILE_FILE_NAME || name == PS_ALL_HOSTS_FILE_NAME
                }),
                "目标应以 {PS_PROFILE_FILE_NAME} / {PS_ALL_HOSTS_FILE_NAME} 收尾，实际 {}",
                target.display()
            );
        }
        let dirs: Vec<String> = targets
            .iter()
            .filter_map(|t| t.parent())
            .map(|d| d.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            dirs.iter().any(|d| d == PS_ENGINE_DIR_51) && dirs.iter().any(|d| d == PS_ENGINE_DIR_7),
            "两套引擎目录都应覆盖: {dirs:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn documents_of_derives_two_levels_up_from_probe_path() {
        let probe = Path::new(
            r"C:\Users\张三\OneDrive\Documents\WindowsPowerShell\Microsoft.PowerShell_profile.ps1",
        );
        assert_eq!(
            documents_of(probe),
            Some(Path::new(r"C:\Users\张三\OneDrive\Documents"))
        );
    }

    #[test]
    fn bash_posix_log_root_converts_drive_paths_only() {
        assert_eq!(
            bash_posix_log_root(Path::new(r"C:\Program Files\TLToolBox\logs\terminals")),
            "/c/Program Files/TLToolBox/logs/terminals"
        );
        assert_eq!(bash_posix_log_root(Path::new("d:/Tools")), "/d/Tools");
        // 非盘符前缀：POSIX / 相对路径原样返回。
        assert_eq!(
            bash_posix_log_root(Path::new("/home/alice/logs/terminals")),
            "/home/alice/logs/terminals"
        );
        assert_eq!(
            bash_posix_log_root(Path::new("logs/terminals")),
            "logs/terminals"
        );
    }

    // ---- PowerShell 注入生成 ----

    #[test]
    fn ps_payload_embeds_log_base_and_recorder_logic() {
        let base = Path::new(r"D:\TLToolBox\logs\terminals");
        let payload = powershell_transcript_payload(base);
        // 防递归环境变量检测 + 置位。
        assert!(payload.contains("$null -eq $env:TLTB_PS_LOGGED"));
        assert!(payload.contains("$env:TLTB_PS_LOGGED = '1'"));
        // Windows 原生路径（含反斜杠）按单引号字符串嵌入，不做任何改写。
        assert!(payload.contains(r"Join-Path 'D:\TLToolBox\logs\terminals' 'powershell'"));
        // 目录预建。
        assert!(payload.contains("New-Item -ItemType Directory"));
        // 会话日志文件名：<log_base>/powershell/yyyy-MM-dd_HH-mm-ss_pid<PID>.log。
        assert!(payload.contains("yyyy-MM-dd_HH-mm-ss"));
        assert!(payload.contains("_pid{1}.log"));
        assert!(payload.contains("$PID"));
        // 惰性转录启动命令（保留既有 prompt 实现、含 -Append 与调用头）。
        assert!(payload.contains("Start-Transcript -Path"));
        assert!(payload.contains("-Append -IncludeInvocationHeader"));
        assert!(payload.contains("$script:tltbTranscriptOn"));
        // 转录必须静默：横幅经 Out-Null 吞掉，严禁打到控制台破坏 PSReadLine
        // 的行缓冲（否则首屏提示符需多敲一次回车）。
        assert!(payload.contains(
            "Start-Transcript -Path $script:tltbLogFile -Append -IncludeInvocationHeader -ErrorAction Stop | Out-Null"
        ));
        // 状态抓取必须先于转录启动：Start-Transcript 会改写 $?，先启动转录会
        // 记录到转录自身状态而非用户上一条命令状态。
        let status_at = payload.find("$script:tltbOk = $?").expect("状态抓取应在");
        let transcript_at = payload
            .find("Start-Transcript -Path")
            .expect("转录启动应在");
        assert!(
            status_at < transcript_at,
            "上一条命令状态必须先抓取，再启动转录"
        );
        // 提示符刷新前记录上一条命令状态：$LASTEXITCODE 与 $? 经 Add-Content
        // 追加进**独立的会话状态流** <ts>_pid<PID>.state.log——活跃的转录会
        // 独占锁定转录文件，同文件追加必然失败（PS 5.1 实测）。
        assert!(payload.contains("function global:prompt"));
        assert!(payload.contains("$global:LASTEXITCODE"));
        assert!(payload.contains("$script:tltbOk = $?"));
        assert!(payload.contains("$script:tltbStateFile"));
        assert!(payload.contains(".state.log"));
        assert!(payload.contains("Add-Content -Path $script:tltbStateFile"));
        assert!(payload.contains("-Encoding UTF8"));
        assert!(payload.contains("[TLTB:exit={0},ok={1}]"));
        // 委托既有 prompt 实现，不破坏用户 / 其它工具的自定义提示符。
        assert!(payload.contains("$script:tltbPrevPrompt = $function:prompt"));
        assert!(payload.contains("& $script:tltbPrevPrompt"));
        // 失败静默降级，不打扰会话。
        assert!(payload.contains("try {"));
        assert!(payload.contains("catch {"));
        // 占位符不得泄漏。
        assert!(!payload.contains(LOG_BASE_PLACEHOLDER));
    }

    #[test]
    fn ps_payload_escapes_apostrophes_in_log_base() {
        // Windows 文件名允许 `'`：单引号字符串内必须翻倍转义。
        let payload = powershell_transcript_payload(Path::new(r"C:\Users\O'Brien\logs"));
        assert!(payload.contains(r"C:\Users\O''Brien\logs"));
        // 转义后字符串仍是「一对单引号包裹」，语义不被截断。
        assert!(payload.contains(r"Join-Path 'C:\Users\O''Brien\logs' 'powershell'"));
    }

    #[test]
    fn ps_injected_content_preserves_user_content_and_is_reversible() {
        let original = "# 用户自己的内容\nWrite-Host 'hi'\n";
        let injected = ps_injected_content(original, Path::new(r"D:\logs"));
        assert!(
            injected.starts_with(original),
            "用户内容应逐字节保留在区块之前: {injected}"
        );
        assert_eq!(count_blocks(&injected, PS_TAG), 1);
        assert!(injected.contains(&block_start_marker(PS_TAG)));
        assert!(injected.contains(&block_end_marker(PS_TAG)));
        assert_eq!(
            ps_removed_content(&injected),
            original,
            "卸载应逐字节还原原文"
        );
    }

    #[test]
    fn ps_injected_content_updates_in_place_and_converges_duplicates() {
        let original = "# 用户内容\n# >>> TLToolBox autostart >>>\nStart-Process x\n# <<< TLToolBox autostart <<<\n";
        let once = ps_injected_content(original, Path::new(r"D:\logs\one"));
        let twice = ps_injected_content(&once, Path::new(r"D:\logs\two"));
        assert_eq!(count_blocks(&twice, PS_TAG), 1, "同 TAG 不得重复叠放");
        assert!(twice.contains(r"D:\logs\two"), "新 log_base 应原位更新负载");
        assert!(!twice.contains(r"D:\logs\one"), "旧负载应被替换");
        assert!(twice.contains("autostart"), "他 TAG 区块不受影响");
        assert!(ps_removed_content(&twice).starts_with("# 用户内容"));
    }

    #[test]
    fn ps_crlf_host_keeps_crlf_through_inject_and_remove() {
        let original = "# 用户内容\r\nWrite-Host 'ok'\r\n";
        let injected = ps_injected_content(original, Path::new(r"D:\logs"));
        assert!(
            injected.contains("\r\n# >>> TLToolBox ps-transcript >>>\r\n"),
            "CRLF 宿主下新增块内行应同样 CRLF"
        );
        assert_eq!(ps_removed_content(&injected), original);
    }

    // ---- Bash 注入生成 ----

    #[test]
    fn bash_payload_embeds_posix_log_root_and_recorder_logic() {
        let payload =
            bash_transcript_payload(Path::new(r"C:\Program Files\TLToolBox\logs\terminals"));
        // 仅交互终端 + 防递归检测（双入口 .bash_profile / .bashrc 只生效一次）。
        assert!(payload.contains(r#"if [ -z "$TLTB_BASH_LOGGED" ]"#));
        assert!(payload.contains("export TLTB_BASH_LOGGED=1"));
        assert!(payload.contains(r#"[ -t 1 ]"#));
        assert!(payload.contains(r#"$BASH_VERSION"#));
        // PROMPT_COMMAND 记录钩子：经 history 1 抓上一条用户实际键入的命令
        // （bash 不把 PROMPT_COMMAND 自身计入历史 → 函数名永不落盘）。
        assert!(payload.contains("PROMPT_COMMAND=__tltb_on_prompt"));
        assert!(payload.contains("history 1 2>/dev/null"));
        assert!(
            payload.contains("sed 's/^[[:space:]]*[0-9][0-9]*[[:space:]]*//'"),
            "应剥离 history 行号、还原用户命令原文"
        );
        assert!(payload.contains("[exit=$rc]"));
        assert!(payload.contains("__TLTB_LAST_HISTNUM"));
        // 旧 DEBUG trap 方案已彻底移除（它会把 __tltb_on_prompt 函数名误记为
        // 待记录命令、且 $BASH_COMMAND 抓不到用户整行）。
        assert!(!payload.contains("trap __tltb_on_debug DEBUG"));
        assert!(!payload.contains("__TLTB_LAST_CMD"));
        assert!(!payload.contains("__tltb_on_debug"));
        assert!(!payload.contains("$BASH_COMMAND"));
        // 既有 PROMPT_COMMAND 链式保留。
        assert!(payload.contains(r#"PROMPT_COMMAND="__tltb_on_prompt; $PROMPT_COMMAND""#));
        // 绝不使用 script(1) 包装（Windows Git Bash 通常缺失该命令）。
        assert!(!payload.contains("exec script"));
        assert!(!payload.contains("UNDER_SCRIPT"));
        // Windows 路径先归一为 MSYS POSIX，再双引号转义后嵌入。
        assert!(
            payload.contains(r#"__tltb_log_dir="/c/Program Files/TLToolBox/logs/terminals/bash""#)
        );
        // 会话日志文件名：<log_base>/bash/$(date …)_$$.log。
        assert!(payload.contains("$(date +%Y-%m-%d_%H-%M-%S)_$$.log"));
        assert!(!payload.contains(LOG_BASE_PLACEHOLDER));
    }

    #[test]
    fn bash_payload_escapes_dollar_backtick_quote_and_backslash() {
        // Windows 目录名可以含 $ 与反引号；转义必须保证双引号语境逐字可靠。
        let payload = bash_transcript_payload(Path::new(r#"C:\we$ird\`tick\quo"te\base"#));
        assert!(payload.contains(r#"/c/we\$ird"#), "`$` 应转义为 `\\$`");
        assert!(payload.contains(r#"/\`tick"#), "反引号应转义");
        assert!(payload.contains(r#"quo\"te"#), "双引号应转义");
        assert!(!payload.contains("we$ird/`tick"), "不得残留未转义形态");
    }

    #[test]
    fn bash_injected_content_preserves_user_content_and_is_reversible() {
        let original = "export EDITOR=vim\n\n# 我的别名\n";
        let injected = bash_injected_content(original, Path::new(r"D:\logs"));
        assert!(injected.starts_with(original));
        assert_eq!(count_blocks(&injected, BASH_TAG), 1);
        assert_eq!(bash_removed_content(&injected), original);
        // 幂等：同参数重复注入逐字节一致。
        assert_eq!(
            bash_injected_content(&injected, Path::new(r"D:\logs")),
            injected
        );
    }

    // ---- 挂载器文件系统行为 ----

    #[test]
    fn ps_hook_install_creates_parent_dirs_and_uninstall_removes_created_file() {
        let root = unique_temp("ps-hook");
        let profile = root
            .join("Docs")
            .join("WindowsPowerShell")
            .join(PS_PROFILE_FILE_NAME);
        let hook = PowerShellHook::at(&profile);
        let log_base = Path::new(r"D:\logs\terminals");

        hook.install(log_base).expect("首次挂载应成功");
        assert!(profile.exists(), "配置文件应被创建");
        assert!(profile.parent().unwrap().is_dir(), "父目录应自动创建");

        let first = std::fs::read_to_string(&profile).unwrap();
        assert_eq!(count_blocks(&first, PS_TAG), 1);
        assert!(first.contains(r"Join-Path 'D:\logs\terminals' 'powershell'"));

        // 幂等：重复挂载不改变内容。
        hook.install(log_base).expect("重复挂载应幂等成功");
        let second = std::fs::read_to_string(&profile).unwrap();
        assert_eq!(first, second, "同参数重复挂载必须逐字节不变");

        hook.uninstall().expect("卸载应成功");
        assert!(
            !profile.exists(),
            "整文件即本区块时，卸载应删除文件（还原挂载前的「不存在」）"
        );
        // 对已卸载 / 不存在的文件再次卸载是空操作。
        hook.uninstall().expect("重复卸载应为空操作");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ps_hook_preserves_user_content_through_install_and_uninstall() {
        let root = unique_temp("ps-hook-user");
        std::fs::create_dir_all(&root).unwrap();
        let profile = root.join("profile.ps1");
        let original = "# 我的配置\r\nSet-Alias ll Get-ChildItem\r\n";
        std::fs::write(&profile, original).unwrap();

        let hook = PowerShellHook::at(&profile);
        hook.install(Path::new(r"D:\logs")).unwrap();
        let installed = std::fs::read_to_string(&profile).unwrap();
        assert!(installed.starts_with(original));
        assert!(installed.contains("\r\n# >>> TLToolBox ps-transcript >>>\r\n"));

        hook.uninstall().unwrap();
        let restored = std::fs::read_to_string(&profile).unwrap();
        assert_eq!(restored, original, "卸载后应逐字节还原用户原文（含 CRLF）");

        // 从未注入过的文件：卸载空操作，内容原样。
        let pristine = std::fs::read_to_string(&profile).unwrap();
        hook.uninstall().unwrap();
        assert_eq!(std::fs::read_to_string(&profile).unwrap(), pristine);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ps_hook_roundtrips_utf16le_profile_bytes() {
        // PowerShell 5.1 历史遗留的 UTF-16LE（带 BOM）配置：读、改、还原必须
        // 字节级无损。
        let root = unique_temp("ps-hook-utf16");
        std::fs::create_dir_all(&root).unwrap();
        let profile = root.join("profile.ps1");
        let user_text = "# 用户内容\r\nWrite-Host 'é'\r\n";
        let original_bytes = encode_text(user_text, TextEncoding::Utf16Le);
        std::fs::write(&profile, &original_bytes).unwrap();

        let hook = PowerShellHook::at(&profile);
        hook.install(Path::new(r"D:\logs")).unwrap();
        let installed = std::fs::read(&profile).unwrap();
        assert!(installed.starts_with(&[0xFF, 0xFE]), "BOM 应原样保留");
        assert!(
            String::from_utf8(installed).is_err(),
            "内容应仍是 UTF-16（而非被改写为 UTF-8）"
        );

        hook.uninstall().unwrap();
        let restored = std::fs::read(&profile).unwrap();
        assert_eq!(restored, original_bytes, "UTF-16 内容卸载后应字节级还原");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bash_hook_install_uninstall_roundtrip_and_bom_heal() {
        let root = unique_temp("bash-hook");
        std::fs::create_dir_all(&root).unwrap();
        let bashrc = root.join(".bashrc");
        // 首字符带 UTF-8 BOM 的既有文件（bash 会把它当命令报错）。
        let mut bom_user = vec![0xEF, 0xBB, 0xBF];
        bom_user.extend_from_slice("# 用户别名\n".as_bytes());
        std::fs::write(&bashrc, &bom_user).unwrap();

        let hook = BashHook::at(&bashrc);
        hook.install(Path::new(r"D:\logs\terminals")).unwrap();
        let installed = std::fs::read(&bashrc).unwrap();
        assert!(
            !installed.starts_with(&[0xEF, 0xBB, 0xBF]),
            "挂载应顺带剥离 bash 无法消费的 UTF-8 BOM"
        );
        let text = String::from_utf8(installed).unwrap();
        assert!(
            text.starts_with("# 用户别名\n"),
            "用户内容应保留且 BOM 已疗愈"
        );
        assert_eq!(count_blocks(&text, BASH_TAG), 1);

        hook.uninstall().unwrap();
        let restored = std::fs::read_to_string(&bashrc).unwrap();
        assert_eq!(
            restored, "# 用户别名\n",
            "卸载后保留用户内容（BOM 已疗愈，不回写）"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bash_hook_creates_file_and_uninstall_deletes_it_again() {
        let root = unique_temp("bash-hook-new");
        let bashrc = root.join("sub").join(".bashrc"); // 父目录也不存在
        let hook = BashHook::at(&bashrc);
        hook.install(Path::new(r"D:\logs")).unwrap();
        assert!(bashrc.exists());
        assert_eq!(
            count_blocks(&std::fs::read_to_string(&bashrc).unwrap(), BASH_TAG),
            1
        );
        hook.uninstall().unwrap();
        assert!(!bashrc.exists(), "自建且无用户内容的 .bashrc 卸载后应删除");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bash_hook_rejects_non_utf8_file_without_touching_it() {
        let root = unique_temp("bash-hook-utf16");
        std::fs::create_dir_all(&root).unwrap();
        let bashrc = root.join(".bashrc");
        let original = encode_text("# utf16 不可用于 bash\n", TextEncoding::Utf16Le);
        std::fs::write(&bashrc, &original).unwrap();

        let hook = BashHook::at(&bashrc);
        let err = hook.install(Path::new(r"D:\logs")).unwrap_err();
        assert!(
            matches!(err, TerminalHookError::Decode { .. }),
            "UTF-16 的 .bashrc 应明确报解码错误: {err}"
        );
        assert_eq!(
            std::fs::read(&bashrc).unwrap(),
            original,
            "解码失败时不得触碰原文件"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- 多引擎 / 多宿主 / 双入口挂载往返 ----

    #[test]
    fn ps_hook_multi_target_install_and_uninstall_roundtrip() {
        let root = unique_temp("ps-hook-multi");
        std::fs::create_dir_all(&root).unwrap();
        let current_host = root.join("WindowsPowerShell").join(PS_PROFILE_FILE_NAME);
        let all_hosts = root.join("PowerShell").join(PS_ALL_HOSTS_FILE_NAME);
        // 用户既有内容（AllHosts 预置一段注释，验证无损保留）。
        std::fs::create_dir_all(all_hosts.parent().unwrap()).unwrap();
        std::fs::write(&all_hosts, "# 用户 AllHosts 内容\n").unwrap();

        let hook = PowerShellHook::at_all(vec![current_host.clone(), all_hosts.clone()]);
        assert_eq!(hook.targets().len(), 2);

        hook.install(Path::new(r"D:\logs\terminals"))
            .expect("多目标安装应成功");
        for target in [&current_host, &all_hosts] {
            let text = std::fs::read_to_string(target).unwrap();
            assert_eq!(count_blocks(&text, PS_TAG), 1, "{}", target.display());
        }
        let preserved = std::fs::read_to_string(&all_hosts).unwrap();
        assert!(
            preserved.starts_with("# 用户 AllHosts 内容\n"),
            "用户内容应逐字节保留"
        );

        hook.uninstall().expect("多目标卸载应成功");
        assert!(!current_host.exists(), "自建文件卸载后应删除");
        assert_eq!(
            std::fs::read_to_string(&all_hosts).unwrap(),
            "# 用户 AllHosts 内容\n",
            "有用户内容的文件卸载后应还原"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bash_hook_dual_entry_install_and_uninstall_roundtrip() {
        let root = unique_temp("bash-hook-dual");
        std::fs::create_dir_all(&root).unwrap();
        let rc = root.join(".bashrc");
        let profile = root.join(".bash_profile");

        let hook = BashHook::at_pair(&rc, &profile);
        assert_eq!(hook.targets().len(), 2);

        hook.install(Path::new(r"D:\logs\terminals"))
            .expect("双入口安装应成功");
        for target in [&rc, &profile] {
            let text = std::fs::read_to_string(target).unwrap();
            assert_eq!(count_blocks(&text, BASH_TAG), 1, "{}", target.display());
            assert!(
                text.contains("PROMPT_COMMAND") && text.contains("history 1 2>/dev/null"),
                "双入口都应含 PROMPT_COMMAND + history 记录方案"
            );
            assert!(
                !text.contains("trap __tltb_on_debug DEBUG"),
                "旧 DEBUG trap 方案（函数名自录 bug 的根源）不得再出现"
            );
        }

        // 幂等：重复安装不改变任何目标。
        hook.install(Path::new(r"D:\logs\terminals"))
            .expect("重复安装应幂等");
        assert_eq!(
            count_blocks(&std::fs::read_to_string(&rc).unwrap(), BASH_TAG),
            1
        );
        assert_eq!(
            count_blocks(&std::fs::read_to_string(&profile).unwrap(), BASH_TAG),
            1
        );

        hook.uninstall().expect("双入口卸载应成功");
        assert!(!rc.exists(), "自建 .bashrc 卸载后应删除");
        assert!(!profile.exists(), "自建 .bash_profile 卸载后应删除");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bash_hook_single_file_targets_never_touch_bash_profile() {
        let root = unique_temp("bash-hook-single");
        std::fs::create_dir_all(&root).unwrap();
        let rc = root.join(".bashrc");
        let profile = root.join(".bash_profile");

        // at() 单文件形态：.bash_profile 不在目标内，安装 / 卸载都不得触碰。
        let hook = BashHook::at(&rc);
        hook.install(Path::new(r"D:\logs")).unwrap();
        assert!(rc.exists());
        assert!(!profile.exists(), "单文件形态不得创建 .bash_profile");

        hook.uninstall().unwrap();
        assert!(!rc.exists());
        assert!(!profile.exists());

        let _ = std::fs::remove_dir_all(&root);
    }
}
