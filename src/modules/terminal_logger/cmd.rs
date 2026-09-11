//! # CMD（cmd.exe）会话记录（terminal_logger · CMD 钩子）
//!
//! cmd.exe **没有**原生「每命令执行 / 每屏幕输出」转录钩子（`PROMPT` 只展开
//! 文本、doskey 宏只在输入首词命中时触发、`AutoRun` 只在进程启动时执行一次）。
//! 本模块因此采用**原生非侵入**会话记录方案（不再包裹 cmd、不再经管道中继，
//! 见「为什么放弃回显捕获壳」）：
//!
//! 1. 在 exe 同级目录生成捕获脚本 `scripts\tltb_cmd_capture.bat`
//!    （[`capture_script_content`] / [`CmdHook::install`]）——**纯原生批处理**，
//!    不再生成任何 .ps1 捕获壳；
//! 2. 把 `call "<脚本绝对路径>"` 挂进 `HKCU\Software\Microsoft\Command
//!    Processor` 的 `AutoRun` 值（[`CmdHook::install`]），每个**新建**的
//!    cmd.exe 启动时都会先执行它；
//! 3. 捕获脚本只做 cmd 原生能力范围内、且**绝不触碰控制台 I/O** 的会话记录：
//!    - **会话头**：交互式 cmd 启动即创建
//!      `<log_base>\cmd\<HHMMSS>_<随机>.log` 并写入会话起始信息（时间 /
//!      用户 / 机器 / 当前目录）；
//!    - **doskey 会话记录机制**：安装一条 `exit` doskey 宏，把「结束会话」变成
//!      「先收尾日志再真正退出」——收尾时把本次会话**用户实际键入的全部命令**
//!      （`doskey /history`，方向键 / Tab / F7 原生历史即其来源）追加进日志并
//!      写入会话结束行（退出码经 `$*` 原样透传，`exit 3` 仍以 3 退出）；
//!    - **定制 PROMPT**：用户未自定义 `PROMPT` 时加 `[$T]` 时间徽标（旧版捕获
//!      壳内同样显示该徽标），提醒会话处于记录中；
//! 4. 挂载与卸载保持既有 AutoRun 语义（[`find_hook_span`] /
//!    [`autorun_value_with_hook`] / [`autorun_value_without_hook`] 的片段识别、
//!    链尾追加、无损还原全部沿用）。
//!
//! # 为什么放弃「PowerShell 回显捕获壳」（本次重构的根因）
//!
//! 旧实现把每个交互会话交给 `tltb_cmd_capture.ps1` 回显捕获壳：壳用管道
//! （`RedirectStandardInput/Output`）接管一个**嵌套** `cmd.exe /k`，逐行镜像
//! 屏幕输出并转发输入。Windows 下用常规进程管道中继交互式 cmd 有根本性缺陷：
//!
//! - cmd 的提示符输出**不带换行**，按行读取的壳会长期阻塞在「等一个永远
//!   不来的行结束」上——输入到达后提示符与输出才可能被误配对，表现为此起
//!   彼落的卡死 / 假死；
//! - cmd 在「stdin 非控制台」时切换成批处理式 I/O 语义（不回显、行为随
//!   stdin 句柄类型漂移），把交互会话塞进管道本质上是把它降级成脚本流；
//! - 壳内再派生嵌套 cmd，方向键历史 / Tab 补全 / F7 等原生行编辑全部失效，
//!   会话内读取 stdin 的交互程序（python REPL 等）还会直接吞掉转发输入。
//!
//! 新方案**彻底删除** `tltb_cmd_capture.ps1`（含其全部管道转发逻辑），改由
//! .bat 在 cmd 进程内做原生会话记录：不派生、不接管、不镜像，因此**绝无进程
//! 挂起与输入卡死**，方向键历史、Tab 补全与全套控制台功能原样保留。
//!
//! # 为什么需要 /c 护栏（防污染构建子进程）
//!
//! `AutoRun` 对**每一个** cmd.exe 进程都会触发——不仅是用户双击打开的交互式
//! 终端，还包括构建工具（cargo / git / npm 等）派生的无数 `cmd /c …` 非交互式
//! 子进程。捕获脚本的第一道护栏检测 `%CMDCMDLINE%` 是否含 `/c`（大小写均查）：
//! 含则 `goto :EOF` 立即退出，**绝不**记录任何非交互式子进程；只有不含 `/c`
//! 的 cmd.exe（用户从开始菜单 / Win+R / Windows Terminal / 资源管理器地址栏
//! 等启动的交互式会话）才进入记录流程。
//!
//! 实现细节（实测结论）：`CMDCMDLINE` 是 cmd 启动期的**动态变量**——AutoRun
//! 阶段经普通 `%CMDCMDLINE%` 展开可见，但延迟展开 `!CMDCMDLINE!` 与
//! `set CMDCMDLINE` 都读不到（它不在真实环境块里）；且把 `%CMDCMDLINE%` 直接
//! 放进 `if` 字符串替换比较会因命令行自带引号而误判。护栏因此采用
//! `echo(%CMDCMDLINE%| findstr /I /C:"/c" >nul && exit /b 0`——普通展开后交给
//! findstr 做**大小写不敏感**子串匹配（一并覆盖 `/c`、` /c `、`/C` 三种字面），
//! 命中即短路退出。短路用 **`exit /b 0` 而非 `goto :EOF`**：findstr 未命中时
//! 会把 `errorlevel` 置 1，若随后经 `goto :EOF` 直接返回，残留的 errorlevel 1
//! 会让用户敲裸 `exit` 时以非零码退出（实测）——显式 `exit /b 0` 保证守卫
//! 短路路径对会话退出码零影响。
//!
//! # 防递归嵌套护栏
//!
//! 已处于记录下的 cmd 会话（环境变量 `TLTB_CMD_LOGGED=1`，子进程继承）内部
//! 再启动的 cmd（用户在会话里敲 `cmd`、或从被记录的会话派生新 cmd）会再次
//! 触发 AutoRun。捕获脚本据此短路（`if defined TLTB_CMD_LOGGED exit /b 0`，
//! 同样显式清零退出码）；该检查排在 `/c` 护栏**之前**——它是纯 `if` 判定、
//! 不改变 errorlevel，而 findstr 会留残留 errorlevel，先跑 findstr 会污染
//! 短路路径的退出码。
//!
//! # AutoRun 值形态：为什么不用字面 `%~dp0`（实测结论）
//!
//! 注册表 `AutoRun` 命令串在 cmd.exe **启动期、任何批处理上下文之外**执行，
//! 实测（Windows 10/11）`%0` / `%~dp0` 在该语境下**不会展开**——会原样保留，
//! 导致 `call "%~dp0scripts\…"` 去调用一个名字里带 `%~dp0` 字面量的不存在的
//! 文件，每个 cmd 启动都报错。因此本模块写入 AutoRun 的是**安装期已解析的
//! 绝对路径**：`call "<exe_dir>\scripts\tltb_cmd_capture.bat"`（[`build_autorun_fragment`]）。
//!
//! 卸载时 [`find_hook_span`] 按「`call "…tltb_cmd_capture.bat"`」的**形状**识别
//! 我们写入的片段，因此同时覆盖三种情形：
//! - exe 被移动 / 升级后 AutoRun 里残留的**旧绝对路径**；
//! - 早期版本 / 手工按字面写下的 `call "%~dp0scripts\tltb_cmd_capture.bat"`
//!   （与规格字面一致的**历史形态**，同样按我们的脚本名收尾，一并清理）。
//!
//! # 脚本的编码策略（无 BOM 纯 ASCII + 路径按机器 OEM 代码页内嵌）
//!
//! 批处理文件**绝不加 UTF-8 BOM**：实测（Windows 10/11）cmd 在 **AutoRun 启动
//! 阶段**按 UTF-8 BOM 解析批处理并不可靠——BOM 会让首行 `@echo off` 失效，
//! 之后整份脚本在「stdin 为控制台、stdout 被重定向」的 cmd 子进程里被逐行
//! 回显（例如 PowerShell 里执行 `cmd /c …` 时捕获流被脚本内容污染，交互
//! 终端每次启动也会刷屏）。脚本因此**恒为纯 ASCII**：命令 / 注释全部 ASCII，
//! 任何控制台代码页下逐字可靠；唯一可能非 ASCII 的日志根路径 `<log_base>\cmd`
//! 在**安装期**由 [`oem_encode`] 转成**机器 OEM 代码页**字节后内嵌——cmd 在
//! 默认控制台（其代码页即系统 OEM 代码页）解析时逐字正确，中文路径在中文
//! 系统（cp936）上原样可用（实测）。用户在异代码页控制台（`chcp` / UTF-8
//! 终端）下打开 cmd 属已知边界：非 ASCII 路径段可能被按控制台代码页误读，
//! 记录器会把日志写到同名近似目录（仅影响落盘位置，绝不挂起、绝不打扰交互）。
//!
//! 安装期校验日志根路径**不得含 `%` / `!`**（cmd 解析批处理时会做变量 / 延迟
//! 展开且无法在 .bat 内可靠转义），返回 [`CmdHookError::InvalidLogBase`]。
//!
//! # AutoRun 的合并 / 卸载语义
//!
//! `AutoRun` 值可能已被其他工具占用（doskey 宏、其他钩子等），且支持
//! `&` 串联多命令（实测有效）。本模块按**链尾追加**合并：
//!
//! - [`autorun_value_with_hook`]：已有我们的片段（任意形态 / 任意历史位置）→
//!   整段移除后把当前规范的 `call "…"` 追加到链尾（幂等收敛，重复安装不叠放）；
//!   否则在用户内容后以 ` & ` 追加；
//! - [`autorun_value_without_hook`]：只删除我们片段的精确区间及紧邻的分隔符，
//!   其余用户命令字节保留；若删除后 AutoRun 值为空（整值即我们的钩子）→
//!   返回 `None`，调用方删除该值（还原挂载前的「无值」状态，见
//!   [`CmdHook::uninstall`]）。
//!
//! 注意：AutoRun 命令之间是纯命令序列，不保证 `&&` 短路语义；卸载时片段紧邻
//! 的多余分隔符（`&` / 换行 / 空白）会被归一为单个 ` & `。
//!
//! # 记录失败永不打扰用户会话
//!
//! 捕获脚本全程在 cmd 进程内执行且失败全部自带降级：目录建失败 / 日志写失败
//! 只影响日志内容，doskey 宏定义失败（用户已有同名 `exit` 宏时主动不覆盖）只
//! 让会话退化为「仅会话头」记录——任何情况下 cmd 本身照常交互，绝不挂起。
//!
//! # 已知边界
//!
//! - `cmd /c …` 启动的会话按规格**一律不记录**（构建子进程的防干扰优先级高于
//!   覆盖度）；
//! - cmd 没有原生每命令钩子，因此**逐命令的屏幕输出不可捕获**；会话日志沉淀
//!   的是「会话头 + 用户实际键入的命令清单 + 会话尾」，键入清单在会话正常
//!   结束（敲 `exit`，经 doskey 宏收尾）时落盘——直接关窗 / 强杀会话只剩会话头；
//!   - 若用户已自定义 doskey `exit` 宏，为不破坏用户宏，记录器**不覆盖**它，
//!     该会话同样退化为「仅会话头」记录；
//! - doskey 宏与 `doskey /history` 仅在**真实交互式控制台**输入下生效（stdin
//!   被重定向的 cmd 不展开宏、不记历史，属 cmd 固有行为）——记录对象是真实
//!   交互会话，非交互形态不受影响；
//! - 会话日志中键入命令的字节遵循 cmd 内建 `doskey /history` 的输出（控制台
//!   代码页编码），非 ASCII 命令在异代码页下可能以该代码页字节落盘；
//! - 每会话一个日志文件（文件名取 `HHMMSS` + 双随机，并发会话互不覆写）；
//! - [`CmdHook::uninstall`] 只清理注册表，**不删除**已生成的捕获脚本与历史
//!   日志（正在运行的会话可能仍持有脚本句柄 / 用户日志属用户数据）；
//! - exe 所在路径含 `%` 或 `!` 时无法安全嵌入 AutoRun 命令行（cmd 启动期会做
//!   变量展开且无法转义），安装返回
//!   [`CmdHookError::UnsafeAutorunPath`]（卸载不受影响）。

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// 常量与 TAG
// ---------------------------------------------------------------------------

