//! # 端口猎手 · Win32 监听端口枚举与降噪过滤引擎（v0.5.0）
//!
//! 基于 Windows 原生网络 API（[`GetExtendedTcpTable`] / [`GetExtendedUdpTable`]，
//! iphlpapi）实现**轻量、毫秒级、零噪音**的本地开发端口枚举：
//!
//! - **原生接口调用**：
//!   - [`GetExtendedTcpTable`]（`TCP_TABLE_OWNER_PID_ALL`）——仅提取状态为
//!     `MIB_TCP_STATE_LISTEN` 的监听项；
//!   - [`GetExtendedUdpTable`]（`UDP_TABLE_OWNER_PID`）——获取绑定的 UDP 监听项；
//! - **进程元数据提取**：[`OpenProcess`]（`PROCESS_QUERY_LIMITED_INFORMATION`）+
//!   [`QueryFullProcessImageNameW`] 获取可执行文件名与绝对路径；经
//!   [`ProcessIdToSessionId`] 获取进程会话。
//!
//! # 物理阻断（v0.5.1 重构核心：在原生表项遍历循环内硬编码拦截，绝不推到 UI 层）
//!
//! 遍历 TCP / UDP 原生表项（[`parse_tcp_table`] / [`parse_udp_table`]）的**循环
//! 首行**直接执行两道物理拦截，被拦截的行根本不会进入任何 `Vec`：
//!
//! 1. **内核态阻断**：[`is_physically_blocked`]——`pid <= 4`（含 0，Idle /
//!    System 内核态条目）**绝对不允许**进入扫描产物，杜绝「(PID: 4)」；
//! 2. **系统端口阻断**：`show_system_ports == false` 时，系统保留端口
//!    （135 / 137 / 138 / 139 / 445 / 1900 / 5353 / 5355 / 5357，见
//!    [`SYSTEM_RESERVED_PORTS`]）在循环内直接 `continue` 跳过；
//! 3. **复合主键去重**：循环内维护 `HashSet<(协议码, local_port, pid)>`——同一
//!    进程在多个 IP 监听同一端口（如 127.0.0.1 与局域网 IP 各占一行）只保留
//!    首行，从底层消灭 5353 / 1900 / 139 的多 IP 刷屏。
//!
//! # 其余降噪过滤（扫描期后置阶段，纯函数见各 `filter_*` / `is_*` 条目）
//!
//! 1. **状态降噪**：[`is_listen_state`]——剔除所有非 LISTEN 状态的 TCP 连接
//!    （丢弃 ESTABLISHED、TIME_WAIT 等），解析期即应用；
//! 2. **动态端口降噪**：[`filter_dynamic_ports`]——`show_system_ports == false`
//!    时默认过滤 IANA 动态端口 49152..=65535（[`is_iana_dynamic_port`]），除非
//!    用户在搜索框显式输入了具体端口号（[`parse_explicit_port`]，见
//!    [`FilterOptions::explicit_port`]）；
//! 3. **会话隔离**：[`filter_same_session`]——按
//!    [`ProcessIdToSessionId`] 保留与当前 GUI 用户**同一会话**的进程，剔除
//!    Session 0（Windows 服务）；会话查询失败的条目按「无法证明是 Session 0」
//!    保守保留；
//! 4. **系统服务镜像黑名单**：[`filter_system_services`] / [`is_system_service_entry`]
//!    ——剔除常见系统镜像（svchost.exe / lsass.exe / services.exe /
//!    spoolsv.exe / dwm.exe，大小写不敏感）。
//!
//! # 展示期过滤（UI 即时搜索）
//!
//! 模块扫描期即完成「物理阻断 + 状态」两阶段（结果缓存在内存），「动态端口 /
//! 系统镜像黑名单 / 搜索匹配」阶段放在渲染期由 [`filter_port_rows`] 纯函数
//! 执行：用户在搜索框输入动态端口号（如 5173）时无需重新枚举即可立即可见，
//! 搜索零延迟。

use std::collections::HashMap;
use std::path::Path;

#[cfg(windows)]
use windows::Win32::Foundation::BOOL;
#[cfg(windows)]
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, GetExtendedUdpTable, MIB_TCPROW_OWNER_PID, MIB_TCP_STATE_LISTEN,
    MIB_UDPROW_OWNER_PID, TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
};
#[cfg(windows)]
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
#[cfg(windows)]
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// IANA 动态 / 私用端口区间起点（49152..=65535）。
pub const IANA_DYNAMIC_PORT_START: u16 = 49152;

/// 会话查询失败时的进程名占位。
pub const UNKNOWN_PROCESS: &str = "<unknown>";

/// 系统镜像黑名单（小写，大小写不敏感匹配）：常见 Windows 系统服务进程。
const SYSTEM_IMAGE_BLACKLIST: &[&str] = &[
    "svchost.exe",
    "lsass.exe",
    "services.exe",
    "spoolsv.exe",
    "dwm.exe",
    "csrss.exe",
    "wininit.exe",
    "smss.exe",
];

/// 系统保留端口黑名单：Windows 内置服务的固定监听端口。
///
/// v0.5.1 强化（广播 / 系统服务降噪）：在既有 135（RPC）/ 139（NetBIOS 会话）/
/// 445（SMB）/ 5357（WSDAPI）之外，补齐 137（NetBIOS 名称服务）、138（NetBIOS
/// 数据报）、1900（SSDP 即插即用发现）、5353（mDNS 多播域名解析）与
/// 5355（LLMNR 链路本地名称解析）——`show_system_ports == false` 时这些系统 /
/// 广播监听不再出现在常规开发列表中。
///
/// 该清单同时被 `killer` 的**系统关键进程闸门**（S2）复用：占用保留端口的目标
/// 一律拒绝终止，与展示选项无关。
pub const SYSTEM_RESERVED_PORTS: &[u16] = &[135, 137, 138, 139, 445, 1900, 5353, 5355, 5357];

/// `ERROR_INSUFFICIENT_BUFFER`（122）：探测尺寸后以正确缓冲重试的标准二段式。
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
/// `AF_INET`（2）：仅枚举 IPv4 监听（与 `netstat -ano` 默认口径一致）。
const AF_INET: u32 = 2;

/// 进程镜像路径查询的初始缓冲容量（宽字符）；不足时按 API 回填的所需长度扩容。
const IDENTITY_BUFFER_INITIAL: usize = 1024;
/// 进程镜像路径查询的缓冲容量上限（宽字符，对应 ~32K 字符的极长路径）。
///
/// v0.6.1（M9②）：旧实现固定 `[0u16; 1024]`，路径超过 1023 宽字符时
/// `QueryFullProcessImageNameW` 返回 `ERROR_INSUFFICIENT_BUFFER`，被统一归入
/// `Err(_) => (<unknown>, "")`——**进程名与路径全部丢失**，既让 UI 失去悬停路径，
/// 也抽掉了「终止前身份复核」（S3）的依据。现改为按错误码扩容重试。
const IDENTITY_BUFFER_MAX: usize = 32_768;

/// 二段式原生表枚举的重试上限（v0.6.2 · M5）。
///
/// 「探测尺寸 → 分配缓冲 → 再次调用」两步之间监听表可能继续增大，此时 API 返回
/// `ERROR_INSUFFICIENT_BUFFER` 并回填新尺寸；旧实现直接 `if ret != 0 { return Err }`，
/// 导致连接频繁变动的机器上扫描**偶发失败且 UI 无任何结果**。现循环重试，
/// 上限取 5（超过即认为表处于持续增长状态，报错比无限重试更诚实）。
const TABLE_SCAN_MAX_RETRIES: usize = 5;

/// `IPPROTO_TCP`（6）：TCP 原生表行的协议码（复合去重主键用）。
const TCP_PROTOCOL_CODE: u8 = 6;
/// `IPPROTO_UDP`（17）：UDP 原生表行的协议码（复合去重主键用）。
const UDP_PROTOCOL_CODE: u8 = 17;

// ---------------------------------------------------------------------------
// 输出结构（UI / 模块缓存的数据形态）
// ---------------------------------------------------------------------------

