use crate::config::Conn;
use crate::console::Console;
use crate::db::{self, Engine};
use crate::edit::Edit;
use crate::grid::{self, GridState, PendingUi};
use crate::ui;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use std::sync::Arc;
use tui_textarea::TextArea;

const PAGE: usize = 100;
const NONE_SORT: usize = usize::MAX;
const HELP: &str = r#"
 TAB        cycle focus (sidebar → grid → SQL → results)
 ?          this help          C-c quit
 Sidebar    j/k move  / filter  Enter open  Esc clear filter
 Grid       h/l j/k move  g/G start/end  [ ] page  s sort column
            e edit cell  o insert row  d (un)delete row
            C-s save all as one transaction  R refresh
 SQL        Enter run  Alt-Enter newline  Alt-Up/Down history
 Results    j/k scroll  Enter back to grid
"#;

#[derive(Clone, Copy, PartialEq)]
pub enum Focus {
    Form,
    Sidebar,
    Grid,
    Editor,
    Results,
}

#[derive(Clone)]
pub struct TableData {
    pub name: String,
    pub cols: Vec<db::Col>,
    pub pk_idx: Option<usize>,
    /// current page of cast-text rows
    pub rows: Vec<Vec<String>>,
}

pub struct CellEdit {
    ta: TextArea<'static>,
    pub row: usize,
    pub col: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum Confirm {
    Refresh,
    Quit,
}

/// What the global key layer did with a key.
enum GlobalKey {
    Quit,
    Handled,
    Pass,
}

pub enum Out {
    Quit,
}

pub struct OpenPayload {
    pub td: TableData,
    pub page: usize,
    pub total: usize,
    pub approx: bool,
}

/// Result of a background database job. Everything that can block (connect,
/// catalogue reads, page fetch, count, save, SQL) runs on a spawned task and
/// reports back here, so the UI keeps drawing and reading keys meanwhile.
pub enum Done {
    Connected {
        engine: Arc<Engine>,
        tables: Vec<String>,
        label: String,
        notice: Option<String>,
    },
    /// table list reload; `opened` re-opens the table that was on screen
    Refreshed {
        tables: Vec<String>,
        opened: Option<OpenPayload>,
    },
    Opened(OpenPayload),
    Page {
        rows: Vec<Vec<String>>,
        total: usize,
        approx: bool,
    },
    Saved {
        msg: String,
        /// exactly what the job wrote: (pk, col, new value) edits it consumed
        edits: Vec<(String, String, String)>,
        dels: Vec<String>,
        ghosts: usize,
    },
    Sql(db::RunResult),
    Failed(String),
}

pub struct App {
    form_ta: TextArea<'static>,
    pub engine: Option<Arc<Engine>>,
    done_tx: tokio::sync::mpsc::UnboundedSender<Done>,
    /// a background job is in flight (status line shows a spinner)
    pub busy: bool,
    tables: Vec<String>,
    /// Some(text) while the sidebar filter is being typed
    filt_in: Option<String>,
    filtered: Vec<String>,
    sb_sel: usize,
    pub td: Option<TableData>,
    pub focus: Focus,
    pub grid: GridState,
    pub page: usize,
    pub total: usize,
    /// total is a catalogue estimate, not an exact count
    pub approx: bool,
    sort_col: usize,
    sort_desc: bool,
    pub con: Console,
    pub results: Option<db::RunResult>,
    pub res: GridState,
    pub pending: PendingUi,
    cell: Option<CellEdit>,
    confirm: Option<Confirm>,
    pub msg: String,
    pub err: String,
    pub help: bool,
    /// database actually opened (shown in the sidebar title)
    label: String,
    /// next frame must repaint every cell: the screen changed shape (form vs
    /// main UI) or the terminal was resized, and diffed painting would leave
    /// the previous screen showing through
    pub need_clear: bool,
}

impl App {
    pub fn new(done_tx: tokio::sync::mpsc::UnboundedSender<Done>) -> Self {
        let mut form_ta = TextArea::default();
        form_ta.set_placeholder_text("postgres://… · mysql://… · ./file.db  (Enter connect, C-c quit)");
        Self {
            form_ta,
            engine: None,
            done_tx,
            busy: false,
            tables: Vec::new(),
            filt_in: None,
            filtered: Vec::new(),
            sb_sel: 0,
            td: None,
            focus: Focus::Form,
            grid: GridState::default(),
            page: 0,
            total: 0,
            approx: false,
            sort_col: NONE_SORT,
            sort_desc: false,
            con: Console::new(),
            results: None,
            res: GridState::default(),
            pending: PendingUi::default(),
            cell: None,
            confirm: None,
            msg: String::new(),
            err: String::new(),
            help: false,
            label: String::new(),
            need_clear: false,
        }
    }

