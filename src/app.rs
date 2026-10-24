use crate::config::{Conn, ConnDef, EngineKind};
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
 C-g        stop a running job (a stopped save usually rolls back)
 Sidebar    j/k move  / filter  Enter open  Esc clear filter
 Grid       h/l j/k move  g/G start/end  [ ] page  s sort column
            e edit cell  o insert row  d (un)delete row
            in the cell editor: :null writes NULL, an empty value writes ''
            C-s save all as one transaction  R refresh  E export page
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
    /// Current page; every cell is already cast to text.
    pub rows: Vec<Vec<String>>,
}

pub struct CellEdit {
    ta: TextArea<'static>,
    pub row: usize,
    pub col: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Confirm {
    Refresh,
    Quit,
    Destructive,
}

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

/// Background job result; everything that can block runs on a task and reports back here.
pub enum Done {
    Connected {
        engine: Arc<Engine>,
        tables: Vec<String>,
        label: String,
        notice: Option<String>,
    },
    /// `opened` re-opens the table that was on screen
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
        /// (pk, col, new) edits this job wrote
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
    done_tx: tokio::sync::mpsc::UnboundedSender<(u64, Done)>,
    pub busy: bool,
    /// Start of the in-flight job, for the stall watchdog.
    busy_since: Option<std::time::Instant>,
    /// The in-flight job, kept so C-g can stop it.
    job: Option<tokio::task::JoinHandle<()>>,
    /// Current job id; a result with a stale id is dropped.
    job_id: u64,
    job_seq: u64,
    /// Why the last key was refused; drawn only while `busy`.
    busy_note: String,
    tables: Vec<String>,
    filt_in: Option<String>,
    filtered: Vec<String>,
    sb_sel: usize,
    pub td: Option<TableData>,
    pub focus: Focus,
    pub grid: GridState,
    pub page: usize,
    pub total: usize,
    /// `total` is a catalogue estimate, not an exact count
    pub approx: bool,
    sort_col: usize,
    sort_desc: bool,
    pub con: Console,
    pub results: Option<db::RunResult>,
    pub res: GridState,
    pub pending: PendingUi,
    cell: Option<CellEdit>,
    confirm: Option<Confirm>,
    /// SQL held from the console until the no-WHERE confirmation is answered
    pending_sql: Option<String>,
    /// Profile `readonly = true`: edits and non-row SQL are refused
    readonly: bool,
    /// Per-job wall clock from the profile, defaulting to `db::QUERY_BUDGET`
    budget: std::time::Duration,
    pub msg: String,
    pub err: String,
    pub help: bool,
    /// Database actually opened, shown in the sidebar title
    label: String,
    target: String,
    /// Next frame must repaint every cell; a shape change or resize invalidates ratatui's diff.
    pub need_clear: bool,
}

impl App {
    pub fn new(done_tx: tokio::sync::mpsc::UnboundedSender<(u64, Done)>) -> Self {
        let mut form_ta = TextArea::default();
        form_ta.set_placeholder_text("postgres://… · mysql://… · ./file.db  (Enter connect, C-c quit)");
        Self {
            form_ta,
            engine: None,
            done_tx,
            busy: false,
            busy_since: None,
            job: None,
            job_id: 0,
            job_seq: 0,
            busy_note: String::new(),
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
            pending_sql: None,
            readonly: false,
            budget: db::QUERY_BUDGET,
            msg: String::new(),
            err: String::new(),
            help: false,
            label: String::new(),
            target: String::new(),
            need_clear: false,
        }
    }

    // Runs under the job budget and always reports back; C-g can stop it.
    fn spawn_job(&mut self, fut: impl std::future::Future<Output = Done> + Send + 'static) {
        self.busy = true;
        self.busy_since = Some(std::time::Instant::now());
        self.busy_note.clear();
        let id = self.fresh_job_id();
        let budget = self.budget;
        let tx = self.done_tx.clone();
        let handle = tokio::spawn(async move {
            let done = match tokio::time::timeout(budget, fut).await {
                Ok(done) => done,
                Err(_) => Done::Failed(format!(
                    "job timed out ({}s): the server may still be working on it",
                    budget.as_secs()
                )),
            };
            let _ = tx.send((id, done));
        });
        self.job = Some(handle);
    }

