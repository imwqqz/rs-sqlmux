/// Dynamic SQL is an audit boundary: every string here is built with quoted identifiers or literals, and `run_sql` passes the user's SQL on purpose.
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

use crate::config::EngineKind;
use crate::edit::{quote_ident, quote_literal, Edit, Report};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Column, Executor, Row, SqlSafeStr, Statement};
use std::str::FromStr;

#[derive(Clone)]
pub struct Col {
    pub name: String,
    pub pk: bool,
}

pub struct Page {
    pub rows: Vec<Vec<String>>,
}

pub struct RunResult {
    pub cols: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// Set for DDL/DML; row queries fill `rows` instead.
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

/// Postgres always reads `public`, regardless of `search_path`.
fn qtable(k: EngineKind, table: &str) -> String {
    match k {
        EngineKind::Postgres => format!("public.{}", ident(k, table)),
        _ => ident(k, table),
    }
}

fn cast_text(k: EngineKind) -> &'static str {
    match k {
        EngineKind::MySql => "CHAR",
        _ => "TEXT",
    }
}

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
    // 0 means "not analyzed yet" as often as "empty", so it cannot drive paging
    Ok(est.filter(|n| *n > 0).map(|n| n as usize))
}

async fn mysql_row_estimate(p: &sqlx::MySqlPool, table: &str) -> Result<Option<usize>, String> {
    let est: Option<Option<i64>> = qs_scalar!(
        "SELECT TABLE_ROWS FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?"
    )
    .bind(table)
    .fetch_optional(p)
    .await
    .map_err(e)?;
    Ok(est.flatten().filter(|n| *n > 0).map(|n| n as usize))
}

fn is_row_query(kw: &str) -> bool {
    matches!(
        kw,
        "select" | "with" | "show" | "explain" | "pragma" | "table" | "values"
            | "describe" | "desc"
    )
}

/// A manual BEGIN would strand the pooled connection mid-transaction.
fn is_tx_keyword(kw: &str) -> bool {
    matches!(
        kw,
        "begin" | "start" | "commit" | "rollback" | "abort" | "savepoint" | "end"
    )
}

fn first_keyword(sql: &str, escapes: bool) -> String {
    code_words(sql, escapes)
        .first()
        .map(|w| w.to_ascii_lowercase())
        .unwrap_or_default()
}

/// A read-only connection takes row queries only; shortcut: a `WITH ... DELETE` or a mutating `SELECT` still passes.
pub fn is_read_sql(sql: &str, escapes: bool) -> bool {
    is_row_query(&first_keyword(sql, escapes))
}

async fn exec_statement(engine: &Engine, sql: &str) -> Result<u64, String> {
    Ok(match engine {
        Engine::Postgres(p) => qs!(sql).execute(p).await.map_err(e)?.rows_affected(),
        Engine::MySql(p) => qs!(sql).execute(p).await.map_err(e)?.rows_affected(),
        Engine::Sqlite(p) => qs!(sql).execute(p).await.map_err(e)?.rows_affected(),
    })
}

async fn query_rows(engine: &Engine, sql: &str, kw: &str) -> Result<RunResult, String> {
    match engine {
        Engine::Postgres(p) => pg_query_rows(p, sql, kw).await,
        Engine::MySql(p) => mysql_query_rows(p, sql, kw).await,
        Engine::Sqlite(p) => sqlite_query_rows(p, sql, kw).await,
    }
}

/// The query is re-selected with `::text` casts, so the server does the conversion.
async fn pg_query_rows(p: &sqlx::PgPool, sql: &str, kw: &str) -> Result<RunResult, String> {
    if !matches!(kw, "select" | "with" | "table" | "values") {
        // EXPLAIN / SHOW: single text column, decode as-is
        let rs = qs!(sql).fetch_all(p).await.map_err(e)?;
        return Ok(RunResult { cols: cols_pg(&rs), rows: decode_pg(&rs), ok: None });
    }
    let names = pg_column_names(p, sql).await?;
    let cast_sql = pg_cast_sql(&names, sql);
    let rs = qs!(&cast_sql).fetch_all(p).await.map_err(e)?;
    Ok(RunResult { cols: cols_pg(&rs), rows: cast_pg(&rs), ok: None })
}