    /// Kick off a connect on a background task; returns immediately so the form
    /// stays visible and redraws while the server is contacted.
    pub fn enqueue_connect(&mut self, dsn: &str) {
        if self.busy {
            return;
        }
        let dsn = dsn.to_string();
        self.err.clear();
        self.msg.clear();
        self.busy = true;
        let tx = self.done_tx.clone();
        tokio::spawn(async move {
            let done = match Conn::from_dsn(&dsn) {
                Err(err) => Done::Failed(err),
                Ok(conn) => match Engine::connect(&conn).await {
                    Err(err) => Done::Failed(err),
                    Ok(engine) => {
                        let engine = Arc::new(engine);
                        // show the database actually opened: Postgres falls back
                        // to a DB named after the user when the DSN omits one
                        let label = match conn.kind {
                            crate::config::EngineKind::Sqlite => {
                                conn.url.trim_start_matches("sqlite:").to_string()
                            }
                            _ => {
                                let db = engine.db_label().await;
                                if db.is_empty() {
                                    conn.db_name.clone().unwrap_or_else(|| "?".into())
                                } else {
                                    db
                                }
                            }
                        };
                        let notice = (conn.db_name.is_none()
                            && conn.kind != crate::config::EngineKind::Sqlite)
                            .then(|| {
                                format!(
                                    "connected to `{label}`. The DSN has no /database name; add /<db> to it"
                                )
                            });
                        match engine.tables().await {
                            Ok(tables) => {
                                Done::Connected { engine, tables, label, notice }
                            }
                            Err(err) => Done::Failed(format!("list tables: {err}")),
                        }
                    }
                },
            };
            let _ = tx.send(done);
        });
    }

    fn refresh_filtered(&mut self) {
        self.filtered = match &self.filt_in {
            Some(s) => self
                .tables
                .iter()
                .filter(|t| t.to_ascii_lowercase().contains(&s.to_ascii_lowercase()))
                .cloned()
                .collect(),
            None => self.tables.clone(),
        };
        self.sb_sel = self.sb_sel.min(self.filtered.len().saturating_sub(1));
    }

    /// Open a table (columns + first page + count) on a background task.
    fn enqueue_open(&mut self, name: &str) {
        let Some(engine) = self.engine.clone() else { return };
        if self.busy {
            return;
        }
        self.err.clear();
        self.busy = true;
        let name = name.to_string();
        let tx = self.done_tx.clone();
        tokio::spawn(async move {
            let done = fetch_open(&engine, &name, 0, None).await;
            let _ = tx.send(done);
        });
    }

    /// Fetch the current page of the current table with the current sort.
    fn enqueue_page(&mut self) {
        let (Some(engine), Some(td)) = (self.engine.clone(), self.td.clone()) else { return };
        if self.busy {
            return;
        }
        self.err.clear();
        self.busy = true;
        let order = if self.sort_col == NONE_SORT {
            None
        } else {
            Some((td.cols[self.sort_col].name.clone(), self.sort_desc))
        };
        let page = self.page;
        let tx = self.done_tx.clone();
        tokio::spawn(async move {
            let done = match fetch_open(&engine, &td.name, page, order).await {
                Done::Opened(p) => Done::Page { rows: p.td.rows, total: p.total, approx: p.approx },
                other => other,
            };
            let _ = tx.send(done);
        });
    }

    /// Reload the table list; reopen the current table when there is one.
    fn enqueue_refresh(&mut self) {
        let engine = self.engine.clone();
        let Some(engine) = engine else { return };
        if self.busy {
            return;
        }
        self.err.clear();
        self.busy = true;
        let reopen = self.td.as_ref().map(|td| td.name.clone());
        let tx = self.done_tx.clone();
        tokio::spawn(async move {
            let done = match engine.tables().await {
                Err(err) => Done::Failed(format!("list tables: {err}")),
                Ok(tables) => match reopen {
                    None => Done::Refreshed { tables, opened: None },
                    Some(name) => match fetch_open(&engine, &name, 0, None).await {
                        Done::Opened(opened) => Done::Refreshed { tables, opened: Some(opened) },
                        other => other,
                    },
                },
            };
            let _ = tx.send(done);
        });
    }

    fn page_move(&mut self, delta: isize) {
        if self.td.is_none() {
            return;
        }
        let pages = self.total.div_ceil(PAGE);
        let next = (self.page as isize + delta).clamp(0, pages.saturating_sub(1) as isize);
        if next as usize == self.page {
            return;
        }
        self.page = next as usize;
        self.enqueue_page();
    }

    fn cycle_sort(&mut self) {
        if self.td.is_none() {
            return;
        }
        if self.sort_col != self.grid.sel.1 || self.sort_col == NONE_SORT {
            self.sort_col = self.grid.sel.1;
            self.sort_desc = false;
        } else if !self.sort_desc {
            self.sort_desc = true;
        } else {
            self.sort_col = NONE_SORT;
            self.sort_desc = false;
        }
        self.enqueue_page();
    }

    /// Apply a finished background job to the UI state.
    pub fn apply(&mut self, done: Done) {
        self.busy = false;
        let was_connected = self.engine.is_some();
        match done {
            Done::Connected { engine, tables, label, notice } => {
                self.apply_connected(engine, tables, label, notice)
            }
            Done::Refreshed { tables, opened } => self.apply_refreshed(tables, opened),
            Done::Opened(p) => self.apply_opened(p),
            Done::Page { rows, total, approx } => self.apply_page(rows, total, approx),
            Done::Saved { msg, edits, dels, ghosts } => {
                self.apply_saved(msg, edits, dels, ghosts)
            }
            Done::Sql(rr) => self.apply_sql(rr),
            Done::Failed(err) => self.apply_failed(err),
        }
        if was_connected != self.engine.is_some() {
            // form and main UI occupy different shapes; repaint from scratch
            self.need_clear = true;
        }
    }

    fn apply_connected(
        &mut self,
        engine: Arc<Engine>,
        tables: Vec<String>,
        label: String,
        notice: Option<String>,
    ) {
        self.tables = tables;
        self.refresh_filtered();
        self.label = label;
        self.engine = Some(engine);
        self.focus = Focus::Sidebar;
        self.msg = notice.unwrap_or_else(|| format!("connected ({})", self.label));
    }

