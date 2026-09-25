//! 构建脚本：嵌入图标与版本信息资源

fn main() {
    // icon.rc 位于 src/ 下，嵌入到最终 exe
    let rc = std::path::Path::new("src").join("icon.rc");
    if rc.exists() {
        embed_resource::compile("src/icon.rc", embed_resource::NONE);
        println!("cargo:rerun-if-changed=src/icon.rc");
        println!("cargo:rerun-if-changed=assets/csv_editor_icon.ico");
        println!("cargo:rerun-if-changed=csv_editor_icon.ico"); // main.rs include_bytes 内嵌
    }
}
