//! Messages en anglais : un clonage complet (image → nouvelle image plus grande, par la
//! même voie que l'interface et la ligne de commande) produit un résultat, des constats et
//! des rapports entièrement en anglais.
//!
//! Binaire de test séparé : la langue est un réglage global du processus, elle ne doit pas
//! changer pendant les tests en français des autres fichiers.

mod common;

use chud_clone_core::engine::{Control, EngineOptions, NullObserver};
use chud_clone_core::i18n::{Lang, set_lang};
use chud_clone_core::job::{JobConfig, Target, execute, prepare};
use chud_clone_core::plan::PlanOptions;
use chud_clone_core::report::{render_html, render_txt};

fn has_french(s: &str) -> Option<&'static str> {
    // Mots et caractères qui n'apparaissent pas dans un texte anglais.
    const WORDS: [&str; 10] = ["é", "è", "à", "ç", "«", " disque", "Clonage", "partition agrandie", "Rapport", "octets"];
    if let Some(w) = WORDS.into_iter().find(|w| s.contains(w)) {
        return Some(w);
    }
    // Unités françaises après un nombre (« 12 Mo »), sans confondre avec « Cascadia Mono ».
    let b = s.as_bytes();
    for unit in [" Ko", " Mo", " Go", " To"] {
        for (i, _) in s.match_indices(unit) {
            let after = b.get(i + 3).copied().unwrap_or(b' ');
            if i > 0 && b[i - 1].is_ascii_digit() && !after.is_ascii_alphanumeric() {
                return Some("French size unit");
            }
        }
    }
    None
}

#[test]
fn whole_job_speaks_english() {
    set_lang(Lang::En);
    let src = common::fixture("gpt-win.img");
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("english");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dst = dir.join("clone.img");
    let size = std::fs::metadata(&src).unwrap().len();
    let cfg = JobConfig {
        source: Target::Image(src.clone()),
        destination: Target::Image(dst.clone()),
        plan: PlanOptions::default(),
        engine: EngineOptions::default(),
        new_image_size: Some(size * 2),
        allow_unvalidated_physical_write: false,
        expected_source_fingerprint: None,
        expected_destination_fingerprint: None,
        app_paths: vec![],
    };

    let prep = prepare(&cfg, false).unwrap();
    assert!(!prep.blocked, "{:#?}", prep.findings);
    for f in &prep.findings {
        assert!(has_french(&f.title).is_none() && has_french(&f.detail).is_none(), "French in finding: {f:#?}");
    }
    assert!(prep.plan.grow.is_some(), "growth should be planned");

    let reports = dir.join("reports");
    let rep = execute(&cfg, &reports, &Control::new(), &NullObserver).unwrap();
    assert_eq!(rep.outcome.status.label(), "Clone succeeded and verified", "{:#?}", rep.outcome.reasons);
    assert!(rep.outcome.reasons.iter().any(|r| r.contains("was grown from")), "{:#?}", rep.outcome.reasons);
    for r in &rep.outcome.reasons {
        assert!(has_french(r).is_none(), "French in reason: {r}");
    }
    for e in &rep.log {
        assert!(has_french(&e.message).is_none(), "French in log: {}", e.message);
    }

    let txt = render_txt(&rep);
    assert!(txt.contains("CLONE REPORT") && txt.contains("RESULT"), "{txt}");
    if let Some(w) = has_french(&txt) {
        panic!("French ({w}) in the text report:\n{txt}");
    }
    let html = render_html(&rep);
    assert!(html.contains("lang=\"en\"") && html.contains("<h2>Method</h2>"));
    if let Some(w) = has_french(&html) {
        panic!("French ({w}) in the HTML report");
    }
}
