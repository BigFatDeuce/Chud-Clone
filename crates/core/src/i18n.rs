//! Langue des messages (français / anglais).
//!
//! La langue est un réglage global du processus : l'interface et la ligne de commande la
//! fixent au démarrage (choix de l'utilisateur, sinon langue de Windows). Les rapports et
//! diagnostics sont rédigés dans la langue active au moment où ils sont produits.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Fr,
    En,
}

impl Lang {
    /// `fr`, `fr-FR`, `en-US`… ; toute autre langue donne l'anglais.
    pub fn from_tag(tag: &str) -> Lang {
        if tag.trim().to_ascii_lowercase().starts_with("fr") { Lang::Fr } else { Lang::En }
    }

    pub fn code(self) -> &'static str {
        match self {
            Lang::Fr => "fr",
            Lang::En => "en",
        }
    }
}

// Français par défaut (langue d'origine du moteur et des tests) ; les programmes la fixent.
static LANG: AtomicU8 = AtomicU8::new(0);

pub fn set_lang(lang: Lang) {
    LANG.store(if lang == Lang::Fr { 0 } else { 1 }, Ordering::Relaxed);
}

pub fn lang() -> Lang {
    if LANG.load(Ordering::Relaxed) == 0 { Lang::Fr } else { Lang::En }
}

pub fn is_fr() -> bool {
    lang() == Lang::Fr
}

pub fn pick(fr: &'static str, en: &'static str) -> &'static str {
    if is_fr() { fr } else { en }
}

/// Langue de l'interface de Windows (ou `LANG` ailleurs) : français si elle l'est, sinon anglais.
pub fn system_lang() -> Lang {
    #[cfg(windows)]
    {
        // SAFETY: fonction sans paramètre.
        let id = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
        // Langue principale : 10 bits de poids faible ; 0x0C = français.
        if id & 0x3ff == 0x0c { Lang::Fr } else { Lang::En }
    }
    #[cfg(not(windows))]
    {
        std::env::var("LANG").map(|l| Lang::from_tag(&l)).unwrap_or(Lang::En)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags() {
        assert_eq!(Lang::from_tag("fr-FR"), Lang::Fr);
        assert_eq!(Lang::from_tag("FR"), Lang::Fr);
        assert_eq!(Lang::from_tag("en-US"), Lang::En);
        assert_eq!(Lang::from_tag("de"), Lang::En);
        assert_eq!(Lang::Fr.code(), "fr");
    }

    #[test]
    fn capture_in_macros() {
        // Le format reçoit les identifiants capturés et les arguments explicites.
        let n = 3;
        let s = tf!("{n} disques, {}", "{n} disks, {}", "x");
        assert!(s == "3 disques, x" || s == "3 disks, x");
        let f: &str = t!("oui", "yes");
        assert!(f == "oui" || f == "yes");
    }
}
