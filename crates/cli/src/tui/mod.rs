//! Full-screen keyboard interface (text mode): `chud-clone-cli tui`.
//!
//! Made for the WinPE rescue key, where the graphical application (WebView2) cannot
//! run, but it works in any Windows console. Same engine and same checks as the
//! application and the command line: everything is re-validated before writing, and
//! the destination is erased only after the confirmation word has been typed.

mod app;
mod view;

pub use app::Options;

use app::App;
use chud_clone_core::tf;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use std::io::Stdout;
use std::time::Duration;

fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen, ratatui::crossterm::cursor::Show);
}

/// Runs the interface until the user leaves it; returns the exit code (see [`exit`]).
pub fn run(opts: Options) -> Result<u8, String> {
    let err = |e: std::io::Error| tf!("console inutilisable : {e}", "console unusable: {e}");
    enable_raw_mode().map_err(err)?;
    if let Err(e) = execute!(std::io::stdout(), EnterAlternateScreen) {
        restore();
        return Err(err(e));
    }
    // A panic on the interface thread must give the console back. Worker threads
    // report their own failures through the interface: leave the screen alone then.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some("main") {
            restore();
        }
        previous(info);
    }));
    let mut terminal = match Terminal::new(CrosstermBackend::new(std::io::stdout())) {
        Ok(t) => t,
        Err(e) => {
            restore();
            return Err(err(e));
        }
    };
    let mut app = App::new(opts);
    let result = event_loop(&mut terminal, &mut app);
    restore();
    result
}

fn event_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<u8, String> {
    let io = |e: std::io::Error| tf!("erreur de console : {e}", "console error: {e}");
    loop {
        app.pump();
        terminal.draw(|f| view::draw(f, app)).map_err(io)?;
        if let Some(code) = app.exit {
            return Ok(code);
        }
        if event::poll(Duration::from_millis(100)).map_err(io)? {
            match event::read().map_err(io)? {
                // Windows also reports key releases: only presses (and repeats) count.
                Event::Key(k) if k.kind != KeyEventKind::Release => app.on_key(k),
                Event::Resize(..) => {
                    let _ = terminal.clear();
                }
                _ => {}
            }
        }
        app.tick = app.tick.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests;
