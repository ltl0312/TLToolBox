# TLToolBox

> 原生 Windows 桌面实用工具箱 —— **轻量 · 纯粹 · 常驻**：单文件、免安装、无网络依赖、随开机静默自启。

[![Release v0.2.0](https://img.shields.io/badge/Release-v0.2.0-2d74e8?style=flat-square)](https://github.com/ltl0312/TLToolBox/releases)
[![Rust](https://img.shields.io/badge/Rust-2021%20%7C%20MSVC-f75208?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/Platform-Windows-0078d6?style=flat-square&logo=windows&logoColor=white)](https://www.microsoft.com/windows)
[![License](https://img.shields.io/badge/License-MIT-10b981?style=flat-square)](https://github.com/ltl0312/TLToolBox)

**TLToolBox** 是一个使用 Rust 编写的原生 Windows 桌面实用工具箱：把「桌面弹窗拦截、系统防休眠、剪贴板纯文本净化」这几件高频琐事收进一个**常驻系统托盘、约 11 MB 的单文件程序**里，开关即用、低打扰、零后台负担。它不含 Electron 外壳、不含解释器运行时、不发起任何网络请求——全部能力都通过 Win32 系统 API 在本地完成。

| 维度 | 说明 |
| --- | --- |
| **定位** | 轻量、纯粹的桌面常驻工具；Release 单文件原生可执行程序（实测约 11 MB） |
| **技术栈** | Rust 2021 · Slint 声明式 UI（1.9 语言规范，当前锁定 1.17.1）· Tokio 异步事件驱动 · windows-rs 0.58 原生绑定 |
| **形态** | 绿色便携：解压即用、无需安装、无管理员权限要求；Release 为 Windows GUI 子系统，无控制台黑框 |
| **系统** | Windows 10 / 11（x64）。应用清单声明兼容 Win7–Win11；完整 Per-Monitor V2 逐显示器高分屏体验需 Windows 10 1703+ |
| **隐私** | 纯本地运行：无遥测、无账号、无后台服务；唯一系统写入为「开机自启」的用户级注册表 Run 键（HKCU，免提权） |

---

## ✨ 功能特性

三个常驻守护模块共享同一套「卡片开关」交互：打开主窗口，拨动开关即启用，点卡片上的 ⚙ 可进入模块级设置（当前弹窗拦截支持黑名单规则管理）。

| 模块 | 它做什么 | 底层原理 | 推荐开启场景 |
| --- | --- | --- | --- |
| **桌面弹窗拦截**<br>`popup_blocker` | 系统级监听新窗口创建，命中黑名单关键词的广告 / 流氓弹窗在出现瞬间被毫秒级 `WM_CLOSE` 关闭 | 基于 `SetWinEventHook` 监听 `EVENT_OBJECT_CREATE`，钩子与 Win32 消息泵运行在**专用原生线程**（`win32-popup-hook-pump`）上；黑名单为 **COW 快照 + 热重载**，增删关键词即时生效、无需重启钩子；匹配语义 = 窗口标题 / 类名**子串匹配、忽略大小写** | 默认随应用启动。对弹窗零容忍的常驻用户、广告软件频发的公共 / 家用机器；规则可在 ⚙ 弹窗中自由增删 |
| **系统防休眠**<br>`keep_awake` | 阻止系统自动睡眠与屏幕空闲熄灭，让下载 / 渲染 / 值守任务彻夜运行 | 调用 `SetThreadExecutionState(ES_CONTINUOUS \| ES_SYSTEM_REQUIRED \| ES_DISPLAY_REQUIRED)` 注入**粘性执行状态**；这是一次微秒级内核调用，**无任何常驻线程，运行时开销为零**；关闭时以 `ES_CONTINUOUS` 还原系统默认电源策略，进程退出由内核自动回收 | 默认关闭（显式开启才合理）。长时间下载 / 视频渲染 / 外接演示 / 隔夜任务时开启；日常保持关闭以尊重系统电源策略 |
| **剪贴板纯文本净化**<br>`clipboard_purifier` | 剪贴板内容同时携带纯文本与富文本（网页 HTML、Office RTF、聊天工具内嵌样式）时，自动清空并**仅写回纯文本**，粘贴不再带格式残留 | 基于 `AddClipboardFormatListener` 注册监听，`WM_CLIPBOARDUPDATE` 由**专用原生线程**的纯消息窗口（`STATIC` + `HWND_MESSAGE`）消息泵接收；净化在**单次持有剪贴板锁**内完成（先分配后清空，失败绝不损伤原数据），并内置**自循环回声防护**（`EchoGuard`），写回不会自我触发 | 默认关闭。常把网页 / Office 内容复制进 Markdown、代码、邮件等纯文本场景的用户；需要保留格式粘贴时请保持关闭 |

> **默认值说明**：弹窗拦截是「装上就想用」的能力，默认随应用启动；防休眠与剪贴板净化分别改变电源策略与剪贴板行为，属于用户预期敏感的操作，故默认关闭，由你在 UI 中显式开启。

### 桌面常驻与生命周期

TLToolBox 定位是**常驻**而非「用完即走」：

- **关闭即收进托盘**：点击窗口右上角 × 默认隐藏到系统托盘（`minimize_to_tray = true`），进程继续在后台守护，需要时双击托盘图标呼出；
- **托盘右键菜单**：`显示主窗口` / `全部模块：开启·关闭`（文案随实际聚合状态自动切换）/ `退出程序`——只有从菜单退出才是真正结束进程；
- **单实例防多开**：基于会话级具名互斥（`CreateMutexW`），再次双击 exe 或系统重复自启时，第二实例会自动把已常驻实例的主窗口唤到前台后自行退出，绝不产生双托盘图标、双钩子；
- **开机静默自启**：打开主窗口顶部「开机自启」开关即可。自启项写入
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`（值名 `TLToolBox`，值形如 `"D:\Tools\TLToolBox\tltoolbox.exe" --silent`）；登录时以 `--silent` 参数拉起，**主窗口保持隐藏、仅托盘常驻**，双击托盘图标即可唤出。

---

## 🏗 架构设计亮点

### 1. Win32 原生线程与消息泵隔离（模块第一性原则）

Windows 的系统级事件通知——`SetWinEventHook`（`WINEVENT_OUTOFCONTEXT`）回调与 `WM_CLIPBOARDUPDATE`——都会被系统投递到**安装方线程自己的消息队列**，必须由该线程运行标准 Win32 消息泵（`GetMessageW` / `DispatchMessageW`）才能派发。Tokio 工作线程是无栈协程调度载体、并不运行 Win32 消息循环，因此在 `tokio::spawn` 中挂接这些钩子**永远收不到回调**。

TLToolBox 因此将「钩子安装 / 监听窗口创建 + 消息泵」整体隔离到 `std::thread` 派生的**专用操作系统原生线程**（`win32-popup-hook-pump`、`win32-clipboard-purifier-pump`），Tokio 侧只做生命周期编排：

- **启动握手**：泵线程先以 `PeekMessageW(PM_NOREMOVE)` 强制建立消息队列，再回报线程 ID——从根上消除「`WM_QUIT` 早于队列建立而投递失败」的竞态；
- **平滑卸载协议**：`stop()` 依次执行 ① `CancellationToken` 广播停机意图 → ② `PostThreadMessageW(WM_QUIT)` 定向唤醒阻塞中的泵 → ③ Join 线程并施加 5 秒超时；`UnhookWinEvent` / `RemoveClipboardFormatListener` + `DestroyWindow` 保证在**安装线程上**执行完毕，杜绝系统级钩子与监听窗口泄漏；实例消亡还有 `Drop` 兜底唤醒；
- **静态回调路由**：WinEvent 回调是 `unsafe extern "system"` 静态函数、拿不到 `&self`，模块通过一张进程级「钩子句柄 → 规则存储」注册表（`OnceLock<RwLock<HashMap>>`）把回调路由回所属实例；
- **COW 规则热重载**：黑名单存为 `RwLock<Arc<RuleSet>>`，`update_rules` 以写锁整体替换不可变快照（写入时一次性完成归一化 + 去重）；回调只在读锁内做一次 `Arc` 克隆，全部字符串匹配在**锁外**完成——规则热更新无需重启消息泵线程，回调内也绝不持有重锁。

### 2. Slint 单向响应式数据流（界面即事实）

Slint 的 `VecModel` / `ModelRc` **非 `Send`**，自创建后终生驻留 UI 主线程。TLToolBox 的跨线程桥接严格遵循这一约束：

- 跨线程载荷只有 `Weak<MainWindow>`（Slint 官方保证 `Send`）与模块调度器 `Arc` 等 `Send` 数据；**模型改写一律发生在 `slint::invoke_from_event_loop` 闭包内**（即 UI 线程消息循环中）；
- 自动启动模块在 UI 装配**之前**执行，初始模块列表是调度层的**真实状态快照**而非请求意图；每次开关落定后，调度器广播真实状态，UI 重拉调度层事实并整体重建列表（模型 reset 语义）——**启动失败、权限不足等异常会自动把开关回滚**，界面永不显示虚假的「运行中」。

### 3. 无锁广播总线与并发安全

- 模块调度器持有 `tokio::sync::broadcast`（容量 256）事件总线：`publish` 为**同步、非阻塞**调用，慢消费者只会收到 `Lagged` 淘汰提示、绝不会拖慢发布方；
- 总线事件**只在状态落定之后**广播（成败皆广播），UI 与托盘菜单看到的永远是模块的最终事实；
- 调度器自身**零全局锁**、无锁跨 `await`；同一模块的并发启停由模块内部的生命周期锁串行收敛（`AtomicBool` + 异步锁 + `CancellationToken`），不同模块的启停天然并行；`ToolModule` 契约收敛为共享借用（`&self`）+ `Send + Sync` + 幂等 `start` / `stop`；
- 托盘线程是托盘资源的唯一拥有者（`tray-icon` 0.19 + `muda` 0.15 为 Rc 句柄、且回调有线程亲和约束），与主线程之间只有**单向、无应答、无锁**的指令 / 事件投递，不存在任何死锁环。

### 4. 配置与状态收敛

- 配置为 TOML，路径**锚定到可执行文件同级目录**的 `config/tltoolbox.toml`（与 CWD 解耦——注册表自启时 CWD 是 `System32`，相对路径会写错位置甚至无权限）；
- 持久化 = 同目录临时文件 + `rename` 原子替换 + 写锁串行化，崩溃不产生半截文件；字段级向前兼容，旧配置缺新字段自动补默认值；解析失败**绝不静默覆盖**用户文件；
- 自启状态以配置为唯一事实源：每次启动都会把注册表 Run 键收敛到配置意图（路径漂移自动重写、残留自动清理、失败仅告警不阻断启动）；UI 开关则「先写注册表 → 再持久化配置 → 回读注册表真实状态驱动开关」。

### 5. Windows 原生适配细节

- **应用清单**：Per-Monitor V2 逐显示器 DPI 感知（自动回退 PerMonitor v1 / 旧式 `dpiAware`），高分屏文本不模糊；`asInvoker` 执行级别——不弹 UAC，与「托盘静默常驻 + 开机自启」定位一致；Common-Controls v6 视觉样式；`supportedOS` 显式声明 Win7–Win11；
- **内嵌多尺寸图标**：`res/app.ico`（16 / 32 / 48 / 256 四帧，由 `scripts/generate-app-icon.ps1` 生成）经 `build.rs` 以 `winresource` 编译进 exe；托盘运行期从 exe 资源段读取并解码 32×32 帧，资源异常时降级为纯代码绘制的备用图标——任何构建形态下托盘都有图标；
- **体积压制**：Release 配置 `opt-level = "z"` + `lto = true` + `codegen-units = 1` + `strip = true`，产出约 11 MB 的 GUI 子系统单文件；刻意保留默认 unwind（不设 `panic = "abort"`），保证 `tokio::spawn` 能把后台任务异常隔离在 `JoinHandle` 内，符合「错误隔离、优雅降级」的常驻定位。

---

## 🚀 快速上手

### 绿色便携版（解压即用）

1. 从 [Releases](https://github.com/ltl0312/TLToolBox/releases) 下载 `tltoolbox-windows-x86_64.zip`，并核对随附的 `.sha256` 校验和；
2. 解压到**任意目录**（例如 `D:\Tools\TLToolBox`），包内结构为 `tltoolbox.exe` 与 `config\tltoolbox.toml`；
3. 双击 `tltoolbox.exe` 启动——首次运行会在 exe 旁自动生成 `config` 目录并落盘默认配置；
4. 在主窗口拨动各模块卡片开关，或点击弹窗拦截卡片上的 ⚙ 管理黑名单规则。

> **绿色便携的边界**：配置始终写在 exe 同级目录，整个目录拷走即完成迁移；程序被移动 / 升级后，已开启的开机自启会在下次启动时自动校正到新路径。程序未做数字签名，SmartScreen / 杀软首次运行可能弹出未知发布者提示，请自行决定是否信任（本项目开源，可自行编译核对）。

### 开机自启

打开主窗口 → 勾选顶部右侧「开机自启」开关。

- 开启后写入 `HKCU\...\CurrentVersion\Run`（免管理员权限），下次登录时程序将以 `--silent` 静默启动：**不弹主窗口，仅托盘常驻**；
- 关闭该开关即删除注册表项；绿色版卸载 = 关闭自启 + 删除整个目录，系统内不残留其他写入。

### 快捷键与常用操作

| 操作 | 效果 |
| --- | --- |
| **双击托盘图标** | 呼出 / 前置主窗口（静默常驻时的唯一唤醒入口） |
| **右键托盘图标** | 弹出原生菜单：`显示主窗口` → `全部模块：开启 / 关闭` → `退出程序` |
| 左键单击托盘图标 | 无动作（双击语义保留给显示窗口，避免误触） |
| 主窗口右上角 × | 最小化到托盘（默认），进程继续常驻；在配置中把 `minimize_to_tray` 改为 `false` 则直接退出 |
| 再次运行 `tltoolbox.exe` | 不产生第二实例：既有实例的主窗口被唤到前台，本次进程立即退出 |
| 开机自启登录后 | 无窗口静默常驻，需要时双击托盘图标唤出 |

### 弹窗拦截 · 黑名单规则管理

点击「桌面弹窗拦截」卡片右侧的 ⚙ 齿轮，可查看 / 新增 / 删除拦截关键词：

- 匹配语义：**窗口标题或类名包含该关键词即拦截**（子串匹配、忽略大小写），中文关键词逐字生效；
- 规则增删**即时生效**（COW 快照热重载，无需重启应用），并自动持久化到 `config/tltoolbox.toml` 的 `popup_blacklist`；
- 内置默认关键词：`广告`、`Flash Helper Service`、`Update Notice`、`推广弹窗`；重复 / 纯空白输入会被自动归一化合并，可放心操作。

---

## ⚙️ 命令行参数

| 参数 | 说明 |
| --- | --- |
| `--silent` | 静默启动：主窗口保持隐藏、仅托盘常驻。开机自启项会自动附带本参数；手动运行同样适用（适用于「登录即常驻、不打扰」的用法） |

---

## 🔧 从源码构建

### 环境准备

| 依赖 | 说明 |
| --- | --- |
| Windows 10 / 11 x64 | 开发与运行目标平台 |
| [Rust stable（MSVC 工具链）](https://www.rust-lang.org/tools/install) | 安装 rustup 后确认默认宿主为 `x86_64-pc-windows-msvc`：`rustup default stable-x86_64-pc-windows-msvc` |
| [Visual Studio Build Tools](https://aka.ms/vs) | 勾选「使用 C++ 的桌面开发」工作负载（含 Windows SDK）——链接阶段的 `link.exe` 与 `build.rs` 内嵌清单 / 图标所需的 `rc.exe` 依赖它 |

### 构建与测试

```powershell
# 1. 构建 Release 单文件（体积优化：LTO + strip，耗时较长属正常）
cargo build --release

# 产物：target\release\tltoolbox.exe（约 11 MB，GUI 子系统、无控制台黑框）

# 2. 运行全部测试（单元 + 集成，全程不触达图形会话，可离线执行）
cargo test --all-targets

# 3. 调试运行（debug 保留控制台子系统，便于观察 tracing 日志）
cargo run
```

补充说明：

- **Windows 资源嵌入在构建期自动完成**：`build.rs` 经 `winresource` 把 `app.manifest`（DPI / UAC / Common-Controls）与 `res/app.ico` 编译进 exe，无需手工步骤；
- `scripts/` 提供图标生成与产物验证脚本：`generate-app-icon.ps1`（重新生成多尺寸 ICO）、`verify-manifest.ps1` / `verify-icon.ps1` / `verify-release-workflow.py`（发布前校验）；
- **发布流水线**：推送 `v0.2.0` 形式的 Tag 即触发 `.github/workflows/release.yml`（Windows 最新镜像 + MSVC），依次执行测试 → Release 编译 → 打包 `tltoolbox-windows-x86_64.zip`（exe + 默认配置）→ 生成 SHA256 → 创建 GitHub Release；
- 涉及真实注册表读写的往返测试默认 `#[ignore]`，需显式执行（会短暂读写当前用户 Run 键，测试自清理）：`cargo test -- --ignored`。

### 项目结构

```text
TLToolBox/
├── Cargo.toml              # 依赖声明 + Release 体积优化配置
├── build.rs                # Slint UI 编译 + Win32 资源嵌入（manifest / ico）
├── app.manifest            # Per-Monitor V2 DPI / asInvoker / Common-Controls v6
├── res/app.ico             # 多尺寸应用图标（16 / 32 / 48 / 256）
├── ui/
│   ├── app.slint           # Slint 声明式界面（单栏工具箱 · 深色主题）
│   └── icons/              # 界面内联 SVG 图标
├── src/
│   ├── main.rs             # 装配点：单实例 / 总线 / 托盘生命周期 / UI 桥
│   ├── lib.rs              # 库入口（可脱离 GUI 测试）
│   ├── bus.rs              # 广播事件总线（tokio::sync::broadcast）
│   ├── manager.rs          # 模块调度管理器
│   ├── config.rs           # TOML 配置引擎（原子写盘）
│   ├── autostart.rs        # 注册表开机自启（HKCU Run 键）
│   ├── single_instance.rs  # 具名互斥 + 唤醒广播
│   ├── tray.rs             # 系统托盘线程与右键菜单
│   └── modules/
│       ├── popup_blocker.rs      # 桌面弹窗拦截
│       ├── keep_awake.rs         # 系统防休眠
│       └── clipboard_purifier.rs # 剪贴板纯文本净化
├── tests/                  # 无图形会话的集成测试
├── scripts/                # 图标生成 / 产物验证脚本
└── .github/workflows/      # Windows 自动发布流水线（release.yml）
```

---

## 📖 常见问题

**需要管理员权限吗？**
不需要。程序以 `asInvoker` 执行级别与启动方同权限运行，自启写入用户级注册表，全程不触发 UAC。对应的已知边界：对**以管理员权限运行**的进程窗口，普通权限进程受 UIPI 保护无法向其投递关闭消息，此类高权限弹窗不会被拦截——这是 Windows 安全模型的刻意设计，暂不以整体提权换取拦截能力。

**程序会联网吗？**
不会。项目已移除全部 HTTP / LLM 依赖，无任何网络请求与遥测上报，可在完全离线的机器上运行。

**配置存在哪里？**
exe 同级目录 `config\tltoolbox.toml`（首次运行自动生成），字段可直接手改，改后重启应用生效。

**点 × 之后程序去哪了？**
默认收进系统托盘继续常驻（托盘图标在通知区，可能需要点 ↑ 展开）。双击托盘图标唤回窗口，右键托盘图标选择「退出程序」才真正结束。

**如何卸载？**
先在主窗口关闭「开机自启」，再删除整个程序目录即可——系统内无其他残留。

---

## 📄 开源许可

本项目以 **MIT License** 开源。你可以自由使用、修改、分发与商用，但请保留版权与许可声明（LICENSE 全文随版本发布提供）。

> 免责声明：弹窗拦截基于「窗口标题 / 类名关键词」匹配，属尽力而为的启发式拦截，可能误伤命名相似的窗口；请通过 ⚙ 规则管理随时校准黑名单。使用本项目即表示你了解并接受上述行为特征。
