/// sqlx 0.9 marks dynamic SQL as an audit boundary (SqlSafeStr). Every string
/// built here goes through quote_ident/quote_literal (tool-crafted
/// identifiers and literals, which is the sanitization); run_sql deliberately
/// passes the user's own SQL text through. The one audit is this comment.
macro_rules! qs {
    ($sql:expr) => {
        sqlx::query(sqlx::AssertSqlSafe(($sql).to_string()))
    };
}
macro_rules! qs_scalar {
    ($sql:expr) => {
        sqlx::query_scalar(sqlx::AssertSqlSafe(($sql).to_string()))
    };
}

/// Read one row's snapshot for the optimistic check: (is_null, text).
macro_rules! check_row {
    ($tx:expr, $check:expr) => {{
        let row = qs!(&$check)
            .fetch_optional(&mut **$tx)
            .await
            .map_err(|err| format!("save check: {err}"))?;
        match row {
            None => None,
            Some(r) => Some((
                r.try_get::<Option<String>, _>(0).ok().flatten().unwrap_or_default() == "1",
                r.try_get::<Option<String>, _>(1).ok().flatten(),
            )),
        }
    }};
}

use crate::config::EngineKind;
use crate::edit::{quote_ident, quote_literal, Edit, Report};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Column, Row};
use std::str::FromStr;

#[derive(Clone)]
pub struct Col {
    pub name: String,
    #[allow(dead_code)]
    pub data_type: String,
    pub pk: bool,
}

pub struct Page {
    pub rows: Vec<Vec<String>>,
}

pub struct RunResult {
    pub cols: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// Set for DDL/DML statements ("OK (n rows affected)"), otherwise rows carry the data.
    pub ok: Option<String>,
}

impl std::fmt::Debug for RunResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunResult")
            .field("cols", &self.cols)
            .field("rows", &self.rows.len())
            .field("ok", &self.ok)
            .finish()
    }
}

#[derive(Clone)]
pub enum Engine {
    Postgres(sqlx::PgPool),
    MySql(sqlx::MySqlPool),
    Sqlite(sqlx::SqlitePool),
}

fn e(err: impl std::fmt::Display) -> String {
    format!("db: {err}")
}

fn timeout_msg(budget: std::time::Duration) -> String {
    format!(
        "connect timed out ({}s). Check host, port and firewall.",
        budget.as_secs()
    )
}

/// `$N` on Postgres, `?` elsewhere.
fn ph(k: EngineKind, i: usize) -> String {
    match k {
        EngineKind::Postgres => format!("${i}"),
        _ => "?".into(),
    }
}

fn ident(k: EngineKind, s: &str) -> String {
    quote_ident(k, s)
}

/// Table reference at query time: Postgres always reads the `public` schema,
/// the same one the catalogue lists, regardless of the session's `search_path`.
fn qtable(k: EngineKind, table: &str) -> String {
    match k {
        EngineKind::Postgres => format!("public.{}", ident(k, table)),
        _ => ident(k, table),
    }
}

/// CAST target for text-ish cells: MySQL spells it CHAR, the others TEXT.
fn cast_text(k: EngineKind) -> &'static str {
    match k {
        EngineKind::MySql => "CHAR",
        _ => "TEXT",
    }
}

