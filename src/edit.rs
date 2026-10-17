use crate::config::EngineKind;

/// identifier quoting: doubles the quote char instead of trusting the SQL layer.
pub fn quote_ident(k: EngineKind, s: &str) -> String {
    match k {
        EngineKind::MySql => format!("`{}`", s.replace('`', "``")),
        _ => format!("\"{}\"", s.replace('"', "\"\"")),
    }
}

/// value literal quoting. shortcut: everything ships as a quoted string and the
/// server coerces (SQLite affinity, MySQL implicit cast, PG unknown-type
/// coercion). Numbers-as-strings can therefore round-trip `1.50` as text into a
/// REAL column; when precision packaging matters, add a typed-literal path.
pub fn quote_literal(v: &str) -> String {
    format!("'{}'", v.replace('\'', "''"))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Edit {
    /// text value of the row's single-column PK ("" = NULL row identity)
    pub pk: String,
    pub col: String,
    pub orig: String,
    pub new: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub updates: usize,
    pub deletes: usize,
    /// PK text values of rows INSERT'd during save
    pub inserts: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EngineKind::*;

    #[test]
    fn ident_quoting() {
        assert_eq!(quote_ident(Sqlite, "name"), r#""name""#);
        assert_eq!(quote_ident(Sqlite, "co\"l"), r#""co""l""#);
        assert_eq!(quote_ident(MySql, "co`l"), "`co``l`");
    }

    #[test]
    fn literal_quoting() {
        assert_eq!(quote_literal("0'Conr"), "'0''Conr'");
        assert_eq!(quote_literal("'; DROP TABLE u;--"), "'''; DROP TABLE u;--'");
        // an embedded quote is always doubled, so it can never end the literal
        assert_eq!(quote_literal("x'"), "'x'''");
        assert_eq!(quote_literal(""), "''");
    }
}
