//! # 开机自启动：Windows 注册表实现（架构转型 · 常驻控制）
//!
//! TLToolBox 裁撤 LLM / Agent 层后，开机自启不再依赖任何第三方服务或网络，
//! 而是直接读写 **HKCU 当前用户** 的 Run 键——Windows 原生、免提权、随用户
//! 配置随系统启动的唯一个人级入口：
//!
//! ```text
//! HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run
//!     值名：TLToolBox
//!     值  ：<当前可执行文件绝对路径> --silent
//! ```
//!
//! # 设计约定
//!
//! - **配置为准（config-as-source-of-truth）**：`auto_start_windows`（`AppConfig`）
//!   是用户意图的持久化事实；本模块只把该意图镜像到注册表。装配层在**配置加载后**
//!   调用 [`synchronize_autostart`]，使注册表与配置收敛（缺失/路径漂移→补写，
//!   多余→删除），见 `crate::main` 的装配点；
//! - **路径转义**：可执行文件路径可能含空格（如 `C:\Program Files\...`），写入
//!   Run 键的命令行必须整体加双引号，否则 Explorer 会把路径拦腰截断成两个参数。
//!   路径中不可能出现 `"` 字符（Windows 文件系统保留），因此无需考虑引号内转义；
//! - **身份校验**：[`is_autostart_enabled`] 不只检查键是否存在，还校验值是否精确
//!   指向**当前**可执行文件（大小写不敏感比较）。程序被移动/升级后旧值视为未启用，
//!   由 `auto_start_windows = true` 触发重写，避免指向已不存在的旧路径；
//! - **优雅失败**：所有注册表操作失败均返回结构化错误（携带操作名与 Win32 错误码），
//!   绝不 panic；装配层对同步失败仅告警降级，不阻断启动。

use std::error::Error;
use std::fmt;
use std::path::Path;

/// Run 键相对 `HKEY_CURRENT_USER` 的子键路径。
pub const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Run 键下的值名（应用专属，避免与其他自启项冲突）。
pub const RUN_VALUE_NAME: &str = "TLToolBox";

/// 静默启动参数：随自启命令追加，供启动装配层识别“本次由系统拉起”并抑制
/// 前台窗口打扰（tray / 最小化行为在常驻层消费本参数）。
pub const SILENT_ARG: &str = "--silent";

/// 开机自启操作的统一错误别名（与 `set_autostart` 的签名约定一致）。
pub type AutostartResult = Result<(), Box<dyn Error + Send + Sync>>;

/// 自启模块错误模型。
#[derive(Debug)]
pub enum AutostartError {
    /// 当前平台不支持注册表自启（非 Windows 的编译期兜底分支）。
    UnsupportedPlatform,
    /// 无法定位当前可执行文件（`std::env::current_exe` 失败）。
    CurrentExeUnavailable {
        /// 底层 IO 错误。
        source: std::io::Error,
    },
    /// Win32 注册表调用失败。
    Registry {
        /// 失败的操作（`RegOpenKeyExW` / `RegSetValueExW` / `RegDeleteValueW`…）。
        operation: &'static str,
        /// Win32 错误码（0 = ERROR_SUCCESS，此处恒非 0）。
        code: u32,
    },
}

impl fmt::Display for AutostartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => write!(f, "开机自启仅支持 Windows（注册表 Run 键）"),
            Self::CurrentExeUnavailable { source } => {
                write!(f, "无法定位当前可执行文件路径: {source}")
            }
            Self::Registry { operation, code } => {
                write!(f, "注册表操作失败: {operation}（Win32 错误码 {code}）")
            }
        }
    }
}

impl Error for AutostartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CurrentExeUnavailable { source } => Some(source),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// 纯函数层：命令行的构造与比较（跨平台可单测，Windows 注册表代码的确定性核心）
// ---------------------------------------------------------------------------

/// 可执行文件路径 → Run 键命令行片段。
///
/// Windows 命令行规则：含空白（空格 / Tab）的参数必须整体加双引号；
/// 不含空白则原样输出。路径字符串不可能含 `"`（NTFS 保留字符），无需处理引号
/// 嵌套转义。
fn quote_exe_path(exe: &str) -> String {
    let trimmed = exe.trim();
    if trimmed.is_empty() || trimmed.contains([' ', '\t']) {
        format!("\"{trimmed}\"")
    } else {
        trimmed.to_string()
    }
}

