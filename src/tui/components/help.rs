use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::tui::theme;

/// Render the help popup overlay.
pub fn render_help(frame: &mut Frame) {
    let area = centered_rect(70, 80, frame.area());

    // Clear the area first
    frame.render_widget(Clear, area);

    let sections = vec![
        (
            "Global",
            vec![
                ("q / Ctrl+c", "Quit"),
                ("1/2/3", "Switch to view"),
                ("Tab / Shift+Tab", "Next / Previous view"),
                ("[ / ]", "Previous / Next view"),
                ("?", "Toggle this help"),
                ("r", "Refresh current view"),
            ],
        ),
        (
            "Environments View (1)",
            vec![
                ("j/k / Up/Down", "Navigate tree"),
                ("Space", "Collapse/expand node"),
                ("Enter", "Switch to selected branch"),
                ("c", "Create new branch"),
                ("d", "Delete selected branch"),
                ("S", "Start service on branch"),
                ("x", "Stop service on branch"),
                ("R", "Reset service on branch"),
                ("l", "View container logs"),
                ("/", "Filter environments"),
                ("Esc", "Clear filter"),
            ],
        ),
        (
            "System View (2)",
            vec![
                ("1/2/3", "Switch sub-section (Config/Hooks/Doctor)"),
                ("h/l / Left/Right", "Previous/next sub-section"),
                ("j/k / Up/Down", "Navigate/scroll content"),
                ("g/G", "Go to top/bottom"),
                ("PgUp/PgDn", "Page up/down"),
                ("D", "Run doctor checks (Doctor section)"),
            ],
        ),
        (
            "Logs View (3)",
            vec![
                ("f", "Toggle picker/content focus"),
                ("j/k / Up/Down", "Navigate picker or scroll logs"),
                ("Enter", "Load logs for selected service"),
                ("g/G", "Go to top/bottom (content)"),
                ("PgUp/PgDn", "Page up/down (content)"),
            ],
        ),
        (
            "Dialogs",
            vec![("y / Enter", "Confirm"), ("n / Esc", "Cancel")],
        ),
    ];

    let mut lines = Vec::new();
    lines.push(Line::styled(
        " devflow TUI - Keyboard Shortcuts",
        Style::default().fg(theme::TAB_TITLE).bold(),
    ));
    lines.push(Line::raw(""));

    for (section, bindings) in &sections {
        lines.push(Line::styled(
            format!(" {}", section),
            Style::default().fg(theme::KEY_HINT).bold(),
        ));
        for (key, desc) in bindings {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("   {:20}", key),
                    Style::default().fg(theme::HOOK_COMMAND),
                ),
                Span::styled(*desc, Style::default().fg(theme::TEXT_PRIMARY)),
            ]));
        }
        lines.push(Line::raw(""));
    }

    lines.push(Line::styled(
        " Press ? or Esc to close",
        Style::default().fg(theme::TEXT_MUTED),
    ));

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::DIALOG_HELP_BORDER))
                .title(" Help ")
                .title_style(Style::default().fg(theme::DIALOG_HELP_BORDER).bold()),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

/// Render a confirmation dialog overlay.
pub fn render_confirm(frame: &mut Frame, title: &str, message: &str) {
    let area = centered_rect(50, 30, frame.area());
    frame.render_widget(Clear, area);

    let lines = vec![
        Line::raw(""),
        Line::styled(message, Style::default().fg(theme::TEXT_PRIMARY)),
        Line::raw(""),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  [y]", Style::default().fg(theme::CHECK_PASS).bold()),
            Span::raw(" Yes    "),
            Span::styled("[n]", Style::default().fg(theme::CHECK_FAIL).bold()),
            Span::raw(" No"),
        ]),
    ];

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::DIALOG_CONFIRM_BORDER))
                .title(format!(" {} ", title))
                .title_style(Style::default().fg(theme::DIALOG_CONFIRM_BORDER).bold()),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

/// Render an input dialog overlay.
pub fn render_input(frame: &mut Frame, title: &str, input: &str) {
    let area = centered_rect(50, 20, frame.area());
    frame.render_widget(Clear, area);

    let lines = vec![
        Line::raw(""),
        Line::from(vec![
            Span::styled("  > ", Style::default().fg(theme::DIALOG_INPUT_BORDER)),
            Span::styled(input, Style::default().fg(theme::TEXT_PRIMARY)),
            Span::styled("_", Style::default().fg(theme::TEXT_MUTED)),
        ]),
        Line::raw(""),
        Line::styled(
            "  Enter to submit, Esc to cancel",
            Style::default().fg(theme::TEXT_MUTED),
        ),
    ];

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::DIALOG_INPUT_BORDER))
                .title(format!(" {} ", title))
                .title_style(Style::default().fg(theme::DIALOG_INPUT_BORDER).bold()),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

/// Render a status bar / notification at the bottom.
///
/// When a status message is active, shows the message prominently on the left
/// and tab-specific hints on the right.
pub fn render_status_bar(
    frame: &mut Frame,
    area: Rect,
    message: &str,
    is_error: bool,
    tab_hints: &str,
) {
    let (fg, bg) = if is_error {
        (theme::STATUS_ERROR_FG, theme::STATUS_ERROR_BG)
    } else {
        (theme::STATUS_SUCCESS_FG, theme::STATUS_BAR_BG)
    };

    let line = Line::from(vec![
        Span::styled(format!(" {} ", message), Style::default().fg(fg).bg(bg)),
        Span::styled(
            " | ",
            Style::default()
                .fg(theme::TEXT_MUTED)
                .bg(theme::STATUS_BAR_BG),
        ),
        Span::styled(
            format!("{} ", tab_hints),
            Style::default()
                .fg(theme::KEY_HINT)
                .bg(theme::STATUS_BAR_BG),
        ),
    ]);
    let paragraph = Paragraph::new(line).style(Style::default().bg(theme::STATUS_BAR_BG));
    frame.render_widget(paragraph, area);
}

/// Create a centered rectangle of the given percentage size.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
