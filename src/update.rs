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
//!   纯函数层（版本提取 / 比较 / 状态码判定）跨平台可单测。
//!
//! # v0.6.1 修复（S1：检查更新 100% 失效 —— 两个叠加缺陷）
//!
//! **缺陷一 · 状态码误读**：`WinHttpQueryHeaders` 在**未带**
//! `WINHTTP_QUERY_FLAG_NUMBER` 时按 WinHttp 约定以 **ASCII 字符串**形式返回头值；
//! 旧实现把返回值直接当 `u32` 接收，`"200"` 的 4 字节 `32 30 30 00` 被小端解释为
//! `0x00303032 = 3,158,066`，于是 `match status { Some(200) => .. }` 永不命中。
//! 现已在 [`imp::query_status_code`] 补上标志位。
//!
//! **缺陷二 · 请求根本发不出去**（在整改实测中由新增的联网回归测试发现）：
//! `WinHttpSendRequest` 的 `dwHeadersLength` 语义是"头块字符数"，而 windows-rs 把
//! `Some(&[u16])` 直接映射为 `(ptr, len)`；旧实现传入的头切片**含尾部 NUL**，
//! WinHttp 判定长度区间内出现 NUL，直接返回 `E_INVALIDARG (0x80070057)`——连
//! 响应都拿不到。现由 [`imp::request_headers`] 统一剥离终止符，并以离线单测 +
//! 联网回归双重守卫。
//!
//! 两处修复的成果：把「状态码 → 结论」映射抽成纯函数 [`interpret_response`]，
//! 并把整条 HTTP 管线收敛到单一的 [`imp::run_pipeline`]，使联网回归测试可以
//! **真正覆盖管线**（而非只覆盖尾部的纯逻辑——旧结构正是 S1 长期潜伏的土壤）。

use std::error::Error;
use std::fmt;

/// 项目开源地址（「关于」对话框展示 + 点击经 [`crate::platform::open_url`] 打开）。
pub const REPO_URL: &str = "https://github.com/ltl0312/TLToolBox.git";

/// GitHub Releases **最新发行版**接口（检查更新的数据源）。
pub const RELEASES_API_URL: &str = "https://api.github.com/repos/ltl0312/TLToolBox/releases/latest";

/// GitHub Releases 页面（发现新版本时「前往下载」的落地页）。
pub const RELEASES_PAGE_URL: &str = "https://github.com/ltl0312/TLToolBox/releases/latest";

/// 响应体读取上限（1 MiB；v0.6.2 · M8）。
///
/// 本模块只需从 JSON 里定位一个 `tag_name` 字段（真实响应体约数 KB），1 MiB 已是
/// 极端宽裕的上界。旧实现以 8 KiB 分块**无上限**累积进 `Vec`——异常 / 恶意响应可
/// 致内存无界增长（调用方在 `spawn_blocking` 里跑，涨到 OOM 会连带拖垮常驻进程）。
/// 超限即中止读取并按"无版本信息"处理（`Ok(None)`），绝不误报「已是最新」。
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

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

/// 再读入 `incoming` 字节是否会突破响应体上限（纯逻辑，可离线单测）。
///
/// 用 `saturating_add` 而非 `+`：分块长度来自 API 回报值，理论上不会溢出，但上限
/// 校验本身不该成为新的 panic 点（与 `retention_cutoff` 的夹取同一纪律）。
fn exceeds_body_limit(current_len: usize, incoming: usize) -> bool {
    current_len.saturating_add(incoming) > MAX_BODY_BYTES
}

