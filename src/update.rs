//! # 检查更新：GitHub Releases 版本探测（WinHttp 原生 · 零额外网络库依赖）
//!
//! v0.3.2 新增。TLToolBox 自架构转型后已无任何 HTTP 依赖（reqwest 随 agent 层
//! 退役）；「检查更新」同样**不引入任何网络库**，改走 Win32 原生 WinHttp API
//! （`WinHttpOpen` → `WinHttpConnect` → `WinHttpOpenRequest` → `WinHttpSendRequest`
//! → `WinHttpReceiveResponse` → `WinHttpQueryDataAvailable` / `WinHttpReadData` →
//! `WinHttpCloseHandle`）请求 GitHub Releases 最新发行版接口：
//!
//! ```text
//! GET https://api.github.com/repos/ltl0312/TLToolBox/releases/latest
//! ```
//!
//! 响应 JSON 只需 `tag_name` 一个字段（如 `v0.3.2`），故**不引入 JSON 解析库**：
//! [`extract_tag_name`] 以字符串定位提取。随后 [`parse_version`] /
//! [`is_newer_version`] 做语义化版本（`major.minor.patch`）数值比较，与当前
//! `CARGO_PKG_VERSION` 对比得出「已是最新」/「发现新版本」结论。
//!
//! # 边界与降级
//!
//! - 无网络 / 被限流（GitHub API 匿名 60 次/小时）时接口返回非 200 或不可解析
//!   的 JSON：一律报告 `Ok(None)`（“无法获取版本信息”），绝不误报“已是
//!   最新版本”；
//! - 全部 WinHttp 句柄以 RAII 守卫管理，任何失败路径都保证 `WinHttpCloseHandle`；
//! - 超时经 [`WinHttpSetTimeouts`] 统一收紧（连接 / 发送 / 接收各 8 秒），
//!   避免极端网络环境下 UI 长期无响应（调用方还以 `spawn_blocking` 移出运行时）；
//! - 非 Windows 目标上 [`fetch_latest_tag`] 返回 [`UpdateError::UnsupportedPlatform`]，
//!   纯函数层（版本提取 / 比较）跨平台可单测。

use std::error::Error;
use std::fmt;

/// 项目开源地址（「关于」对话框展示 + 点击经 [`crate::platform::open_url`] 打开）。
pub const REPO_URL: &str = "https://github.com/ltl0312/TLToolBox.git";

/// GitHub Releases **最新发行版**接口（检查更新的数据源）。
pub const RELEASES_API_URL: &str = "https://api.github.com/repos/ltl0312/TLToolBox/releases/latest";

/// GitHub Releases 页面（发现新版本时「前往下载」的落地页）。
pub const RELEASES_PAGE_URL: &str = "https://github.com/ltl0312/TLToolBox/releases/latest";

/// 检查更新错误模型。
#[derive(Debug)]
pub enum UpdateError {
    /// 非 Windows 平台：WinHttp 为 Windows 原生能力。
    UnsupportedPlatform,
    /// WinHttp API 调用失败（`WinHttpOpen` / `Connect` / `OpenRequest` /
    /// `SendRequest` / `ReceiveResponse` 等），携带人类可读原因。
    Http {
        /// 失败原因（含底层错误码文本）。
        message: String,
    },
    /// 请求成功但响应异常（非 200 / 非法 HTTP 状态等）。
    BadResponse {
        /// HTTP 状态码（响应可读到时）。
        status: u32,
    },
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                write!(f, "检查更新仅支持 Windows（WinHttp 原生 API）")
            }
            Self::Http { message } => write!(f, "WinHttp 请求失败: {message}"),
            Self::BadResponse { status } => {
                write!(f, "GitHub Releases 接口返回异常状态码 {status}")
            }
        }
    }
}

impl Error for UpdateError {}

// ---------------------------------------------------------------------------
// 纯函数层（跨平台可单测：JSON 定位 + 语义化版本比较的确定性核心）
// ---------------------------------------------------------------------------

