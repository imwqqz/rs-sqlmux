use ratatui::prelude::*;
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};
use std::collections::{BTreeMap, BTreeSet};

use crate::ui;

#[derive(Default)]
pub struct PendingUi {
    /// pk cell text -> col name -> (orig, new)
    pub edits: BTreeMap<String, BTreeMap<String, (String, String)>>,
    pub deletes: BTreeSet<String>,
    /// pending inserts, each carrying the pk draft typed onto it
    pub drafts: Vec<String>,
}

#[derive(Default)]
pub struct GridState {
    /// (row into rows (+drafts), col into cols)
    pub sel: (usize, usize),
    /// first scrolled column (1 or more: column 0 is frozen)
    pub col_off: usize,
    pub table_state: TableState,
}



impl GridState {
    pub fn move_row(&mut self, delta: isize, len: usize) {
        self.sel.0 = (self.sel.0 as isize + delta).clamp(0, len.saturating_sub(1) as isize) as usize;
        self.table_state.select(Some(self.sel.0));
    }

    pub fn move_col(&mut self, delta: isize, len: usize) {
        self.sel.1 = (self.sel.1 as isize + delta).clamp(0, len.saturating_sub(1) as isize) as usize;
    }

    pub fn home(&mut self) {
        self.sel.0 = 0;
        self.table_state.select(Some(0));
    }

    pub fn end(&mut self, len: usize) {
        self.sel.0 = len.saturating_sub(1);
        self.table_state.select(Some(self.sel.0));
    }
}

fn col_widths(cols: &[String], rows: &[Vec<String>]) -> Vec<usize> {
    cols.iter()
        .enumerate()
        .map(|(i, c)| {
            let mut w = c.chars().count();
            for r in rows {
                if let Some(v) = r.get(i) {
                    w = w.max(v.chars().count());
                }
            }
            (w + 2).min(35)
        })
        .collect()
}

fn fit_forward(widths: &[usize], budget: usize, start: usize) -> (usize, usize) {
    let mut end = start;
    let mut used = 0;
    while end < widths.len() {
        if used + widths[end] > budget && end > start {
            break;
        }
        used += widths[end];
        end += 1;
    }
    // degenerate: a single wider-than-budget column still shows
    (start, end.max(start + 1).min(widths.len()))
}

fn fit_back(widths: &[usize], budget: usize, last: usize) -> (usize, usize) {
    let mut start = last;
    let mut used = widths[start];
    while start > 0 && used + widths[start - 1] <= budget {
        start -= 1;
        used += widths[start];
    }
    fit_forward(widths, budget, start)
}

fn pick_window(widths: &[usize], budget: usize, sel: usize, col_off: usize) -> (usize, usize) {
    let win = fit_forward(widths, budget, col_off);
    if sel < win.0 {
        fit_back(widths, budget, sel)
    } else if sel >= win.1 {
        fit_forward(widths, budget, sel)
    } else {
        win
    }
}

/// Column 0 is frozen; the rest scroll in the width it leaves.
fn visible_cols(widths: &[usize], budget: usize, sel: usize, col_off: usize) -> Vec<usize> {
    let avail = budget.saturating_sub(widths[0]);
    let (start, end) = if sel == 0 {
        fit_forward(widths, avail, col_off.max(1).min(widths.len()))
    } else {
        let off = col_off.saturating_sub(1).min(widths.len() - 1);
        let (s, e) = pick_window(&widths[1..], avail, sel - 1, off);
        (s + 1, e + 1)
    };
    std::iter::once(0).chain(start..end).collect()
}

struct RowCtx<'a> {
    cols: &'a [String],
    win: &'a [usize],
    pk_idx: Option<usize>,
    pending: &'a PendingUi,
    sel: (usize, usize),
}

