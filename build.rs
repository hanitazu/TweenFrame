//! Embeds the app icon and version info into the .exe.
//! Needs the Windows SDK resource compiler (installed with the MSVC build
//! tools); without it the build still succeeds, just without an exe icon.

#[path = "src/icon_raster.rs"]
#[allow(dead_code)]
mod icon_raster;

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/icon_raster.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico = out.join("tweenframe.ico");
    std::fs::write(&ico, icon_raster::ico_file(&[16, 20, 24, 32, 40, 48, 64, 128])).unwrap();

    let v = |k: &str| std::env::var(k).unwrap_or_else(|_| "0".into());
    let (major, minor, patch) =
        (v("CARGO_PKG_VERSION_MAJOR"), v("CARGO_PKG_VERSION_MINOR"), v("CARGO_PKG_VERSION_PATCH"));
    let version = v("CARGO_PKG_VERSION");
    let rc = format!(
        r#"1 ICON "{ico}"
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "TweenFrame"
      VALUE "FileDescription", "TweenFrame - Frame Generation for FiveM"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "TweenFrame"
      VALUE "LegalCopyright", "MIT License"
      VALUE "OriginalFilename", "TweenFrame.exe"
      VALUE "ProductName", "TweenFrame"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        ico = ico.display().to_string().replace('\\', "/"),
    );
    let rc_path = out.join("tweenframe.rc");
    std::fs::write(&rc_path, rc).unwrap();
    if let Err(e) = embed_resource::compile(&rc_path, embed_resource::NONE).manifest_optional() {
        println!("cargo:warning=could not embed the exe icon: {e}");
    }
}
