        use tui_textarea::TextArea;

/// Bottom pane: SQL editor (tui-textarea) + recallable history.
/// shortcut: no vim motions in the editor (tui-textarea 0.7 has no vim mode);
/// C-p/C-n recall history, Enter runs the query. Upgrade = real vim emulator.
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
        Self { ta: TextArea::default(), history: Vec::new(), hidx: None }
    }

    pub fn set_text(&mut self, sql: &str) {
        self.ta = TextArea::from(sql.lines().map(str::to_string).collect::<Vec<_>>());
        self.hidx = None;
    }

    pub fn sql(&self) -> String {
        self.ta.lines().join("\n")
    }

    /// Walk history: -1 older, +1 newer. Fresh composing starts at the newest.
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
            self.history.push(sql);
            self.hidx = None;
        }
    }
}