fn data_row(row: &[String], ri: usize, ctx: &RowCtx<'_>) -> Row<'static> {
    let pk = ctx.pk_idx.and_then(|p| row.get(p).cloned()).unwrap_or_default();
    let is_del = ctx.pending.deletes.contains(&pk);
    let cells: Vec<Cell> = ctx
        .win
        .iter()
        .enumerate()
        .map(|(pos, &ci)| {
            let txt = row.get(ci).cloned().unwrap_or_default();
            let mut style = Style::default();
            if is_del {
                style = style.fg(Color::Red).crossed_out();
            } else if ctx
                .pending
                .edits
                .get(&pk)
                .is_some_and(|ed| ed.contains_key(&ctx.cols[ci]))
            {
                style = style.fg(Color::Yellow);
            }
            if (ri, ci) == ctx.sel {
                style = style.bg(Color::Rgb(80, 80, 80));
            }
            Cell::from(with_sep(pos, txt)).style(style)
        })
        .collect();
    Row::new(cells)
}

/// A pending insert shows only its PK draft; the other cells stay empty.
fn ghost_row(gi: usize, data_len: usize, ctx: &RowCtx<'_>) -> Row<'static> {
    let draft = ctx.pending.drafts.get(gi).cloned().unwrap_or_default();
    let cells: Vec<Cell> = ctx
        .win
        .iter()
        .enumerate()
        .map(|(pos, &ci)| {
            let is_pk = ctx.pk_idx == Some(ci);
            let txt = if is_pk { draft.clone() } else { String::new() };
            let mut style = Style::default().fg(Color::Yellow).italic();
            if is_pk && ctx.sel.0 == data_len + gi {
                style = style.bg(Color::Rgb(80, 80, 80));
            }
            Cell::from(with_sep(pos, txt)).style(style)
        })
        .collect();
    Row::new(cells)
}

/// A divider between columns, taken from the padding each width already reserves.
fn with_sep(pos: usize, txt: String) -> String {
    if pos == 0 { txt } else { format!("│{txt}") }
}

#[allow(clippy::too_many_arguments)]
pub fn render(
    f: &mut Frame,
    area: Rect,
    title: String,
    focused: bool,
    cols: &[String],
    rows: &[Vec<String>],
    pk_idx: Option<usize>,
    pending: &PendingUi,
    st: &mut GridState,
) {
    if cols.is_empty() {
        f.render_widget(
            Paragraph::new("select a table (Sidebar → Enter) or run SQL")
                .style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }
    let widths = col_widths(cols, rows);
    let budget = area.width.saturating_sub(2) as usize;
    let win = visible_cols(&widths, budget, st.sel.1, st.col_off);
    st.col_off = win.get(1).copied().unwrap_or(1);
    let hidden = cols.len() - win.len();
    let title = if hidden > 0 {
        format!("{title} (+{hidden} cols)")
    } else {
        title
    };

    let header: Vec<Cell> = win
        .iter()
        .enumerate()
        .map(|(pos, &i)| {
            let name = if Some(i) == pk_idx {
                format!("*{}", cols[i])
            } else {
                cols[i].clone()
            };
            Cell::from(with_sep(pos, name)).style(Style::default().fg(Color::Black).bg(Color::Gray))
        })
        .collect();

    let ctx = RowCtx { cols, win: &win, pk_idx, pending, sel: st.sel };
    let mut table_rows: Vec<Row> = Vec::with_capacity(rows.len() + pending.drafts.len());
    for (ri, row) in rows.iter().enumerate() {
        table_rows.push(data_row(row, ri, &ctx));
    }
    for gi in 0..pending.drafts.len() {
        table_rows.push(ghost_row(gi, rows.len(), &ctx));
    }

    let constraints: Vec<Constraint> = win
        .iter()
        .map(|&i| Constraint::Length(widths[i] as u16))
        .chain(std::iter::once(Constraint::Min(0)))
        .collect();
    let table = Table::new(table_rows, constraints)
        .header(Row::new(header))
        .column_spacing(0)
        .row_highlight_style(Style::default())
        .block(ui::block(&title, focused));
    st.table_state.select(Some(st.sel.0));
    f.render_stateful_widget(table, area, &mut st.table_state);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf.cell((x, y)).map_or(' ', |c| c.symbol().chars().next().unwrap_or(' ')))
            .collect()
    }

    // the selection must stay inside the window; a regression only shows on screen
    fn inside(win: (usize, usize), sel: usize) -> bool {
        win.0 <= sel && sel < win.1 && win.1 > win.0
    }

    #[test]
    fn a_wide_budget_keeps_every_column_and_the_selection() {
        let widths = [5, 5, 5];
        assert_eq!(fit_forward(&widths, 100, 0), (0, 3));
        assert!(inside(pick_window(&widths, 100, 2, 0), 2));
    }

    #[test]
    fn a_selection_right_of_the_window_slides_forward_into_view() {
        let widths = [10, 10, 10, 10];
        let win = pick_window(&widths, 10, 3, 0);
        assert_eq!(win, (3, 4));
        assert!(inside(win, 3));
    }

    #[test]
    fn a_selection_left_of_the_window_slides_back_into_view() {
        let widths = [10, 10, 10, 10];
        let win = pick_window(&widths, 10, 0, 3);
        assert_eq!(win, (0, 1));
        assert!(inside(win, 0));
    }

    #[test]
    fn a_column_wider_than_the_budget_still_shows_and_covers_itself() {
        let widths = [40, 5];
        let win = pick_window(&widths, 10, 0, 0);
        assert_eq!(win, (0, 1));
        assert!(inside(win, 0));
    }

    #[test]
    fn the_last_column_is_reachable() {
        let widths = [5, 5, 5];
        let win = pick_window(&widths, 10, 2, 0);
        assert!(inside(win, 2), "{win:?}");
        assert!(win.1 <= widths.len());
    }

    #[test]
    fn the_first_column_stays_frozen_while_the_rest_scroll() {
        let widths = [10, 10, 10, 10];
        let win = visible_cols(&widths, 30, 3, 0);
        assert_eq!(win, vec![0, 3], "the selection slides into view, column 0 stays");
        let win = visible_cols(&widths, 30, 0, 3);
        assert_eq!(win, vec![0, 3]);
        assert_eq!(visible_cols(&[10], 30, 0, 0), vec![0]);
    }

    #[test]
    fn a_window_that_fits_keeps_every_column_exactly_once() {
        let widths = [5, 5, 5];
        assert_eq!(visible_cols(&widths, 100, 2, 0), vec![0, 1, 2]);
    }

    #[test]
    fn a_divider_sits_between_columns() {
        use ratatui::backend::TestBackend;
        let mut term = ratatui::Terminal::new(TestBackend::new(20, 5)).unwrap();
        let mut st = GridState::default();
        let pending = PendingUi::default();
        let cols = ["a".to_string(), "b".to_string()];
        let rows = vec![vec!["1".to_string(), "2".to_string()]];
        term.draw(|f| {
            render(f, f.area(), "t".into(), false, &cols, &rows, None, &pending, &mut st);
        })
        .unwrap();

        let buf = term.backend().buffer();
        let head = row_text(buf, 1);
        assert!(head.starts_with("│a  │b"), "{head}");
        let row = row_text(buf, 2);
        assert!(row.starts_with("│1  │2"), "{row}");
    }

    #[test]
    fn a_scrolled_window_keeps_the_frozen_column_clean() {
        use ratatui::backend::TestBackend;
        let mut term = ratatui::Terminal::new(TestBackend::new(12, 5)).unwrap();
        let mut st = GridState { sel: (0, 2), ..Default::default() };
        let pending = PendingUi::default();
        let cols = ["aaa".to_string(), "bbb".to_string(), "ccc".to_string()];
        let rows = vec![vec!["111".to_string(), "222".to_string(), "333".to_string()]];
        term.draw(|f| {
            render(f, f.area(), "t".into(), false, &cols, &rows, None, &pending, &mut st);
        })
        .unwrap();

        let buf = term.backend().buffer();
        let head = row_text(buf, 1);
        assert!(head.starts_with("│aaa"), "{head}");
        assert!(head.contains("aaa  │ccc"), "{head}");
        let row = row_text(buf, 2);
        assert!(row.starts_with("│111"), "{row}");
        assert!(row.contains("111  │333"), "{row}");
    }
}