/// Preparing does not execute, so a data-modifying CTE runs once.
async fn pg_column_names(p: &sqlx::PgPool, sql: &str) -> Result<Vec<String>, String> {
    let stmt = p
        .prepare(sqlx::AssertSqlSafe(sql).into_sql_str())
        .await
        .map_err(e)?;
    Ok(stmt.columns().iter().map(|c| c.name().to_string()).collect())
}

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
    let capped = if kw == "select" || kw == "with" {
        cap_select(sql)
    } else {
        sql.to_string()
    };
    let cols = mysql_column_names(p, &capped).await?;
    let rs = qs!(&capped).fetch_all(p).await.map_err(e)?;
    Ok(RunResult { cols, rows: decode_my(&rs), ok: None })
}

/// Statement metadata keeps the headers when the result is empty.
async fn mysql_column_names(p: &sqlx::MySqlPool, sql: &str) -> Result<Vec<String>, String> {
    let stmt = p.prepare(sqlx::AssertSqlSafe(sql).into_sql_str()).await.map_err(e)?;
    Ok(stmt.columns().iter().map(|c| c.name().to_string()).collect())
}

async fn sqlite_query_rows(p: &sqlx::SqlitePool, sql: &str, kw: &str) -> Result<RunResult, String> {
    let capped = if matches!(kw, "select" | "with" | "table" | "values") {
        cap_select(sql)
    } else {
        sql.to_string()
    };
    let cols = sqlite_column_names(p, &capped).await?;
    let rs = qs!(&capped).fetch_all(p).await.map_err(e)?;
    Ok(RunResult { cols, rows: decode_li(&rs), ok: None })
}

async fn sqlite_column_names(p: &sqlx::SqlitePool, sql: &str) -> Result<Vec<String>, String> {
    let stmt = p.prepare(sqlx::AssertSqlSafe(sql).into_sql_str()).await.map_err(e)?;
    Ok(stmt.columns().iter().map(|c| c.name().to_string()).collect())
}

const CONSOLE_ROW_CAP: usize = 500;

// cap rows by wrapping: appending LIMIT turns "... LIMIT 1" into a syntax error
fn cap_select(sql: &str) -> String {
    format!("SELECT * FROM ({sql}) AS capped LIMIT {CONSOLE_ROW_CAP}")
}

fn cell(row: Option<String>) -> String {
    row.unwrap_or_default()
}

#[derive(Clone, Copy, PartialEq)]
enum Scan {
    Code,
    Single,
    Double,
    LineComment,
    BlockComment,
}

// Counts top-level statements; `escapes` is MySQL, where \' does not close a string.
fn statement_count(sql: &str, escapes: bool) -> usize {
    let mut n = 1;
    let mut st = Scan::Code;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        st = comment_step(st, c, &mut chars);
        if matches!(st, Scan::LineComment | Scan::BlockComment) {
            continue;
        }
        if escapes && in_string(st) && c == '\\' {
            chars.next();
            continue;
        }
        st = code_step(st, c, &mut n);
    }
    n
}

fn in_string(st: Scan) -> bool {
    matches!(st, Scan::Single | Scan::Double)
}

fn comment_step(st: Scan, c: char, chars: &mut std::iter::Peekable<std::str::Chars>) -> Scan {
    match (st, c) {
        (Scan::Code, '/') if chars.peek() == Some(&'*') => {
            chars.next();
            Scan::BlockComment
        }
        (Scan::BlockComment, '*') if chars.peek() == Some(&'/') => {
            chars.next();
            Scan::Code
        }
        (Scan::Code, '-') if chars.peek() == Some(&'-') => {
            chars.next();
            Scan::LineComment
        }
        (Scan::LineComment, '\n') => Scan::Code,
        (other, _) => other,
    }
}

