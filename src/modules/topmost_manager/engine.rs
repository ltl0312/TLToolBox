//! # Z-Order 优先级链式置顶引擎（v0.4.0 · 全局窗口置顶守护）
//!
//! 本文件承载「多窗口分级置顶」的**纯算法核心**（优先级排序、链式锚定规划、
//! 前台抢占纠偏的受保护窗口规划）与**薄 FFI 边界**（`SetWindowPos` / `IsWindow`
//! 等最小 Win32 调用），供 `super::mod` 的模块生命周期层驱动。
//!
//! # 1~9 级优先级链式排列
//!
//! 受管窗口按 `priority` **升序**排列（1 级最顶层，9 级最底层）；等优先级窗口
//! 以各自的**操作世代号**（每次置顶 / 改级自增）为次级排序键，保证“后操作者在上”
//! 的直觉顺序且重刷 Z-Order 时相对顺序稳定。应用链条时执行链式 `SetWindowPos`：
//!
//! - 第 1 项：`SetWindowPos(hwnd₀, HWND_TOPMOST, …, SWP_NOMOVE|SWP_NOSIZE|SWP_NOACTIVATE)`
//!   锚定在绝对置顶层；
//! - 第 i 项：`SetWindowPos(hwndᵢ, hwndᵢ₋₁, …, 同标志)`——锚定在上一窗口之后，
//!   逐级收紧成不可穿插的优先级链。
//!
//! 取消置顶：`SetWindowPos(hwnd, HWND_NOTOPMOST, …, 同标志)`。
//!
//! # 前台抢占纠偏（防下压）
//!
//! 用户点击某个低优先级置顶窗口时，系统会将其提升到绝对顶层（激活即置顶）。此时
//! 守护线程把该窗口之后、优先级 ≤ 2 的受管窗口沿其内部链序重刷一次 Z-Order
//! （[`plan_shield_refresh`]）——重新锚定回 1 / 2 级窗口之上，全程
//! `SWP_NOACTIVATE`，**绝不抢占用户焦点**。
//!
//! # 健壮性防御
//!
//! - 任何 `SetWindowPos` 前置 `IsWindow` 校验：失效句柄静默跳过（调用方随后清理）；
//! - UIPI 隔离拦截：`SetWindowPos` 返回 `ERROR_ACCESS_DENIED`（HRESULT
//!   `0x80070005`，目标窗口完整性级别高于本进程）时，经 [`Win32Error::AccessDenied`]
//!   显式上报——由模块层转 Toast 提示用户“目标窗口具备高特权，请提权运行
//!   TLToolBox”。
//!
//! # 纯逻辑 / FFI 分层（可测性）
//!
//! 排序、锚定规划等算法全部为**无 FFI 的纯函数**（`order_entries` /
//! [`plan_shield_refresh`] / [`chain_apply_plan`]），单元测试直接构造假窗口条目
//! 验证；FFI 仅剩 `set_topmost` / `set_notopmost` / [`apply_chain`] / `is_window_alive`
//! 四个薄封装。

use crate::modules::topmost_manager::enum_windows::to_hwnd;

#[cfg(windows)]
use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{
        IsWindow, SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SET_WINDOW_POS_FLAGS, SWP_NOACTIVATE,
        SWP_NOMOVE, SWP_NOSIZE,
    },
};

/// 置顶优先级下界（1 = 最顶层）。
pub const PRIORITY_MIN: u8 = 1;
/// 置顶优先级上界（9 = 最低层置顶）。
pub const PRIORITY_MAX: u8 = 9;
/// 前台抢占纠偏时受保护的最高优先级（重刷 1 级与 2 级窗口）。
pub const SHIELD_PRIORITY_MAX: u8 = 2;

/// 把任意 `u8` 夹紧到合法优先级区间（`1..=9`）。
///
/// 注意：`u8::clamp` 在 const 上下文中不可用（`Ord` 尚非 const trait），
/// 故本函数刻意不做 `const fn`。
pub fn clamp_priority(priority: u8) -> u8 {
    priority.clamp(PRIORITY_MIN, PRIORITY_MAX)
}

