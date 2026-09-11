# TLToolBox 全量代码审计报告

- **审计对象**：`TLToolBox` v0.6.0（Rust 2021 · Slint 1.9/1.17 · 原生 Win32 常驻桌面工具箱）
- **审计日期**：2026-09-10
- **审计方式**：**只读**。未修改任何源文件、未运行构建产物、未触碰系统注册表与用户 Shell 配置。
- **代码规模**：`src/` 9,806 行 + `src/modules/` 13,599 行 + `ui/app.slint` 3,111 行 ≈ 26,500 行
- **方法**：入口与调度中枢逐行通读（`main.rs` 2,967 行全程阅读）+ 8 个模块域并行深读 + 关键指控的原始代码回溯验证 + 一次静态检查（Clippy）。

## 0. 结论速览

| 等级 | 数量 | 关键词 |
| --- | --- | --- |
| **严重** | 6 | 更新检测完全失效、可误杀系统关键进程、PID 复用 TOCTOU、停机永久挂死、FFI 回调无 panic 边界、提权后单实例失效 |
| **中等** | 10 | 配置损坏即无法启动、日志二次初始化 panic、弹窗误拦/漏拦、置顶无法真正关闭、扫描偶发失败、终端日志静默失效、清理误删、内存无上限、失败静默、卸载残留 |
| **轻微** | 18 | GDI 句柄、路径长度上限、日志 I/O 放大、并发文件竞争、匹配大小写、防御性校验缺失、UI 单文件、仓库残留等 |

**静态检查证据**：`cargo clippy --all-targets`（含全部测试目标）**零告警**通过（`Finished dev profile in 1m 03s`）。这说明本次报告中的问题**全部落在 Clippy 默认规则（`clippy::all`）的盲区**——它们不是"写法不规范"，而是"语义/边界/危险动作"层面的缺陷，必须靠针对性测试与人工审查发现。这也是本报告第二部分"补齐原生代码测试"优先级较高的直接理由。

**整体评价**：这是一份**架构纪律明显高于同类项目**的代码库——锁不跨 `await`、模型不跨线程、配置原子写 + 共享冲突重试、句柄 RAII 守卫、模块生命周期幂等、审计双轨日志，绝大多数声明与实现一致。问题集中在两类：①**少数未被真实执行路径覆盖的功能缺陷**（最典型的是更新检测，见 S1）；②**危险动作缺少"最后一道闸门"**（端口终止、FFI 回调边界，见 S2/S3/S5）。前者是"写了但跑不通"，后者是"跑得通但可能伤到用户"。

---

## 1. 严重问题（Critical）

### S1 · 检查更新永久失效：WinHttp 状态码读取缺 `WINHTTP_QUERY_FLAG_NUMBER`

- **位置**：`src/update.rs:210-225`（`query_status_code`），消费点 `src/update.rs:342-356`
- **成因**：`WinHttpQueryHeaders` 在**未带** `WINHTTP_QUERY_FLAG_NUMBER` 时，按 WinHttp 约定以 **ASCII 字符串**形式返回头值。此处把返回值直接当 `u32` 接收：`"200"` 的 4 字节 `32 30 30 00` 按小端解释为 `0x00303032 = 3,158,066`。
- **影响**：`match status { Some(200) => ... }` **永不命中**，任何成功响应都会落入 `Some(code)` 分支 → 返回 `Ok(None)` → UI 永远显示"无法获取版本信息（可能无网络或接口限流）"。**更新检查功能 100% 不可用**，且错误文案会把用户引向排查网络，误导性强。
- **建议**：
  ```rust
  WinHttpQueryHeaders(
      request,
      WINHTTP_QUERY_FLAG_NUMBER | WINHTTP_QUERY_STATUS_CODE, // ← 补上标志位
      PCWSTR::null(), Some((&mut status as *mut u32).cast()), &mut length, null_mut(),
  )
  ```
  或改为按字符串读取后 `parse::<u32>()`。**必须补一条覆盖真实 HTTP 管线的测试**（当前 `update.rs` 的 8 个单测全部只覆盖纯函数层，恰好绕开了这个 bug——这正是它长期存活的原因）。
- **置信度**：高（WinHttp 文档明确约定）；建议在提权/联网环境实测一次确认。

### S2 · 「一键释放端口」可终止系统关键进程，无任何最后闸门