fn code_step(st: Scan, c: char, n: &mut usize) -> Scan {
    match (st, c) {
        (Scan::Code, '\'') => Scan::Single,
        (Scan::Code, '"') => Scan::Double,
        (Scan::Code, ';') => {
            *n += 1;
            Scan::Code
        }
        (Scan::Single, '\'') | (Scan::Double, '"') => Scan::Code,
        (other, _) => other,
    }
}

/// Words outside comments and strings, so a keyword in a literal is not read.
fn code_words(sql: &str, escapes: bool) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut st = Scan::Code;
    let mut semicolons = 0;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        st = comment_step(st, c, &mut chars);
        if matches!(st, Scan::LineComment | Scan::BlockComment) {
            flush_word(&mut words, &mut cur);
            continue;
        }
        if escapes && in_string(st) && c == '\\' {
            chars.next();
            continue;
        }
        let in_code = st == Scan::Code;
        st = code_step(st, c, &mut semicolons);
        if in_code && (c.is_alphanumeric() || c == '_') {
            cur.push(c);
        } else {
            flush_word(&mut words, &mut cur);
        }
    }
    flush_word(&mut words, &mut cur);
    words
}

fn flush_word(words: &mut Vec<String>, cur: &mut String) {
    if !cur.is_empty() {
        words.push(std::mem::take(cur));
    }
}

/// UPDATE/DELETE with no WHERE; shortcut: a `WITH ... DELETE` or a WHERE in a subquery is missed.
pub fn destructive_without_where(sql: &str, escapes: bool) -> bool {
    let words = code_words(sql, escapes);
    let Some(first) = words.first() else { return false };
    let dml = first.eq_ignore_ascii_case("update") || first.eq_ignore_ascii_case("delete");
    dml && !words.iter().any(|w| w.eq_ignore_ascii_case("where"))
}

const CONNECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

// One job's wall clock; the server may still finish a timed-out statement.
pub const QUERY_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

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
    // Passed as a path, never through URL parsing, so Windows drive letters survive.
    let opts = if conn.memory {
        SqliteConnectOptions::from_str("sqlite::memory:").map_err(e)?
    } else {
        let path = conn.path.clone().unwrap_or_else(|| ":memory:".into());
        // Only an explicit sqlite: scheme may create the file.
        SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(conn.create)
    };
    // One connection for :memory:; a pool of five would open five empty databases.
    let max = if conn.memory { 1 } else { 5 };
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(max)
        .connect_with(opts)
        .await
        .map_err(e)?;
    // A 0-byte file would be refused on the next bare open, so write the header.
    if conn.create
        && !conn.memory
        && conn
            .path
            .as_ref()
            .is_some_and(|p| std::fs::metadata(p).map(|m| m.len() == 0).unwrap_or(true))
    {
        let _ = sqlx::query("PRAGMA user_version = 0").execute(&pool).await;
    }
    Ok(pool)
}

impl Engine {
    /// Fails after the budget so a slow server cannot hang the UI.
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

    /// Postgres falls back to a database named after the user, so show what opened.
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

