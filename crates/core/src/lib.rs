//! Chud Clone — moteur de clonage et de diagnostic de disques.
//!
//! Ce crate contient toute la logique indépendante de l'interface : accès aux
//! périphériques, tables de partitions, systèmes de fichiers, planification, copie,
//! vérification, diagnostic et rapports.
//!
//! Tous les messages destinés à l'utilisateur existent en français et en anglais
//! (`t!` / `tf!`, langue choisie par `i18n::set_lang`).

/// Texte fixe bilingue : `t!("français", "English")` → `&'static str` dans la langue active.
#[macro_export]
macro_rules! t {
    ($fr:literal, $en:literal) => {
        $crate::i18n::pick($fr, $en)
    };
}

/// Texte mis en forme bilingue : `tf!("{n} disque", "{n} disk")` → `String`, comme `format!`.
/// Les deux formats reçoivent les mêmes arguments.
#[macro_export]
macro_rules! tf {
    ($fr:literal, $en:literal $(,)?) => {
        if $crate::i18n::is_fr() { format!($fr) } else { format!($en) }
    };
    ($fr:literal, $en:literal, $($arg:tt)+) => {
        if $crate::i18n::is_fr() { format!($fr, $($arg)+) } else { format!($en, $($arg)+) }
    };
}

pub mod i18n;

pub mod boot;
pub mod clone;
pub mod device;
pub mod diag;
pub mod engine;
pub mod fs;
pub mod guid;
pub mod health;
pub mod image;
pub mod job;
pub mod model;
pub mod partition;
pub mod plan;
pub mod precheck;
pub mod report;
pub mod scan;
pub mod smart;
pub mod testing;
#[cfg(windows)]
pub mod win;

pub const APP_NAME: &str = "Chud Clone";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
