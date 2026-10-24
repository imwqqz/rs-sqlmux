use crate::config::EngineKind;

/// Doubles the quote char instead of trusting the SQL layer.
pub fn quote_ident(k: EngineKind, s: &str) -> String {
    match k {
        EngineKind::MySql => format!("`{}`", s.replace('`', "``")),
        _ => format!("\"{}\"", s.replace('"', "\"\"")),
    }
}

/// MySQL needs backslashes doubled; every value ships as text, so `1.50` can land in a REAL column as text.
pub fn quote_literal(k: EngineKind, v: &str) -> String {
    let esc = match k {
        EngineKind::MySql => v.replace('\\', "\\\\").replace('\'', "''"),
        _ => v.replace('\'', "''"),
    };
    format!("'{esc}'")
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
        assert_eq!(quote_literal(Sqlite, "0'Conr"), "'0''Conr'");
        assert_eq!(quote_literal(Sqlite, "'; DROP TABLE u;--"), "'''; DROP TABLE u;--'");
        // an embedded quote is always doubled, so it can never end the literal
        assert_eq!(quote_literal(Sqlite, "x'"), "'x'''");
        assert_eq!(quote_literal(Sqlite, ""), "''");
    }

    #[test]
    fn mysql_doubles_a_backslash_so_it_cannot_escape_the_quote() {
        assert_eq!(quote_literal(MySql, r"a\'b"), r"'a\\''b'");
        // the payload that used to close the string and comment out the WHERE
        let sql = format!("UPDATE t SET c = {}", quote_literal(MySql, r"\' OR 1=1 -- "));
        assert!(sql.contains(r"\\'' OR 1=1"), "{sql}");
        assert_eq!(quote_literal(Sqlite, r"a\'b"), r"'a\''b'");
    }
}
