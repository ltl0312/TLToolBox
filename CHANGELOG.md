# 更新日志

本项目的全部重要变更均记录在此文件中。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
本项目遵循 [语义化版本](https://semver.org/lang/zh-CN/)（SemVer）。

> **版本史说明**：`v0.2.0` 起的条目依据 git 提交历史与 Release Tag 回溯；
> `v0.1.0` 早于仓库首个提交（首个提交即 v0.2.0 架构转型版），该条目依据
> 《计划书.md》与 `Cargo.toml` 中的架构转型注释回溯，日期不可考。

## [Unreleased]

（当前无未发布变更。）

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
