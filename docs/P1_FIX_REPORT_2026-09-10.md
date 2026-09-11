# TLToolBox P1 整改记录（2026-09-10）

- **上游依据**：[`CODE_AUDIT_2026-09-10.md`](./CODE_AUDIT_2026-09-10.md) 第 5 节「改进优先级路线 · P1 — 近期（1–2 个迭代）」
- **前置**：[`P0_FIX_REPORT_2026-09-10.md`](./P0_FIX_REPORT_2026-09-10.md)（S1 / S2 / S4 / S5 已完成）
- **本次范围**：P1 全部 6 项（S3 + M9②、S6、M2 + M1、M4①、M6 + M7、CI 闸门）——**全部完成**
- **验证**：`cargo fmt --all -- --check` ✅ · `cargo clippy --all-targets -- -D warnings` **零告警** ✅ · `cargo test` **293 单测 + 5 集成/doc 全绿**（0 失败，5 项 `#[ignore]` 联网回归保持一致）✅

## 进度总览

| # | P1 子项 | 状态 | 核心落点 |
| --- | --- | --- | --- |
| 5 | **S3** 终止前重新核验进程身份（消除 PID 复用 TOCTOU） | ✅ 完成 | 终止入口新增身份复核 + 端口新鲜度复核两道闸门 |
| 5 | **M9②** 长路径进程身份退化 | ✅ 完成 | `query_process_identity` 按 `ERROR_INSUFFICIENT_BUFFER` 扩容重试 |
| 6 | **S6** 提权交接期重新持有单实例互斥 | ✅ 完成 | `acquire_after_handoff` 退避询位直至真正取得守卫 |
| 7 | **M2** 日志二次初始化 panic | ✅ 完成 | `init()` → `try_init()`，重复装配降级为可观测告警 |
| 7 | **M1** 配置损坏导致无法启动 | ✅ 完成 | `load_or_recover`：备份坏文件 + 默认值启动 + 审计/Toast |
| 8 | **M4①** 置顶用户手动取消后自动置回（UI 撒谎） | ✅ 完成 | `engine::is_topmost` 对账 + 纠偏前 / 8s 兜底周期移除条目并持久化 |
| 9 | **M6** bash `!` 未处理导致日志静默失效 | ✅ 完成 | `bash_transcript_payload` 硬拒绝 `!` / 换行 / NUL（与 cmd 对齐） |
| 9 | **M7** 终端日志清理范围过宽（误删用户数据风险） | ✅ 完成 | 目录白名单 + 深度上限 + 文件名形态白名单三重收敛 |
| 10 | **CI 闸门** `fmt --check` + `clippy -D warnings` | ✅ 完成 | `release.yml` 新增两道闸门（先于测试与编译） |

**附带收口**：M9①（非 UIPI 失败静默）——`kill_process_with_events` 现对每条非 UIPI 失败
路径必留一条 `warn` 日志，不再把可诊断性完全押在装配层的提示上。

累计新增/重写测试 **21 个**（272 → 293），全部为对"具体回归动作"的可执行断言。

---

## 5 · S3 终止前重新核验进程身份 + M9②

**问题**：用户点击的目标 `pid` 来自**上一次扫描的缓存**，点击可能发生在数分钟之后。
从「枚举到该 PID」到「真正 `OpenProcess`」之间，原进程可能已退出、端口可能已被释放、
或 PID 被系统**复用**给任意其他进程（含系统服务 / 提权进程）——旧实现直接用陈旧 PID
执行终止，会误杀与目标端口毫无关系的进程。

**处理**（在 `killer::kill_process_and_release_port` 中形成 S2/S3 共三道闸门）：

1. **系统关键进程闸门**（S2，已有）：内核态 / 系统镜像 / 保留端口一律拒绝；
2. **身份复核**（S3 新增，[`check_identity`]）：实时镜像名（`QueryFullProcessImageNameW`）
   与扫描缓存的展示名比对，不一致即判 `StaleCheck::ImageChanged`（PID 被复用）；
