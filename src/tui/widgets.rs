use crate::db::connection::QueryRow;
use crate::db::session_manager::{ConnectionDialog, Session, SessionManager, SessionMode};
use crate::tui::app::{ActiveWindow, ResultFormat};
use ratatui::{prelude::*, widgets::*};
use unicode_width::UnicodeWidthStr;

pub fn render_tabs(sessions: &[Session], active_idx: usize, f: &mut Frame, area: Rect) {
    let titles: Vec<Line> = sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let style = if i == active_idx {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let (icon, color) = if s.pending_query {
                ("⟳ RUNNING", Color::Yellow)
            } else if s.connecting {
                ("⟳ CONNECTING", Color::Yellow)
            } else if s.is_connected {
                ("●", Color::Green)
            } else {
                ("○", Color::Red)
            };
            Line::from(vec![
                Span::styled(format!(" {} ", icon), Style::default().fg(color)),
                Span::styled(format!("{}: {}", s.id, s.name), style),
                Span::raw("  "),
            ])
        })
        .collect();

    let tabs = Tabs::new(titles)
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .highlight_style(Style::default().fg(Color::Cyan))
        .select(active_idx);

    f.render_widget(tabs, area);
}

/// Render the tmux-style command bar (Ctrl+:).
pub fn render_command_bar(
    input: &str,
    error: Option<&str>,
    confirm: Option<&str>,
    f: &mut Frame,
    area: Rect,
) {
    let prompt = Span::styled(
        ": ",
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );

    let mut spans = vec![prompt, Span::raw(input.to_string())];
    if let Some(cmd) = confirm {
        spans.push(Span::styled(
            format!("  Run '{}'? [y/N] ", cmd),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(err) = error {
        spans.push(Span::styled(
            format!("  ⚠ {}", err),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }

    let bar =
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Cyan).fg(Color::Black));
    f.render_widget(bar, area);
}

pub fn render_status_bar(
    session: &Session,
    active_window: ActiveWindow,
    mouse_capture: bool,
    f: &mut Frame,
    area: Rect,
) {
    let mode_str = match session.mode {
        SessionMode::Query => "[QUERY]",
        SessionMode::Results => "[RESULTS]",
        SessionMode::SessionView => "[SESSIONS]",
        SessionMode::Help => "[HELP]",
        SessionMode::History => "[HISTORY]",
        SessionMode::ConnectionPickerDialog => "[CONNECTING...]",
    };

    let focus_str = match active_window {
        ActiveWindow::Editor => "Focus: EDITOR",
        ActiveWindow::Results => "Focus: RESULTS",
        ActiveWindow::Sidebar => "Focus: SIDEBAR",
    };

    let conn_status = if session.connecting {
        " Connecting... "
    } else if session.is_connected {
        " Connected "
    } else {
        " Disconnected (Ctrl+O) "
    };

    let conn_style = if session.connecting {
        Style::default()
            .bg(Color::Yellow)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
    } else if session.is_connected {
        Style::default()
            .bg(Color::Green)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .bg(Color::Red)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    };

    let error_part = if let Some(ref err) = session.connect_error {
        format!(" ⚠ {} ", err)
    } else {
        String::new()
    };

    let status_msg_part = if let Some(ref msg) = session.status_message {
        format!(" ⓘ {} ", msg)
    } else {
        String::new()
    };

    let exec_part = if session.pending_query {
        Span::styled(
            " ⟳ EXECUTING QUERY... ",
            Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("")
    };

    let cursor_info = format!(
        " | L{}:C{} ({}) | {}",
        session.editor.cursor_row + 1,
        session.editor.cursor_col + 1,
        session.editor.lines.len(),
        focus_str
    );

    let mouse_part = if mouse_capture {
        Span::raw("")
    } else {
        Span::styled(
            " | Mouse:OFF (terminal select/copy)",
            Style::default().fg(Color::Magenta),
        )
    };

    let status_text = Line::from(vec![
        Span::styled(
            mode_str,
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(conn_status, conn_style),
        exec_part,
        Span::styled(error_part, Style::default().fg(Color::Red)),
        Span::styled(status_msg_part, Style::default().fg(Color::Cyan)),
        Span::raw(cursor_info),
        mouse_part,
        Span::raw(" | Tab: Focus | Alt+Up/Down: Resize Editor | F3: History | Ctrl+Y: Copy"),
    ]);

    let bar = Paragraph::new(status_text)
        .style(Style::default().bg(Color::Rgb(30, 30, 30)).fg(Color::White));
    f.render_widget(bar, area);
}

pub fn render_editor(session: &Session, focused: bool, f: &mut Frame, area: Rect) {
    let border_color = if session.pending_query {
        Color::Yellow
    } else if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };

    let title = if session.pending_query {
        " SQL Editor (⟳ EXECUTING QUERY...) "
    } else {
        " SQL Editor (Ctrl+Enter: run stmt | F5: run script | Ctrl+:: run @file | Ctrl+/-: resize) "
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner_area = block.inner(area);
    let vis_height = inner_area.height as usize;
    let scroll = session.editor.scroll_offset;

    let stmt_rows = session.editor.current_statement_rows();

    let lines: Vec<Line> = session
        .editor
        .lines
        .iter()
        .enumerate()
        .skip(scroll)
        .take(vis_height)
        .map(|(idx, line_str)| {
            let is_current_line = idx == session.editor.cursor_row;
            let is_in_stmt = if let Some((s, e)) = stmt_rows {
                idx >= s && idx <= e
            } else {
                false
            };

            let line_num_str = format!("{:3} ", idx + 1);
            let num_style = if is_current_line {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            let bg = if is_current_line {
                Color::Rgb(40, 40, 60)
            } else if is_in_stmt {
                Color::Rgb(25, 28, 38)
            } else {
                Color::Reset
            };

            let text_style = Style::default().bg(bg).fg(Color::White);

            Line::from(vec![
                Span::styled(line_num_str, num_style),
                Span::styled(line_str.to_string(), text_style),
            ])
        })
        .collect();

    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

pub fn render_history(session: &Session, f: &mut Frame, area: Rect) {
    let block = Block::default()
        .title(" SQL History Browser (Up/Down: navigate | Enter: insert into editor | Esc: exit) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));

    if session.query_history.is_empty() {
        let p = Paragraph::new("No history entries recorded yet.")
            .style(Style::default().fg(Color::DarkGray))
            .block(block);
        f.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = session
        .query_history
        .iter()
        .enumerate()
        .map(|(i, stmt)| {
            let style = if i == session.history_cursor {
                Style::default()
                    .bg(Color::Rgb(60, 60, 100))
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let prefix = if i == session.history_cursor {
                "▶ "
            } else {
                "  "
            };
            let single_line_stmt = stmt.replace('\n', " ");
            ListItem::new(Line::from(vec![
                Span::styled(prefix, Style::default().fg(Color::Cyan)),
                Span::styled(
                    format!("[#{:02}] ", i + 1),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(single_line_stmt, style),
            ]))
        })
        .collect();

    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

/// Render-time state for the result viewer (scroll + display format + focus).
#[derive(Debug, Clone, Copy)]
pub struct TableViewState {
    pub scroll_offset: usize,
    pub col_scroll_offset: usize,
    pub result_format: ResultFormat,
    pub focused: bool,
}

pub fn render_table(
    session: &Session,
    query_row: &Option<&QueryRow>,
    view: TableViewState,
    f: &mut Frame,
    area: Rect,
) {
    let TableViewState {
        scroll_offset,
        col_scroll_offset,
        result_format,
        focused,
    } = view;
    let border_color = if session.pending_query {
        Color::Yellow
    } else if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };

    let title = if session.pending_query {
        " Result Viewer (⟳ EXECUTING QUERY...) "
    } else {
        " Result Viewer (First 100 rows default) "
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    if session.pending_query {
        let p = Paragraph::new(" ⟳ Executing SQL query... Waiting for database response.")
            .style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            .block(block);
        f.render_widget(p, area);
        return;
    }

    if let Some(qr) = query_row {
        if qr.is_error {
            let err_msg = qr.error_msg.as_deref().unwrap_or("Unknown error");
            let p = Paragraph::new(format!("ERROR:\n{}", err_msg))
                .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
                .block(block);
            f.render_widget(p, area);
            return;
        }

        match result_format {
            ResultFormat::Markdown => {
                let total_cols = qr.columns.len();
                let start_col = col_scroll_offset.min(total_cols.saturating_sub(1));
                let end_col = (start_col + 10).min(total_cols);
                let visible_cols = &qr.columns[start_col..end_col];

                // Apply vertical scroll (rows) just like Table mode.
                let avail_height = area.height.saturating_sub(3) as usize;
                let rows: Vec<&Vec<String>> = qr
                    .rows
                    .iter()
                    .skip(scroll_offset)
                    .take(avail_height)
                    .collect();

                let mut md_text = String::new();
                md_text.push_str("| ");
                md_text.push_str(&visible_cols.join(" | "));
                md_text.push_str(" |\n|");
                for _ in visible_cols {
                    md_text.push_str("---|");
                }
                md_text.push('\n');
                let visible_rows = rows.len();
                for row in &rows {
                    let visible_row: Vec<&str> =
                        row[start_col..end_col].iter().map(|s| s.as_str()).collect();
                    md_text.push_str("| ");
                    md_text.push_str(&visible_row.join(" | "));
                    md_text.push_str(" |\n");
                }
                let p = Paragraph::new(md_text)
                    .style(Style::default().fg(Color::White))
                    .block(
                        Block::default()
                            .title(format!(
                                " Result Viewer (Markdown Format | rows {}-{} of {}) ",
                                scroll_offset + 1,
                                (scroll_offset + visible_rows).min(qr.rows.len()),
                                qr.rows.len()
                            ))
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Magenta)),
                    );
                f.render_widget(p, area);
                return;
            }
            ResultFormat::Ascii => {
                let total_cols = qr.columns.len();
                let start_col = col_scroll_offset.min(total_cols.saturating_sub(1));
                let avail_height = area.height.saturating_sub(3) as usize;
                let ascii_text = format_ascii_table(qr, start_col, scroll_offset, avail_height);
                let p = Paragraph::new(ascii_text)
                    .style(Style::default().fg(Color::White))
                    .block(
                        Block::default()
                            .title(format!(
                                " Result Viewer (ASCII Table | rows {}-{} of {}) ",
                                scroll_offset + 1,
                                (scroll_offset + avail_height).min(qr.rows.len()),
                                qr.rows.len()
                            ))
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Cyan)),
                    );
                f.render_widget(p, area);
                return;
            }
            ResultFormat::Table => {
                // Fall through to normal table rendering
            }
        }

        if qr.columns.is_empty() || qr.rows.is_empty() {
            let msg = if let Some(rows_affected) = qr.rows_affected {
                format!(
                    "Statement executed. {} rows affected. Elapsed: {}ms",
                    rows_affected, qr.elapsed_ms
                )
            } else {
                "No data returned. Elapsed: ".to_string() + &qr.elapsed_ms.to_string() + "ms"
            };
            let p = Paragraph::new(msg)
                .style(Style::default().fg(Color::Yellow))
                .block(block);
            f.render_widget(p, area);
            return;
        }

        let total_cols = qr.columns.len();
        let start_col = col_scroll_offset.min(total_cols.saturating_sub(1));
        let visible_cols: Vec<String> = qr
            .columns
            .iter()
            .skip(start_col)
            .take(10)
            .cloned()
            .collect();

        let header = Row::new(visible_cols.clone())
            .style(
                Style::default()
                    .bg(Color::Rgb(40, 40, 80))
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            .height(1);

        let total_rows = qr.rows.len();
        let avail_height = area.height.saturating_sub(4) as usize;

        let mut visible_rows: Vec<Row> = qr
            .rows
            .iter()
            .skip(scroll_offset)
            .take(avail_height)
            .enumerate()
            .map(|(idx, row)| {
                let bg = if idx % 2 == 0 {
                    Color::Rgb(18, 18, 22)
                } else {
                    Color::Rgb(24, 24, 30)
                };
                let sliced_row: Vec<String> =
                    row.iter().skip(start_col).take(10).cloned().collect();
                Row::new(sliced_row).style(Style::default().bg(bg).fg(Color::White))
            })
            .collect();

        // If result is truncated and we have space, show a "load more" footer row
        if qr.truncated && visible_rows.len() < avail_height {
            let hint = format!(
                " ↓ {} rows loaded — press Ctrl+F to fetch next {} rows ",
                total_rows, qr.page_size
            );
            let mut footer_cells = vec![hint];
            for _ in 1..visible_cols.len() {
                footer_cells.push(String::new());
            }
            visible_rows.push(
                Row::new(footer_cells).style(
                    Style::default()
                        .bg(Color::Rgb(30, 30, 60))
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD | Modifier::ITALIC),
                ),
            );
        }

        let more_indicator = if qr.truncated {
            " [MORE ↓ Ctrl+F]"
        } else if qr.page_offset > 0 {
            " [END — no more rows]"
        } else {
            ""
        };
        let title = format!(
            " Result Viewer (rows {}-{} of {}{} | cols {}-{} of {} | {} rows, {} | {}ms) ",
            scroll_offset + 1,
            (scroll_offset + visible_rows.len()).min(total_rows),
            total_rows,
            more_indicator,
            start_col + 1,
            start_col + visible_cols.len(),
            total_cols,
            qr.total_fetched,
            format_bytes(qr.byte_count),
            qr.elapsed_ms
        );

        let widths: Vec<Constraint> = visible_cols
            .iter()
            .map(|c| Constraint::Length((c.len() as u16).clamp(12, 35)))
            .collect();

        let table = Table::new(visible_rows, widths)
            .header(header)
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .row_highlight_style(Style::default().bg(Color::Rgb(60, 60, 80)));

        f.render_widget(table, area);
    } else {
        let p = Paragraph::new("No query executed yet. Type SQL and press F5.")
            .style(Style::default().fg(Color::DarkGray))
            .block(block);
        f.render_widget(p, area);
    }
}

/// Format a result page as an ASCII table using display width (so wide /
/// multibyte characters align correctly).
pub fn format_ascii_table(
    qr: &QueryRow,
    start_col: usize,
    scroll_offset: usize,
    avail_height: usize,
) -> String {
    if qr.columns.is_empty() || qr.rows.is_empty() {
        return "No data to display".to_string();
    }

    let num_cols = qr.columns.len();
    let start = start_col.min(num_cols.saturating_sub(1));
    let end = (start + 10).min(num_cols);
    let visible_range = start..end;

    let mut col_widths = Vec::new();
    for i in visible_range.clone() {
        let mut w = UnicodeWidthStr::width(qr.columns[i].as_str());
        for row in qr.rows.iter().skip(scroll_offset).take(avail_height) {
            if i < row.len() {
                w = w.max(UnicodeWidthStr::width(row[i].as_str()));
            }
        }
        col_widths.push(w);
    }

    // Pad `s` on the right to the display width `w`.
    fn padded(s: &str, w: usize) -> String {
        format!("{}{}", s, " ".repeat(w - UnicodeWidthStr::width(s) + 1))
    }

    let columns: Vec<&str> = qr.columns[visible_range.clone()]
        .iter()
        .map(|s| s.as_str())
        .collect();
    let rows: Vec<Vec<&str>> = qr
        .rows
        .iter()
        .skip(scroll_offset)
        .take(avail_height)
        .map(|r| {
            r[visible_range.clone()]
                .iter()
                .map(|s| s.as_str())
                .collect()
        })
        .collect();

    let mut output = String::new();

    output.push('+');
    for w in &col_widths {
        output.push_str(&"-".repeat(w + 2));
        output.push('+');
    }
    output.push('\n');

    output.push('|');
    for (i, col) in columns.iter().enumerate() {
        output.push(' ');
        output.push_str(&padded(col, col_widths[i]));
        output.push('|');
    }
    output.push('\n');

    output.push('+');
    for w in &col_widths {
        output.push_str(&"=".repeat(w + 2));
        output.push('+');
    }
    output.push('\n');

    for row in &rows {
        output.push('|');
        for (i, cell) in row.iter().enumerate() {
            output.push(' ');
            output.push_str(&padded(cell, col_widths[i]));
            output.push('|');
        }
        output.push('\n');
    }

    output.push('+');
    for w in &col_widths {
        output.push_str(&"-".repeat(w + 2));
        output.push('+');
    }

    output
}

fn format_bytes(bytes: usize) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

pub fn render_session_overview(sm: &SessionManager, f: &mut Frame, area: Rect) {
    let db_sessions = sm.get_session_info();
    let header = Row::new(vec![
        "SID",
        "Serial#",
        "User",
        "Status",
        "Machine",
        "Program",
        "SQL_ID",
        "Logon Time",
    ])
    .style(
        Style::default()
            .bg(Color::Rgb(40, 40, 60))
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );

    let avail = area.height.saturating_sub(4) as usize;
    let rows: Vec<Row> = db_sessions
        .iter()
        .take(avail)
        .map(|ds| {
            Row::new(vec![
                ds.sid.to_string(),
                ds.serial.to_string(),
                ds.username.clone(),
                ds.status.clone(),
                ds.machine.clone(),
                ds.program.clone(),
                ds.sql_id.clone(),
                ds.logon_time.clone(),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Length(18),
            Constraint::Length(14),
            Constraint::Length(20),
        ],
    )
    .header(header)
    .block(Block::default().title(" v$session ").borders(Borders::ALL));

    f.render_widget(table, area);
}

pub fn render_help(f: &mut Frame, area: Rect) {
    let help_text = vec![
        Line::from(Span::styled(
            " Frog Oracle Client — Welcome & Usability Overview ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            " ─ Window & Panels ─ ",
            Style::default().fg(Color::Cyan),
        )),
        Line::from("  Ctrl+T              Open new session tab"),
        Line::from("  Ctrl+W              Close current tab"),
        Line::from("  Ctrl+Right / Alt+Right  Next session tab"),
        Line::from("  Ctrl+Left / Alt+Left   Previous session tab"),
        Line::from("  Alt+1..9            Switch to session tab 1-9"),
        Line::from("  Tab                 Switch active window (Editor / Results / Sidebar)"),
        Line::from("  Ctrl+Z / F11        Zoom / Maximize active panel"),
        Line::from("  Ctrl+B              Toggle connection/info sidebar"),
        Line::from("  Alt+Up / Alt+Down   Grow / shrink SQL editor height"),
        Line::from("  Mouse Hover         Auto-focus panels (Hyprland style)"),
        Line::from("  Mouse Drag Border   Resize panels horizontally"),
        Line::from("  Mouse Scroll        Scroll result table vertically"),
        Line::from("  Middle-click        Paste selection at cursor (tmux style)"),
        Line::from("  Ctrl+M              Toggle mouse capture — while OFF the terminal"),
        Line::from("                      handles select/copy natively (or hold Shift+drag)"),
        Line::from(""),
        Line::from(Span::styled(
            " ─ SQL Execution ─ ",
            Style::default().fg(Color::Cyan),
        )),
        Line::from("  Ctrl+Enter / Alt+Enter / F9  Execute statement under cursor"),
        Line::from("  F5 / Ctrl+R         Execute all statements as script"),
        Line::from("  Ctrl+:              Command bar (e.g. @file.sql, @@inc, clear)"),
        Line::from("  Ctrl+C / Esc          Cancel running query"),
        Line::from(""),
        Line::from(Span::styled(
            " ─ Results ─ ",
            Style::default().fg(Color::Cyan),
        )),
        Line::from("  Ctrl+F              Fetch next page of results (100 rows)"),
        Line::from("  Ctrl+Y              Copy results to clipboard"),
        Line::from("  Ctrl+D              Cycle result format: Table → Markdown → ASCII"),
        Line::from("  Arrow keys / PgUp/PgDn  Scroll results"),
        Line::from(""),
        Line::from(Span::styled(
            " ─ SQL Editor ─ ",
            Style::default().fg(Color::Cyan),
        )),
        Line::from("  Arrow keys          Move cursor"),
        Line::from("  Home / End          Start / end of line"),
        Line::from("  Ctrl+A / Ctrl+E     Start / end of line"),
        Line::from("  Ctrl+K              Kill (delete) to end of line"),
        Line::from("  Ctrl+U              Delete to start of line"),
        Line::from("  Delete / Backspace  Delete character"),
        Line::from("  F3                  Open SQL history"),
        Line::from(""),
        Line::from(Span::styled(
            " ─ Connection & Views ─ ",
            Style::default().fg(Color::Cyan),
        )),
        Line::from("  Ctrl+O              Open connection dialog"),
        Line::from("  F1                  This help screen"),
        Line::from("  F2                  Oracle v$session overview"),
        Line::from("  Esc / q             Return to query editor"),
        Line::from("  Ctrl+Q              Quit application"),
    ];
    let p = Paragraph::new(help_text).block(
        Block::default()
            .title(" Welcome / Help (Press any key to start) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );
    f.render_widget(p, area);
}

pub fn render_connection_dialog(session: &Session, f: &mut Frame, area: Rect) {
    let dlg = &session.conn_dialog;
    let labels = [
        "Host:    ",
        "Port:    ",
        "Service: ",
        "User:    ",
        "Password:",
    ];
    let raw_values = [
        dlg.host.clone(),
        dlg.port.clone(),
        dlg.service.clone(),
        dlg.user.clone(),
        // Mask every character individually so edits stay visible.
        "*".repeat(dlg.password.chars().count()),
    ];

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            " Oracle Connection ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    for (i, (label, value)) in labels.iter().zip(raw_values.iter()).enumerate() {
        let is_active = i == dlg.active_field;

        // Draw the insertion cursor inside the text for the active field.
        let shown: Span = if is_active {
            let pos = dlg.cursor.min(value.len());
            let mut p = pos;
            while p > 0 && !value.is_char_boundary(p) {
                p -= 1;
            }
            Span::styled(
                format!("{}▏{}", &value[..p], &value[p..]),
                Style::default()
                    .bg(Color::Rgb(60, 60, 90))
                    .fg(Color::Yellow),
            )
        } else {
            Span::raw(value.clone())
        };

        lines.push(Line::from(vec![
            Span::styled(*label, Style::default().fg(Color::Cyan)),
            shown,
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(
            " Tab/Up/Down: field ({}/{}) | Left/Right: cursor | Ctrl+U: clear | Enter: connect | Esc: cancel ",
            dlg.active_field + 1,
            ConnectionDialog::FIELD_COUNT
        ),
        Style::default().fg(Color::DarkGray),
    )));

    if session.connecting {
        lines.push(Line::from(Span::styled(
            " Connecting... ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
    }
    if let Some(ref err) = session.connect_error {
        lines.push(Line::from(Span::styled(
            format!(" Error: {} ", err),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }

    let p = Paragraph::new(lines).block(
        Block::default()
            .title(" Connect ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Green)),
    );
    f.render_widget(p, area);
}

pub fn render_right_panel(session: &Session, f: &mut Frame, area: Rect) {
    let text = vec![
        Line::from(Span::styled(
            " Connections ",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(if session.is_connected {
            Span::styled(" ✔ Connected ", Style::default().fg(Color::Green))
        } else if session.connecting {
            Span::styled(" ⟳ Connecting... ", Style::default().fg(Color::Yellow))
        } else {
            Span::styled(" ✖ Disconnected ", Style::default().fg(Color::Red))
        }),
        Line::from(""),
        Line::from(Span::styled(
            " (Press Ctrl+B to hide sidebar)",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let p =
        Paragraph::new(text).block(Block::default().title(" Info Panel ").borders(Borders::ALL));
    f.render_widget(p, area);
}
