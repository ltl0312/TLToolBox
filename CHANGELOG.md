# 更新日志

本项目的全部重要变更均记录在此文件中。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
本项目遵循 [语义化版本](https://semver.org/lang/zh-CN/)（SemVer）。

> **版本史说明**：`v0.2.0` 起的条目依据 git 提交历史与 Release Tag 回溯；
> `v0.1.0` 早于仓库首个提交（首个提交即 v0.2.0 架构转型版），该条目依据
> 《计划书.md》与 `Cargo.toml` 中的架构转型注释回溯，日期不可考。

## [Unreleased]

### Fixed

- **M5 · 监听表扫描偶发失败（`port_hunter/scanner`）**：TCP / UDP 二段式枚举收敛为
  `enumerate_table` 通用骨架，对 `ERROR_INSUFFICIENT_BUFFER` 循环重试（上限 5 次、
  每次按 API 回填尺寸重新分配），消除连接频繁变动时"扫描无结果"；
- **M8 · 更新请求边界（`update`）**：响应体读取加 1 MiB 上限（超限中止并按"无法获取
  版本信息"处理，杜绝内存无界增长）；请求设置 `WINHTTP_OPTION_REDIRECT_POLICY_NEVER`
  收紧信任边界（联网回归实测通过）；
- **M10 · 卸载失败残留（`terminal_logger`）**：卸载自动重试（3 次 × 150ms）；
  新增 `needs_repair` / `last_stop_error` 残留可见；新增 `force_cleanup_hooks` 强制清理
  入口并接入终端设置弹窗按钮；`set_all_modules` 与收尾路径的失败补写审计；
- **M3 · 弹窗黑名单匹配过宽 / 漏拦（`popup_blocker`）**：关键词支持 `exact:` / `prefix:` /
  `word:`（ASCII 词边界）显式收窄前缀（无前缀保持既有子串语义，旧配置零迁移）；
  标题 / 类名改动态缓冲（4096 字符封顶），超长标题尾部关键词不再漏拦；
- **M4② · 置顶 stop 超时双钩子（`topmost_manager`）**：泵线程未按期退出时保持
  「停止未完成」并放回运行句柄，不再错误置 `running = false` 导致重复装钩；
- **P2-12 · 图标锁拓扑指纹生效（`icon_locker`）**：自动还原前校验方案拓扑指纹与当前
  拓扑一致性，不匹配即跳过 + Toast，修复"旧坐标刷到不同桌面、误移动用户图标"；
- **L 级项全量（18 项）**：GDI 位图先还原再删除（L1，截图 / 托盘两处）、文件夹选择改用
  `SHGetPathFromIDListEx` 支持长路径（L3）、审计日志句柄复用（L4）、终端日志钩子安装 /
  卸载 IO 移出 Tokio 工作线程（L6）、cmd 会话日志名追加熵段防碰撞（L7）、自身窗口按 PID
  精确识别（L8）、优先级记忆键大小写不敏感（L9）、规则持久化失败告警（L10）、恢复匹配
  反向子串收紧为前缀关系（L11）、原生表行数按缓冲长度封顶（L12）、扫描身份查询按 PID
  复用（L13）、剪贴板溢出 `.expect` 结构化降级（L14）、剪贴板格式注册失败告警（L15）、
  配置整读大小上限 + 自启注册表 `REG_SZ` 类型校验（L16）、窗口类 atom 成对注销（L17）、
  图标显示名缓冲扩容 + 触顶跳过（L18）；L2 / L5 按审计给出的替代方案以文档固化取舍。
- **S3 · 端口终止的 PID 复用 TOCTOU（`port_hunter`）**：`kill_process_and_release_port`
  在系统关键进程闸门之后追加**两道新鲜度核验**——① 实时镜像名与扫描缓存名比对，
  不一致即判 PID 已被复用；② 重新枚举监听表确认该 PID 此刻仍持有该端口。任一不过即
  `Err(StaleTarget)` 并**不发起终止**，提示「请刷新列表后重试」；
- **M9 · 长路径下进程身份退化 + 非 UIPI 失败静默（`port_hunter`）**：
  `query_process_identity` 的镜像路径缓冲由固定 1024 宽字符改为按
  `ERROR_INSUFFICIENT_BUFFER` 扩容重试（上限 32768）；`kill_process_with_events`
  对非 UIPI 失败补 `warn` 日志，消除"失败静默"；
- **S6 · 提权重启后单实例守护空窗（`single_instance`）**：新增
  `acquire_after_handoff`——交接期以 100ms 退避**持续询位**（不广播唤醒消息），直到旧实例
  释放互斥后**重新持有**守卫，单实例保证全程连续；超时（15s）才降级并如实告警。此前该分支
  降级为无守卫，提权执行期间可并存多实例（托盘双图标、钩子重复注册、日志跨进程竞争）；
- **M2 · 日志二次初始化 panic（`logging`）**：三处全局订阅者装配点由 `init()` 改为
  `try_init()`，重复装配降级为可观测告警 + 保留既有订阅者，不再把"日志配置问题"升级为
  进程崩溃；
- **M1 · 配置损坏导致应用无法启动（`config` / `main`）**：新增
  `ConfigManager::load_or_recover`——解析失败时先把原文件备份为
  `<原文件名>.bak.<yyyyMMdd-HHmmss>`（rename 失败退回 copy，内容一字不丢），再以默认配置
  继续启动，并写 `[配置自愈]` 审计 + Toast 告知备份位置；仅当连备份都失败才终止启动；
- **M4① · 置顶窗口"用户手动取消"后被自动置回（`topmost_manager`）**：新增
  `engine::is_topmost`（读 `WS_EX_TOPMOST`）与 `sweep_user_unpinned` 对账——纠偏路径
  **前置对账** + 8s 兜底周期计时器，发现用户已取消即移除受管条目并持久化规则，
  消除「用户关不掉置顶」与「UI 状态撒谎」；
- **M6 · bash 日志路径含 `!` 导致会话日志静默失效（`terminal_logger`）**：
  `bash_transcript_payload` 硬拒绝 `!` / 换行 / NUL（`TerminalHookError::InvalidLogBase`），
  在写入任何目标文件之前中止；与 `cmd.rs` 对 `%` / `!` 的策略对齐，三端一致；
- **M7 · 终端日志清理范围过宽、存在误删用户数据风险（`terminal_logger/retention`）**：
  从"递归删任意 `.log`"收敛为三道白名单——目录范围（根 + `powershell`/`bash`/`cmd`，
  深度上限 1）、文件名形态（`<yyyy-MM-dd_HH-mm-ss>_…` 或 `cmd_…`）、仅普通文件。
  用户自建目录 / 深层嵌套 / 根目录内非受管命名一律不触碰。

**同批次 P0（S1 / S2 / S4 / S5）**

- **S1 · 检查更新永久失效（`update`）**：`WinHttpQueryHeaders` 补 `WINHTTP_QUERY_FLAG_NUMBER`
  ——缺失该标志位时 WinHttp 按 ASCII 字符串写回状态码，`"200"` 被小端误读为 `3158066`，
  导致 `Some(200)` 永不命中、检查更新 100% 不可用；
- **S1-b · 更新请求根本发不出去（`update`）**：`WinHttpSendRequest` 的头块切片此前带
  NUL 终止符，WinHttp 判定 `dwHeadersLength` 区间内出现 NUL 直接返回 `E_INVALIDARG`
  （`0x80070057`）。现统一剥离终止符（本缺陷为整改实测新发现，与 S1 叠加存在）；
- **S2 · 一键释放端口可终止系统关键进程**：`kill_process_and_release_port` 入口新增
  **无条件系统关键进程闸门**（内核态 PID ≤ 4 / 系统服务镜像 / 系统保留端口），命中即
  `Err(ProtectedProcess)` 且**永不触碰**终止 API；判定与「显示系统服务」选项彻底解耦，
  镜像名取终止时刻实时查询结果。UI 侧受保护行不再渲染释放按钮（降级为「系统进程」标签）；
- **S4 · 图标锁守护线程创建失败导致停机永久挂死**：`CreateWindowExW` 失败即经就绪通道
  回传错误并返回（不进入消息泵），`spawn` 向上返回 `Err` 而非静默降级为「运行中」；
  `Drop` 改为**带 2s 超时的 join**，`stop()` 不再可能永久阻塞 Tokio worker 与应用收尾；
- **S5 · FFI 回调缺少 panic 边界**：新增 `src/ffi_guard.rs`（`guard_ffi`），为 5 处
  `extern "system"` 回调入口统一包裹 `catch_unwind`——此前回调内任意 panic 都会跨 FFI
  展开直接 abort 整个常驻进程（托盘、钩子、监听全部瞬失且无任何反馈）。托盘消息泵
  单轮迭代亦整体入界，覆盖 muda / tray-icon 的第三方窗口过程。

### Added

- **P2 · 线程铁律类型化（`thread_rules`）**：新增 `UiThreadToken`（UI 线程唯一令牌，
  main 装配期领取）与 `UiDeliver<T>`（凭令牌构造的 UI 交付通道），铁律①获得构造期守卫；
- **P2 · UI 拆分第一步**：`Theme` 主题令牌与 6 个共享控件外移至 `ui/theme.slint` /
  `ui/components.slint`，`app.slint` 3158 → 2860 行；
- **P2 · 原生测试 46 项**（293 → 318）：覆盖监听表解析与畸形表、弹窗匹配模式、
  更新上限 / 策略守卫、终端日志钩子清理与重试、图标锁指纹校验、模块 50 轮启停压测等；
- **CI 质量闸门（`.github/workflows/release.yml`）**：新增 `cargo fmt --all -- --check`
  与 `cargo clippy --all-targets -- -D warnings`，先于测试与编译执行、失败即终止流水线；
  工具链步骤补 `rustfmt` / `clippy` 组件。本仓库 clippy 长期零告警，闸门即时成本为零；
- **P1 回归测试 21 项**（272 → 293）：覆盖 PID 复用判定、端口持有者重枚举、提权互斥交接、
  配置自愈、用户取消置顶判定与真实窗口 `WS_EX_TOPMOST` 读取、bash `!` 拒绝、清理范围越界
  保留等；
- **可测性重构（`update`）**：`interpret_response` 纯函数抽出，WinHttp 管线收敛为单一
  `run_pipeline`，新增 2 条 `#[ignore]` 联网回归测试（`cargo test -- --ignored` 手动触发）
  与 4 条离线回归守卫；`ffi_guard` 自身 4 个单测。

### Docs

- 新增 `docs/P2_FIX_REPORT_2026-09-11.md`：P2 五项进度（M5/M8/M10 + L 全量、指纹生效、
  线程铁律类型化、UI 拆分第一步、原生测试）、§6.3 lint 评估结论与未完成事项；
- 新增 `docs/P1_FIX_REPORT_2026-09-10.md`：P1 六项（S3 + M9、S6、M2 + M1、M4①、M6 + M7、
  CI 闸门）逐项整改记录、验证证据，以及尚未完成的 P2 子项清单与进度；
- 新增 `docs/P0_FIX_REPORT_2026-09-10.md`：逐项记录 P0（S1 / S2 / S4 / S5）的整改内容、
  验证证据与未闭环项（其中 S3 已在本次 P1 迭代闭环）。

## [0.6.0] - 2026-09-10

### Added

- **桌面图标布局锁模块（`icon_locker`，第七个模块卡片）**：保存当前桌面图标布局为方案，
  屏幕拓扑切换后自动瞬移归位，解决插拔外接屏 / 修改分辨率导致图标乱跑的顽疾；
- **显示器拓扑指纹**：基于 `EnumDisplayMonitors` + `GetMonitorInfoW` 采集主副屏几何，
  按「主屏优先 + 坐标升序」归一化为唯一指纹（如 `P:0,0,2560x1440|S:-1920,0,1920x1080`），
  不同屏幕拓扑独立保存布局方案、互不串扰；
- **纯原生 STA Shell COM 抓取 / 还原引擎**：`IShellWindows::FindWindowSW(SWC_DESKTOP)`
  直连桌面视图，沿 `IShellBrowser → IFolderView` 链路以 `GetItemPosition` /
  `SelectAndPositionItems` 批量操作坐标；**零跨进程内存注入**——不调用
  `VirtualAllocEx` / `ReadProcessMemory` 等进程内存 API；
- **独立 OS 线程 COM 执行**：每次抓取 / 还原经 `spawn_com_thread` 拉起全新独立线程，
  线程内严格配对 `CoInitializeEx(COINIT_APARTMENTTHREADED)` / `CoUninitialize`
  （`RPC_E_CHANGED_MODE` 容错复用既有公寓），绝不复用 `spawn_blocking` 阻塞池线程，
  避免第三方残留 MTA 公寓污染 Tokio 异步运行时线程池；
- **无感拓扑守护**：专用原生消息泵线程监听 `WM_DISPLAYCHANGE`，以 **1500ms 可重置防抖**
  （同 ID `SetTimer` 重新武装，连续变化自动顺延）待系统重绘稳定后自动还原，
  绝不在高频显示事件风暴中重复执行；
- **`[icon_locker]` 配置节**：`enabled`（启停事实源）/ `auto_restore`（拓扑变化后自动还原，
  默认开启）/ `profiles`（按拓扑指纹自动维护的布局方案表）；
- **失败 Toast 提示**：自动还原失败时提示排查桌面右键「自动排列图标」。

### Fixed & Changed

- 布局方案生命周期以 `id` 唯一键管理（保存 / 还原 / 删除 / 活动方案回退），
  删除活动方案后自动还原回退到最新保存方案；
- 模块并发状态收敛于短临界 std Mutex + 原子标志，`ToolModule` 生命周期永不在锁内跨 `await`；
- 守护线程回调只做「发送一条空信号」的短促动作，真正的还原经独立 OS 线程执行，
  规避 COM 操作阻塞 UI / Tokio 工作线程。

## [0.5.0] - 2026-09-09

### Added

- **本地开发端口猎手模块（`port_hunter`，第六个模块卡片：即开即用工具）**：
  毫秒级定位本地监听端口（8080 / 3000 / 5173 等）的占用进程，支持搜索过滤与一键释放；
- **原生监听端口枚举**：基于 iphlpapi（`GetExtendedTcpTable` / `GetExtendedUdpTable`）
  按协议拆分枚举，仅提取 LISTEN / 绑定条目；
- **四重降噪纯函数过滤**：仅 LISTEN 状态 / 剔除 IANA 动态高位端口 / 会话隔离
  （剔除 Session 0 系统服务进程）/ 系统服务黑名单；
- **进程安全终止**：`OpenProcess(PROCESS_TERMINATE)` + `TerminateProcess`，
  释放前按 `confirm_before_kill` 二次确认；
- **UIPI 防御**：`ERROR_ACCESS_DENIED`（错误码 5）判定为 UIPI 拦截 / 完整性级别不足，
  Toast 提示「需要管理员权限，请通过顶部盾牌提权运行」；
- **双轨日志**：全局审计日志（`app_audit.log` 记录释放端口关键操作）+
  模块专属明细日志（`port_hunter.log`，`SCAN` / `SEARCH` / `KILL` 事件，
  UTC 微秒时间戳，流式追加）；
- **`[port_hunter]` 配置节**：`confirm_before_kill`（默认 `true`）/
  `show_system_ports`（默认 `false`）/ `log_dir`（缺省回退 exe 同级 `logs/port_hunter`）；
- **UI 绝对栅格落地**：卡片操作区重构为 156px 绝对坐标容器
  （状态区 76 + 间距 8 + 齿轮 24 + 间距 8 + 开关 40），接入 Slint 原生矢量图标。

### Fixed & Changed

- 彻底解决端口重复显示与卡片错位问题：状态文本 / 圆点坐标绝对钉死，
  文字长短变化不再推移相邻元素，X/Y 轴排版偏差归零；
- 端口猎手为即开即用工具（无常驻后台开关）：卡片以「即开即用」态呈现，
  `start` / `stop` 为幂等空操作、`is_running` 恒为 `false`，资源占用仅在弹窗打开扫描时发生；
- 扫描（同步 Win32 调用）经 `spawn_blocking` 移出 UI 线程，终止动作同样异步执行。

## [0.4.1] - 2026-09-08

### Added

- 置顶规则记忆恢复：模块启动时按持久化规则（进程名 + 标题子串 + 优先级）重新枚举窗口
  自动恢复上次会话的置顶。

### Fixed & Changed

- **置顶状态同步根治**：模块联锁修复，UI 开关与底层守护状态不再反转
  （切换后立即回读实际状态广播，界面永不展示虚假状态）；
- **「死尸复活」修复**：目标窗口销毁 / 关闭后不再残留置顶规则死尸，
  沿 Z-Order 链重排时正确跳过已失效窗口；
- **最小化自动解置顶**：监听 `EVENT_SYSTEM_MINIMIZESTART`，最小化窗口自动解除置顶，
  恢复时按需重新置顶（1~9 级优先级链重建）；
- 置顶优先级记忆修正：越界优先级由引擎归一化夹紧，规则持久化往返一致。

## [0.4.0] - 2026-09-08

### Added

- **全局窗口置顶守护模块（`topmost_manager`，第五个模块卡片）**：
  需要常驻的窗口一键钉在顶层，支持 **1~9 级置顶优先级**（1 级最顶层）；
- **专用泵线程守护**：`SetWinEventHook` 监听前台切换
  （`EVENT_SYSTEM_FOREGROUND` ~ `EVENT_SYSTEM_MINIMIZESTART`），
  15ms `SetTimer` 防抖后沿 Z-Order 链以 `SetWindowPos(SWP_NOACTIVATE)` 重排纠偏，
  绝不抢占焦点；
- **DWM 挂起窗口过滤**：`DwmGetWindowAttribute(DWMWA_CLOAKED)` 排除 DWM 挂起 /
  虚拟桌面隐藏的窗口；
- **`[topmost_manager]` 配置节**：`enabled`（默认关闭）/ `pinned_rules`
  （进程名 + 标题子串 + 优先级 1~9 + 启用标志）；
- 全套配置依赖与总线装配：置顶规则变更整体重建落盘，UI / 托盘切换即时持久化。

### Fixed & Changed

- 修复全局设置弹窗滚轮滑动失效；
- 修复「在文件资源管理器中打开日志目录」在部分环境下的回退缺陷。

## [0.3.2] - 2026-09-07

### Added

- **用户操作审计日志**：危险操作（释放端口、路径更改等）逐行追加
  `logs/app_audit.log`，UTC 微秒时间戳，与模块明细日志构成双轨留痕；
- **弹窗拦截截图留痕**：命中黑名单的弹窗经 `PrintWindow` 捕获窗口位图，
  以 GDI+（`GdipCreateBitmapFromHBITMAP` / `GdipSaveImageToFile`）编码为 PNG
  落盘 `logs/popup_screenshots/`（零第三方图像库依赖），并支持预览；
- **路径自定义**：`terminal_log_dir` / `app_log_dir` / `popup_screenshot_dir` /
  `terminal_log_retention_days`（默认 14 天）四项配置，
  全部按 `current_exe()` 基准绝对锚定（绝对路径原样使用，相对路径锚定 exe 同级）；
- **终端日志过期清理守护**：超过保留期限的 `.log` / `.state.log` 会话日志自动删除；
- **原生更新检查**：`WinHttp` 原生 API 请求 GitHub Releases `latest` 接口
  （连接 / 发送 / 接收各 8 秒超时），零额外网络库依赖；
- UI 布局对齐优化。

### Fixed & Changed

- 更新检查不引入 JSON 解析库：`tag_name` 以字符串定位提取，字段顺序不可假设时仍可靠命中；
- 审计日志 / 截图目录自动创建，失败仅降级告警、不阻断主功能。

## [0.3.1] - 2026-09-07

### Added

- **日志轮转守护**：按天滚动日志（`tracing-appender`），磁盘上最多保留
  [`MAX_LOG_FILES`] 份（15 份 ≈ 两周排查窗口），WorkerGuard 退出前保证刷盘；
- **Windows 写盘冲突防御**：配置落盘（临时文件写入 + 原子 `rename`）遭遇
  `ERROR_SHARING_VIOLATION`（错误码 32，杀毒 / 索引器瞬态占用）时退避 20ms
  重试至多 3 次后平滑降级，非共享冲突错误立即上抛；
- **剪贴板净化锁退避重试**：剪贴板被其他进程瞬态占用时退避重试，
  避免一次竞争即失败。

### Fixed & Changed

- 系统健壮性加固（配置 / 日志 / 剪贴板三条写入路径的并发与竞争处理）；
- 全仓库 Clippy 零告警规范落地。

## [0.3.0] - 2026-09-06

### Added

- **终端交互日志模块（`terminal_logger`）**：自动记录 CMD、PowerShell (5.1/7+)、
  Bash 终端的全部输入指令、交互会话与命令退出状态码（Exit Code）；
  - **PowerShell**：挂载静默转录流并代理 `prompt` 捕获 `$LASTEXITCODE`；
  - **Bash**：基于 `PROMPT_COMMAND` 与历史行解析记录用户指令与 `$?`；
  - **CMD**：注册表 AutoRun 挂载原生非侵入式脚本，`doskey` 捕获用户键入与退出码，
    内置 `/c` 护栏严禁挂起构建子进程；
- **Shell 配置文件文本锚点引擎**：对 Shell 配置文件采用
  `# >>> TLToolBox TAG >>>` 注释块封装，修改具备幂等性与原位更新能力，
  卸载时逐字节还原，绝不破坏用户原生配置与行尾格式（CRLF / LF）；
- **滚动文件日志**：Release 无控制台黑框仍可稳定记录排错日志，
  按天切分到 exe 同级 `logs/`（每日一个文件），路径以 `current_exe()` 基准绝对锚定；
- **Toast 操作气泡**：150ms 平滑淡入淡出的悬浮气泡，操作结果即时反馈；
- **UAC 提权重启**：点击顶部盾牌按钮 / 托盘菜单「以管理员身份重启」，
  经 `runas` 提权拉起自身并协同 `--restart-as-admin` 标记跳过单实例互斥；
- **物理内存极致压制**：主窗口隐藏进托盘时自动调用 Win32 `EmptyWorkingSet`，
  物理内存占用自 ~80 MB 骤降至 **~1.8 MB**；
- MIT License 全文。

### Fixed & Changed

- **CMD 动态命令行护栏**：AutoRun 捕获脚本利用 `findstr /I /C:"/c"` 严格探测
  `%CMDCMDLINE%`，遇到构建工具链（`cargo`、`git`、`npm`）派生的后台子进程瞬时跳过，
  彻底杜绝管道中继导致的终端假死与编译挂起；
- 静默 PowerShell 启动横幅，消除转录噪音；
- 精化 Bash 历史行解析（指令行 / 退出码对齐）；
- 适配 CI 跨区域 OEM 代码页测试（兼容 cp437 与 cp936）。

## [0.2.0] - 2026-09-04

### Added

- **架构转型**：从「大模型 Agent 工作站」裁撤为「专注桌面常驻与开关控制的
  轻量级原生 Windows 工具箱」——移除 LLM / HTTP 依赖（`reqwest`、`serde_json`
  随 agent 层一并退役），保留纯本地基础设施（Slint / Tokio / serde+toml /
  tracing / chrono）；
- **原生系统托盘**：`tray-icon`（`Shell_NotifyIcon`）+ `muda` 原生右键菜单
  （显示主窗口 / 以管理员身份重启 / 全部模块启停 / 退出程序）；
- **托盘图标资源加载**：从 exe 内嵌资源段读取 `RT_GROUP_ICON` / `RT_ICON` 图标帧，
  含多级降级链（资源缺失时回退系统默认图标）；
- **开机自启**：`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 读写
  （免管理员权限），登录时以 `--silent` 参数静默启动；
- **单实例守护**：会话级具名互斥（`CreateMutexW`）+ 自定义广播消息
  （`RegisterWindowMessageW` + `HWND_BROADCAST`）唤醒既有实例窗口，绝不产生双托盘图标；
- **系统防休眠模块（`keep_awake`）**：`SetThreadExecutionState(ES_CONTINUOUS |
  ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED)` 粘性执行状态，关闭时还原默认策略；
- **剪贴板纯文本净化模块（`clipboard_purifier`）**：`AddClipboardFormatListener`
  注册监听，专用纯消息窗口处理 `WM_CLIPBOARDUPDATE`，原子清空并重写纯文本，
  内置自循环回声防护（EchoGuard）；
- **弹窗拦截规则管理弹窗**：黑名单关键词查看 / 新增 / 删除，即时热重载生效；
- **配置路径绝对锚定**：`config/tltoolbox.toml` 与日志目录均以 `current_exe()`
  父目录为基准解析，根治注册表自启时（CWD 为 `C:\Windows\System32`）找不到文件 /
  无权写入的隐患；
- 完整生产 README、MIT License、GitHub Actions Windows 自动化发布流水线
  （`release.yml`：测试 + 编译 + 打包 zip + SHA256 校验和 + 创建 Release）。

### Fixed & Changed

- **ToolModule 契约重构**：`start(&mut self)` / `stop(&mut self)` 强依赖独占可变借用
  改为共享借用（`&self`），模块内部状态以内部可变性（`AtomicBool`、
  `tokio::sync::RwLock`）封装，消除全局锁长时间持有导致的 UI 掉帧 / 冻结；
- **Win32 消息循环缺陷根治**：`SetWinEventHook`（`WINEVENT_OUTOFCONTEXT`）回调与
  `WM_CLIPBOARDUPDATE` 强制要求宿主线程运行标准 Win32 消息泵，从 Tokio 异步协程
  整体隔离至专用操作系统原生线程（`GetMessageW` / `DispatchMessageW`），
  修复回调永不被触发的设计缺陷；
- **UI 状态单向数据流**：跨线程改写严格包裹于 `slint::invoke_from_event_loop` 闭包，
  状态经广播事件总线发布，界面永不展示虚假状态；
- 配置写盘采用同目录临时文件 + `rename` 原子替换，崩溃不产生半截损坏文件；
- 黑名单存为 `RwLock<Arc<RuleSet>>` COW 快照，热重载无需重启钩子线程。

## [0.1.0] - （未标记日期 · 依据《计划书.md》与 `Cargo.toml` 转型注释回溯）

> 该版本早于仓库首个提交，为「大模型 Agent 工作站」原型。

### Added

- Rust + Slint 声明式 UI + Tokio 异步运行时工程骨架；
- `ToolModule` Trait 动态多态模块契约（初始为独占可变借用 `&mut self`）；
- 桌面弹窗拦截模块初版（基于 `SetWinEventHook` / `EVENT_OBJECT_CREATE`，
  初始直接在 Tokio 协程内挂接——该实现存在消息循环失效缺陷，于 v0.2.0 修复）；
- 本地智能体架构原型：LLM 客户端（OpenAI 兼容规范，Ollama / llama.cpp 本地端点）、
  ReAct 推理循环、AgentTool 工具注册表（JSON Schema 参数自省）、
  事件总线（`tokio::sync::broadcast`）、TOML 配置引擎（含 `llm_api_base` /
  `llm_model`）；
- 双栏分屏 UI 规划（模块面板 + Agent 会话控制台）。

### Fixed & Changed

- 初始原型版本，无用户可见修复项；后续版本演进目标（专用 Win32 消息泵线程、
  共享借用契约、单向响应式数据流）记录于《计划书.md》分阶段路线图，
  并在 v0.2.0 架构转型中落地。

[Unreleased]: https://github.com/ltl0312/TLToolBox/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/ltl0312/TLToolBox/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/ltl0312/TLToolBox/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/ltl0312/TLToolBox/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/ltl0312/TLToolBox/compare/v0.3.2...v0.4.0
[0.3.2]: https://github.com/ltl0312/TLToolBox/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/ltl0312/TLToolBox/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/ltl0312/TLToolBox/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/ltl0312/TLToolBox/compare/v0.1.0...v0.2.0