3. **端口新鲜度复核**（S3 新增，[`check_port_freshness`] + `scanner::owner_of_port`）：
   重新枚举 TCP/UDP 监听表，确认该 PID **此刻仍持有**该端口；无持有者或已易主即判
   `StaleCheck::PortReleased`。

两道新增核验共用闸门已做的**那一次**实时身份查询，整体成本为一次 `OpenProcess` + 一次
监听表枚举（毫秒级；调用方本就经 `spawn_blocking` 执行）。命中即返回
[`PortError::StaleTarget`]（**不发起任何终止调用**），提示语为「请刷新列表后重试」——
不误导用户去排查权限或配置。

判定逻辑抽成纯函数 [`check_identity`] / [`check_port_freshness`]，使"PID 复用"这一关键
回归点可离线单测（真实的"扫描后结束进程再点释放"难以稳定构造，其**判定语义**可以）。

**M9②**：`query_process_identity` 的进程镜像路径缓冲由固定 `[0u16; 1024]` 改为从 1024 起
按 API 回填的所需长度扩容重试、上限 32768 宽字符（≈32K 字符极长路径）。旧实现在路径
>1023 宽字符时统一归入 `<unknown>`——进程名与路径全部丢失，既让 UI 失去悬停路径，
也抽掉了 S3 身份复核的依据。

**M9① 附带**：`kill_process_with_events` 对非 UIPI 失败补 `warn` 日志（消除"失败静默"）。

**验证证据（新增 7 项）**：
`stale_check_descriptions_are_self_explanatory`、`stale_target_error_model_is_actionable`、
`identity_recheck_blocks_reused_pid_before_terminate`（真实 FFI：用当前进程冒充被复用的 PID，
断言拦下且**没有产生 Win32 错误码** → 证明未走到 `OpenProcess`）、
`identity_recheck_decision_table`、`port_freshness_decision_table`、
`owner_of_port_finds_self_bound_listener`（真实绑定临时端口，断言持有者 = 本进程）、
`owner_of_port_reports_none_for_unlistened_port`。

> 测试设计说明：涉及"终止"的用例一律采用**确定性分支**（名称不一致 / 保留端口），
> 绝不构造"所有核验都通过、只差真正终止"的场景——那会让测试进程有机会杀掉自己。
> 端口新鲜度的真实 FFI 链路由 `scanner::owner_of_port` 的独立真实监听测试覆盖。

---

## 6 · S6 提权交接期重新持有单实例互斥

**问题**：提权重启由旧实例经 `ShellExecuteW("runas")` 拉起新实例，旧实例尚持有互斥、要等
平滑收尾才释放。旧实现对该分支把守卫**降级为 `None`** 继续装配——旧实例退出后**没有任何
进程持有互斥**，整个提权执行期间单实例保证被打破：此时再次双击 exe 的新副本会被判定为
`Primary` 并常驻 → 托盘多图标、WinEvent 钩子 / 剪贴板监听 / 终端日志钩子重复注册、
模块状态互相覆盖、日志文件跨进程竞争（即 L5 被激活的场景）。

**处理**：
1. `single_instance` 拆出**不产生广播副作用**的询位原语 `imp::probe_named`（`acquire_named`
   改为在其上叠加"广播唤醒"，保持既有签名与测试不变）；
2. 新增 `acquire_after_handoff(timeout)`：以 100ms 间隔退避询位，直到真正成为 `Primary`
   并**重新持有守卫**（单实例保证全程连续）；超时（默认 [`HANDOFF_WAIT`] = 15s）才降级并
   `warn` 告警——不静默失败、不无限阻塞启动；
3. **不广播唤醒消息**：交接期不是"用户重复启动"，向旧实例广播只会让它把正在收尾的主窗口
   反复弹出（每次询位一次）；
4. `main` 的 `RESTART_MARKER` 分支改走该路径，三种结果（取得 / 超时 / 硬失败）分别给出
   如实的 `instance_log` 文案，落盘到启动日志。

