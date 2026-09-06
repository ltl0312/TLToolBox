# TLToolBox

原生 Windows 桌面实用工具箱 —— 轻量 · 纯粹 · 常驻：单文件、免安装、无网络依赖、随开机静默自启。

[![Release](https://img.shields.io/github/v/release/ltl0312/TLToolBox?style=flat-square&color=2563eb&label=Release)](https://github.com/ltl0312/TLToolBox/releases)
[![Rust](https://img.shields.io/badge/Rust-2021%20%7C%20MSVC-ea580c?style=flat-square&logo=rust)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/Platform-Windows%2010%2F11%20x64-0284c7?style=flat-square&logo=windows)](https://www.microsoft.com/windows)
[![License](https://img.shields.io/badge/License-MIT-16a34a?style=flat-square)](LICENSE)

TLToolBox 是一个使用 Rust 编写的原生 Windows 桌面实用工具箱：把「桌面弹窗拦截、系统防休眠、剪贴板纯文本净化、终端交互与状态日志」收进一个常驻系统托盘、约 11 MB 的单文件程序里。后台常驻物理内存低至约 **1.8 MB**，开关即用、低打扰、零后台负担。它不含 Electron 外壳、不含解释器运行时、不发起任何网络请求——全部能力均通过 Win32 系统原生 API 本地完成。

| 维度       | 说明                                                         |
| :--------- | :----------------------------------------------------------- |
| **定位**   | 轻量、纯粹的桌面常驻守护工具；Release 单文件原生可执行程序（实测磁盘体积约 11 MB，后台常驻内存约 1.8 MB） |
| **技术栈** | Rust 2021 · Slint 声明式 UI（当前锁定 1.17.1）· Tokio 异步事件驱动 · windows-rs 0.58 原生绑定 |
| **形态**   | 绿色便携：解压即用、无需安装、默认普通权限运行（支持一键 UAC 提权）；Release 为 Windows GUI 子系统，无控制台黑框 |
| **系统**   | Windows 10 / 11（x64）。应用清单声明兼容 Win7–Win11；完整 Per-Monitor V2 逐显示器高分屏体验需 Windows 10 1703+ |
| **隐私**   | 纯本地运行：无遥测、无账号、无后台服务；默认仅在开启开机自启时写入用户级注册表 Run 键（HKCU，免提权） |

---

## ✨ 功能特性

四个常驻守护模块共享同一套「卡片开关」交互：打开主窗口，拨动开关即启用，点击卡片上的 ⚙ 齿轮图标可进入对应模块的设置弹窗（弹窗拦截规则管理、终端日志存储管理等）。

| 模块                                         | 它做什么                                                     | 底层原理                                                     | 推荐开启场景                                                 |
| :------------------------------------------- | :----------------------------------------------------------- | :----------------------------------------------------------- | :----------------------------------------------------------- |
| **桌面弹窗拦截**<br>`popup_blocker`          | 系统级监听新窗口创建，命中黑名单关键词的广告 / 流氓弹窗在出现瞬间被毫秒级 `WM_CLOSE` 关闭 | 基于 `SetWinEventHook` 监听 `EVENT_OBJECT_CREATE`，Win32 消息泵运行在专用原生线程（`win32-popup-hook-pump`）；黑名单采用 COW 快照 + 热重载，增删关键词即时生效、无需重启钩子；支持一键提权重启突破 UIPI 限制 | **默认随应用启动**。对弹窗零容忍的常驻用户、流氓广告频发的机器；规则可在 ⚙ 弹窗中自由增删 |
| **系统防休眠**<br>`keep_awake`               | 阻止系统自动睡眠与屏幕空闲熄灭，让下载 / 渲染 / 编译 / 值守任务彻夜稳定运行 | 调用 `SetThreadExecutionState(ES_CONTINUOUS \| ES_SYSTEM_REQUIRED \| ES_DISPLAY_REQUIRED)` 注入粘性执行状态；纯内核状态标记，无常驻循环，CPU 开销绝对为零；关闭时以 `ES_CONTINUOUS` 还原默认策略 | **默认关闭**。长时间下载 / 视频渲染 / 外接投影演示 / 隔夜挂机任务时开启；日常保持关闭以尊重系统节能策略 |
| **剪贴板纯文本净化**<br>`clipboard_purifier` | 剪贴板内容同时携带纯文本与富文本（网页 HTML、Office RTF、聊天工具内嵌样式）时，自动剔除格式残留，粘贴始终为纯文本 | 基于 `AddClipboardFormatListener` 注册监听，专用原生纯消息窗口（STATIC + `HWND_MESSAGE`）处理 `WM_CLIPBOARDUPDATE`；原子清空并重写，内置自循环回声防护（EchoGuard）彻底避免自我触发 | **默认关闭**。常将网页 / 文档内容复制进 Markdown、代码编辑器、终端等纯文本环境的用户；需要富文本粘贴时关闭 |
| **终端交互日志**<br>`terminal_logger`        | 自动记录 CMD、PowerShell (5.1/7+)、Bash 终端的全部输入指令、交互会话与命令退出状态码（Exit Code） | **PowerShell**：挂载静默转录流并代理 `prompt` 捕获 `$LASTEXITCODE`；<br>**Bash**：基于 `PROMPT_COMMAND` 与历史行解析记录用户指令与 `$?`；<br>**CMD**：注册表 AutoRun 挂载原生非侵入式脚本，doskey 捕获用户键入与退出码，内置 `/c` 护栏严禁挂起构建子进程 | **默认关闭**。开发调试、命令行操作审计、运维排错及终端历史持久化留痕场景 |

> **默认值说明**：弹窗拦截属于“装上即用”的核心能力，默认随应用启动；防休眠、剪贴板净化与终端日志涉及系统电源策略、剪贴板行为与外部 Shell 挂接，属于操作敏感型功能，默认保持关闭，由用户显式开启。

---

## 🖥 桌面常驻与生命周期

TLToolBox 专注于低打扰、低开销的常驻守护：

* **关闭即收进托盘**：点击窗口右上角 × 默认隐藏至系统托盘（`minimize_to_tray = true`），主事件循环由 `slint::run_event_loop_until_quit` 驱动，后台平稳守护；双击托盘图标随时呼回主窗口。
* **物理内存极致压制**：主窗口隐藏进托盘或空闲时，自动调用 Win32 `EmptyWorkingSet`，将闲置的 GPU/DirectX 驱动缓存与字体内存页移出工作集，**物理内存占用自 ~80 MB 骤降至 1.8 MB**。
* **管理员提权感知与突破 (UIPI)**：普通权限进程受 Windows UIPI 限制无法关闭高特权流氓弹窗。工具箱启动时自动探测权限状态：未提权时标题栏显示琥珀色提权按钮、托盘提供「以管理员身份重启」；点击后通过 `runas` 提权拉起自身并协同 `--restart-as-admin` 标记跳过单实例互斥，已提权时显示翡翠色「管理员 (Admin)」徽标。
* **操作气泡即时反馈 (Toast)**：规则增删、自启切换、提权变迁均具备 150ms 平滑淡入淡出悬浮气泡，操作结果一目了然。
* **单实例防多开**：基于会话级具名互斥（`CreateMutexW`），重复双击 exe 或自启拉起时，第二实例自动向系统广播自定义唤醒消息（`RegisterWindowMessageW`）将既有窗口前置，自身立即退出，绝不产生双托盘图标。
* **开机静默自启**：开启主窗口顶部开关，自动写入 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`（免提权）。系统登录时自动附加 `--silent` 参数启动：窗口静默隐藏、托盘常驻。

---

## 🏗 架构设计亮点

### 1. Win32 原生线程与消息泵隔离（模块第一性原则）
Windows 的系统级事件通知——`SetWinEventHook`（`WINEVENT_OUTOFCONTEXT`）回调与 `WM_CLIPBOARDUPDATE`——强制要求安装方线程自身的消息队列运行标准 Win32 消息泵（`GetMessageW` / `DispatchMessageW`）。Tokio 异步工作线程是无栈协程调度载体、不运行此类消息循环。TLToolBox 将系统钩子与消息泵整体隔离在专用操作系统原生线程中：
* **启动握手竞态防护**：泵线程先以 `PeekMessageW(PM_NOREMOVE)` 强制建立系统消息队列，再回报线程 ID，从根上消除停机信号早于队列建立而投递失败的竞态；
* **平滑卸载协议**：`stop()` 依次执行 `CancellationToken` 广播停机意图 -> `PostThreadMessageW(WM_QUIT)` 唤醒阻塞的泵 -> 5 秒超时保护平滑收尾，`UnhookWinEvent` / `RemoveClipboardFormatListener` 保证在安装线程安全释放；
* **静态回调路由**：WinEvent 回调是 `unsafe extern "system"` 静态函数，模块通过进程级无锁/分段锁注册表将回调无损路由至所属实例；
* **COW 规则快照**：黑名单存为 `RwLock<Arc<RuleSet>>`，写操作整包原子替换，钩子回调仅克隆引用在锁外执行子串比对，热重载无需重启线程。

### 2. 终端无损插拔与死锁防护
* **通用文本锚点引擎**：对 Shell 配置文件采用 `# >>> TLToolBox TAG >>>` 注释块封装，修改具备幂等性与原位更新能力，卸载时逐字节还原，绝不破坏用户原生配置与行尾格式（CRLF / LF）；
* **CMD 动态命令行护栏**：注册表 AutoRun 挂载的捕获脚本利用 `findstr /I /C:"/c"` 严格探测 `%CMDCMDLINE%`，遇到构建工具链（`cargo`、`git`、`npm`）派生的后台子进程瞬时跳过，彻底杜绝管道中继导致的终端假死与编译挂起。

### 3. Slint 单向响应式数据流（界面即事实）
* 跨线程改写严格包裹于 `slint::invoke_from_event_loop` 闭包内（即 UI 线程消息循环中）；
* 状态调度基于 `tokio::sync::broadcast` 无锁总线，发布为非阻塞同步调用，慢消费者不会阻塞发布方，UI 与托盘均只消费调度层确认的最终事实，界面永不展示虚假状态。

### 4. 生产级日志与配置路径绝对锚定
* **路径绝对锚定**：配置文件 `config/tltoolbox.toml` 与日志目录 `logs/` 均以 `current_exe()` 父目录为基准绝对解析，彻底根治注册表开机自启时（CWD 为 `C:\Windows\System32`）找不到文件或无权写入的隐患；
* **原子写盘机制**：配置持久化采用同目录临时文件 + rename 原子替换，系统崩溃不产生半截损坏文件；
* **滚动日志落盘**：引入 `tracing-appender` 按天切分本地日志（保留 15 份），Release 模式下无控制台黑框仍可稳定记录排错日志。

## 🚀 快速上手

### 绿色便携版（解压即用）
1. 从 Releases 下载 `tltoolbox-windows-x86_64.zip`，并核对随附的 `.sha256` 校验和；
2. 解压到任意目录（例如 `D:\Tools\TLToolBox`），包内包含 `tltoolbox.exe` 与 `config\tltoolbox.toml`；
3. 双击 `tltoolbox.exe` 启动；
4. 在主窗口拨动各模块卡片开关，或点击卡片上的 ⚙ 图标进入高级配置。

> **绿色便携边界**：所有配置与日志均保存在 exe 同级目录，拷贝整个文件夹即可完成数据迁移。程序未作付费商业数字签名，SmartScreen / 杀软首次运行可能弹出未知发布者提示，可自行编译源码核对。

### 开机自启
打开主窗口顶部「开机自启」开关即可。程序自动写入 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`（免管理员权限），登录时以 `--silent` 参数启动并静默常驻托盘；关闭开关即可完全清理该注册表键值。

### 快捷键与常用操作
| 操作                 | 效果                                                         |
| :------------------- | :----------------------------------------------------------- |
| **双击托盘图标**     | 呼出 / 前置主窗口（静默常驻时的唤醒入口）                    |
| **右键托盘图标**     | 弹出原生菜单：显示主窗口 / 以管理员身份重启 / 全部模块启停 / 退出程序 |
| **主窗口右上角 ×**   | 隐藏到托盘继续常驻，触发内存工作集回收（工作集降至 ~1.8 MB） |
| **再次运行 exe**     | 单实例守卫生效：既有实例窗口唤出至前台，当前新进程立即退出   |
| **点击顶部盾牌按钮** | 发起 UAC 提权重启，用于突破 Windows UIPI 拦截高特权安装包广告 |

### 弹窗拦截 · 黑名单规则管理
点击「桌面弹窗拦截」卡片右侧的 ⚙ 齿轮，可查看 / 新增 / 删除拦截关键词：
* **匹配语义**：窗口标题或类名包含该关键词即拦截（子串匹配、忽略大小写），中文关键词逐字生效；
* **即时生效**：规则增删即时热重载生效，并自动持久化到 `config/tltoolbox.toml` 的 `popup_blacklist`；
* **默认规则**：内置 `广告`、`Flash Helper Service`、`Update Notice`、`推广弹窗` 等默认项，重复输入自动去重。

### 终端日志 · 存储管理
点击「终端交互日志」卡片右侧的 ⚙ 齿轮：
* 查看当前日志绝对路径（默认 `exe 同级/logs/terminals/`）；
* 点击「在文件资源管理器中打开日志目录」快速调取各终端生成的会话 log 文件。

---

## ⚙️ 命令行参数

| 参数                 | 说明                                                         |
| :------------------- | :----------------------------------------------------------- |
| `--silent`           | 静默启动：主窗口保持隐藏、仅托盘常驻。开机自启项会自动附带本参数 |
| `--restart-as-admin` | 提权重启握手标记：由普通权限实例拉起提权新实例时内部传递，用于跳过单实例互斥阻断 |

---

## 🔧 从源码构建

### 环境准备
* **目标系统**：Windows 10 / 11 x64
* **Rust 工具链**：Rust stable MSVC（`rustup default stable-x86_64-pc-windows-msvc`）
* **编译组件**：Visual Studio Build Tools（勾选“使用 C++ 的桌面开发”，包含 MSVC 编译器、`link.exe` 以及内嵌资源编译所需的 `rc.exe`）

### 构建与测试
```powershell
# 1. 构建 Release 单文件（体积极致压制：LTO + Strip）
cargo build --release
# 产物：target\release\tltoolbox.exe（约 11 MB，Windows GUI 子系统，无控制台黑框）

# 2. 运行全量脱机测试（140+ 单元测试与集成测试，不依赖图形环境）
cargo test --all-targets

# 3. 运行含真实注册表往返与控制台自清理的实机测试
cargo test --all-targets -- --ignored

# 4. 调试模式运行（保留控制台黑框，实时输出 tracing debug 日志）
cargo run
```

### 项目结构

```text
TLToolBox/
├── Cargo.toml               # 依赖声明 + Release 优化配置 (opt-level="z", lto, strip)
├── build.rs                 # Slint UI 编译 + Win32 资源嵌入 (manifest / ico)
├── app.manifest             # Per-Monitor V2 DPI 感知 / asInvoker 清单声明
├── res/app.ico              # 多尺寸内嵌应用图标 (16 / 32 / 48 / 256)
├── ui/
│   ├── app.slint            # Slint 声明式主界面 (深色主题 / Toast / 设置弹窗)
│   └── icons/               # 界面内嵌 SVG 矢量图标 (盾牌、齿轮、关闭等)
├── src/
│   ├── main.rs              # 程序入口：单实例守卫 / 托盘 / 内存优化 / UI 调度
│   ├── lib.rs               # 核心库入口 (供单元与集成测试引用)
│   ├── logging.rs           # 本地滚动日志系统 (tracing-appender 每日切分)
│   ├── platform.rs          # 平台级调用 (EmptyWorkingSet 内存压制 / UAC runas 提权重启)
│   ├── bus.rs               # 广播事件总线 (tokio::sync::broadcast)
│   ├── manager.rs           # 模块调度生命周期控制器
│   ├── config.rs            # TOML 配置引擎 (路径绝对锚定 + 原子覆写)
│   ├── autostart.rs         # 注册表开机自启管理 (HKCU Run 键)
│   ├── single_instance.rs   # 具名互斥锁 + 跨进程唤醒广播
│   ├── tray.rs              # 原生托盘线程、图标多级降级与 Win32 菜单
│   └── modules/
│       ├── mod.rs           # 模块统一契约 (ToolModule)
│       ├── popup_blocker.rs # 桌面弹窗拦截模块
│       ├── keep_awake.rs    # 系统防休眠模块
│       ├── clipboard_purifier.rs # 剪贴板格式净化模块
│       └── terminal_logger/ # 终端交互日志记录子系统
│           ├── mod.rs       # 终端调度器与 HookManager
│           ├── anchor.rs    # Shell 配置文件文本锚点注入引擎
│           ├── ps_bash.rs   # PowerShell 与 Bash 挂载实现
│           └── cmd.rs       # CMD AutoRun 批处理与 doskey 状态捕获
├── tests/                   # 模块生命周期无头集成测试
└── .github/workflows/       # GitHub Actions Windows 自动化发版流水线 (release.yml)
```

---

## 📖 常见问题

**Q: 需要管理员权限吗？**

A: 默认不需要。程序默认以普通用户权限（`asInvoker`）运行，自启仅写入当前用户注册表，启动全程不弹出 UAC。但针对以管理员身份运行的流氓安装包或更新器弹窗，受 Windows UIPI 安全隔离限制，普通权限程序无法向其投递关闭消息。遇到此类弹窗时，只需点击窗口顶部盾牌按钮或托盘菜单中的「以管理员身份重启」，即可无缝提权以获得 100% 拦截能力。

**Q: 软件会发起网络请求吗？**

A: 绝对不会。本项目不包含任何网络请求或遥测上报代码，无外部域名解析，可在完全离线的保密开发机安心常驻。

**Q: 配置文件与日志存放在哪里？**

- 配置文件：`exe 同级目录\config\tltoolbox.toml`（首次启动自动创建）；
- 系统运行日志：`exe 同级目录\logs\tltoolbox.YYYY-MM-DD.log`（保留最近 15 天）；
- 终端交互日志：`exe 同级目录\logs\terminals\<终端类型>\`（可在设置弹窗中一键在资源管理器中打开）。

**Q: 点击窗口右上角的 × 为什么不退出？**

A: TLToolBox 采用常驻守护设计。点击 × 会将窗口隐藏进托盘，并自动调用 `EmptyWorkingSet` 释放物理内存至 1.8 MB。若需彻底退出，请右键托盘图标点击「退出程序」。

**Q: 如何彻底卸载？**

A: 先在主界面关闭「开机自启」以及各功能开关，然后直接删除整个程序目录即可——系统内无任何其他常驻服务或残留文件。

---

## 📄 开源许可

本项目以 **MIT License** 开源。你可以自由使用、修改、分发与商用，但请保留版权与许可声明（LICENSE 全文随版本发布提供）。

> 免责声明：弹窗拦截基于「窗口标题 / 类名关键词」匹配，属尽力而为的启发式拦截，可能误伤命名相似的窗口；请通过 ⚙ 规则管理随时校准黑名单。使用本项目即表示你了解并接受上述行为特征。
