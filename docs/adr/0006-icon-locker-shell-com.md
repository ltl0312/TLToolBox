# ADR-0006:桌面图标布局锁采用 STA Shell COM 无注入方案

- 状态:已接受 (Accepted)
- 日期:2026-09-10(v0.6.0)
- 决策者:TLToolBox 维护者
- 关联版本:v0.6.0(`[icon_locker]` 模块)
- 关联代码:`src/modules/icon_locker/{mod.rs, daemon.rs, explorer.rs}`

## 背景 (Context)

插拔外接屏、修改分辨率、切换投影模式时,Windows 桌面图标会因显示器拓扑变化而
被 Explorer 重新排列,产生「图标乱跑」的顽疾。v0.6.0 引入「桌面图标布局锁」
(`icon_locker`)模块,目标:

1. 抓取当前桌面图标布局并保存为方案(图标名 → 坐标);
2. 显示器拓扑变化后自动把图标瞬移回原位。

实现该能力有两条技术路线:

- **A. 跨进程内存注入路线**:向 Explorer 进程调用 `VirtualAllocEx` 分配内存、
  `ReadProcessMemory` / `WriteProcessMemory` 读写其内部图标位置表(常见于第三方
  桌面整理工具)。
- **B. Shell COM 公开接口路线**:经 `IShellWindows::FindWindowSW(SWC_DESKTOP)`
  直连桌面视图,沿 `IShellBrowser → IFolderView` 链路,用
  `IFolderView::GetItemPosition` / `SelectAndPositionItems` 读写图标坐标。

## 决策 (Decision)

采用 **路线 B:纯原生 STA Shell COM 无注入方案**,并配套以下设计:

1. **弃用跨进程内存注入**:严禁调用 `VirtualAllocEx` / `ReadProcessMemory` /
   `WriteProcessMemory` 等进程内存 API。
2. **COM 链路**:`IShellWindows::FindWindowSW(SWC_DESKTOP)` 直连桌面窗口 →
   `IDispatch → IServiceProvider → QueryService(SID_STopLevelBrowser) →
   IShellBrowser → QueryActiveShellView → IShellView → IFolderView →
   IShellFolder`。
3. **纯净 STA 线程模型**:每次抓取 / 还原经 `spawn_com_thread` 拉起**全新的独立
   OS 线程**,线程内严格配对 `CoInitializeEx(COINIT_APARTMENTTHREADED)` /
   `CoUninitialize`(`RPC_E_CHANGED_MODE` 容错复用既有公寓);**绝不复用**
   `tokio::task::spawn_blocking` 阻塞池线程,避免第三方残留 MTA 公寓污染 Tokio
   异步运行时线程池。
4. **虚拟桌面绝对坐标**:保存与还原均以**桌面绝对坐标**(虚拟桌面坐标系,
   `POINT{x, y}`)为准,由 `IFolderView::GetItemPosition` 读取、
   `SelectAndPositionItems` 批量写入,不做屏幕相对换算。
5. **1500ms 可重置防抖**:专用原生消息泵线程监听 `WM_DISPLAYCHANGE`,
   每次收到消息都以**同 ID `SetTimer` 重新武装**一枚 1500ms 一次性定时器
   (天然重置计时的防抖语义),拓扑稳定 1500ms 后才触发一次自动还原,
   绝不在高频显示事件风暴中重复执行。
6. **自动排列图标冲突应对**:还原结果可能被 Explorer 的「自动排列图标」吸附网格
   吞掉——文档与失败 Toast 明确指引用户关闭桌面右键「自动排列图标」,并在
   还原失败时经事件总线弹出提示(见 `docs/TROUBLESHOOTING.md`)。

## 备选方案 (Alternatives Considered)

### A. 跨进程内存注入(`VirtualAllocEx` / `ReadProcessMemory`)

**被否决**。理由:

- **杀毒软件启发式误报**:跨进程内存分配 + 读写是注入类恶意软件的标准特征,
  `VirtualAllocEx` / `ReadProcessMemory` 调用序列极易触发 Defender 等杀软的
  行为启发式拦截,轻则误报、重则隔离可执行文件——对「绿色便携、免安装」定位
  是致命伤;
- **系统完整性风险**:硬编码读写 Explorer 进程内部数据结构,版本升级后偏移
  失效,且写坏内存可能导致 Explorer 崩溃(桌面 / 任务栏 / 文件管理器全灭);
- **UIPI / 完整性级别限制**:普通权限进程对更高完整性的 Explorer 读写受限,
  需要提权,进一步放大风险面;
- **维护成本**:内部结构是未文档化的私有布局,随时可能变化。

### B' 方案变体:在 Tokio 工作线程池内直接执行 COM

**被否决**。`tokio::task::spawn_blocking` 复用的阻塞池线程可能残留第三方隐式
初始化的 COM 公寓(如 MTA),既与 `COINIT_APARTMENTTHREADED` 冲突(返回
`RPC_E_CHANGED_MODE`),又会把公寓状态随线程复用**污染**后续任务。因此每次
COM 操作都在全新独立 OS 线程内完成,线程结束公寓零残留。

## 后果 (Consequences)

### 正面 (Positive)

- **零跨进程内存注入**:从根上杜绝杀毒软件启发式误报与系统完整性风险,
  契合绿色便携、默认普通权限的定位;
- **纯公开接口**:`IFolderView` 是 Shell 公开 COM 接口,版本演进由微软背书,
  不依赖 Explorer 私有数据结构;
- **公寓纯净**:STA 严格配对,`RPC_E_CHANGED_MODE` 容错,Tokio 运行时线程池
  永不接触 Shell COM;
- **防抖可重置**:连续显示事件(拔插屏、分辨率切换过程)自动顺延,
  只在稳定后执行一次还原,避免重复瞬移与 UI 卡顿。

### 负面 / 约束 (Negative / Constraints)

- **依赖 Explorer 存活**:`FindWindowSW(SWC_DESKTOP)` 要求 Explorer 桌面视图
  可访问;Explorer 崩溃 / 未启动时 COM 链路失败(已按步骤记录含 HRESULT 的
  错误日志并 Toast 提示);
- **受「自动排列图标」制约**:启用自动排列时 Explorer 会强制吸附网格并吞掉
  `SelectAndPositionItems` 的定位结果——这是 Windows 行为限制,只能文档化指引,
  无法从程序侧绕过;
- **兼容性窗口**:部分第三方 Shell 扩展可能接管桌面视图,COM 链路仍走
  `SWC_DESKTOP` 的默认桌面窗口,若被接管需重新评估;
- **坐标键为 DisplayName**:图标坐标以显示名称(DisplayName)为键,同名图标
  (如两个同名快捷方式)存在合并覆盖的边界情况,属已知取舍。

## 关联文档

- `docs/TROUBLESHOOTING.md` — 桌面图标还原无效(自动排列图标排查)等 FAQ;
- `README.md` §「桌面图标布局锁」与架构亮点 §5;
- `src/modules/icon_locker/explorer.rs` — COM 链路与 `spawn_com_thread` 实现;
- `src/modules/icon_locker/daemon.rs` — 拓扑指纹与 `WM_DISPLAYCHANGE` 防抖守护。