- **位置**：`src/modules/port_hunter/killer.rs:126-161`（终止动作）、`mod.rs:192-223`（调用链）、`scanner.rs:257-270`（过滤器）、`ui/app.slint:1635-1776`（释放按钮）
- **成因**：三重失守叠加：
  1. `SYSTEM_IMAGE_BLACKLIST` / `is_kernel_owner` **只作用于扫描与展示过滤**；`filter_system_services` 在剔除 `PID ≤ 4` 后，若 `show_system_ports == true` 即 **直接 return**，`lsass.exe` / `services.exe` / `svchost.exe` 等条目正常进入缓存并渲染；
  2. UI 的 `PortHunterRow` 对释放按钮**不区分系统行**（`app.slint:1769-1776` 仅受 `confirm-enabled` 控制，而 `confirm_before_kill` 默认可关闭）；
  3. `killer.rs:129` 是 `let _ = (port, protocol);` —— 连传进来的进程名都被显式丢弃，终止前**零身份复核**。
- **影响**：**以管理员身份运行 + 勾选「显示系统服务与高位端口」后，一次点击即可 `TerminateProcess` 关键系统进程**，可导致系统不稳定、蓝屏或数据丢失。非管理员虽会被 UIPI 以 `ACCESS_DENIED` 挡下，但这是操作系统的兜底，不是本程序的安全设计。
- **建议**：在 `kill_process_and_release_port` **入口处无条件**复用 `is_kernel_owner(pid) || is_system_service_entry(pid, name, port)`，命中即返回专用错误且不发布 Toast（**与 UI 显示选项彻底解耦**——"可显示"不等于"可终止"）；同时 UI 侧对系统行禁用释放按钮或降级为双重确认。

### S3 · PID 复用 TOCTOU：终止前不校验进程身份

- **位置**：`killer.rs:131-146`；缓存来源 `mod.rs:135-148` / `scanner.rs` 身份富化段
- **成因**：用户点击的目标 `pid` 来自**上一次扫描的缓存**（可能已过去数分钟）。从"枚举到该 PID"到"真正 `OpenProcess`"之间，原进程可能已退出且 PID 被**任意其他进程（含系统服务、提权进程）复用**；代码直接用陈旧 PID 执行终止。
- **影响**：误杀与目标端口毫无关系的进程。与 S2 叠加后，误杀后果被进一步放大。
- **建议**：终止前重新 `OpenProcess` + `QueryFullProcessImageNameW` 取**当前**镜像名/路径，与缓存 `process_name` 比对一致才继续；或重新枚举 TCP/UDP 表确认该 PID 此刻仍持有该端口。任一路径不通过即中止并提示"进程已变化，请刷新后重试"。

### S4 · 图标锁守护线程创建失败 → 停机永久挂死

- **位置**：`src/modules/icon_locker/daemon.rs:228-242`（`.unwrap_or_default()`）、`251`、`253-257`（`GetMessageW` 循环）、`155-165`（`request_stop`）、`170-177`（`Drop` → `join`）；触发点 `icon_locker/mod.rs:270-277`
- **成因**：`CreateWindowExW(...).unwrap_or_default()` 在失败时产生**空 HWND**，但代码仍 `ready.send(hwnd.0 as isize)`（发送 `0`）并进入 `GetMessageW` 消息循环。该线程没有窗口、也没人会向它投递 `WM_QUIT`，`GetMessageW` **永不返回**。`Drop` 中 `request_stop()` 对空 HWND 判定为 `is_null()` 而跳过投递，紧接着 `handle.join()` **永久阻塞**（`icon_locker/mod.rs:275-277` 的 `drop(watchdog)` 位于 **async 上下文**）。
- **影响**：① 模块 `stop()` 挂死，**应用收尾阶段（`main.rs` 第 11 步）永久卡住，进程无法退出**，且占用一个 Tokio worker 线程；② `spawn` 静默返回 `Ok`，模块显示"运行中"却永不生效（无显示器变化响应）。
- **建议**：窗口创建后**立即判空**，失败则通过 `ready` 通道回传错误并直接返回（不进入消息泵）；`Drop` 的 `join` 改为**带超时的 join**，或先 `PostThreadMessageW(thread_id, WM_QUIT)` 再 join；`spawn` 失败必须向上返回 `Err`，不得静默降级为"运行中"。

### S5 · 全部 FFI 回调缺少 `catch_unwind` 边界（跨模块共性）