    fn apply_refreshed(&mut self, tables: Vec<String>, opened: Option<OpenPayload>) {
        self.tables = tables;
        self.refresh_filtered();
        self.pending = PendingUi::default();
        if let Some(p) = opened {
            self.set_opened(p);
        }
    }

    fn apply_opened(&mut self, p: OpenPayload) {
        self.pending = PendingUi::default();
        self.sort_col = NONE_SORT;
        self.sort_desc = false;
        self.set_opened(p);
    }

    fn apply_page(&mut self, rows: Vec<Vec<String>>, total: usize, approx: bool) {
        self.total = total;
        self.approx = approx;
        if let Some(td) = self.td.as_mut() {
            td.rows = rows;
            if self.total > 0 && td.rows.is_empty() {
                self.err = format!(
                    "0 rows returned but the table reports {}: wrong page, wrong schema, or RLS",
                    self.total
                );
            }
        }
        self.grid.home();
    }

    fn apply_saved(
        &mut self,
        msg: String,
        consumed: Vec<(String, String, String)>,
        consumed_dels: Vec<String>,
        consumed_ghosts: usize,
    ) {
        self.msg = msg;
        // remove only what this job wrote: anything typed while it was in flight
        // stays pending, re-based onto the saved value
        for (pk, col, saved) in consumed {
            self.forget_consumed_edit(&pk, &col, &saved);
        }
        for pk in consumed_dels {
            self.pending.deletes.remove(&pk);
        }
        if consumed_ghosts > 0 && self.pending.drafts.len() >= consumed_ghosts {
            self.pending.drafts.drain(0..consumed_ghosts);
        }
        self.enqueue_page();
    }

    /// Drop one consumed edit, or re-base it when the user typed again mid-save.
    fn forget_consumed_edit(&mut self, pk: &str, col: &str, saved: &str) {
        let Some(m) = self.pending.edits.get_mut(pk) else { return };
        if let Some((orig, cur)) = m.get_mut(col) {
            if cur == saved {
                m.remove(col);
            } else {
                *orig = saved.to_string();
            }
        }
        if m.is_empty() {
            self.pending.edits.remove(pk);
        }
    }

    fn apply_sql(&mut self, rr: db::RunResult) {
        self.results = Some(rr);
        self.res.home();
        self.focus = Focus::Results;
    }

    fn apply_failed(&mut self, err: String) {
        self.err = err;
        if self.engine.is_none() {
            self.focus = Focus::Form;
        }
    }

    fn set_opened(&mut self, p: OpenPayload) {
        self.td = Some(p.td);
        self.page = p.page;
        self.total = p.total;
        self.approx = p.approx;
        self.grid.home();
        if self.total > 0 && self.td.as_ref().is_some_and(|t| t.rows.is_empty()) {
            self.err = format!(
                "0 rows returned but the table reports {}: wrong page, wrong schema, or RLS",
                self.total
            );
        }
    }

    fn editable_pk(&self) -> Option<(usize, &str)> {
        let td = self.td.as_ref()?;
        Some((td.pk_idx?, td.pk_idx.and_then(|p| td.cols.get(p)).map(|c| c.name.as_str())?))
    }

    fn grid_len(&self) -> usize {
        self.td.as_ref().map_or(0, |td| td.rows.len()) + self.pending.drafts.len()
    }

    fn pk_of_row(&self, row: usize) -> Option<String> {
        let td = self.td.as_ref()?;
        let (pk_idx, _) = self.editable_pk()?;
        let cells = td.rows.get(row)?;
        Some(cells.get(pk_idx).cloned().unwrap_or_default())
    }

    /// pending edit value for (row,col) if any, else the page value
    fn cell_value(&self, row: usize, col: usize) -> String {
        let td = self.td.as_ref().unwrap();
        let (pk_idx, _) = self.editable_pk().unwrap_or((usize::MAX, ""));
        let pk = td.rows.get(row).and_then(|r| r.get(pk_idx).cloned()).unwrap_or_default();
        let base = td.rows.get(row).and_then(|r| r.get(col)).cloned().unwrap_or_default();
        if let Some((_, new)) = self
            .pending
            .edits
            .get(&pk)
            .and_then(|m| m.get(&td.cols[col].name))
        {
            return new.clone();
        }
        base
    }

    fn open_cell(&mut self) {
        let (row, col) = self.grid.sel;
        if row >= self.td.as_ref().map_or(0, |td| td.rows.len()) {
            // ghost row: only the PK cell is editable before save
            let Some(td) = self.td.as_ref() else { return };
            let Some(pkidx) = td.pk_idx else {
                self.err = "adding rows needs a table with a single-column primary key".into();
                return;
            };
            if col != pkidx {
                self.msg = "ghost rows: save first (C-s), then edit".into();
                return;
            }
        }
        let Some(td) = self.td.as_ref() else { return };
        if td.pk_idx.is_none() {
            self.err = "read-only: this table has no single-column primary key".into();
            return;
        }
        let ta = TextArea::new(vec![]);
        self.cell = Some(CellEdit { ta, row, col });
    }