/// 一条监听端口条目（UI 列表行与模块缓存的统一形态）。
///
/// 与 Slint 侧 `PortEntryItem` 一一对应；`process_path` 同时承担悬停提示的完整
/// 物理路径展示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortEntry {
    /// 协议（`TCP` / `UDP`，大写）。
    pub protocol: String,
    /// 本地监听端口（主机字节序）。
    pub local_port: u16,
    /// 本地绑定地址（IPv4 点分文本，如 `0.0.0.0` / `127.0.0.1`）。
    pub local_addr: String,
    /// 占用进程的 PID。
    pub pid: u32,
    /// 可执行文件名（如 `node.exe`；查询失败为 `<unknown>`）。
    pub process_name: String,
    /// 可执行文件绝对路径（查询失败为空串）。
    pub process_path: String,
}

/// 扫描期降噪 / 展示期过滤的共享选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FilterOptions {
    /// 是否显示系统服务与动态高位端口（`true` 时阶段 2 / 4 整体豁免）。
    pub show_system_ports: bool,
    /// 用户在搜索框显式输入的端口号：非 `None` 时动态端口阶段豁免
    /// （用户明确点名的高位端口不得被静默隐藏）。
    pub explicit_port: Option<u16>,
}

/// 一次扫描的报告（供模块缓存与模块日志消费）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanReport {
    /// 经「状态 + 会话」两阶段降噪后保留的条目（展示期过滤的缓存源）。
    pub entries: Vec<PortEntry>,
    /// TCP 协议原始 LISTEN 条目数。
    pub tcp_listeners: usize,
    /// UDP 协议原始绑定条目数。
    pub udp_listeners: usize,
}

/// 内部原始监听行（枚举结果，未富集进程元数据）。
struct RawListener {
    protocol: String,
    local_port: u16,
    local_addr: String,
    pid: u32,
}

// ---------------------------------------------------------------------------
// 四重降噪纯函数（可跨平台单测）
// ---------------------------------------------------------------------------

/// 阶段 1 · 状态降噪：仅 `MIB_TCP_STATE_LISTEN`（2）视为监听。
///
/// 解析 TCP 表时逐行调用——ESTABLISHED（5）、TIME_WAIT（11）、CLOSE_WAIT（8）
/// 等全部在此被剔除。
pub fn is_listen_state(state: u32) -> bool {
    state == MIB_TCP_STATE_LISTEN.0 as u32
}

/// 是否为 IANA 动态 / 私用端口（49152..=65535）。
pub fn is_iana_dynamic_port(port: u16) -> bool {
    (IANA_DYNAMIC_PORT_START..=u16::MAX).contains(&port)
}

/// 阶段 2 · 动态端口降噪。
///
/// `show_system_ports` 为 `true` 或搜索框显式点名了端口号（`explicit_port`）
/// 时整体豁免，否则剔除全部 IANA 动态端口。
pub fn filter_dynamic_ports(entries: &[PortEntry], opts: &FilterOptions) -> Vec<PortEntry> {
    if opts.show_system_ports || opts.explicit_port.is_some() {
        return entries.to_vec();
    }
    entries
        .iter()
        .filter(|entry| !is_iana_dynamic_port(entry.local_port))
        .cloned()
        .collect()
}

/// 阶段 3 · 会话隔离。
///
/// `sessions` 为 pid → 会话号查询结果（`None` = 查询失败）；仅保留与
/// `current_session` 相同会话的条目。**查询失败的条目保守保留**——无法证明其
/// 属于 Session 0 系统服务，宁可展示也不误杀（列表总会在显示期可被搜索 / 黑名单
/// 再次收敛）。
pub fn filter_same_session(
    entries: &[PortEntry],
    sessions: &HashMap<u32, Option<u32>>,
    current_session: u32,
) -> Vec<PortEntry> {
    entries
        .iter()
        .filter(|entry| match sessions.get(&entry.pid) {
            Some(Some(session)) => *session == current_session,
            _ => true, // 查询失败 / 未知：保守保留
        })
        .cloned()
        .collect()
}

/// 内核态条目判定：PID ≤ 4（Idle(0) / System(4) 等）恒为系统内核所有者。
///
/// 与黑名单的「按配置豁免」语义不同：PID ≤ 4 的条目**任何形态下都不展示**
/// （System 进程不可被「一键释放」，展示只会产生「PID: 4」噪音），故
/// [`filter_system_services`] 在按 `show_system_ports` 分支之前无条件剔除。
pub fn is_kernel_owner(pid: u32) -> bool {
    pid <= 4
}

/// 物理阻断判定（v0.5.1 重构核心：在遍历原生表项的**循环首行**硬编码调用，
/// 严禁把过滤推给 UI 层）：
///
/// - `pid <= 4`（含 0，Idle / System 内核态条目）：**绝对不允许**进入任何
///   `Vec`——不进入缓存、不进入展示，任何形态下都见不到「(PID: 4)」；
/// - `show_system_ports == false` 时，系统保留端口（135 / 137 / 138 / 139 /
///   445 / 1900 / 5353 / 5355 / 5357，见 [`SYSTEM_RESERVED_PORTS`]）：在循环
///   内直接跳过，常规开发列表不出现系统 / 广播监听。
///
/// 返回 `true` 表示该行应被物理丢弃（调用方 `continue`）。
pub fn is_physically_blocked(pid: u32, port: u16, show_system_ports: bool) -> bool {
    if pid <= 4 {
        return true; // 内核态条目：绝对不允许 PID 4 进入任何 Vec
    }
    !show_system_ports && SYSTEM_RESERVED_PORTS.contains(&port)
}

/// 是否为应被黑名单剔除的系统服务条目（PID ≤ 4 / 常见系统镜像 / 保留端口）。
///
/// 纯函数判定——[`filter_system_services`] 与测试共用。
pub fn is_system_service_entry(pid: u32, process_name: &str, port: u16) -> bool {
    if pid <= 4 {
        return true; // System(4) / Idle(0)：内核态条目
    }
    let lower_name = process_name.to_lowercase();
    if SYSTEM_IMAGE_BLACKLIST
        .iter()
        .any(|name| *name == lower_name)
    {
        return true;
    }
    SYSTEM_RESERVED_PORTS.contains(&port)
}

/// 阶段 4 · 系统服务黑名单：**无条件**剔除内核态条目（PID ≤ 4，见
/// [`is_kernel_owner`]）后，再按 `show_system_ports` 决定是否剔除
/// [`is_system_service_entry`] 命中的其余系统服务条目。
pub fn filter_system_services(entries: &[PortEntry], opts: &FilterOptions) -> Vec<PortEntry> {
    // 无条件先行剔除 PID ≤ 4（System / Idle）：内核态条目不得以「PID: 4」形式
    // 出现在任何展示形态中（不可释放 + 纯噪音，见 is_kernel_owner 的文档说明）。
    let mut out: Vec<PortEntry> = entries
        .iter()
        .filter(|entry| !is_kernel_owner(entry.pid))
        .cloned()
        .collect();
    if opts.show_system_ports {
        return out;
    }
    out.retain(|entry| !is_system_service_entry(entry.pid, &entry.process_name, entry.local_port));
    out
}

/// 从搜索文本解析用户显式输入的端口号（整串为 1..=65535 纯数字时）。
///
/// - `"5173"` / `" 5173 "` → `Some(5173)`（首尾空白容忍）；
/// - `"node"` / `""` / `"5173x"` / `"0"` → `None`（非纯端口输入）。
pub fn parse_explicit_port(search: &str) -> Option<u16> {
    let trimmed = search.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<u16>().ok().filter(|port| *port >= 1)
}

/// 展示期完整过滤（UI 即时搜索的单一入口）：
///
/// 按顺序执行「动态端口（阶段 2，含显式端口豁免）→ 系统服务黑名单（阶段 4）
/// → 搜索匹配」。`rows` 为模块缓存的「状态 + 会话」已降噪条目；搜索匹配为
/// **端口号精确** + 进程名 / 协议 / 绑定地址子串（忽略大小写）。
pub fn filter_port_rows(
    rows: &[PortEntry],
    search: &str,
    show_system_ports: bool,
) -> Vec<PortEntry> {
    let opts = FilterOptions {
        show_system_ports,
        explicit_port: parse_explicit_port(search),
    };
    let mut filtered = filter_dynamic_ports(rows, &opts);
    filtered = filter_system_services(&filtered, &opts);

    let needle = search.trim().to_lowercase();
    if !needle.is_empty() {
        filtered.retain(|entry| {
            entry.local_port.to_string() == needle
                || entry.process_name.to_lowercase().contains(&needle)
                || entry.protocol.to_lowercase().contains(&needle)
                || entry.local_addr.to_lowercase().contains(&needle)
        });
    }
    filtered
}