/// 窗口置顶 FFI 调用的结果分类（供模块层决定清理 / 上报 / Toast）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Win32Error {
    /// 目标窗口已不存在（`IsWindow` 为假）——调用方应清理记录。
    InvalidWindow,
    /// UIPI 隔离拦截（`ERROR_ACCESS_DENIED`）——目标窗口完整性级别高于本进程。
    AccessDenied,
    /// 其余 Win32 失败（携带底层错误文本）。
    Other(String),
}

impl std::fmt::Display for Win32Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidWindow => write!(f, "窗口已不存在"),
            Self::AccessDenied => write!(
                f,
                "拒绝访问（目标窗口完整性级别更高，请提权运行 TLToolBox）"
            ),
            Self::Other(reason) => write!(f, "Win32 调用失败: {reason}"),
        }
    }
}

/// 判定窗口句柄当前是否仍然有效（`IsWindow`）。
///
/// 任何后续 FFI 调用前都须先过本校验（模块层在清理记录时同样复用）。
pub fn is_window_alive(hwnd: isize) -> bool {
    #[cfg(windows)]
    {
        // SAFETY: IsWindow 是文档化的只读校验，对任意值句柄均安全返回布尔。
        unsafe { IsWindow(to_hwnd(hwnd)).as_bool() }
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        false
    }
}

/// `SetWindowPos` 的统一标志位：不移动 / 不缩放 / 不激活。
///
/// 链式守护的全部重定位都携带这三个标志——置顶语义只关心 Z-Order，绝不移动窗口
/// 位置、改变尺寸或抢走用户焦点。
#[cfg(windows)]
const ZORDER_ONLY_FLAGS: SET_WINDOW_POS_FLAGS =
    SET_WINDOW_POS_FLAGS(SWP_NOMOVE.0 | SWP_NOSIZE.0 | SWP_NOACTIVATE.0);

/// 把 `windows::core::Error` 分类为 [`Win32Error`]（含 UIPI 拦截识别）。
#[cfg(windows)]
fn classify_win32_error(err: windows::core::Error) -> Win32Error {
    // ERROR_ACCESS_DENIED (5) → HRESULT_FROM_WIN32 = 0x80070005。
    if err.code().0 == (0x8007_0005u32 as i32) {
        Win32Error::AccessDenied
    } else {
        Win32Error::Other(err.to_string())
    }
}

/// 把 `isize` 句柄还原为 `HWND` 后执行一次链式定位。
#[cfg(windows)]
fn set_window_pos(hwnd: isize, insert_after: HWND) -> Result<(), Win32Error> {
    // SAFETY: SetWindowPos 为纯句柄调用；前置 IsWindow 校验由调用方完成，
    // 此处对已失效句柄调用仅返回错误码，无未定义行为。
    unsafe {
        SetWindowPos(to_hwnd(hwnd), insert_after, 0, 0, 0, 0, ZORDER_ONLY_FLAGS)
            .map_err(classify_win32_error)
    }
}

/// 把窗口置顶（锚定 `HWND_TOPMOST` 绝对置顶层）。
pub fn set_topmost(hwnd: isize) -> Result<(), Win32Error> {
    if !is_window_alive(hwnd) {
        return Err(Win32Error::InvalidWindow);
    }
    #[cfg(windows)]
    {
        set_window_pos(hwnd, HWND_TOPMOST)
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        Err(Win32Error::Other("非 Windows 平台不支持置顶".into()))
    }
}

/// 取消窗口置顶（锚定 `HWND_NOTOPMOST` 归还普通层）。
pub fn set_notopmost(hwnd: isize) -> Result<(), Win32Error> {
    if !is_window_alive(hwnd) {
        return Err(Win32Error::InvalidWindow);
    }
    #[cfg(windows)]
    {
        set_window_pos(hwnd, HWND_NOTOPMOST)
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        Err(Win32Error::Other("非 Windows 平台不支持取消置顶".into()))
    }
}

/// 单条受管窗口条目（排序算法的输入形态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainEntry {
    /// 窗口句柄裸值。
    pub hwnd: isize,
    /// 置顶优先级（1~9，1 最顶层）。
    pub priority: u8,
    /// 操作世代号：每次置顶 / 改级自增；同优先级下世代号小者在上
    /// （先操作者先入链，后操作者排在其后）。
    pub seq: u64,
}

