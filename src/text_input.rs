use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use tui_input::Input;
use unicode_truncate::UnicodeTruncateStr;
use unicode_width::UnicodeWidthStr;

pub fn draw(frame: &mut Frame, area: Rect, prefix: Line<'_>, input: &Input) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let prefix_width = prefix.width().min(usize::from(area.width - 1)) as u16;
    frame.render_widget(
        Paragraph::new(prefix),
        Rect::new(area.x, area.y, prefix_width, 1),
    );
    let field = Rect::new(area.x + prefix_width, area.y, area.width - prefix_width, 1);
    let scroll = input.visual_scroll(usize::from(field.width - 1));
    let (text, _) = input
        .value()
        .unicode_truncate_start(input.value().width().saturating_sub(scroll));
    frame.render_widget(Paragraph::new(text), field);
    let cursor = input.visual_cursor().saturating_sub(scroll) as u16;
    frame.set_cursor_position((field.x + cursor, field.y));
}