    fn accept_cell(&mut self) {
        let Some(cell) = self.cell.take() else { return };
        let (row, col) = (cell.row, cell.col);
        let new: String = cell.ta.lines().join("\n");
        let td = self.td.as_ref().unwrap();
        if row >= td.rows.len() {
            // ghost pk draft
            let gi = row - td.rows.len();
            if gi < self.pending.drafts.len() {
                self.pending.drafts[gi] = new;
            }
            return;
        }
        let Some(pk) = self.pk_of_row(row) else { return };
        let col_name = td.cols[col].name.clone();
        let col_orig = td
            .rows
            .get(row)
            .and_then(|r| r.get(col))
            .cloned()
            .unwrap_or_default();
        let m = self.pending.edits.entry(pk.clone()).or_default();
        if new == col_orig {
            // reverted: drop the entry; empty maps cleaned at save
            m.remove(&col_name);
            if m.is_empty() {
                self.pending.edits.remove(&pk);
            }
        } else {
            m.insert(col_name, (col_orig, new));
        }
    }

    fn toggle_delete(&mut self) {
        let row = self.grid.sel.0;
        if self.pk_of_row(row).is_none_or(|s| s.is_empty()) {
            if row >= self.td.as_ref().map_or(0, |td| td.rows.len()) {
                // delete a ghost: just remove it locally
                let gi = row - self.td.as_ref().unwrap().rows.len();
                self.pending.drafts.remove(gi);
                
                self.grid.end(self.grid_len());
                return;
            }
            self.err = "cannot delete a row with a NULL primary key".into();
            return;
        }
        let pk = self.pk_of_row(row).unwrap();
        if !self.pending.deletes.remove(&pk) {
            self.pending.deletes.insert(pk);
        }
    }

    fn add_ghost(&mut self) {
        if self.editable_pk().is_none() {
            self.err = "adding rows needs a table with a single-column primary key".into();
            return;
        }
        self.pending.drafts.push(String::new());
        self.grid.end(self.grid_len());
    }

    fn enqueue_save(&mut self) {
        self.err.clear();
        let Some(engine) = self.engine.clone() else { return };
        let Some(td) = self.td.as_ref() else { return };
        let Some((_, pk_name)) = self.editable_pk() else {
            return;
        };
        if self.busy {
            return;
        }
        let edits: Vec<Edit> = self
            .pending
            .edits
            .iter()
            .filter(|(pk, _)| !pk.is_empty() && !self.pending.deletes.contains(*pk))
            .flat_map(|(pk, m)| {
                m.iter().map(move |(col, (orig, new))| Edit {
                    pk: pk.clone(),
                    col: col.clone(),
                    orig: orig.clone(),
                    new: new.clone(),
                })
            })
            .collect();
        let dels: Vec<String> = self.pending.deletes.iter().cloned().collect();
        let inserts: Vec<Option<String>> = self
            .pending
            .drafts
            .iter()
            .map(|d| if d.is_empty() { None } else { Some(d.clone()) })
            .collect();
        if edits.is_empty() && dels.is_empty() && inserts.is_empty() {
            self.msg = "nothing to save".into();
            return;
        }
        let (name, pk_name) = (td.name.clone(), pk_name.to_string());
        // remember precisely what this job takes, so a save landing later does
        // not wipe edits the user typed while it ran
        let consumed_edits: Vec<(String, String, String)> = edits
            .iter()
            .map(|e| (e.pk.clone(), e.col.clone(), e.new.clone()))
            .collect();
        let consumed_dels = dels.clone();
        let consumed_ghosts = inserts.len();
        self.busy = true;
        let tx = self.done_tx.clone();
        tokio::spawn(async move {
            let done = match engine.save(&name, &pk_name, &edits, &dels, &inserts).await {
                Ok(rep) => Done::Saved {
                    msg: format!(
                        "saved: {} updates, {} deletes{}",
                        rep.updates,
                        rep.deletes,
                        if rep.inserts.is_empty() {
                            String::new()
                        } else {
                            format!(", inserted pk {}", rep.inserts.join(", "))
                        }
                    ),
                    edits: consumed_edits,
                    dels: consumed_dels,
                    ghosts: consumed_ghosts,
                },
                Err(err) => Done::Failed(err),
            };
            let _ = tx.send(done);
        });
    }

    fn enqueue_run_sql(&mut self, sql: &str) {
        let Some(engine) = self.engine.clone() else { return };
        if self.busy {
            self.msg = "busy: a query is still running".into();
            return;
        }
        self.err.clear();
        self.busy = true;
        let sql = sql.to_string();
        let tx = self.done_tx.clone();
        tokio::spawn(async move {
            let done = match engine.run_sql(&sql).await {
                Ok(rr) => Done::Sql(rr),
                Err(err) => Done::Failed(err),
            };
            let _ = tx.send(done);
        });
    }

    fn export_csv(&mut self) {
        let Some(td) = self.td.as_ref() else { return };
        let path = format!("./{}.csv", td.name);
        let esc = |v: &str| {
            if v.contains(',') || v.contains('\"') || v.contains('\n') {
                format!("\"{}\"", v.replace('\"', "\"\""))
            } else {
                v.to_string()
            }
        };
        let mut body = td.cols.iter().map(|c| esc(&c.name)).collect::<Vec<_>>().join(",");
        body.push('\n');
        for row in td.rows.iter() {
            body.push_str(&row.iter().map(|v| esc(v)).collect::<Vec<_>>().join(","));
            body.push('\n');
        }
        match std::fs::write(&path, body) {
            Ok(()) => self.msg = format!("exported page → {path}"),
            Err(e) => self.err = format!("export: {e}"),
        }
    }

    fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Form => Focus::Form,
            Focus::Sidebar => Focus::Grid,
            Focus::Grid => Focus::Editor,
            Focus::Editor => Focus::Results,
            Focus::Results => Focus::Sidebar,
        };
    }

    // ---------------------------------------------------------------- events

    pub fn handle(&mut self, ev: Event) -> Option<Out> {
        match ev {
            Event::Key(k) => self.key(k),
            Event::Mouse(m) => {
                self.mouse(m);
                None
            }
            // a resize invalidates ratatui's diff bookkeeping
            Event::Resize(_, _) => {
                self.need_clear = true;
                None
            }
            _ => None,
        }
    }

    fn key(&mut self, k: KeyEvent) -> Option<Out> {
        if k.kind != crossterm::event::KeyEventKind::Press {
            return None;
        }
        // cell edit overlay eats everything
        if self.cell.is_some() {
            return self.cell_key(k);
        }
        // confirm overlay eats everything
        if let Some(mode) = self.confirm {
            return self.confirm_key(k, mode);
        }
        match self.global_key(&k) {
            GlobalKey::Quit => return Some(Out::Quit),
            GlobalKey::Handled => return None,
            GlobalKey::Pass => {}
        }
        self.focus_key(k)
    }

    /// Dispatch to the focused pane's key handler.
    fn focus_key(&mut self, k: KeyEvent) -> Option<Out> {
        match self.focus {
            Focus::Form => self.form_key(k),
            Focus::Sidebar => self.sidebar_key(k),
            Focus::Grid => self.grid_key(k),
            Focus::Editor => self.editor_key(k),
            Focus::Results => self.results_key(k),
        }
    }

    /// Keys that work from any pane. `Pass` means the focused pane handles it.
    fn global_key(&mut self, k: &KeyEvent) -> GlobalKey {
        match (k.code, k.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => GlobalKey::Quit,
            (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                self.enqueue_save();
                GlobalKey::Handled
            }
            (KeyCode::Char('?'), KeyModifiers::NONE)
                if !matches!(self.focus, Focus::Editor | Focus::Form)
                    && self.filt_in.is_none() =>
            {
                self.help = !self.help;
                GlobalKey::Handled
            }
            (KeyCode::Tab, _) => {
                self.cycle_focus();
                GlobalKey::Handled
            }
            (KeyCode::BackTab, _) => {
                self.focus = match self.focus {
                    Focus::Grid => Focus::Sidebar,
                    Focus::Editor => Focus::Grid,
                    Focus::Results => Focus::Editor,
                    other => other,
                };
                GlobalKey::Handled
            }
            _ => GlobalKey::Pass,
        }
    }

    /// Keys for the cell overlay: Enter commits, Esc cancels, everything else
    /// types into the one-line textarea.
    fn cell_key(&mut self, k: KeyEvent) -> Option<Out> {
        match k.code {
            KeyCode::Enter => self.accept_cell(),
            KeyCode::Esc => self.cell = None,
            _ => {
                if let Some(cell) = self.cell.as_mut() {
                    cell.ta.input(k);
                }
            }
        }
        None
    }

    /// Keys while a confirm box is up. Some(Quit) means yes to the quit prompt;
    /// every other key is consumed by the prompt.
    fn confirm_key(&mut self, k: KeyEvent, mode: Confirm) -> Option<Out> {
        let yes = k.code == KeyCode::Char('y')
            || (k.code == KeyCode::Enter && mode == Confirm::Refresh);
        if yes {
            match mode {
                Confirm::Refresh => {
                    self.confirm = None;
                    self.pending = PendingUi::default();
                    self.enqueue_refresh();
                }
                Confirm::Quit => return Some(Out::Quit),
            }
            return None;
        }
        if matches!(k.code, KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q')) {
            self.confirm = None;
        }
        None
    }

    fn form_key(&mut self, k: KeyEvent) -> Option<Out> {
        match k.code {
            KeyCode::Enter => {
                let dsn = self.form_ta.lines().join("");
                self.enqueue_connect(&dsn);
            }
            _ => {
                self.form_ta.input(k);
            }
        }
        None
    }

    fn sidebar_key(&mut self, k: KeyEvent) -> Option<Out> {
        if self.filt_in.is_some() {
            match k.code {
                KeyCode::Char(c) => self.filt_in.as_mut().unwrap().push(c),
                KeyCode::Backspace => {
                    self.filt_in.as_mut().unwrap().pop();
                }
                KeyCode::Enter => self.filt_in = Some(self.filt_in.take().unwrap_or_default()),
                KeyCode::Esc => self.filt_in = None,
                _ => {}
            }
            self.refresh_filtered();
            return None;
        }
        let len = self.filtered.len();
        match k.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.sb_sel = (self.sb_sel + 1).min(len.saturating_sub(1))
            }
            KeyCode::Char('k') | KeyCode::Up => self.sb_sel = self.sb_sel.saturating_sub(1),
            KeyCode::Char('g') => self.sb_sel = 0,
            KeyCode::Char('G') => self.sb_sel = len.saturating_sub(1),
            KeyCode::Char('/') => self.filt_in = Some(String::new()),
            KeyCode::Char('q') => {
                // don't throw away unsaved work on quit
                if self.dirty() {
                    self.confirm = Some(Confirm::Quit);
                    return None;
                }
                return Some(Out::Quit);
            }
            KeyCode::Enter => {
                if let Some(name) = self.filtered.get(self.sb_sel).cloned() {
                    self.enqueue_open(&name);
                }
            }
            KeyCode::Esc => self.filt_in = None,
            _ => {}
        }
        None
    }

    fn grid_key(&mut self, k: KeyEvent) -> Option<Out> {
        let total_rows = self.grid_len();
        let n_cols = self.td.as_ref().map_or(1, |td| td.cols.len());
        match k.code {
            KeyCode::Char('j') | KeyCode::Down => self.grid.move_row(1, total_rows),
            KeyCode::Char('k') | KeyCode::Up => self.grid.move_row(-1, total_rows),
            KeyCode::Char('h') | KeyCode::Left => self.grid.move_col(-1, n_cols),
            KeyCode::Char('l') | KeyCode::Right => self.grid.move_col(1, n_cols),
            KeyCode::Char('g') => self.grid.home(),
            KeyCode::Char('G') => self.grid.end(total_rows),
            KeyCode::PageDown => self.page_move(1),
            KeyCode::PageUp => self.page_move(-1),
            KeyCode::Char('[') => self.page_move(-1),
            KeyCode::Char(']') => self.page_move(1),
            KeyCode::Char('s') => self.cycle_sort(),
            KeyCode::Char('e') => self.open_cell(),
            KeyCode::Char('o') => self.add_ghost(),
            KeyCode::Char('d') => self.toggle_delete(),
            KeyCode::Char('R') => {
                if self.dirty() {
                    self.confirm = Some(Confirm::Refresh);
                } else {
                    self.enqueue_refresh();
                }
            }
            KeyCode::Char('E') => self.export_csv(),
            KeyCode::Enter => {
                self.focus = Focus::Editor;
            }
            KeyCode::Esc => self.focus = Focus::Sidebar,
            _ => {}
        }
        None
    }

    fn dirty(&self) -> bool {
        !self.pending.edits.is_empty()
            || !self.pending.deletes.is_empty()
            || !self.pending.drafts.is_empty()
    }

    fn editor_key(&mut self, k: KeyEvent) -> Option<Out> {
        match k.code {
            KeyCode::Esc => self.focus = Focus::Grid,
            // recall history: Alt-Up/Down or C-p/C-n (works everywhere)
            KeyCode::Up
                if k.modifiers.contains(KeyModifiers::ALT)
                    || k.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.con.recall(-1)
            }
            KeyCode::Down
                if k.modifiers.contains(KeyModifiers::ALT)
                    || k.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.con.recall(1)
            }
            KeyCode::Enter if k.modifiers.is_empty() => {
                let sql = self.con.sql();
                self.con.push_history(sql.clone());
                self.enqueue_run_sql(&sql);
            }
            // C-f: reformat the buffer with sqlformat
            KeyCode::Char('f') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                let sql = self.con.sql();
                let pretty = sqlformat::format(
                    &sql,
                    &sqlformat::QueryParams::None,
                    &sqlformat::FormatOptions::default(),
                );
                self.con.set_text(&pretty);
            }
            _ => {
                self.con.ta.input(k);
            }
        }
        None
    }

    fn results_key(&mut self, k: KeyEvent) -> Option<Out> {
        let total = self.results.as_ref().map_or(0, |r| r.rows.len());
        match k.code {
            KeyCode::Char('j') | KeyCode::Down => self.res.move_row(1, total),
            KeyCode::Char('k') | KeyCode::Up => self.res.move_row(-1, total),
            KeyCode::Char('g') => self.res.home(),
            KeyCode::Char('G') => self.res.end(total),
            KeyCode::Esc | KeyCode::Enter => self.focus = Focus::Grid,
            _ => {}
        }
        None
    }

    fn mouse(&mut self, m: crossterm::event::MouseEvent) {
        let delta = match m.kind {
            MouseEventKind::ScrollUp => Some(-3),
            MouseEventKind::ScrollDown => Some(3),
            _ => None,
        };
        if let Some(d) = delta {
            match self.focus {
                Focus::Grid => {
                    self.grid.move_row(d, self.grid_len());
                }
                Focus::Sidebar => {
                    let len = self.filtered.len();
                    self.sb_sel =
                        (self.sb_sel as isize + d).clamp(0, len.saturating_sub(1) as isize)
                            as usize;
                }
                Focus::Results => {
                    let total = self.results.as_ref().map_or(0, |r| r.rows.len());
                    self.res.move_row(d, total);
                }
                _ => {}
            }
        }
    }

    // ------------------------------------------------------------------ draw

    pub fn draw(&mut self, f: &mut Frame) {
        if self.engine.is_none() {
            self.draw_form(f);
            return;
        }
        let main_h = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(f.area());
        let main = main_h[1];
        let sb_w = 26u16.min(main.width / 3);
        let row = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(sb_w + 2), Constraint::Min(0)])
            .split(main);
        self.draw_sidebar(f, row[0]);
        let pane = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(55),
                Constraint::Length(if self.focus == Focus::Editor { 6 } else { 4 }),
                Constraint::Min(0),
            ])
            .split(row[1]);
        self.draw_grid(f, pane[0]);
        self.draw_editor(f, pane[1]);
        self.draw_results(f, pane[2]);
        self.draw_status(f, main_h[0]);
        if self.cell.is_some() {
            self.draw_edit_overlay(f);
        }
        if self.confirm.is_some() {
            self.draw_confirm(f);
        }
        if self.help {
            self.draw_help(f);
        }
    }

    fn draw_form(&mut self, f: &mut Frame) {
        let area = centered(f.area(), 60, 6);
        f.render_widget(ratatui::widgets::Clear, area);
        let title = if self.busy { "connecting… (8s budget)" } else { "connect dsn (Enter)" };
        self.form_ta.set_block(ui::block(title, true));
        f.render_widget(&self.form_ta, area);
        let hint = if !self.err.is_empty() {
            Span::styled(self.err.clone(), Style::default().fg(Color::Red))
        } else if self.busy {
            Span::styled("⏳ connecting; keys are queued", Style::default().fg(Color::Yellow))
        } else {
            Span::styled(
                "postgres://user@host/db · mysql://user:env:PW@host/db · ./app.db",
                Style::default().fg(Color::DarkGray),
            )
        };
        let earea = Rect { x: area.x, y: area.y + area.height, width: area.width, height: 1 };
        f.render_widget(Paragraph::new(Line::from(hint)), earea);
    }

    fn draw_sidebar(&mut self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Sidebar;
        let title = match &self.filt_in {
            Some(s) => format!("/ {s}▏"),
            None => {
                if self.label.is_empty() {
                    format!("tables ({})", self.filtered.len())
                } else {
                    format!("tables ({}): {}", self.filtered.len(), self.label)
                }
            }
        };
        let items: Vec<String> = if self.filtered.is_empty() {
            vec!["(none) · / filter, Enter open".into()]
        } else {
            self.filtered
                .clone()
                .into_iter()
                .map(|t| {
                    if Some(&t) == self.filtered.get(self.sb_sel) {
                        format!("▸ {t}")
                    } else {
                        format!("  {t}")
                    }
                })
                .collect()
        };
        let list = ratatui::widgets::List::new(items).block(ui::block(&title, focused));
        let mut ls = ratatui::widgets::ListState::default().with_selected(Some(self.sb_sel));
        f.render_stateful_widget(list, area, &mut ls);
    }

    fn draw_grid(&mut self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Grid;
        let title = match self.td.as_ref() {
            None => "no table".into(),
            Some(td) => {
                let pages = self.total.div_ceil(PAGE).max(1);
                let rows_txt = if self.approx {
                    format!("≈{}", self.total)
                } else {
                    self.total.to_string()
                };
                let mut t = format!("{}  ({})  page {}/{}", td.name, rows_txt, self.page + 1, pages);
                if self.sort_col != NONE_SORT {
                    let d = if self.sort_desc { "↓" } else { "↑" };
                    t = format!("{t}  sort {}:{}", td.cols[self.sort_col].name, d);
                }
                if self.dirty() {
                    t = format!("{t}  ⚡ unsaved");
                }
                t
            }
        };
        let (Some(td), Some(_)) = (self.td.as_ref(), self.engine.as_ref()) else {
            f.render_widget(Paragraph::new("open a table: Enter in sidebar"), area);
            return;
        };
        let colnames: Vec<String> = td.cols.iter().map(|c| c.name.clone()).collect();
        grid::render(
            f,
            area,
            title,
            focused,
            &colnames,
            &td.rows,
            td.pk_idx,
            &self.pending,
            &mut self.grid,
        );
    }

    fn draw_editor(&mut self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Editor;
        let title = "sql  ⏎run  alt-⏎newline  c-p/n history";
        self.con.ta.set_block(ui::block(title, focused));
        f.render_widget(&self.con.ta, area);
    }

    fn draw_results(&mut self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Results;
        let Some(rr) = self.results.as_ref() else {
            f.render_widget(
                Paragraph::new(" sql pane: enter a query above and press Enter")
                    .style(Style::default().fg(Color::DarkGray)),
                area,
            );
            return;
        };
        if let Some(ok) = rr.ok.clone() {
            f.render_widget(Paragraph::new(ok).style(Style::default().fg(Color::Green)), area);
        } else {
            let pending_ui = PendingUi::default();
            grid::render(
                f,
                area,
                format!("results ({})", rr.rows.len()),
                focused,
                &rr.cols,
                &rr.rows,
                None,
                &pending_ui,
                &mut self.res,
            );
        }
    }

    fn draw_status(&mut self, f: &mut Frame, area: Rect) {
        let left = format!("{}{}  ? help  C-s save  Tab", self.focus_name(), self.pending_summary());
        let txt = Line::from(vec![
            if self.busy {
                Span::styled("⏳ loading…  ", Style::default().fg(Color::Yellow))
            } else {
                Span::raw("")
            },
            Span::raw(left),
            self.status_note(area.width),
        ]);
        f.render_widget(Paragraph::new(txt), area);
    }

    fn focus_name(&self) -> &'static str {
        match self.focus {
            Focus::Form => "form",
            Focus::Sidebar => "sidebar",
            Focus::Grid => "grid",
            Focus::Editor => "sql",
            Focus::Results => "results",
        }
    }

    fn pending_summary(&self) -> String {
        format!(
            " edits:{} dels:{} new:{}",
            self.pending.edits.values().map(|m| m.len()).sum::<usize>(),
            self.pending.deletes.len(),
            self.pending.drafts.len()
        )
    }

    /// The error or status message at the right of the status line.
    fn status_note(&self, width: u16) -> Span<'static> {
        let (mark, text, color) = if !self.err.is_empty() {
            ("⚠", self.err.chars().take(width.saturating_sub(2) as usize).collect::<String>(), Color::Red)
        } else if !self.msg.is_empty() {
            ("✓", self.msg.chars().take(width as usize / 2).collect(), Color::Green)
        } else {
            ("", String::new(), Color::Green)
        };
        let line = if text.is_empty() { String::new() } else { format!("  {mark} {text}") };
        Span::styled(line, Style::default().fg(color))
    }

    fn draw_edit_overlay(&mut self, f: &mut Frame) {
        let Some((row, col)) = self.cell.as_ref().map(|c| (c.row, c.col)) else { return };
        let w = 48u16.min(f.area().width / 2);
        let area = centered(f.area(), w, 4);
        f.render_widget(ratatui::widgets::Clear, area);
        // compute the title before borrowing the textarea mutably
        let title = match self.td.as_ref() {
            Some(td) => format!("edit {} [{}]", td.cols[col].name, self.cell_value(row, col)),
            None => "edit".into(),
        };
        let Some(cell) = self.cell.as_mut() else { return };
        cell.ta.set_block(ui::block(&title, true));
        f.render_widget(&cell.ta, area);
    }

    fn draw_confirm(&mut self, f: &mut Frame) {
        let mode = self.confirm.unwrap_or(Confirm::Refresh);
        let txt = match mode {
            Confirm::Refresh => "discard unsaved changes and refresh? [y/N]",
            Confirm::Quit => "discard unsaved changes and quit? [y/N]",
        };
        let area = centered(f.area(), 46, 5);
        f.render_widget(ratatui::widgets::Clear, area);
        f.render_widget(
            Paragraph::new(txt).block(ui::block("confirm", true)),
            area,
        );
    }

    fn draw_help(&mut self, f: &mut Frame) {
        let area = centered(f.area(), 64, 18);
        f.render_widget(ratatui::widgets::Clear, area);
        f.render_widget(Paragraph::new(HELP).block(ui::block("keys", true)), area);
    }
}

