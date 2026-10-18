mod app;
mod config;
mod console;
mod db;
mod edit;
mod grid;
mod ui;

use std::io::Stdout;
use std::sync::mpsc::Receiver;

use app::{App, Out};
use crossterm::event::{Event, KeyCode};
use crossterm::{execute, terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen}};
use ratatui::backend::CrosstermBackend;

fn read_events(tx: std::sync::mpsc::Sender<Event>) {
    loop {
        match crossterm::event::read() {
            Ok(ev) => {
                let ctrl_c = matches!(
                    &ev,
                    Event::Key(k) if k.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                        && k.code == KeyCode::Char('c')
                );
                if tx.send(ev).is_err() || ctrl_c {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

/// Default DSN from `~/.config/ru-sqlmux/connections.toml` (entry with
/// `default = true`, else the only/first entry). Errs when there is none, in
/// which case the app shows the connect form.
fn ru_sqlmux_load_default() -> Result<String, String> {
    let defs = config::Conn::load_all()?;
    let hit = defs
        .iter()
        .find(|c| c.default.unwrap_or(false))
        .or_else(|| defs.first())
        .ok_or("no saved connections")?;
    Ok(hit.dsn.clone())
}

/// Restores the terminal on every exit path, including a panic or an early
/// error after raw mode is on. Without it a crash leaves the shell without
/// echo, cursor, or the alternate screen.
struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut stdout = std::io::stdout();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen)?;
    let _guard = TermGuard;
    let terminal = ratatui::Terminal::new(CrosstermBackend::new(stdout))?;
    let (tx, rx): (_, Receiver<Event>) = std::sync::mpsc::channel();
    std::thread::spawn(move || read_events(tx));

    let dsn = std::env::args().nth(1);
    let (done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(done_tx);
    match dsn {
        Some(dsn) => app.enqueue_connect(&dsn),
        None => {
            // no arg: use the default entry from connections.toml when present
            if let Ok(defs) = ru_sqlmux_load_default() {
                app.enqueue_connect(&defs);
            }
        }
    }

    run(terminal, rx, done_rx, &mut app).await;
    Ok(())
}

/// Event pump: background job results, keys, then a redraw, so a slow server
/// shows a spinner instead of a frozen screen.
/// shortcut: polls at 20 Hz instead of a wake-based channel; the CPU cost is
/// negligible on desktops. upgrade = tokio-friendly event stream behind a
/// feature flag.
async fn run(
    mut terminal: ratatui::Terminal<CrosstermBackend<Stdout>>,
    rx: Receiver<Event>,
    mut done_rx: tokio::sync::mpsc::UnboundedReceiver<(u64, app::Done)>,
    app: &mut App,
) {
    loop {
        apply_jobs(&mut done_rx, app);
        // backstop: a job that stopped reporting must not block the UI
        app.tick();
        // a mode change or resize needs every cell repainted, not a diff
        if app.need_clear {
            app.need_clear = false;
            let _ = terminal.clear();
        }
        if let Err(err) = terminal.draw(|f| app.draw(f)) {
            app.err = format!("draw: {err}");
        }
        if handle_input(&rx, app) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

// drain finished jobs; a stale result (its job was replaced) is dropped
fn apply_jobs(
    done_rx: &mut tokio::sync::mpsc::UnboundedReceiver<(u64, app::Done)>,
    app: &mut App,
) {
    while let Ok((id, done)) = done_rx.try_recv() {
        app.apply_job(id, done);
    }
}

/// Handle every queued input event. True means the user asked to quit.
fn handle_input(rx: &Receiver<Event>, app: &mut App) -> bool {
    while let Ok(ev) = rx.try_recv() {
        if let Some(Out::Quit) = app.handle(ev) {
            return true;
        }
    }
    false
}
