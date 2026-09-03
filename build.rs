//! TLToolBox · 构建脚本。
//!
//! 职责：
//!   1. **Slint UI 编译**：把 `ui/app.slint`（单栏「桌面实用工具箱」界面）编译进
//!      二进制目标；`src/main.rs` 经 `slint::include_modules!()` 引入生成的
//!      `MainWindow` / `ModuleItem` 类型（原有职责，保持不变）；
//!   2. **Windows 平台资源嵌入**：经 `winresource`（winres 的维护版分支）把
//!      `app.manifest` 编译为 RT_MANIFEST（资源 ID 1）内嵌进 exe，携带
//!      asInvoker 执行级别、Per-Monitor V2 DPI 感知与 Common-Controls v6
//!      声明；`res/app.ico` 图标为预留位——文件一旦放入即自动嵌入。
//!
//! 语法目标说明：`ui/app.slint` 按 Slint 1.9 语言规范书写（兼容本仓库 Cargo.lock
//! 锁定的 1.17.x，改动须保持双向可编译：不引入 1.9 之后新增的语法，也不使用已被
//! 后续版本弃用的旧 API）。若界面资产在 1.17 编译期被报告弃用/语法错误，须以
//! 1.9 规范的兼容写法修正 `.slint`，而非在编译脚本中规避。

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=ui/app.slint");

    // ---- Windows 平台资源嵌入（清单 + 预留图标）。构建脚本以包根目录为 CWD，
    //      故下述资源路径均相对包根解析；非 Windows 目标（如交叉编译）整体跳过，
    //      与 Cargo.toml 中 target.'cfg(windows)'.build-dependencies 的裁剪一致。
    #[cfg(windows)]
    embed_windows_resources();

    slint_build::compile("ui/app.slint").expect("Slint UI 编译失败");
}

/// Windows 目标专属：把应用清单（及就绪后的应用图标）嵌入最终 exe。
///
/// - `app.manifest`：必需，缺失即构建失败（清单语义见该文件头部注释）；
/// - `res/app.ico`：可选预留位，文件存在即嵌入为 exe 默认图标（建议含
///   16/24/32/48/64/128/256 等多尺寸 ICO 帧）；缺失时跳过、不影响构建。
///
/// 产出资源段：RT_MANIFEST(1) +（可选）ICON(1)。`winresource` 内部生成 .rc
/// 后调用 MSVC 工具链的 rc.exe（按 Windows SDK 标准路径自动发现）编译，最终
/// 由链接器并入 exe。
#[cfg(windows)]
fn embed_windows_resources() {
    use winresource::WindowsResource;

    println!("cargo:rerun-if-changed=app.manifest");

    if !std::path::Path::new("app.manifest").exists() {
        panic!("缺少 app.manifest（应用清单），请将其置于包根目录（与 Cargo.toml 同级）");
    }

    let mut res = WindowsResource::new();

    // 1) 应用清单（UAC asInvoker / Per-Monitor V2 DPI / Common-Controls v6）。
    res.set_manifest_file("app.manifest");

    // 2) 应用图标（预留位）：res/app.ico 一旦放入即自动嵌入，无需再改本脚本。
    let icon = std::path::Path::new("res/app.ico");
    if icon.exists() {
        println!("cargo:rerun-if-changed=res/app.ico");
        println!("cargo:warning=检测到 res/app.ico，已随构建嵌入应用图标");
        res.set_icon("res/app.ico");
    }

    res.compile()
        .expect("嵌入 Windows 应用资源（app.manifest / app.ico）失败");
}