/// 由可执行文件绝对路径构造完整的 Run 键值（引用后的路径 + `--silent`）。
///
/// 输出形如：`"C:\Program Files\TLToolBox\tltoolbox.exe" --silent`。
fn build_run_value(exe_path: &Path) -> String {
    format!(
        "{} {SILENT_ARG}",
        quote_exe_path(&exe_path.to_string_lossy())
    )
}

/// 由当前进程可执行文件构造期望的 Run 键值。
fn expected_run_value() -> Result<String, AutostartError> {
    let exe_path = std::env::current_exe()
        .map_err(|source| AutostartError::CurrentExeUnavailable { source })?;
    Ok(build_run_value(&exe_path))
}

/// 判断注册表现存值是否精确指向给定可执行文件。
///
/// - 忽略首尾空白与 ASCII 大小写（Windows 路径大小写不敏感）；
/// - 值指向旧路径 / 旧版本命令（缺 `--silent`）一律视为“不是我们”，促使
///   `auto_start_windows = true` 时重写。
fn value_matches_exe(stored: &str, exe_path: &Path) -> bool {
    stored
        .trim()
        .eq_ignore_ascii_case(&build_run_value(exe_path))
}

// ---------------------------------------------------------------------------
// Windows 注册表实现
// ---------------------------------------------------------------------------

/// 把当前 `auto_start_windows` 配置镜像到注册表（以配置为准的幂等同步）。
///
/// - 配置开启而注册表缺失 / 指向旧路径 → 写入当前可执行文件 + `--silent`；
/// - 配置关闭而注册表存在我们的值 → 删除；
/// - 已一致 → 空操作。
///
/// 非 Windows 平台恒返回 [`AutostartError::UnsupportedPlatform`]（开启时）。
pub fn synchronize_autostart(enable: bool) -> AutostartResult {
    if is_autostart_enabled() != enable {
        set_autostart(enable)?;
    }
    Ok(())
}

/// 启用（`true`）/ 停用（`false`）开机自启。
///
/// 启用时写入 `HKCU\...\CurrentVersion\Run` 的 `TLToolBox` 值，值为
/// 当前可执行文件绝对路径（含空格时加双引号）+ ` --silent`；
/// 停用时删除该值（值不存在视为成功，幂等）。
pub fn set_autostart(enable: bool) -> AutostartResult {
    #[cfg(windows)]
    {
        win32::set_autostart_impl(enable)
            .map_err(|err| -> Box<dyn Error + Send + Sync> { Box::new(err) })
    }
    #[cfg(not(windows))]
    {
        let _ = enable;
        Err(Box::new(AutostartError::UnsupportedPlatform))
    }
}