/// 纯函数：把「HTTP 状态码 + 响应体文本」映射为检查更新结论。
///
/// 该映射即 S1 缺陷的语义核心，抽成纯函数后可在**不联网**的前提下被单测覆盖
/// （旧实现把这段 `match` 埋在 WinHttp 管线尾部，全部单测都绕过了它）。
///
/// - `Some(200)`：请求成功 → 从响应体提取 `tag_name`（提不到即 `None`，
///   调用方据此提示“无法获取版本信息”而非误报“已是最新”）；
/// - 其他状态码（403 限流 / 404 / 5xx 等）：告警留痕 → `None`；
/// - `None`（状态码读取失败）：告警留痕 → `None`。
pub fn interpret_response(status: Option<u32>, body_text: &str) -> Option<String> {
    match status {
        Some(200) => extract_tag_name(body_text),
        Some(code) => {
            tracing::warn!(
                target: "update",
                "GitHub Releases 接口返回状态码 {code}（可能被限流；响应: {}）",
                body_text.chars().take(160).collect::<String>()
            );
            None
        }
        None => {
            tracing::warn!(target: "update", "无法读取 GitHub Releases 响应状态码");
            None
        }
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
        WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
        WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
        WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts,
        WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, WINHTTP_FLAG_REFRESH, WINHTTP_FLAG_SECURE,
        WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
        WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
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

    /// `WinHttpQueryHeaders` 读取 HTTP 状态码时使用的信息类标志组合。
    ///
    /// 拆成常量是为了让 [`tests::status_code_query_flags_must_include_number_flag`]
    /// 能对"标志位被误删"这一 S1 回归点做**离线**守卫（FFI 调用本身无法单测）。
    pub(super) const STATUS_CODE_QUERY_FLAGS: u32 =
        WINHTTP_QUERY_FLAG_NUMBER | WINHTTP_QUERY_STATUS_CODE;

    /// 请求附加头块的 UTF-16 载荷（**不含**尾部 NUL 终止符）。
    ///
    /// # 为什么必须去掉尾部 NUL（v0.6.1 · S1 整改实测发现）
    /// `WinHttpSendRequest` 的 `dwHeadersLength` 语义是"头块字符数"，而
    /// windows-rs 的包装把 `Some(&[u16])` 直接映射为 `(ptr, len)`。若切片仍带
    /// 尾部 NUL，WinHttp 会因"长度区间内出现 NUL"直接返回
    /// `E_INVALIDARG (0x80070057)`——请求**根本发不出去**，表现为"检查更新永远
    /// 失败"。这条与 S1 的状态码误读是**两个叠加的独立缺陷**：只修状态码标志位
    /// 仍然拿不到任何结果。该形态由
    /// [`tests::request_header_block_has_no_trailing_nul`] 离线守卫，
    /// 并由 `live_*` 系列联网回归实测。
    pub(super) fn request_headers() -> Vec<u16> {
        let mut headers =
            to_wide_units("User-Agent: TLToolBox\r\nAccept: application/vnd.github+json\r\n");
        headers.pop(); // 去掉 NUL：dwHeadersLength 不包含终止符
        headers
    }

    /// 请求级重定向策略取值（v0.6.2 · M8）。
    ///
    /// 取 `NEVER`：本模块只向 `api.github.com` 取一个 JSON 字段，**完全不需要**跟随
    /// 重定向；默认策略（`DISALLOW_HTTPS_TO_HTTP`）仍允许跳到别的 HTTPS 主机，会把
    /// 数据源引离既定主机。拆成常量以便离线守卫"策略被改回可跟随"这一回归。
    pub(super) const REDIRECT_POLICY_VALUE: u32 = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;

    /// 把请求级重定向策略收紧为「从不跟随」（M8）。
    ///
    /// 失败仅告警不阻断：该选项在某些代理 / 沙箱环境下可能不被接受，而它的作用是
    /// **收紧**信任边界（默认值已禁止 HTTPS→HTTP 降级，残留风险限于"跳到另一台
    /// HTTPS 主机"），为本模块只读一个 JSON 字段的用途做取舍——不因加固动作本身
    /// 让"检查更新"整体不可用。
    fn set_redirect_policy_never(request: *mut c_void) {
        let value = REDIRECT_POLICY_VALUE.to_le_bytes();
        // SAFETY: value 为 4 字节 DWORD 的本地副本，在本调用期间存活；选项
        // REDIRECT_POLICY 要求 lpbuffer 指向一个 DWORD。
        match unsafe {
            WinHttpSetOption(Some(request), WINHTTP_OPTION_REDIRECT_POLICY, Some(&value))
        } {
            Ok(()) => tracing::debug!(target: "update", "已收紧重定向策略为 NEVER"),
            Err(err) => tracing::warn!(
                target: "update",
                "设置重定向策略失败（将继续使用默认策略，仅允许 HTTPS 同协议跳转）: {err}"
            ),
        }
    }

    /// 从请求句柄读取 HTTP 状态码（读不到时返回 `None`）。
    ///
    /// # S1 关键点（v0.6.1）
    /// 必须携带 [`WINHTTP_QUERY_FLAG_NUMBER`]：缺失该标志位时 WinHttp 按约定把
    /// 头值以 **ASCII 字符串**写回缓冲区，`"200"` 的 4 字节 `32 30 30 00` 会被
    /// 按小端读成 `0x00303032 = 3,158,066`，导致上游 `Some(200)` 永不命中、
    /// 检查更新整体失效。带上标志位后 WinHttp 直接以 `DWORD` 写回。
    fn query_status_code(request: *mut c_void) -> Option<u32> {
        let mut status = 0u32;
        let mut length = std::mem::size_of::<u32>() as u32;
        // SAFETY: status 指向栈上 u32，长度精确匹配；信息类 = NUMBER 标志 | 标准
        // STATUS_CODE，此时 WinHttp 以 DWORD 形式写回（不得再用默认的字符串形态）。
        let ok = unsafe {
            WinHttpQueryHeaders(
                request,
                STATUS_CODE_QUERY_FLAGS,
                PCWSTR::null(),
                Some((&mut status as *mut u32).cast()),
                &mut length,
                std::ptr::null_mut(),
            )
        };
        ok.is_ok().then_some(status)
    }

    /// 同步读取整个响应体（循环 `QueryDataAvailable` → `ReadData`）。
    ///
    /// 返回 `None` = 累计长度突破 [`MAX_BODY_BYTES`]（v0.6.2 · M8）：
    /// 立即停止读取，调用方按"无版本信息"处理。分块上限校验在**读取之前**完成，
    /// 因此极端响应不会先在内存里长大再被丢弃。
    fn read_body(request: *mut c_void) -> Option<Vec<u8>> {
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
            if exceeds_body_limit(body.len(), to_read) {
                tracing::warn!(
                    target: "update",
                    "响应体已超过 {} 字节上限，中止读取（按「无法获取版本信息」处理）",
                    MAX_BODY_BYTES
                );
                return None;
            }
            let mut read = 0u32;
            // SAFETY: buffer 长度 ≥ to_read，写入不越界；read 指向栈上 u32。
            if unsafe {
                WinHttpReadData(
                    request,
                    buffer.as_mut_ptr().cast(),
                    to_read as u32,
                    &mut read,
                )
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
        Some(body)
    }

    pub(super) fn fetch_latest_tag_impl() -> Result<Option<String>, UpdateError> {
        // 生产路径：状态码 + 响应体 → 检查更新结论（纯函数，见 interpret_response）。
        run_pipeline(interpret_response)
    }

    /// 仅供测试：真实发起一次请求并返回**原始 HTTP 状态码**。
    ///
    /// 这是唯一能"实锤" S1 的探针——修复前 `query_status_code` 会把 `"200"`
    /// 误读为 `3_158_066`，本探针可直接观测到该非 HTTP 语义的数值。
    #[cfg(test)]
    pub(super) fn probe_status_code_once() -> Result<Option<u32>, UpdateError> {
        run_pipeline(|status, _body| status)
    }

    /// 执行完整 WinHttp 管线并交出「状态码 + 响应体文本」。
    ///
    /// 抽成「管线 + 结果处理器」两段是为了可测性：生产路径传 [`interpret_response`]，
    /// 测试路径传「只取状态码」的处理器——两者共享**同一份**会话 / 连接 / 请求 /
    /// 发送 / 接收 / 读头 / 读体代码，从而让单元测试真正覆盖 HTTP 管线，而不是
    /// 只覆盖其尾部的纯逻辑（旧实现正是因此让 S1 长期潜伏）。
    fn run_pipeline<T>(handle: impl FnOnce(Option<u32>, &str) -> T) -> Result<T, UpdateError> {
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
        let connection =
            HttpHandle(unsafe { WinHttpConnect(session.0, PCWSTR(host.as_ptr()), 443, 0) });
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
        let _ = unsafe { WinHttpSetTimeouts(request.0, 8000, 8000, 8000, 8000) };

        // 4.5) 收紧重定向策略（v0.6.2 · M8）：不跟随任何重定向，杜绝数据源被引离
        //      api.github.com（失败仅告警，见 set_redirect_policy_never）。
        set_redirect_policy_never(request.0);

        // 5) 携带 UA 头发送（GitHub 强制要求 User-Agent，缺失返回 403）。
        //    注意：头块**不含**尾部 NUL——带了会被 WinHttp 判 E_INVALIDARG，
        //    请求发不出去（见 request_headers 的文档）。
        let headers = request_headers();
        // SAFETY: headers 为 UTF-16 缓冲且在本调用期间存活；长度即真实字符数。
        unsafe {
            WinHttpSendRequest(request.0, Some(&headers), None, 0, 0, 0).map_err(|err| {
                UpdateError::Http {
                    message: format!("WinHttpSendRequest 失败: {err}"),
                }
            })?;
            WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(|err| {
                UpdateError::Http {
                    message: format!("WinHttpReceiveResponse 失败: {err}"),
                }
            })?;
        }

        // 6) 读取响应体（先取状态码再读正文；失败路径一律 Ok(None)——不误报）。
        //    正文超过 MAX_BODY_BYTES 时按空正文处理 → 生产路径必然 Ok(None)（M8）。
        let status = query_status_code(request.0);
        let body = read_body(request.0);
        let text = match body {
            Some(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            None => String::new(),
        };
        Ok(handle(status, &text))
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
        assert!(
            is_newer_version("1.2.3", "1.2.10"),
            "patch 按数值比较而非字典序"
        );
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

    // ---- S1：HTTP 状态码管线（v0.6.1 整改） ----

    /// 复现 S1 的失效机制：不带 `WINHTTP_QUERY_FLAG_NUMBER` 时，WinHttp 以 ASCII
    /// 字符串写回头值，`"200\0"` 按小端读成 `3_158_066`——该值绝不可能命中
    /// `Some(200)` 分支。本测试把这一"非 HTTP 语义的数值"固化为可读证据。
    #[test]
    fn ascii_status_read_as_dword_is_not_http_semantic() {
        let misinterpreted = u32::from_le_bytes(*b"200\0");
        assert_eq!(misinterpreted, 3_158_066, "S1 报告的误读值");
        assert!(
            !(100..=599).contains(&misinterpreted),
            "误读值落在合法 HTTP 状态码区间之外——这正是「状态码永不命中」的根因"
        );
    }

    /// 离线回归守卫：状态码查询标志位必须包含 `WINHTTP_QUERY_FLAG_NUMBER`。
    ///
    /// FFI 调用本身无法在单测里验证，但"标志位被误删"这一**具体的回归动作**
    /// 可以被静态成分断言挡住——它正是 S1 的唯一成因。
    #[cfg(windows)]
    #[test]
    fn status_code_query_flags_must_include_number_flag() {
        use windows::Win32::Networking::WinHttp::{
            WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
        };
        let flags = imp::STATUS_CODE_QUERY_FLAGS;
        assert_ne!(
            flags & WINHTTP_QUERY_FLAG_NUMBER,
            0,
            "缺失 WINHTTP_QUERY_FLAG_NUMBER 会让状态码以 ASCII 字符串写回并被误读（S1）"
        );
        assert_eq!(
            flags & WINHTTP_QUERY_STATUS_CODE,
            WINHTTP_QUERY_STATUS_CODE,
            "信息类必须仍指向 STATUS_CODE"
        );
    }

    /// 离线回归守卫：发送头块**必须不含**尾部 NUL。
    ///
    /// 含 NUL 时 `WinHttpSendRequest` 直接返回 `E_INVALIDARG`（请求发不出去），
    /// 与 S1 的状态码误读叠加会导致"检查更新 100% 失败且无任何线索"。
    #[cfg(windows)]
    #[test]
    fn request_header_block_has_no_trailing_nul() {
        let headers = imp::request_headers();
        assert!(!headers.is_empty(), "头块不应为空");
        assert_ne!(
            *headers.last().unwrap(),
            0,
            "头块尾部不得含 NUL：dwHeadersLength 计入它会让 WinHttp 判 E_INVALIDARG"
        );
        // UA 与 Accept 必须都在（GitHub 缺 UA 返回 403）。
        let text = String::from_utf16_lossy(&headers);
        assert!(text.contains("User-Agent:"), "必须携带 User-Agent: {text}");
        assert!(text.ends_with("\r\n"), "头块应以 CRLF 收尾: {text}");
    }

    /// `interpret_response` 的状态码映射：200 提取版本，其余一律保守返回 `None`。
    #[test]
    fn interpret_response_maps_status_codes_conservatively() {
        let body = r#"{"tag_name":"v9.9.9"}"#;
        assert_eq!(
            interpret_response(Some(200), body).as_deref(),
            Some("v9.9.9"),
            "200 且 JSON 合法应提取 tag_name"
        );
        assert_eq!(
            interpret_response(Some(200), "rate limited"),
            None,
            "200 但正文非 JSON：不得误报版本"
        );
        for code in [0u32, 301, 403, 404, 500, 3_158_066] {
            assert_eq!(
                interpret_response(Some(code), body),
                None,
                "非 200 状态码 {code} 必须保守返回 None（含 S1 的误读值）"
            );
        }
        assert_eq!(
            interpret_response(None, body),
            None,
            "状态码读取失败必须保守返回 None"
        );
    }

    /// 真实网络回归（默认忽略）：`cargo test -- --ignored` 手动触发。
    ///
    /// 断言"读到的状态码是合法 HTTP 状态码"——修复前这里会得到 `3_158_066`。
    /// 网络不可用时仅打印跳过信息；但 **`E_INVALIDARG` 属于代码缺陷**（请求头块
    /// 形态错误），必须判失败而不是伪装成"环境问题"跳过。
    #[cfg(windows)]
    #[test]
    #[ignore = "真实网络回归：需联网，手动以 cargo test -- --ignored 触发"]
    fn live_status_code_is_read_as_number_not_ascii_string() {
        match imp::probe_status_code_once() {
            Ok(Some(code)) => assert!(
                (100..=599).contains(&code),
                "状态码 {code} 不是合法 HTTP 状态码——WINHTTP_QUERY_FLAG_NUMBER 很可能缺失（S1 复发）"
            ),
            Ok(None) => panic!("HTTP 管线可用时应能读到状态码"),
            Err(err) => {
                let text = err.to_string();
                assert!(
                    !text.contains("0x80070057"),
                    "E_INVALIDARG 是请求参数形态缺陷（如头块含 NUL），不得当作网络问题跳过: {text}"
                );
                eprintln!("网络不可用，跳过真实网络回归: {text}");
            }
        }
    }

    /// 真实网络回归（默认忽略）：完整管线端到端跑通（含 TLS / UA / JSON 提取）。
    /// 通过即证明三件事同时成立：请求发得出去（头块长度正确）、状态码读得对
    /// （NUMBER 标志位生效）、`tag_name` 能被提取出来。
    #[cfg(windows)]
    #[test]
    #[ignore = "真实网络回归：需联网，手动以 cargo test -- --ignored 触发"]
    fn live_fetch_latest_tag_runs_end_to_end() {
        match fetch_latest_tag() {
            Ok(Some(tag)) => assert!(
                parse_version(&tag).is_some(),
                "提取到的 tag 应可解析为语义化版本，实际: {tag}"
            ),
            Ok(None) => eprintln!("接口未返回可解析版本（可能被限流），不算回归"),
            Err(err) => {
                let text = err.to_string();
                assert!(
                    !text.contains("0x80070057"),
                    "E_INVALIDARG 是请求参数形态缺陷（如头块含 NUL），不得当作网络问题跳过: {text}"
                );
                eprintln!("网络不可用，跳过真实网络回归: {text}");
            }
        }
    }

    // ---- M8：响应体上限 + 重定向策略（v0.6.2 整改） ----

    /// 上限判定必须是**闭区间内允许、越界即拒**——边界差一字节就是"漏放 1 MiB"
    /// 或"正常响应被误拒"。
    #[test]
    fn body_limit_boundary_is_exact() {
        assert!(!exceeds_body_limit(0, 0), "空正文不受限");
        assert!(
            !exceeds_body_limit(MAX_BODY_BYTES - 1, 1),
            "恰好读到上限应放行"
        );
        assert!(!exceeds_body_limit(MAX_BODY_BYTES, 0), "刚好用满上限应放行");
        assert!(exceeds_body_limit(MAX_BODY_BYTES, 1), "超出一字节必须拒绝");
        // 极端分块长度不得引发溢出 panic（saturating 语义）。
        assert!(exceeds_body_limit(usize::MAX, usize::MAX));
    }

    /// 上限取值本身要合理：远大于真实响应体（数 KB），但仍是可承受的上界。
    ///
    /// 范围断言在**编译期**完成（常量比较无运行期意义）。
    #[test]
    fn body_limit_is_sane() {
        const _: () = {
            assert!(MAX_BODY_BYTES == 1024 * 1024, "上限应为 1 MiB");
            assert!(
                MAX_BODY_BYTES >= 64 * 1024,
                "上限过小会把正常响应误判为异常"
            );
            assert!(
                MAX_BODY_BYTES <= 16 * 1024 * 1024,
                "上限过大则失去抑制内存无界增长的意义"
            );
        };
    }

    /// 离线守卫：重定向策略必须是 `NEVER`（不允许跟随），不得被改回"可跟随"族。
    #[cfg(windows)]
    #[test]
    fn redirect_policy_must_be_never() {
        use windows::Win32::Networking::WinHttp::{
            WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP,
            WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
        };
        assert_eq!(
            imp::REDIRECT_POLICY_VALUE,
            WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
            "必须收紧为「从不跟随重定向」，否则数据源可能被引离 api.github.com"
        );
        assert_ne!(
            imp::REDIRECT_POLICY_VALUE,
            WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP,
            "默认策略仍允许跳到别的 HTTPS 主机，不足以满足信任边界要求"
        );
    }

    /// 正文超限（读到 `None`）时，生产路径必须落到"无版本信息"而**不是**误报
    /// 或有新版本——这是上限截断与报告语义之间的契约。
    #[test]
    fn oversized_body_maps_to_no_version_information() {
        // run_pipeline 在超限时交给 handle 的空正文，等价于下列调用：
        assert_eq!(
            interpret_response(Some(200), ""),
            None,
            "200 + 空正文（超限截断后的形态）必须报告「无法获取版本信息」"
        );
    }
}
