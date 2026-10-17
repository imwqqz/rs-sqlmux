use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

#[derive(Deserialize, Default)]
pub struct ConnDef {
    pub dsn: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<bool>,
}

#[derive(Deserialize, Default)]
pub struct Connections {
    #[serde(default)]
    pub connection: std::collections::BTreeMap<String, ConnDef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    Postgres,
    MySql,
    Sqlite,
}

/// DSN + resolved engine kind. A bare path (or `*.db`/`*.sqlite`) opens as SQLite;
/// `env:VAR` segments are expanded from the process environment (e.g. password).
#[derive(Clone)]
pub struct Conn {
    pub kind: EngineKind,
    pub url: String,
    /// sqlite only: filesystem path (kept separate from `url` so Windows drive
    /// letters never go through URL parsing) and whether it is `:memory:`.
    pub path: Option<PathBuf>,
    pub memory: bool,
    /// server only: database name from the DSN path, when one was given
    pub db_name: Option<String>,
}

impl Conn {
    /// Parse a user-supplied DSN with optional `env:VAR` expansion markers.
    /// `env:NAME` is replaced with the process env value (passwords, hosts).
    pub fn from_dsn(dsn: &str) -> Result<Self, String> {
        let dsn = expand_env(dsn)?;
        let kind = if dsn.starts_with("postgres://") || dsn.starts_with("postgresql://") {
            EngineKind::Postgres
        } else if dsn.starts_with("mysql://") {
            EngineKind::MySql
        } else if dsn.starts_with("sqlite:") {
            // covers sqlite: and sqlite://
            EngineKind::Sqlite
        } else {
            // bare path: treat as a sqlite file (use the local file, not a server)
            EngineKind::Sqlite
        };
        let mut path = None;
        let mut memory = false;
        let mut db_name = None;
        let url = match kind {
            EngineKind::Sqlite => {
                let p = dsn.strip_prefix("sqlite://").unwrap_or(&dsn);
                let p = p.strip_prefix("sqlite:").unwrap_or(p);
                memory = p == ":memory:" || p.starts_with(":memory:?");
                if !memory {
                    path = Some(PathBuf::from(p));
                }
                format!("sqlite:{p}")
            }
            _ => {
                db_name = dsn_name(&dsn);
                dsn
            }
        };
        Ok(Conn { kind, url, path, memory, db_name })
    }

    /// Load saved connections (< 5) from `~/.config/ru-sqlmux/connections.toml`.
    /// Missing file is not an error; startup without a DSN falls back to the
    /// connection named "default".
    pub fn load_all() -> Result<Vec<ConnDef>, String> {
        let cfg = config_dir().join("connections.toml");
        let Ok(text) = fs::read_to_string(&cfg) else {
            return Ok(Vec::new());
        };
        let parsed: Connections = toml::from_str(&text)
            .map_err(|e| format!("{}: {e}", cfg.display()))?;
        Ok(parsed
            .connection
            .into_values()
            .map(|v| ConnDef { dsn: v.dsn, default: v.default })
            .collect())
    }
}

pub fn config_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home).join(".config").join("ru-sqlmux");
    }
    // Windows: %APPDATA%
    if let Ok(appdata) = std::env::var("APPDATA")
        && !appdata.is_empty()
    {
        return PathBuf::from(appdata).join("ru-sqlmux");
    }
    PathBuf::from(".config").join("ru-sqlmux")
}

/// Database name from a server DSN's path segment (`postgres://h:5432/app` → `app`).
/// Postgres defaults to a database named after the user when this is absent,
/// so showing it surfaces that trap early. The userinfo is stripped from the
/// right, because a password may contain a slash.
fn dsn_name(dsn: &str) -> Option<String> {
    let rest = dsn.split_once("://").map(|(_, r)| r).unwrap_or(dsn);
    let host = rest.rsplit_once('@').map(|(_, h)| h).unwrap_or(rest);
    let after_host = host.split_once('/').map(|(_, r)| r)?.split(['?', '#']).next().unwrap_or("");
    if after_host.is_empty() {
        None
    } else {
        Some(after_host.to_string())
    }
}

/// Replace every `env:NAME` with process env NAME; empty result is an error.
fn expand_env(dsn: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = dsn;
    while let Some(at) = rest.find("env:") {
        // marker name runs up to whitespace, quote or semicolon
        let after = &rest[at + 4..];
        let name_end = after
            .find(|c: char| c.is_whitespace() || c == '"' || c == ';' || c == '@' || c == '/')
            .map(|k| k + 4)
            .unwrap_or(rest.len());
        let name = &rest[at + 4..at + name_end];
        let val = std::env::var(name)
            .map_err(|_| format!("env var {name} not set (dsn uses env:{name})"))?;
        out.push_str(&rest[..at]);
        out.push_str(&val);
        rest = &rest[at + name_end..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(Conn::from_dsn("postgres://h/db").unwrap().kind, EngineKind::Postgres);
        assert_eq!(Conn::from_dsn("mysql://u:p@h/db").unwrap().kind, EngineKind::MySql);
        assert_eq!(Conn::from_dsn("app.db").unwrap().kind, EngineKind::Sqlite);
        assert_eq!(Conn::from_dsn("sqlite://app.db").unwrap().url, "sqlite:app.db");
        assert_eq!(Conn::from_dsn("./data/app.db").unwrap().url, "sqlite:./data/app.db");
        assert_eq!(Conn::from_dsn("sqlite::memory:").unwrap().url, "sqlite::memory:");
    }

    #[test]
    fn sqlite_path_kept_out_of_url_parsing() {
        // drive letters and backslashes must survive
        let c = Conn::from_dsn(r"D:\dev\data\app.db").unwrap();
        assert!(c.memory == false);
        assert_eq!(c.path, Some(PathBuf::from(r"D:\dev\data\app.db")));
        let m = Conn::from_dsn("sqlite::memory:").unwrap();
        assert!(m.memory);
        assert_eq!(m.path, None);
    }

    #[test]
    fn server_dsn_name() {
        assert_eq!(Conn::from_dsn("postgres://me@localhost:5432/app").unwrap().db_name.as_deref(), Some("app"));
        assert_eq!(
            Conn::from_dsn("postgres://me@localhost:5432/app?sslmode=disable").unwrap().db_name.as_deref(),
            Some("app")
        );
        // no /dbname: Postgres would silently open a DB named after the user
        assert_eq!(Conn::from_dsn("postgres://me@localhost:5432").unwrap().db_name, None);
        // a slash inside the password must not become the "database"
        assert_eq!(
            Conn::from_dsn("postgres://me:P/3n@localhost:5432/app").unwrap().db_name.as_deref(),
            Some("app")
        );
    }

    #[test]
    fn env_expand() {
        // Rust 2024: env mutation is unsafe (process-global)
        unsafe { std::env::set_var("RUSQLMUX_TEST_PW", "s3cret") };
        let c = Conn::from_dsn("mysql://u:env:RUSQLMUX_TEST_PW@h/db").unwrap();
        assert_eq!(c.url, "mysql://u:s3cret@h/db");
        assert!(Conn::from_dsn("mysql://u:env:NOPE_XYZ_42@h/db").is_err());
    }
}
