# TLToolBox 贡献指南 (Contributing Guide)

欢迎为 TLToolBox 贡献力量!本项目是使用 **Rust + Slint 编写的原生 Windows 桌面
实用工具箱**,遵循「轻量 · 纯粹 · 常驻」的定位:单文件、免安装、无网络依赖、
低打扰。提交代码前请阅读本指南,确保变更与项目定位一致、通过全部质量门禁。

---

## 目录

1. [项目定位与设计铁律](#1-项目定位与设计铁律)
2. [环境与构建要求](#2-环境与构建要求)
3. [质量门禁(提交前必须全绿)](#3-质量门禁提交前必须全绿)
4. [代码风格与模块契约](#4-代码风格与模块契约)
5. [UI 156px 绝对栅格规范](#5-ui-156px-绝对栅格规范)
6. [提交信息规范](#6-提交信息规范)
7. [PR 提交流程](#7-pr-提交流程)
8. [发布流程(维护者)](#8-发布流程维护者)

---

## 1. 项目定位与设计铁律

在提出新功能前,先对照以下原则(违反者大概率被打回):

| 原则 | 说明 |
| :--- | :--- |
| **零跨进程内存注入** | 严禁调用 `VirtualAllocEx` / `ReadProcessMemory` / `WriteProcessMemory` 等进程内存 API(见 ADR-0006)——杀软启发式误报是绿色便携工具的致命伤;一切系统能力走 Win32 公开 API 或 Shell COM |
| **模块第一性原则** | `SetWinEventHook` / `WM_CLIPBOARDUPDATE` 等系统钩子必须隔离在**专用原生线程的消息泵**(`GetMessageW` / `DispatchMessageW`)内,严禁在 Tokio 协程中运行;停止协议需经取消令牌 + `PostThreadMessageW(WM_QUIT)` + 超时收尾 |
| **绝不阻塞 UI / Tokio 工作线程** | 同步 Win32 调用走 `spawn_blocking`;STA COM 操作经 `spawn_com_thread` 拉起全新独立 OS 线程(严格配对 `CoInitializeEx(COINIT_APARTMENTTHREADED)` / `CoUninitialize`) |
| **无请求 / 无遥测** | 程序不含任何网络请求与遥测代码;检查更新走 WinHttp 原生 API 且失败不影响功能 |
| **路径绝对锚定** | 配置与日志路径一律以 `current_exe()` 父目录为基准解析(`resolve_app_path`),与 CWD 彻底解耦 |
| **原子持久化** | 配置写盘采用同目录临时文件 + `rename` 原子替换,坏配置解析失败绝不静默覆盖 |
| **低打扰常驻** | 新模块默认关闭、由用户显式开启;常驻物理内存保持在 ~1.8 MB 级别(隐藏进托盘时 `EmptyWorkingSet`) |

> 新增模块前建议先发起 Issue 讨论定位与原理,避免与既有能力重复
> (当前 7 个模块:poper_blocker / keep_awake / clipboard_purifier /
> terminal_logger / port_hunter / topmost_manager / icon_locker)。

---

## 2. 环境与构建要求

### 2.1 最低环境

| 组件 | 要求 |
| :--- | :--- |
| 操作系统 | Windows 10 / 11 x64(构建目标 `x86_64-pc-windows-msvc`) |
| Rust 工具链 | **stable MSVC**:`rustup default stable-x86_64-pc-windows-msvc` |
| 编译组件 | Visual Studio Build Tools(勾选「使用 C++ 的桌面开发」——提供 MSVC 编译器、`link.exe` 与内嵌资源编译所需的 `rc.exe`) |
| 依赖锁定 | 提交必须附带更新后的 `Cargo.lock`(当前锁定 Slint 1.17.1 / windows 0.58.0) |

### 2.2 常用命令

```powershell
# 调试运行(保留控制台黑框,实时输出 tracing debug 日志)
cargo run

# Release 构建(单文件、LTO + Strip,产物约 11 MB)
cargo build --release

# 全量测试(见下节)
cargo test --all-targets
```

> 时区注意:本项目部分测试(CMD 捕获脚本)涉及 OEM 代码页(兼容 cp437 / cp936),
> CI 与本地均需保证跨区域可运行。

---

## 3. 质量门禁(提交前必须全绿)

每份 PR 合并前,以下三条必须通过;维护者会以 CI 结果为**硬性**门槛:

### 3.1 Format 检查

```powershell
cargo fmt --all -- --check
```

> 代码格式必须与 `rustfmt` 默认配置一致(项目未自定义 rustfmt.toml,遵循官方默认)。

### 3.2 Clippy 零告警

```powershell
cargo clippy --all-targets -- -D warnings
```

项目自 v0.3.1 起确立 **Clippy 零告警规范**:任何 `clippy` 警告(而非仅错误)
都被 `-D warnings` 提升为编译失败。新增代码不得引入新的 lint 告警;
如确需豁免,应使用带理由的局部 `#[allow(...)]`,并在代码注释中说明原因。

### 3.3 全量测试

```powershell
# 全量脱机测试(单元 + 集成,不依赖图形环境)
cargo test --all-targets

# 含真实注册表往返 / 控制台自清理的实机测试(需要真实 Windows 会话)
cargo test --all-targets -- --ignored
```

- 项目现有 **140+ 项**单元与集成测试,覆盖:模块生命周期与广播状态、配置
  引擎的路径锚定与原子持久化、端口猎手降噪过滤、图标锁拓扑指纹与方案生命周期、
  COM 错误模型、更新检查版本解析等;
- **新增功能必须配测试**:纯逻辑(过滤、解析、指纹、格式)写单元测试;
  涉及模块生命周期的事件断言走 `tests/module_lifecycle_integration_tests.rs`
  的无头集成测试;
- 标 `#[ignore]` 的实机测试仅在本地执行确认,需在 PR 描述中注明已验证。

### 3.4 CI 流水线

`.github/workflows/release.yml` 在推送 `v*` tag 或手动触发时执行
`cargo test --release --all-targets` + `cargo build --release` + 打包
zip/SHA256(Windows x86_64,vcvars64 环境)。PR 分支虽不触发发布,但建议
本地先行跑通上述三扇门。

---

## 4. 代码风格与模块契约

- **函数 / 模块文档**:新增 `pub` 项必须带 `///` 或 `//!` 中文文档注释,
  说明动机、线程模型与调用约束(参照既有模块的文档风格);
- **模块统一契约**:实现 `crate::modules::ToolModule`(共享借用 `&self`,
  内部可变性状态),`id()` 使用 kebab-case 模块 ID;新模块需在
  `src/modules/mod.rs` 声明、`crate::main` 注册、`config.rs` 增加配置节、
  README 模块表补充一行;
- **tracing 目标**:日志一律带 `target: "<module_id>"` 参数,便于按模块过滤;
- **Windows 平台**:平台相关代码用 `#[cfg(windows)]` 包裹,并提供非 Windows
  占位实现(如返回 `Unsupported`),保证 `cargo test` 在跨平台 CI 可编译;
- **COM 生命周期**:取得 COM 接口后必须保证在 STA 公寓存活期内使用与释放
  (按声明顺序析构,先 Release 接口再 `CoUninitialize`);PIDL 等调用方释放
  契约的句柄用 RAII(如 `PidlPool`)统一回收。

---

## 5. UI 156px 绝对栅格规范

主界面模块卡片(ModuleCard)的操作区为 **156px 绝对坐标栅格**——这是 v0.5.0
定稿的硬性排版规范,彻底消除 X/Y 轴弹性布局偏差。修改卡片布局时必须严格遵循,
禁止用 Flex / 占位矩形重新引入漂移。

### 5.1 操作区总宽(固定 156px)

```
156px = 76(状态区) + 8(间距) + 24(齿轮) + 8(间距) + 40(开关)
```

定义于 `ui/app.slint` 的卡片组件(`action-box := Rectangle { width: 156px; ... }`)。
四块区域坐标恒定,元素按线性坐标放置,不依赖任何弹性布局:

| 区域 | X 起点 | 宽度 | 说明 |
| :--- | :--- | :--- | :--- |
| 状态区 `status-container` | `x: 0` | `76px` | 圆点 + 状态文本,见 5.2 |
| 间距 | `x: 76` | `8px` | 固定留白 |
| 齿轮按钮 `GhostIconButton` | `x: 84px` | `24px`(高 24) | 仅 `has_settings` 渲染 |
| 间距 | `x: 108` | `8px` | 固定留白 |
| 开关 `Switch` | `x: 116px` | `40px`(高 20) | 仅 `toggleable` 渲染 |

### 5.2 状态区内部绝对钉死

- **圆点**:`x: 4px` 钉死(不随文本长度移动),6 × 6px 圆角;
- **状态文本**:`x: 16px` 左对齐钉死,宽度 `parent.width - 16px`,`overflow: elide`;
- 状态文本「运行中 / 已停止 / 即开即用」长短变化**绝不**推移圆点或后续元素;
  - 颜色语义:运行中 = `Theme.running-dot` / `theme.accent-strong`,
    已停止 = `Theme.stopped-dot` / `Theme.text-muted`,
    即开即用(工具态)= `Theme.tool-dot` / `Theme.text-secondary`。

### 5.3 Y 轴与缺省渲染

- 每块元素 Y 轴各自按 `(parent.height - self.height) / 2` 严格垂直居中;
- **端口占用管理卡片**(`toggleable = false`)无开关时,`x: 116` 处**自然留白**,
  齿轮恒在 `x: 84`——不得因缺开关而让齿轮漂移;
- 齿轮仅在 `has_settings` 为真时渲染,图标走 `ui/icons/*.svg` 矢量资源
  (如 `settings.svg`),禁止引入位图图标。

### 5.4 验证方式

调整卡片布局后,请以 125% / 150% 缩放(Per-Monitor V2)目检 7 张卡片:
状态文本与圆点必须恒左对齐、齿轮 / 开关必须恒在 x:84 / x:116 纵向居中,
任何缩放比例下不得出现错位或截断。

---

## 6. 提交信息规范

采用 **Conventional Commits** 风格(与既有 git 历史一致,中文描述):

```
<type>(<scope>): <简短中文摘要>

[可选正文:说明动机、变更要点、破坏性影响]
```

- **type**:`feat` / `fix` / `docs` / `chore` / `ci` / `test` / `refactor`;
- **scope**(可选):模块 ID(`popup_blocker` / `icon_locker` / `port_hunter` /
  `topmost_manager` / `terminal_logger`)或 `ui` / `config` / `core` / `release`;
- **示例**:
  - `feat(icon_locker): 新增显示器拓扑指纹去重,插拔屏不重复建方案`
  - `fix(port_hunter): 修复释放失败时错误码未透传到 Toast 的问题`
  - `docs: 补充 ADR-0006 与故障排查指南`
- 禁止裸提交无说明;一个 PR 含多个逻辑变更时拆分为多个提交。
- 除提交信息外,**请同步更新 CHANGELOG.md** 对应版本(或 [Unreleased])
  的 [Added] / [Fixed & Changed] 条目。

---

## 7. PR 提交流程

1. **先建分支**:从最新 `main` 拉取,分支名建议
   `fix/<module>-<简述>` 或 `feat/<module>-<简述>`(如 `feat/icon-locker-fingerprint`);
2. **实现 + 测试**:本地完成开发,补测试;
3. **过门禁**:按第 3 节执行 `cargo fmt --check` / `clippy -D warnings` /
   `cargo test --all-targets`,全部通过;涉及实机行为时本地跑 `-- --ignored`
   并在 PR 描述注明;
4. **更新文档**:按变更类别更新 `README.md`(模块表 / 配置节)、
   `docs/TROUBLESHOOTING.md`(新增已知问题)、`docs/adr/`(新架构决策,
   编号延续 `0006`)、`config/tltoolbox.example.toml`(配置节改动时);
5. **发 PR**:标题同提交规范(如 `fix(ui): 修正高 DPI 下卡片状态区错位`);
   描述包含:
   - 变更动机与行为差异;
   - 质量门禁结果摘要(测试数、clippy 0 warning);
   - 手动验证清单(如「125% 缩放目检 7 卡片」);
   - 是否涉及配置格式 / 兼容性(旧配置回退路径)。
6. **等待评审**:维护者会按上述铁律逐条审查;评审意见以修改请求形式提出,
   请基于讨论更新分支(避免 force-push 覆盖评审历史,采用补充提交),解决后
   @ 维护者复审;
7. **合并**:通过评审且 CI 全绿后由维护者 squash-merge 或 rebase-merge。

---

## 8. 发布流程(维护者)

1. 更新 `Cargo.toml` 版本号(如 `0.6.0`)与 `CHANGELOG.md`
   (将 [Unreleased] 改写为对应版本条目,补齐日期);
2. 本地全量验证 `cargo test --all-targets` + `cargo build --release`;
3. 推送提交并打 tag:
   ```powershell
   git tag -a v0.6.0 -m "release v0.6.0"
   git push origin v0.6.0
   ```
4. 推送 `v*` tag 触发 `.github/workflows/release.yml`:
   自动执行测试 + Release 编译 + 打包 `tltoolbox-windows-x86_64.zip` 与
   `.sha256`,并创建 GitHub Release(形如 `v0.6.0-rc.1` 的 tag 自动标记预发布);
5. Release 就绪后确认资产完整,必要时补发 `workflow_dispatch` 演练。

---

## 参考文档

- `README.md` — 功能特性 / 架构设计亮点 / 配置说明;
- `docs/adr/0006-icon-locker-shell-com.md` — 图标锁零注入决策(ADR 模板首次落地);
- `docs/TROUBLESHOOTING.md` — 常见问题与排查;
- `CHANGELOG.md` — 版本变更历史(Keep a Changelog);
- `config/tltoolbox.example.toml` — 全量默认配置与注释。