/// 从 GitHub Releases `latest` 接口的 JSON 响应中提取 `tag_name` 字段值。
///
/// 定位语义：查找 `"tag_name"` 键 → 跳过空白与冒号 → 读取随后的引号字符串
/// （含转义处理，按 JSON 字符串字面量解析，如 `\"` / `\\`）。找不到键 /
/// 值非字符串（如 `null`）→ `None`。
///
/// 说明：GitHub API 的字段顺序由服务器决定（不可假设 `tag_name` 恒在前面），
/// 故本实现做**全串定位**而非“截取头部”。
pub fn extract_tag_name(json: &str) -> Option<String> {
    let key = "\"tag_name\"";
    let start = json.find(key)? + key.len();
    let rest = &json[start..];
    // 键后允许空白 + 冒号 + 空白。
    let colon_rel = rest.find(':')?;
    let after_colon = rest[colon_rel + 1..].trim_start();
    if !after_colon.starts_with('"') {
        return None; // 值不是字符串（如 null）→ 无版本可提取
    }
    let mut out = String::new();
    let mut chars = after_colon[1..].chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some(out), // 未转义收尾引号
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                other => {
                    out.push('\\');
                    out.push(other);
                }
            },
            other => out.push(other),
        }
    }
    None // 字符串未闭合（畸形 JSON）
}

/// 把带可选 `v` 前缀的版本字符串解析为 `(major, minor, patch)` 三元组。
///
/// 容错语义：`v1.2` → `(1, 2, 0)`（缺省段按 0 补全）；非法段（非数字）返回
/// `None`。仅用于版本比较，不处理预发布 / 构建元数据后缀。
pub fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let trimmed = version.trim().trim_start_matches('v');
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = match parts.next() {
        Some(s) => s.parse::<u64>().ok()?,
        None => 0,
    };
    let patch = match parts.next() {
        Some(s) => s.parse::<u64>().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None; // 多于三段（如 1.2.3.4）→ 非本项目使用的版本形态
    }
    Some((major, minor, patch))
}

