use serde::Deserialize;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Real SQLite files start with this 16 byte header. A zero byte file, a text
/// file, or anything else is not a database, so a bare path to one is refused
/// instead of being opened as a blank database.
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

fn looks_like_sqlite(path: &Path) -> bool {
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 16];
    file.read_exact(&mut head).is_ok() && &head == SQLITE_MAGIC
}

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

/// DSN + resolved engine kind. A bare path opens an existing SQLite file;
/// `sqlite:`/`sqlite://` may create a new one. `env:VAR` segments are expanded
/// from the process environment (e.g. password).
#[derive(Clone, Debug)]
pub struct Conn {
    pub kind: EngineKind,
    pub url: String,
    /// sqlite only: filesystem path (kept separate from `url` so Windows drive
    /// letters never go through URL parsing) and whether it is `:memory:`.
    pub path: Option<PathBuf>,
    pub memory: bool,
    /// sqlite only: the user named the file explicitly (`sqlite:` scheme), so a
    /// missing file may be created. A bare path must already exist, otherwise a
    /// typo would silently invent an empty database.
    pub create: bool,
    /// server only: database name from the DSN path, when one was given
    pub db_name: Option<String>,
}

impl Conn {
    /// Parse a user-supplied DSN with optional `env:VAR` expansion markers.
    /// `env:NAME` is replaced with the process env value (passwords, hosts).
    /// Rejects incomplete server DSNs and bare paths that do not exist, so the
    /// caller can report a real failure instead of opening an empty database.
    pub fn from_dsn(dsn: &str) -> Result<Self, String> {
        let dsn = expand_env(dsn)?;
        let dsn = dsn.trim();
        if dsn.is_empty() {
            return Err("empty DSN".into());
        }
        let kind = if dsn.starts_with("postgres://") || dsn.starts_with("postgresql://") {
            EngineKind::Postgres
        } else if dsn.starts_with("mysql://") {
            EngineKind::MySql
        } else if dsn.starts_with("sqlite:") {
            // covers sqlite: and sqlite://
            EngineKind::Sqlite
        } else if dsn.contains("://") || dsn.contains(|c: char| c.is_whitespace()) {
            // neither a known scheme nor a plausible file path
            return Err(format!("unsupported or incomplete DSN: {dsn}"));
        } else {
            // bare path: an existing sqlite file, never a new one
            EngineKind::Sqlite
        };
        if kind != EngineKind::Sqlite && host_of(dsn).is_empty() {
            return Err(format!("incomplete DSN, no host: {dsn}"));
        }
        let mut path = None;
        let mut memory = false;
        let mut create = false;
        let mut db_name = None;
        let url = match kind {
            EngineKind::Sqlite => {
                let explicit = dsn.starts_with("sqlite:");
                let p = dsn.strip_prefix("sqlite://").unwrap_or(dsn);
                let p = p.strip_prefix("sqlite:").unwrap_or(p);
                memory = p == ":memory:" || p.starts_with(":memory:?");
                if !memory {
                    let file = PathBuf::from(p);
                    if !explicit {
                        if !file.exists() {
                            return Err(format!("no such database file: {}", file.display()));
                        }
                        if !looks_like_sqlite(&file) {
                            return Err(format!(
                                "{} is not a SQLite database; use sqlite:// to create a new one",
                                file.display()
                            ));
                        }
                    }
                    create = explicit;
                    path = Some(file);
                }
                format!("sqlite:{p}")
            }
            _ => {
                db_name = dsn_name(dsn);
                dsn.to_string()
            }
        };
        Ok(Conn { kind, url, path, memory, create, db_name })
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

/// Host portion of a server DSN (`postgres://u:p@host:5432/db` → `host:5432`).
fn host_of(dsn: &str) -> &str {
    let rest = dsn.split_once("://").map(|(_, r)| r).unwrap_or("");
    let host = rest.rsplit_once('@').map(|(_, h)| h).unwrap_or(rest);
    host.split(['/', '?', '#']).next().unwrap_or("")
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
        assert_eq!(Conn::from_dsn("sqlite://app.db").unwrap().kind, EngineKind::Sqlite);
        assert_eq!(Conn::from_dsn("sqlite://app.db").unwrap().url, "sqlite:app.db");
        assert_eq!(Conn::from_dsn("sqlite::memory:").unwrap().url, "sqlite::memory:");
        // an explicit sqlite scheme may create the file
        assert!(Conn::from_dsn("sqlite://brand-new.db").unwrap().create);
    }

    /// Write a valid SQLite header, enough for the bare-path check.
    fn write_sqlite_header(path: &Path) {
        use std::io::Write;
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(SQLITE_MAGIC).unwrap();
    }

    #[test]
    fn bare_path_must_already_exist() {
        let dir = std::env::temp_dir();
        let file = dir.join(format!("ru-sqlmux-cfg-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let dsn = file.to_string_lossy().to_string();

        let err = Conn::from_dsn(&dsn).expect_err("missing file is refused");
        assert!(err.contains("no such database file"), "{err}");

        write_sqlite_header(&file);
        let c = Conn::from_dsn(&dsn).unwrap();
        assert_eq!(c.kind, EngineKind::Sqlite);
        assert!(!c.create, "an existing bare path is opened, not created");
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn rejects_files_that_are_not_databases() {
        let dir = std::env::temp_dir();
        let empty = dir.join(format!("ru-sqlmux-empty-{}.db", std::process::id()));
        let text = dir.join(format!("ru-sqlmux-text-{}.db", std::process::id()));

        // a zero byte file is what an accidental "touch a" leaves behind
        std::fs::File::create(&empty).unwrap();
        let err = Conn::from_dsn(&empty.to_string_lossy()).expect_err("empty file refused");
        assert!(err.contains("is not a SQLite database"), "{err}");

        std::fs::write(&text, b"hello, this is not a database").unwrap();
        let err = Conn::from_dsn(&text.to_string_lossy()).expect_err("text file refused");
        assert!(err.contains("is not a SQLite database"), "{err}");

        // the explicit scheme is still allowed to create a new file
        assert!(Conn::from_dsn(&format!("sqlite://{}", empty.to_string_lossy())).is_ok());

        let _ = std::fs::remove_file(&empty);
        let _ = std::fs::remove_file(&text);
    }

    #[test]
    fn rejects_unusable_dsns() {
        for bad in [
            "",
            "   ",
            "postgres://",
            "postgres://user@",
            "postgresql:///db",
            "mysql://",
            "postgrs://localhost/db", // typo: no known scheme, but a URL shape
            "not a dsn at all",
        ] {
            assert!(Conn::from_dsn(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn sqlite_path_kept_out_of_url_parsing() {
        // drive letters and backslashes must survive, so the path is handed to
        // the driver as a path, not parsed as a URL
        let dir = std::env::temp_dir();
        let file = dir.join(format!("ru-sqlmux-path-{}.db", std::process::id()));
        write_sqlite_header(&file);
        let c = Conn::from_dsn(&file.to_string_lossy()).unwrap();
        assert!(!c.memory);
        assert_eq!(c.path, Some(file.clone()));
        let m = Conn::from_dsn("sqlite::memory:").unwrap();
        assert!(m.memory);
        assert_eq!(m.path, None);
        let _ = std::fs::remove_file(&file);
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