**验证证据（新增 3 项）**：`handoff_reacquires_mutex_once_previous_instance_releases`
（旧实例持有时按超时降级、释放后**重新取得** Primary）、
`handoff_guard_actually_holds_uniqueness`（交接取得的守卫真实占位：他人询位为 `None`）、
`handoff_wait_is_bounded`。

---

## 7 · M2 日志二次初始化 + M1 配置损坏自愈

### M2：`init()` → `try_init()`

`tracing_subscriber` 的全局 `set_global_default` **第二次调用即 panic**。旧实现用
`SubscriberInitExt::init()`，仅靠文档口头约定"只调用一次"——`main` 当前两处调用互斥，
逻辑上成立但极其脆弱：任何未来的重复装配都会把"日志配置问题"升级为"进程崩溃"。

现三处装配点（文件层、控制台兜底层、降级兜底）全部改用 `try_init()`：重复装配成为一种
**可观测的降级**——`eprintln` + 保留既有订阅者 + 返回不含文件写线程的 guard
（`is_file_logging_active()` 为 `false`），进程照常启动。

### M1：配置损坏不再阻断启动

旧行为是「配置解析失败 → `main` 直接 `return Err` 退出」。release 构建无控制台，用户只看到
「双击程序没反应」，且没有任何应用内手段修复——必须手工定位并删掉配置文件。

新增 `ConfigManager::load_or_recover()`（`main` 的推荐入口）：

1. 正常路径与 `load()` 完全一致（含首次运行落盘默认配置）；
2. 命中 `ConfigError::Parse` 时：先把原文件备份为 `<原文件名>.bak.<yyyyMMdd-HHmmss>`
   （先 `rename`，失败退回 `copy`），再以 `AppConfig::default()` 落盘并继续启动；
3. 仅当**连坏文件都无法备份**时才返回 `Err` 终止启动——此时若继续运行，后续任何 `save`
   的原子替换都会直接覆盖用户内容，等于静默销毁数据；
4. 其他 IO 错误原样上抛，不做掩盖。

装配层（`main`）在审计子系统就绪后立刻写 `[配置自愈]` 审计 + `error` 级启动日志；Toast 因
启动期总线尚无订阅者，延后到「总线 → UI 桥」启动后补发（新增 7.1 段），告知用户备份位置。
落盘默认配置失败**不**阻断启动（与既有"首次运行引导"分支同策略：内存默认值完全可用，
等待后续某次 save 补写）。

**验证证据（新增 3 项）**：
`load_or_recover_backs_up_broken_config_and_falls_back_to_defaults`（备份内容与原始坏文件
**逐字节一致** + 生效配置为默认值 + 目标路径已重新落盘 + 命名形态校验）、
`load_or_recover_reports_nothing_for_healthy_config`、
`backup_path_is_sibling_with_timestamp_suffix`。

---

## 8 · M4① 置顶窗口「用户手动取消」检测

**问题**：用户可经 Windows 原生窗口菜单取消置顶。旧实现只在 `apply_unpin` 时移除受管条目，
用户手动取消后条目仍在 `entries`，下一次前台切换事件即把窗口**重新置顶**——用户无法真正
关闭；且 `entries` 标记 `topmost = true` 而系统实际已非置顶，**UI 在撒谎**。

**处理**：
1. `engine::is_topmost(hwnd)`：以 `GetWindowLongPtrW(GWL_EXSTYLE) & WS_EX_TOPMOST` 读取真实
   置顶状态（前置 `IsWindow` 收窄；句柄失效返回 `false`，由 `sweep_dead` 负责该类条目）；
2. `TopmostState::sweep_user_unpinned()`：批量移除「窗口存活但已非置顶」的条目，返回被移除
   的 `(hwnd, 进程名)`；判定抽成纯函数 `should_drop_user_unpinned(alive, topmost)`；
