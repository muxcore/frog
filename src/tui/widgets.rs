use crate::db::connection::QueryRow;
use crate::db::session_manager::{
    ConnectionDialog, ExplorerRow, Session, SessionManager, SessionMode,
};
use crate::db::DbType;
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
        SessionMode::DbExplorer => "[EXPLORER]",
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

pub fn render_session_overview(sm: &mut SessionManager, f: &mut Frame, area: Rect) {
    let db_type = sm.active_db_type();
    let db_sessions = sm.fresh_session_list();
    let sess = sm.active_session();
    let cursor = sess
        .session_view_cursor
        .min(db_sessions.len().saturating_sub(1));
    let (title, headers): (&str, Vec<&str>) = match db_type {
        DbType::Oracle => (
            " v$session — ↑/↓ pick · Enter explain · Esc back ",
            vec![
                "SID",
                "Serial#",
                "User",
                "Status",
                "Machine",
                "Program",
                "SQL_ID",
                "Logon Time",
            ],
        ),
        DbType::Postgres => (
            " pg_stat_activity — ↑/↓ pick · Enter explain · Esc back ",
            vec![
                "PID",
                "—",
                "User",
                "State",
                "Client",
                "App",
                "Query",
                "Backend Start",
            ],
        ),
    };
    let header = Row::new(headers)
    .style(
        Style::default()
            .bg(Color::Rgb(40, 40, 60))
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );

    let panes = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(45), Constraint::Min(8)])
        .split(area);

    // Empty list: show why (privileges / not connected) instead of a bare
    // table — an unexplained empty F2 looks like "no sessions".
    if db_sessions.is_empty() {
        let (title, body, style) = match &sess.session_view_error {
            Some(err) => (
                " Sessions — error (press r to retry, Esc to go back) ",
                err.clone(),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            None => (
                " Sessions ",
                "No other sessions found.".to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        };
        let p = Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .style(style)
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Red)),
            );
        f.render_widget(p, panes[0]);
    } else {
    // Keep the highlighted row visible.
    let avail = panes[0].height.saturating_sub(4) as usize;
    let start = if avail == 0 {
        0
    } else if cursor >= avail {
        cursor + 1 - avail
    } else {
        0
    };
    let rows: Vec<Row> = db_sessions
        .iter()
        .skip(start)
        .take(avail)
        .enumerate()
        .map(|(rel, ds)| {
            let idx = start + rel;
            let serial = if db_type == DbType::Postgres {
                String::from("—")
            } else {
                ds.serial.to_string()
            };
            let row = Row::new(vec![
                ds.sid.to_string(),
                serial,
                ds.username.clone(),
                ds.status.clone(),
                ds.machine.clone(),
                ds.program.clone(),
                ds.sql_id.clone(),
                ds.logon_time.clone(),
            ]);
            if idx == cursor {
                row.style(
                    Style::default()
                        .bg(Color::Rgb(60, 60, 100))
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                row
            }
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
    .block(Block::default().title(title).borders(Borders::ALL));

    f.render_widget(table, panes[0]);
    } // end else (non-empty session list)

    // --- detail panes: current SQL + explain plan ---
    let detail = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(3)])
        .split(panes[1]);

    let sql_title = match &sess.plan_for {
        Some(who) => format!(" Current SQL — session {} (r: reload) ", who),
        None => " Current SQL ".to_string(),
    };
    let sql_text = sess
        .plan_sql_text
        .as_deref()
        .unwrap_or("↑/↓ selects a session · its SQL loads here · Enter runs EXPLAIN");
    let sql_para = Paragraph::new(sql_text)
        .wrap(Wrap { trim: false })
        .style(Style::default().fg(Color::White))
        .block(
            Block::default()
                .title(sql_title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
    f.render_widget(sql_para, detail[0]);

    render_plan_pane(sess, f, detail[1]);
}

/// Slice `s` from char offset `skip` (plan-pane horizontal scroll).
fn hslice(s: &str, skip: usize) -> String {
    if skip == 0 {
        return s.to_string();
    }
    s.chars().skip(skip).collect()
}

/// Bottom F2 pane: the stored explain plan (or a hint when none ran yet).
fn render_plan_pane(session: &Session, f: &mut Frame, area: Rect) {
    let block = || {
        Block::default()
            .title(" Explain plan — Enter: run · PgUp/PgDn: scroll · ←/→: sideways ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
    };
    let Some(qr) = session.plan_result.as_ref() else {
        let p = Paragraph::new("Press Enter to EXPLAIN the SQL above.")
            .style(Style::default().fg(Color::DarkGray))
            .block(block());
        f.render_widget(p, area);
        return;
    };
    if qr.is_error {
        let p = Paragraph::new(format!(
            "EXPLAIN failed:\n{}",
            qr.error_msg.as_deref().unwrap_or("Unknown error")
        ))
        .style(Style::default().fg(Color::Red))
        .block(block());
        f.render_widget(p, area);
        return;
    }
    if qr.columns.is_empty() {
        let p = Paragraph::new("Plan returned no columns.")
            .style(Style::default().fg(Color::Yellow))
            .block(block());
        f.render_widget(p, area);
        return;
    }
    let mut lines: Vec<Line> = Vec::with_capacity(qr.rows.len() + 1);
    lines.push(Line::from(Span::styled(
        hslice(&qr.columns.join(" | "), session.plan_hscroll),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    for row in &qr.rows {
        lines.push(Line::from(Span::raw(hslice(
            &row.join(" | "),
            session.plan_hscroll,
        ))));
    }
    let skip = session
        .plan_scroll
        .min(lines.len().saturating_sub(1));
    let visible: Vec<Line> = lines.into_iter().skip(skip).collect();
    let p = Paragraph::new(visible)
        .style(Style::default().fg(Color::White))
        .block(block());
    f.render_widget(p, area);
}

pub fn render_help(f: &mut Frame, area: Rect) {
    let help_text = vec![
        Line::from(Span::styled(
            " Frog DB Client — Welcome & Usability Overview ",
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
        Line::from("  Ctrl+O              Open connection dialog (Type/Host/Port/Service|DB/User/Password)"),
        Line::from("  F1                  This help screen"),
        Line::from("  F2                  Session browser — pick a session, Enter shows EXPLAIN plan"),
        Line::from("  F12                 DB explorer — schema/type folders, result-style preview, DDL"),
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
    let db_name_label = dlg.db_name_label();
    let db_name_value = match dlg.db_type {
        DbType::Oracle => dlg.service.clone(),
        DbType::Postgres => dlg.database.clone(),
    };
    let labels = [
        "Type:    ",
        "Host:    ",
        "Port:    ",
        db_name_label,
        "User:    ",
        "Password:",
    ];
    // Mask every password character individually so edits stay visible.
    let masked_pw = "*".repeat(dlg.password.chars().count());
    let raw_values: Vec<String> = vec![
        dlg.db_type.to_string(),
        dlg.host.clone(),
        dlg.port.clone(),
        db_name_value,
        dlg.user.clone(),
        masked_pw,
    ];

    let title = match dlg.db_type {
        DbType::Oracle => " Oracle Connection ",
        DbType::Postgres => " Postgres Connection ",
    };
    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            title,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    for (i, (label, value)) in labels.iter().zip(raw_values.iter()).enumerate() {
        let is_active = i == dlg.active_field;

        // Draw the insertion cursor inside the text for the active field.
        // The type row is a selector (Space/Left/Right or o/p toggles).
        let shown: Span = if is_active {
            if i == 0 {
                Span::styled(
                    format!("[{}]▏ (Space: toggle, o/p: pick)", value),
                    Style::default()
                        .bg(Color::Rgb(60, 60, 90))
                        .fg(Color::Yellow),
                )
            } else {
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
            }
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
    let backend = match session.conn_dialog.db_type {
        DbType::Oracle => format!(
            "Backend: oracle  {}:{}/{}",
            session.conn_dialog.host,
            session.conn_dialog.port,
            if session.conn_dialog.service.is_empty() {
                session.conn_dialog.database.clone()
            } else {
                session.conn_dialog.service.clone()
            },
        ),
        DbType::Postgres => format!(
            "Backend: postgres  {}:{}/{}",
            session.conn_dialog.host,
            session.conn_dialog.port,
            if session.conn_dialog.database.is_empty() {
                session.conn_dialog.service.clone()
            } else {
                session.conn_dialog.database.clone()
            },
        ),
    };
    let text = vec![
        Line::from(Span::styled(
            " Connections ",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            backend,
            Style::default().fg(Color::Cyan),
        )),
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

/// F12 DB explorer (DBeaver-style): object tree + detail pane with a live
/// Object tree + detail pane (live 20-row preview in the result-viewer
/// style for row-bearing objects, DDL/source for the rest).
///
/// Returns the (tree_area, detail_area) inner rects so the app can map mouse
/// clicks/scrolls. Layout adapts to small windows: side-by-side on wide
/// screens, stacked tree-over-detail on narrow ones, tree-only when tiny.
pub fn render_db_explorer(
    sm: &mut SessionManager,
    result_format: ResultFormat,
    f: &mut Frame,
    area: Rect,
) -> (Rect, Rect) {
    // Degenerate window: don't attempt any split, just say so.
    if area.height < 5 || area.width < 24 {
        let p = Paragraph::new("Window too small for the explorer — enlarge it or press Esc.")
            .style(Style::default().fg(Color::Yellow))
            .block(
                Block::default()
                    .title(" DB Explorer (F12) ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            );
        f.render_widget(p, area);
        return (Rect::default(), Rect::default());
    }

    // Wide screens: tree | detail. Narrow: tree over detail.
    let wide = area.width >= 80;
    let (tree_area, detail_area, stacked) = if wide {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(area);
        (cols[0], cols[1], false)
    } else if area.height >= 14 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(55), Constraint::Min(5)])
            .split(area);
        (rows[0], rows[1], true)
    } else {
        // Short window: the tree gets everything, preview hint in the title.
        (area, Rect::default(), false)
    };
    let show_detail = detail_area.height >= 4 && detail_area.width >= 20;

    let total_rows = sm.explorer_visible_rows().len();
    let scroll = {
        let sess = sm.active_session_mut();
        // Clamp scroll + keep the cursor visible (wheel may have moved the view).
        let cursor = sess.explorer_cursor.min(total_rows.saturating_sub(1));
        sess.explorer_cursor = cursor;
        let tree_view_h = tree_area.height.saturating_sub(2) as usize;
        if total_rows == 0 {
            sess.explorer_scroll = 0;
        } else if tree_view_h == 0 {
            sess.explorer_scroll = sess.explorer_scroll.min(total_rows - 1);
        } else {
            if cursor < sess.explorer_scroll {
                sess.explorer_scroll = cursor;
            } else if cursor >= sess.explorer_scroll + tree_view_h {
                sess.explorer_scroll = cursor + 1 - tree_view_h;
            }
            sess.explorer_scroll = sess
                .explorer_scroll
                .min(total_rows.saturating_sub(1));
        }
        sess.explorer_scroll
    };
    let tree_view_h = tree_area.height.saturating_sub(2) as usize;

    render_explorer_tree(sm, f, tree_area, scroll, tree_view_h, show_detail || stacked);
    if show_detail {
        render_explorer_detail(sm, result_format, f, detail_area);
    }
    (tree_area, detail_area)
}

/// Left pane of the explorer: the object tree itself.
fn render_explorer_tree(
    sm: &SessionManager,
    f: &mut Frame,
    area: Rect,
    scroll: usize,
    view_h: usize,
    detail_hidden: bool,
) {
    let sess = sm.active_session();
    let rows = sm.explorer_visible_rows();
    let cursor = sess.explorer_cursor.min(rows.len().saturating_sub(1));
    let inner_w = area.width.saturating_sub(2) as usize;

    let filter_note = if sess.explorer_filtering {
        format!("  [Filter: {}▏ Enter/Esc done]", sess.explorer_filter)
    } else if sess.explorer_filter.is_empty() {
        String::new()
    } else {
        format!("  [Filter: {}]", sess.explorer_filter)
    };
    let title = if detail_hidden {
        format!(
            " DB Explorer — ↑↓ move · →/Enter expand · s SELECT→editor · d reload · / filter · r refresh · Esc back{} ",
            filter_note
        )
    } else {
        format!(
            " Objects ({} shown{}){} ",
            rows.len(),
            if sess.explorer_filter.is_empty() {
                String::new()
            } else {
                " · filtered".into()
            },
            filter_note
        )
    };

    if !sess.explorer_loaded {
        let p = Paragraph::new("⟳ Loading schemas…")
            .style(Style::default().fg(Color::Yellow))
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            );
        f.render_widget(p, area);
        return;
    }
    if rows.is_empty() {
        let body = sess
            .explorer_error
            .clone()
            .unwrap_or_else(|| "No objects match the filter — clear it with Esc.".into());
        let p = Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::Red))
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            );
        f.render_widget(p, area);
        return;
    }

    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(scroll)
        .take(view_h.max(1))
        .map(|(idx, row)| explorer_tree_line(sess, row, idx == cursor, inner_w))
        .collect();
    let p = Paragraph::new(lines).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(p, area);

    // Thin scrollbar so wheel/keyboard position is visible in deep trees.
    if rows.len() > view_h.max(1) && area.height >= 5 {
        let mut state = ScrollbarState::new(rows.len().saturating_sub(1))
            .position(scroll.min(rows.len().saturating_sub(1)));
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(Color::DarkGray)),
            area,
            &mut state,
        );
    }
}

/// One tree line: indent + expand glyph + name + kind badge / column detail.
fn explorer_tree_line(sess: &Session, row: &ExplorerRow, selected: bool, inner_w: usize) -> Line<'static> {
    let base = if selected {
        Style::default()
            .bg(Color::Rgb(60, 60, 100))
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let truncate = |s: String| -> String {
        if inner_w == 0 {
            return String::new();
        }
        let mut out = String::new();
        let mut w = 0usize;
        for ch in s.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
            if w + cw > inner_w {
                break;
            }
            out.push(ch);
            w += cw;
        }
        out
    };
    let text: String = match row {
        ExplorerRow::Schema { idx } => {
            let s = &sess.explorer_schemas[*idx];
            let glyph = if s.expanded { "▾" } else { "▸" };
            match s.groups.as_ref() {
                Some(g) => format!("{} {} ({} types)", glyph, s.name, g.len()),
                None => format!("{} {}", glyph, s.name),
            }
        }
        ExplorerRow::Group { sidx, gidx } => {
            let g = &sess.explorer_schemas[*sidx].groups.as_ref().unwrap()[*gidx];
            let glyph = if g.expanded { "▾" } else { "▸" };
            let loaded = g.objects.as_ref().map(|o| o.len()).unwrap_or(0);
            if g.objects.is_some() {
                format!("  {} {} ({})", glyph, g.label(), loaded)
            } else {
                format!("  {} {} ({})", glyph, g.label(), g.count)
            }
        }
        ExplorerRow::Table { sidx, gidx, tidx } => {
            let t = &sess.explorer_schemas[*sidx].groups.as_ref().unwrap()[*gidx]
                .objects
                .as_ref()
                .unwrap()[*tidx];
            if t.is_expandable() {
                let glyph = if t.expanded { "▾" } else { "▸" };
                format!("    {} {} [{}]", glyph, t.name, t.kind)
            } else {
                format!("    ƒ {} [{}]", t.name, t.kind)
            }
        }
        ExplorerRow::Column {
            sidx,
            gidx,
            tidx,
            cidx,
        } => {
            let c = &sess.explorer_schemas[*sidx].groups.as_ref().unwrap()[*gidx]
                .objects
                .as_ref()
                .unwrap()[*tidx]
                .columns
                .as_ref()
                .unwrap()[*cidx];
            format!(
                "      • {} {}{}",
                c.name,
                c.data_type,
                if c.nullable { "" } else { " NOT NULL" }
            )
        }
    };
    // Kind badges get a tint when the row isn't selected.
    let mut line = truncate(text);
    if !selected {
        // Keep it a single span (cheap); selection already stands out.
        return Line::from(Span::styled(line, base));
    }
    line.push(' ');
    Line::from(Span::styled(line, base))
}

/// Right (or bottom) pane: info/DDL paragraph plus — for row-bearing
/// objects — the preview rendered with the real result viewer, so it looks
/// exactly like an executed query (same Table/Markdown/ASCII style).
fn render_explorer_detail(
    sm: &SessionManager,
    result_format: ResultFormat,
    f: &mut Frame,
    area: Rect,
) {
    let sess = sm.active_session();
    let rows = sm.explorer_visible_rows();
    let inner_w = area.width.saturating_sub(2) as usize;
    let fit = |s: &str| -> String {
        if inner_w == 0 {
            return String::new();
        }
        let mut out = String::new();
        let mut w = 0usize;
        for ch in s.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
            if w + cw > inner_w {
                break;
            }
            out.push(ch);
            w += cw;
        }
        out
    };
    // Hard-truncate each line to the pane width (wrap would fight the
    // vertical scroll offset).
    let fit_line = |l: &Line| -> Line<'static> {
        let s: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
        Line::from(Span::styled(
            fit(&s),
            l.spans.first().map(|sp| sp.style).unwrap_or_default(),
        ))
    };

    let detail: ExplorerDetail = match rows.get(sess.explorer_cursor) {
        None => ExplorerDetail::Text(
            " Detail ".into(),
            vec![Line::from(Span::styled(
                sess.explorer_error
                    .clone()
                    .unwrap_or_else(|| "Nothing to show.".into()),
                Style::default().fg(Color::DarkGray),
            ))],
        ),
        Some(ExplorerRow::Schema { idx }) => {
            ExplorerDetail::Text(format!(" Detail — {} ", sess.explorer_schemas[*idx].name), {
                let s = &sess.explorer_schemas[*idx];
                let mut lines = vec![Line::from(Span::styled(
                    format!("Schema {}", s.name),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ))];
                match s.groups.as_ref() {
                    Some(groups) => {
                        for g in groups {
                            lines.push(Line::from(format!("  {} ({})", g.label(), g.count)));
                        }
                    }
                    None => lines.push(Line::from(Span::styled(
                        "Press → or Enter to list its type folders.",
                        Style::default().fg(Color::DarkGray),
                    ))),
                }
                if let Some(err) = s.load_error.as_ref() {
                    lines.push(Line::from(Span::styled(
                        err.clone(),
                        Style::default().fg(Color::Red),
                    )));
                }
                lines
            })
        }
        Some(ExplorerRow::Group { sidx, gidx }) => {
            ExplorerDetail::Text(
                format!(
                    " Detail — {}.{} ",
                    sess.explorer_schemas[*sidx].name,
                    sess.explorer_schemas[*sidx]
                        .groups
                        .as_ref()
                        .and_then(|g| g.get(*gidx))
                        .map(|g| g.label())
                        .unwrap_or("?")
                ),
                {
                    let s = &sess.explorer_schemas[*sidx];
                    let g = s.groups.as_ref().and_then(|g| g.get(*gidx));
                    let mut lines = vec![Line::from(Span::styled(
                        format!(
                            "{} in {}",
                            g.map(|g| g.label()).unwrap_or("?"),
                            s.name
                        ),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ))];
                    match g.and_then(|g| g.objects.as_ref()) {
                        Some(objs) if !objs.is_empty() => {
                            lines.push(Line::from(format!("{} object(s), first few:", objs.len())));
                            for o in objs.iter().take(30) {
                                lines.push(Line::from(format!("  {} [{}]", o.name, o.kind)));
                            }
                            if objs.len() > 30 {
                                lines.push(Line::from(Span::styled(
                                    format!("… and {} more (filter with /)", objs.len() - 30),
                                    Style::default().fg(Color::DarkGray),
                                )));
                            }
                        }
                        _ => {}
                    }
                    if let Some(err) = g.and_then(|g| g.load_error.as_ref()) {
                        lines.push(Line::from(Span::styled(
                            err.clone(),
                            Style::default().fg(Color::Red),
                        )));
                    } else if g.map(|g| g.objects.is_none()).unwrap_or(true) {
                        lines.push(Line::from(Span::styled(
                            "Press → or Enter to list its objects.",
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                    lines
                },
            )
        }
        Some(ExplorerRow::Table { sidx, gidx, tidx })
        | Some(ExplorerRow::Column {
            sidx, gidx, tidx, ..
        }) => explorer_object_detail(sess, *sidx, *gidx, *tidx),
    };

    match detail {
        ExplorerDetail::Text(title, content) => {
            let view_h = area.height.saturating_sub(2) as usize;
            let total = content.len();
            let skip = sess
                .explorer_detail_scroll
                .min(total.saturating_sub(1));
            let visible: Vec<Line> = content
                .into_iter()
                .skip(skip)
                .take(view_h.max(1))
                .map(|l| fit_line(&l))
                .collect();
            let p = Paragraph::new(visible).block(
                Block::default()
                    .title(format!(
                        " {} · PgUp/PgDn scroll ({} lines) · s SELECT→editor · d reload ",
                        title.trim(),
                        total
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            );
            f.render_widget(p, area);
        }
        ExplorerDetail::Split(title, top, preview) => {
            // Info on top (fixed, truncated), live result viewer below with
            // the same style as executed SQL. Needs room for both.
            let total_h = area.height as usize;
            let need_top = top.len() + 2;
            if total_h >= need_top + 8 {
                let top_h = (need_top.min(total_h - 8)).max(3) as u16;
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(top_h), Constraint::Min(8)])
                    .split(area);
                let shown = (top_h as usize).saturating_sub(2);
                let visible: Vec<Line> =
                    top.into_iter().take(shown).map(|l| fit_line(&l)).collect();
                let p = Paragraph::new(visible).block(
                    Block::default()
                        .title(format!(" {} ", title.trim()))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::DarkGray)),
                );
                f.render_widget(p, chunks[0]);
                render_table(
                    sess,
                    &Some(preview),
                    TableViewState {
                        scroll_offset: sess.explorer_detail_scroll,
                        col_scroll_offset: 0,
                        result_format,
                        focused: false,
                    },
                    f,
                    chunks[1],
                );
            } else {
                // Too short to split: info + hint, scrollable.
                let mut content = top;
                content.push(Line::from(Span::styled(
                    "…preview below — enlarge the window to see the result table…",
                    Style::default().fg(Color::DarkGray),
                )));
                let view_h = area.height.saturating_sub(2) as usize;
                let total = content.len();
                let skip = sess.explorer_detail_scroll.min(total.saturating_sub(1));
                let visible: Vec<Line> = content
                    .into_iter()
                    .skip(skip)
                    .take(view_h.max(1))
                    .map(|l| fit_line(&l))
                    .collect();
                let p = Paragraph::new(visible).block(
                    Block::default()
                        .title(format!(" {} ", title.trim()))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::DarkGray)),
                );
                f.render_widget(p, area);
            }
        }
    }
}

/// Explorer detail content: either a plain scrollable paragraph, or info
/// lines on top with the live result viewer below (same style as SQL output).
enum ExplorerDetail<'a> {
    Text(String, Vec<Line<'static>>),
    Split(String, Vec<Line<'static>>, &'a QueryRow),
}

/// Detail for one object: info/DDL lines plus, for row-bearing objects with
/// loaded preview data, the preview itself (rendered as a result table).
fn explorer_object_detail(
    sess: &Session,
    sidx: usize,
    gidx: usize,
    tidx: usize,
) -> ExplorerDetail<'_> {
    use ExplorerDetail as D;
    let (schema_name, t) = match sess
        .explorer_schemas
        .get(sidx)
        .and_then(|s| {
            s.groups
                .as_ref()
                .and_then(|g| g.get(gidx))
                .and_then(|g| g.objects.as_ref())
                .and_then(|o| o.get(tidx))
                .map(|t| (s.name.clone(), t))
        }) {
        Some(v) => v,
        None => return D::Text(" Detail ".into(), vec![]),
    };
    let title = format!(" Detail — {}.{} [{}] ", schema_name, t.name, t.kind);
    let header = Line::from(Span::styled(
        format!("{}.{} — {}", schema_name, t.name, t.kind),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));

    // Column section for expandable objects.
    let mut top = vec![header];
    if t.is_expandable() {
        match t.columns.as_ref() {
            Some(cols) if !cols.is_empty() => {
                top.push(Line::from(Span::styled(
                    format!("Columns ({}):", cols.len()),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )));
                for c in cols {
                    top.push(Line::from(format!(
                        "  {} {}{}",
                        c.name,
                        c.data_type,
                        if c.nullable { "" } else { " NOT NULL" }
                    )));
                }
            }
            _ => {
                if let Some(err) = t.load_error.as_ref() {
                    top.push(Line::from(Span::styled(
                        err.clone(),
                        Style::default().fg(Color::Red),
                    )));
                } else if t.columns.is_none() {
                    top.push(Line::from(Span::styled(
                        "Columns not loaded — expand with →.",
                        Style::default().fg(Color::DarkGray),
                    )));
                } else {
                    top.push(Line::from(Span::styled(
                        "(no columns)",
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
        }
    }

    // DDL/source section for definition-bearing objects.
    if t.has_ddl() {
        top.push(Line::from(""));
        top.push(Line::from(Span::styled(
            "Definition:",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )));
        if t.ddl_loading {
            top.push(Line::from(Span::styled(
                "⟳ Loading definition…",
                Style::default().fg(Color::Yellow),
            )));
        } else if let Some(err) = t.ddl_error.as_ref() {
            top.push(Line::from(Span::styled(
                err.clone(),
                Style::default().fg(Color::Red),
            )));
        } else if let Some(ddl) = t.ddl.as_ref() {
            for l in ddl.lines() {
                top.push(Line::from(Span::raw(l.to_string())));
            }
        } else {
            top.push(Line::from(Span::styled(
                "Press d to load the definition.",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    // Live 20-row preview in result-viewer style (same as executed SQL).
    if let Some(qr) = t.preview.as_ref() {
        return D::Split(title, top, qr);
    }
    if t.has_preview() {
        top.push(Line::from(""));
        top.push(Line::from(if t.preview_loading {
            Span::styled(
                "⟳ Loading preview…",
                Style::default().fg(Color::Yellow),
            )
        } else {
            Span::styled(
                "Preview pending…",
                Style::default().fg(Color::DarkGray),
            )
        }));
    }
    D::Text(title, top)
}