- **位置**（已用全仓 grep 确认 `catch_unwind` **零命中**）：
  - `src/modules/popup_blocker.rs:602` `win_event_proc`
  - `src/modules/topmost_manager/mod.rs:1048` `win_event_proc`
  - `src/modules/topmost_manager/enum_windows.rs:340` `enum_proc`
  - `src/modules/icon_locker/daemon.rs:60` `callback`、`:262` `window_proc`
  - `src/tray.rs` 的消息泵窗口过程（经 tray-icon/muda 间接承载）
- **成因**：这些 `unsafe extern "system" fn` 回调内部执行内存分配、`format!`/`String::from_utf16_lossy`、`Mutex` 加锁、`tracing!` 宏、总线广播等**均可 panic** 的操作；`popup_blocker.rs:594-595` 的文档明确写了"严禁 panic 跨 FFI 展开"，但**没有任何强制手段**。
- **影响**：回调内一次 panic 即跨 FFI 展开（现代 Rust 在 `extern "system"` 中直接 **abort**），**整个常驻进程瞬间消失**——无 Toast、无日志、托盘图标与全部 Win32 钩子丢失。且触发点位于系统事件派发上下文中，用户完全无从排查。这是"常驻工具"最不可接受的失败模式。
- **建议**：为每个 `extern "system"` 回调入口统一加
  ```rust
  let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| { /* 原逻辑 */ }));
  ```
  并配一条 Clippy/审查约定：FFI 回调体内禁止"可能 panic 但不被捕获"的调用。

### S6 · 提权重启后单实例守护失效窗口

- **位置**：`src/main.rs:1696-1719` + `src/single_instance.rs:224-236`
- **成因**：`acquire_named` 在 `ERROR_ALREADY_EXISTS` 分支**关闭了自己拿到的互斥句柄副本**并返回 `Secondary`；`main` 携带 `RESTART_MARKER_ARG` 时降级为 `None` guard 继续装配（注释称"旧实例必然随即退出"）。但旧实例退出时 `Drop` 会**释放互斥**，此后**没有任何进程持有该互斥**。
- **影响**：整个提权执行期间单实例保证被打破——此时再次双击 `exe` 的新副本会被判定为 `Primary` 并常驻，导致：托盘多图标、WinEvent 钩子/剪贴板监听/终端日志钩子**重复注册**、模块状态互相覆盖、日志文件跨进程竞争。这正是 `logging.rs` 午夜轮转竞态（见 L5）被激活的场景。
- **建议**：`RESTART_MARKER` 分支在确认旧实例退出后**重新 `CreateMutexW` 并重新持有守卫**（可加短重试循环直到真正成为 `Primary`）；或改用具名管道/文件锁等具备"接管语义"的交接协议，而非"存在性探测"。

---

## 2. 中等问题（Medium）

### M1 · 配置解析失败 = 应用无法启动，无自愈、无备份
**位置**：`src/config.rs:564-588`（`load`）+ `src/main.rs:1726-1734`
`load()` 对 `Parse` 返回 `Err` 且**保留用户文件**（这点正确），但 `main` 直接 `return Err` 退出。release 构建无控制台，用户只看到"程序双击无反应"；且没有任何应用内手段修复，必须手工定位并删除配置文件。
**建议**：解析失败时把坏文件改名 `tltoolbox.toml.bak.<时间戳>` 备份，**以默认配置继续启动**并写审计 + Toast 提示；仅在"连默认配置都无法落盘"时才终止启动。

### M2 · 日志二次初始化直接 panic
**位置**：`src/logging.rs:185` 与 `:207`（均为 `subscriber.init()`）
底层 `set_global_default` 在进程内第二次调用即 panic；代码用 `init()` 而非 `try_init()`，仅靠文档口头约定"只调用一次"。当前 `main` 中两处调用互斥（正常路径 / 配置失败兜底），逻辑成立但**极其脆弱**。
**建议**：统一改 `try_init()`，失败降级为告警并返回无文件日志的 guard，让"重复装配"成为一种可观测的降级而非进程崩溃。