/// 按（priority 升序, seq 升序）稳定排序受管窗口，产出链式应用顺序。
///
/// # 纯函数（无 FFI）
pub fn order_entries(mut entries: Vec<ChainEntry>) -> Vec<ChainEntry> {
    entries.sort_by_key(|entry| (entry.priority, entry.seq));
    entries
}

/// 链式应用的锚定规划：第 1 项锚 `HWND_TOPMOST`，第 i 项锚上一项。
///
/// # 纯函数（无 FFI）
pub fn chain_apply_plan(ordered: &[ChainEntry]) -> Vec<(isize, bool)> {
    // bool = 是否锚定在“上一个窗口之后”（true）；false = 锚定绝对置顶层。
    ordered
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.hwnd, index > 0))
        .collect()
}

/// 前台抢占纠偏：规划需要重刷的受保护窗口及其新锚点。
///
/// # 语义
/// 用户激活了 `activated`（受管窗口，且不在链条首位）→ 系统已把它顶到绝对顶层。
/// 本函数取链条中位于 `activated` **之前**、优先级 ≤ [`SHIELD_PRIORITY_MAX`]（即
/// 1 / 2 级）的窗口，按其相对链序规划：首窗口回锚 `HWND_TOPMOST`，后续窗口顺次
/// 锚在前一受保护窗口之后——把被压下去的 1 / 2 级窗口重新顶回（全程不激活，
/// 焦点仍留在用户点中的窗口上）。
///
/// 返回 `None` 表示无需动作：`activated` 不在受管列表 / 就是链条首位 /
/// 前方没有 1~2 级窗口。
///
/// # 纯函数（无 FFI）
pub fn plan_shield_refresh(ordered: &[ChainEntry], activated: isize) -> Option<Vec<(isize, bool)>> {
    let activated_pos = ordered.iter().position(|entry| entry.hwnd == activated)?;
    if activated_pos == 0 {
        return None; // 激活的已是链条首位（1 级），无需纠偏
    }
    let protected: Vec<ChainEntry> = ordered[..activated_pos]
        .iter()
        .copied()
        .filter(|entry| entry.priority <= SHIELD_PRIORITY_MAX)
        .collect();
    if protected.is_empty() {
        return None;
    }
    Some(chain_apply_plan(&protected))
}

/// 按一次链式应用规划逐窗口执行 `SetWindowPos`。
///
/// 每个窗口前置 `IsWindow` 校验；失效窗口跳过（调用方随后统一清理）；UIPI
/// 拦截（`AccessDenied`）单独计数上报，不中断其余窗口的定位。
pub fn apply_plan(plan: &[(isize, bool)]) -> ChainApplyStats {
    let mut stats = ChainApplyStats::default();
    let mut last_applied: Option<isize> = None;
    for &(hwnd, anchor_after_prev) in plan {
        if !is_window_alive(hwnd) {
            stats.invalid += 1;
            continue;
        }
        #[cfg(windows)]
        let insert_after = if anchor_after_prev {
            match last_applied {
                Some(prev) => to_hwnd(prev),
                // 上一窗口失效被跳过：退回绝对置顶层，保持链条连续。
                None => HWND_TOPMOST,
            }
        } else {
            HWND_TOPMOST
        };
        #[cfg(not(windows))]
        let insert_after: Option<HWND> = None;
        #[cfg(not(windows))]
        let _ = anchor_after_prev;

        #[cfg(windows)]
        let outcome = set_window_pos(hwnd, insert_after);
        #[cfg(not(windows))]
        let outcome: Result<(), Win32Error> = Err(Win32Error::Other("非 Windows".into()));

        match outcome {
            Ok(()) => {
                stats.applied += 1;
                last_applied = Some(hwnd);
            }
            Err(Win32Error::InvalidWindow) => stats.invalid += 1,
            Err(Win32Error::AccessDenied) => {
                stats.denied += 1;
                stats.denied_hwnds.push(hwnd);
            }
            Err(Win32Error::Other(_)) => stats.failed += 1,
        }
    }
    stats
}

