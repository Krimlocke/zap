use std::path::PathBuf;

/// Generates and embeds the Windows resources: the two tray icons (the first is
/// also the exe icon) and version details from Cargo.toml for Explorer's
/// Properties > Details tab and Task Manager.
fn main() {
    let root = std::env::var("CARGO_MANIFEST_DIR").unwrap().replace('\\', "/");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let numeric = version
        .split(['.', '-', '+'])
        .take(3)
        .map(|p| p.parse::<u16>().unwrap_or(0).to_string())
        .collect::<Vec<_>>()
        .join(",");

    let rc = format!(
        r#"1 ICON "{root}/assets/zap.ico"
2 ICON "{root}/assets/zap-paused.ico"

1 VERSIONINFO
FILEVERSION {numeric},0
PRODUCTVERSION {numeric},0
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "FileDescription", "Zap"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "zap"
      VALUE "LegalCopyright", "MIT License. Based on Caffeine, (c) 2020 Kyle Leong"
      VALUE "OriginalFilename", "zap.exe"
      VALUE "ProductName", "Zap"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("zap.rc");
    std::fs::write(&out, rc).unwrap();
    embed_resource::compile(&out, embed_resource::NONE)
        .manifest_optional()
        .unwrap();

    println!("cargo:rerun-if-changed=assets/zap.ico");
    println!("cargo:rerun-if-changed=assets/zap-paused.ico");
}