/// cmd.exe AutoRun 注册表键（相对 `HKEY_CURRENT_USER`）。
pub const AUTORUN_KEY_PATH: &str = r"Software\Microsoft\Command Processor";

/// AutoRun 值名（cmd.exe 启动时自动执行的命令串）。
pub const AUTORUN_VALUE_NAME: &str = "AutoRun";

/// 捕获脚本相对 exe 所在目录的子目录名（脚本落在 `<exe_dir>\scripts`）。
pub const CAPTURE_SCRIPT_REL_DIR: &str = "scripts";

/// 捕获脚本固定文件名（.bat，AutoRun 直接 call 它；纯原生批处理，无 .ps1）。
pub const CAPTURE_SCRIPT_FILE_NAME: &str = "tltb_cmd_capture.bat";

/// 历史版本生成的捕获壳固定文件名（.ps1，本次重构后已彻底废除）。安装时会
/// 尽力删除同目录残留的旧文件（升级前由旧版 install 生成、仍在磁盘上的）。
pub const LEGACY_CAPTURE_PS1_FILE_NAME: &str = "tltb_cmd_capture.ps1";

/// 防递归环境变量名（捕获脚本据此短路重复记录）。
pub const CMD_RECURSION_GUARD_ENV: &str = "TLTB_CMD_LOGGED";

/// 日志根目录下的 cmd 子目录名（会话日志落在 `<log_base>\cmd`）。
pub const CMD_LOG_SUBDIR: &str = "cmd";

/// 捕获脚本「收尾模式」的开关参数（doskey `exit` 宏以该参数 call 本脚本，
/// 脚本据此跳过守卫、直达收尾逻辑）。与规格字面一致，供测试引用。
pub const FINALIZE_SWITCH: &str = "__TLTB_CMD_FINALIZE__";

/// 与规格字面一致的**历史形态** AutoRun 片段（`%~dp0` 在 AutoRun 语境不展开，
/// 见模块文档）；卸载识别器按脚本名收尾的形状统一清理它。
pub const LEGACY_AUTORUN_FRAGMENT: &str = r#"call "%~dp0scripts\tltb_cmd_capture.bat""#;

/// 捕获脚本模板中日志根路径的占位符（替换为 `<log_base>\cmd` 的文本；模板
/// 除该占位符外恒为纯 ASCII，非 ASCII 路径段由 [`oem_encode`] 按机器 OEM
/// 代码页转字节后内嵌，见模块文档「脚本的编码策略」）。
const LOG_ROOT_PLACEHOLDER: &str = "__LOG_ROOT__";

// ---------------------------------------------------------------------------
// 错误模型
// ---------------------------------------------------------------------------

/// CMD 钩子操作错误（携带失败路径 / 操作名，便于 UI / 日志直接展示）。
#[derive(Debug)]
pub enum CmdHookError {
    /// 当前平台不支持注册表 AutoRun 挂载（非 Windows 的编译期兜底分支）。
    UnsupportedPlatform,
    /// 无法定位当前可执行文件（`std::env::current_exe` 失败或父目录缺失）。
    ExeDirUnavailable {
        /// 底层 IO 错误。
        source: io::Error,
    },
    /// `log_base` 无法安全内嵌进批处理脚本。
    InvalidLogBase {
        /// 触发失败的路。
        path: PathBuf,
        /// 面向用户的说明（如“不得包含换行 / NUL”）。
        hint: &'static str,
    },
    /// 脚本绝对路径含 cmd AutoRun 启动期会展开且无法转义的字符（`%` / `!`）。
    UnsafeAutorunPath {
        /// 触发失败的路径。
        path: PathBuf,
        /// 禁止出现的字符（如 `%`、`!`）。
        forbidden: char,
    },
    /// 建目录 / 写临时文件 / 原子替换失败。
    Io {
        /// 触发失败的文件路径。
        path: PathBuf,
        /// 底层 IO 错误。
        source: io::Error,
    },
    /// Win32 注册表调用失败。
    Registry {
        /// 失败的操作（`RegCreateKeyExW` / `RegSetValueExW` / `RegDeleteValueW`…）。
        operation: &'static str,
        /// Win32 错误码（0 = ERROR_SUCCESS，此处恒非 0）。
        code: u32,
    },
}

impl fmt::Display for CmdHookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                write!(f, "CMD 会话录制仅支持 Windows（注册表 AutoRun 挂载）")
            }
            Self::ExeDirUnavailable { source } => {
                write!(f, "无法定位当前可执行文件目录: {source}")
            }
            Self::InvalidLogBase { path, hint } => {
                write!(f, "日志根路径不合法 '{}'：{hint}", path.display())
            }
            Self::UnsafeAutorunPath { path, forbidden } => write!(
                f,
                "捕获脚本路径 '{}' 含 `{forbidden}`，无法安全写入 cmd AutoRun 命令行 \
                 （cmd 启动期会展开该字符且无法转义）",
                path.display()
            ),
            Self::Io { path, source } => write!(f, "写入失败 '{}': {source}", path.display()),
            Self::Registry { operation, code } => {
                write!(f, "注册表操作失败: {operation}（Win32 错误码 {code}）")
            }
        }
    }
}

impl std::error::Error for CmdHookError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ExeDirUnavailable { source } | Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// CMD 钩子操作的统一结果别名。
pub type CmdHookResult<T> = Result<T, CmdHookError>;

// ---------------------------------------------------------------------------
// 纯函数层：捕获脚本内容生成（跨平台可单测的确定性核心）
// ---------------------------------------------------------------------------

/// 捕获 .bat 模板（LF 占位形态；字节层生成时转 CRLF）。模板**恒为纯 ASCII**
/// ——cmd 在 AutoRun 启动阶段对 UTF-8 BOM 批处理的解析不可靠（BOM 会让首行
/// `@echo off` 失效、整份脚本被逐行回显，实测见模块文档「脚本的编码策略」），
/// 故绝不加 BOM；唯一可能非 ASCII 的日志根路径由 [`oem_encode`] 按机器 OEM
/// 代码页转字节后填入 `__LOG_ROOT__`，cmd 在默认控制台（代码页即系统 OEM
/// 代码页）解析时逐字正确。
///
/// 模板内容即**原生非侵入会话记录**（详见模块文档，不包裹、不派生、不接管
/// 控制台 I/O）：
/// 1. `__TLTB_CMD_FINALIZE__` 收尾分支须在一切守卫**之前**判定（收尾 call 发生
///    在已置位 `TLTB_CMD_LOGGED` 的会话内，若先走守卫会被短路）；
/// 2. `/c` 护栏——`echo(%CMDCMDLINE%| findstr /I /C:"/c"`（`%CMDCMDLINE%` 为
///    cmd 启动期的**动态变量，仅普通展开可见**；/c 大小写不敏感匹配）命中即
///    `goto :EOF`，构建派生的非交互式子进程绝不进入记录流程；
/// 3. `TLTB_CMD_LOGGED` 防递归护栏——记录会话内再启动的 cmd（环境继承）短路；
/// 4. 置位 `TLTB_CMD_LOGGED=1` → 创建 `<log_base>\cmd\cmd_HHMMSS_随机.log`
///    并写会话头（时间 / 用户 / 机器 / 当前目录）→ 定义 doskey `exit` 收尾宏
///    （`call "%~f0" __TLTB_CMD_FINALIZE__ $T exit $*`，退出码经 `$*` 透传；
///    用户已有同名宏时不覆盖）→ 用户未自定义 `PROMPT` 时加 `[$T]` 时间徽标；
/// 5. `:finalize` 收尾：写会话尾 + `doskey /history` 键入命令清单后返回
///    （真正退出由宏体 `exit $*` 完成）。
const CAPTURE_SCRIPT_TEMPLATE: &str = r###"@echo off
rem ============================================================================
rem  TLToolBox cmd.exe session recorder (AutoRun hook) - MANAGED FILE.
rem  Regenerate with terminal_logger::cmd::CmdHook::install(); manual edits are
rem  lost on the next install.
rem
rem  NATIVE, NON-INTRUSIVE session recorder. This file NEVER wraps cmd.exe, never
rem  touches its console I/O and never spawns a capture shell, so it cannot hang
rem  or steal input: arrow-key history, Tab completion and every native console
rem  feature keep working unchanged. It only uses what cmd.exe offers natively:
rem    * AutoRun fires once when an INTERACTIVE cmd starts  -> session header;
rem    * a doskey macro overrides the typed "exit" line     -> session footer;
rem    * "doskey /history" dumps the commands typed in this session into the log.
rem  Recording scope is therefore the session command list (what the user typed,
rem  captured at session end). cmd.exe has no per-command output hook, and piping
rem  its I/O used to deadlock interactive cmd, so screen output is intentionally
rem  not captured.
rem
rem  Hard guards (never remove):
rem   1) AutoRun fires for EVERY new cmd.exe - including the countless
rem      "cmd /c ..." subprocesses spawned by build tools (cargo, git, npm...).
rem      The dynamic CMDCMDLINE variable carries the launching command line
rem      (available during AutoRun via normal percent expansion only), so the
rem      line below bails out as soon as "/c" appears in it (any case). The
rem      short-circuit uses "exit /b 0", never a bare goto :EOF: findstr leaves
rem      errorlevel=1 when it finds nothing, which would otherwise leak into the
rem      caller and make a plain "exit" at the prompt return 1.
rem   2) A cmd started inside an already recorded session inherits
rem      TLTB_CMD_LOGGED=1 and must not create a second recorder. This check
rem      comes FIRST: it is a plain "if defined" (errorlevel-neutral), while the
rem      findstr above would poison errorlevel and corrupt guard path exit codes.
rem
rem  Encoding: pure ASCII, NO BOM. (cmd's AutoRun-phase parsing of UTF-8-BOM
rem  batch files is unreliable: the BOM breaks the first "@echo off" line and the
rem  whole script gets echoed line-by-line into redirected output.) The embedded
rem  log-root path line is ASCII when the log root is ASCII, otherwise it holds
rem  the machine OEM code page bytes of the path - matching the code page of the
rem  default console. Install rejects log roots containing % or !.
rem ============================================================================

rem ---- session-end finalize (invoked by the doskey "exit" macro) ----
if /I "%~1"=="__TLTB_CMD_FINALIZE__" goto :finalize

rem ---- guard 2 (checked first, errorlevel-neutral): recursion ----
rem A cmd started inside an already recorded session inherits TLTB_CMD_LOGGED=1;
rem bail with exit /b 0 so a bare "exit" at the prompt still returns 0.
if defined TLTB_CMD_LOGGED exit /b 0

