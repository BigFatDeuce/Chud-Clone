//! Modèle commun des constats de diagnostic.
//!
//! Chaque contrôle (table de partitions, système de fichiers, SMART, pré-vérification,
//! copie, vérification) produit des [`Finding`]. La gravité décrit l'importance du
//! constat ; `blocking` indique séparément s'il interdit de démarrer le clonage. Un
//! disque source en fin de vie est « critique » mais n'interdit pas le clonage — c'est
//! au contraire le moment de le cloner.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Information : rien d'anormal, ou remarque mineure.
    Info,
    /// Avertissement : anomalie détectée, clonage potentiellement risqué.
    Warning,
    /// Erreur : problème nécessitant une intervention.
    Error,
    /// Critique : disque potentiellement défaillant ou opération dangereuse.
    Critical,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Info => t!("Information", "Information"),
            Severity::Warning => t!("Avertissement", "Warning"),
            Severity::Error => t!("Erreur", "Error"),
            Severity::Critical => t!("Critique", "Critical"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    /// Code stable (ex. `GPT_BACKUP_INVALID`), utile pour les journaux et les tests.
    pub code: String,
    pub title: String,
    pub detail: String,
    /// Vrai si ce constat interdit de démarrer le clonage.
    pub blocking: bool,
    /// Portée : « source », « destination », « partition 3 »…
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Position concernée (LBA en secteurs logiques), lorsque connue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lba: Option<u64>,
}

impl Finding {
    pub fn new(severity: Severity, code: &str, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { severity, code: code.to_string(), title: title.into(), detail: detail.into(), blocking: false, scope: None, lba: None }
    }
    pub fn info(code: &str, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(Severity::Info, code, title, detail)
    }
    pub fn warning(code: &str, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(Severity::Warning, code, title, detail)
    }
    pub fn error(code: &str, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(Severity::Error, code, title, detail)
    }
    pub fn critical(code: &str, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(Severity::Critical, code, title, detail)
    }
    pub fn blocking(mut self) -> Self {
        self.blocking = true;
        self
    }
    pub fn scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }
    pub fn at_lba(mut self, lba: u64) -> Self {
        self.lba = Some(lba);
        self
    }
}

/// Gravité maximale d'une liste de constats.
pub fn max_severity(findings: &[Finding]) -> Option<Severity> {
    findings.iter().map(|f| f.severity).max()
}

pub fn has_blocking(findings: &[Finding]) -> bool {
    findings.iter().any(|f| f.blocking)
}

/// Formate une taille en unités binaires lisibles (Go/GB = 2^30, comme l'Explorateur Windows).
pub fn human_size(bytes: u64) -> String {
    const UNITS_FR: [&str; 5] = ["o", "Ko", "Mo", "Go", "To"];
    const UNITS_EN: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let units = if crate::i18n::is_fr() { UNITS_FR } else { UNITS_EN };
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < units.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        tf!("{bytes} o", "{bytes} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", units[u])
    } else if v >= 10.0 {
        format!("{v:.1} {}", units[u])
    } else {
        format!("{v:.2} {}", units[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_ordering() {
        assert!(Severity::Critical > Severity::Error);
        assert!(Severity::Error > Severity::Warning);
        assert!(Severity::Warning > Severity::Info);
        let f = vec![Finding::info("A", "a", ""), Finding::error("B", "b", "").blocking()];
        assert_eq!(max_severity(&f), Some(Severity::Error));
        assert!(has_blocking(&f));
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_size(512), "512 o");
        assert_eq!(human_size(1024 * 1024), "1.00 Mo");
        assert_eq!(human_size(500 * 1024 * 1024 * 1024), "500 Go");
    }
}
