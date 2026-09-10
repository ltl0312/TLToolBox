//! Native desktop icon layout access through STA Shell COM.

use crate::config::IconCoordinate;
use std::collections::HashMap;
use std::fmt;

#[derive(Debug)]
pub enum ExplorerError {
    Unsupported,
    Com(String),
}

impl fmt::Display for ExplorerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => write!(f, "桌面图标布局仅支持 Windows"),
            Self::Com(message) => write!(f, "Shell COM 操作失败: {message}"),
        }
    }
}
impl std::error::Error for ExplorerError {}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesktopLayout {
    pub positions: HashMap<String, IconCoordinate>,
}

#[cfg(not(windows))]
pub fn capture_layout() -> Result<DesktopLayout, ExplorerError> {
    Err(ExplorerError::Unsupported)
}

#[cfg(not(windows))]
pub fn restore_layout(_layout: &DesktopLayout) -> Result<(), ExplorerError> {
    Err(ExplorerError::Unsupported)
}

/// 在**全新的独立 OS 线程**内执行一次 Shell COM 操作（非 Windows 占位：直接失败，
/// 且不向 `_tx` 发送——调用方 `rx.await` 得到 `Err(RecvError)` 后走「任务异常」分支）。
#[cfg(not(windows))]
pub fn spawn_com_thread<T, E, F>(
    _thread_name: &'static str,
    _op: F,
    _tx: tokio::sync::oneshot::Sender<Result<T, E>>,
) -> std::io::Result<()>
where
    T: Send + 'static,
    E: Send + 'static,
    F: FnOnce() -> Result<T, E> + Send + 'static,
{
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "桌面图标布局仅支持 Windows",
    ))
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use windows::core::{GUID, Interface, VARIANT};
    use windows::Win32::Foundation::POINT;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_LOCAL_SERVER,
        COINIT_APARTMENTTHREADED, IDispatch, IServiceProvider,
    };
    use windows::Win32::UI::Shell::{
        IFolderView, IShellBrowser, IShellFolder, IShellWindows, ShellWindows, SHGDNF,
        SIGDN_NORMALDISPLAY, SWC_DESKTOP, SWFO_NEEDDISPATCH,
    };

    /// `SID_STopLevelBrowser`（{4C96BE40-915C-11CF-99D3-00AA004AE837}）：从桌面
    /// `IServiceProvider::QueryService` 取得 `IShellBrowser` 的服务标识。
    const SID_S_TOP_LEVEL_BROWSER: GUID =
        GUID::from_u128(0x4C96BE40_915C_11CF_99D3_00AA004AE837);

    /// `RPC_E_CHANGED_MODE`（0x80010106）：`CoInitializeEx` 在该线程已被其它 COM
    /// 线程模型初始化时返回。按规格容错——不中断执行，复用既有公寓。
    const RPC_E_CHANGED_MODE: i32 = 0x80010106u32 as i32;

    /// STA COM 单元守卫：`CoInitializeEx(COINIT_APARTMENTTHREADED)` 与 `CoUninitialize`
    /// 严格配对。
    ///
    /// 容错：`CoInitializeEx` 返回 `RPC_E_CHANGED_MODE`（线程已被其它 COM 线程模型
    /// 初始化，如 Tokio 阻塞池线程被第三方隐式初始化为 MTA）时**不视为致命错误**，
    /// 复用它既有的公寓继续执行；此时初始化并非本守卫所有，Drop 不再
    /// `CoUninitialize`（避免拆掉别人初始化的公寓）。
    struct StaCom {
        owns_apartment: bool,
    }
    impl StaCom {
        fn initialize() -> Result<Self, ExplorerError> {
            let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            if hr.0 == RPC_E_CHANGED_MODE {
                tracing::warn!(
                    target: "icon_locker",
                    "CoInitializeEx 返回 RPC_E_CHANGED_MODE(0x80010106)：线程已被其它 COM 线程模型初始化，复用既有公寓继续执行"
                );
                return Ok(Self {
                    owns_apartment: false,
                });
            }
            hr.ok().map_err(|err| {
                ExplorerError::Com(format!(
                    "CoInitializeEx 失败(HRESULT=0x{:08X}): {err}",
                    hr.0 as u32
                ))
            })?;
            Ok(Self {
                owns_apartment: true,
            })
        }
    }
    impl Drop for StaCom {
        fn drop(&mut self) {
            if self.owns_apartment {
                unsafe { CoUninitialize() };
            }
        }
    }

    /// 把 `windows::core::Error` 转为 [`ExplorerError::Com`]，并在此记录**步骤级**
    /// 错误日志（含 HRESULT），便于定位具体是 `FindWindowSW` / `QueryService` /
    /// `Item` 枚举 / 定位 / 批量还原哪一步抛错。
    fn com_step<T>(step: &'static str, result: windows::core::Result<T>) -> Result<T, ExplorerError> {
        result.map_err(|err| {
            let code = err.code().0;
            tracing::error!(
                target: "icon_locker",
                "{step} 失败: {err} (HRESULT=0x{:08X})",
                code as u32
            );
            ExplorerError::Com(format!("{step}(HRESULT=0x{:08X}): {err}", code as u32))
        })
    }

    fn display_name(
        folder: &IShellFolder,
        child: *const windows::Win32::UI::Shell::Common::ITEMIDLIST,
    ) -> Result<String, ExplorerError> {
        let mut raw = windows::Win32::UI::Shell::Common::STRRET::default();
        unsafe {
            com_step(
                "IShellFolder::GetDisplayNameOf",
                folder.GetDisplayNameOf(child, SHGDNF(SIGDN_NORMALDISPLAY.0 as u32), &mut raw),
            )?;
            let mut buffer = [0u16; 512];
            com_step(
                "StrRetToBufW",
                windows::Win32::UI::Shell::StrRetToBufW(&mut raw, Some(child), &mut buffer),
            )?;
            let len = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
            Ok(String::from_utf16_lossy(&buffer[..len]))
        }
    }

    /// `IFolderView::Item` 返回的 PIDL 由调用方以 `CoTaskMemFree` 释放（MSDN 契约）。
    /// 池化收集 + Drop 统一释放：无论正常完成还是中途 `?` 提前返回（display_name /
    /// `SelectAndPositionItems` 失败等），已取出的 PIDL 全部回收，杜绝泄漏。
    struct PidlPool(Vec<*mut windows::Win32::UI::Shell::Common::ITEMIDLIST>);
    impl PidlPool {
        fn new() -> Self {
            Self(Vec::new())
        }
        fn push(&mut self, pidl: *mut windows::Win32::UI::Shell::Common::ITEMIDLIST) {
            self.0.push(pidl);
        }
    }
    impl Drop for PidlPool {
        fn drop(&mut self) {
            for pidl in &self.0 {
                if !pidl.is_null() {
                    unsafe { CoTaskMemFree(Some(*pidl as *const _)) };
                }
            }
        }
    }

    /// 桌面视图链路 + 守护其存活期的 STA 单元。
    ///
    /// 字段**按声明顺序析构**：`folder_view` / `folder` 先 Release，`_sta` 最后
    /// `CoUninitialize`——保证在 STA 单元内取得的 Shell 接口始终在公寓存活期内使用
    /// 与释放（先 Uninit 后调方法属未定义行为）。
    struct DesktopView {
        folder_view: IFolderView,
        folder: IShellFolder,
        _sta: StaCom,
    }

    /// 经 `IShellWindows::FindWindowSW(SWC_DESKTOP)` **直连**桌面窗口（严禁遍历
    /// `IShellWindows` 集合），沿 `IDispatch → IServiceProvider →
    /// QueryService(SID_STopLevelBrowser) → IShellBrowser → QueryActiveShellView →
    /// IShellView → IFolderView → IShellFolder` 取得活动视图链路。
    ///
    /// 任一步失败都经 [`com_step`] 记录**含 HRESULT** 的步骤级错误日志并立即上报，
    /// 不做静默跳过。
    fn find_desktop_view() -> Result<DesktopView, ExplorerError> {
        let _sta = StaCom::initialize()?;
        let windows: IShellWindows = com_step(
            "CoCreateInstance(CLSID_ShellWindows)",
            unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER) },
        )?;

        // FindWindowSW 直连桌面：SWC_DESKTOP = 8，SWFO_NEEDDISPATCH = 1（要求返回
        // IDispatch），pvarloc = VT_I4(0)（桌面位置的 PIDL 位置参数），pvarlocroot 空。
        let mut hwnd = 0i32;
        let dispatch: IDispatch = com_step(
            "FindWindowSW(SWC_DESKTOP)",
            unsafe {
                windows.FindWindowSW(
                    &VARIANT::from(0i32),
                    &VARIANT::default(),
                    SWC_DESKTOP,
                    &mut hwnd,
                    SWFO_NEEDDISPATCH,
                )
            },
        )?;
        tracing::debug!(
            target: "icon_locker",
            "FindWindowSW 命中桌面窗口 hwnd=0x{:X}",
            hwnd as u32
        );

        let provider: IServiceProvider =
            com_step("IDispatch → IServiceProvider", dispatch.cast())?;
        let browser: IShellBrowser = com_step(
            "QueryService(SID_STopLevelBrowser)",
            unsafe { provider.QueryService(&SID_S_TOP_LEVEL_BROWSER) },
        )?;
        let shell_view = com_step(
            "IShellBrowser::QueryActiveShellView",
            unsafe { browser.QueryActiveShellView() },
        )?;
        let folder_view: IFolderView = com_step("IShellView → IFolderView", shell_view.cast())?;
        let folder: IShellFolder = com_step(
            "IFolderView::GetFolder",
            unsafe { folder_view.GetFolder() },
        )?;
        Ok(DesktopView {
            folder_view,
            folder,
            _sta,
        })
    }

    pub fn capture_layout() -> Result<DesktopLayout, ExplorerError> {
        let desktop = find_desktop_view()?;
        let count = com_step(
            "IFolderView::ItemCount",
            unsafe { desktop.folder_view.ItemCount(windows::Win32::UI::Shell::_SVGIO(0)) },
        )?;
        tracing::info!(target: "icon_locker", "抓取桌面图标布局：共 {count} 个图标");
        let mut positions = HashMap::new();
        let mut pidl_pool = PidlPool::new();
        for item in 0..count {
            let pidl = com_step(
                "IFolderView::Item 枚举",
                unsafe { desktop.folder_view.Item(item) },
            )?;
            pidl_pool.push(pidl);
            let point = com_step(
                "IFolderView::GetItemPosition",
                unsafe { desktop.folder_view.GetItemPosition(pidl) },
            )?;
            let name = display_name(&desktop.folder, pidl as *const _)?;
            positions.insert(
                name,
                IconCoordinate {
                    x: point.x,
                    y: point.y,
                },
            );
        }
        tracing::info!(
            target: "icon_locker",
            "抓取完成：{} 个图标坐标入册",
            positions.len()
        );
        Ok(DesktopLayout { positions })
    }

    pub fn restore_layout(layout: &DesktopLayout) -> Result<(), ExplorerError> {
        let desktop = find_desktop_view()?;
        let count = com_step(
            "IFolderView::ItemCount",
            unsafe { desktop.folder_view.ItemCount(windows::Win32::UI::Shell::_SVGIO(0)) },
        )?;
        let mut pidls = Vec::new();
        let mut points = Vec::new();
        let mut pidl_pool = PidlPool::new();
        for item in 0..count {
            let pidl = com_step(
                "IFolderView::Item 枚举",
                unsafe { desktop.folder_view.Item(item) },
            )?;
            pidl_pool.push(pidl);
            let name = display_name(&desktop.folder, pidl as *const _)?;
            if let Some(position) = layout.positions.get(&name) {
                pidls.push(pidl);
                points.push(POINT {
                    x: position.x,
                    y: position.y,
                });
            }
        }
        tracing::info!(
            target: "icon_locker",
            "还原桌面图标布局：方案 {} 个图标，匹配到 {} 个",
            layout.positions.len(),
            pidls.len()
        );
        if !pidls.is_empty() {
            let raw: Vec<*const windows::Win32::UI::Shell::Common::ITEMIDLIST> =
                pidls.iter().map(|p| *p as *const _).collect();
            com_step(
                "IFolderView::SelectAndPositionItems",
                unsafe {
                    desktop.folder_view.SelectAndPositionItems(
                        raw.len() as u32,
                        raw.as_ptr(),
                        Some(points.as_ptr()),
                        0,
                    )
                },
            )?;
        }
        // 所有已取出的 PIDL（含未匹配方案、错误提前返回）随池析构统一 CoTaskMemFree。
        drop(pidl_pool);
        Ok(())
    }

    /// 在**全新的独立 OS 线程**内执行一次 Shell COM 操作（抓取 / 还原共用入口）。
    ///
    /// 动机：`tokio::task::spawn_blocking` 复用的阻塞池线程可能残留第三方隐式初始化
    /// 的 COM 公寓（如 MTA），既与 `COINIT_APARTMENTTHREADED` 冲突（返回
    /// `RPC_E_CHANGED_MODE`），又会把公寓状态随线程复用**污染**后续任务。本函数每次
    /// 拉一条全新 OS 线程，保证：
    ///
    /// 1. 线程内先 `CoInitializeEx(None, COINIT_APARTMENTTHREADED)`。若返回
    ///    `RPC_E_CHANGED_MODE`（0x80010106）——该线程已被其它 COM 线程模型初始
    ///    化——**不视为致命错误**：记日志后继续执行，且不执行 `CoUninitialize`
    ///    （该初始化不为本线程所有）；
    /// 2. 执行 `op`（如 `capture_layout` / `restore_layout` / 模块层抓取还原）；
    /// 3. 返回前调用 `CoUninitialize()`，线程随即结束，公寓零残留。
    ///
    /// 结果经 `tx`（`tokio::sync::oneshot`）异步回传，调用方在 Tokio 任务内
    /// `await rx` 即可拿到 `Result<T, E>`——Tokio 工作线程池永不接触 Shell COM。
    pub fn spawn_com_thread<T, E, F>(
        thread_name: &'static str,
        op: F,
        tx: tokio::sync::oneshot::Sender<Result<T, E>>,
    ) -> std::io::Result<()>
    where
        T: Send + 'static,
        E: Send + 'static,
        F: FnOnce() -> Result<T, E> + Send + 'static,
    {
        std::thread::Builder::new()
            .name(thread_name.to_string())
            .spawn(move || {
                let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
                let owns_apartment = if hr.0 >= 0 {
                    // S_OK(0) 全新初始化 / S_FALSE(1) 同模型重复初始化：两者都需一次
                    // 配对的 CoUninitialize（MSDN：含 S_FALSE 的成功调用必须平衡）。
                    true
                } else if hr.0 == RPC_E_CHANGED_MODE {
                    tracing::warn!(
                        target: "icon_locker",
                        "{thread_name}: CoInitializeEx 返回 RPC_E_CHANGED_MODE(0x80010106)，线程已被其它 COM 线程模型初始化，继续执行（不执行 CoUninitialize）"
                    );
                    false
                } else {
                    tracing::error!(
                        target: "icon_locker",
                        "{thread_name}: CoInitializeEx 失败 HRESULT=0x{:08X}",
                        hr.0 as u32
                    );
                    // 不 send：tx 随闭包结束 drop → 调用方 rx.await 得 Err(RecvError)，
                    // 走「COM 线程异常」分支。
                    return;
                };
                let result = op();
                if owns_apartment {
                    unsafe { CoUninitialize() };
                }
                let _ = tx.send(result);
            })?;
        Ok(())
    }
}

#[cfg(windows)]
pub use windows_impl::{capture_layout, restore_layout, spawn_com_thread};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn coordinates_compare_exactly() {
        let a = IconCoordinate { x: 10, y: 20 };
        assert_eq!(a, IconCoordinate { x: 10, y: 20 });
        assert_ne!(a, IconCoordinate { x: 10, y: 21 });
    }
}