    // A new id also invalidates a stopped job's late result.
    fn fresh_job_id(&mut self) -> u64 {
        let id = self.job_seq;
        self.job_seq += 1;
        self.job_id = id;
        id
    }

    fn refuse(&mut self, what: &str) {
        self.busy_note = format!("{what}: wait, or C-g to stop");
    }

    // A stopped save usually rolls back, but the server may still finish it.
    fn abort_job(&mut self) {
        if let Some(handle) = self.job.take() {
            handle.abort();
        }
        self.fresh_job_id();
        self.busy = false;
        self.busy_since = None;
    }

    fn stop_job(&mut self) {
        self.abort_job();
        self.busy_note.clear();
        self.msg = "stopped".into();
    }

    // A job that stopped reporting must not block the UI.
    pub fn tick(&mut self) {
        const SLACK: std::time::Duration = std::time::Duration::from_secs(5);
        if self
            .busy_since
            .is_some_and(|t0| t0.elapsed() > self.budget + SLACK)
        {
            self.abort_job();
            self.err =
                "a background job stopped reporting; the connection may be stuck — reconnect or restart"
                    .into();
        }
    }

    /// Starts a connect on a task; the DSN is validated first, so a bad one fails at once.
    pub fn enqueue_connect(&mut self, dsn: &str, def: Option<&ConnDef>) {
        if self.busy {
            self.refuse("still connecting");
            return;
        }
        self.err.clear();
        self.msg.clear();
        let conn = match Conn::from_dsn(dsn) {
            Ok(conn) => conn,
            Err(err) => {
                self.err = format!("cannot connect: {err}");
                self.focus = Focus::Form;
                return;
            }
        };
        self.readonly = def.is_some_and(ConnDef::is_readonly);
        self.budget = def.map_or(db::QUERY_BUDGET, ConnDef::timeout);
        self.target = target_label(&conn);
        self.spawn_job(async move {
            let engine = match Engine::connect(&conn).await {
                Err(err) => return Done::Failed(err),
                Ok(engine) => Arc::new(engine),
            };
            // Postgres falls back to a database named after the user when the DSN omits one
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
                Ok(tables) => Done::Connected { engine, tables, label, notice },
                Err(err) => Done::Failed(format!("list tables: {err}")),
            }
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

    fn enqueue_open(&mut self, name: &str) {
        if self.dirty() {
            self.err = "unsaved edits: C-s saves them, R discards and refreshes".into();
            return;
        }
        let Some(engine) = self.engine.clone() else { return };
        if self.busy {
            self.refuse("still loading");
            return;
        }
        self.err.clear();
        let name = name.to_string();
        self.spawn_job(async move { fetch_open(&engine, &name, 0, None).await });
    }

    fn enqueue_page(&mut self) {
        let Some(engine) = self.engine.clone() else { return };
        if self.busy {
            self.refuse("still loading");
            return;
        }
        self.err.clear();
        let Some(td) = self.td.as_ref() else { return };
        let order = if self.sort_col == NONE_SORT {
            None
        } else {
            Some((td.cols[self.sort_col].name.clone(), self.sort_desc))
        };
        let name = td.name.clone();
        let cols = td.cols.clone();
        let page = self.page;
        // Keep the known total; recounting here would run COUNT(*) on every turn.
        let (total, approx) = (self.total, self.approx);
        self.spawn_job(async move {
            match engine.fetch_page(&name, &cols, order, PAGE, page * PAGE).await {
                Ok(p) => Done::Page { rows: p.rows, total, approx },
                Err(err) => Done::Failed(format!("query: {err}")),
            }
        });
    }

    fn enqueue_refresh(&mut self) {
        let engine = self.engine.clone();
        let Some(engine) = engine else { return };
        if self.busy {
            self.refuse("still loading");
            return;
        }
        self.err.clear();
        let reopen = self.td.as_ref().map(|td| td.name.clone());
        self.spawn_job(async move {
            let tables = match engine.tables().await {
                Err(err) => return Done::Failed(format!("list tables: {err}")),
                Ok(tables) => tables,
            };
            match reopen {
                None => Done::Refreshed { tables, opened: None },
                Some(name) => match fetch_open(&engine, &name, 0, None).await {
                    Done::Opened(opened) => Done::Refreshed { tables, opened: Some(opened) },
                    other => other,
                },
            }
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

    pub fn apply(&mut self, done: Done) {
        self.busy = false;
        self.busy_since = None;
        self.job = None;
        self.busy_note.clear();
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
            // the form and the main UI have different shapes, so repaint from scratch
            self.need_clear = true;
        }
    }

    // A result whose job was replaced or stopped is dropped.
    pub fn apply_job(&mut self, id: u64, done: Done) {
        if id == self.job_id {
            self.apply(done);
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
        self.cell = None;
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
        // Remove only what this job wrote; anything typed meanwhile stays pending.
        for (pk, col, saved) in consumed {
            self.forget_consumed_edit(&pk, &col, &saved);
        }
        let removed = consumed_dels.len();
        for pk in consumed_dels {
            self.pending.deletes.remove(&pk);
        }
        if consumed_ghosts > 0 && self.pending.drafts.len() >= consumed_ghosts {
            self.pending.drafts.drain(0..consumed_ghosts);
        }
        // a page fetch does not recount, so adjust the header total here
        self.total = self.total.saturating_sub(removed).saturating_add(consumed_ghosts);
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
        // on the form, say that the connection itself failed
        self.err = if self.engine.is_none() && !err.starts_with("list tables") {
            format!("cannot connect: {err}")
        } else {
            err
        };
        if self.engine.is_none() {
            self.focus = Focus::Form;
        }
    }

    fn set_opened(&mut self, p: OpenPayload) {
        self.cell = None;
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
        if self.busy {
            self.refuse("still loading");
            return;
        }
        let (row, col) = self.grid.sel;
        let n_rows = self.td.as_ref().map_or(0, |td| td.rows.len());
        if row >= n_rows {
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
        } else if self.pk_of_row(row).is_none_or(|s| s.is_empty()) {
            // an empty key cannot identify the row, and saving would drop the edit
            self.err = "this row has no primary key value; it cannot be edited".into();
            return;
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
        // An empty commit writes '' (NULL and '' look alike in the grid); `:null` writes a real NULL.
        if new == col_orig && !new.is_empty() {
            m.remove(&col_name);
            if m.is_empty() {
                self.pending.edits.remove(&pk);
            }
        } else {
            m.insert(col_name, (col_orig, new));
        }
    }

    fn toggle_delete(&mut self) {
        if self.busy {
            self.refuse("still loading");
            return;
        }
        let row = self.grid.sel.0;
        if self.pk_of_row(row).is_none_or(|s| s.is_empty()) {
            if row >= self.td.as_ref().map_or(0, |td| td.rows.len()) {
                // a ghost is only a local draft, so nothing is deleted on the server
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
        if self.busy {
            self.refuse("still loading");
            return;
        }
        if self.editable_pk().is_none() {
            self.err = "adding rows needs a table with a single-column primary key".into();
            return;
        }
        self.pending.drafts.push(String::new());
        self.grid.end(self.grid_len());
    }

    fn enqueue_save(&mut self) {
        if self.readonly {
            self.err = "this connection is read-only: edits cannot be saved".into();
            return;
        }
        if self.busy {
            self.refuse("a save or query is still running");
            return;
        }
        self.err.clear();
        let Some(engine) = self.engine.clone() else { return };
        let Some(td) = self.td.as_ref() else { return };
        let Some((_, pk_name)) = self.editable_pk() else {
            return;
        };
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
        // Remember what this job takes so a later save cannot wipe edits typed meanwhile.
        let consumed_edits: Vec<(String, String, String)> = edits
            .iter()
            .map(|e| (e.pk.clone(), e.col.clone(), e.new.clone()))
            .collect();
        let consumed_dels = dels.clone();
        let consumed_ghosts = inserts.len();
        self.spawn_job(async move {
            match engine.save(&name, &pk_name, &edits, &dels, &inserts).await {
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
            }
        });
    }

    fn enqueue_run_sql(&mut self, sql: &str) {
        let Some(engine) = self.engine.clone() else { return };
        if self.busy {
            self.refuse("a statement is still running");
            return;
        }
        let mysql = engine.kind() == EngineKind::MySql;
        if self.readonly && !db::is_read_sql(sql, mysql) {
            self.err = "this connection is read-only: only row queries are allowed".into();
            return;
        }
        self.err.clear();
        self.con.push_history(sql.to_string());
        let sql = sql.to_string();
        self.spawn_job(async move {
            match engine.run_sql(&sql).await {
                Ok(rr) => Done::Sql(rr),
                Err(err) => Done::Failed(err),
            }
        });
    }

    fn export_csv(&mut self) {
        let Some(name) = self.td.as_ref().map(|td| td.name.clone()) else { return };
        let path = export_path(&name);
        if path.exists() {
            self.err = format!(
                "export: {} already exists; move or delete it first",
                path.display()
            );
            return;
        }
        let Some(td) = self.td.as_ref() else { return };
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
            Ok(()) => self.msg = format!("exported page → {}", path.display()),
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
        if self.cell.is_some() {
            return self.cell_key(k);
        }
        if let Some(mode) = self.confirm {
            return self.confirm_key(k, mode);
        }
        // the help box swallows keys; C-c keeps its usual quit behavior
        if self.help {
            if matches!(k.code, KeyCode::Esc | KeyCode::Char('?')) {
                self.help = false;
                return None;
            }
            if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
                self.help = false; // let the quit path ask below
            } else {
                return None;
            }
        }
        match self.global_key(&k) {
            GlobalKey::Quit => return Some(Out::Quit),
            GlobalKey::Handled => return None,
            GlobalKey::Pass => {}
        }
        self.focus_key(k)
    }

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
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                // the same guard as q: a pending edit survives a stray C-c
                if self.dirty() {
                    self.confirm = Some(Confirm::Quit);
                    GlobalKey::Handled
                } else {
                    GlobalKey::Quit
                }
            }
            (KeyCode::Char('g'), KeyModifiers::CONTROL) if self.busy => {
                self.stop_job();
                GlobalKey::Handled
            }
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

    /// Only `y` accepts, plus Enter for the refresh prompt.
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
                Confirm::Destructive => {
                    self.confirm = None;
                    if let Some(sql) = self.pending_sql.take() {
                        self.enqueue_run_sql(&sql);
                    }
                }
            }
            return None;
        }
        if matches!(k.code, KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q')) {
            self.confirm = None;
            self.pending_sql = None;
        }
        None
    }

    fn form_key(&mut self, k: KeyEvent) -> Option<Out> {
        match k.code {
            KeyCode::Enter => {
                let dsn = self.form_ta.lines().join("");
                self.enqueue_connect(&dsn, None);
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
                // a statement that would rewrite or empty a table gets a y/N
                let mysql = self.engine.as_ref().is_some_and(|e| e.kind() == EngineKind::MySql);
                if db::destructive_without_where(&sql, mysql) {
                    self.pending_sql = Some(sql);
                    self.confirm = Some(Confirm::Destructive);
                } else {
                    self.enqueue_run_sql(&sql);
                }
            }
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
        if self.help || self.confirm.is_some() {
            return;
        }
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
        let title = if self.busy {
            format!("connecting to {} (8s budget)", self.target)
        } else {
            "connect dsn (Enter)".to_string()
        };
        self.form_ta.set_block(ui::block(&title, true));
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
                let ro = if self.readonly { " [read-only]" } else { "" };
                if self.label.is_empty() {
                    format!("tables ({}){ro}", self.filtered.len())
                } else {
                    format!("tables ({}): {}{ro}", self.filtered.len(), self.label)
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
                let note = if self.busy_note.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", self.busy_note)
                };
                Span::styled(
                    format!("⏳ loading…{note}  "),
                    Style::default().fg(Color::Yellow),
                )
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
            Confirm::Refresh => "discard unsaved changes and refresh? [y/N]".to_string(),
            Confirm::Quit => "discard unsaved changes and quit? [y/N]".to_string(),
            Confirm::Destructive => {
                let kw = self
                    .pending_sql
                    .as_deref()
                    .and_then(|s| s.split_whitespace().next())
                    .map(str::to_ascii_uppercase)
                    .unwrap_or_else(|| "statement".into());
                format!("run {kw} with no WHERE clause? [y/N]")
            }
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

/// A safe file stem in the working directory, so `..` cannot escape it.
fn export_path(table: &str) -> std::path::PathBuf {
    let stem: String = table
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    std::path::PathBuf::from(format!("{stem}.csv"))
}

fn target_label(conn: &Conn) -> String {
    match conn.kind {
        crate::config::EngineKind::Postgres => "postgres".into(),
        crate::config::EngineKind::MySql => "mysql".into(),
        crate::config::EngineKind::Sqlite => {
            format!("sqlite {}", conn.url.trim_start_matches("sqlite:"))
        }
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

/// Only a single-column pk is editable; a composite key stays read-only.
fn pk_index(cols: &[db::Col]) -> Option<usize> {
    let mut pks = cols.iter().enumerate().filter(|(_, c)| c.pk).map(|(i, _)| i);
    match (pks.next(), pks.next()) {
        (Some(i), None) => Some(i),
        _ => None,
    }
}

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
    let pk_idx = pk_index(&cols);
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
                db::Col { name: "id".into(), pk: true },
                db::Col { name: "name".into(), pk: false },
                db::Col { name: "score".into(), pk: false },
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
        // the job wrote this one, so it is consumed
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
    async fn a_refused_key_leaves_no_sticky_message() {
        let mut app = app_with_row();
        app.busy = true;
        app.refuse("a statement is still running");
        assert!(app.busy_note.contains("still running"));

        app.apply(Done::Failed("boom".into()));
        assert!(!app.busy);
        assert!(app.busy_note.is_empty(), "the note must not outlive the job");
        assert!(!app.msg.contains("still running"), "and not reach the status line");
    }

    #[tokio::test]
    async fn spawn_job_reports_and_clears_the_handle() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(tx);
        app.spawn_job(async { Done::Failed("boom".into()) });
        assert!(app.busy && app.job.is_some() && app.busy_since.is_some());

        let (id, done) = rx.recv().await.unwrap();
        app.apply_job(id, done);
        assert!(!app.busy && app.job.is_none() && app.busy_since.is_none());
        assert!(app.err.contains("boom"), "{}", app.err);
    }

    #[tokio::test]
    async fn c_g_stops_a_job_and_clears_the_flags() {
        let mut app = app_with_row();
        app.busy = true;
        app.busy_since = Some(std::time::Instant::now());
        app.refuse("a save is still running");

        app.stop_job();
        assert!(!app.busy && app.busy_since.is_none() && app.job.is_none());
        assert!(app.busy_note.is_empty(), "the refusal note goes with the job");
        assert_eq!(app.msg, "stopped");
    }

    #[tokio::test]
    async fn a_stopped_jobs_result_cannot_clear_the_next_job() {
        let mut app = app_with_row();
        let stale = app.fresh_job_id(); // the job the user stops
        app.busy = true;
        app.busy_since = Some(std::time::Instant::now());
        app.abort_job();
        app.busy = true; // the next job takes a newer id
        app.busy_since = Some(std::time::Instant::now());

        app.apply_job(stale, Done::Failed("late".into()));
        assert!(app.busy, "a late result must not touch the newer job");
        assert!(!app.err.contains("late"), "{}", app.err);
    }

    #[tokio::test]
    async fn a_saved_delete_updates_the_total() {
        let mut app = app_with_row();
        app.total = 5;
        app.apply(Done::Saved {
            msg: "ok".into(),
            edits: vec![],
            dels: vec!["1".into()],
            ghosts: 2,
        });
        assert_eq!(app.total, 6, "5 rows minus one delete plus two inserts");
    }

    #[tokio::test]
    async fn a_delete_without_where_asks_first() {
        let mut app = app_with_row();
        app.focus = Focus::Editor;
        app.con.set_text("delete from t");
        app.handle(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));

        assert_eq!(app.confirm, Some(Confirm::Destructive));
        assert_eq!(app.pending_sql.as_deref(), Some("delete from t"));

        app.handle(Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)));
        assert_eq!(app.confirm, None);
        assert!(app.pending_sql.is_none(), "no is not a queued statement");
    }

    #[test]
    fn export_path_cannot_escape_the_working_directory() {
        assert_eq!(export_path("users"), std::path::PathBuf::from("users.csv"));
        assert_eq!(
            export_path("../../etc/passwd"),
            std::path::PathBuf::from("______etc_passwd.csv")
        );
    }

    #[tokio::test]
    async fn a_table_with_pending_edits_does_not_open_silently() {
        let mut app = app_with_row();
        app.pending.edits.insert(
            "1".into(),
            std::collections::BTreeMap::from([(
                "name".to_string(),
                ("a".to_string(), "b".to_string()),
            )]),
        );
        app.enqueue_open("other");
        assert!(app.err.contains("unsaved"), "{}", app.err);
        assert!(!app.pending.edits.is_empty(), "the edit is still there");
    }

    #[tokio::test]
    async fn opening_a_table_closes_the_cell_editor() {
        let mut app = app_with_row();
        app.cell = Some(CellEdit { ta: TextArea::new(vec![]), row: 0, col: 2 });
        app.apply(Done::Opened(OpenPayload {
            td: TableData {
                name: "narrow".into(),
                cols: vec![db::Col { name: "id".into(), pk: true }],
                pk_idx: Some(0),
                rows: vec![],
            },
            page: 0,
            total: 0,
            approx: false,
        }));
        assert!(app.cell.is_none(), "an overlay from the old table must not survive");
    }

    #[tokio::test]
    async fn the_help_box_swallows_editing_keys() {
        let mut app = app_with_row();
        app.focus = Focus::Grid;
        app.help = true;
        app.handle(Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
        assert!(app.pending.deletes.is_empty(), "d must not reach the grid");
        app.handle(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(!app.help, "Esc closes the help box");
    }

    #[tokio::test]
    async fn a_running_job_blocks_new_pending_edits() {
        let mut app = app_with_row();
        app.busy = true;
        app.grid.sel = (0, 1);
        app.open_cell();
        assert!(app.cell.is_none());
        app.toggle_delete();
        assert!(app.pending.deletes.is_empty());
        app.add_ghost();
        assert!(app.pending.drafts.is_empty());
        assert!(app.busy_note.contains("still loading"));
    }

    #[tokio::test]
    async fn a_row_without_a_pk_value_cannot_be_edited() {
        let mut app = app_with_row();
        app.td.as_mut().unwrap().rows[0][0] = String::new();
        app.grid.sel = (0, 1);
        app.open_cell();
        assert!(app.cell.is_none());
        assert!(app.err.contains("no primary key"), "{}", app.err);
    }

    #[tokio::test]
    async fn ctrl_c_asks_before_discarding_edits() {
        let mut dirty = app_with_row();
        dirty.pending.edits.insert(
            "1".into(),
            std::collections::BTreeMap::from([(
                "name".to_string(),
                ("a".to_string(), "b".to_string()),
            )]),
        );
        let out = dirty.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        assert!(out.is_none(), "the first C-c only asks");
        assert_eq!(dirty.confirm, Some(Confirm::Quit));
        assert!(matches!(
            dirty.handle(Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))),
            Some(Out::Quit)
        ));

        let mut clean = app_with_row();
        assert!(matches!(
            clean.handle(Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL
            ))),
            Some(Out::Quit)
        ));
    }

    #[test]
    fn a_composite_pk_is_read_only() {
        let pk = |name: &str| db::Col { name: name.into(), pk: true };
        let npk = || db::Col { name: "v".into(), pk: false };
        assert_eq!(pk_index(&[]), None);
        assert_eq!(pk_index(&[npk()]), None, "no pk at all");
        assert_eq!(pk_index(&[pk("a"), npk()]), Some(0));
        assert_eq!(pk_index(&[npk(), pk("a"), pk("b")]), None, "composite key");
    }

    #[tokio::test]
    async fn a_job_that_stopped_reporting_does_not_block_forever() {
        let mut app = app_with_row();
        app.busy = true;
        app.busy_since = Some(
            std::time::Instant::now() - db::QUERY_BUDGET - std::time::Duration::from_secs(10),
        );

        app.tick();
        assert!(!app.busy, "the watchdog must clear a latch");
        assert!(app.busy_since.is_none());
        assert!(app.err.contains("stuck"), "{}", app.err);
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
