use ratatui::style::{Color, Style};
use ratatui::widgets::Block;

/// Bordered block; focused panes get a cyan border.
pub fn block(title: &str, focused: bool) -> Block<'static> {
    let mut b = Block::bordered().title(title.to_string());
    if focused {
        b = b.border_style(Style::new().fg(Color::Cyan));
    }
    b
}