### M3 · 弹窗黑名单匹配过宽 + 定长缓冲截断（误拦 + 漏拦并存）
**位置**：`src/modules/popup_blocker.rs:185-194`（匹配）、`:628-634`（`[0u16; 256]` 双缓冲）
① 子串 + 忽略大小写匹配、无词边界与白名单：默认黑名单含 `"Update Notice"`、`"Flash Helper Service"`、`"广告"`，任何标题**含**该串的正常窗口都会被 `PostMessageW(WM_CLOSE)` 直接关闭；`WINEVENT_SKIPOWNPROCESS` 只跳过本进程，挡不住误伤其他应用。
② 标题/类名超过 255 字符时被截断，关键词位于尾部则匹配不到 → **漏拦**。
**建议**：引入匹配模式（精确 / 前缀 / 词边界）与类名白名单；缓冲改为先 `GetWindowTextLengthW` 动态分配；`WM_CLOSE` 前对非黑名单进程做二次确认或提供"误拦恢复"入口。

### M4 · 置顶守护：用户手动取消后被自动置回；stop 超时导致双钩子
**位置**：`src/modules/topmost_manager/mod.rs:545-553`、`789-799`、`668-703`
① 受管集合只在 `apply_unpin` 时移除；用户经 Windows 原生"始终置顶"菜单取消后条目仍在 `entries`，下一次前台切换事件即把该窗口**重新置顶**——用户无法真正关闭，且 `entries` 标记 `topmost=true` 而 OS 实际已非置顶，**UI 在撒谎**。
② `stop` 的 `Err(_elapsed)` 分支仅记日志**未 `return`**，随后仍置 `running=false`，而旧泵线程尚未退出；再次 `start` 会安装**第二个** `SetWinEventHook` + `SetTimer` → 钩子/定时器泄漏与重复纠偏。
**建议**：① 纠偏路径用 `GetWindowLongPtrW(GWL_EXSTYLE) & WS_EX_TOPMOST` 校验，发现用户已取消则惰性移除条目并持久化；② stop 超时后**不得**置 `running=false`（保持"停止中"），或改为可靠 join + 幂等的残留线程检查。建议追加 5–10s 的 Z-Order 对账作为兜底。

### M5 · `GetExtendedTcpTable/UdpTable` 二段式调用未重试 `ERROR_INSUFFICIENT_BUFFER`
**位置**：`src/modules/port_hunter/scanner.rs:392-433`、`488-528`
先取尺寸 → 分配缓冲 → 再次调用；若期间表增大则返回 `122`，而代码 `if ret != 0 { return Err }` **不重试**。
**影响**：连接频繁变动的机器上扫描偶发失败，UI 无结果。
**建议**：对 `122` 循环重试（上限 3–5 次，每次用返回的新尺寸重新分配）。