/// 全量四阶段降噪（一次性管线；测试与需要一次到位结果的调用方使用）。
///
/// UI 主路径请使用 [`scan_and_collect`]（阶段 2/4 延迟到渲染期）以支持即时搜索。
pub fn denoise_all(
    entries: &[PortEntry],
    opts: &FilterOptions,
    sessions: &HashMap<u32, Option<u32>>,
    current_session: u32,
) -> Vec<PortEntry> {
    let mut out = filter_dynamic_ports(entries, opts);
    out = filter_same_session(&out, sessions, current_session);
    out = filter_system_services(&out, opts);
    out
}

// v0.5.1：端口去重不再作为独立聚合阶段——同一进程多 IP 监听同一端口在
// 原生表项遍历循环内即被 HashSet 物理阻断（见 `parse_tcp_table` /
// `parse_udp_table` 的复合主键去重），缓存从诞生起就是去重形态。

// ---------------------------------------------------------------------------
// Win32 原生枚举（Windows 专属）
// ---------------------------------------------------------------------------

/// 当前进程的会话 ID（`GetCurrentProcessId` + [`ProcessIdToSessionId`]）。
#[cfg(windows)]
pub fn current_session_id() -> Option<u32> {
    session_of_pid(unsafe { GetCurrentProcessId() })
}

/// 查询指定 PID 的会话号（失败返回 `None`）。
#[cfg(windows)]
pub fn session_of_pid(pid: u32) -> Option<u32> {
    let mut session = 0u32;
    unsafe { ProcessIdToSessionId(pid, &mut session) }
        .ok()
        .map(|_| session)
}

/// 扫描全部本地监听端口（TCP + UDP），产出「物理阻断 + 状态」降噪后的
/// 展示缓存（v0.5.1 重构：PID ≤ 4、系统保留端口与多 IP 重复行全部在
/// **原生表项遍历循环内**被物理丢弃，见 [`is_physically_blocked`] 与
/// `parse_tcp_table` / `parse_udp_table` 的循环首行拦截）。
///
/// `show_system_ports`：`false` 时系统保留端口（135/137/138/139/445/1900/
/// 5353/5355/5357）在循环内直接跳过；`true` 时放行（PID ≤ 4 仍无条件阻断）。
///
/// Win32 同步调用，毫秒级；调用方（UI 线程）须经 `spawn_blocking` 移出事件循环。
#[cfg(windows)]
pub fn scan_and_collect(show_system_ports: bool) -> Result<ScanReport, super::PortError> {
    let tcp_raw = scan_tcp_listeners(show_system_ports)?;
    let udp_raw = scan_udp_listeners(show_system_ports)?;
    let tcp_count = tcp_raw.len();
    let udp_count = udp_raw.len();

    // 富集进程元数据（进程名 / 路径），并一次性收集会话查询结果。
    // L13（v0.6.2）：同一 PID 常监听多个端口，经 `identity_cache` 复用一次查询结果
    // ——旧实现对每行都 `OpenProcess` + `QueryFullProcessImageNameW`，同一进程重复
    // 几十次属无谓开销。
    let mut identity_cache: HashMap<u32, (String, String)> = HashMap::new();
    let entries: Vec<PortEntry> = tcp_raw
        .into_iter()
        .chain(udp_raw)
        .map(|raw| enrich(raw, &mut identity_cache))
        .collect();
    let mut sessions: HashMap<u32, Option<u32>> = HashMap::new();
    for entry in &entries {
        sessions
            .entry(entry.pid)
            .or_insert_with(|| session_of_pid(entry.pid));
    }

    // 会话隔离（阶段 3）：当前会话查询失败时整体跳过该阶段（避免误杀全部）。
    let entries = match current_session_id() {
        Some(current) => filter_same_session(&entries, &sessions, current),
        None => entries,
    };

    Ok(ScanReport {
        entries,
        tcp_listeners: tcp_count,
        udp_listeners: udp_count,
    })
}

/// 重新枚举监听表，返回**此刻**持有 `port` / `protocol` 的 PID（无持有者 → `None`）。
///
/// # 用途（v0.6.1 · S3）
/// 「一键释放」的目标来自**上一次扫描的缓存**，而点击可能发生在数分钟之后。
/// 从「枚举到该 PID」到「真正 `OpenProcess`」之间，原进程可能已退出、端口可能
/// 已被释放，或 PID 被系统**复用**给任意其他进程（含系统服务 / 提权进程）。
/// 终止前调用本函数做一次新鲜度核验，可消除「误杀与目标端口毫无关系的进程」。
///
/// # 口径
/// 以 `show_system_ports = true` 重扫（不做动态端口 / 系统端口过滤，避免因展示
/// 选项把目标藏掉而误判为"端口已释放"）；`pid <= 4` 的内核态条目仍被物理阻断，
/// 但此类目标会在更早的**系统关键进程闸门**（S2）被拒绝，不会走到本核验。
///
/// 非 Windows 恒返回 `Ok(None)`（端口猎手为 Win32 能力）。
#[cfg(windows)]
pub fn owner_of_port(port: u16, protocol: &str) -> Result<Option<u32>, super::PortError> {
    let report = scan_and_collect(true)?;
    Ok(report
        .entries
        .iter()
        .find(|entry| entry.local_port == port && entry.protocol.eq_ignore_ascii_case(protocol))
        .map(|entry| entry.pid))
}

/// 非 Windows 兜底：无原生监听表，恒无持有者。
#[cfg(not(windows))]
pub fn owner_of_port(_port: u16, _protocol: &str) -> Result<Option<u32>, super::PortError> {
    Ok(None)
}

/// 仅枚举 TCP LISTEN 项（`GetExtendedTcpTable(TCP_TABLE_OWNER_PID_ALL)`；
/// 解析期即应用 [`is_listen_state`] 状态降噪与循环首行物理阻断）。
#[cfg(windows)]
fn scan_tcp_listeners(show_system_ports: bool) -> Result<Vec<RawListener>, super::PortError> {
    let buffer = enumerate_table("GetExtendedTcpTable 枚举", |table, size| unsafe {
        // SAFETY: table 为本次分配的可写缓冲（首轮为 None 表示仅探测尺寸），
        // size 指向栈上 u32；失败仅返回错误码，无未定义行为。
        GetExtendedTcpTable(
            table,
            size,
            BOOL::from(false),
            AF_INET,
            TCP_TABLE_OWNER_PID_ALL,
            0,
        )
    })?;
    Ok(parse_tcp_table(&buffer, show_system_ports))
}

/// 仅枚举 UDP 绑定项（`GetExtendedUdpTable(UDP_TABLE_OWNER_PID)`；UDP 无状态
/// 概念，全部视为监听；解析期即应用循环首行物理阻断）。
#[cfg(windows)]
fn scan_udp_listeners(show_system_ports: bool) -> Result<Vec<RawListener>, super::PortError> {
    let buffer = enumerate_table("GetExtendedUdpTable 枚举", |table, size| unsafe {
        // SAFETY: 同 scan_tcp_listeners。
        GetExtendedUdpTable(
            table,
            size,
            BOOL::from(false),
            AF_INET,
            UDP_TABLE_OWNER_PID,
            0,
        )
    })?;
    Ok(parse_udp_table(&buffer, show_system_ports))
}