3. `TopmostManagerModule::reconcile_user_unpin()`：移除后**持久化规则快照**（配置事实源与 UI
   列表同步收敛，否则下次启动会按旧规则重新置顶），并逐条 `info` 留痕；
4. 两条触发路径：
   - **前台纠偏前**（`shield_refresh` 首行）——覆盖绝大多数场景，消除"取消后被立刻置回"；
   - **8s 兜底周期计时器**（`UNPIN_RECONCILE_TIMER_ID`）——覆盖"用户取消后长时间不切前台"，
     保证最迟一个周期内界面不再撒谎；泵退出时与防抖计时器一并 `KillTimer`。

**验证证据（新增 3 项）**：`user_unpin_detection_truth_table`（含"窗口已消亡 → 不越权，
交给 `sweep_dead`"的边界）、`reconcile_on_empty_state_is_noop`、
`is_topmost_reflects_real_window_ex_style`（真实 FFI：置顶 → 真、取消 → 假、销毁 → 假）。

> 实测要点：该测试最初用 `HWND_MESSAGE`（消息专用窗口）构造，得到**假阴性**——消息窗口不
> 参与 Z 序，`SetWindowPos(HWND_TOPMOST)` 不会落下 `WS_EX_TOPMOST`。最终改用真实的隐藏
> 顶层窗口（`WS_POPUP` 不带 `WS_VISIBLE`）。该结论已写入测试注释，避免后续误改。

---

## 9 · M6 bash `!` 处理 + M7 清理范围收敛

### M6：bash 日志路径含 `!` 时**拒绝挂载**

旧 `bash_double_quote_escape` 只转义 `` \ " $ ` ``，漏掉 `!`：注入块中
`__tltb_log_dir="…!…"` 的赋值行在交互式 Git Bash 中可能触发 history expansion
（`!xxx: event not found`），整行赋值被丢弃 → `__tltb_log_dir` 为空、`mkdir` 静默失败
（`2>/dev/null || true`），该会话 bash 日志**完全失效**且用户每次开终端都会看到报错。

处理：`bash_transcript_payload` / `bash_injected_content` 改为返回 `TerminalHookResult`，
路径含 `!` / 换行 / NUL 时返回 `TerminalHookError::InvalidLogBase`（带可读原因与修复建议），
`BashHook::install` 在**生成负载阶段**即以 `?` 中止——**不写入任何目标文件**，避免
"挂上去但静默失效"。换行 / NUL 一并从 `debug_assert!`（release 下形同虚设）升级为运行期拒绝。

策略与 `cmd.rs` 对齐（同样硬拒 `%` / `!`），三端从此一致：
「要么可用，要么明确报错」。

### M7：清理范围从"递归删一切"收敛为"只在已知布局内清理"

旧 `cleanup_expired_logs_sync` 对 `log_base` 下**任意（递归）**`.log` 文件按 mtime 删除，
`is_managed_log_file` 仅判 `.ends_with(".log")`；文档声称只扫三个子目录而**代码无此限制**
——用户把终端日志目录误配到含其他 `.log` 的项目根目录（该目录由用户自由配置）会**删除
用户数据**。

处理：三道白名单式限制（不在名单内即绝不触碰）：

1. **目录范围**：只遍历根目录（深度 0）与 `MANAGED_SUBDIRS`（`powershell` / `bash` / `cmd`，
   深度 1）；深度上限 `MAX_SCAN_DEPTH = 1`——其他目录、更深嵌套一律不进入；
2. **文件名形态**：必须匹配 `<yyyy-MM-dd_HH-mm-ss>_…`（时间戳前缀，逐位校验、零额外依赖）
   或 `cmd_…` 之一；
3. **只删普通文件**：目录、符号链接及其它条目一律不动。

