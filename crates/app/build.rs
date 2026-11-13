//! Tauri build step. Release builds embed a Windows manifest that makes the application
//! always start with administrator rights (see `windows-app-manifest.xml`).
//!
//! Debug builds keep Tauri's default manifest: the manifest is also linked into the test
//! executables, which `cargo test` could not start without elevation. A debug build
//! started without rights offers to restart as administrator from its title bar.

fn main() {
    println!("cargo:rerun-if-changed=windows-app-manifest.xml");
    let mut attrs = tauri_build::Attributes::new();
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        attrs = attrs.windows_attributes(tauri_build::WindowsAttributes::new().app_manifest(include_str!("windows-app-manifest.xml")));
    }
    tauri_build::try_build(attrs).expect("failed to run the Tauri build script");
}
