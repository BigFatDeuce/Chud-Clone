//! Links the Visual Studio runtime (vcruntime) statically and keeps the UCRT dynamic,
//! like the graphical application (tauri-build): the executable no longer depends on
//! VCRUNTIME140.dll, missing from a fresh Windows and from WinPE. The UCRT is part of
//! Windows 10/11 and of WinPE.

fn main() {
    static_vcruntime::metabuild();
}