rem ---- guard 1: never touch "cmd /c ..." build subprocesses ----
rem findstr sets errorlevel to 1 on "no match", which would leak into the caller
rem (a plain "exit" then returns 1); short-circuit with an explicit exit /b 0.
echo(%CMDCMDLINE%| findstr /I /C:"/c" >nul && exit /b 0
set "TLTB_CMD_LOGGED=1"

rem ---- session start: pick a fresh log file and write the session header ----
set "TLTB_CMD_LOG_DIR=__LOG_ROOT__"
if not exist "%TLTB_CMD_LOG_DIR%\" mkdir "%TLTB_CMD_LOG_DIR%" >nul 2>&1
rem v0.6.2 (L7): name = centisecond time + two RANDOM segments (cmd.exe has no
rem PID env var); two segments reduce same-instant collision to ~1e-9.
set "TLTB_CMD_LOG=%TLTB_CMD_LOG_DIR%\cmd_%TIME:~0,2%%TIME:~3,2%%TIME:~6,2%%TIME:~9,2%_%RANDOM%_%RANDOM%.log"
>> "%TLTB_CMD_LOG%" echo # TLToolBox cmd session start: %DATE% %TIME%
>> "%TLTB_CMD_LOG%" echo # user: "%USERDOMAIN%\%USERNAME%"  machine: "%COMPUTERNAME%"
>> "%TLTB_CMD_LOG%" echo # cwd: "%CD%"

rem ---- native session-end hook: flush the typed commands and close the log ----
rem (skip it when the user already has their own doskey "exit" macro)
doskey /macros | findstr /B /I "exit=" >nul 2>&1
if errorlevel 1 doskey exit=call "%~f0" __TLTB_CMD_FINALIZE__ $T exit $*

rem ---- visible time beacon (only when the user has no custom PROMPT) ----
if not defined PROMPT set "PROMPT=[$T]$P$G "

exit /b 0

:finalize
>> "%TLTB_CMD_LOG%" echo # TLToolBox cmd session end: %DATE% %TIME%
>> "%TLTB_CMD_LOG%" echo # commands typed in this session:
doskey /history >> "%TLTB_CMD_LOG%" 2>nul
exit /b 0
"###;

/// 把 UTF-8 路径文本转成 cmd 批处理内嵌字节：
///
/// - 纯 ASCII → 原样 ASCII 字节（任何代码页下逐字可靠）；
/// - 含非 ASCII → Windows 上经 [`oem_encode`] 按机器 OEM 代码页编码（cmd 在
///   默认控制台即 OEM 代码页下解析正确，中文路径在中文系统原样可用）；
///   非 Windows 平台无法取得 OEM 代码页，以 UTF-8 字节兜底（.bat 只服务
///   Windows，兜底仅保证跨平台测试可运行）。
pub fn batch_path_bytes(text: &str) -> Vec<u8> {
    if text.is_ascii() {
        return text.as_bytes().to_vec();
    }
    oem_encode(text)
}

/// 机器 OEM 代码页编码（Windows）：`WideCharToMultiByte(CP_OEMCP)`，无法表示
/// 的字符按系统默认策略（通常 `?`）占位。非 Windows 平台退回 UTF-8（见
/// [`batch_path_bytes`]）。
#[cfg(windows)]
fn oem_encode(text: &str) -> Vec<u8> {
    use windows::Win32::Globalization::WideCharToMultiByte;
    // CP_OEMCP = 1：系统 OEM 代码页（与默认控制台代码页一致）。
    const CP_OEMCP: u32 = 1;

    let wide: Vec<u16> = text.encode_utf16().collect();
    // SAFETY: 探测调用（lpMultiByteStr=None → 返回所需字节数）与写入调用
    // （缓冲长度即探测值）均在调用期间持有所有权。
    unsafe {
        let needed =
            WideCharToMultiByte(CP_OEMCP, 0, &wide, None, windows::core::PCSTR::null(), None);
        if needed <= 0 {
            return text.as_bytes().to_vec(); // 兜底：编码失败按 UTF-8 输出
        }
        let mut out = vec![0u8; needed as usize];
        let written = WideCharToMultiByte(
            CP_OEMCP,
            0,
            &wide,
            Some(&mut out),
            windows::core::PCSTR::null(),
            None,
        );
        out.truncate(written.max(0) as usize);
        out
    }
}

/// 机器 OEM 代码页编码（非 Windows 兜底：UTF-8）。
#[cfg(not(windows))]
fn oem_encode(text: &str) -> Vec<u8> {
    text.as_bytes().to_vec()
}

/// 捕获 .bat 的落盘字节（CRLF 行尾；**无 BOM**，命令 / 注释恒为纯 ASCII，
/// 内嵌的日志根路径段为 ASCII 或机器 OEM 代码页字节，见 [`batch_path_bytes`]）。
///
/// 内容语义（详见模块文档）：
/// 1. `__TLTB_CMD_FINALIZE__` 收尾分支（doskey `exit` 宏 call 本脚本的入口，
///    写会话尾 + `doskey /history` 键入命令清单），置于守卫之前；
/// 2. `/c` 护栏与 `TLTB_CMD_LOGGED` 防递归护栏（守卫语义不变）；
/// 3. 会话头（建目录 / 写起始行）+ doskey `exit` 收尾宏 + 默认 `[$T]` PROMPT；
/// 4. 日志根路径 `<log_base>\cmd` 以 ASCII / OEM 字节填入占位符（不再经
///    base64 / .ps1 转交——整份脚本自给自足）。
///
/// 路径含 `\r` / `\n` / `\0` / `%` / `!` 时报 [`CmdHookError::InvalidLogBase`]
/// （`%` / `!` 会被 cmd 在解析 .bat 时展开且无法安全转义）。
pub fn capture_script_bytes(log_base: &Path) -> CmdHookResult<Vec<u8>> {
    let lossy = log_base.to_string_lossy();
    if lossy.contains(['\r', '\n', '\0']) {
        return Err(CmdHookError::InvalidLogBase {
            path: log_base.to_path_buf(),
            hint: "日志根路径不得包含换行或 NUL（无法内嵌进批处理脚本）",
        });
    }
    if lossy.contains(['%', '!']) {
        return Err(CmdHookError::InvalidLogBase {
            path: log_base.to_path_buf(),
            hint: "日志根路径不得包含 % 或 !（cmd 解析批处理时会展开该字符且无法转义）",
        });
    }
    let log_root = log_base.join(CMD_LOG_SUBDIR);
    let (head, tail) = CAPTURE_SCRIPT_TEMPLATE
        .split_once(LOG_ROOT_PLACEHOLDER)
        .unwrap_or_else(|| panic!("捕获脚本模板缺少占位符 `{LOG_ROOT_PLACEHOLDER}`"));
    let mut bytes = Vec::with_capacity(head.len() + tail.len() + 64);
    bytes.extend_from_slice(head.as_bytes());
    bytes.extend_from_slice(&batch_path_bytes(&log_root.to_string_lossy()));
    bytes.extend_from_slice(tail.as_bytes());
    // LF → CRLF（模板不含孤立 CR；占位符两侧都可能在路径外）。
    let mut out = Vec::with_capacity(bytes.len() + 16);
    for &b in &bytes {
        if b == b'\n' {
            out.extend_from_slice(b"\r\n");
        } else {
            out.push(b);
        }
    }
    debug_assert!(
        !out.windows(LOG_ROOT_PLACEHOLDER.len())
            .any(|w| w == LOG_ROOT_PLACEHOLDER.as_bytes()),
        "占位符必须被替换"
    );
    Ok(out)
}

/// 捕获 .bat 的 UTF-8 视图文本（仅供纯 ASCII 日志根路径的调用方 / 测试使用；
/// 含非 ASCII 路径时请用 [`capture_script_bytes`]——内嵌字节为 OEM 代码页，
/// 无法无损表示为 UTF-8 文本）。
pub fn capture_script_content(log_base: &Path) -> CmdHookResult<String> {
    let bytes = capture_script_bytes(log_base)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// 捕获脚本绝对路径：`<exe_dir>\scripts\tltb_cmd_capture.bat`。
pub fn capture_script_path(exe_dir: &Path) -> PathBuf {
    exe_dir
        .join(CAPTURE_SCRIPT_REL_DIR)
        .join(CAPTURE_SCRIPT_FILE_NAME)
}

/// 历史版本捕获壳（旧版 install 生成、本次重构已废除）的绝对路径：
/// `<exe_dir>\scripts\tltb_cmd_capture.ps1`。
pub fn legacy_ps1_path(exe_dir: &Path) -> PathBuf {
    exe_dir
        .join(CAPTURE_SCRIPT_REL_DIR)
        .join(LEGACY_CAPTURE_PS1_FILE_NAME)
}

/// 尽力删除旧版残留的捕获壳 `tltb_cmd_capture.ps1`（升级前由旧版 install
/// 生成的同目录文件，含管道转发逻辑的脚本本体已不再需要）。文件缺失 /
/// 删除失败一律静默——只做清理，不构成安装错误。
pub fn remove_legacy_capture_ps1(exe_dir: &Path) {
    let stale = legacy_ps1_path(exe_dir);
    if let Err(source) = std::fs::remove_file(&stale) {
        if source.kind() != io::ErrorKind::NotFound {
            tracing::warn!(
                target: "terminal_logger",
                "清理旧版捕获壳失败 '{}': {source}（可手工删除）",
                stale.display()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 纯函数层：AutoRun 值的片段识别 / 合并 / 卸载（跨平台可单测）
// ---------------------------------------------------------------------------

/// 由捕获脚本绝对路径构造规范 AutoRun 片段：`call "<绝对路径>"`。
///
/// 路径含 `%` / `!` 时报 [`CmdHookError::UnsafeAutorunPath`]——这两个字符在
/// cmd 启动期解析 AutoRun 命令串时会被展开（`%VAR%` 变量、延迟展开 `!VAR!`）
/// 且命令行语境无法转义，内嵌必然破坏路径。`"` 不可能出现在 Windows 文件
/// 路径中，无需处理引号嵌套。
pub fn build_autorun_fragment(script_path: &Path) -> CmdHookResult<String> {
    let text = script_path.to_string_lossy();
    for forbidden in ['%', '!'] {
        if text.contains(forbidden) {
            return Err(CmdHookError::UnsafeAutorunPath {
                path: script_path.to_path_buf(),
                forbidden,
            });
        }
    }
    Ok(format!("call \"{text}\""))
}

/// 在 AutoRun 值中定位「属于本模块的 call 片段」的字节区间 `[start, end)`
/// （含 `call ` 关键字与外层双引号，整段可直接剥除）。识别规则按**形状**
/// 而非精确路径，因此同时命中：
///
/// - 规范形态 `call "D:\…\scripts\tltb_cmd_capture.bat"`（含 exe 移动后的旧
///   绝对路径残留——按文件名 + `scripts\` 前缀识别，路径本身不参与匹配）；
/// - 历史 / 字面形态 `call "%~dp0scripts\tltb_cmd_capture.bat"`（`%~dp0` 在
///   AutoRun 语境不展开，属无效残留，同样以脚本名收尾故一并清理）。
///
/// 判定约束（防止误删用户内容）：
/// - 脚本名必须是**完整路径分量**：前一字符为 `\` 或 `/`，后一字符为 `"`（或
///   空白后 `"`）——`tltb_cmd_capture.bat.bak` 之类名字不会误命中；
/// - 片段前必须是 `call ` 关键字且二者之间无其他 `"`（不跨其他命令）。
pub fn find_hook_span(value: &str) -> Option<(usize, usize)> {
    const NAME: &str = "tltb_cmd_capture.bat";
    let lower = value.to_ascii_lowercase();
    let mut search_from = 0usize;
    while search_from < value.len() {
        let rel = lower[search_from..].find(NAME)?;
        let name_start = search_from + rel;
        let name_end = name_start + NAME.len();

        // 完整路径分量：前一位是分隔符。
        let before_name = &value[..name_start];
        if !(before_name.ends_with('\\') || before_name.ends_with('/')) {
            search_from = name_end;
            continue;
        }
        // 后一位（允许空白）是片段结束引号——排除 .bat.bak 之类长尾名字。
        let after_name = value[name_end..].trim_start_matches([' ', '\t']);
        if !after_name.starts_with('"') {
            search_from = name_end;
            continue;
        }
        // 最近的前置引号之后到名字之间不得再有引号，且引号前是 `call `。
        let Some(open_quote) = before_name.rfind('"') else {
            search_from = name_end;
            continue;
        };
        if !before_name[..open_quote]
            .to_ascii_lowercase()
            .ends_with("call ")
        {
            search_from = name_end;
            continue;
        }
        let close_quote = name_end
            + (value.len() - name_end - after_name.len())
            + after_name.find('"').expect("starts_with(\" \") 已保证存在")
            + 1;
        // 区间从 `call ` 关键字起始（含关键字）：移除时整段剥除，不留悬空关键字。
        return Some((open_quote - "call ".len(), close_quote));
    }
    None
}

/// 卸载语义的确定性核心：移除 AutoRun 值中**所有**属于本模块的片段后返回
/// 剩余内容；返回 `None` 表示剩余为空——调用方应删除整个 AutoRun 值。
///
/// 值中本就没有我们的片段 → `Some(原文)`（逐字节不变，调用方据此跳过写入）。
/// 移除单个片段时清理其紧邻分隔符（`&` / 换行 / 空白；残余两侧均有内容时以
/// 单个 ` & ` 重新连接，多 `&` / 换行分隔会被归一，见模块文档）。
pub fn autorun_value_without_hook(current: &str) -> Option<String> {
    if find_hook_span(current).is_none() {
        return Some(current.to_string());
    }
    let mut rest = current.to_string();
    while let Some((start, end)) = find_hook_span(&rest) {
        let prefix = rest[..start].trim_end_matches([' ', '\t', '\r', '\n', '&']);
        let suffix = rest[end..].trim_start_matches([' ', '\t', '\r', '\n', '&']);
        rest = match (prefix.is_empty(), suffix.is_empty()) {
            // 片段即整值（两侧均无残余）→ 删除整个值。
            (true, true) => return None,
            (true, false) => suffix.to_string(),
            (false, true) => prefix.to_string(),
            (false, false) => format!("{prefix} & {suffix}"),
        };
    }
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 挂载语义的确定性核心：把规范的 `call "…"` 片段合并进 AutoRun 值。
///
/// - 值中已有我们的片段（规范 / 历史 / 旧路径任意形态）→ 全部移除后把当前
///   规范片段**追加到链尾**（幂等：重复安装不叠放；exe 移动后自动原位更新）；
/// - 否则保留用户内容逐字节，以 ` & ` 追加到链尾；值为空 → 片段即整值。
pub fn autorun_value_with_hook(current: &str, fragment: &str) -> String {
    let rest = autorun_value_without_hook(current).unwrap_or_default();
    let rest = rest.trim_end();
    if rest.is_empty() {
        return fragment.to_string();
    }
    if rest.ends_with('&') {
        format!("{rest} {fragment}")
    } else {
        format!("{rest} & {fragment}")
    }
}

// ---------------------------------------------------------------------------
// 文件读写：捕获脚本的原子落盘（幂等：内容未变不触碰 mtime）
// ---------------------------------------------------------------------------

/// 把捕获脚本内容原子写入 `script_path`（父目录自动创建）。`content` 为
/// [`capture_script_bytes`] 产出的**落盘字节**（CRLF、无 BOM、纯 ASCII /
/// OEM 路径段）。与现文件逐字节一致时跳过（幂等，不触碰 mtime）；否则走
/// 「同目录临时文件 + 原子改名」，任意时刻磁盘上都是完整文件。
pub fn write_capture_script(script_path: &Path, content: &[u8]) -> CmdHookResult<()> {
    if let Ok(existing) = std::fs::read(script_path) {
        if existing == content {
            return Ok(());
        }
    }
    if let Some(parent) = script_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|source| CmdHookError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let file_name = script_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("系统时钟应晚于 UNIX 纪元")
        .as_nanos();
    let tmp = script_path.with_file_name(format!(
        ".{file_name}.tltb{}{nanos}.tmp",
        std::process::id()
    ));
    std::fs::write(&tmp, content).map_err(|source| CmdHookError::Io {
        path: tmp.clone(),
        source,
    })?;
    if let Err(source) = std::fs::rename(&tmp, script_path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(CmdHookError::Io {
            path: script_path.to_path_buf(),
            source,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 挂载器（面向装配层的公共入口）
// ---------------------------------------------------------------------------

/// cmd.exe 会话记录挂载器（AutoRun + 原生非侵入 .bat 会话记录）。
///
/// 锚点目录为 exe 所在目录：捕获脚本落在 `<exe_dir>\scripts\tltb_cmd_capture.bat`
/// （单一 .bat，不再生成 .ps1 捕获壳），AutoRun 片段指向其绝对路径。
/// [`install`](Self::install) 先原子生成 .bat 并清理旧版残留 .ps1、再合并
/// 注册表 AutoRun 值（顺序保证注册表永远不会指向缺失脚本）；
/// [`uninstall`](Self::uninstall) 只清理注册表片段（保留脚本与日志，见模块
/// 文档）。
#[derive(Debug, Clone)]
pub struct CmdHook {
    exe_dir: PathBuf,
}

impl CmdHook {
    /// 以当前可执行文件所在目录为锚点（exe 移动后需重新 install 以更新
    /// AutoRun 中的绝对路径；uninstall 不受影响）。
    pub fn new() -> CmdHookResult<Self> {
        let exe =
            std::env::current_exe().map_err(|source| CmdHookError::ExeDirUnavailable { source })?;
        let exe_dir =
            exe.parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| CmdHookError::ExeDirUnavailable {
                    source: io::Error::new(io::ErrorKind::NotFound, "当前可执行文件没有父目录"),
                })?;
        Ok(Self { exe_dir })
    }

    /// 显式指定锚点目录（测试注入临时目录 / 便携安装时使用）。
    pub fn at(exe_dir: impl Into<PathBuf>) -> Self {
        Self {
            exe_dir: exe_dir.into(),
        }
    }

    /// 当前钩子锚定的 exe 目录。
    pub fn exe_dir(&self) -> &Path {
        &self.exe_dir
    }

    /// 捕获脚本绝对路径（`<exe_dir>\scripts\tltb_cmd_capture.bat`）。
    pub fn script_path(&self) -> PathBuf {
        capture_script_path(&self.exe_dir)
    }

    /// 历史版本捕获壳（旧版 install 生成、本次重构已废除）的绝对路径
    /// （`<exe_dir>\scripts\tltb_cmd_capture.ps1`，install 时尽力清理）。
    pub fn legacy_ps1_path(&self) -> PathBuf {
        legacy_ps1_path(&self.exe_dir)
    }

    /// 挂载 cmd 会话记录：原子生成原生 .bat 捕获脚本（含 `/c` 护栏、防递归
    /// 护栏与内嵌的 ASCII / OEM 字节日志根路径）→ 清理旧版残留的 .ps1 捕获壳
    /// → 把 `call "<脚本绝对路径>"` 合并进 AutoRun 值（幂等，见
    /// [`autorun_value_with_hook`]）。非 Windows 平台返回
    /// [`CmdHookError::UnsupportedPlatform`]。
    pub fn install(&self, log_base: &Path) -> CmdHookResult<()> {
        #[cfg(windows)]
        {
            let content = capture_script_bytes(log_base)?;
            write_capture_script(&self.script_path(), &content)?;
            // 旧版捕获壳已废除：升级时把同目录残留一并清除。
            remove_legacy_capture_ps1(&self.exe_dir);
            let fragment = build_autorun_fragment(&self.script_path())?;
            let current = win32::read_autorun()?.unwrap_or_default();
            let next = autorun_value_with_hook(&current, &fragment);
            if next != current {
                win32::write_autorun(&next)?;
            }
            tracing::debug!(
                target: "terminal_logger",
                "cmd 会话记录钩子已挂载: AutoRun += '{}'（log_base: '{}'）",
                fragment,
                log_base.display()
            );
            Ok(())
        }
        #[cfg(not(windows))]
        {
            let _ = (log_base,);
            Err(CmdHookError::UnsupportedPlatform)
        }
    }

    /// 卸载 cmd 会话录制：移除 AutoRun 值中属于我们的片段；值因移除而变空 →
    /// 删除该值；从未挂载 / 键值缺失 → 空操作（幂等）。非 Windows 平台返回
    /// [`CmdHookError::UnsupportedPlatform`]。
    pub fn uninstall(&self) -> CmdHookResult<()> {
        #[cfg(windows)]
        {
            let Some(current) = win32::read_autorun()? else {
                tracing::debug!(
                    target: "terminal_logger",
                    "cmd 会话录制卸载: AutoRun 值不存在，空操作"
                );
                return Ok(());
            };
            match autorun_value_without_hook(&current) {
                None => {
                    // 整值即我们的钩子：删除值，还原挂载前的「无值」状态。
                    win32::delete_autorun()?;
                }
                Some(next) if next != current => {
                    win32::write_autorun(&next)?;
                }
                Some(_) => {
                    tracing::debug!(
                        target: "terminal_logger",
                        "cmd 会话录制卸载: AutoRun 中无本模块片段，空操作"
                    );
                }
            }
            tracing::debug!(
                target: "terminal_logger",
                "cmd 会话录制钩子已卸载（AutoRun）"
            );
            Ok(())
        }
        #[cfg(not(windows))]
        {
            Err(CmdHookError::UnsupportedPlatform)
        }
    }

    /// 只读探测：AutoRun 值当前是否含本模块的 call 片段（任意形态 / 路径）。
    /// 注册表不可读 / 值缺失时一律返回 `false`（宽松语义，供 UI 状态展示）。
    pub fn is_installed(&self) -> bool {
        #[cfg(windows)]
        {
            win32::read_autorun()
                .ok()
                .flatten()
                .is_some_and(|value| find_hook_span(&value).is_some())
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
}

/// Windows 原生实现：AutoRun 注册表读写（advapi32）。
#[cfg(windows)]
mod win32 {
    use super::*;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
        RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_EXPAND_SZ,
        REG_OPTION_NON_VOLATILE, REG_SZ, REG_VALUE_TYPE,
    };

    /// 字符串 → 以 `\0` 结尾的 UTF-16LE 单元序列（注册表 API 的载荷形态）。
    fn to_wide_units(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 以 `\0` 结尾的 UTF-16LE 单元序列 → 字节切片（`REG_SZ` 写入载荷，
    /// 字节序固定为小端以匹配 Windows 内存布局）。
    fn units_to_bytes(units: &[u16]) -> Vec<u8> {
        units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
    }

    /// 字节载荷 → 字符串（截断尾随 `\0`，非法代理对按 U+FFFD 容忍——注册表
    /// 值来自本模块自身写入，正常为合法 UTF-16LE）。
    fn bytes_to_units(bytes: &[u8]) -> Vec<u16> {
        bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect()
    }

    /// RAII 句柄守卫：`Drop` 时自动 `RegCloseKey`，杜绝错误路径上键句柄泄漏。
    struct KeyGuard(HKEY);

    impl Drop for KeyGuard {
        fn drop(&mut self) {
            // SAFETY: 句柄来自 RegOpenKeyExW / RegCreateKeyExW 的成功返回；
            // 关闭无效句柄仅返回错误码，无未定义行为。
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }

    /// 打开（存在时）或创建 AutoRun 所在键，返回句柄守卫。
    ///
    /// `HKCU\Software\Microsoft\Command Processor` 默认**不存在**（实测），
    /// 首次挂载必须先创建；只读探测路径上键缺失返回 `Ok(None)`。
    fn open_key(create: bool) -> CmdHookResult<Option<KeyGuard>> {
        let key_path = to_wide_units(AUTORUN_KEY_PATH);
        let mut key = HKEY::default();
        let sam = KEY_QUERY_VALUE | KEY_SET_VALUE;
        let code = if create {
            // SAFETY: key_path 为 NUL 结尾的 UTF-16 缓冲，其指针在本调用期间
            // 存活；sam 仅含查询 / 写入权限位；安全描述符与创建处置不关心。
            unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    PCWSTR(key_path.as_ptr()),
                    0,
                    None,
                    REG_OPTION_NON_VOLATILE,
                    sam,
                    None,
                    &mut key,
                    None,
                )
            }
        } else {
            // SAFETY: key_path 为 NUL 结尾的 UTF-16 缓冲，其指针在本调用期间
            // 存活；samdesired 仅含查询 / 写入权限位。
            unsafe {
                RegOpenKeyExW(
                    HKEY_CURRENT_USER,
                    PCWSTR(key_path.as_ptr()),
                    0,
                    sam,
                    &mut key,
                )
            }
        };
        if code == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if code != ERROR_SUCCESS {
            let operation = if create {
                "RegCreateKeyExW"
            } else {
                "RegOpenKeyExW"
            };
            return Err(CmdHookError::Registry {
                operation,
                code: code.0,
            });
        }
        Ok(Some(KeyGuard(key)))
    }

    /// 读取 AutoRun 值：`Ok(None)` = 键或值缺失 / 值非字符串类型 / 空串。
    /// 非字符串类型（如 REG_DWORD）对 cmd 无意义，按「无值」处理——挂载时
    /// 会以 REG_SZ 覆写，卸载时空操作不动它。
    pub(super) fn read_autorun() -> CmdHookResult<Option<String>> {
        let Some(key) = open_key(false)? else {
            return Ok(None);
        };
        let value_name = to_wide_units(AUTORUN_VALUE_NAME);

        // 第一趟：仅探测所需字节长度与类型。
        let mut value_type = REG_VALUE_TYPE(0);
        let mut size: u32 = 0;
        // SAFETY: 探测调用不写入数据（lpdata = None）；指针参数均为可选空指针。
        let code = unsafe {
            RegQueryValueExW(
                key.0,
                PCWSTR(value_name.as_ptr()),
                None,
                Some(&mut value_type),
                None,
                Some(&mut size),
            )
        };
        if code == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if code != ERROR_SUCCESS {
            return Err(CmdHookError::Registry {
                operation: "RegQueryValueExW(探测)",
                code: code.0,
            });
        }
        if value_type != REG_SZ && value_type != REG_EXPAND_SZ {
            return Ok(None);
        }

        // 第二趟：按探测长度读取完整数据。
        let mut buf = vec![0u8; size as usize];
        // SAFETY: buf 长度即 size（探测返回值），写入不会越界；value_type 指向
        // 栈上变量，供调用方核对存储类型。
        let code = unsafe {
            RegQueryValueExW(
                key.0,
                PCWSTR(value_name.as_ptr()),
                None,
                Some(&mut value_type),
                Some(buf.as_mut_ptr()),
                Some(&mut size),
            )
        };
        if code != ERROR_SUCCESS {
            return Err(CmdHookError::Registry {
                operation: "RegQueryValueExW(读取)",
                code: code.0,
            });
        }

        buf.truncate(size as usize);
        let mut units = bytes_to_units(&buf);
        while units.last() == Some(&0) {
            units.pop(); // 去掉结尾 `\0`
        }
        let text = String::from_utf16_lossy(&units);
        if text.is_empty() {
            return Ok(None); // 空串视为无有效值
        }
        Ok(Some(text))
    }

    /// 只读探测：AutoRun 所在键当前是否存在（测试还原现场用；读失败按不存在
    /// 处理，宽松语义）。
    #[cfg(test)]
    pub(super) fn autorun_key_exists() -> bool {
        open_key(false).map(|key| key.is_some()).unwrap_or(false)
    }

    /// 删除 AutoRun 所在整键（仅测试自清理使用：安装前键不存在时，卸载后把
    /// 重建的空键一并移除，完整还原现场）。键不存在视为成功。
    #[cfg(test)]
    pub(super) fn delete_autorun_key() -> CmdHookResult<()> {
        use windows::Win32::System::Registry::RegDeleteKeyW;
        let key_path = to_wide_units(AUTORUN_KEY_PATH);
        // SAFETY: key_path 为 NUL 结尾的 UTF-16 缓冲，其指针在本调用期间存活。
        let code = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(key_path.as_ptr())) };
        if code != ERROR_SUCCESS && code != ERROR_FILE_NOT_FOUND {
            return Err(CmdHookError::Registry {
                operation: "RegDeleteKeyW",
                code: code.0,
            });
        }
        Ok(())
    }

    /// 写入 AutoRun 值（REG_SZ；键缺失时自动创建）。
    pub(super) fn write_autorun(value: &str) -> CmdHookResult<()> {
        let key = open_key(true)?.expect("创建模式下键句柄必然存在");
        let value_name = to_wide_units(AUTORUN_VALUE_NAME);
        let data = units_to_bytes(&to_wide_units(value));
        // SAFETY: value_name / data 均为本函数持有的 NUL 结尾缓冲，调用期间
        // 存活；REG_SZ 要求数据含结尾 NUL，data 已满足。
        let code = unsafe {
            RegSetValueExW(
                key.0,
                PCWSTR(value_name.as_ptr()),
                0,
                REG_SZ,
                Some(data.as_slice()),
            )
        };
        if code != ERROR_SUCCESS {
            return Err(CmdHookError::Registry {
                operation: "RegSetValueExW",
                code: code.0,
            });
        }
        Ok(())
    }

    /// 删除 AutoRun 值（键或值不存在视为成功——幂等卸载）。
    pub(super) fn delete_autorun() -> CmdHookResult<()> {
        let Some(key) = open_key(false)? else {
            return Ok(());
        };
        let value_name = to_wide_units(AUTORUN_VALUE_NAME);
        // SAFETY: value_name 为 NUL 结尾缓冲，调用期间存活。
        let code = unsafe { RegDeleteValueW(key.0, PCWSTR(value_name.as_ptr())) };
        if code != ERROR_SUCCESS && code != ERROR_FILE_NOT_FOUND {
            return Err(CmdHookError::Registry {
                operation: "RegDeleteValueW",
                code: code.0,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 系统临时目录下本次测试独有的目录路径（并行测试互不干扰）。
    fn unique_temp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("系统时钟应晚于 UNIX 纪元")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "tltoolbox-cmd-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    /// 从捕获 .bat 中提取内嵌的日志根路径文本（`set "TLTB_CMD_LOG_DIR=<path>"` 行
    /// 引号内载荷；供**纯 ASCII** 日志根路径的断言使用——非 ASCII 段为 OEM
    /// 代码页字节，请走 [`capture_script_bytes`] + [`oem_decode_for_test`]）。
    fn embedded_log_root(content: &str) -> String {
        let marker = "set \"TLTB_CMD_LOG_DIR=";
        let start = content.find(marker).expect("脚本应含日志根路径行") + marker.len();
        let end = content[start..]
            .find('"')
            .expect("日志根路径应以双引号收尾")
            + start;
        content[start..end].to_string()
    }

    /// 从捕获 .bat 字节中提取内嵌的日志根路径段（供非 ASCII 路径断言）。
    fn embedded_log_root_bytes(bytes: &[u8]) -> &[u8] {
        let marker = b"set \"TLTB_CMD_LOG_DIR=";
        let start = bytes
            .windows(marker.len())
            .position(|w| w == marker)
            .expect("脚本应含日志根路径行")
            + marker.len();
        let end = start
            + bytes[start..]
                .iter()
                .position(|&b| b == b'"')
                .expect("日志根路径应以双引号收尾");
        &bytes[start..end]
    }

    /// 检查捕获 .bat 的「批处理格式合法性」并返回问题清单（空 = 合法）。
    /// 无损 / 有损两个编码分支共用：无论内嵌路径段是 OEM 双字节还是 '?' 替换
    /// 字节，生成的脚本都必须保持 cmd 可正常解析的结构（行结构 / 引号配对 /
    /// CRLF 行尾 / 段外恒为纯 ASCII / 占位符不泄漏）。
    fn find_batch_format_problems(bytes: &[u8], segment: &[u8]) -> Vec<String> {
        let mut problems = Vec::new();
        let marker = b"set \"TLTB_CMD_LOG_DIR=";
        let Some(marker_at) = bytes.windows(marker.len()).position(|w| w == marker) else {
            problems.push("缺少日志根路径行标记 `set \"TLTB_CMD_LOG_DIR=`".into());
            return problems;
        };
        let seg_start = marker_at + marker.len();
        if bytes.get(seg_start..seg_start + segment.len()) != Some(segment) {
            problems.push("内嵌路径段与断言段不一致".into());
        }
        let after = seg_start + segment.len();
        if bytes.get(after) != Some(&b'"') {
            problems.push("路径段之后缺失收尾双引号（引号结构被破坏）".into());
        }
        if segment.contains(&b'"') {
            problems.push("路径段内含双引号（引号结构被破坏）".into());
        }
        if segment.contains(&b'\r') || segment.contains(&b'\n') || segment.contains(&0) {
            problems.push("路径段内含 CR / LF / NUL（行结构被破坏）".into());
        }
        // 段之外必须恒为纯 ASCII（模板无 BOM、无其它非 ASCII 字节）。
        let head_ascii = bytes[..seg_start].iter().all(u8::is_ascii);
        let tail_ascii = bytes
            .get(after + 1..)
            .is_none_or(|tail| tail.iter().all(u8::is_ascii));
        if !head_ascii || !tail_ascii {
            problems.push("路径段之外存在非 ASCII 字节".into());
        }
        // 行尾统一 CRLF：无裸 LF、无孤立 CR。
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\r' => {
                    if bytes.get(i + 1) != Some(&b'\n') {
                        problems.push("存在孤立 CR（行尾应统一 CRLF）".into());
                        break;
                    }
                    i += 2;
                }
                b'\n' => {
                    problems.push("存在裸 LF（行尾应统一 CRLF）".into());
                    break;
                }
                _ => i += 1,
            }
        }
        // 占位符不得泄漏。
        if bytes
            .windows(LOG_ROOT_PLACEHOLDER.len())
            .any(|w| w == LOG_ROOT_PLACEHOLDER.as_bytes())
        {
            problems.push("日志根路径占位符泄漏".into());
        }
        problems
    }

    /// 机器 OEM 代码页编号（仅测试侧）：`GetOEMCP()`——en-US CI 虚拟机为 437
    /// （US-ASCII），本地中文系统为 936（GBK）。测试据此对非 ASCII 日志根路径
    /// 的编码结果分支断言（见 [`capture_script_oem_embeds_non_ascii_log_root`]）。
    #[cfg(windows)]
    fn oem_codepage_for_test() -> u32 {
        use windows::Win32::Globalization::GetOEMCP;
        // SAFETY: GetOEMCP 无参数、无前置条件，恒可调用；返回系统 OEM 代码页编号。
        unsafe { GetOEMCP() }
    }

    /// 以**显式代码页**编码（仅测试侧模拟，不依赖系统当前代码页）：与生产
    /// [`oem_encode`] 同一 Win32 调用形态，仅代码页由参数指定——用于在任意
    /// 机器上确定性地模拟 en-US CI（OEMCP=437）等异代码页环境的编码结果。
    #[cfg(windows)]
    fn oem_encode_with_cp_for_test(text: &str, codepage: u32) -> Vec<u8> {
        use windows::Win32::Globalization::WideCharToMultiByte;
        let wide: Vec<u16> = text.encode_utf16().collect();
        // SAFETY: 探测调用（lpMultiByteStr=None → 返回所需字节数）与写入调用
        // （缓冲长度即探测值）均在调用期间持有所有权。
        unsafe {
            let needed =
                WideCharToMultiByte(codepage, 0, &wide, None, windows::core::PCSTR::null(), None);
            if needed <= 0 {
                return text.as_bytes().to_vec(); // 与生产一致：失败按 UTF-8 兜底
            }
            let mut out = vec![0u8; needed as usize];
            let written = WideCharToMultiByte(
                codepage,
                0,
                &wide,
                Some(&mut out),
                windows::core::PCSTR::null(),
                None,
            );
            out.truncate(written.max(0) as usize);
            out
        }
    }

    /// 以**显式代码页**解码（仅测试侧模拟，不依赖系统当前代码页）。
    #[cfg(windows)]
    fn oem_decode_with_cp_for_test(bytes: &[u8], codepage: u32) -> String {
        use windows::Win32::Globalization::{MultiByteToWideChar, MULTI_BYTE_TO_WIDE_CHAR_FLAGS};
        // SAFETY: 探测调用（lpWideCharStr=None → 返回所需宽字符数）与写入调用
        // （缓冲长度即探测值）均在调用期间持有所有权。
        unsafe {
            let needed =
                MultiByteToWideChar(codepage, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, None);
            if needed <= 0 {
                return String::new();
            }
            let mut out = vec![0u16; needed as usize];
            let written = MultiByteToWideChar(
                codepage,
                MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0),
                bytes,
                Some(&mut out),
            );
            out.truncate(written.max(0) as usize);
            String::from_utf16_lossy(&out)
        }
    }

    /// 按机器 OEM 代码页解码字节（仅测试侧：Windows 用
    /// `MultiByteToWideChar(CP_OEMCP)` 还原非 ASCII 日志根路径）。
    #[cfg(windows)]
    fn oem_decode_for_test(bytes: &[u8]) -> String {
        oem_decode_with_cp_for_test(bytes, 1 /* CP_OEMCP */)
    }

    // ---- 捕获脚本内容生成（原生非侵入方案）----

    #[test]
    fn capture_script_is_crlf_guarded_and_native() {
        let content = capture_script_content(Path::new(r"D:\logs\terminals")).unwrap();
        // 行尾统一 CRLF（cmd 对 LF 批处理存在解析怪癖，统一 CRLF 规避）。
        assert!(content.starts_with("@echo off\r\n"), "首行应为 @echo off");
        assert!(
            content.ends_with("exit /b 0\r\n"),
            "末行应为收尾分支的 exit /b 0"
        );
        let stripped = content.replace("\r\n", "");
        assert!(!stripped.contains(['\n', '\r']), "不得出现裸 LF / 孤立 CR");
        // 日志根路径纯 ASCII 时整份脚本保持纯 ASCII。
        assert!(content.is_ascii(), "ASCII 日志根路径下脚本应保持纯 ASCII");

        // /c 护栏：%CMDCMDLINE%（动态变量，普通展开可见）经 findstr 大小写
        // 不敏感匹配 /c，命中即短路——绝不拦截非交互式子进程。短路用显式
        // `exit /b 0`（findstr 未命中会留 errorlevel=1，经 goto :EOF 返回会让
        // 裸 exit 以非零码退出——实测，见模块文档）。
        assert!(content.contains(r#"echo(%CMDCMDLINE%| findstr /I /C:"/c" >nul && exit /b 0"#));
        assert!(
            !content.contains("!CMDCMDLINE!"),
            "CMDCMDLINE 为动态变量，延迟展开不可见，护栏必须用普通展开"
        );
        // 防递归护栏 + 置位。递归检查必须排在 /c 护栏（findstr）之前——它是
        // 纯 if 判定、不污染 errorlevel；findstr 的残留 errorlevel 会破坏短路
        // 路径的退出码。
        assert!(content.contains("if defined TLTB_CMD_LOGGED exit /b 0"));
        assert!(content.contains("set \"TLTB_CMD_LOGGED=1\""));
        let recursion_at = content
            .find("if defined TLTB_CMD_LOGGED exit /b 0")
            .expect("应含防递归护栏");
        assert!(
            recursion_at < content.find("findstr /I /C:\"/c\"").unwrap(),
            "防递归护栏必须排在 /c 护栏（findstr）之前"
        );
        assert!(
            !content.contains("if defined TLTB_CMD_LOGGED goto :EOF"),
            "防递归护栏必须 exit /b 0，不得 goto :EOF（残留 errorlevel）"
        );

        // 收尾分支必须先于一切守卫判定（收尾 call 发生在已置位防递归变量的
        // 会话内，若先走守卫会被短路）。
        let finalize_at = content
            .find(r#"if /I "%~1"=="__TLTB_CMD_FINALIZE__" goto :finalize"#)
            .expect("应含收尾分支开关行");
        assert!(
            finalize_at < content.find("findstr /I /C:\"/c\"").unwrap(),
            "收尾分支必须排在 /c 护栏之前"
        );
        assert!(content.contains(":finalize"));

        // 会话头：建目录 + 写起始行（时间 / 用户 / 机器 / 当前目录）。
        assert!(content.contains("if not exist \"%TLTB_CMD_LOG_DIR%\\\" mkdir"));
        assert!(content.contains("cmd session start"));
        assert!(content.contains("%USERDOMAIN%\\%USERNAME%"));
        assert!(content.contains("%CD%"));

        // doskey 会话记录机制：exit 宏（收尾 + 真实退出，退出码经 $* 透传）
        // + 收尾时 doskey /history 键入命令清单落盘。
        assert!(content.contains("doskey exit=call \"%~f0\" __TLTB_CMD_FINALIZE__ $T exit $*"));
        assert!(content.contains("doskey /history >> \"%TLTB_CMD_LOG%\""));
        assert!(content.contains("cmd session end"));
        // 用户已有同名 exit 宏时不覆盖。
        assert!(content.contains("doskey /macros | findstr /B /I \"exit=\""));
        assert!(content.contains("if errorlevel 1 doskey exit="));
        // 定制 PROMPT：默认（用户未自定义）加 [$T] 时间徽标。
        assert!(content.contains("if not defined PROMPT set \"PROMPT=[$T]$P$G \""));

        // 旧「PowerShell 回显捕获壳」逻辑已彻底移除：无 .ps1、无管道接管、
        // 无嵌套 cmd、无 Start-Transcript。
        assert!(!content.contains("tltb_cmd_capture.ps1"));
        assert!(!content.contains("powershell.exe"));
        assert!(!content.contains("RedirectStandardInput"));
        assert!(!content.contains("RedirectStandardOutput"));
        assert!(!content.contains("Start-Transcript"));
        assert!(!content.contains("cmd.exe /k"));
        assert!(!content.contains("-Command"));

        // 占位符不得泄漏。
        assert!(!content.contains(LOG_ROOT_PLACEHOLDER));
    }

    #[test]
    fn capture_script_embeds_log_root_as_cmd_subdir() {
        let content = capture_script_content(Path::new(r"D:\TLToolBox\logs\terminals")).unwrap();
        // 内嵌的是 <log_base>\cmd 的 Windows 原生文本路径。
        assert_eq!(
            embedded_log_root(&content),
            r"D:\TLToolBox\logs\terminals\cmd"
        );
    }

    #[test]
    fn capture_script_oem_embeds_non_ascii_log_root() {
        // 中文路径：Windows 上按机器 OEM 代码页内嵌字节，cmd 在默认控制台解析
        // 正确；非 Windows 平台以 UTF-8 兜底。
        //
        // 跨区域兼容（GitHub Actions windows-latest 为 en-US，OEMCP=437，中文
        // 无法映射为 OEM 字节）：不硬编码「当前环境一定能转出 GBK 字节」，而按
        // 机器实际 OEMCP 分支——
        //   * 无损（cp936 等可表示中文的代码页）：断言 OEM 解码逐字还原，cp936
        //     下再断言具体 GBK 双字节（张 = D5 C5，三 = C8 FD）；
        //   * 有损（cp437 等西文代码页，不可表示字符被替换为 '?'）：断言降级 /
        //     替换逻辑，且脚本格式仍合法。
        let path_text = r"C:\Users\张三\O'Brien\logs";
        let expected = r"C:\Users\张三\O'Brien\logs\cmd";
        let bytes = capture_script_bytes(Path::new(path_text)).unwrap();
        let segment = embedded_log_root_bytes(&bytes);
        // 格式合法性两个分支都必须满足（行结构 / 引号 / CRLF / 段外纯 ASCII）。
        let problems = find_batch_format_problems(&bytes, segment);
        assert!(problems.is_empty(), ".bat 脚本格式不合法: {problems:?}");

        #[cfg(windows)]
        {
            // 机器 OEM 代码页：本地中文系统 936（GBK），en-US CI 虚拟机 437。
            let oemcp = oem_codepage_for_test();
            let decoded = oem_decode_for_test(segment);
            if decoded == expected {
                // ---- 无损分支：代码页可表示中文，OEM 解码逐字还原 ----
                assert_eq!(decoded, expected, "OEM 解码应还原原始日志根路径");
                if oemcp == 936 {
                    // cp936 (GBK) 的具体双字节断言：张 = D5 C5、三 = C8 FD，
                    // 而非仅泛泛断言「能转出字节」。
                    let mut want = Vec::new();
                    want.extend_from_slice(br"C:\Users\");
                    want.extend_from_slice(&[0xD5, 0xC5]); // 张（GBK 双字节）
                    want.extend_from_slice(&[0xC8, 0xFD]); // 三（GBK 双字节）
                    want.extend_from_slice(br"\O'Brien\logs\cmd");
                    assert_eq!(segment, &want[..], "cp936 下中文路径段应按 GBK 双字节内嵌");
                } else {
                    // 其它支持多字节 CJK 的代码页（cp932 / 950 / 949 / 65001 等）：
                    // 双字节证据——两个中文字符各编码为 >1 字节（含 ≥0x80 的高位
                    // 字节特征）。
                    let non_ascii = segment.iter().filter(|&&b| !b.is_ascii()).count();
                    assert!(
                        non_ascii >= 4 && segment.iter().any(|&b| b >= 0x80),
                        "多字节代码页下中文应编码为双字节（至少 4 个非 ASCII 字节）: \
                         {segment:02X?}"
                    );
                }
            } else {
                // ---- 有损分支（cp437 等西文代码页，或 CJK 字符不可表示）----
                // 降级 / 替换逻辑：不可表示字符按系统默认策略替换为 '?'，ASCII
                // 部分原样保留；脚本格式合法性已由上面的统一断言覆盖。
                assert_ne!(decoded, expected, "有损分支不应还原原始路径");
                assert!(
                    decoded.contains('?'),
                    "不可表示字符应替换为 '?'（系统默认策略）: {decoded:?}"
                );
                assert!(
                    decoded.starts_with(r"C:\Users\") && decoded.ends_with(r"\O'Brien\logs\cmd"),
                    "ASCII 部分应原样保留: {decoded:?}"
                );
            }
        }
        #[cfg(not(windows))]
        {
            assert_eq!(
                std::str::from_utf8(segment).unwrap(),
                r"C:\Users\张三\O'Brien\logs\cmd"
            );
        }
    }

    /// 模拟 en-US CI（OEMCP=437）的降级输出：**不依赖系统当前代码页**，用显式
    /// CP437 对同一路径编码（中文 → '?'），把结果原位替换进真实脚本后验证——
    /// 降级 / 替换逻辑成立，且生成的 .bat 仍是格式合法的批处理。本测试在任意
    /// 机器（含本地 cp936）上都确定性地覆盖「非 936 代码页」路径。
    #[cfg(windows)]
    #[test]
    fn capture_script_oem_lossy_cp437_replacement_keeps_batch_legal() {
        let path_text = r"C:\Users\张三\O'Brien\logs";
        let expected = r"C:\Users\张三\O'Brien\logs\cmd";
        let bytes = capture_script_bytes(Path::new(path_text)).unwrap();
        let segment = embedded_log_root_bytes(&bytes);

        // 显式 CP437 编码（等价于 en-US 机器上 oem_encode 的输出）：不可表示的
        // 中文字符按默认策略替换为 '?'，ASCII 部分原样。
        let cp437_segment = oem_encode_with_cp_for_test(expected, 437);
        assert!(
            cp437_segment.iter().all(u8::is_ascii),
            "cp437 下非 ASCII 输入应全部替换为 ASCII 占位: {cp437_segment:02X?}"
        );
        assert_eq!(
            oem_decode_with_cp_for_test(&cp437_segment, 437),
            r"C:\Users\??\O'Brien\logs\cmd",
            "cp437 下每个不可表示字符应替换为单个 '?'"
        );

        // 用 437 形态段原位替换真实段：段之外均为模板 ASCII 字节，替换结果与
        // 「在 437 机器上由 capture_script_bytes 生成的脚本」逐字节一致。
        let seg_start = segment.as_ptr() as usize - bytes.as_ptr() as usize;
        let mut simulated = Vec::with_capacity(bytes.len() - segment.len() + cp437_segment.len());
        simulated.extend_from_slice(&bytes[..seg_start]);
        simulated.extend_from_slice(&cp437_segment);
        simulated.extend_from_slice(&bytes[seg_start + segment.len()..]);

        // 降级输出后脚本格式必须仍然合法（cmd 可正常解析）。
        let problems = find_batch_format_problems(&simulated, &cp437_segment);
        assert!(
            problems.is_empty(),
            "437 降级输出的 .bat 格式不合法: {problems:?}"
        );
        assert!(
            simulated.iter().all(u8::is_ascii),
            "437 下整份脚本应保持纯 ASCII（无 BOM、无其它非 ASCII 字节）"
        );
    }

    #[test]
    fn capture_script_rejects_unsafe_log_base() {
        // 换行 / NUL：无法内嵌。
        for bad in ["bad\npath", "bad\rpath", "bad\0path"] {
            let err = capture_script_content(Path::new(bad)).unwrap_err();
            assert!(matches!(err, CmdHookError::InvalidLogBase { .. }), "{err}");
        }
        // % / !：cmd 解析批处理时会展开且无法在 .bat 内可靠转义。
        for bad in [r"D:\logs\100% done", r"D:\logs\!important"] {
            let err = capture_script_content(Path::new(bad)).unwrap_err();
            assert!(matches!(err, CmdHookError::InvalidLogBase { .. }), "{err}");
        }
    }

    #[test]
    fn script_paths_and_legacy_ps1_cleanup() {
        let exe_dir = Path::new(r"D:\TLToolBox");
        assert_eq!(
            capture_script_path(exe_dir),
            PathBuf::from(r"D:\TLToolBox\scripts\tltb_cmd_capture.bat")
        );
        assert_eq!(
            legacy_ps1_path(exe_dir),
            PathBuf::from(r"D:\TLToolBox\scripts\tltb_cmd_capture.ps1")
        );

        // 尽力清理旧版 .ps1：存在 → 删除；缺失 / 不可删 → 静默。
        let root = unique_temp("legacy-ps1");
        let ps1 = root.join("scripts").join(LEGACY_CAPTURE_PS1_FILE_NAME);
        std::fs::create_dir_all(ps1.parent().unwrap()).unwrap();
        std::fs::write(&ps1, "# stale").unwrap();
        remove_legacy_capture_ps1(&root);
        assert!(!ps1.exists(), "旧版捕获壳应被清理");
        remove_legacy_capture_ps1(&root); // 再删（缺失）应静默
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- AutoRun 片段构造 ----

    #[test]
    fn autorun_fragment_quotes_absolute_script_path() {
        assert_eq!(
            build_autorun_fragment(Path::new(r"D:\TLToolBox\scripts\tltb_cmd_capture.bat"))
                .unwrap(),
            r#"call "D:\TLToolBox\scripts\tltb_cmd_capture.bat""#
        );
        assert_eq!(
            build_autorun_fragment(Path::new(
                r"C:\Program Files\TLToolBox\scripts\tltb_cmd_capture.bat"
            ))
            .unwrap(),
            r#"call "C:\Program Files\TLToolBox\scripts\tltb_cmd_capture.bat""#
        );
    }

    #[test]
    fn autorun_fragment_rejects_percent_and_bang() {
        // `%` / `!` 在 cmd 启动期解析 AutoRun 命令串时会被展开且无法转义。
        for bad in [
            r"D:\TLToolBox\50%\scripts\tltb_cmd_capture.bat",
            r"D:\TLToolBox\!x\scripts\tltb_cmd_capture.bat",
        ] {
            let err = build_autorun_fragment(Path::new(bad)).unwrap_err();
            assert!(
                matches!(err, CmdHookError::UnsafeAutorunPath { .. }),
                "{err}"
            );
        }
    }

    // ---- 片段识别（find_hook_span）----

    #[test]
    fn find_hook_span_locates_canonical_fragment() {
        let value = r#"doskey h=history & call "D:\TLToolBox\scripts\tltb_cmd_capture.bat""#;
        let (start, end) = find_hook_span(value).expect("应识别规范片段");
        assert_eq!(
            &value[start..end],
            r#"call "D:\TLToolBox\scripts\tltb_cmd_capture.bat""#,
            "区间应含 call 关键字与整段引号路径"
        );
    }

    #[test]
    fn find_hook_span_locates_legacy_literal_dp0_fragment() {
        // 规格字面形态（%~dp0 在 AutoRun 语境不展开，属历史残留，仍须可清理）。
        let value = r#"call "%~dp0scripts\tltb_cmd_capture.bat""#;
        let (start, end) = find_hook_span(value).expect("应识别历史字面形态");
        assert_eq!(&value[start..end], value, "整值即片段时区间应覆盖全值");
    }

    #[test]
    fn find_hook_span_locates_relocated_old_path() {
        // exe 移动后 AutoRun 里残留的旧绝对路径：按脚本名形状识别，路径无关。
        let value = r#"call "D:\Old\Location\scripts\tltb_cmd_capture.bat" & set X=1"#;
        let (start, end) = find_hook_span(value).expect("应识别旧路径残留");
        assert!(&value[start..end].ends_with(r#"tltb_cmd_capture.bat""#));
    }

    #[test]
    fn find_hook_span_is_case_insensitive_and_tolerates_mid_chain() {
        let value = r#"rem my hook & CALL "d:\tl\scripts\TLTB_CMD_CAPTURE.BAT" & echo ok"#;
        let (start, end) = find_hook_span(value).expect("大小写与链中位置均应识别");
        assert_eq!(
            &value[start..end],
            r#"CALL "d:\tl\scripts\TLTB_CMD_CAPTURE.BAT""#,
            "关键字大小写不敏感且区间含关键字"
        );
    }

    #[test]
    fn find_hook_span_ignores_lookalike_names_and_foreign_calls() {
        // 相似文件名（.bat.bak 长尾 / 别的脚本名）不误命中。
        assert!(find_hook_span(r#"call "D:\x\scripts\tltb_cmd_capture.bat.bak""#).is_none());
        assert!(find_hook_span(r#"call "D:\x\scripts\other_capture.bat""#).is_none());
        // 非 call 形态（echo 等）不误命中。
        assert!(find_hook_span(r#"echo D:\x\scripts\tltb_cmd_capture.bat"#).is_none());
        // 完全无关的值。
        assert!(find_hook_span("doskey /macrofile=C:\\macros.doskey").is_none());
    }

    // ---- 合并（autorun_value_with_hook）----

    #[test]
    fn hook_into_empty_value_is_just_the_fragment() {
        let fragment = r#"call "D:\TL\scripts\tltb_cmd_capture.bat""#;
        assert_eq!(autorun_value_with_hook("", fragment), fragment);
        assert_eq!(autorun_value_with_hook("   ", fragment), fragment);
    }

    #[test]
    fn hook_appends_after_user_content_with_separator() {
        let fragment = r#"call "D:\TL\scripts\tltb_cmd_capture.bat""#;
        assert_eq!(
            autorun_value_with_hook("doskey /macrofile=C:\\m.doskey", fragment),
            format!("doskey /macrofile=C:\\m.doskey & {fragment}")
        );
    }

    #[test]
    fn hook_is_idempotent_and_moves_legacy_to_canonical_tail() {
        let canonical = r#"call "D:\TL\scripts\tltb_cmd_capture.bat""#;
        // 已含规范片段（链尾）→ 逐字节不变。
        let once = format!("set X=1 & {canonical}");
        assert_eq!(autorun_value_with_hook(&once, canonical), once);

        // 已含历史字面形态 → 收敛为链尾规范片段，用户内容保留。
        let legacy = r#"set X=1 & call "%~dp0scripts\tltb_cmd_capture.bat""#;
        let merged = autorun_value_with_hook(legacy, canonical);
        assert_eq!(merged, format!("set X=1 & {canonical}"));

        // 重复安装不叠放。
        let twice = autorun_value_with_hook(&merged, canonical);
        assert_eq!(twice, merged);
    }

    // ---- 卸载（autorun_value_without_hook）----

    #[test]
    fn uninstall_without_hook_is_noop_byte_exact() {
        let value = "doskey /macrofile=C:\\m.doskey";
        assert_eq!(autorun_value_without_hook(value), Some(value.to_string()));
    }

    #[test]
    fn uninstall_whole_value_ours_yields_none() {
        let canonical = r#"call "D:\TL\scripts\tltb_cmd_capture.bat""#;
        assert_eq!(autorun_value_without_hook(canonical), None);
        // 历史字面形态同。
        assert_eq!(
            autorun_value_without_hook(r#"call "%~dp0scripts\tltb_cmd_capture.bat""#),
            None
        );
    }

    #[test]
    fn uninstall_keeps_user_content_and_cleans_separators() {
        // 链尾：清掉前导 ` & `。
        assert_eq!(
            autorun_value_without_hook(&format!(
                "set X=1 & {}",
                r#"call "D:\TL\scripts\tltb_cmd_capture.bat""#
            )),
            Some("set X=1".to_string())
        );
        // 链首：清掉后随 ` & `。
        assert_eq!(
            autorun_value_without_hook(&format!(
                r#"{} & set X=1"#,
                r#"call "D:\TL\scripts\tltb_cmd_capture.bat""#
            )),
            Some("set X=1".to_string())
        );
        // 链中：两侧内容以 ` & ` 接回。
        assert_eq!(
            autorun_value_without_hook(&format!(
                "a & {} & b",
                r#"call "%~dp0scripts\tltb_cmd_capture.bat""#
            )),
            Some("a & b".to_string())
        );
        // 多片段收敛（历史遗留双份残留）。
        let doubled = format!(
            "a & {} & {} & b",
            r#"call "D:\old\scripts\tltb_cmd_capture.bat""#,
            r#"call "D:\new\scripts\tltb_cmd_capture.bat""#
        );
        assert_eq!(
            autorun_value_without_hook(&doubled),
            Some("a & b".to_string())
        );
        // 换行分隔同样归一。
        let newline = format!(
            "first\r\n{}\r\nsecond",
            r#"call "D:\TL\scripts\tltb_cmd_capture.bat""#
        );
        assert_eq!(
            autorun_value_without_hook(&newline),
            Some("first & second".to_string())
        );
    }

    #[test]
    fn hook_and_unhook_roundtrip_exactly() {
        let original = "doskey /macrofile=C:\\m.doskey";
        let fragment = r#"call "D:\TL\scripts\tltb_cmd_capture.bat""#;
        let hooked = autorun_value_with_hook(original, fragment);
        assert_eq!(
            autorun_value_without_hook(&hooked),
            Some(original.to_string())
        );
    }

    // ---- 捕获脚本落盘（幂等 / 原子 / 自建目录）----

    #[test]
    fn write_capture_script_creates_parents_idempotently() {
        let root = unique_temp("script-write");
        let script = root
            .join("nested")
            .join("scripts")
            .join(CAPTURE_SCRIPT_FILE_NAME);
        let content = capture_script_bytes(Path::new(r"D:\logs\terminals")).unwrap();

        write_capture_script(&script, &content).expect("首次写入应成功");
        assert!(script.exists(), "父目录应自动创建");
        let raw = std::fs::read(&script).unwrap();
        assert_eq!(
            raw, content,
            "落盘字节应与生成字节逐字节一致（CRLF、无 BOM、纯 ASCII / OEM 段）"
        );
        let first_mtime = std::fs::metadata(&script).unwrap().modified().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(20));
        write_capture_script(&script, &content).expect("同内容重复写入应幂等");
        let second_mtime = std::fs::metadata(&script).unwrap().modified().unwrap();
        assert_eq!(first_mtime, second_mtime, "幂等写入不得触碰 mtime");

        let other = capture_script_bytes(Path::new(r"D:\other\logs")).unwrap();
        write_capture_script(&script, &other).expect("内容变化应可更新");
        let raw2 = std::fs::read(&script).unwrap();
        let text = std::str::from_utf8(&raw2).unwrap();
        assert!(embedded_log_root(text).ends_with("other\\logs\\cmd"));

        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- 实机注册表往返（默认忽略：会读写当前用户 AutoRun 键，仅显式运行）----

    /// 真实的 install → 注册表校验 → 旧版 .ps1 清理 → cmd /c 无干扰验证 →
    /// 「交互形态」cmd 会话头落盘（无挂死）→ uninstall 往返。测试自清理：
    /// 无论断言成败都会还原 AutoRun 与键的原始状态。
    /// **注意**：与同模块其余 `real_` 测试一样会读写真实 AutoRun 键，必须
    /// **串行**运行（`--test-threads=1`），否则并行互相覆盖注册表值。
    #[cfg(windows)]
    #[test]
    #[ignore = "读写当前用户真实 AutoRun 键并短暂挂载钩子，需显式运行（--ignored --test-threads=1）"]
    fn real_registry_install_uninstall_self_cleaning() {
        use std::process::Command;

        // 快照现场（键是否存在 + AutoRun 原值），无论断言成败都据此还原。
        let existed = win32::autorun_key_exists();
        let prev = win32::read_autorun().expect("读取 AutoRun 应成功");

        let root = unique_temp("real-hook");
        let log_base = root.join("logs").join("terminals");
        let hook = CmdHook::at(&root);

        let result = (|| -> CmdHookResult<()> {
            // 0) 预置一枚旧版残留 .ps1：install 必须把它清理掉。
            let stale_ps1 = root.join("scripts").join(LEGACY_CAPTURE_PS1_FILE_NAME);
            std::fs::create_dir_all(stale_ps1.parent().unwrap()).unwrap();
            std::fs::write(&stale_ps1, "# stale capture shell").unwrap();

            // 1) 挂载：生成 .bat + 合并 AutoRun。
            hook.install(&log_base)?;
            assert!(hook.is_installed(), "挂载后 is_installed 应为 true");
            let stored = win32::read_autorun()
                .expect("读取 AutoRun 应成功")
                .expect("AutoRun 值应已写入");
            let script = hook.script_path();
            assert!(
                stored.contains(&format!("call \"{}\"", script.to_string_lossy())),
                "AutoRun 应含指向脚本绝对路径的 call 片段: {stored}"
            );
            assert!(script.exists(), ".bat 捕获脚本应已生成");
            assert!(
                !stale_ps1.exists(),
                "install 应清理旧版捕获壳 .ps1（本次重构已废除）"
            );
            let raw = std::fs::read(&script).unwrap();
            assert!(
                raw.iter().all(u8::is_ascii),
                "ASCII 日志根路径下生成的 .bat 应整份纯 ASCII（无 BOM）"
            );
            assert!(
                raw.windows(3).all(|w| w != [0xEF, 0xBB, 0xBF]),
                "生成的 .bat 不得带 UTF-8 BOM（AutoRun 阶段会破坏 @echo off）"
            );

            // 2) 幂等：重复 install 不改变 AutoRun 值。
            hook.install(&log_base)?;
            assert_eq!(win32::read_autorun().unwrap(), Some(stored));

            // 3) 非交互子进程（cmd /c，构建工具形态）不得被拦截 / 不得挂死 /
            //    不得产生会话日志。
            let cmd_log_dir = log_base.join(CMD_LOG_SUBDIR);
            let out = Command::new("cmd.exe")
                .args(["/c", "echo TLTB_GUARD_OK"])
                .output()
                .expect("cmd /c 应正常返回");
            assert!(out.status.success(), "cmd /c 子进程应正常退出");
            assert!(String::from_utf8_lossy(&out.stdout).contains("TLTB_GUARD_OK"));
            let files = std::fs::read_dir(&cmd_log_dir)
                .map(|d| d.count())
                .unwrap_or(0);
            assert_eq!(files, 0, "非交互子进程不得产生会话日志");

            // 4) 卸载：AutoRun 还原（含用户原有内容时保留）。
            hook.uninstall()?;
            assert!(!hook.is_installed(), "卸载后 is_installed 应为 false");
            Ok(())
        })();

        // 还原现场：无论成败都把 AutoRun / 键恢复到运行前状态。
        restore_autorun_state(existed, prev);
        let _ = std::fs::remove_dir_all(&root);

        result.expect("安装卸载往返应成功");
    }

    /// 真实「交互形态」冒烟：AutoRun 挂载后经管道驱动一个「交互形态」cmd
    /// （无 /c），验证**原生 .bat 记录器**会话头落盘、命令照常执行、进程
    /// 正常收尾、全程不挂死（doskey `exit` 宏只在真实控制台输入下触发——
    /// 管道驱动的会话为会话头形态，属既有边界，见模块文档）。测试自清理。
    /// **注意**：与同模块其余 `real_` 测试一样会读写真实 AutoRun 键，必须
    /// **串行**运行（`--test-threads=1`），否则并行互相覆盖注册表值。
    #[cfg(windows)]
    #[test]
    #[ignore = "读写当前用户真实 AutoRun 键并短时挂载记录钩子，需显式运行（--ignored --test-threads=1）"]
    fn real_native_session_no_deadlock_self_cleaning() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let existed = win32::autorun_key_exists();
        let prev = win32::read_autorun().expect("读取 AutoRun 应成功");

        let root = unique_temp("real-session");
        let log_base = root.join("logs").join("terminals");
        let hook = CmdHook::at(&root);

        let result = (|| -> CmdHookResult<()> {
            hook.install(&log_base)?;
            let cmd_log_dir = log_base.join(CMD_LOG_SUBDIR);

            // 管道驱动「交互形态」cmd（无 /c）：AutoRun → .bat 守卫 → 原生会话
            // 头落盘；喂入命令与 exit（stdout 收到回显即命令确实执行了）。
            // 注意：测试进程自身可能运行在一个已被记录的会话里（继承
            // TLTB_CMD_LOGGED=1），必须从子进程环境剥离该守卫——本测试要
            // 验证的正是「全新未记录会话」首次触发 AutoRun 的行为。
            let mut child = Command::new("cmd.exe")
                .env_remove(CMD_RECURSION_GUARD_ENV)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn cmd");
            let mut stdin = child.stdin.take().expect("stdin 句柄");
            let mut stdout = child.stdout.take().expect("stdout 句柄");
            let writer = std::thread::spawn(move || {
                let _ = stdin.write_all(b"echo TLTB_SESSION_MARKER\r\nexit\r\n");
            });
            let mut echoed = String::new();
            use std::io::Read;
            let reader = std::thread::spawn(move || {
                let _ = stdout.read_to_string(&mut echoed);
                echoed
            });

            // 等待收尾（原生 .bat 无任何冷启动依赖；卡死由超时暴露）。
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                if let Some(status) = child.try_wait().expect("try_wait") {
                    assert!(status.success(), "记录会话进程应正常退出: {status}");
                    break;
                }
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    panic!("记录会话 30s 未收尾——疑似死循环 / 假死");
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let _ = writer.join();
            let echoed = reader.join().unwrap_or_default();
            assert!(
                echoed.contains("TLTB_SESSION_MARKER"),
                "命令应照常执行并回显（原生控制台功能不被破坏）"
            );

            // 恰好生成一份会话头日志（重定向 stdin 下 doskey 宏不触发，收尾
            // 分支不落盘——记录器本身不挂起、不产生多余日志）。
            let files: Vec<PathBuf> = std::fs::read_dir(&cmd_log_dir)
                .expect("日志目录应被创建")
                .map(|e| e.unwrap().path())
                .collect();
            assert_eq!(files.len(), 1, "应恰好生成一份会话日志: {files:?}");
            let body = std::fs::read_to_string(&files[0]).expect("日志可读");
            assert!(
                body.contains("TLToolBox cmd session start"),
                "日志应含会话头: {body}"
            );
            assert!(
                !body.contains("commands typed"),
                "重定向 stdin 的会话不得含键入命令清单"
            );

            hook.uninstall()?;
            Ok(())
        })();

        restore_autorun_state(existed, prev);
        let _ = std::fs::remove_dir_all(&root);
        result.expect("原生会话头冒烟应成功");
    }

    /// 还原 AutoRun 现场（仅 #[ignore] 实机测试使用）：值按快照回写 / 删除；
    /// 安装前键不存在时把重建的空键一并移除，完整还原初始状态。
    #[cfg(windows)]
    fn restore_autorun_state(key_existed_before: bool, previous: Option<String>) {
        match previous {
            Some(value) => {
                let _ = win32::write_autorun(&value);
            }
            None => {
                let _ = win32::delete_autorun();
            }
        }
        if !key_existed_before {
            let _ = win32::delete_autorun_key();
        }
    }
}
