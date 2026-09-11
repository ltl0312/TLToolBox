//! # FFI 回调 panic 边界（v0.6.1 · S5 整改）
//!
//! 本模块提供**唯一**的 FFI 回调 panic 兜底原语 [`guard_ffi`]，供全部
//! `unsafe extern "system" fn` 回调入口复用。
//!
//! # 为什么必须有它
//!
//! 现代 Rust 中 panic 一旦试图跨 `extern "system"` 边界展开，标准库会直接
//! **abort 整个进程**（`panic in a function that cannot unwind`）。对本项目这类
//! **常驻桌面工具**而言，这是最不可接受的失败模式：托盘图标、WinEvent 钩子、
//! 剪贴板监听、终端日志钩子全部随之丢失，用户既看不到 Toast 也查不到日志。
//!
//! 而钩子回调内部天然包含「可 panic」的操作——`String::from_utf16_lossy` 分配、
//! `format!`、`std::sync::Mutex` 加锁、`tracing!` 宏、`Vec` 增长等。纪律（注释里
//! 写的"严禁 panic 跨 FFI 展开"）没有强制力，因此这里用**代码**把它变成强制约束：
//! 每个回调入口都经由 [`guard_ffi`] 包裹，panic 被就地截停在 Rust 侧。
//!
//! # 语义
//!
//! - 正常返回 `Some(value)`；
//! - 闭包 panic 时返回 `None`，并尽力写下一条 `error` 级日志（记录回调位置），
//!   **调用方须提供无害的降级返回值**（如 `DefWindowProcW` 的转发结果、`TRUE`
//!   继续枚举、或直接放弃本次事件）。
//!
//! 注：`Cargo.toml` 刻意保留 `panic = "unwind"`（release profile），
//! `catch_unwind` 在此构建形态下有效；若未来改为 `panic = "abort"`，本模块的
//! 兜底将失效——该约束已在 `Cargo.toml` 的 profile 注释中声明。

/// 在 FFI 回调边界内执行闭包，捕获 panic，避免其跨 `extern "system"` 展开。
///
/// # 参数
/// - `context`：回调标识（如 `"popup_blocker::win_event_proc"`），用于日志定位；
/// - `f`：回调原始逻辑；使用 [`std::panic::AssertUnwindSafe`] 包裹——回调内部
///   持有的多为裸指针 / `Cell` / 短临界 `Mutex`，panic 后调用方会放弃本次事件，
///   不存在"观察到中间不一致状态后被复用"的路径，故该断言在此处成立。
///
/// # 返回
/// panic 时返回 `None`；调用方必须给出无害降级值（见模块文档）。
#[must_use]
pub fn guard_ffi<R>(context: &'static str, f: impl FnOnce() -> R) -> Option<R> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(value) => Some(value),
        Err(_) => {
            // panic 载荷可能在 drop 时二次 panic，且日志订阅者自身也可能 panic；
            // 这里再包一层 catch_unwind，保证"兜底本身永不再成爆点"。
            let _ = std::panic::catch_unwind(|| {
                tracing::error!(
                    target: "ffi_guard",
                    "FFI 回调 panic 已捕获（{context}）：已就地截停，进程继续运行"
                );
            });
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_through_normal_return_value() {
        assert_eq!(guard_ffi("test::ok", || 42u32), Some(42));
        assert_eq!(guard_ffi("test::ok_unit", || ()), Some(()));
    }

    /// 核心契约：闭包 panic **不得**向外传播（否则跨 FFI 边界将 abort 进程）。
    #[test]
    fn swallows_panic_and_returns_none() {
        let outcome = guard_ffi("test::panic", || -> u32 {
            panic!("模拟回调内部 panic");
        });
        assert_eq!(outcome, None, "panic 必须被就地捕获并降级为 None");
    }

    /// panic 载荷是 `String`（格式化 panic，如 `panic!("{detail}")`）时同样截停。
    #[test]
    fn swallows_panic_with_string_payload() {
        let outcome = guard_ffi("test::panic_string", || {
            let detail = String::from("载荷 panic");
            panic!("{detail}");
        });
        assert_eq!(outcome, None::<()>);
    }

    /// 截停后调用方仍可继续执行后续逻辑（验真"进程存活"语义）。
    #[test]
    fn caller_keeps_running_after_panic() {
        let mut hits = 0;
        for _ in 0..3 {
            let _ = guard_ffi("test::loop", || panic!("每轮都炸"));
            hits += 1;
        }
        assert_eq!(hits, 3, "每轮 panic 都被截停，循环未被中断");
    }
}