**验证证据（新增 5 项，另重写 2 项）**：
`managed_log_file_rejects_user_files_that_merely_end_with_log`（`notes.log` / `build.log` /
`app-2026-01-01.log` / `2026-01-01_misc.log` / `26-01-01_…` / `2026/01/01_…` 全部拒绝）、
`managed_subdir_whitelist_is_case_insensitive_and_closed`、
`bash_payload_rejects_log_base_containing_history_expansion_char`、
`bash_payload_rejects_log_base_with_control_characters`、
`bash_and_cmd_agree_on_rejecting_bang_in_log_base`（三端一致性）；
树形清理测试重写为显式断言**越界内容一律保留**：用户自建目录（`notes/`）、深层嵌套
（`nested/deep/`）、受管子目录之下再下一层（`powershell/archive/`）、根目录内的非受管命名
（`build.log`）——清理前后 `scanned` = 7 / `removed` = 5（保留 0 天时 removed = 7）与实际完全吻合。

---

## 10 · CI 质量闸门

`.github/workflows/release.yml`：

1. 工具链步骤补 `components: rustfmt, clippy`（缺失会让闸门以"command not found"这类噪声
   失败）；
2. 新增两道闸门，**先于测试与编译、失败即终止流水线**：
   - `cargo fmt --all -- --check`
   - `cargo clippy --all-targets -- -D warnings`
3. 头部注释同步说明闸门语义。

成本说明：本仓库 `cargo clippy --all-targets` 长期保持零告警，故 `-D warnings` 的**即时成本
为零**，却能永久锁住该成果并拦住"静默吞错误"（`let _ =` / `Err(_) => {}`）的增量。
本地已按同一命令验证通过。

**验证证据**：YAML 解析通过（13 个步骤，两道闸门位于测试步骤之前，顺序已用脚本核对）。

---

## 尚未完成的子项（P2 — 择期）与当前进度

P1 已全部闭环；下列为**尚未开工**的 P2 项，按审计路线原样保留，便于下一迭代排期：

| 来源 | 子项 | 说明 |
| --- | --- | --- |
| M5 | `GetExtendedTcpTable/UdpTable` 二段式未重试 `ERROR_INSUFFICIENT_BUFFER(122)` | 连接频繁变动时扫描偶发失败 |
| M8 | 更新请求响应体无上限 + 重定向策略未收紧 | 加 `MAX_BODY` 上限与 `REDIRECT_POLICY_NEVER` |
| M10 | 模块卸载失败仅告警，可能残留对系统的持久性改写 | 升级为可重试任务 + 手动强制清理入口 |
| L1–L18 | 全部轻微项 | GDI 还原、`GdiplusShutdown`、长路径文件夹选择、日志 I/O 放大、跨进程轮转、`spawn_blocking`、会话日志命名碰撞、自身窗口识别、大小写敏感查表、失败静默、双向子串匹配、原生表边界断言、重复 `OpenProcess`、剪贴板 `expect`、`RegisterClipboardFormatW` 告警、防御性上限、`RegisterClassW` 返回值、`display_name` 缓冲 |
| §4.6 | `icon_locker` 拓扑指纹**存而不比** | 显示器拓扑变化后会把旧坐标刷到当前桌面，误移动用户图标（指纹实为死数据） |
| §4.3 | `ui/app.slint` 单文件 3,111 行 | 按模块拆分 + 抽共享组件 |
| §4.1 | 把三条线程铁律**类型化** | 用编译器替代注释守卫（`UiDeliver<T>` / `ComThread`） |
| §4.4 | 补齐原生代码测试 | 端口表解析 / 弹窗匹配 / 脚本转义 / WinHttp 管线（本次已补 21 项，仍需系统化） |
| §6.3 | `#![deny(clippy::unwrap_used, clippy::expect_used)]`、长稳测试、错误路径审计断言 | 工程手段加固 |

> 说明：本次 P1 修复过程中**未发现需要回退的既有实现**——所有改动都是在既有架构纪律
> （锁不跨 await、模型只在 UI 线程改写、COM 只在独立 OS 线程）之内叠加闸门与对账逻辑，
> 未触碰审计报告附录「已确认健壮的部分」。

---

*本文件为整改产物，随代码变更一并入库。*