/// 便捷入口：给定已排序受管条目，直接执行整条优先级链的完整重刷。
pub fn apply_chain(ordered: &[ChainEntry]) -> ChainApplyStats {
    let plan = chain_apply_plan(ordered);
    apply_plan(&plan)
}

/// 一次链式应用的统计结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChainApplyStats {
    /// 成功重定位的窗口数。
    pub applied: usize,
    /// 前置 `IsWindow` 校验失败（窗口已销毁）的窗口数。
    pub invalid: usize,
    /// 遭遇 UIPI 拦截（`AccessDenied`）的窗口数。
    pub denied: usize,
    /// 其余原因失败的窗口数。
    pub failed: usize,
    /// 被 UIPI 拦截的具体窗口（供模块层点名提示提权）。
    pub denied_hwnds: Vec<isize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(hwnd: isize, priority: u8, seq: u64) -> ChainEntry {
        ChainEntry {
            hwnd,
            priority,
            seq,
        }
    }

    // -----------------------------------------------------------------------
    // 优先级边界夹紧（1~9）
    // -----------------------------------------------------------------------

    #[test]
    fn priority_clamping_keeps_bounds() {
        assert_eq!(clamp_priority(0), 1);
        assert_eq!(clamp_priority(1), 1);
        assert_eq!(clamp_priority(5), 5);
        assert_eq!(clamp_priority(9), 9);
        assert_eq!(clamp_priority(10), 9);
        assert_eq!(clamp_priority(255), 9);
    }

    // -----------------------------------------------------------------------
    // 优先级升序排列（1 级最前、9 级最后）
    // -----------------------------------------------------------------------

    #[test]
    fn ordering_puts_lower_priority_number_first() {
        let sorted = order_entries(vec![
            entry(0x30, 9, 0),
            entry(0x10, 1, 0),
            entry(0x20, 5, 0),
            entry(0x18, 2, 0),
        ]);
        let order: Vec<isize> = sorted.iter().map(|e| e.hwnd).collect();
        assert_eq!(
            order,
            vec![0x10, 0x18, 0x20, 0x30],
            "应按优先级升序（1 最前）"
        );
    }

    #[test]
    fn ordering_is_stable_within_same_priority_by_seq() {
        let sorted = order_entries(vec![
            entry(0x40, 3, 2),
            entry(0x10, 3, 0),
            entry(0x20, 3, 1),
            entry(0x30, 1, 9), // 1 级无论 seq 多大都在最前
        ]);
        let order: Vec<isize> = sorted.iter().map(|e| e.hwnd).collect();
        assert_eq!(
            order,
            vec![0x30, 0x10, 0x20, 0x40],
            "同优先级按世代号升序（先操作者在上）；1 级恒最前"
        );
    }

    // -----------------------------------------------------------------------
    // 链式应用规划（锚定目标）
    // -----------------------------------------------------------------------

    #[test]
    fn chain_plan_anchors_first_to_topmost_rest_after_previous() {
        let ordered = order_entries(vec![
            entry(0x10, 1, 0),
            entry(0x20, 2, 0),
            entry(0x30, 3, 0),
        ]);
        let plan = chain_apply_plan(&ordered);
        assert_eq!(
            plan,
            vec![(0x10, false), (0x20, true), (0x30, true)],
            "第 1 项锚 HWND_TOPMOST（false），后续各项锚上一窗口（true）"
        );
    }

    #[test]
    fn empty_chain_produces_empty_plan() {
        assert!(chain_apply_plan(&[]).is_empty());
        assert_eq!(apply_chain(&[]), ChainApplyStats::default());
    }

    // -----------------------------------------------------------------------
    // 前台抢占纠偏规划（防下压）
    // -----------------------------------------------------------------------

    /// 激活 3 级窗口后，链条前部的 1 / 2 级窗口应被规划重刷。
    #[test]
    fn shield_refresh_reclaims_levels_1_and_2_after_lower_activation() {
        let ordered = order_entries(vec![
            entry(0x10, 1, 0),
            entry(0x20, 2, 0),
            entry(0x30, 3, 0), // 用户激活 3 级
            entry(0x40, 1, 0), // 另一个 1 级也在激活窗口之前
        ]);
        // 排序后链序：0x10(1) → 0x40(1) → 0x20(2) → 0x30(3)（同优先级按 seq / 插入序稳定）。
        let chain_order: Vec<isize> = ordered.iter().map(|e| e.hwnd).collect();
        assert_eq!(chain_order, vec![0x10, 0x40, 0x20, 0x30]);
        let plan = plan_shield_refresh(&ordered, 0x30).expect("应有纠偏动作");
        assert_eq!(
            plan,
            vec![(0x10, false), (0x40, true), (0x20, true)],
            "1/2 级窗口按相对链序重刷：首项回锚置顶层，后续依次锚前一受保护窗口"
        );
    }

    /// 激活链条首位（1 级）时无需任何纠偏（它已在最顶层）。
    #[test]
    fn shield_refresh_is_noop_when_top_entry_activated() {
        let ordered = order_entries(vec![entry(0x10, 1, 0), entry(0x30, 3, 0)]);
        assert_eq!(plan_shield_refresh(&ordered, 0x10), None);
    }

    /// 激活不受管窗口（外部窗口）时不触发纠偏。
    #[test]
    fn shield_refresh_is_noop_for_unmanaged_window() {
        let ordered = order_entries(vec![entry(0x10, 1, 0), entry(0x30, 3, 0)]);
        assert_eq!(plan_shield_refresh(&ordered, 0x9999), None);
    }

    /// 激活窗口前方没有 ≤2 级窗口（如前方只有 3 级）时不触发纠偏。
    #[test]
    fn shield_refresh_is_noop_without_protected_predecessors() {
        let ordered = order_entries(vec![entry(0x30, 3, 0), entry(0x50, 5, 0)]);
        assert_eq!(
            plan_shield_refresh(&ordered, 0x50),
            None,
            "前方 3/5 级窗口不在 1~2 级保护带内"
        );
    }

    /// 纠偏重刷的锚定与整链应用一致（复用同一规划器，回归防线）。
    #[test]
    fn shield_plan_reuses_chain_anchor_rule() {
        let ordered = order_entries(vec![
            entry(0x10, 1, 0),
            entry(0x11, 1, 1),
            entry(0x30, 3, 0),
        ]);
        let plan = plan_shield_refresh(&ordered, 0x30).expect("应有纠偏动作");
        assert_eq!(plan, vec![(0x10, false), (0x11, true)]);
    }

    // -----------------------------------------------------------------------
    // v0.4.1 死尸复活回归防线：取消置顶后剩余链条重排
    // -----------------------------------------------------------------------

    /// 被移除（取消置顶）的窗口绝不再出现在任何链条规划中——15ms 纠偏定时器 /
    /// 整链重刷输入的都是移除后的快照，无法复活已取消的窗口。
    #[test]
    fn removed_window_is_absent_from_all_chain_plans() {
        // 链条：0x10(1) → 0x20(2) → 0x30(3)；取消 0x20 后。
        let before = order_entries(vec![
            entry(0x10, 1, 0),
            entry(0x20, 2, 0),
            entry(0x30, 3, 0),
        ]);
        let remaining: Vec<ChainEntry> =
            before.iter().copied().filter(|e| e.hwnd != 0x20).collect();

        let plan = chain_apply_plan(&remaining);
        assert_eq!(
            plan,
            vec![(0x10, false), (0x30, true)],
            "移除后首项锚置顶层、后续锚前一窗口——链条连续且不含已取消窗口"
        );
        assert!(
            !plan.iter().any(|(hwnd, _)| *hwnd == 0x20),
            "已取消的窗口不得进入任何重排规划"
        );

        // 纠偏规划同样不得触碰已取消窗口：激活 0x30 时保护带只含 0x10。
        let shield = plan_shield_refresh(&remaining, 0x30).expect("应有纠偏动作");
        assert_eq!(shield, vec![(0x10, false)]);
        assert!(
            !shield.iter().any(|(hwnd, _)| *hwnd == 0x20),
            "纠偏规划不得包含已取消窗口"
        );
    }
}