### M6 · 终端日志：bash 路径未处理 `!`，交互式会话日志静默失效
**位置**：`src/modules/terminal_logger/ps_bash.rs:397-409`、`593-600`
`bash_double_quote_escape` 仅转义 `\ " $ ` `` ` ``，**漏掉 `!`**。写入 `.bashrc` 的赋值行在交互式 Git Bash 中会触发 history expansion → `!xxx: event not found`，**整行赋值被丢弃**，`__tltb_log_dir` 为空、`mkdir` 静默失败（`2>/dev/null || true`），该会话 bash 日志**完全失效且用户会看到报错**。
**附**：三端处理**不一致**——`cmd.rs:443-456` 硬拒绝 `!`、PowerShell 单引号内 `!` 为字面量（安全）、bash 被破坏。
**建议**：与 `cmd.rs` 对齐，在 bash 负载生成处硬拒绝 `!`；或在注入块首行加 `set +H` 关闭历史展开（两者可同时做）。

### M7 · 终端日志过期清理范围过宽，存在误删用户数据风险
**位置**：`src/modules/terminal_logger/retention.rs:56-58`、`82-124`
`cleanup_expired_logs_sync` 对 `log_base` 下**任意（递归）`.log` 文件**按 mtime 删除，`is_managed_log_file` 仅判 `ends_with(".log")`；文档声称只扫 `powershell/bash/cmd` 三个子目录，**代码无此限制**。
**影响**：用户把终端日志目录误配到含其他 `.log` 的目录（如某项目根目录），**会删除用户数据**。
**建议**：限定扫描已知子目录 + 文件名白名单（如 `yyyy-MM-dd_*`/`cmd_*`），并加递归深度上限。

### M8 · 更新请求：响应体无长度上限 + 重定向策略未收紧
**位置**：`src/update.rs:228-261`（`read_body`）、`:300-310`（`WinHttpOpenRequest`）
响应体以 8 KiB 分块无上限累积进 `Vec`；请求未设 `WINHTTP_OPTION_REDIRECT_POLICY_NEVER`，默认跟随重定向（可能跨主机）。
**影响**：异常/恶意响应可致内存无界增长；重定向可把数据源引离 `api.github.com`（本模块不下载二进制，泄露面有限，但信任边界应收紧）。
**建议**：加 `MAX_BODY`（如 1 MiB）上限，超限即中止返回 `Ok(None)`；设 `WINHTTP_OPTION_REDIRECT_POLICY_NEVER`，或校验最终 URL 主机仍为 `api.github.com`。

### M9 · 端口终止：非 UIPI 失败静默；长路径进程身份退化
**位置**：`killer.rs:195`（`Err(_) => {}`）、`scanner.rs:604-630`（`[0u16; 1024]` 固定缓冲）
① 除 `ACCESS_DENIED` 外的失败不发布任何 Toast，依赖装配层补齐——一旦装配层遗漏，用户"点了没反应"，状态与 UI 不一致。
② `QueryFullProcessImageNameW` 遇路径 > 1023 宽字符返回 `ERROR_INSUFFICIENT_BUFFER`，落入 `Err(_) => (UNKNOWN, "")`，进程名/路径**全部丢失**——这同时削弱了 S3 身份复核的依据。
**建议**：非 `access_denied` 失败也发一条通用失败 Toast + 审计；身份查询按返回错误码扩容重试。

### M10 · 模块卸载失败仅告警，可能留下对系统的持久性改写
**位置**：`src/main.rs:1561-1578`（全量切换）、`2941-2958`（收尾）、`terminal_logger` 卸载路径
`set_all_modules` 与收尾循环对单模块失败只 `warn!` 并继续。而终端日志模块的 `stop` 承担**回滚系统改写**的职责（移除 `.bashrc`/PowerShell profile 注入块、删除注册表 AutoRun 项）。
**影响**：卸载失败时用户的 Shell 配置文件与注册表会**残留注入块**，且下次启动可能因状态不一致而重复注入。
**建议**：卸载失败升级为可重试任务并写审计；提供"强制清理终端日志钩子"的手动入口；`is_running` 已联合注册表状态判定，建议同样用于 UI 的"需修复"提示。

---

## 3. 轻微问题（Low）

| # | 位置 | 问题 | 建议 |
| --- | --- | --- | --- |
| L1 | `tray.rs:822-910`、`platform.rs:884-902` | 位图仍被 `SelectObject` 选中时即 `DeleteObject`（应先还原原位图再删） | 用 `SelectObject` 返回值还原后再 `DeleteDC`/`DeleteObject` |
| L2 | `platform.rs:767` | `GdiplusStartup` 经 `OnceLock` 懒初始化，全程无 `GdiplusShutdown` | 进程级泄漏，常规退出无影响；如需热重载再补 |
| L3 | `platform.rs:702-729` | `SHBrowseForFolderW` 结果缓冲固定 260 宽字符，> MAX_PATH 目录无法选择 | 改用 `IFileOpenDialog` 或长路径 API |
| L4 | `logging.rs:291-315` | `AuditSink` 每条记录都重新 `open` 一次文件（I/O 放大）；运行时先关闭时最后若干条丢失 | 持有 `File` 句柄复用；收尾时显式排空 |
| L5 | `logging.rs:234-245` | `RollingFileAppender` 非跨进程安全，多进程写同一文件时午夜轮转可能互相截断（配合 S6 会被激活） | 文件名加 PID，或依赖 S6 修复后天然规避 |
| L6 | `terminal_logger/mod.rs:458-519`、`cmd.rs:754` | `install_all`/`uninstall_all` 的同步文件与注册表 IO 未走 `spawn_blocking` | 移入 `spawn_blocking` |
| L7 | `cmd.rs:356` | 会话日志文件名仅 `%TIME%+%RANDOM%`，同秒并发可能碰撞（`>>` 追加故不损坏，仅混乱） | 叠加 PID/`$$` 或更高精度时间 |
| L8 | `enum_windows.rs:125-127` | 自身窗口识别靠 `class_name.contains("slint")`，会误排其他 Slint 应用窗口 | 改用进程 PID 与自身 PID 比对 |
| L9 | `topmost_manager/mod.rs:350-357`、`881` | `memorized` 以进程名精确查 `HashMap`（大小写敏感），与规则匹配的大小写不敏感语义不一致 | 键统一 `to_ascii_lowercase` |
| L10 | `topmost_manager/mod.rs:513` | `let _ = tx.send(rules)` 静默丢弃持久化失败 | 至少 `tracing::warn!` |
| L11 | `topmost_manager/mod.rs:884-895` | 规则匹配用 `title.contains(needle) \|\| needle.contains(title)` 双向子串，可能置顶错窗口 | 恢复阶段用更严格的 `eq`/前缀匹配 |
| L12 | `scanner.rs:455-459`、`538-542` | `from_raw_parts(ptr.add(4), count)` 完全信赖 `dwNumEntries`，仅校验 `len < 4` | 加 `4 + count * size_of::<ROW>() <= buffer.len()` 断言 |
| L13 | `scanner.rs:368-374` | 同一 PID 监听多端口时重复 `OpenProcess` + 查询 | 按 PID 缓存身份 |
| L14 | `clipboard_purifier.rs:362` | `.expect("剪贴板文本长度溢出 usize")` 位于泵循环调用链，panic 会导致泵线程静默死亡（`running` 仍为真） | 改为返回 `Err` 并 `CloseClipboard` |
| L15 | `clipboard_purifier.rs:474-476` | `RegisterClipboardFormatW` 失败返回 0 被当作"格式不可用"且不记日志 | 补一条 warn 日志 |
| L16 | `config.rs:565`、`autostart.rs:260-319` | 配置整文件读入无大小上限；注册表读取未校验 `REG_SZ` 类型、按注册表声明长度分配 | 加防御性上限与类型校验 |
| L17 | `icon_locker/daemon.rs:226` | `RegisterClassW` 返回值被忽略，退出未 `UnregisterClassW`（窗口类 atom 泄漏） | 检查返回值并成对注销 |
| L18 | `icon_locker/explorer.rs:148` | `display_name` 固定 512 宽字符缓冲，超长显示名被截断 → 抓取/还原键不一致，个别图标漏还原 | 动态分配或改用稳定 ID 作键 |

**另有两项观察（非代码缺陷）**：
- `popup_blocker` 的 `EVENT_OBJECT_CREATE` 是**系统级全局钩子**，每次任意进程创建窗口都会回调。空黑名单时有短路优化，但非空时每次要压 2×512B 栈缓冲并可能分配 2 个 `String`。此处**澄清一个常见误判**：回调内的 `GetWindowTextW` / `GetClassNameW` 对**跨进程窗口**是文档保证的**非阻塞**读取（不会像 `SendMessage` 那样被挂起窗口拖死），因此"会阻塞消息泵"的说法不成立——真正的成本是**高频回调下的分配与匹配开销**。建议先按类名快速排除，或评估以 `idProcess` 收窄钩子范围。
- 仓库根目录残留一个空目录 `''/` 与探测脚本残留文件 `CUsersZhuanZAppData...session.log`（`*.log` 已在 `.gitignore` 中，实际不入库）。建议清理，避免后续 `git status` 噪音。

---

## 4. 架构与可维护性观察

1. **线程模型是本项目最大的资产，也是最需要被守卫的资产。** "模型只在 UI 线程改写""锁不跨 `await`""COM 只在独立 OS 线程"这三条铁律在 `main.rs` 中被严格执行且注释充分。但 S4（async 上下文里 `join` 阻塞线程）与 S6（互斥交接期空窗）说明：**纪律靠注释维系，缺少编译期约束**。建议把这些约束固化成类型（如封装一个 `UiDeliver<T>`、一个 `ComThread<'a>`），让违规写法无法通过编译。
2. **"失败静默"是全项目最普遍的反模式。** `killer.rs:195`、`topmost_manager/mod.rs:513`、`icon_locker/daemon.rs:226`、`clipboard_purifier.rs:474` 等处都以 `let _ =` / `Err(_) => {}` 吞掉了失败。本项目已经有很好的审计与 Toast 基建，应当**默认要求每条失败路径至少留下一条审计或日志**。
3. **UI 单文件已到维护临界点。** `ui/app.slint` 3,111 行承载 7 个模块的设置弹窗、全部组件与主题令牌。建议按模块拆分（`ui/components/`、`ui/modals/*.slint`），当前新增模块的边际成本正在快速上升。
4. **测试覆盖与风险分布严重错配。** `tests/` 仅 2 个文件 269 行（日志链路 + 模块生命周期冒烟）；而风险最高的原生代码——端口表解析、弹窗匹配、窗口过滤、脚本转义、WinHttp 管线——**几乎没有测试**。`update.rs` 的 8 个单测全部落在纯函数层，正是 S1 得以长期潜伏的直接原因。
5. **CI 缺少质量闸门。** `.github/workflows/release.yml` 只跑 `cargo test` + `cargo build`，**没有 `cargo fmt --check` 与 `clippy -D warnings`**。本次本地 `cargo clippy --all-targets` 已确认**零告警**——也就是说，加上 `-D warnings` 的**即时成本为零**，却能永久锁住这个成果、拦住"静默吞错误"这类问题的增量。
6. **文档密度很高，但存在与代码不一致之处**：`retention.rs` 声称只扫三个子目录而代码递归删所有 `.log`（M7）；`icon_locker` 的拓扑指纹被写入方案却**从不参与还原校验**（`pick_auto_restore_profile` 只挑"活动/最新"就直接还原）——显示器拓扑变化后会把旧坐标刷到当前桌面，**误移动用户图标**，指纹实为死数据。建议要么让指纹真正参与校验（不匹配则跳过并 Toast），要么删除该字段以免误导。

