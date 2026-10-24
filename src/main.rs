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
use crossterm::event::Event;
use crossterm::{execute, terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen}};
use ratatui::backend::CrosstermBackend;

fn read_events(tx: std::sync::mpsc::Sender<Event>) {
    loop {
        match crossterm::event::read() {
            Ok(ev) => {
                if tx.send(ev).is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

/// Default profile from `connections.toml`, or the first one when nothing is
/// marked `default`; its read-only and timeout flags come with it.
fn ru_sqlmux_load_default() -> Result<config::ConnDef, String> {
    let defs = config::Conn::load_all()?;
    let at = defs.iter().position(|c| c.default.unwrap_or(false)).unwrap_or(0);
    defs.into_iter()
        .nth(at)
        .ok_or_else(|| "no saved connections".to_string())
}

/// Restores the terminal on every exit path; without it a crash leaves the
/// shell without echo or cursor.
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
        Some(dsn) => app.enqueue_connect(&dsn, None),
        None => {
            if let Ok(def) = ru_sqlmux_load_default() {
                app.enqueue_connect(&def.dsn, Some(&def));
            }
        }
    }

    run(terminal, rx, done_rx, &mut app).await;
    Ok(())
}

/// Polls at 20 Hz; the CPU cost is negligible and a slow server still shows a
/// spinner.
async fn run(
    mut terminal: ratatui::Terminal<CrosstermBackend<Stdout>>,
    rx: Receiver<Event>,
    mut done_rx: tokio::sync::mpsc::UnboundedReceiver<(u64, app::Done)>,
    app: &mut App,
) {
    loop {
        apply_jobs(&mut done_rx, app);
        // a job that stopped reporting must not block the UI
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

// a result whose job was replaced is dropped
fn apply_jobs(
    done_rx: &mut tokio::sync::mpsc::UnboundedReceiver<(u64, app::Done)>,
    app: &mut App,
) {
    while let Ok((id, done)) = done_rx.try_recv() {
        app.apply_job(id, done);
    }
}

/// True means the user asked to quit.
fn handle_input(rx: &Receiver<Event>, app: &mut App) -> bool {
    while let Ok(ev) = rx.try_recv() {
        if let Some(Out::Quit) = app.handle(ev) {
            return true;
        }
    }
    false
}
