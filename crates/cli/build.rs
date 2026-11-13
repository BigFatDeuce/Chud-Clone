//! Lie statiquement le runtime de Visual Studio (vcruntime) et garde l'UCRT dynamique,
//! comme l'application graphique (tauri-build) : l'exécutable ne dépend plus de
//! VCRUNTIME140.dll, absente d'un Windows neuf et de WinPE. L'UCRT fait partie de
//! Windows 10/11 et de WinPE.

fn main() {
    static_vcruntime::metabuild();
}
