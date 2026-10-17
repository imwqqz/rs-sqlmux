use ratatui::prelude::*;
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};
use std::collections::{BTreeMap, BTreeSet};

use crate::ui;

/// Everything the grid needs to show unsaved work.
#[derive(Default)]
pub struct PendingUi {
    /// pk cell text -> col name -> (orig, new)
    pub edits: BTreeMap<String, BTreeMap<String, (String, String)>>,
    pub deletes: BTreeSet<String>,
    /// pending inserted rows, each carrying the pk draft typed onto it
    pub drafts: Vec<String>,
}

#[derive(Default)]
pub struct GridState {
    /// (row into rows (+drafts), col into cols)
    pub sel: (usize, usize),
    /// first visible column (kept in sync by render)
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

/// Width per column: widest of header/cells, capped.
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
            (w + 2).min(34)
        })
        .collect()
}

/// Window [start..end) of columns that fits `budget`, starting at `start`.
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

/// Window ending at column `last`, grown backwards. Used when the cursor moves
/// left out of view.
fn fit_back(widths: &[usize], budget: usize, last: usize) -> (usize, usize) {
    let mut start = last;
    let mut used = widths[start];
    while start > 0 && used + widths[start - 1] <= budget {
        start -= 1;
        used += widths[start];
    }
    fit_forward(widths, budget, start)
}

/// The visible column window: keep `sel` inside, sliding from `col_off`.
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

/// What the row builders need to know to style cells.
struct RowCtx<'a> {
    cols: &'a [String],
    win: &'a [usize],
    pk_idx: Option<usize>,
    pending: &'a PendingUi,
    sel: (usize, usize),
}

/// One data row, with deleted / edited / cursor styling applied.
fn data_row(row: &[String], ri: usize, ctx: &RowCtx<'_>) -> Row<'static> {
    let pk = ctx.pk_idx.and_then(|p| row.get(p).cloned()).unwrap_or_default();
    let is_del = ctx.pending.deletes.contains(&pk);
    let cells: Vec<Cell> = ctx
        .win
        .iter()
        .map(|&ci| {
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
            Cell::from(txt).style(style)
        })
        .collect();
    Row::new(cells)
}

/// One pending-insert row: the pk draft shows yellow, other cells stay empty.
fn ghost_row(gi: usize, data_len: usize, ctx: &RowCtx<'_>) -> Row<'static> {
    let draft = ctx.pending.drafts.get(gi).cloned().unwrap_or_default();
    let cells: Vec<Cell> = ctx
        .win
        .iter()
        .map(|&ci| {
            let is_pk = ctx.pk_idx == Some(ci);
            let txt = if is_pk { draft.clone() } else { String::new() };
            let mut style = Style::default().fg(Color::Yellow).italic();
            if is_pk && ctx.sel.0 == data_len + gi {
                style = style.bg(Color::Rgb(80, 80, 80));
            }
            Cell::from(txt).style(style)
        })
        .collect();
    Row::new(cells)
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
    let (start, end) = pick_window(&widths, budget, st.sel.1, st.col_off);
    st.col_off = start;
    let win: Vec<usize> = (start..end).collect();
    let hidden = cols.len() - win.len();
    let title = if hidden > 0 {
        format!("{title} (+{hidden} cols)")
    } else {
        title
    };

    let header: Vec<Cell> = win
        .iter()
        .map(|&i| {
            let name = if Some(i) == pk_idx {
                format!("*{}", cols[i])
            } else {
                cols[i].clone()
            };
            Cell::from(name).style(Style::default().fg(Color::Black).bg(Color::Gray))
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
