//! Chud Clone — disk cloning and diagnostics engine.
//!
//! This crate holds all the logic that is independent of the interface: device
//! access, partition tables, file systems, planning, copy, verification,
//! diagnostics and reports.
//!
//! Every message meant for the user exists in French and in English
//! (`t!` / `tf!`, language chosen with `i18n::set_lang`).

/// Fixed bilingual text: `t!("français", "English")` → `&'static str` in the active language.
#[macro_export]
macro_rules! t {
    ($fr:literal, $en:literal) => {
        $crate::i18n::pick($fr, $en)
    };
}

/// Formatted bilingual text: `tf!("{n} disque", "{n} disk")` → `String`, like `format!`.
/// Both formats receive the same arguments.
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

pub mod awake;
pub mod boot;
pub mod bootprep;
pub mod clone;
pub mod device;
pub mod diag;
pub mod engine;
pub mod format;
pub mod fs;
pub mod guid;
pub mod health;
pub mod image;
pub mod job;
pub mod model;
pub mod partition;
pub mod plan;
pub mod precheck;
pub mod readahead;
pub mod report;
pub mod scan;
pub mod smart;
pub mod testing;
#[cfg(windows)]
pub mod win;

pub const APP_NAME: &str = "Chud Clone";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