    /// shortcut: only single-column PKs are editable and only the default schema is browsed.
    pub async fn columns(&self, table: &str) -> Result<Vec<Col>, String> {
        match self {
            Engine::Postgres(p) => {
                let pk = pk_of_pg(p, table).await;
                let rows = sqlx::query(
                    "SELECT column_name FROM information_schema.columns \
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
                        let is_pk = name.as_ref().is_some_and(|n| pk.contains(n));
                        Col { name: name.unwrap_or_default(), pk: is_pk }
                    })
                    .collect())
            }
            Engine::MySql(p) => {
                let pk = pk_of_mysql(p, table).await;
                let rows = sqlx::query(
                    "SELECT column_name FROM information_schema.columns \
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
                        let is_pk = name.as_ref().is_some_and(|n| pk.contains(n));
                        Col { name: name.unwrap_or_default(), pk: is_pk }
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
                        pk: r.get::<i64, _>(5) != 0,
                    })
                    .collect())
            }
        }
    }

    /// shortcut: blobs come back as cast text, often garbage.
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

    /// Catalogue estimate with an exact fallback; SQLite always counts exactly.
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

    /// One statement only; results are capped at 500 rows.
    pub async fn run_sql(&self, sql_in: &str) -> Result<RunResult, String> {
        let sql = sql_in.trim();
        let sql = sql.strip_suffix(';').unwrap_or(sql);
        if sql.is_empty() {
            return Ok(RunResult { cols: vec![], rows: vec![], ok: Some(String::new()) });
        }
        let escapes = self.kind() == EngineKind::MySql;
        if statement_count(sql, escapes) > 1 {
            return Err(
                "one statement at a time: split on the semicolon and run each part".into(),
            );
        }
        let kw = first_keyword(sql, escapes);
        // A manual BEGIN would strand the pooled connection mid-transaction.
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

    /// One transaction; each update carries the old cell in its WHERE, so a concurrent change aborts the batch.
    /// shortcut: deletes are by PK and are not checked against a concurrent change.
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
// sqlx types differ per engine, so each engine keeps its own steps; the SQL comes from the builders above.
fn pk_pred(k: EngineKind, pkc: &str, pk: &str) -> String {
    if pk.is_empty() {
        format!("{pkc} IS NULL")
    } else {
        format!("{pkc} = {}", quote_literal(k, pk))
    }
}

fn conflict_msg(pk: &str, col: &str) -> String {
    format!(
        "conflict: row pk={} col={} changed by someone else. refresh (R) and retry",
        if pk.is_empty() { "NULL" } else { pk },
        col
    )
}

/// The old value is in the WHERE, so check and write are atomic; an empty original accepts NULL or ''.
fn update_sql(
    k: EngineKind,
    t: &str,
    col: &str,
    new: &str,
    pkc: &str,
    pk: &str,
    orig: &str,
) -> String {
    let c = ident(k, col);
    let v = if new == ":null" { "NULL".to_string() } else { quote_literal(k, new) };
    let old = if orig.is_empty() {
        format!("({c} IS NULL OR CAST({c} AS {}) = '')", cast_text(k))
    } else {
        format!("CAST({c} AS {}) = {}", cast_text(k), quote_literal(k, orig))
    };
    format!("UPDATE {t} SET {c} = {v} WHERE {} AND {old}", pk_pred(k, pkc, pk))
}

fn delete_sql(k: EngineKind, t: &str, pkc: &str, pk: &str) -> String {
    format!("DELETE FROM {t} WHERE {}", pk_pred(k, pkc, pk))
}

fn draft_pk(draft: &Option<String>) -> Option<&str> {
    draft.as_deref().map(str::trim).filter(|d| !d.is_empty())
}

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
    let sql = update_sql(k, &t, &ed.col, &ed.new, &pkc, &ed.pk, &ed.orig);
    let n = qs!(&sql)
        .execute(&mut **tx)
        .await
        .map_err(|err| format!("save update: {err}"))?
        .rows_affected();
    if n == 1 { Ok(()) } else { Err(conflict_msg(&ed.pk, &ed.col)) }
}

async fn pg_delete(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    pk_col: &str,
    pk: &str,
) -> Result<(), String> {
    let k = EngineKind::Postgres;
    let sql = delete_sql(k, &qtable(k, table), &ident(k, pk_col), pk);
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
            quote_literal(k, pk)
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
    let sql = update_sql(k, &t, &ed.col, &ed.new, &pkc, &ed.pk, &ed.orig);
    let n = qs!(&sql)
        .execute(&mut **tx)
        .await
        .map_err(|err| format!("save update: {err}"))?
        .rows_affected();
    if n == 1 { Ok(()) } else { Err(conflict_msg(&ed.pk, &ed.col)) }
}