/// 二段式原生表枚举的通用骨架（v0.6.2 · M5）：
/// 「尺寸探测（`table = None`）→ 分配缓冲 → 枚举 → 缓冲不足则按新尺寸重试」。
///
/// - `ret == 0`：枚举完成（含**空表**：首轮 `size == 0` 时 API 直接返回 0），
///   返回写满的缓冲；
/// - `ret == ERROR_INSUFFICIENT_BUFFER`：表在两步之间增大，`size` 已被 API 回填为
///   新尺寸 → 重新分配后重试，至多 [`TABLE_SCAN_MAX_RETRIES`] 次；
/// - 其他非零：硬失败，携错误码上抛（错误文本由 `context` 定位到具体表）。
///
/// `invoke` 由调用方绑定具体的 API 与表类（TCP / UDP 仅表类常量不同）。
#[cfg(windows)]
fn enumerate_table(
    context: &'static str,
    mut invoke: impl FnMut(Option<*mut core::ffi::c_void>, *mut u32) -> u32,
) -> Result<Vec<u8>, super::PortError> {
    let mut size: u32 = 0;
    for _ in 0..TABLE_SCAN_MAX_RETRIES {
        let mut buffer = vec![0u8; size as usize];
        // 首轮 size == 0：传 None 仅探测尺寸（与 MSDN 二段式示例一致），
        // 避免把空 Vec 的悬垂指针交给 API。
        let table = if buffer.is_empty() {
            None
        } else {
            Some(buffer.as_mut_ptr() as *mut core::ffi::c_void)
        };
        let ret = invoke(table, &mut size);
        match ret {
            0 => return Ok(buffer),
            ERROR_INSUFFICIENT_BUFFER => {
                // API 已回填所需尺寸；尺寸未增长（异常值）则跳出，避免空转。
                if size == 0 {
                    break;
                }
            }
            other => {
                return Err(super::PortError::ScanFailed {
                    context,
                    code: other,
                })
            }
        }
    }
    Err(super::PortError::ScanFailed {
        context,
        code: ERROR_INSUFFICIENT_BUFFER,
    })
}

/// 行切片安全边界：按 `dwNumEntries` 计算行数，但**必须**以缓冲区实际长度封顶
/// （v0.6.2 · L12）。
///
/// 旧实现只校验 `buffer.len() < 4` 就 `from_raw_parts(ptr + 4, count)`——完全信赖
/// API 写入的 `dwNumEntries`。一旦该字段与实际长度不符（第三方 API 拦截 / 驱动
/// 改写 / 未来布局变化），就会构造越界切片，属未定义行为。
///
/// `row_size` 为单行字节数（TCP 24 / UDP 12）；头部 4 字节不参与计算。
fn bounded_row_count(buffer: &[u8], row_size: usize) -> usize {
    if buffer.len() < 4 || row_size == 0 {
        return 0;
    }
    // SAFETY: 缓冲区头部为 API 写入的 u32 条目数；read_unaligned 不要求对齐。
    let declared = unsafe { (buffer.as_ptr() as *const u32).read_unaligned() } as usize;
    let capacity = buffer.len().saturating_sub(4) / row_size;
    declared.min(capacity)
}

/// 解析 `MIB_TCPTABLE_OWNER_PID` 缓冲区：逐行应用状态降噪 + **循环首行物理
/// 阻断**后产出 `RawListener`。
///
/// 物理阻断（v0.5.1 重构核心，见 [`is_physically_blocked`]）：
///   1. `pid <= 4`（内核态）绝对不允许进入任何 `Vec`——循环首行 `continue`；
///   2. `show_system_ports == false` 时系统保留端口直接 `continue`；
///   3. 以 `(协议码, local_port, pid)` 为复合主键的 `HashSet` 去重——同一进程
///      在多个 IP 监听同一端口只保留首行，从底层消灭 5353/1900/139 多 IP 刷屏。
///
/// # Safety / 布局
/// 缓冲区由 `GetExtendedTcpTable` 写满：头部 4 字节为 `dwNumEntries`（u32），
/// 其后紧邻 `dwNumEntries` 个 `MIB_TCPROW_OWNER_PID`（24 字节 / 行，4 字节对齐）。
/// 头部以 `read_unaligned` 读取（不假设缓冲对齐）；行切片基址 = 堆指针 + 4，
/// Windows 全局分配器返回的内存至少 16 字节对齐，故 +4 后仍满足 4 字节对齐
/// （与 windows-rs 官方示例同一读写模式）。行数经 [`bounded_row_count`] 以实际
/// 缓冲长度封顶（L12），畸形表不会产生越界切片。
fn parse_tcp_table(buffer: &[u8], show_system_ports: bool) -> Vec<RawListener> {
    let count = bounded_row_count(buffer, std::mem::size_of::<MIB_TCPROW_OWNER_PID>());
    if count == 0 {
        return Vec::new();
    }
    // SAFETY: count 已由 bounded_row_count 保证 4 + count * 24 <= buffer.len()，
    // 行切片满足对齐（见上文布局说明）。
    let rows = unsafe {
        std::slice::from_raw_parts(buffer.as_ptr().add(4) as *const MIB_TCPROW_OWNER_PID, count)
    };
    // 复合主键去重集（协议码, 本地端口, PID）——循环内凡已存在的一律丢弃。
    let mut seen: std::collections::HashSet<(u8, u16, u32)> = std::collections::HashSet::new();
    rows.iter()
        .filter(|row| is_listen_state(row.dwState))
        .filter_map(|row| {
            let port = ntohs_port(row.dwLocalPort);
            let pid = row.dwOwningPid;
            // 物理阻断第一道（循环首行）：PID ≤ 4 内核态 / 系统保留端口。
            if is_physically_blocked(pid, port, show_system_ports) {
                return None; // continue：该行不进入任何 Vec
            }
            // 物理阻断第二道：同进程同端口多 IP 只保留首行。
            if !seen.insert((TCP_PROTOCOL_CODE, port, pid)) {
                return None; // continue：重复行直接丢弃
            }
            Some(RawListener {
                protocol: "TCP".to_string(),
                local_port: port,
                local_addr: fmt_ipv4(row.dwLocalAddr),
                pid,
            })
        })
        .collect()
}

/// 解析 `MIB_UDPTABLE_OWNER_PID` 缓冲区：循环首行物理阻断 + 复合主键去重
/// （布局 / 安全说明同 [`parse_tcp_table`]，拦截语义见其文档与
/// [`is_physically_blocked`]）。
fn parse_udp_table(buffer: &[u8], show_system_ports: bool) -> Vec<RawListener> {
    let count = bounded_row_count(buffer, std::mem::size_of::<MIB_UDPROW_OWNER_PID>());
    if count == 0 {
        return Vec::new();
    }
    // SAFETY: count 已由 bounded_row_count 保证 4 + count * 12 <= buffer.len()
    //（同 parse_tcp_table 的对齐说明）。
    let rows = unsafe {
        std::slice::from_raw_parts(buffer.as_ptr().add(4) as *const MIB_UDPROW_OWNER_PID, count)
    };
    // 复合主键去重集（协议码, 本地端口, PID）——循环内凡已存在的一律丢弃。
    let mut seen: std::collections::HashSet<(u8, u16, u32)> = std::collections::HashSet::new();
    rows.iter()
        .filter_map(|row| {
            let port = ntohs_port(row.dwLocalPort);
            let pid = row.dwOwningPid;
            // 物理阻断第一道（循环首行）：PID ≤ 4 内核态 / 系统保留端口。
            if is_physically_blocked(pid, port, show_system_ports) {
                return None; // continue：该行不进入任何 Vec
            }
            // 物理阻断第二道：同进程同端口多 IP 只保留首行。
            if !seen.insert((UDP_PROTOCOL_CODE, port, pid)) {
                return None; // continue：重复行直接丢弃
            }
            Some(RawListener {
                protocol: "UDP".to_string(),
                local_port: port,
                local_addr: fmt_ipv4(row.dwLocalAddr),
                pid,
            })
        })
        .collect()
}

/// 把 `dwLocalPort`（网络字节序的 u32 低 16 位）转换为主机字节序端口号。
fn ntohs_port(raw: u32) -> u16 {
    u16::from_be(raw as u16)
}

/// 把 IPv4 地址的 u32 网络字节序值格式化为点分文本（`0.0.0.0` / `127.0.0.1`）。
fn fmt_ipv4(raw: u32) -> String {
    let [a, b, c, d] = raw.to_be_bytes();
    format!("{a}.{b}.{c}.{d}")
}