/// 查询开机自启当前是否**确实生效**：注册表存在 `TLToolBox` 值且精确指向
/// 当前可执行文件（含 `--silent`）。
///
/// 注册表不可读 / 值缺失 / 值指向其他程序或旧路径时一律返回 `false`，
/// 永不返回 `Err`（只读探测的宽松语义，供启动装配层做收敛判断）。
pub fn is_autostart_enabled() -> bool {
    #[cfg(windows)]
    {
        let Some(exe_path) = std::env::current_exe().ok() else {
            return false;
        };
        match win32::read_run_value() {
            Ok(Some(stored)) => value_matches_exe(&stored, &exe_path),
            _ => false,
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Windows 原生实现：注册表读写（advapi32）。
#[cfg(windows)]
mod win32 {
    use super::*;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, REG_VALUE_TYPE,
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
            // SAFETY: 句柄来自 RegOpenKeyExW 的成功返回；关闭无效句柄仅返回
            // 错误码，无未定义行为。
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }

    /// 打开 Run 键（只读 + 可写），返回句柄守卫。
    fn open_run_key() -> Result<KeyGuard, AutostartError> {
        let key_path = to_wide_units(RUN_KEY_PATH);
        let mut key = HKEY::default();
        // SAFETY: key_path 为 NUL 结尾的 UTF-16 缓冲，其指针在本调用期间存活；
        // samdesired 仅含查询/写入权限位，不涉及任何已被废弃的 KEY_ALL_ACCESS 面。
        let code = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(key_path.as_ptr()),
                0,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                &mut key,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(AutostartError::Registry {
                operation: "RegOpenKeyExW",
                code: code.0,
            });
        }
        Ok(KeyGuard(key))
    }

    /// 读取 `TLToolBox` 值：`Ok(None)` = 值不存在，`Ok(Some(s))` = 现存的 REG_SZ 文本。
    pub(super) fn read_run_value() -> Result<Option<String>, AutostartError> {
        let key = open_run_key()?;
        let value_name = to_wide_units(RUN_VALUE_NAME);

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
            return Err(AutostartError::Registry {
                operation: "RegQueryValueExW(探测)",
                code: code.0,
            });
        }

        // v0.6.2（L16）：**类型校验**——本键只应承载 REG_SZ 文本。若被外部改成
        // REG_DWORD / REG_BINARY 等，按 UTF-16 解析会得到无意义文本（甚至影响
        // 自启命令行的判等），此处显式拒绝并按"值不存在"处理（随后会被正确
        // 重写为 REG_SZ）。
        if value_type != REG_SZ {
            tracing::warn!(
                target: "autostart",
                "自启注册表值类型异常（{:?} 而非 REG_SZ），将按无值处理并重写",
                value_type.0
            );
            return Ok(None);
        }
        // v0.6.2（L16）：**长度上限**——防御异常/被篡改的超大值。自启命令行
        // 合理长度远小于 4 KiB；超出即按无值处理（随后重写）。
        const MAX_RUN_VALUE_BYTES: u32 = 4096;
        if size > MAX_RUN_VALUE_BYTES {
            tracing::warn!(
                target: "autostart",
                "自启注册表值过大（{size} 字节 > {MAX_RUN_VALUE_BYTES}），将按无值处理并重写"
            );
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
            return Err(AutostartError::Registry {
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

    /// 写入 `TLToolBox` 值（REG_SZ，含 `--silent` 的完整命令行）。
    fn write_run_value(value: &str) -> Result<(), AutostartError> {
        let key = open_run_key()?;
        let value_name = to_wide_units(RUN_VALUE_NAME);
        let data = units_to_bytes(&to_wide_units(value));
        // SAFETY: value_name / data 均为本函数持有的 NUL 结尾缓冲，调用期间存活；
        // REG_SZ 要求数据含结尾 NUL，data 已满足。
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
            return Err(AutostartError::Registry {
                operation: "RegSetValueExW",
                code: code.0,
            });
        }
        Ok(())
    }

    /// 删除 `TLToolBox` 值（值不存在视为成功——幂等停用）。
    fn delete_run_value() -> Result<(), AutostartError> {
        let key = open_run_key()?;
        let value_name = to_wide_units(RUN_VALUE_NAME);
        // SAFETY: value_name 为 NUL 结尾缓冲，调用期间存活。
        let code = unsafe { RegDeleteValueW(key.0, PCWSTR(value_name.as_ptr())) };
        if code != ERROR_SUCCESS && code != ERROR_FILE_NOT_FOUND {
            return Err(AutostartError::Registry {
                operation: "RegDeleteValueW",
                code: code.0,
            });
        }
        Ok(())
    }

    /// Windows 平台实现入口（见 [`super::set_autostart`] 语义）。
    pub(super) fn set_autostart_impl(enable: bool) -> Result<(), AutostartError> {
        if enable {
            write_run_value(&expected_run_value()?)
        } else {
            delete_run_value()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn exe(path: &str) -> PathBuf {
        PathBuf::from(path)
    }

    // ---- 命令行构造 ----

    #[test]
    fn path_without_spaces_is_not_quoted() {
        assert_eq!(
            quote_exe_path(r"C:\Tools\tltoolbox.exe"),
            r"C:\Tools\tltoolbox.exe"
        );
        assert_eq!(
            quote_exe_path(r"C:\ProgramFiles\tltoolbox.exe"),
            r"C:\ProgramFiles\tltoolbox.exe"
        );
    }

    #[test]
    fn path_with_spaces_is_fully_quoted() {
        assert_eq!(
            quote_exe_path(r"C:\Program Files\TLToolBox\tltoolbox.exe"),
            r#""C:\Program Files\TLToolBox\tltoolbox.exe""#
        );
        // Tab 同属命令行分隔空白，同样需要引号保护。
        assert_eq!(quote_exe_path("C:\\a\tb\\x.exe"), "\"C:\\a\tb\\x.exe\"");
    }

    #[test]
    fn empty_or_blank_path_is_quoted_to_stay_single_argument() {
        assert_eq!(quote_exe_path("   "), r#""""#);
    }

    #[test]
    fn run_value_appends_silent_arg_after_quoted_path() {
        assert_eq!(
            build_run_value(&exe(r"C:\Program Files\TLToolBox\tltoolbox.exe")),
            r#""C:\Program Files\TLToolBox\tltoolbox.exe" --silent"#
        );
        assert_eq!(
            build_run_value(&exe(r"C:\Tools\tltoolbox.exe")),
            r"C:\Tools\tltoolbox.exe --silent"
        );
    }

    // ---- 值比较 ----

    #[test]
    fn stored_value_matching_exe_is_enabled() {
        let exe_path = exe(r"C:\Program Files\TLToolBox\tltoolbox.exe");
        let stored = build_run_value(&exe_path);
        assert!(value_matches_exe(&stored, &exe_path));
    }

    #[test]
    fn comparison_is_case_insensitive_and_trims_whitespace() {
        let exe_path = exe(r"C:\Program Files\TLToolBox\tltoolbox.exe");
        let stored = r#"  "C:\PROGRAM FILES\TLTOOLBOX\TLTOOLBOX.EXE" --SILENT  "#;
        assert!(
            value_matches_exe(stored, &exe_path),
            "大小写与首尾空白应被容忍"
        );
    }

    #[test]
    fn stale_or_alien_value_counts_as_disabled() {
        let exe_path = exe(r"C:\Program Files\TLToolBox\tltoolbox.exe");

        // 指向旧路径：值在，但不是我们 → 视为未启用，触发重写。
        assert!(!value_matches_exe(
            r#""D:\Old\TLToolBox\tltoolbox.exe" --silent"#,
            &exe_path
        ));

        // 其他程序占用同名值：同样视为未启用。
        assert!(!value_matches_exe(r#"C:\Windows\notepad.exe"#, &exe_path));

        // 旧版本命令缺 --silent：不匹配，促使按新契约重写。
        assert!(!value_matches_exe(
            r#""C:\Program Files\TLToolBox\tltoolbox.exe""#,
            &exe_path
        ));
    }

    // ---- 平台差异兜底 ----

    #[cfg(not(windows))]
    #[test]
    fn non_windows_platform_is_never_enabled() {
        assert!(!is_autostart_enabled());
        let err = set_autostart(true).expect_err("非 Windows 平台应拒绝写入");
        assert!(err.to_string().contains("仅支持 Windows"));
    }

    // ---- 实机注册表往返（默认忽略：会读写当前用户 Run 键，仅手工/CI 显式运行） ----

    /// 真实的写→读→删往返，验证与 Windows 注册表 API 的端到端契约。
    /// 测试自清理：无论断言成败都会在结尾删除 `TLToolBox` 值，不残留自启项。
    #[cfg(windows)]
    #[test]
    #[ignore = "读写当前用户真实注册表 Run 键，需显式运行（--ignored）"]
    fn real_registry_roundtrip_self_cleaning() {
        let was_enabled = is_autostart_enabled();

        set_autostart(true).expect("写入注册表应成功");
        assert!(is_autostart_enabled(), "写入后应报告已启用");

        set_autostart(false).expect("删除注册表值应成功");
        assert!(!is_autostart_enabled(), "删除后应报告未启用");

        // 恢复现场：若运行前本已启用（异常状态），按原状回写，避免改变用户意图。
        if was_enabled {
            set_autostart(true).expect("恢复原自启状态应成功");
        }
    }
}