---

## 5. 改进优先级路线

### P0 — 立即（本迭代内，均为小改动、高收益）
1. **修 S1**：补 `WINHTTP_QUERY_FLAG_NUMBER`，并补一条覆盖 HTTP 管线的测试。
2. **修 S2**：`kill_process_and_release_port` 入口强制系统进程拒绝清单，与 UI 选项解耦。
3. **修 S4**：窗口创建失败即返回错误 + `join` 加超时。
4. **修 S5**：为 5 处 `extern "system"` 回调统一加 `catch_unwind`（机械性改动，半小时内可完成）。

### P1 — 近期（1–2 个迭代）
5. **修 S3 + M9②**：终止前重新核验进程身份，消除 PID 复用 TOCTOU。
6. **修 S6**：提权交接期重新持有单实例互斥。
7. **修 M2 + M1**：`try_init` 降级；配置损坏自动备份 + 默认值启动。
8. **修 M4①**：置顶窗口的"用户手动取消"检测，消除 UI 状态撒谎。
9. **修 M6 + M7**：bash `!` 处理 + 清理范围收敛（一个可能静默失效、一个可能误删数据）。
10. **补 CI 闸门**：`cargo fmt --check` + `clippy -D warnings`。

### P2 — 择期（结构性改善）
11. **修 M5 / M8 / M10 与全部 L 级项**：二段式 API 重试、HTTP 上限与重定向策略、卸载失败重试、GDI 还原、防御性边界校验。
12. **让 `icon_locker` 的拓扑指纹真正生效**，或删除该字段。
13. **拆分 `ui/app.slint`**，并抽出共享组件。
14. **把三条线程铁律类型化**，用编译器替代注释来守卫。
15. **补齐原生代码测试**（见下节），把高风险纯函数从设备相关代码中剥离出来以提升可测性。