async fn mysql_delete(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    table: &str,
    pk_col: &str,
    pk: &str,
) -> Result<(), String> {
    let k = EngineKind::MySql;
    let sql = delete_sql(k, &qtable(k, table), &ident(k, pk_col), pk);
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
            let ins = format!("INSERT INTO {t} ({pkc}) VALUES ({})", quote_literal(k, pk));
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
    let sql = update_sql(k, &t, &ed.col, &ed.new, &pkc, &ed.pk, &ed.orig);
    let n = qs!(&sql)
        .execute(&mut **tx)
        .await
        .map_err(|err| format!("save update: {err}"))?
        .rows_affected();
    if n == 1 { Ok(()) } else { Err(conflict_msg(&ed.pk, &ed.col)) }
}

async fn sqlite_delete(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    pk_col: &str,
    pk: &str,
) -> Result<(), String> {
    let k = EngineKind::Sqlite;
    let sql = delete_sql(k, &qtable(k, table), &ident(k, pk_col), pk);
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
        Some(pk) => format!("INSERT INTO {t} ({pkc}) VALUES ({})", quote_literal(k, pk)),
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

// sqlx's row traits are per-database, so each helper exists three times; copying is cheaper than deduping.
use sqlx::postgres::PgRow;
use sqlx::mysql::MySqlRow;
use sqlx::sqlite::SqliteRow;

fn cols_pg(rs: &[PgRow]) -> Vec<String> {
    rs.first()
        .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
        .unwrap_or_default()
}

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

/// Tries String, i64, f64, then bytes; shortcut: a u64 beyond i64 shows in float form.
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
         ON tc.constraint_schema = kcu.constraint_schema \
         AND tc.constraint_name = kcu.constraint_name \
         AND tc.table_schema = kcu.table_schema \
         AND tc.table_name = kcu.table_name \
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
         ON tc.constraint_schema = kcu.constraint_schema \
         AND tc.constraint_name = kcu.constraint_name \
         AND tc.table_schema = kcu.table_schema \
         AND tc.table_name = kcu.table_name \
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

    // The console refuses multi-statement text, so tests split their scripts.
    async fn script(e: &Engine, sql: &str) {
        for stmt in sql.split(';') {
            if !stmt.trim().is_empty() {
                e.run_sql(stmt).await.unwrap();
            }
        }
    }

    /// One connection, so the in-memory database is shared across statements.
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
        assert_eq!(page.rows[0][1], "a");
        assert_eq!(page.rows[0][2], "1.5");
        assert_eq!(e.count("t").await.unwrap(), 2);

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

        // The grid shows NULL as "", so an empty snapshot must match a NULL cell.
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
            Col { name: "id".into(), pk: true },
            Col { name: "na\"me".into(), pk: false },
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
        assert_eq!(statement_count("select 1", false), 1);
        assert_eq!(statement_count("select ';'", false), 1);
        assert_eq!(statement_count("select 1; select 2", false), 2);
        assert_eq!(statement_count("select 1 -- ; ignored\n", false), 1);
        assert_eq!(statement_count(r#"select "a;b""#, false), 1);
        // a trailing semicolon counts, and run_sql strips it before counting
        assert_eq!(statement_count("select 1;", false), 2);
    }

    #[test]
    fn statement_scan_ignores_comments() {
        assert_eq!(statement_count("select 1 /* a ; b */", false), 1);
        assert_eq!(statement_count("select 1; /* x */ select 2", false), 2);
        // a quote cannot open a comment: a -- inside a string is text
        assert_eq!(statement_count("select '--'", false), 1);
    }

    #[test]
    fn statement_scan_escapes_only_on_mysql() {
        assert_eq!(statement_count(r"select 'it\'s; x'", true), 1);
        assert_eq!(statement_count(r"select 'it\'s; x'", false), 2);
    }

    #[test]
    fn a_whole_table_mutation_is_detected() {
        for sql in [
            "update t set a = 1",
            "DELETE FROM t",
            // a keyword in a literal or a comment is not a WHERE
            "update t set note = 'where x'",
            "-- delete from t where id=1\nupdate t set a=1",
        ] {
            assert!(destructive_without_where(sql, false), "{sql}");
        }
    }

    #[test]
    fn a_where_clause_or_a_plain_select_is_not_flagged() {
        for sql in [
            "UPDATE t SET a = 1 WHERE id = 1",
            "delete from t where id = 2",
            "select 'update t set a = 1'",
            // an identifier that starts with a keyword is not that keyword
            "delete_note from t",
        ] {
            assert!(!destructive_without_where(sql, false), "{sql}");
        }
    }

    #[test]
    fn readonly_sql_is_only_the_row_queries() {
        assert!(is_read_sql("select 1", false));
        assert!(is_read_sql("  WITH x AS (SELECT 1) SELECT * FROM x", false));
        assert!(is_read_sql("PRAGMA table_info(t)", false));
        // a leading comment must not hide the keyword
        assert!(is_read_sql("-- note\nSELECT 1", false));
        assert!(!is_read_sql("update t set a = 1", false));
        assert!(!is_read_sql("drop table t", false));
    }

    #[test]
    fn a_select_cap_wraps_and_keeps_a_user_limit() {
        let capped = cap_select("SELECT id FROM t LIMIT 1");
        assert!(capped.contains("(SELECT id FROM t LIMIT 1) AS capped"), "{capped}");
        assert!(capped.ends_with(&format!("LIMIT {CONSOLE_ROW_CAP}")), "{capped}");
    }

    #[test]
    fn a_null_token_writes_a_bare_null() {
        let sql = update_sql(EngineKind::Sqlite, "t", "c", ":null", "id", "1", "old");
        assert!(sql.contains("= NULL"), "{sql}");
        assert!(!sql.contains("':null'"), "{sql}");
        let sql = update_sql(EngineKind::Sqlite, "t", "c", "x", "id", "1", "old");
        assert!(sql.contains("= 'x'"), "{sql}");
    }

    #[test]
    fn the_old_value_is_part_of_the_update() {
        let sql = update_sql(EngineKind::Sqlite, "t", "c", "new", "id", "1", "old");
        assert!(sql.contains("WHERE id = '1' AND CAST(\"c\" AS TEXT) = 'old'"), "{sql}");
        // NULL and '' both read as an empty cell, so an empty original accepts either
        let sql = update_sql(EngineKind::Sqlite, "t", "c", "new", "id", "1", "");
        assert!(sql.contains("(\"c\" IS NULL OR CAST(\"c\" AS TEXT) = '')"), "{sql}");
    }

    #[tokio::test]
    async fn a_select_with_a_user_limit_run_from_the_console() {
        let e = mem().await;
        // the old append path made this "... LIMIT 1 LIMIT 500": a syntax error
        let r = e.run_sql("SELECT id FROM t LIMIT 1").await.unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], "1");
    }

    #[tokio::test]
    async fn multiple_statements_are_refused_with_a_clear_message() {
        let e = mem().await;
        // The Postgres path wraps the query, so a second statement would be a syntax error.
        let err = e.run_sql("SELECT 1; SELECT 2").await.unwrap_err();
        assert!(err.contains("one statement"), "{err}");
        let r = e.run_sql("SELECT ';' AS x").await.unwrap();
        assert_eq!(r.rows[0][0], ";");
        assert_eq!(e.run_sql("SELECT 1;").await.unwrap().rows[0][0], "1");
    }

    #[tokio::test]
    async fn manual_transaction_is_refused() {
        let e = mem().await;
        let err = e.run_sql("BEGIN").await.unwrap_err();
        assert!(err.contains("one statement at a time"), "{err}");
        assert_eq!(e.count("t").await.unwrap(), 2);
    }

    #[tokio::test]
    async fn sqlite_file_path_roundtrip() {
        // A real path must open through Engine::connect, not URL parsing.
        let dir = std::env::temp_dir();
        let file = dir.join(format!("ru-sqlmux-test-{}.db", std::process::id()));
        let dsn = file.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&file);
        assert!(crate::config::Conn::from_dsn(&dsn).is_err(), "missing file is refused");

        let made = crate::config::Conn::from_dsn(&format!("sqlite://{dsn}")).unwrap();
        let creator = Engine::connect(&made).await.unwrap();
        script(
            &creator,
            "CREATE TABLE z (id INTEGER PRIMARY KEY, v TEXT); INSERT INTO z (v) VALUES ('hi')",
        )
        .await;
        drop(creator);

        let conn = crate::config::Conn::from_dsn(&dsn).unwrap();
        let e = Engine::connect(&conn).await.unwrap();
        assert_eq!(e.tables().await.unwrap(), vec!["z"]);
        let cols = e.columns("z").await.unwrap();
        let page = e.fetch_page("z", &cols, None, 100, 0).await.unwrap();
        assert_eq!(page.rows[0][1], "hi");
        drop(e);

        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn sqlite_scheme_creates_a_real_file() {
        // A 0-byte file would be refused on the next bare open, so keep a real header.
        let dir = std::env::temp_dir();
        let file = dir.join(format!("ru-sqlmux-new-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let dsn = file.to_string_lossy().to_string();

        let made = crate::config::Conn::from_dsn(&format!("sqlite://{dsn}")).unwrap();
        assert!(made.create);
        let engine = Engine::connect(&made).await.unwrap();
        assert!(engine.tables().await.unwrap().is_empty());
        drop(engine);

        let len = std::fs::metadata(&file).unwrap().len();
        assert!(len >= 16, "created database should carry the sqlite header, got {len} bytes");

        let bare = crate::config::Conn::from_dsn(&dsn).unwrap();
        assert!(!bare.create);
        let reopened = Engine::connect(&bare).await.unwrap();
        assert!(reopened.tables().await.unwrap().is_empty());
        drop(reopened);

        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn console_sql_paths() {
        let e = mem().await;

        // A row query decodes by value, not by cast.
        let r = e.run_sql("SELECT id, name FROM t ORDER BY id").await.unwrap();
        assert_eq!(r.cols, vec!["id", "name"]);
        assert_eq!(r.rows[0], vec!["1".to_string(), "a".to_string()]);
        assert!(r.ok.is_none());

        let r = e.run_sql("UPDATE t SET name = 'x' WHERE id = 1;").await.unwrap();
        assert_eq!(r.ok.as_deref(), Some("OK (1 rows affected)"));
        let r = e.run_sql("DELETE FROM t").await.unwrap();
        assert_eq!(r.ok.as_deref(), Some("OK (2 rows affected)"));
        assert_eq!(e.count("t").await.unwrap(), 0);

        assert_eq!(e.run_sql("  ").await.unwrap().ok.as_deref(), Some(""));
    }

    #[tokio::test]
    async fn an_empty_select_still_has_headers() {
        let e = mem().await;
        let r = e.run_sql("SELECT id, name FROM t WHERE 1 = 0").await.unwrap();
        assert_eq!(r.cols, vec!["id", "name"]);
        assert!(r.rows.is_empty());
    }

    #[tokio::test]
    async fn a_leading_comment_does_not_hide_a_select() {
        let e = mem().await;
        let r = e.run_sql("-- note\nSELECT id FROM t ORDER BY id").await.unwrap();
        assert_eq!(r.cols, vec!["id"]);
        assert_eq!(r.rows.len(), 2);
    }
}