fn centered(r: Rect, w: u16, h: u16) -> Rect {
    Rect {
        x: r.x + (r.width.saturating_sub(w)) / 2,
        y: r.y + (r.height.saturating_sub(h)) / 2,
        width: w.min(r.width),
        height: h.min(r.height),
    }
}

/// Catalogue + first/next page + best count for one table; runs on a task.
async fn fetch_open(
    engine: &Arc<Engine>,
    name: &str,
    page: usize,
    order: Option<(String, bool)>,
) -> Done {
    let mut cols = match engine.columns(name).await {
        Ok(c) => c,
        Err(err) => return Done::Failed(format!("columns({name}): {err}")),
    };
    let pk_idx = cols.iter().position(|c| c.pk);
    for c in &mut cols {
        c.pk = false;
    }
    if let Some(p) = pk_idx {
        cols[p].pk = true;
    }
    let rows = match engine.fetch_page(name, &cols, order, PAGE, page * PAGE).await {
        Ok(p) => p.rows,
        Err(err) => return Done::Failed(format!("query: {err}")),
    };
    let (total, approx) = engine.count_estimate(name).await.unwrap_or((0, false));
    Done::Opened(OpenPayload {
        td: TableData { name: name.to_string(), cols, pk_idx, rows },
        page,
        total,
        approx,
    })
}
// the run loop lives in main.rs (keep app.rs free of terminal/IO plumbing)

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn app_with_row() -> App {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(tx);
        app.td = Some(TableData {
            name: "t".into(),
            cols: vec![
                db::Col { name: "id".into(), data_type: "int".into(), pk: true },
                db::Col { name: "name".into(), data_type: "text".into(), pk: false },
                db::Col { name: "score".into(), data_type: "real".into(), pk: false },
            ],
            pk_idx: Some(0),
            rows: vec![vec!["1".into(), "a".into(), "1".into()]],
        });
        app
    }

    #[tokio::test]
    async fn save_result_keeps_edits_typed_while_it_ran() {
        let mut app = app_with_row();
        let mut row = std::collections::BTreeMap::new();
        // this one the job wrote: same new value, so it is consumed
        row.insert("name".to_string(), ("a".to_string(), "saved".to_string()));
        // this one was typed while the job ran: kept, re-based on the saved value
        row.insert("score".to_string(), ("1".to_string(), "3".to_string()));
        app.pending.edits.insert("1".into(), row);
        app.pending.drafts = vec!["7".into(), "8".into()];

        app.apply(Done::Saved {
            msg: "ok".into(),
            edits: vec![("1".into(), "name".into(), "saved".into())],
            dels: vec![],
            ghosts: 1,
        });

        let kept = app.pending.edits.get("1").expect("row keeps its later edit");
        assert!(!kept.contains_key("name"), "consumed edit is removed");
        assert_eq!(kept["score"], ("1".to_string(), "3".to_string()));
        // the saved ghost is gone, the one added afterwards stays
        assert_eq!(app.pending.drafts, vec!["8".to_string()]);
    }

    #[tokio::test]
    async fn saved_value_rebases_a_cell_edited_again_mid_save() {
        let mut app = app_with_row();
        let mut row = std::collections::BTreeMap::new();
        row.insert("name".to_string(), ("a".to_string(), "second".to_string()));
        app.pending.edits.insert("1".into(), row);

        app.apply(Done::Saved {
            msg: "ok".into(),
            edits: vec![("1".into(), "name".into(), "first".into())],
            dels: vec![],
            ghosts: 0,
        });

        // kept, and compared against what the server now holds ("first")
        assert_eq!(app.pending.edits["1"]["name"], ("first".to_string(), "second".to_string()));
    }

    #[tokio::test]
    async fn mode_change_asks_for_a_full_repaint() {
        let mut app = app_with_row(); // engine is None: the form is on screen
        assert!(!app.need_clear);

        let opts = sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:").unwrap();        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        app.apply(Done::Connected {
            engine: Arc::new(Engine::Sqlite(pool)),
            tables: vec![],
            label: "x".into(),
            notice: None,
        });
        assert!(app.need_clear, "form to main UI must repaint every cell");

        app.need_clear = false;
        app.apply(Done::Sql(db::RunResult { cols: vec![], rows: vec![], ok: None }));
        assert!(!app.need_clear, "same screen keeps diffed painting");
    }
}
