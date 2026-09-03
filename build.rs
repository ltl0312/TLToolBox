//! TLToolBox · Slint UI 编译脚本。
//!
//! 把 `ui/app.slint`（单栏「桌面实用工具箱」界面）编译进二进制目标；`src/main.rs` 经
//! `slint::include_modules!()` 引入生成的 `MainWindow` / `ModuleItem` 类型。
//!
//! 语法目标说明：`ui/app.slint` 按 Slint 1.9 语言规范书写（兼容本仓库 Cargo.lock
//! 锁定的 1.17.x，改动须保持双向可编译：不引入 1.9 之后新增的语法，也不使用已被
//! 后续版本弃用的旧 API）。若界面资产在 1.17 编译期被报告弃用/语法错误，须以
//! 1.9 规范的兼容写法修正 `.slint`，而非在编译脚本中规避。

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=ui/app.slint");

    // 阶段四启用点：把声明式界面编译为 Rust 代码（此前阶段保留空实现，仅保证
    // `cargo build` 全链路可编译；UI 资产就绪后即启用）。
    slint_build::compile("ui/app.slint").expect("Slint UI 编译失败");
}
