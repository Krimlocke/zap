fn main() {
    // Embeds assets/zap.ico as the exe icon and as resource 1 for the tray icon.
    embed_resource::compile("zap.rc", embed_resource::NONE)
        .manifest_optional()
        .unwrap();
    println!("cargo:rerun-if-changed=assets/zap.ico");
}