/// 语义化版本比较：`candidate` 是否严格**新于** `current`。
///
/// 任一版本不可解析 → `false`（保守：宁可“不是更新”也不给“有新版本”的
/// 误报——检查更新的乐观误报会诱导用户做无谓升级）。
pub fn is_newer_version(current: &str, candidate: &str) -> bool {
    match (parse_version(current), parse_version(candidate)) {
        (Some(cur), Some(cand)) => cand > cur,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// WinHttp 原生请求（仅 Windows）
// ---------------------------------------------------------------------------

/// 请求 GitHub Releases `latest` 接口并提取最新发行版的 `tag_name`。
///
/// - `Ok(Some(tag))`：请求成功且 JSON 含可解析的 `tag_name`（如 `v1.2.3`）；
/// - `Ok(None)`：请求成功但无法解析出版本（非 200 / 被限流 / JSON 变形）——
///   调用方应提示“无法获取版本信息”而非“已是最新”；
/// - `Err(..)`：WinHttp 管线本身的硬失败。
#[cfg(windows)]
pub fn fetch_latest_tag() -> Result<Option<String>, UpdateError> {
    imp::fetch_latest_tag_impl()
}

/// 非 Windows 兜底（见模块文档）。
#[cfg(not(windows))]
pub fn fetch_latest_tag() -> Result<Option<String>, UpdateError> {
    Err(UpdateError::UnsupportedPlatform)
}

/// WinHttp 实现细节（Windows 专用）。
#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;
    use windows::core::PCWSTR;
    use windows::Win32::Networking::WinHttp::{
        WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryDataAvailable,
        WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
        WinHttpSetTimeouts, WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, WINHTTP_FLAG_REFRESH,
        WINHTTP_FLAG_SECURE, WINHTTP_QUERY_STATUS_CODE,
    };

    /// WinHttp 句柄 RAII 守卫：任何错误路径都保证 `WinHttpCloseHandle`。
    struct HttpHandle(*mut c_void);

    impl Drop for HttpHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: 句柄来自 WinHttpOpen / WinHttpConnect / WinHttpOpenRequest
                // 的成功返回；对已关闭句柄重复调用仅返回错误，无未定义行为。
                unsafe {
                    let _ = WinHttpCloseHandle(self.0);
                }
            }
        }
    }

    fn to_wide_units(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 从请求句柄读取 HTTP 状态码（读不到时返回 `None`）。
    fn query_status_code(request: *mut c_void) -> Option<u32> {
        let mut status = 0u32;
        let mut length = std::mem::size_of::<u32>() as u32;
        // SAFETY: status 指向栈上 u32，长度精确匹配；信息类取标准 STATUS_CODE。
        let ok = unsafe {
            WinHttpQueryHeaders(
                request,
                WINHTTP_QUERY_STATUS_CODE,
                PCWSTR::null(),
                Some((&mut status as *mut u32).cast()),
                &mut length,
                std::ptr::null_mut(),
            )
        };
        ok.is_ok().then_some(status)
    }

    /// 同步读取整个响应体（循环 `QueryDataAvailable` → `ReadData`）。
    fn read_body(request: *mut c_void) -> Vec<u8> {
        let mut body = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            let mut available = 0u32;
            // SAFETY: available 指向栈上 u32。
            if unsafe { WinHttpQueryDataAvailable(request, &mut available) }.is_err() {
                break;
            }
            if available == 0 {
                break;
            }
            let to_read = (available as usize).min(buffer.len());
            let mut read = 0u32;
            // SAFETY: buffer 长度 ≥ to_read，写入不越界；read 指向栈上 u32。
            if unsafe {
                WinHttpReadData(request, buffer.as_mut_ptr().cast(), to_read as u32, &mut read)
            }
            .is_err()
            {
                break;
            }
            if read == 0 {
                break;
            }
            body.extend_from_slice(&buffer[..read as usize]);
        }
        body
    }

    pub(super) fn fetch_latest_tag_impl() -> Result<Option<String>, UpdateError> {
        // 1) 打开会话句柄（User-Agent 含应用名与版本；默认代理设置）。
        let agent = format!("TLToolBox/{}", env!("CARGO_PKG_VERSION"));
        let agent_wide = to_wide_units(&agent);
        // SAFETY: agent_wide 在本调用期间存活；默认代理 + 无代理绕行名单。
        let session = HttpHandle(unsafe {
            WinHttpOpen(
                PCWSTR(agent_wide.as_ptr()),
                WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            )
        });
        if session.0.is_null() {
            return Err(UpdateError::Http {
                message: "WinHttpOpen 失败".to_string(),
            });
        }

        // 2) 连接 api.github.com:443（HTTPS）。
        let host = to_wide_units("api.github.com");
        // SAFETY: host 在本调用期间存活；端口 443 = HTTPS。
        let connection = HttpHandle(unsafe {
            WinHttpConnect(session.0, PCWSTR(host.as_ptr()), 443, 0)
        });
        if connection.0.is_null() {
            return Err(UpdateError::Http {
                message: "WinHttpConnect(api.github.com:443) 失败".to_string(),
            });
        }

        // 3) 打开 GET 请求（Accept: */* 数组以 NULL 结尾；TLS + 强制刷新缓存）。
        let path = to_wide_units("/repos/ltl0312/TLToolBox/releases/latest");
        let verb = to_wide_units("GET");
        let accept_type = to_wide_units("*/*");
        // SAFETY: accept_types 数组元素（PCWSTR）在本调用期间存活且以 NULL 结尾。
        let accept_types = [PCWSTR(accept_type.as_ptr()), PCWSTR::null()];
        let request = HttpHandle(unsafe {
            WinHttpOpenRequest(
                connection.0,
                PCWSTR(verb.as_ptr()),
                PCWSTR(path.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                accept_types.as_ptr(),
                WINHTTP_FLAG_SECURE | WINHTTP_FLAG_REFRESH,
            )
        });
        if request.0.is_null() {
            return Err(UpdateError::Http {
                message: "WinHttpOpenRequest 失败".to_string(),
            });
        }

        // 4) 统一收紧超时（解析/连接/发送/接收各 8 秒，防极端网络挂起 UI 侧任务）。
        // SAFETY: 请求句柄在本调用期间存活。
        let _ = unsafe {
            WinHttpSetTimeouts(request.0, 8000, 8000, 8000, 8000)
        };

        // 5) 携带 UA 头发送（GitHub 强制要求 User-Agent，缺失返回 403）。
        let headers = to_wide_units("User-Agent: TLToolBox\r\nAccept: application/vnd.github+json\r\n");
        // SAFETY: headers 为 NUL 结尾 UTF-16 缓冲，本调用期间存活。
        unsafe {
            WinHttpSendRequest(request.0, Some(&headers), None, 0, 0, 0)
                .map_err(|err| UpdateError::Http {
                    message: format!("WinHttpSendRequest 失败: {err}"),
                })?;
            WinHttpReceiveResponse(request.0, std::ptr::null_mut())
                .map_err(|err| UpdateError::Http {
                    message: format!("WinHttpReceiveResponse 失败: {err}"),
                })?;
        }

        // 6) 读取响应体（先取状态码再读正文；失败路径一律 Ok(None)——不误报）。
        let status = query_status_code(request.0);
        let body = read_body(request.0);
        let text = String::from_utf8_lossy(&body);
        match status {
            Some(200) => Ok(extract_tag_name(&text)),
            Some(code) => {
                tracing::warn!(
                    target: "update",
                    "GitHub Releases 接口返回状态码 {code}（可能被限流；响应: {}）",
                    text.chars().take(160).collect::<String>()
                );
                Ok(None)
            }
            None => {
                tracing::warn!(target: "update", "无法读取 GitHub Releases 响应状态码");
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- tag_name 提取（JSON 定位核心） ----

    #[test]
    fn extracts_tag_name_from_typical_latest_release_json() {
        let json = r#"{
  "url": "https://api.github.com/repos/ltl0312/TLToolBox/releases/3",
  "tag_name": "v0.3.2",
  "name": "v0.3.2",
  "draft": false,
  "prerelease": false
}"#;
        assert_eq!(extract_tag_name(json).as_deref(), Some("v0.3.2"));
    }

    #[test]
    fn extracts_tag_name_when_key_appears_after_other_fields() {
        // 字段顺序不可假设：tag_name 位于靠后位置也必须命中。
        let json = r#"{"draft":false,"prerelease":false,"published_at":"2026-09-04T00:00:00Z","tag_name":"v1.2.3"}"#;
        assert_eq!(extract_tag_name(json).as_deref(), Some("v1.2.3"));
    }

    #[test]
    fn handles_escaped_quotes_inside_tag_value() {
        let json = r#"{"tag_name":"v9\"9\"9"}"#;
        assert_eq!(extract_tag_name(json).as_deref(), Some("v9\"9\"9"));
    }

    #[test]
    fn missing_or_null_tag_returns_none() {
        assert_eq!(extract_tag_name(r#"{"name":"a"}"#), None);
        assert_eq!(extract_tag_name(r#"{"tag_name":null}"#), None);
        assert_eq!(extract_tag_name("not json at all"), None);
        assert_eq!(extract_tag_name(r#"{"tag_name":"unterminated"#), None);
    }

    // ---- 语义化版本解析与比较 ----

    #[test]
    fn parse_version_accepts_v_prefix_and_partial_parts() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("v1.2"), Some((1, 2, 0))); // 缺省段按 0 补全
        assert_eq!(parse_version("0.3.2"), Some((0, 3, 2)));
    }

    #[test]
    fn parse_version_rejects_garbage() {
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("v"), None);
        assert_eq!(parse_version("1.2.3.4"), None); // 超三段
        assert_eq!(parse_version("a.b.c"), None);
        assert_eq!(parse_version("1.x.3"), None);
    }

    #[test]
    fn newer_version_comparison_is_strict_and_conservative() {
        assert!(is_newer_version("0.3.1", "0.3.2"));
        assert!(is_newer_version("v0.3.2", "v1.0.0"));
        assert!(is_newer_version("1.2.3", "1.2.10"), "patch 按数值比较而非字典序");
        // 相同版本 → 不视为更新。
        assert!(!is_newer_version("0.3.2", "0.3.2"));
        // 候选更旧 → 不视为更新。
        assert!(!is_newer_version("0.3.2", "0.3.1"));
        // 任一版本不可解析 → 保守返回 false（绝不误报“有新版本”）。
        assert!(!is_newer_version("garbage", "0.3.2"));
        assert!(!is_newer_version("0.3.2", "latest"));
    }

    #[test]
    fn update_error_display_mentions_platform() {
        let text = format!("{}", UpdateError::UnsupportedPlatform);
        assert!(text.contains("Windows"), "文案应点名平台: {text}");
    }
}