/// 富集进程元数据：`OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` +
/// `QueryFullProcessImageNameW(PROCESS_NAME_WIN32)` 取得可执行文件绝对路径，
/// 文件名由路径末级派生；任一步失败回退占位（进程多半已退出或受保护）。
///
/// `identity_cache`：本轮扫描内 `pid → (进程名, 路径)` 的复用表（v0.6.2 · L13），
/// 消除"同一 PID 多端口重复查询"的放大。
#[cfg(windows)]
fn enrich(raw: RawListener, identity_cache: &mut HashMap<u32, (String, String)>) -> PortEntry {
    let (process_name, process_path) = identity_cache
        .entry(raw.pid)
        .or_insert_with(|| query_process_identity(raw.pid))
        .clone();
    PortEntry {
        protocol: raw.protocol,
        local_port: raw.local_port,
        local_addr: raw.local_addr,
        pid: raw.pid,
        process_name,
        process_path,
    }
}

/// 查询 PID 对应的（可执行文件名, 绝对路径）；失败返回（`<unknown>`, 空串）。
///
/// # 缓冲策略（v0.6.1 · M9②）
/// 从 [`IDENTITY_BUFFER_INITIAL`] 起按 `ERROR_INSUFFICIENT_BUFFER` 扩容重试
/// （API 会把所需长度回填进 `lpdwsize`），上限 [`IDENTITY_BUFFER_MAX`]——超长路径
/// 不再退化为「进程名 / 路径全部丢失」。该结果同时是「终止前身份复核」（S3）的
/// 唯一依据，故不得再以固定小缓冲换取"够用就行"。
#[cfg(windows)]
pub fn query_process_identity(pid: u32) -> (String, String) {
    // SAFETY: OpenProcess 以受限查询权限打开，句柄使用后经 CloseHandle 释放；
    // 失败返回空句柄 + 错误码，无未定义行为。
    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(handle) => handle,
        Err(_) => return (UNKNOWN_PROCESS.to_string(), String::new()),
    };

    let outcome = query_image_path(handle);
    // SAFETY: 句柄使用完毕，CloseHandle 释放（失败仅返回错误码）。
    let _ = unsafe { windows::Win32::Foundation::CloseHandle(handle) };

    match outcome {
        Some(path) => {
            let name = Path::new(&path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| path.clone());
            (name, path)
        }
        None => (UNKNOWN_PROCESS.to_string(), String::new()),
    }
}

