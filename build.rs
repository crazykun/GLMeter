//! Windows 目标：把 assets/icon.ico 嵌入 exe 资源段，
//! 使 exe 文件 / 任务栏 / 开始菜单快捷方式显示应用图标。
//! winresource 为纯 Rust 实现，无需 rc.exe / windres。
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=嵌入图标失败（不影响功能）: {e}");
        }
    }
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");
}
