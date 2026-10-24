use std::path::{Path, PathBuf};
use tui_textarea::TextArea;

/// History records are separated by ASCII RS, so multi-line SQL round-trips.
const RS: char = '\u{1e}';

/// shortcut: history.txt grows without bound and is read whole at startup;
/// upgrade = rotate at a few hundred KB.
fn history_path() -> PathBuf {
    crate::config::config_dir().join("history.txt")
}

fn read_history(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|t| {
            t.split(RS)
                .map(str::to_string)
                .filter(|e| !e.trim().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Append one statement; a failed write only loses the on-disk copy, never
/// the in-memory history.
fn write_history(path: &Path, sql: &str) {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = write!(f, "{sql}{RS}");
    }
}

/// shortcut: no vim motions (tui-textarea 0.7 has none); upgrade = a real vim emulator.
pub struct Console {
    pub ta: TextArea<'static>,
    pub history: Vec<String>,
    /// history recall cursor; None = composing fresh text
    hidx: Option<usize>,
}

impl Default for Console {
    fn default() -> Self {
        Self::new()
    }
}

impl Console {
    pub fn new() -> Self {
        Self { ta: TextArea::default(), history: read_history(&history_path()), hidx: None }
    }

    pub fn set_text(&mut self, sql: &str) {
        self.ta = TextArea::from(sql.lines().map(str::to_string).collect::<Vec<_>>());
        self.hidx = None;
    }

    pub fn sql(&self) -> String {
        self.ta.lines().join("\n")
    }

    /// `dir` -1 is older and +1 newer; fresh text starts at the newest.
    pub fn recall(&mut self, dir: isize) {
        if self.history.is_empty() {
            return;
        }
        let end = self.history.len() - 1;
        let cur = self.hidx.unwrap_or(end + 1);
        let next = (cur as isize + dir).clamp(0, end as isize) as usize;
        let text = self.history[next].clone();
        self.set_text(&text);
        self.hidx = Some(next);
    }

    pub fn push_history(&mut self, sql: String) {
        if !sql.trim().is_empty() {
            write_history(&history_path(), &sql);
            self.history.push(sql);
            self.hidx = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_round_trips_multiline_sql() {
        let path =
            std::env::temp_dir().join(format!("ru-sqlmux-history-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);

        write_history(&path, "select 1");
        write_history(&path, "select\n2");

        assert_eq!(
            read_history(&path),
            vec!["select 1".to_string(), "select\n2".to_string()]
        );
        let _ = std::fs::remove_file(&path);
    }
}
