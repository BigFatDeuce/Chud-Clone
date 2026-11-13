//! Drives the full-screen interface with keys, on image files only (never a disk),
//! and renders every screen into an in-memory terminal.

use super::app::{App, Modal, Options, Screen};
use super::view;
use chud_clone_core::clone::FinalStatus;
use chud_clone_core::i18n::{Lang, set_lang};
use chud_clone_core::scan::ScanEnd;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn fixture(dir: &Path) -> PathBuf {
    let gz = Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/tests/fixtures/gpt-win.img.gz");
    let out = dir.join("source.img");
    let mut dec = flate2::read::GzDecoder::new(std::fs::File::open(gz).unwrap());
    let mut f = std::fs::File::create(&out).unwrap();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = dec.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n]).unwrap();
    }
    out
}

fn press(app: &mut App, code: KeyCode) {
    app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn typ(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c));
    }
}

fn wait(app: &mut App, what: &str, cond: impl Fn(&App) -> bool) {
    let start = Instant::now();
    loop {
        app.pump();
        if cond(app) {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(300), "timeout waiting for {what}");
        app.tick += 1;
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn render(app: &mut App, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| view::draw(f, app)).unwrap();
    let buf = term.backend().buffer().clone();
    let mut s = String::new();
    for y in 0..h {
        for x in 0..w {
            s.push_str(buf[(x, y)].symbol());
        }
        s.push('\n');
    }
    s
}

/// Renders at the smallest supported size and at a large one: no panic, text present.
fn render_all_sizes(app: &mut App, expect: &str) -> String {
    let small = render(app, 60, 18);
    let big = render(app, 140, 45);
    assert!(big.contains(expect), "screen does not contain {expect:?}:\n{big}");
    let _ = small;
    let shot = render(app, 80, 25);
    // CHUD_TUI_DUMP=1 cargo test -p chud-clone-cli -- --nocapture: prints every screen.
    if std::env::var_os("CHUD_TUI_DUMP").is_some() {
        println!("{shot}");
    }
    shot
}

#[test]
fn clone_scan_and_screens_on_images() {
    set_lang(Lang::En);
    let dir = std::env::temp_dir().join(format!("chud-tui-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = fixture(&dir);
    let dst = dir.join("clone.img");
    let reports = dir.join("reports");
    let mut app = App::new(Options {
        reports: reports.clone(),
        images: vec![src.clone()],
        list_disks: false,
        only_virtual: true,
        winpe: false,
        lang_file: None,
    });
    wait(&mut app, "disk list", |a| a.busy.is_none());
    assert_eq!(app.entries.len(), 1);
    render_all_sizes(&mut app, "1/4: SOURCE disk");

    // Analysis and health of the image (read-only).
    press(&mut app, KeyCode::Char('d'));
    wait(&mut app, "analysis", |a| a.busy.is_none());
    assert_eq!(app.screen, Screen::Text);
    render_all_sizes(&mut app, "Boot: Windows (UEFI / GPT)");
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('h'));
    wait(&mut app, "SMART", |a| a.busy.is_none());
    assert_eq!(app.screen, Screen::Text);
    render_all_sizes(&mut app, "Health data unavailable");
    press(&mut app, KeyCode::Esc);

    // Source, then a new image file as destination.
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen, Screen::PickDest);
    render_all_sizes(&mut app, "Image file (new or existing)");
    press(&mut app, KeyCode::Enter);
    assert!(matches!(app.modal, Some(Modal::Input { .. })));
    typ(&mut app, &dst.to_string_lossy());
    render_all_sizes(&mut app, "Clone to an image file");
    press(&mut app, KeyCode::Enter);
    wait(&mut app, "destination", |a| a.busy.is_none());
    let shot = render(&mut app, 100, 30);
    assert_eq!(app.screen, Screen::Options, "{shot}");
    render_all_sizes(&mut app, "Copy method");
    // Change an option back and forth, then analyse.
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Left);
    assert!(app.settings.optimize);
    press(&mut app, KeyCode::Enter);
    wait(&mut app, "preparation", |a| a.busy.is_none());
    let shot = render(&mut app, 100, 30);
    assert_eq!(app.screen, Screen::Review, "{shot}");
    assert!(!app.prep.as_ref().unwrap().blocked);
    render_all_sizes(&mut app, "Type ERASE then Enter");
    assert!(!dst.exists(), "nothing may be written before the confirmation");

    // A wrong word does not start anything.
    typ(&mut app, "ERAS");
    press(&mut app, KeyCode::Enter);
    assert!(matches!(app.modal, Some(Modal::Message { .. })));
    assert_eq!(app.screen, Screen::Review);
    press(&mut app, KeyCode::Enter);
    typ(&mut app, "e");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen, Screen::Running);
    render_all_sizes(&mut app, "Cancel the clone");
    // Esc asks before cancelling; "Keep going" is the default.
    press(&mut app, KeyCode::Esc);
    assert!(matches!(app.modal, Some(Modal::Confirm { .. })));
    press(&mut app, KeyCode::Enter);
    assert!(app.modal.is_none());
    wait(&mut app, "end of the clone", |a| a.screen == Screen::Result);
    let text = render_all_sizes(&mut app, "Report:");
    assert_eq!(app.final_status(), Some(FinalStatus::SuccessVerified), "{text}");
    let id = app.report.as_ref().unwrap().id.clone();
    for ext in ["json", "txt", "html", "log"] {
        assert!(reports.join(format!("{id}.{ext}")).exists(), "{ext} report missing");
    }

    // Back to the list, then a surface scan of the clone's source.
    press(&mut app, KeyCode::Enter);
    wait(&mut app, "disk list", |a| a.busy.is_none());
    assert_eq!(app.screen, Screen::Home);
    press(&mut app, KeyCode::Char('t'));
    render_all_sizes(&mut app, "Read-only: nothing is written");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen, Screen::Scan);
    wait(&mut app, "end of the scan", |a| a.scan.as_ref().is_some_and(|s| s.result.is_some() || s.error.is_some()));
    let scan = app.scan.as_ref().unwrap();
    assert_eq!(scan.result.as_ref().map(|r| r.end.clone()), Some(ScanEnd::Completed), "{:?}", scan.error);
    render_all_sizes(&mut app, "Surface map");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen, Screen::Home);

    // French, then leaving through the exit menu.
    press(&mut app, KeyCode::Char('l'));
    render_all_sizes(&mut app, "1/4 : disque SOURCE");
    press(&mut app, KeyCode::Esc);
    render_all_sizes(&mut app, "Quitter Chud Clone");
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.exit, None, "\"Back\" must not leave");
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.exit, Some(super::app::exit::QUIT));
    set_lang(Lang::En);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn destination_rules() {
    let dir = std::env::temp_dir().join(format!("chud-tui-rules-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = fixture(&dir);
    let mut app = App::new(Options {
        reports: dir.join("reports"),
        images: vec![src],
        list_disks: false,
        only_virtual: true,
        winpe: true,
        lang_file: Some(dir.join("lang.txt")),
    });
    wait(&mut app, "disk list", |a| a.busy.is_none());
    // A physical disk is refused with --only-virtual, and the running system always.
    let mut e = app.entries[0].clone();
    e.info.media = chud_clone_core::model::MediaKind::Ssd;
    assert!(app.dest_refusal(&e).is_some());
    e.info.media = chud_clone_core::model::MediaKind::Image;
    assert!(app.dest_refusal(&e).is_none());
    e.info.is_system = true;
    assert!(app.dest_refusal(&e).is_some());
    // The WinPE exit menu offers restart, shut down, prompt and the classic menu.
    let codes: Vec<u8> = app.exit_items().iter().map(|(_, c)| *c).collect();
    assert_eq!(codes, vec![super::app::exit::REBOOT, super::app::exit::SHUTDOWN, super::app::exit::SHELL, super::app::exit::CLASSIC_MENU]);
    // A source image that does not exist is reported, not added.
    press(&mut app, KeyCode::Char('f'));
    typ(&mut app, &dir.join("missing.img").to_string_lossy());
    press(&mut app, KeyCode::Enter);
    wait(&mut app, "image", |a| a.busy.is_none());
    assert!(matches!(app.modal, Some(Modal::Message { .. })));
    assert_eq!(app.entries.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