---

## 6. 验证与测试思路（可选）

### 6.1 针对 P0 的定向验证
- **S1**：在联网环境点一次"检查更新"并抓 `logs/tltoolbox.<date>.log` 中 `target: "update"` 的 warn 行。若打印出的状态码是 `3158066` 而不是 `200`，即**实锤**。随后补 `#[ignore]` 的真实网络回归测试 + 一个可注入 `status` 的单元测试。
- **S2/S3**：在**虚拟机快照**中，以管理员启动 → 勾选"显示系统服务与高位端口" → 确认 `lsass.exe` 等行出现；用 `SendMessage`/调试器在 `TerminateProcess` 前打断点，验证是否走了拒绝分支。TOCTOU 可用"扫描后手动结束目标进程，再点释放"的方式复现（观察是否报错而非误杀）。
- **S4**：单元测试注入"窗口创建失败"（可将 `CreateWindowExW` 调用抽到可替换的 trait 后 mock），断言 `spawn` 返回 `Err`；另加一个 `stop()` 带超时的测试，确保不会挂死。
- **S5**：为每个回调写一个"故意 panic"的测试替身（`#[should_panic]` 不适用，改为断言进程存活 + 日志出现"回调 panic 已捕获"）。

### 6.2 建议新增的自动化测试（按性价比排序）
1. **端口表解析（`scanner`）**：构造人工 `MIB_TCPTABLE_OWNER_PID` 字节缓冲（含边界条目：`dwNumEntries = 0`、单条、`count` 与实际长度不符的畸形表），断言不 panic 且解析正确 → 同时覆盖 L12/L13。
2. **弹窗匹配（`popup_blocker`）**：纯函数化 `matches(title, class, patterns)`，覆盖：超长标题（>255）尾部关键词、`Update` 命中 `Update Notice` 之外的正常标题、大小写、空白规则 → 覆盖 M3。
3. **终端脚本转义（`ps_bash`/`cmd`）**：表驱动测试 `log_base` 取值集合 `{"含空格", "含\"", "含!", "含$", "含`", "含换行", "超长路径"}`，断言"要么生成可被对应 shell 正确解析的脚本，要么明确返回 Err"——当前 M6 正是被这条测出来的。
4. **清理策略（`retention`）**：构造临时目录树，断言只删已知子目录的已知形态文件，其他 `.log` 一律保留 → 覆盖 M7。
5. **版本比较与状态码映射（`update`）**：把 `status → Result` 的映射抽成纯函数 `fn evaluate(status_str_or_num, body)`，直接覆盖 S1。
6. **配置降级（`config`）**：断言"文件损坏 → 备份原文件 + 返回默认配置 + 原内容不丢" → 覆盖 M1。
7. **模块生命周期并发**：对每个模块跑 `start/stop` 各 100 次的并发压测（`clipboard_purifier.rs:808-822` 已有雏形），断言 `is_running()` 与实际资源一致、无句柄泄漏（可用 `GetGuiResources` / `GetProcessHandleCount` 前后对比）。