/// 按需扩容读取进程镜像全路径（`QueryFullProcessImageNameW`）。
///
/// 返回 `None` = 该进程无法读取映像路径（权限 / 进程已退出 / API 持续报错）。
#[cfg(windows)]
fn query_image_path(handle: windows::Win32::Foundation::HANDLE) -> Option<String> {
    use windows::Win32::System::Threading::{QueryFullProcessImageNameW, PROCESS_NAME_WIN32};

    let mut capacity = IDENTITY_BUFFER_INITIAL;
    loop {
        let mut buffer = vec![0u16; capacity];
        let mut size = capacity as u32;
        // SAFETY: 形参为有效句柄与容量为 capacity 的可写缓冲区；PWSTR 指向的缓冲
        // 容量实时同步给 lpdwsize，API 按容量截断 / 回填所需长度，无越界写风险。
        let result = unsafe {
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(buffer.as_mut_ptr()),
                &mut size,
            )
        };
        match result {
            Ok(()) => {
                let len = (size as usize).min(buffer.len());
                return Some(String::from_utf16_lossy(&buffer[..len]));
            }
            Err(err) => {
                let code = (err.code().0 as u32) & 0xFFFF;
                // 仅 `ERROR_INSUFFICIENT_BUFFER` 值得重试：按 API 回填的长度扩容
                // （若回填长度不可用则翻倍），到上限即放弃。
                if code != ERROR_INSUFFICIENT_BUFFER {
                    return None;
                }
                let needed = size as usize;
                let grown = if needed > capacity {
                    needed
                } else {
                    capacity.saturating_mul(2)
                };
                if grown > IDENTITY_BUFFER_MAX || grown <= capacity {
                    return None;
                }
                capacity = grown;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 非 Windows 兜底（跨平台可编译；运行期返回空结果，供调度联调）
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
pub fn current_session_id() -> Option<u32> {
    None
}

#[cfg(not(windows))]
pub fn session_of_pid(_pid: u32) -> Option<u32> {
    None
}

#[cfg(not(windows))]
pub fn scan_and_collect(_show_system_ports: bool) -> Result<ScanReport, super::PortError> {
    Ok(ScanReport {
        entries: Vec::new(),
        tcp_listeners: 0,
        udp_listeners: 0,
    })
}

#[cfg(not(windows))]
pub fn query_process_identity(_pid: u32) -> (String, String) {
    (UNKNOWN_PROCESS.to_string(), String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(protocol: &str, port: u16, addr: &str, pid: u32, process_name: &str) -> PortEntry {
        PortEntry {
            protocol: protocol.to_string(),
            local_port: port,
            local_addr: addr.to_string(),
            pid,
            process_name: process_name.to_string(),
            process_path: format!("C:\\Program Files\\{process_name}"),
        }
    }

    // ---- 阶段 1 · 状态降噪 ----

    #[test]
    fn listen_state_only_keeps_mib_tcp_state_listen() {
        assert!(is_listen_state(2), "MIB_TCP_STATE_LISTEN(2) 应判定为监听");
        assert!(!is_listen_state(5), "ESTABLISHED(5) 应被剔除");
        assert!(!is_listen_state(11), "TIME_WAIT(11) 应被剔除");
        assert!(!is_listen_state(8), "CLOSE_WAIT(8) 应被剔除");
        assert!(!is_listen_state(3), "SYN_SENT(3) 应被剔除");
        assert!(!is_listen_state(1), "CLOSED(1) 应被剔除");
        assert!(!is_listen_state(0), "未知状态 0 应被剔除");
    }

    // ---- 阶段 2 · 动态端口降噪 ----

    #[test]
    fn iana_dynamic_port_interval_boundaries() {
        assert!(!is_iana_dynamic_port(49151), "49151 应在动态区间之外");
        assert!(is_iana_dynamic_port(49152), "49152 是动态区间起点");
        assert!(is_iana_dynamic_port(60000), "60000 落在 IANA 动态区间内");
        assert!(is_iana_dynamic_port(65535), "65535 是动态区间终点");
        assert!(
            !is_iana_dynamic_port(5173),
            "5173 低于 IANA 动态区间下限（注册端口，默认保留）"
        );
        assert!(
            !is_iana_dynamic_port(8080),
            "8080 是常用开发端口，不属于动态区间"
        );
        assert!(
            !is_iana_dynamic_port(3000),
            "3000 是常用开发端口，不属于动态区间"
        );
    }

    #[test]
    fn dynamic_ports_filtered_unless_show_system_or_explicit_search() {
        let rows = vec![
            entry("TCP", 80, "0.0.0.0", 100, "nginx.exe"),
            entry("TCP", 8080, "127.0.0.1", 101, "java.exe"),
            entry("TCP", 5173, "127.0.0.1", 102, "node.exe"),
            entry("UDP", 49152, "0.0.0.0", 103, "game.exe"),
            entry("TCP", 60000, "0.0.0.0", 104, "devtools.exe"),
        ];

        // 默认（show=false，无显式端口）：动态端口（49152..=65535）被剔除；
        // 5173 低于动态区间下限，属于需保留的开发端口。
        let filtered = filter_dynamic_ports(&rows, &FilterOptions::default());
        let ports: Vec<u16> = filtered.iter().map(|e| e.local_port).collect();
        assert_eq!(
            ports,
            vec![80, 8080, 5173],
            "默认应剔除 49152 与 60000，保留 5173（非 IANA 动态端口）"
        );

        // show_system_ports = true：全部保留。
        let filtered = filter_dynamic_ports(
            &rows,
            &FilterOptions {
                show_system_ports: true,
                explicit_port: None,
            },
        );
        assert_eq!(filtered.len(), 5, "显示系统与高位端口时应全部保留");

        // show=false 但搜索框显式输入了端口号：动态端口豁免（用户点名必须可见）。
        let filtered = filter_dynamic_ports(
            &rows,
            &FilterOptions {
                show_system_ports: false,
                explicit_port: Some(60000),
            },
        );
        assert_eq!(filtered.len(), 5, "显式端口输入应豁免动态端口过滤");
    }

    // ---- 阶段 3 · 会话隔离 ----

    #[test]
    fn session_isolation_drops_session_zero_but_keeps_unknown() {
        let rows = vec![
            entry("TCP", 8080, "0.0.0.0", 1001, "node.exe"),
            entry("TCP", 80, "0.0.0.0", 4, "System"), // Session 0 服务条目
            entry("UDP", 5353, "0.0.0.0", 1002, "svc.exe"), // 查询失败（未知会话）
        ];
        let sessions = HashMap::from([(1001, Some(1u32)), (4, Some(0u32)), (1002, None)]);

        let kept = filter_same_session(&rows, &sessions, 1);
        let pids: Vec<u32> = kept.iter().map(|e| e.pid).collect();
        assert_eq!(
            pids,
            vec![1001, 1002],
            "Session 1 条目保留；Session 0（System）剔除；查询失败的条目保守保留"
        );
        assert!(
            !kept.iter().any(|e| e.pid == 4),
            "Session 0 的 PID 4 必须被会话隔离剔除"
        );
    }

    // ---- 阶段 4 · 系统服务黑名单 ----

    #[test]
    fn system_service_blacklist_covers_pid_images_and_reserved_ports() {
        assert!(is_system_service_entry(0, "Idle", 0), "PID 0（Idle）应命中");
        assert!(
            is_system_service_entry(4, "System", 4),
            "PID 4（System）应命中"
        );
        assert!(
            is_system_service_entry(888, "svchost.exe", 49153),
            "svchost.exe 应命中"
        );
        assert!(
            is_system_service_entry(888, "SVCHOST.EXE", 49153),
            "镜像名应大小写不敏感"
        );
        assert!(
            is_system_service_entry(888, "lsass.exe", 49153),
            "lsass.exe 应命中"
        );
        assert!(
            is_system_service_entry(888, "services.exe", 49153),
            "services.exe 应命中"
        );
        assert!(
            is_system_service_entry(888, "spoolsv.exe", 49153),
            "spoolsv.exe 应命中"
        );
        assert!(
            is_system_service_entry(888, "DWM.EXE", 49153),
            "dwm.exe 应命中"
        );
        assert!(
            is_system_service_entry(1234, "anything.exe", 135),
            "保留端口 135 应命中"
        );
        assert!(
            is_system_service_entry(1234, "anything.exe", 445),
            "保留端口 445 应命中"
        );
        assert!(
            is_system_service_entry(1234, "anything.exe", 5357),
            "保留端口 5357 应命中"
        );
        assert!(
            is_system_service_entry(1234, "anything.exe", 139),
            "SMB 固定监听 139 应命中"
        );
        // v0.5.1 强化：广播 / 系统服务端口全部进入黑名单（show=false 时不再刷屏）。
        for broadcast_port in [137u16, 138, 1900, 5353, 5355] {
            assert!(
                is_system_service_entry(1234, "anything.exe", broadcast_port),
                "广播 / 系统端口 {broadcast_port} 应命中黑名单"
            );
        }
        assert!(
            !is_system_service_entry(1234, "node.exe", 3000),
            "普通开发进程 + 开发端口不应命中"
        );
        assert!(
            !is_system_service_entry(1234, "node.exe", 5173),
            "动态端口本身不触发黑名单（由阶段 2 负责）"
        );
    }

    #[test]
    fn kernel_owner_is_unconditionally_filtered_even_when_showing_system() {
        assert!(is_kernel_owner(0), "Idle(0) 恒为内核态");
        assert!(is_kernel_owner(4), "System(4) 恒为内核态");
        assert!(!is_kernel_owner(5), "PID 5 不是内核态");
        assert!(!is_kernel_owner(888), "普通进程 PID 不是内核态");

        // 「显示系统服务与高位端口」开启时，svchost 等黑名单豁免，但 PID ≤ 4
        // 的内核态条目仍无条件剔除——列表绝不出现「(PID: 4)」。
        let rows = vec![
            entry("TCP", 80, "0.0.0.0", 4, "System"),
            entry("TCP", 135, "0.0.0.0", 888, "svchost.exe"),
            entry("TCP", 8080, "127.0.0.1", 101, "java.exe"),
        ];
        let filtered = filter_system_services(
            &rows,
            &FilterOptions {
                show_system_ports: true,
                explicit_port: None,
            },
        );
        assert_eq!(filtered.len(), 2, "显示系统服务时仍剔除 PID 4 内核态条目");
        assert!(
            filtered.iter().all(|e| e.pid > 4),
            "任何展示形态下都不得包含 PID ≤ 4 的条目"
        );
    }

    #[test]
    fn system_services_filtered_unless_show_system() {
        let rows = vec![
            entry("TCP", 80, "0.0.0.0", 4, "System"),
            entry("TCP", 135, "0.0.0.0", 888, "svchost.exe"),
            entry("TCP", 8080, "127.0.0.1", 101, "java.exe"),
        ];
        let filtered = filter_system_services(&rows, &FilterOptions::default());
        let ports: Vec<u16> = filtered.iter().map(|e| e.local_port).collect();
        assert_eq!(
            ports,
            vec![8080],
            "默认黑名单应剔除 System/svchost/保留端口"
        );

        // show=true：黑名单整体豁免，但 PID 4（System）内核态条目仍无条件剔除
        //（v0.5.1 语义：PID ≤ 4 永不展示，见 is_kernel_owner）。
        let filtered = filter_system_services(
            &rows,
            &FilterOptions {
                show_system_ports: true,
                explicit_port: None,
            },
        );
        let ports: Vec<u16> = filtered.iter().map(|e| e.local_port).collect();
        assert_eq!(
            ports,
            vec![135, 8080],
            "显示系统服务时仅剩 PID 4 被无条件剔除"
        );
    }

    // ---- 显式端口解析 ----

    #[test]
    fn explicit_port_parses_only_plain_numeric_searches() {
        assert_eq!(parse_explicit_port("5173"), Some(5173));
        assert_eq!(parse_explicit_port(" 8080 "), Some(8080));
        assert_eq!(parse_explicit_port("65535"), Some(65535));
        assert_eq!(parse_explicit_port("1"), Some(1));
        assert_eq!(
            parse_explicit_port("0"),
            None,
            "端口 0 无意义，不视为显式输入"
        );
        assert_eq!(parse_explicit_port("node"), None);
        assert_eq!(parse_explicit_port(""), None);
        assert_eq!(parse_explicit_port("5173x"), None, "混合输入不视为端口号");
        assert_eq!(
            parse_explicit_port("65536"),
            None,
            "超出 u16 上限不视为端口号"
        );
    }

    // ---- 展示期完整过滤（阶段 2 + 4 + 搜索匹配） ----

    #[test]
    fn display_filter_hides_system_rows_but_respects_explicit_dynamic_port_search() {
        // 模拟模块缓存（状态 + 会话已降噪，仍含动态端口与系统黑名单条目）。
        let rows = vec![
            entry("TCP", 8080, "127.0.0.1", 1001, "node.exe"),
            entry("TCP", 3000, "127.0.0.1", 1003, "vite.exe"),
            entry("TCP", 60000, "127.0.0.1", 1004, "node.exe"), // IANA 动态端口
            entry("TCP", 80, "0.0.0.0", 4, "System"),
            entry("TCP", 445, "0.0.0.0", 888, "svchost.exe"),
        ];

        // 默认（show=false，空搜索）：动态端口 + 系统黑名单全部隐藏。
        let filtered = filter_port_rows(&rows, "", false);
        let ports: Vec<u16> = filtered.iter().map(|e| e.local_port).collect();
        assert_eq!(ports, vec![8080, 3000], "默认只剩 8080 / 3000 两个开发端口");

        // 显式搜索动态端口 60000：阶段 2 豁免 → 行可见（用户点名必须找得到）。
        let filtered = filter_port_rows(&rows, "60000", false);
        assert_eq!(filtered.len(), 1, "显式端口搜索应命中 60000 一行");
        assert_eq!(filtered[0].local_port, 60000);

        // 搜索进程名：子串匹配；动态端口仍被阶段 2 隐藏（5173 未在缓存中）。
        let filtered = filter_port_rows(&rows, "node", false);
        let ports: Vec<u16> = filtered.iter().map(|e| e.local_port).collect();
        assert_eq!(
            ports,
            vec![8080],
            "进程名搜索命中 node.exe 的 8080；60000 动态端口仍被隐藏"
        );

        // 搜索系统镜像名但不打开「显示系统服务」：阶段 4 仍生效（黑名单优先）。
        let filtered = filter_port_rows(&rows, "svchost", false);
        assert!(
            filtered.is_empty(),
            "未开启显示系统服务时黑色名单进程不可见"
        );

        // show=true：系统 / 动态全部可见；但 PID 4（System）内核态条目仍无条件
        // 剔除（v0.5.1，见 is_kernel_owner）——目录中的 System 一行不再出现。
        let filtered = filter_port_rows(&rows, "", true);
        assert_eq!(
            filtered.len(),
            4,
            "显示系统服务时展示除 PID 4 外的全部缓存行"
        );
        assert!(
            filtered.iter().all(|e| e.pid > 4),
            "任何展示形态下都不得包含 PID ≤ 4 的条目"
        );
        let filtered = filter_port_rows(&rows, "node", true);
        let ports: Vec<u16> = filtered.iter().map(|e| e.local_port).collect();
        assert_eq!(
            ports,
            vec![8080, 60000],
            "显示全量后按进程名搜索命中两条 node 行"
        );

        // 协议子串搜索（TCP）。
        let filtered = filter_port_rows(&rows, "udp", false);
        assert!(filtered.is_empty(), "无 UDP 条目时协议搜索应空");
    }

    /// 四阶段全量管线（denoise_all）组合语义：与逐阶段调用结果一致。
    #[test]
    fn denoise_all_composes_four_stages_in_order() {
        let rows = vec![
            entry("TCP", 8080, "127.0.0.1", 1001, "node.exe"),
            entry("TCP", 60000, "127.0.0.1", 1002, "node.exe"), // IANA 动态端口
            entry("TCP", 49152, "0.0.0.0", 0, "Idle"),          // Session 0 + 动态
            entry("TCP", 445, "0.0.0.0", 888, "svchost.exe"),   // 黑名单
        ];
        let sessions = HashMap::from([
            (1001, Some(1u32)),
            (1002, Some(1u32)),
            (0, Some(0u32)),
            (888, Some(1u32)),
        ]);
        let opts = FilterOptions {
            show_system_ports: false,
            explicit_port: None,
        };

        let kept = denoise_all(&rows, &opts, &sessions, 1);
        let ports: Vec<u16> = kept.iter().map(|e| e.local_port).collect();
        assert_eq!(
            ports,
            vec![8080],
            "四阶段后仅剩同会话 + 非动态 + 非黑名单的 8080"
        );

        // 显式搜索 60000：动态豁免，但会话 / 黑名单仍生效。
        let opts = FilterOptions {
            show_system_ports: false,
            explicit_port: Some(60000),
        };
        let kept = denoise_all(&rows, &opts, &sessions, 1);
        let ports: Vec<u16> = kept.iter().map(|e| e.local_port).collect();
        assert_eq!(ports, vec![8080, 60000], "显式端口只豁免阶段 2，不放松 3/4");
    }

    // ---- 物理阻断（v0.5.1：循环首行硬编码拦截，PID ≤ 4 绝不允许进入任何 Vec） ----

    #[test]
    fn physical_block_never_lets_kernel_owner_enter() {
        // PID ≤ 4（含 0）无论端口 / 显示选项如何，一律物理阻断。
        for pid in [0u32, 1, 2, 3, 4] {
            assert!(
                is_physically_blocked(pid, 8080, true),
                "PID {pid} 即使显示系统端口也阻断"
            );
            assert!(
                is_physically_blocked(pid, 8080, false),
                "PID {pid} 默认形态阻断"
            );
            assert!(
                is_physically_blocked(pid, 0, false),
                "PID {pid} 任意端口阻断"
            );
        }
        assert!(
            !is_physically_blocked(5, 8080, true),
            "PID 5 非内核态，显示系统端口时放行"
        );
        assert!(!is_physically_blocked(888, 8080, true), "普通 PID 放行");
    }

    #[test]
    fn physical_block_drops_system_ports_unless_shown() {
        // show=false：9 个系统保留端口全部在循环内跳过。
        for port in [135u16, 137, 138, 139, 445, 1900, 5353, 5355, 5357] {
            assert!(
                is_physically_blocked(888, port, false),
                "默认形态下系统端口 {port} 应被物理阻断"
            );
        }
        // show=true：放行（PID 正常）。
        for port in [135u16, 1900, 5353] {
            assert!(
                !is_physically_blocked(888, port, true),
                "显示系统服务时 {port} 放行"
            );
        }
        // 普通开发端口两种形态均放行。
        assert!(!is_physically_blocked(888, 8080, false));
        assert!(!is_physically_blocked(888, 5173, false));
    }

    #[test]
    fn physical_block_dedup_key_uses_protocol_code_port_pid() {
        // 协议码区分 TCP(6)/UDP(17)：同端口同 PID 不同协议不算重复。
        assert_ne!(TCP_PROTOCOL_CODE, UDP_PROTOCOL_CODE);
        // 复刻 parse 循环内的去重逻辑：以 (协议码, 端口, PID) 为 key，多 IP 只留首行。
        // 使用非系统端口（8080），确保物理阻断不干扰本测试的去重语义。
        let mut seen: std::collections::HashSet<(u8, u16, u32)> = std::collections::HashSet::new();
        let rows = [
            (TCP_PROTOCOL_CODE, 8080u16, 1001u32),
            (TCP_PROTOCOL_CODE, 8080, 1001), // 同进程多 IP：重复
            (TCP_PROTOCOL_CODE, 8080, 1002), // 不同 PID：不重复
            (UDP_PROTOCOL_CODE, 8080, 1001), // 不同协议：不重复
        ];
        let kept: Vec<_> = rows
            .into_iter()
            .filter(|(_, port, pid)| !is_physically_blocked(*pid, *port, false))
            .filter(|key| seen.insert(*key))
            .collect();
        assert_eq!(kept.len(), 3, "同协议同端口同 PID 的重复行被丢弃，其余保留");
        assert_eq!(kept[0], (TCP_PROTOCOL_CODE, 8080, 1001));
        assert_eq!(kept[1], (TCP_PROTOCOL_CODE, 8080, 1002));
        assert_eq!(kept[2], (UDP_PROTOCOL_CODE, 8080, 1001));
    }

    // ---- 端口 / 地址字节序（Windows 网络字节序 → 主机序） ----

    #[test]
    fn port_and_address_byte_order_conversions() {
        // dwLocalPort 为网络字节序（低 16 位）：端口 5173 (0x1435) 以 LE u32 读取
        // 得到 0x3514；ntohs_port 经 from_be 还原。
        assert_eq!(ntohs_port(0x3514), 5173);
        // 8080 = 0x1F90 → 内存 LE 读 = 0x901F → from_be = 0x1F90 = 8080。
        assert_eq!(ntohs_port(0x901F), 8080);

        assert_eq!(fmt_ipv4(0), "0.0.0.0");
        assert_eq!(fmt_ipv4(0x7F00_0001), "127.0.0.1");
        assert_eq!(fmt_ipv4(0xC0A8_0001), "192.168.0.1");
        assert_eq!(fmt_ipv4(0xFFFF_FFFF), "255.255.255.255");
    }

    #[test]
    fn port_byte_order_roundtrip() {
        // 网络字节序 → 主机序往返：任意端口的 from_be 幂等性。
        for port in [80u16, 443, 3000, 5173, 65535] {
            let network_ordered = u16::to_be(port); // API 存储形态（u16 取低 16 位）
            assert_eq!(
                ntohs_port(network_ordered as u32),
                port,
                "端口 {port} 往返应一致"
            );
        }
    }

    // ---- 原生表解析（v0.6.2 · §6.2 建议测试 #1 + L12 边界） ----

    /// 行布局常量：TCP 行 6 个 DWORD（24B）、UDP 行 3 个 DWORD（12B）。
    ///
    /// 解析代码依赖这两个数字，锁住它们可让未来的结构体变更在编译期暴露。
    #[test]
    fn native_row_sizes_match_documented_layout() {
        assert_eq!(std::mem::size_of::<MIB_TCPROW_OWNER_PID>(), 24);
        assert_eq!(std::mem::size_of::<MIB_UDPROW_OWNER_PID>(), 12);
    }

    /// 构造 `MIB_TCPTABLE_OWNER_PID` 字节缓冲：头部 `count` + 若干 24B 行。
    fn tcp_table_bytes(count: u32, rows: &[(u32, u16, u32, u32)]) -> Vec<u8> {
        let mut buf = count.to_le_bytes().to_vec();
        for (state, port, addr, pid) in rows {
            for field in [*state, *addr, u16::to_be(*port) as u32, 0, 0, *pid] {
                buf.extend_from_slice(&field.to_le_bytes());
            }
        }
        buf
    }

    /// 正常表：LISTEN 行被保留并按（端口 / 地址 / PID）正确还原。
    #[test]
    fn parses_well_formed_tcp_table() {
        let buf = tcp_table_bytes(
            2,
            &[
                (MIB_TCP_STATE_LISTEN.0 as u32, 8080, 0x7F00_0001, 1001),
                (5 /* ESTABLISHED */, 9999, 0x7F00_0001, 1002),
            ],
        );
        let rows = parse_tcp_table(&buf, true);
        assert_eq!(rows.len(), 1, "仅 LISTEN 行应保留");
        assert_eq!(rows[0].protocol, "TCP");
        assert_eq!(rows[0].local_port, 8080);
        assert_eq!(rows[0].local_addr, "127.0.0.1");
        assert_eq!(rows[0].pid, 1001);
    }

    /// 空表（`dwNumEntries = 0`）：解析为空，不 panic。
    #[test]
    fn parses_empty_tables_without_panicking() {
        assert!(parse_tcp_table(&tcp_table_bytes(0, &[]), true).is_empty());
        // 头部不足 4 字节 / 全空缓冲：同样不得 panic。
        assert!(parse_tcp_table(&[], true).is_empty());
        assert!(parse_tcp_table(&[0x01, 0x02], true).is_empty());
    }

    /// **L12 核心**：`dwNumEntries` 大于实际缓冲可容纳的行数时，必须按缓冲长度截断
    /// （而不是构造越界切片——那是未定义行为）。
    #[test]
    fn malformed_table_with_overcounted_entries_is_truncated() {
        // 声明 5 行，实际只写了 1 行（24B）。
        let buf = tcp_table_bytes(
            5,
            &[(MIB_TCP_STATE_LISTEN.0 as u32, 8080, 0x7F00_0001, 1001)],
        );
        assert_eq!(
            bounded_row_count(&buf, std::mem::size_of::<MIB_TCPROW_OWNER_PID>()),
            1,
            "行数必须以缓冲实际长度封顶"
        );
        let rows = parse_tcp_table(&buf, true);
        assert_eq!(rows.len(), 1, "应只解析出真实存在的那一行");
        assert_eq!(rows[0].pid, 1001);
    }

    /// 头部声明 0 行但缓冲里有垃圾行：按声明解析为空（不猜测、不越界）。
    #[test]
    fn malformed_table_with_undercounted_entries_yields_declared_count() {
        let buf = tcp_table_bytes(
            0,
            &[(MIB_TCP_STATE_LISTEN.0 as u32, 8080, 0x7F00_0001, 1001)],
        );
        assert!(parse_tcp_table(&buf, true).is_empty());
    }

    /// 极端声明值（`u32::MAX`）不得引发 panic 或越界读。
    #[test]
    fn absurd_entry_count_is_safely_bounded() {
        let buf = tcp_table_bytes(
            u32::MAX,
            &[(MIB_TCP_STATE_LISTEN.0 as u32, 5173, 0x0000_0000, 2002)],
        );
        assert_eq!(
            bounded_row_count(&buf, std::mem::size_of::<MIB_TCPROW_OWNER_PID>()),
            1
        );
        assert_eq!(parse_tcp_table(&buf, true).len(), 1);
    }

    /// UDP 表同构验证：三段行布局、无状态概念，全部视为监听。
    #[test]
    fn parses_udp_table_rows() {
        let mut buf = 1u32.to_le_bytes().to_vec();
        for field in [0x0000_0000u32, u16::to_be(5353) as u32, 3003] {
            buf.extend_from_slice(&field.to_le_bytes());
        }
        let rows = parse_udp_table(&buf, true);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].protocol, "UDP");
        assert_eq!(rows[0].local_port, 5353);
        assert_eq!(rows[0].pid, 3003);
    }

    /// Windows 集成冒烟：真实枚举本机监听表（无需网络；断言仅限调用成功与
    /// 结构合法性，不依赖机器上的具体端口）。
    ///
    /// v0.5.1 强化断言：物理阻断（PID ≤ 4 与默认形态的系统保留端口）在
    /// 原生表项遍历循环内生效——产物中**绝不允许**出现 PID ≤ 4，且默认形态
    /// 下 9 个系统保留端口一个都不该在场。
    #[cfg(windows)]
    #[test]
    fn live_scan_smoke_test_on_windows() {
        // 默认形态（show=false）：物理阻断应剔除全部 PID ≤ 4 与系统保留端口。
        let report = scan_and_collect(false).expect("本机监听表扫描应成功");
        for entry in &report.entries {
            assert!(!entry.protocol.is_empty());
            assert!(entry.local_port > 0, "监听端口不应为 0");
            assert!(entry.pid > 4, "物理阻断失效：扫描产物绝不允许出现 PID ≤ 4");
            assert!(
                !SYSTEM_RESERVED_PORTS.contains(&entry.local_port),
                "物理阻断失效：默认形态下系统保留端口 {} 不应在场",
                entry.local_port
            );
        }
        assert!(
            report.tcp_listeners + report.udp_listeners >= report.entries.len(),
            "降噪后条目数不应超过原始枚举数"
        );

        // 显示系统服务形态（show=true）：PID ≤ 4 仍被无条件物理阻断（内核态
        // 绝不进入任何 Vec），系统端口放行。
        let report = scan_and_collect(true).expect("本机监听表扫描应成功");
        for entry in &report.entries {
            assert!(
                entry.pid > 4,
                "物理阻断失效：即使显示系统服务，PID ≤ 4 也不得进入"
            );
        }
    }

    // ---- S3：端口持有者重枚举（"一键释放"前的新鲜度核验依据） ----

    /// 自建一条真实 TCP 监听，`owner_of_port` 必须报告**本进程**为持有者。
    ///
    /// 这是 S3 第 3 道核验的端到端验证：端口 → 持有者 PID 的解析链路真实可用，
    /// 且与 `netstat` 口径一致（仅 LISTEN 状态计入）。临时端口由 OS 分配，测试
    /// 之间互不冲突；监听器随作用域结束自然释放。
    #[cfg(windows)]
    #[test]
    fn owner_of_port_finds_self_bound_listener() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("应能绑定临时端口");
        let port = listener.local_addr().expect("应能取到本地监听地址").port();
        let owner = owner_of_port(port, "TCP").expect("监听表扫描应成功");
        assert_eq!(
            owner,
            Some(std::process::id()),
            "自建监听的持有者必须是当前进程（端口 {port}）"
        );
        // 协议名大小写不敏感（UI 传值形态与缓存形态可能不同）。
        assert_eq!(
            owner_of_port(port, "tcp").expect("扫描应成功"),
            Some(std::process::id())
        );
    }

    /// 无人监听的端口必须报告"无持有者"而非报错——S3 据此判定 `PortReleased`。
    #[cfg(windows)]
    #[test]
    fn owner_of_port_reports_none_for_unlistened_port() {
        // 先取一个由 OS 分配、随即释放的端口，保证"刚刚还在用、现在没人用"。
        let port = {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("应能绑定临时端口");
            listener.local_addr().expect("应能取到本地地址").port()
        };
        let owner = owner_of_port(port, "TCP").expect("监听表扫描应成功");
        assert_eq!(owner, None, "已释放的端口不应报告持有者（端口 {port}）");
    }
}