/// SELECT for one page, every column cast to text by the server.
fn page_sql(k: EngineKind, table: &str, cols: &[Col], order: Option<&(String, bool)>) -> String {
    let sel = cols
        .iter()
        .map(|c| {
            format!(
                "CAST({} AS {}) AS {}",
                ident(k, &c.name),
                cast_text(k),
                ident(k, &c.name)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!("SELECT {sel} FROM {}", qtable(k, table));
    if let Some((col, desc)) = order {
        sql.push_str(&format!(
            " ORDER BY {}{}",
            ident(k, col),
            if *desc { " DESC" } else { "" }
        ));
    }
    sql.push_str(&format!(" LIMIT {} OFFSET {}", ph(k, 1), ph(k, 2)));
    sql
}

/// Run a cast-text query and collect every cell as a string.
async fn fetch_cast_text(
    engine: &Engine,
    sql: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<Vec<String>>, String> {
    Ok(match engine {
        Engine::Postgres(p) => cast_pg(
            &qs!(sql).bind(limit).bind(offset).fetch_all(p).await.map_err(e)?,
        ),
        Engine::MySql(p) => cast_my(
            &qs!(sql).bind(limit).bind(offset).fetch_all(p).await.map_err(e)?,
        ),
        Engine::Sqlite(p) => cast_li(
            &qs!(sql).bind(limit).bind(offset).fetch_all(p).await.map_err(e)?,
        ),
    })
}

/// Postgres row estimate from the catalogue, None when it is unusable.
async fn pg_row_estimate(p: &sqlx::PgPool, table: &str) -> Result<Option<usize>, String> {
    let est: Option<i64> = qs_scalar!(
        "SELECT reltuples::BIGINT FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relname = $1 AND n.nspname = 'public' AND c.relkind = 'r'"
    )
    .bind(table)
    .fetch_optional(p)
    .await
    .map_err(e)?;
    Ok(est.filter(|n| *n >= 0).map(|n| n as usize))
}

/// MySQL row estimate from information_schema, None when it is unusable.
async fn mysql_row_estimate(p: &sqlx::MySqlPool, table: &str) -> Result<Option<usize>, String> {
    let est: Option<Option<i64>> = qs_scalar!(
        "SELECT TABLE_ROWS FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?"
    )
    .bind(table)
    .fetch_optional(p)
    .await
    .map_err(e)?;
    Ok(est.flatten().filter(|n| *n >= 0).map(|n| n as usize))
}

/// Statements whose first word returns rows (the rest run as one DML exec).
fn is_row_query(kw: &str) -> bool {
    matches!(
        kw,
        "select" | "with" | "show" | "explain" | "pragma" | "table" | "values"
            | "describe" | "desc"
    )
}

/// Transaction control words the console refuses, because a transaction left
/// open on a pooled connection breaks every query that follows.
fn is_tx_keyword(kw: &str) -> bool {
    matches!(
        kw,
        "begin" | "start" | "commit" | "rollback" | "abort" | "savepoint" | "end"
    )
}

/// Run a DDL/DML statement, returning rows affected.
async fn exec_statement(engine: &Engine, sql: &str) -> Result<u64, String> {
    Ok(match engine {
        Engine::Postgres(p) => qs!(sql).execute(p).await.map_err(e)?.rows_affected(),
        Engine::MySql(p) => qs!(sql).execute(p).await.map_err(e)?.rows_affected(),
        Engine::Sqlite(p) => qs!(sql).execute(p).await.map_err(e)?.rows_affected(),
    })
}

/// Run a row-returning statement per engine.
async fn query_rows(engine: &Engine, sql: &str, kw: &str) -> Result<RunResult, String> {
    match engine {
        Engine::Postgres(p) => pg_query_rows(p, sql, kw).await,
        Engine::MySql(p) => mysql_query_rows(p, sql, kw).await,
        Engine::Sqlite(p) => sqlite_query_rows(p, sql, kw).await,
    }
}

/// Postgres result can carry any row type; instead of a giant type map, the
/// user's query is re-selected with per-column ::text casts, so the server does
/// the conversion and the client sees strings only.
async fn pg_query_rows(p: &sqlx::PgPool, sql: &str, kw: &str) -> Result<RunResult, String> {
    if !matches!(kw, "select" | "with" | "table" | "values") {
        // EXPLAIN / SHOW: single text column, decode as-is
        let rs = qs!(sql).fetch_all(p).await.map_err(e)?;
        return Ok(RunResult { cols: cols_pg(&rs), rows: decode_pg(&rs), ok: None });
    }
    let names = pg_probe_names(p, sql).await?;
    let cast_sql = pg_cast_sql(&names, sql);
    let rs = qs!(&cast_sql).fetch_all(p).await.map_err(e)?;
    Ok(RunResult { cols: cols_pg(&rs), rows: cast_pg(&rs), ok: None })
}

/// Column names of the user's query, from a zero-row probe.
async fn pg_probe_names(p: &sqlx::PgPool, sql: &str) -> Result<Vec<String>, String> {
    let probe = qs!(format!("SELECT * FROM ({sql}) AS probe LIMIT 0"))
        .fetch_all(p)
        .await
        .map_err(e)?;
    Ok(probe
        .first()
        .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
        .unwrap_or_default())
}

/// The cast-everything select built around the user's query text.
fn pg_cast_sql(names: &[String], sql: &str) -> String {
    if names.is_empty() {
        return format!(
            "SELECT ''::text AS \"(empty)\" FROM ({sql}) AS probe LIMIT {CONSOLE_ROW_CAP}"
        );
    }
    let sel = names
        .iter()
        .map(|c| {
            format!(
                "probe.{}::text AS {}",
                ident(EngineKind::Postgres, c),
                ident(EngineKind::Postgres, c)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("SELECT {sel} FROM ({sql}) AS probe LIMIT {CONSOLE_ROW_CAP}")
}

async fn mysql_query_rows(p: &sqlx::MySqlPool, sql: &str, kw: &str) -> Result<RunResult, String> {
    let cap = if kw == "select" || kw == "with" {
        format!("{sql} LIMIT {CONSOLE_ROW_CAP}")
    } else {
        sql.to_string()
    };
    let rs = qs!(&cap).fetch_all(p).await.map_err(e)?;
    Ok(RunResult { cols: cols_my(&rs), rows: decode_my(&rs), ok: None })
}

async fn sqlite_query_rows(p: &sqlx::SqlitePool, sql: &str, kw: &str) -> Result<RunResult, String> {
    let cap = if matches!(kw, "select" | "with" | "table" | "values") {
        format!("{sql} LIMIT {CONSOLE_ROW_CAP}")
    } else {
        sql.to_string()
    };
    let rs = qs!(&cap).fetch_all(p).await.map_err(e)?;
    Ok(RunResult { cols: cols_li(&rs), rows: decode_li(&rs), ok: None })
}

const CONSOLE_ROW_CAP: usize = 500;

/// Cell text for a NULL is the empty string grid-wide.
fn cell(row: Option<String>) -> String {
    row.unwrap_or_default()
}

/// Scanner state for `statement_count`.
#[derive(Clone, Copy, PartialEq)]
enum Scan {
    Code,
    Single,
    Double,
    Comment,
}

/// One character of the scan. `n` counts statements finished at this character.
fn scan_step(st: Scan, c: char, n: &mut usize) -> Scan {
    match st {
        Scan::Comment => if c == '\n' { Scan::Code } else { Scan::Comment },
        Scan::Single => if c == '\'' { Scan::Code } else { Scan::Single },
        Scan::Double => if c == '"' { Scan::Code } else { Scan::Double },
        Scan::Code => match c {
            '\'' => Scan::Single,
            '"' => Scan::Double,
            ';' => {
                *n += 1;
                Scan::Code
            }
            _ => Scan::Code,
        },
    }
}

/// Count top-level statements: semicolons outside quotes and line comments.
/// The console runs one statement, so anything above 1 is refused up front with
/// a message the user can act on.
fn statement_count(sql: &str) -> usize {
    let mut n = 1;
    let mut st = Scan::Code;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        // "--" opens a comment; the second dash is handled by the Comment state
        if st == Scan::Code && c == '-' && chars.peek() == Some(&'-') {
            st = Scan::Comment;
            continue;
        }
        st = scan_step(st, c, &mut n);
    }
    n
}

/// Connection budget shared by the three pool builders.
const CONNECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

async fn pg_pool(url: &str) -> Result<sqlx::PgPool, String> {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(CONNECT_BUDGET)
        .connect(url)
        .await
        .map_err(e)
}

async fn mysql_pool(url: &str) -> Result<sqlx::MySqlPool, String> {
    sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(CONNECT_BUDGET)
        .connect(url)
        .await
        .map_err(e)
}

async fn sqlite_pool(conn: &crate::config::Conn) -> Result<sqlx::SqlitePool, String> {
    // path goes in as a path, never through URL parsing, so drive letters and
    // backslashes stay intact
    let opts = if conn.memory {
        SqliteConnectOptions::from_str("sqlite::memory:").map_err(e)?
    } else {
        let path = conn.path.clone().unwrap_or_else(|| ":memory:".into());
        SqliteConnectOptions::new().filename(path).create_if_missing(true)
    };
    // one connection for :memory:, because a pool of five would open five
    // separate empty databases
    let max = if conn.memory { 1 } else { 5 };
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(max)
        .connect_with(opts)
        .await
        .map_err(e)
}

impl Engine {
    /// Connect with a hard wall-clock budget: a slow or unreachable server must
    /// not leave the UI hanging. It fails after `budget` and the caller shows
    /// the error on the form.
    pub async fn connect(conn: &crate::config::Conn) -> Result<Engine, String> {
        let budget = CONNECT_BUDGET;
        let r = match tokio::time::timeout(budget, Engine::connect_inner(conn)).await {
            Ok(r) => r,
            Err(_) => return Err(timeout_msg(budget)),
        };
        r.map_err(|e| {
            if e.to_ascii_lowercase().contains("timed out") {
                timeout_msg(budget)
            } else {
                e
            }
        })
    }

    async fn connect_inner(conn: &crate::config::Conn) -> Result<Engine, String> {
        Ok(match conn.kind {
            EngineKind::Postgres => Engine::Postgres(pg_pool(&conn.url).await?),
            EngineKind::MySql => Engine::MySql(mysql_pool(&conn.url).await?),
            EngineKind::Sqlite => Engine::Sqlite(sqlite_pool(conn).await?),
        })
    }

    /// The database actually being read. Postgres defaults to a database named
    /// after the connecting user, so showing this catches wrong-DSN mistakes.
    pub async fn db_label(&self) -> String {
        match self {
            Engine::Postgres(p) => {
                qs_scalar!("SELECT current_database()").fetch_one(p).await.unwrap_or_default()
            }
            Engine::MySql(p) => {
                qs_scalar!("SELECT DATABASE()").fetch_one(p).await.unwrap_or_default()
            }
            Engine::Sqlite(_) => String::new(),
        }
    }

    pub fn kind(&self) -> EngineKind {
        match self {
            Engine::Postgres(_) => EngineKind::Postgres,
            Engine::MySql(_) => EngineKind::MySql,
            Engine::Sqlite(_) => EngineKind::Sqlite,
        }
    }

    /// Names of browsable tables in the default schema/DB.
    pub async fn tables(&self) -> Result<Vec<String>, String> {
        let sql = match self {
            Engine::Postgres(_) => {
                "SELECT table_name FROM information_schema.tables \
                 WHERE table_schema = 'public' AND table_type = 'BASE TABLE' ORDER BY 1"
            }
            Engine::MySql(_) => {
                "SELECT table_name FROM information_schema.tables \
                 WHERE table_schema = DATABASE() AND table_type = 'BASE TABLE' ORDER BY 1"
            }
            Engine::Sqlite(_) => {
                "SELECT name FROM sqlite_master WHERE type = 'table' \
                 AND name NOT LIKE 'sqlite_%' ORDER BY 1"
            }
        };
        Ok(match self {
            Engine::Postgres(p) => {
                let rows = sqlx::query(sql).fetch_all(p).await.map_err(e)?;
                let mut v = rows.into_iter().map(|r| cell(r.try_get(0).ok().flatten())).collect::<Vec<_>>();
                v.sort();
                v
            }
            Engine::MySql(p) => {
                let rows = sqlx::query(sql).fetch_all(p).await.map_err(e)?;
                let mut v = rows.into_iter().map(|r| cell(r.try_get(0).ok().flatten())).collect::<Vec<_>>();
                v.sort();
                v
            }
            Engine::Sqlite(p) => {
                let rows = sqlx::query(sql).fetch_all(p).await.map_err(e)?;
                let mut v = rows.into_iter().map(|r| cell(r.try_get(0).ok().flatten())).collect::<Vec<_>>();
                v.sort();
                v
            }
        })
    }

    /// Columns of a table + single-column PK flag.
    /// shortcut: only single-column PKs are editable and only the default
    /// schema (`public` / DATABASE()) is browsed. upgrade = schema panel +
    /// composite-PK row keys.
    pub async fn columns(&self, table: &str) -> Result<Vec<Col>, String> {
        match self {
            Engine::Postgres(p) => {
                let pk = pk_of_pg(p, table).await;
                let rows = sqlx::query(
                    "SELECT column_name, data_type FROM information_schema.columns \
                     WHERE table_schema = 'public' AND table_name = $1 \
                     ORDER BY ordinal_position",
                )
                .bind(table)
                .fetch_all(p)
                .await
                .map_err(e)?;
                Ok(rows
                    .into_iter()
                    .map(|r| {
                        let name: Option<String> = r.try_get(0).ok().flatten();
                        let data_type: Option<String> = r.try_get(1).ok().flatten();
                        Col {
                            name: name.clone().unwrap_or_default(),
                            data_type: data_type.unwrap_or_default(),
                            pk: name.is_some_and(|n| pk.contains(&n)),
                        }
                    })
                    .collect())
            }
            Engine::MySql(p) => {
                let pk = pk_of_mysql(p, table).await;
                let rows = sqlx::query(
                    "SELECT column_name, data_type FROM information_schema.columns \
                     WHERE table_schema = DATABASE() AND table_name = ? \
                     ORDER BY ordinal_position",
                )
                .bind(table)
                .fetch_all(p)
                .await
                .map_err(e)?;
                Ok(rows
                    .into_iter()
                    .map(|r| {
                        let name: Option<String> = r.try_get(0).ok().flatten();
                        let data_type: Option<String> = r.try_get(1).ok().flatten();
                        Col {
                            name: name.clone().unwrap_or_default(),
                            data_type: data_type.unwrap_or_default(),
                            pk: name.is_some_and(|n| pk.contains(&n)),
                        }
                    })
                    .collect())
            }
            Engine::Sqlite(p) => {
                let sql = format!("PRAGMA table_info({})", ident(EngineKind::Sqlite, table));
                let rows = qs!(&sql).fetch_all(p).await.map_err(e)?;
                Ok(rows
                    .into_iter()
                    .map(|r| Col {
                        name: r.try_get(1).ok().flatten().unwrap_or_default(),
                        data_type: r.try_get(2).ok().flatten().unwrap_or_default(),
                        pk: r.get::<i64, _>(5) != 0,
                    })
                    .collect())
            }
        }
    }

    /// One page of table data; every cell CAST to TEXT by the server so the
    /// client never needs a type map for browsing.
    /// shortcut: blobs come back as cast text (often garbage); upgrade = typed
    /// decode in the grid too.
    pub async fn fetch_page(
        &self,
        table: &str,
        cols: &[Col],
        order: Option<(String, bool)>,
        limit: usize,
        offset: usize,
    ) -> Result<Page, String> {
        let k = self.kind();
        let sql = page_sql(k, table, cols, order.as_ref());
        let rows = fetch_cast_text(self, &sql, limit as i64, offset as i64).await?;
        Ok(Page { rows })
    }

    pub async fn count(&self, table: &str) -> Result<usize, String> {
        let sql = format!("SELECT COUNT(*) FROM {}", qtable(self.kind(), table));
        let n: i64 = match self {
            Engine::Postgres(p) => qs_scalar!(&sql).fetch_one(p).await,
            Engine::MySql(p) => qs_scalar!(&sql).fetch_one(p).await,
            Engine::Sqlite(p) => qs_scalar!(&sql).fetch_one(p).await,
        }
        .map_err(e)?;
        Ok(n.max(0) as usize)
    }

    /// (rows, approximate?). Postgres/MySQL take the cheap catalogue statistic
    /// so opening a huge table never freezes on a full COUNT(*); they fall back
    /// to an exact count only when the statistic is missing (never analyzed,
    /// view, unsupported). SQLite counts exactly, since it is a local file scan.
    pub async fn count_estimate(&self, table: &str) -> Result<(usize, bool), String> {
        let est = match self {
            Engine::Postgres(p) => pg_row_estimate(p, table).await?,
            Engine::MySql(p) => mysql_row_estimate(p, table).await?,
            Engine::Sqlite(_) => None,
        };
        match est {
            Some(n) => Ok((n, true)),
            None => self.count(table).await.map(|n| (n, false)),
        }
    }

    /// Row-returning SQL from the console; DDL/DML returns "OK (n affected)".
    /// shortcut: only the first statement of a multi-statement string runs;
    /// edits and results are capped to 500 rows.
    pub async fn run_sql(&self, sql_in: &str) -> Result<RunResult, String> {
        let sql = sql_in.trim();
        let sql = sql.strip_suffix(';').unwrap_or(sql);
        if sql.is_empty() {
            return Ok(RunResult { cols: vec![], rows: vec![], ok: Some(String::new()) });
        }
        if statement_count(sql) > 1 {
            return Err(
                "one statement at a time: split on the semicolon and run each part".into(),
            );
        }
        let kw = sql.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
        // A manual BEGIN leaves a pooled connection mid-transaction that the
        // next statement may or may not land on. Transactions are owned by the
        // edit path (C-s), so the console refuses to start one.
        if is_tx_keyword(&kw) {
            return Err(
                "the console runs one statement at a time; C-s applies your edits in a transaction"
                    .into(),
            );
        }
        if !is_row_query(&kw) {
            let n = exec_statement(self, sql).await?;
            return Ok(RunResult {
                cols: vec![],
                rows: vec![],
                ok: Some(format!("OK ({n} rows affected)")),
            });
        }
        query_rows(self, sql, &kw).await
    }

    /// Apply pending edits + deletes + inserts in one transaction.
    /// Optimistic: every update re-reads the original cell inside the tx and
    /// aborts the whole batch if another client changed it meanwhile.
    /// Delete is by PK only (shortcut: mid-transaction row mutations by others
    /// are not detected for deletes; upgrade = full-row compare).
    pub async fn save(
        &self,
        table: &str,
        pk_col: &str,
        edits: &[Edit],
        dels: &[String],
        insert_drafts: &[Option<String>],
    ) -> Result<Report, String> {
        Ok(match self {
            Engine::Postgres(p) => pg_save(p, table, pk_col, edits, dels, insert_drafts).await?,
            Engine::MySql(p) => mysql_save(p, table, pk_col, edits, dels, insert_drafts).await?,
            Engine::Sqlite(p) => sqlite_save(p, table, pk_col, edits, dels, insert_drafts).await?,
        })
    }
}
// ---------- per-engine transactional save --------------------------------
// sqlx transaction and row types differ per database, so each engine keeps its
// own small steps. The SQL text itself comes from the pure builders above.

/// WHERE clause for the row identity.
fn pk_pred(pkc: &str, pk: &str) -> String {
    if pk.is_empty() {
        format!("{pkc} IS NULL")
    } else {
        format!("{pkc} = {}", quote_literal(pk))
    }
}

/// Message shown when the optimistic check finds a different value.
fn conflict_msg(pk: &str, col: &str) -> String {
    format!(
        "conflict: row pk={} col={} changed by someone else. refresh (R) and retry",
        if pk.is_empty() { "NULL" } else { pk },
        col
    )
}

/// Read NULL-ness and text together: the grid shows NULL as "", so an empty
/// snapshot must match a NULL cell.
fn check_sql(k: EngineKind, t: &str, col: &str, pkc: &str, pk: &str) -> String {
    format!(
        "SELECT CASE WHEN {c} IS NULL THEN '1' ELSE '0' END, CAST({c} AS {cas}) FROM {t} WHERE {pred}",
        c = ident(k, col),
        cas = cast_text(k),
        pred = pk_pred(pkc, pk)
    )
}

fn update_sql(k: EngineKind, t: &str, col: &str, new: &str, pkc: &str, pk: &str) -> String {
    format!(
        "UPDATE {t} SET {c} = {v} WHERE {pred}",
        c = ident(k, col),
        v = quote_literal(new),
        pred = pk_pred(pkc, pk)
    )
}

fn delete_sql(t: &str, pkc: &str, pk: &str) -> String {
    format!("DELETE FROM {t} WHERE {}", pk_pred(pkc, pk))
}

/// A draft is an explicit primary key unless the user left it empty.
fn draft_pk(draft: &Option<String>) -> Option<&str> {
    draft.as_deref().map(str::trim).filter(|d| !d.is_empty())
}

/// Shared optimistic check for one edit.
fn check_edit(tx_row: Option<(bool, Option<String>)>, ed: &Edit) -> Result<(), String> {
    let Some((is_null, text)) = tx_row else {
        return Err(conflict_msg(&ed.pk, &ed.col));
    };
    let same = if is_null {
        ed.orig.is_empty()
    } else {
        text.unwrap_or_default() == ed.orig
    };
    if same {
        Ok(())
    } else {
        Err(conflict_msg(&ed.pk, &ed.col))
    }
}

// --- Postgres ------------------------------------------------------------

async fn pg_save(
    p: &sqlx::PgPool,
    table: &str,
    pk_col: &str,
    edits: &[Edit],
    dels: &[String],
    drafts: &[Option<String>],
) -> Result<Report, String> {
    let mut tx = p.begin().await.map_err(e)?;
    let updates = pg_apply_updates(&mut tx, table, pk_col, edits).await?;
    let deletes = pg_apply_deletes(&mut tx, table, pk_col, dels).await?;
    let inserts = pg_apply_inserts(&mut tx, table, pk_col, drafts).await?;
    tx.commit().await.map_err(e)?;
    Ok(Report { updates, deletes, inserts })
}

async fn pg_apply_updates(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    pk_col: &str,
    edits: &[Edit],
) -> Result<usize, String> {
    let mut n = 0;
    for ed in edits {
        pg_update(tx, table, pk_col, ed).await?;
        n += 1;
    }
    Ok(n)
}

async fn pg_apply_deletes(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    pk_col: &str,
    dels: &[String],
) -> Result<usize, String> {
    let mut n = 0;
    for pk in dels {
        pg_delete(tx, table, pk_col, pk).await?;
        n += 1;
    }
    Ok(n)
}

async fn pg_apply_inserts(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    pk_col: &str,
    drafts: &[Option<String>],
) -> Result<Vec<String>, String> {
    let mut keys = Vec::with_capacity(drafts.len());
    for draft in drafts {
        keys.push(pg_insert(tx, table, pk_col, draft).await?);
    }
    Ok(keys)
}

async fn pg_update(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    pk_col: &str,
    ed: &Edit,
) -> Result<(), String> {
    let k = EngineKind::Postgres;
    let t = qtable(k, table);
    let pkc = ident(k, pk_col);
    let snapshot = check_row!(tx, check_sql(k, &t, &ed.col, &pkc, &ed.pk));
    check_edit(snapshot, ed)?;
    let sql = update_sql(k, &t, &ed.col, &ed.new, &pkc, &ed.pk);
    qs!(&sql)
        .execute(&mut **tx)
        .await
        .map_err(|err| format!("save update: {err}"))?;
    Ok(())
}

async fn pg_delete(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    pk_col: &str,
    pk: &str,
) -> Result<(), String> {
    let k = EngineKind::Postgres;
    let sql = delete_sql(&qtable(k, table), &ident(k, pk_col), pk);
    qs!(&sql)
        .execute(&mut **tx)
        .await
        .map_err(|err| format!("save delete: {err}"))?;
    Ok(())
}

async fn pg_insert(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    pk_col: &str,
    draft: &Option<String>,
) -> Result<String, String> {
    let k = EngineKind::Postgres;
    let t = qtable(k, table);
    let pkc = ident(k, pk_col);
    let cas = cast_text(k);
    let ins = match draft_pk(draft) {
        Some(pk) => format!(
            "INSERT INTO {t} ({pkc}) VALUES ({}) RETURNING CAST({pkc} AS {cas})",
            quote_literal(pk)
        ),
        None => format!("INSERT INTO {t} DEFAULT VALUES RETURNING CAST({pkc} AS {cas})"),
    };
    let row = qs!(&ins)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|err| format!("insert row (needs all-default columns + PK): {err}"))?
        .ok_or("insert row produced no identity")?;
    Ok(cell(row.try_get(0).ok().flatten()))
}

// --- MySQL ---------------------------------------------------------------

async fn mysql_save(
    p: &sqlx::MySqlPool,
    table: &str,
    pk_col: &str,
    edits: &[Edit],
    dels: &[String],
    drafts: &[Option<String>],
) -> Result<Report, String> {
    let mut tx = p.begin().await.map_err(e)?;
    let updates = mysql_apply_updates(&mut tx, table, pk_col, edits).await?;
    let deletes = mysql_apply_deletes(&mut tx, table, pk_col, dels).await?;
    let inserts = mysql_apply_inserts(&mut tx, table, pk_col, drafts).await?;
    tx.commit().await.map_err(e)?;
    Ok(Report { updates, deletes, inserts })
}

async fn mysql_apply_updates(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    table: &str,
    pk_col: &str,
    edits: &[Edit],
) -> Result<usize, String> {
    let mut n = 0;
    for ed in edits {
        mysql_update(tx, table, pk_col, ed).await?;
        n += 1;
    }
    Ok(n)
}

async fn mysql_apply_deletes(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    table: &str,
    pk_col: &str,
    dels: &[String],
) -> Result<usize, String> {
    let mut n = 0;
    for pk in dels {
        mysql_delete(tx, table, pk_col, pk).await?;
        n += 1;
    }
    Ok(n)
}

async fn mysql_apply_inserts(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    table: &str,
    pk_col: &str,
    drafts: &[Option<String>],
) -> Result<Vec<String>, String> {
    let mut keys = Vec::with_capacity(drafts.len());
    for draft in drafts {
        keys.push(mysql_insert(tx, table, pk_col, draft).await?);
    }
    Ok(keys)
}

async fn mysql_update(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    table: &str,
    pk_col: &str,
    ed: &Edit,
) -> Result<(), String> {
    let k = EngineKind::MySql;
    let t = qtable(k, table);
    let pkc = ident(k, pk_col);
    let snapshot = check_row!(tx, check_sql(k, &t, &ed.col, &pkc, &ed.pk));
    check_edit(snapshot, ed)?;
    let sql = update_sql(k, &t, &ed.col, &ed.new, &pkc, &ed.pk);
    qs!(&sql)
        .execute(&mut **tx)
        .await
        .map_err(|err| format!("save update: {err}"))?;
    Ok(())
}

async fn mysql_delete(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    table: &str,
    pk_col: &str,
    pk: &str,
) -> Result<(), String> {
    let k = EngineKind::MySql;
    let sql = delete_sql(&qtable(k, table), &ident(k, pk_col), pk);
    qs!(&sql)
        .execute(&mut **tx)
        .await
        .map_err(|err| format!("save delete: {err}"))?;
    Ok(())
}

async fn mysql_insert(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    table: &str,
    pk_col: &str,
    draft: &Option<String>,
) -> Result<String, String> {
    let k = EngineKind::MySql;
    let t = qtable(k, table);
    let pkc = ident(k, pk_col);
    // explicit pk: LAST_INSERT_ID() stays 0 here, so the draft IS the identity
    match draft_pk(draft) {
        Some(pk) => {
            let ins = format!("INSERT INTO {t} ({pkc}) VALUES ({})", quote_literal(pk));
            qs!(&ins)
                .execute(&mut **tx)
                .await
                .map_err(|err| format!("insert row: {err}"))?;
            Ok(pk.to_string())
        }
        None => {
            qs!(format!("INSERT INTO {t} () VALUES ()"))
                .execute(&mut **tx)
                .await
                .map_err(|err| format!("insert row: {err}"))?;
            let row: (String,) = sqlx::query_as("SELECT CAST(LAST_INSERT_ID() AS CHAR)")
                .fetch_one(&mut **tx)
                .await
                .map_err(e)?;
            Ok(row.0)
        }
    }
}

// --- SQLite --------------------------------------------------------------

async fn sqlite_save(
    p: &sqlx::SqlitePool,
    table: &str,
    pk_col: &str,
    edits: &[Edit],
    dels: &[String],
    drafts: &[Option<String>],
) -> Result<Report, String> {
    let mut tx = p.begin().await.map_err(e)?;
    let updates = sqlite_apply_updates(&mut tx, table, pk_col, edits).await?;
    let deletes = sqlite_apply_deletes(&mut tx, table, pk_col, dels).await?;
    let inserts = sqlite_apply_inserts(&mut tx, table, pk_col, drafts).await?;
    tx.commit().await.map_err(e)?;
    Ok(Report { updates, deletes, inserts })
}

async fn sqlite_apply_updates(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    pk_col: &str,
    edits: &[Edit],
) -> Result<usize, String> {
    let mut n = 0;
    for ed in edits {
        sqlite_update(tx, table, pk_col, ed).await?;
        n += 1;
    }
    Ok(n)
}

async fn sqlite_apply_deletes(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    pk_col: &str,
    dels: &[String],
) -> Result<usize, String> {
    let mut n = 0;
    for pk in dels {
        sqlite_delete(tx, table, pk_col, pk).await?;
        n += 1;
    }
    Ok(n)
}

async fn sqlite_apply_inserts(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    pk_col: &str,
    drafts: &[Option<String>],
) -> Result<Vec<String>, String> {
    let mut keys = Vec::with_capacity(drafts.len());
    for draft in drafts {
        keys.push(sqlite_insert(tx, table, pk_col, draft).await?);
    }
    Ok(keys)
}

async fn sqlite_update(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    pk_col: &str,
    ed: &Edit,
) -> Result<(), String> {
    let k = EngineKind::Sqlite;
    let t = qtable(k, table);
    let pkc = ident(k, pk_col);
    let snapshot = check_row!(tx, check_sql(k, &t, &ed.col, &pkc, &ed.pk));
    check_edit(snapshot, ed)?;
    let sql = update_sql(k, &t, &ed.col, &ed.new, &pkc, &ed.pk);
    qs!(&sql)
        .execute(&mut **tx)
        .await
        .map_err(|err| format!("save update: {err}"))?;
    Ok(())
}

async fn sqlite_delete(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    pk_col: &str,
    pk: &str,
) -> Result<(), String> {
    let k = EngineKind::Sqlite;
    let sql = delete_sql(&qtable(k, table), &ident(k, pk_col), pk);
    qs!(&sql)
        .execute(&mut **tx)
        .await
        .map_err(|err| format!("save delete: {err}"))?;
    Ok(())
}

async fn sqlite_insert(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    pk_col: &str,
    draft: &Option<String>,
) -> Result<String, String> {
    let k = EngineKind::Sqlite;
    let t = qtable(k, table);
    let pkc = ident(k, pk_col);
    let ins = match draft_pk(draft) {
        Some(pk) => format!("INSERT INTO {t} ({pkc}) VALUES ({})", quote_literal(pk)),
        None => format!("INSERT INTO {t} DEFAULT VALUES"),
    };
    qs!(&ins)
        .execute(&mut **tx)
        .await
        .map_err(|err| format!("insert row: {err}"))?;
    let n: i64 = sqlx::query_scalar("SELECT last_insert_rowid()")
        .fetch_one(&mut **tx)
        .await
        .map_err(e)?;
    Ok(n.to_string())
}

// ---------- concrete per-row-type helpers (kept non-generic on purpose) -----
// sqlx's Row/Column traits are per-database; every helper below exists three
// times. Copy is cheaper than the trait machinery it would take to dedupe.

use sqlx::postgres::PgRow;
use sqlx::mysql::MySqlRow;
use sqlx::sqlite::SqliteRow;

fn cols_pg(rs: &[PgRow]) -> Vec<String> {
    rs.first()
        .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
        .unwrap_or_default()
}

fn cols_my(rs: &[MySqlRow]) -> Vec<String> {
    rs.first()
        .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
        .unwrap_or_default()
}

fn cols_li(rs: &[SqliteRow]) -> Vec<String> {
    rs.first()
        .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
        .unwrap_or_default()
}

/// every cell of a set that was built with per-column CAST(... AS TEXT/CHAR)
fn cast_pg(rs: &[PgRow]) -> Vec<Vec<String>> {
    rs.iter()
        .map(|r| {
            (0..r.columns().len())
                .map(|i| cell(r.try_get(i).ok().flatten()))
                .collect()
        })
        .collect()
}

fn cast_my(rs: &[MySqlRow]) -> Vec<Vec<String>> {
    rs.iter()
        .map(|r| {
            (0..r.columns().len())
                .map(|i| cell(r.try_get(i).ok().flatten()))
                .collect()
        })
        .collect()
}

fn cast_li(rs: &[SqliteRow]) -> Vec<Vec<String>> {
    rs.iter()
        .map(|r| {
            (0..r.columns().len())
                .map(|i| cell(r.try_get(i).ok().flatten()))
                .collect()
        })
        .collect()
}

/// Decode arbitrary rows by trying types in order until one lands.
/// String first: text/date/decimal read as the server sent them; numbers
/// fall through. shortcut: unsigned bigint beyond i64 shows in float
/// scientific form; upgrade = per-engine type match.
fn decode_pg(rs: &[PgRow]) -> Vec<Vec<String>> {
    rs.iter()
        .map(|r| {
            (0..r.columns().len())
                .map(|i| {
                    if let Ok(Some(v)) = r.try_get::<Option<String>, _>(i) { return v; }
                    if let Ok(Some(v)) = r.try_get::<Option<i64>, _>(i) { return v.to_string(); }
                    if let Ok(Some(v)) = r.try_get::<Option<f64>, _>(i) { return v.to_string(); }
                    if let Ok(Some(b)) = r.try_get::<Option<Vec<u8>>, _>(i) {
                        return format!("<blob {} B>", b.len());
                    }
                    String::new()
                })
                .collect()
        })
        .collect()
}

fn decode_my(rs: &[MySqlRow]) -> Vec<Vec<String>> {
    rs.iter()
        .map(|r| {
            (0..r.columns().len())
                .map(|i| {
                    if let Ok(Some(v)) = r.try_get::<Option<String>, _>(i) { return v; }
                    if let Ok(Some(v)) = r.try_get::<Option<i64>, _>(i) { return v.to_string(); }
                    if let Ok(Some(v)) = r.try_get::<Option<u64>, _>(i) { return v.to_string(); }
                    if let Ok(Some(v)) = r.try_get::<Option<f64>, _>(i) { return v.to_string(); }
                    if let Ok(Some(v)) = r.try_get::<Option<sqlx::types::chrono::NaiveDateTime>, _>(i) {
                        return v.to_string();
                    }
                    if let Ok(Some(v)) = r.try_get::<Option<sqlx::types::chrono::NaiveDate>, _>(i) {
                        return v.to_string();
                    }
                    if let Ok(Some(v)) = r.try_get::<Option<sqlx::types::chrono::NaiveTime>, _>(i) {
                        return v.to_string();
                    }
                    if let Ok(Some(b)) = r.try_get::<Option<Vec<u8>>, _>(i) {
                        return format!("<blob {} B>", b.len());
                    }
                    String::new()
                })
                .collect()
        })
        .collect()
}

fn decode_li(rs: &[SqliteRow]) -> Vec<Vec<String>> {
    rs.iter()
        .map(|r| {
            (0..r.columns().len())
                .map(|i| {
                    if let Ok(Some(v)) = r.try_get::<Option<i64>, _>(i) { return v.to_string(); }
                    if let Ok(Some(v)) = r.try_get::<Option<f64>, _>(i) { return v.to_string(); }
                    if let Ok(Some(v)) = r.try_get::<Option<String>, _>(i) { return v; }
                    if let Ok(Some(b)) = r.try_get::<Option<Vec<u8>>, _>(i) {
                        return format!("<blob {} B>", b.len());
                    }
                    String::new()
                })
                .collect()
        })
        .collect()
}

async fn pk_of_pg(p: &sqlx::PgPool, table: &str) -> Vec<String> {
    sqlx::query(
        "SELECT kcu.column_name FROM information_schema.table_constraints tc \
         JOIN information_schema.key_column_usage kcu \
         ON tc.constraint_name = kcu.constraint_name \
         WHERE tc.constraint_type = 'PRIMARY KEY' \
         AND tc.table_schema = 'public' AND tc.table_name = $1",
    )
    .bind(table)
    .fetch_all(p)
    .await
    .unwrap_or_default()
    .into_iter()
    .filter_map(|r| r.try_get::<Option<String>, _>(0).ok().flatten())
    .collect()
}

async fn pk_of_mysql(p: &sqlx::MySqlPool, table: &str) -> Vec<String> {
    sqlx::query(
        "SELECT kcu.column_name FROM information_schema.table_constraints tc \
         JOIN information_schema.key_column_usage kcu \
         ON tc.constraint_name = kcu.constraint_name \
         WHERE tc.constraint_type = 'PRIMARY KEY' \
         AND tc.table_schema = DATABASE() AND tc.table_name = ?",
    )
    .bind(table)
    .fetch_all(p)
    .await
    .unwrap_or_default()
    .into_iter()
    .filter_map(|r| r.try_get::<Option<String>, _>(0).ok().flatten())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::Edit;
    use sqlx::sqlite::SqlitePoolOptions;

    /// Run a fixture script: the console refuses multi-statement text on
    /// purpose, so tests split it themselves.
    async fn script(e: &Engine, sql: &str) {
        for stmt in sql.split(';') {
            if !stmt.trim().is_empty() {
                e.run_sql(stmt).await.unwrap();
            }
        }
    }

    /// single connection so the in-memory DB is shared across statements
    async fn mem() -> Engine {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:").unwrap();
        let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
        let e = Engine::Sqlite(pool);
        script(
            &e,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL); \
             INSERT INTO t (id, name, score) VALUES (1, 'a', 1.5), (2, 'b', 2.5)",
        )
        .await;
        e
    }

    #[tokio::test]
    async fn catalog_browse_and_save_roundtrip() {
        let e = mem().await;

        assert_eq!(e.tables().await.unwrap(), vec!["t"]);
        let cols = e.columns("t").await.unwrap();
        assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "name", "score"]);
        assert!(cols[0].pk);
        assert!(!cols[1].pk);

        let page = e.fetch_page("t", &cols, Some(("id".into(), false)), 100, 0).await.unwrap();
        assert_eq!(page.rows.len(), 2);
        // cast-to-text pages: every cell is a string
        assert_eq!(page.rows[0][1], "a");
        assert_eq!(page.rows[0][2], "1.5");
        assert_eq!(e.count("t").await.unwrap(), 2);

        // update one cell inside a transaction, optimistic check passes
        let rep = e
            .save(
                "t",
                "id",
                &[Edit { pk: "1".into(), col: "name".into(), orig: "a".into(), new: "a2".into() }],
                &[],
                &[],
            )
            .await
            .unwrap();
        assert_eq!(rep.updates, 1);
        let page = e.fetch_page("t", &cols, None, 100, 0).await.unwrap();
        assert_eq!(page.rows[0][1], "a2");

        // a verdict against a stale original must abort, not overwrite
        let err = e
            .save(
                "t",
                "id",
                &[Edit { pk: "1".into(), col: "name".into(), orig: "a".into(), new: "zzz".into() }],
                &[],
                &[],
            )
            .await
            .unwrap_err();
        assert!(err.contains("conflict"), "{err}");
        let page = e.fetch_page("t", &cols, None, 100, 0).await.unwrap();
        assert_eq!(page.rows[0][1], "a2", "conflicted row must be untouched");

        // delete + insert (draft pk typed / default pk)
        let rep = e.save("t", "id", &[], &["2".into()], &[Some("42".into()), None]).await.unwrap();
        assert_eq!(rep.deletes, 1);
        // sqlite rowid after an explicit 42 is 43 (max+1)
        assert_eq!(rep.inserts, vec!["42".to_string(), "43".to_string()]);
        assert_eq!(e.count("t").await.unwrap(), 3);
        let page = e.fetch_page("t", &cols, Some(("id".into(), false)), 100, 0).await.unwrap();
        assert_eq!(page.rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>(), ["1", "42", "43"]);
    }

    #[tokio::test]
    async fn null_cell_can_be_filled_and_empty_orig_matches() {
        let e = mem().await;
        e.run_sql("INSERT INTO t (id, name, score) VALUES (9, NULL, NULL)").await.unwrap();

        // the grid shows NULL as "", so an empty snapshot must not read as
        // "someone else changed it"
        let rep = e
            .save(
                "t",
                "id",
                &[Edit {
                    pk: "9".into(),
                    col: "name".into(),
                    orig: String::new(),
                    new: "filled".into(),
                }],
                &[],
                &[],
            )
            .await
            .unwrap();
        assert_eq!(rep.updates, 1);
        let r = e.run_sql("SELECT name FROM t WHERE id = 9").await.unwrap();
        assert_eq!(r.rows[0][0], "filled");

        // a non-empty snapshot against a NULL cell is still a conflict
        e.run_sql("UPDATE t SET name = NULL WHERE id = 9").await.unwrap();
        let err = e
            .save(
                "t",
                "id",
                &[Edit {
                    pk: "9".into(),
                    col: "name".into(),
                    orig: "stale".into(),
                    new: "x".into(),
                }],
                &[],
                &[],
            )
            .await
            .unwrap_err();
        assert!(err.contains("conflict"), "{err}");
    }

    #[tokio::test]
    async fn count_estimate_reports_exact_for_sqlite() {
        let e = mem().await;
        // sqlite has no catalogue statistic to trust, so this is exact
        assert_eq!(e.count_estimate("t").await.unwrap(), (2, false));
    }

    #[test]
    fn page_sql_dialects() {
        let cols = vec![
            Col { name: "id".into(), data_type: "int".into(), pk: true },
            Col { name: "na\"me".into(), data_type: "text".into(), pk: false },
        ];
        let pg = page_sql(EngineKind::Postgres, "t", &cols, Some(&("id".into(), true)));
        assert_eq!(
            pg,
            "SELECT CAST(\"id\" AS TEXT) AS \"id\", CAST(\"na\"\"me\" AS TEXT) AS \"na\"\"me\" \
             FROM public.\"t\" ORDER BY \"id\" DESC LIMIT $1 OFFSET $2"
        );
        let my = page_sql(EngineKind::MySql, "t", &cols, None);
        assert_eq!(
            my,
            "SELECT CAST(`id` AS CHAR) AS `id`, CAST(`na\"me` AS CHAR) AS `na\"me` \
             FROM `t` LIMIT ? OFFSET ?"
        );
    }

    #[test]
    fn statement_scan() {
        assert_eq!(statement_count("select 1"), 1);
        assert_eq!(statement_count("select ';'"), 1);
        assert_eq!(statement_count("select 1; select 2"), 2);
        assert_eq!(statement_count("select 1 -- ; ignored\n"), 1);
        assert_eq!(statement_count(r#"select "a;b""#), 1);
        // a trailing semicolon counts, and run_sql strips it before counting
        assert_eq!(statement_count("select 1;"), 2);
    }

    #[tokio::test]
    async fn multiple_statements_are_refused_with_a_clear_message() {
        let e = mem().await;
        // the Postgres path wraps row-returning SQL in a subquery, so a second
        // statement would surface as a baffling syntax error; refuse first
        let err = e.run_sql("SELECT 1; SELECT 2").await.unwrap_err();
        assert!(err.contains("one statement"), "{err}");
        // a semicolon inside a string literal is not a second statement
        let r = e.run_sql("SELECT ';' AS x").await.unwrap();
        assert_eq!(r.rows[0][0], ";");
        // and a lone trailing semicolon is fine
        assert_eq!(e.run_sql("SELECT 1;").await.unwrap().rows[0][0], "1");
    }

    #[tokio::test]
    async fn manual_transaction_is_refused() {
        let e = mem().await;
        let err = e.run_sql("BEGIN").await.unwrap_err();
        assert!(err.contains("one statement at a time"), "{err}");
        // normal statements still work afterwards
        assert_eq!(e.count("t").await.unwrap(), 2);
    }

    #[tokio::test]
    async fn sqlite_file_path_roundtrip() {
        // P2 regression: a real file path (Windows drive letters included) must
        // open through Conn → Engine::connect, not through URL parsing.
        let dir = std::env::temp_dir();
        let file = dir.join(format!("ru-sqlmux-test-{}.db", std::process::id()));
        let dsn = file.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&file);

        let conn = crate::config::Conn::from_dsn(&dsn).unwrap();
        let e = Engine::connect(&conn).await.unwrap();
        script(&e, "CREATE TABLE z (id INTEGER PRIMARY KEY, v TEXT); INSERT INTO z (v) VALUES ('hi')")
            .await;
        assert_eq!(e.tables().await.unwrap(), vec!["z"]);
        let cols = e.columns("z").await.unwrap();
        let page = e.fetch_page("z", &cols, None, 100, 0).await.unwrap();
        assert_eq!(page.rows[0][1], "hi");
        drop(e);

        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn console_sql_paths() {
        let e = mem().await;

        // row-returning: typed decode by value, not by cast
        let r = e.run_sql("SELECT id, name FROM t ORDER BY id").await.unwrap();
        assert_eq!(r.cols, vec!["id", "name"]);
        assert_eq!(r.rows[0], vec!["1".to_string(), "a".to_string()]);
        assert!(r.ok.is_none());

        // DML reports affected rows; trailing semicolon is tolerated
        let r = e.run_sql("UPDATE t SET name = 'x' WHERE id = 1;").await.unwrap();
        assert_eq!(r.ok.as_deref(), Some("OK (1 rows affected)"));
        let r = e.run_sql("DELETE FROM t").await.unwrap();
        assert_eq!(r.ok.as_deref(), Some("OK (2 rows affected)"));
        assert_eq!(e.count("t").await.unwrap(), 0);

        // empty input is a no-op, not an error
        assert_eq!(e.run_sql("  ").await.unwrap().ok.as_deref(), Some(""));
    }
}
