use serde::Deserialize;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// A file without this header is not a database, so a bare path to it is refused.
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

fn looks_like_sqlite(path: &Path) -> bool {
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 16];
    file.read_exact(&mut head).is_ok() && &head == SQLITE_MAGIC
}

#[derive(Deserialize, Default, Clone)]
pub struct ConnDef {
    pub dsn: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<bool>,
    /// Refuses cell saves and non-row SQL from the console.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readonly: Option<bool>,
    /// Wall clock for one background job, in seconds (default 30).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_timeout: Option<u64>,
}

impl ConnDef {
    pub fn is_readonly(&self) -> bool {
        self.readonly.unwrap_or(false)
    }

    /// A profile that sets 0 would time out every job, so the floor is one second.
    pub fn timeout(&self) -> std::time::Duration {
        let secs = self.query_timeout.unwrap_or(crate::db::QUERY_BUDGET.as_secs());
        std::time::Duration::from_secs(secs.max(1))
    }
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

/// DSN plus resolved engine; `env:VAR` segments are expanded from the environment.
#[derive(Clone, Debug)]
pub struct Conn {
    pub kind: EngineKind,
    pub url: String,
    /// SQLite only; kept out of `url` so Windows drive letters never go through URL parsing.
    pub path: Option<PathBuf>,
    pub memory: bool,
    /// SQLite only: only a `sqlite:` DSN may create the file, so a typo in a
    /// bare path cannot invent a database.
    pub create: bool,
    pub db_name: Option<String>,
}

impl Conn {
    /// Parses a DSN, expands `env:VAR`, and refuses a server DSN or bare path that cannot open.
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
            EngineKind::Sqlite
        } else if dsn.contains("://") || dsn.contains(|c: char| c.is_whitespace()) {
            return Err(format!("unsupported or incomplete DSN: {}", redact(dsn)));
        } else {
            EngineKind::Sqlite
        };
        if kind != EngineKind::Sqlite && host_of(dsn).is_empty() {
            return Err(format!("incomplete DSN, no host: {}", redact(dsn)));
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

    /// Loads profiles from `connections.toml`; a missing file is not an error.
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
            .map(|v| ConnDef {
                dsn: v.dsn,
                default: v.default,
                readonly: v.readonly,
                query_timeout: v.query_timeout,
            })
            .collect())
    }
}

pub fn config_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home).join(".config").join("ru-sqlmux");
    }
    if let Ok(appdata) = std::env::var("APPDATA")
        && !appdata.is_empty()
    {
        return PathBuf::from(appdata).join("ru-sqlmux");
    }
    PathBuf::from(".config").join("ru-sqlmux")
}

fn host_of(dsn: &str) -> &str {
    let rest = dsn.split_once("://").map(|(_, r)| r).unwrap_or("");
    let host = rest.rsplit_once('@').map(|(_, h)| h).unwrap_or(rest);
    host.split(['/', '?', '#']).next().unwrap_or("")
}

/// Database name from the DSN path; the userinfo is stripped from the right because a password may contain a slash.
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

fn expand_env(dsn: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = dsn;
    while let Some(at) = rest.find("env:") {
        let after = &rest[at + 4..];
        let name_len = after
            .find(|c: char| c.is_whitespace() || c == '"' || c == ';' || c == '@' || c == '/')
            .unwrap_or(after.len());
        let name = &after[..name_len];
        let val = std::env::var(name)
            .map_err(|_| format!("env var {name} not set (dsn uses env:{name})"))?;
        out.push_str(&rest[..at]);
        out.push_str(&val);
        rest = &after[name_len..];
    }
    out.push_str(rest);
    Ok(out)
}

/// DSN with the userinfo replaced, for messages that end up on screen.
fn redact(dsn: &str) -> String {
    let Some((scheme, rest)) = dsn.split_once("://") else {
        return dsn.to_string();
    };
    match rest.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://***@{host}"),
        None => dsn.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_flags_default_off() {
        let parsed: Connections = toml::from_str(
            r#"
            [connection.ro]
            dsn = "sqlite://a.db"
            readonly = true
            query_timeout = 90

            [connection.fast]
            dsn = "sqlite://b.db"
            "#,
        )
        .unwrap();
        let ro = &parsed.connection["ro"];
        assert!(ro.is_readonly());
        assert_eq!(ro.timeout().as_secs(), 90);
        let fast = &parsed.connection["fast"];
        assert!(!fast.is_readonly());
        assert_eq!(fast.timeout().as_secs(), 30);

        // zero would time out every statement at once
        let zero: Connections = toml::from_str("[connection.z]\ndsn='x'\nquery_timeout=0\n").unwrap();
        assert_eq!(zero.connection["z"].timeout().as_secs(), 1);
    }

    #[test]
    fn kinds() {
        assert_eq!(Conn::from_dsn("postgres://h/db").unwrap().kind, EngineKind::Postgres);
        assert_eq!(Conn::from_dsn("mysql://u:p@h/db").unwrap().kind, EngineKind::MySql);
        assert_eq!(Conn::from_dsn("sqlite://app.db").unwrap().kind, EngineKind::Sqlite);
        assert_eq!(Conn::from_dsn("sqlite://app.db").unwrap().url, "sqlite:app.db");
        assert_eq!(Conn::from_dsn("sqlite::memory:").unwrap().url, "sqlite::memory:");
        assert!(Conn::from_dsn("sqlite://brand-new.db").unwrap().create);
    }

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
            "postgrs://localhost/db",
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

    #[test]
    fn env_marker_at_the_end_of_the_dsn() {
        // no @, / or whitespace after the marker: the old slice ran past the end
        unsafe { std::env::set_var("RUSQLMUX_TAIL_PW", "pw") };
        let c = Conn::from_dsn("postgres://u@h/db?password=env:RUSQLMUX_TAIL_PW").unwrap();
        assert!(c.url.ends_with("password=pw"), "{}", c.url);
    }

    #[test]
    fn a_bad_dsn_does_not_echo_the_password() {
        let err = Conn::from_dsn("mysql://me:s3cret@/db").unwrap_err();
        assert!(!err.contains("s3cret"), "{err}");
        assert!(err.contains("***@"), "{err}");
    }
}