### 6.3 建议引入的工程手段
- **`cargo clippy -D warnings` + `cargo fmt --check` 进 CI**（P1）。
- **`#![deny(clippy::unwrap_used, clippy::expect_used)]` 于 `src/`，仅在 `#[cfg(test)]` 内放宽**——本项目大量 `expect` 都落在测试里，这个 lint 的成本几乎为零。
- **长稳测试**：常驻类工具应有一项"连续运行 24h + 反复切换全部模块 200 次"的冒烟，断言句柄数与工作集不单调增长。本项目已有 `EmptyWorkingSet` 主动压制内存，这类测试能同时验证该机制。
- **错误路径审计断言**：写一条测试钩子，统计"被静默吞掉的 `Result`"数量并设阈值，把第 4 节观察到的反模式变成可度量的指标。

---

## 附录 · 已确认健壮的部分（避免误改）

以下经代码核对确认实现正确，建议在重构时**保持不动**：

- **配置原子写**：同目录临时文件 + `rename`，并对 `ERROR_SHARING_VIOLATION` 做 20ms×3 退避重试（`config.rs:600-652`）——设计优于多数同类项目。
- **`resolve_app_path`**：`current_exe()` 失败时优雅回退而非 panic（`config.rs:78-89`），"Run 键自启时 CWD=System32"这一经典坑已被正确规避。
- **句柄释放**：`win32::KeyGuard`、`HttpHandle`、`TokenGuard`、`SingleInstanceGuard` 均为全路径 RAII；`CoTaskMemFree` 释放 PIDL 正确（`platform.rs:729`）。
- **提权参数转义**：`quote_cmdline_arg` 严格遵循 `CommandLineToArgvW` 逆规则且有单测（`platform.rs:385`），UAC 取消（`1223`）被正确识别。
- **`keep_awake`**：`SetThreadExecutionState` 的 `ES_CONTINUOUS` 语义、错误码检查、停止时还原**全部正确**。
- **事件总线**：基于 `broadcast` 无锁发布，无订阅者容器增长、无重入死锁、无锁中毒风险（`bus.rs:126-137`）。
- **调度器**：模块契约强制 `&self` + `Send + Sync`，无 `std::sync::Mutex` 跨 `await`（`modules/mod.rs:33-54`、`manager.rs`）。
- **UI 状态收敛**：`set_vec` reset 语义 + 真实状态快照回填，失败自然回滚，"界面即事实"的闭环成立。
- **`update.rs` 纯函数层**：`extract_tag_name` / `parse_version` / `is_newer_version` 语义正确且测试完备（含 `1.2.10 > 1.2.3` 的数值比较、畸形 JSON 与不可解析版本的保守返回）。

---

*报告结束。本报告为只读审计产物，未对任何源文件、系统注册表或用户 Shell 配置做出修改。*
