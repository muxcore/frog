use crate::db::completion::CompletionPopup;
use crate::db::connection::QueryRow;
use crate::db::session_manager::{
    ConnectionDialog, ExplorerRow, Session, SessionManager, SessionMode, TableRect, TableSelection,
};
use crate::db::DbType;
use crate::tui::app::{ActiveWindow, ExplorerPane, ResultFormat, SessionViewPane};
use crate::tui::perf_view::PerfPane;
use ratatui::{prelude::*, widgets::*};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

/// Emacs I-search prompt (replaces the status bar while searching).
/// Shows direction, query, match position and the key legend.
#[allow(clippy::too_many_arguments)]
pub fn render_search_bar(
    query: &str,
    forward: bool,
    current: usize,
    total: usize,
    failing: bool,
    wrapped: bool,
    target_label: &str,
    snippet: Option<&str>,
    f: &mut Frame,
    area: Rect,
) {
    let dir = if forward { "forward" } else { "backward" };
    let pos = if query.is_empty() {
        "type to search".to_string()
    } else if failing {
        "no match".to_string()
    } else {
        format!("{}/{}", current + 1, total)
    };
    let flags = if failing {
        " [FAILING]"
    } else if wrapped {
        " [WRAPPED]"
    } else {
        ""
    };
    let prompt = Span::styled(
        format!(" I-search {} [{}]: ", dir, target_label),
        Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );
    let mut spans = vec![
        prompt,
        Span::styled(
            query.to_string(),
            Style::default().fg(Color::Black).bg(Color::Yellow),
        ),
        Span::styled(
            "▏".to_string(),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {}{}  (Ctrl-S next · Ctrl-R prev · RET/Esc done · Ctrl-G cancel)", pos, flags),
            Style::default().fg(Color::Black).bg(Color::Yellow),
        ),
    ];
    // Current-match context: readable on any terminal even when the
    // in-view highlight can't be (limited palette, relayed sessions).
    if let Some(snip) = snippet.filter(|s| !s.is_empty()) {
        spans.push(Span::styled(
            format!(" ▸ {}", snip),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let bar =
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Yellow).fg(Color::Black));
    f.render_widget(bar, area);
}

/// Highlight info for the SQL editor: all occurrences of `query` get a
/// tint, the match at (`current_row`, `current_col`) gets the cursor tint.
#[derive(Debug, Clone)]
pub struct EditorSearchHighlight {
    pub query: String,
    pub case_sensitive: bool,
    pub current_row: usize,
    pub current_col: usize,
    pub current_len: usize,
    pub has_current: bool,
}

/// Highlight info for result tables: any cell containing `query` gets a
/// tint, the match at (`current_row`, `current_col`) gets the cursor tint.
#[derive(Debug, Clone)]
pub struct TableSearchHighlight {
    pub query: String,
    pub case_sensitive: bool,
    pub current_row: Option<usize>,
    pub current_col: usize,
    pub has_current: bool,
}

fn cell_contains(haystack: &str, query: &str, case_sensitive: bool) -> bool {
    if query.is_empty() {
        return false;
    }
    if case_sensitive {
        haystack.contains(query)
    } else {
        haystack.to_lowercase().contains(&query.to_lowercase())
    }
}

/// Search highlight styles, built only from 4-bit ANSI colors plus
/// modifiers so they survive limited terminals (git-bash/mintty, jump-host
/// proxies that strip 24-bit sequences): other matches black-on-yellow,
/// the current match black-on-green + bold + underline — distinct from
/// each other and from the cyan drag-selection.
fn search_styles() -> (Style, Style) {
    let match_style = Style::default().bg(Color::Yellow).fg(Color::Black);
    let current_style = Style::default()
        .bg(Color::Green)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    (match_style, current_style)
}

/// Split `line` into spans: every occurrence of `query` gets `match_style`,
/// the occurrence at byte `current_col` (on the current row) gets
/// `current_style` instead. Caller guarantees `query` is non-empty.
fn highlight_spans(
    line: &str,
    query: &str,
    case_sensitive: bool,
    current_col: Option<usize>,
    base: Style,
    match_style: Style,
    current_style: Style,
) -> Vec<Span<'static>> {
    let idxs: Vec<(usize, usize)> = if case_sensitive {
        line.match_indices(query)
            .map(|(o, m)| (o, m.len()))
            .collect()
    } else {
        let lower = line.to_lowercase();
        let q = query.to_lowercase();
        lower
            .match_indices(q.as_str())
            .map(|(o, m)| (o, m.len()))
            .collect()
    };
    if idxs.is_empty() {
        return vec![Span::styled(line.to_string(), base)];
    }
    let mut spans = Vec::new();
    let mut pos = 0usize;
    for (off, len) in idxs {
        let off = off.min(line.len());
        let mut end = off.saturating_add(len).min(line.len());
        if off > 0 && !line.is_char_boundary(off) {
            // Lower-casing edge case: fall back to base text.
            return vec![Span::styled(line.to_string(), base)];
        }
        while end < line.len() && !line.is_char_boundary(end) {
            end -= 1;
        }
        if off < pos {
            continue;
        }
        if off > pos {
            spans.push(Span::styled(line[pos..off].to_string(), base));
        }
        let st = if Some(off) == current_col {
            current_style
        } else {
            match_style
        };
        spans.push(Span::styled(line[off..end.max(off)].to_string(), st));
        pos = end.max(off);
    }
    if pos < line.len() {
        spans.push(Span::styled(line[pos..].to_string(), base));
    }
    if spans.is_empty() {
        spans.push(Span::styled(line.to_string(), base));
    }
    spans
}

#[allow(clippy::too_many_arguments)]
pub fn render_status_bar(
    session: &Session,
    active_window: ActiveWindow,
    session_view_focus: SessionViewPane,
    session_view_maximized: Option<SessionViewPane>,
    explorer_focus: ExplorerPane,
    explorer_maximized: bool,
    perf_focus: PerfPane,
    perf_maximized: bool,
    mouse_capture: bool,
    f: &mut Frame,
    area: Rect,
) {
    let mode_str = match session.mode {
        SessionMode::Query => "[QUERY]",
        SessionMode::SessionView => "[SESSIONS]",
        SessionMode::Help => "[HELP]",
        SessionMode::History => "[HISTORY]",
        SessionMode::DbExplorer => "[EXPLORER]",
        SessionMode::ObjectDdl => "[DDL]",
        SessionMode::Performance => "[PERF]",
        SessionMode::ConnectionPickerDialog => "[CONNECTING...]",
    };

    let focus_str = if session.mode == SessionMode::SessionView {
        let base = match session_view_focus {
            SessionViewPane::List => "Focus: LIST",
            SessionViewPane::Sql => "Focus: SQL",
            SessionViewPane::Plan => "Focus: PLAN",
        };
        if session_view_maximized.is_some() {
            format!("{} [MAX]", base)
        } else {
            base.to_string()
        }
    } else if session.mode == SessionMode::DbExplorer {
        let base = match explorer_focus {
            ExplorerPane::Tree => "Focus: OBJECTS",
            ExplorerPane::Detail => "Focus: DETAIL",
        };
        if explorer_maximized {
            format!("{} [MAX]", base)
        } else {
            base.to_string()
        }
    } else if session.mode == SessionMode::Performance {
        let base = match perf_focus {
            PerfPane::Queries => "Focus: QUERIES",
            PerfPane::Results => "Focus: RESULTS",
            PerfPane::Sql => "Focus: SQL",
        };
        if perf_maximized {
            format!("{} [MAX]", base)
        } else {
            base.to_string()
        }
    } else if session.mode == SessionMode::ObjectDdl {
        "Focus: DDL".to_string()
    } else {
        match active_window {
            ActiveWindow::Editor => "Focus: EDITOR".to_string(),
            ActiveWindow::Results => "Focus: RESULTS".to_string(),
            ActiveWindow::Sidebar => "Focus: SIDEBAR".to_string(),
        }
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
            " | Mouse:OFF (Alt+M on, gutter hidden)",
            Style::default().fg(Color::Magenta),
        )
    };

    let hint = if session.mode == SessionMode::SessionView {
        " | Tab: Focus | ↑↓/PgUp/PgDn: Scroll focused | Home/End: Top/Bottom | Alt+Up/Down: Size | Ctrl+Z: Zoom | Enter: Explain | Esc: Back"
    } else if session.mode == SessionMode::DbExplorer {
        " | Tab: Pane | F4: DDL | Ctrl+Z: Zoom | Alt+Up/Down: Resize | Arrows/PgUp/PgDn: Scroll | Ctrl+F: More | Esc: Back"
    } else if session.mode == SessionMode::ObjectDdl {
        " | Ctrl+C/Y: Copy DDL | Enter/i: →Editor | n: →New Tab | r: Reload | Arrows/PgUp/PgDn: Scroll | F4/Esc: Back"
    } else if session.mode == SessionMode::Performance {
        " | 1-0/↑↓: Query | Enter/R: Run | a: Auto-5s | v: SQL | s: →Editor | Tab: Pane | Ctrl+Z: Zoom | F6/Esc: Back"
    } else {
        " | Tab: Focus | Ctrl+S: Search | F2: Sessions | F4: DDL | F6: Perf | F12: Explorer | Ctrl+C: Copy | Ctrl+V: Paste"
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
        Span::raw(hint),
    ]);

    let bar = Paragraph::new(status_text)
        .style(Style::default().bg(Color::Rgb(30, 30, 30)).fg(Color::White));
    f.render_widget(bar, area);
}

pub fn render_editor(
    session: &Session,
    focused: bool,
    show_line_numbers: bool,
    search: Option<&EditorSearchHighlight>,
    f: &mut Frame,
    area: Rect,
) {
    // Mouse:OFF native-copy mode: no box borders and no gutter, so a
    // terminal drag-select copies clean SQL without `│`/`─` or ` 12 ` junk.
    // (Borders stay on in normal mouse-capture mode.)
    let borderless = !show_line_numbers;
    let border_color = if session.pending_query {
        Color::Yellow
    } else if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };

    let title = if session.pending_query {
        " SQL Editor (⟳ EXECUTING QUERY...) "
    } else if show_line_numbers {
        " SQL Editor (Ctrl+Enter/Alt+Enter/F8/F9: run stmt | F5/Ctrl+R: script | F4: object DDL | F6: perf | Ctrl+Space: complete | drag: select+copy) "
    } else {
        " SQL Editor — Mouse:OFF, gutter hidden for clean copy (Alt+M restores | Ctrl+Y copies SQL) "
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner_area = if borderless { area } else { block.inner(area) };
    let vis_height = inner_area.height as usize;
    let vis_width = inner_area.width as usize;
    let scroll = session.editor.scroll_offset;
    let hscroll = session.editor.col_offset;

    /// Byte range of `line` covering display cells `[skip, skip+width)`.
    /// The end includes the char overlapping the right edge (the terminal
    /// truncates it); an empty window yields an empty range.
    fn visible_byte_range(line: &str, skip: usize, width: usize) -> (usize, usize) {
        if width == 0 {
            return (0, 0);
        }
        let mut cells = 0usize;
        let mut start = line.len();
        let mut end = line.len();
        for (bi, ch) in line.char_indices() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(1);
            if start == line.len() && cells + w > skip {
                start = bi;
            }
            cells += w;
            if cells >= skip + width {
                end = bi + ch.len_utf8();
                break;
            }
        }
        if start > line.len() {
            start = line.len();
        }
        // No visible chars (line scrolled fully past): empty range at end.
        if start == line.len() {
            return (line.len(), line.len());
        }
        // If we never reached the right edge, the window runs to EOL.
        if end < start {
            end = start;
        }
        (start.min(line.len()), end.min(line.len()))
    }

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

            // Horizontal window into this line.
            let (vb, ve) = visible_byte_range(line_str, hscroll, vis_width.max(1));
            let vis_slice = &line_str[vb..ve];

            // Mouse drag-selection window for this line, if any. Endpoints
            // are floored to char boundaries so wide chars highlight cleanly.
            // Intersected with the horizontal viewport.
            let sel_span: Option<(usize, usize)> = session.editor_selection.and_then(|sel| {
                let ((sr, sc), (er, ec)) = sel.ordered();
                if sr == er && sc == ec {
                    return None;
                }
                if idx < sr || idx > er {
                    return None;
                }
                let len = line_str.len();
                let mut a = if idx == sr { sc.min(len) } else { 0 };
                let mut b = if idx == er { ec.min(len) } else { len };
                while a > 0 && !line_str.is_char_boundary(a) {
                    a -= 1;
                }
                while b > 0 && !line_str.is_char_boundary(b) {
                    b -= 1;
                }
                // Intersect with viewport.
                a = a.max(vb).min(ve);
                b = b.max(vb).min(ve);
                if a >= b {
                    None
                } else {
                    Some((a - vb, b - vb))
                }
            });

            if show_line_numbers {
                if let Some((a, b)) = sel_span {
                    // Selected text wins over the cursor/statement tint.
                    let sel_style = Style::default()
                        .bg(Color::Cyan)
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD);
                    let mut spans = vec![Span::styled(line_num_str, num_style)];
                    for (bi, ch) in vis_slice.char_indices() {
                        let be = bi + ch.len_utf8();
                        let st = if bi >= a && be <= b { sel_style } else { text_style };
                        spans.push(Span::styled(ch.to_string(), st));
                    }
                    Line::from(spans)
                } else if let Some(hl) = search.filter(|h| !h.query.is_empty()) {
                    let (match_style, current_style) = search_styles();
                    let cur = if hl.has_current && idx == hl.current_row {
                        hl.current_col.checked_sub(vb)
                    } else {
                        None
                    };
                    let mut spans = vec![Span::styled(line_num_str, num_style)];
                    spans.extend(highlight_spans(
                        vis_slice,
                        &hl.query,
                        hl.case_sensitive,
                        cur,
                        text_style,
                        match_style,
                        current_style,
                    ));
                    Line::from(spans)
                } else {
                    Line::from(vec![
                        Span::styled(line_num_str, num_style),
                        Span::styled(vis_slice.to_string(), text_style),
                    ])
                }
            } else {
                // Mouse:OFF native-selection mode: no gutter, so a terminal
                // drag-select copies clean SQL without `  12 ` prefixes.
                // (num_style/line_num_str intentionally unused here.)
                let _ = (&line_num_str, &num_style);
                if let Some(hl) = search.filter(|h| !h.query.is_empty()) {
                    let (match_style, current_style) = search_styles();
                    let cur = if hl.has_current && idx == hl.current_row {
                        hl.current_col.checked_sub(vb)
                    } else {
                        None
                    };
                    Line::from(highlight_spans(
                        vis_slice,
                        &hl.query,
                        hl.case_sensitive,
                        cur,
                        text_style,
                        match_style,
                        current_style,
                    ))
                } else {
                    Line::from(vec![Span::styled(vis_slice.to_string(), text_style)])
                }
            }
        })
        .collect();

    // Horizontal scrollbar when any line is wider than the viewport.
    let max_w = session
        .editor
        .lines
        .iter()
        .map(|l| UnicodeWidthStr::width(l.as_str()))
        .max()
        .unwrap_or(0);
    let show_hbar = max_w > vis_width.max(1);

    if borderless {
        f.render_widget(Paragraph::new(lines), area);
    } else {
        let paragraph = Paragraph::new(lines).block(block);
        f.render_widget(paragraph, area);
    }
    if show_hbar && area.height >= 3 {
        let max_h = max_w.saturating_sub(vis_width.max(1)).max(1);
        let mut hstate = ScrollbarState::new(max_h).position(hscroll.min(max_h));
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::HorizontalBottom)
                .thumb_style(Style::default().fg(Color::DarkGray)),
            area,
            &mut hstate,
        );
    }
}

/// Draw the completion popup just below (or above) the cursor. This overlays
/// the editor area only, so it never disturbs the results pane.
pub fn render_completion_popup(
    session: &Session,
    popup: &CompletionPopup,
    show_gutter: bool,
    f: &mut Frame,
    editor_area: Rect,
) {
    if editor_area.width < 12 || editor_area.height < 4 {
        return;
    }
    let ed = &session.editor;
    let gutter = if show_gutter { 4 } else { 0 };
    let frame: u16 = if show_gutter { 1 } else { 0 };
    let visible_row = ed.cursor_row.saturating_sub(ed.scroll_offset);
    let line = &ed.lines[ed.cursor_row];
    let mut col = ed.cursor_col.min(line.len());
    while col > 0 && !line.is_char_boundary(col) {
        col -= 1;
    }
    let cursor_cells = UnicodeWidthStr::width(&line[..col]) as u16;
    let x_off = cursor_cells.saturating_sub(ed.col_offset as u16);

    let max_rows = 10usize;
    let rows = popup.items.len().min(max_rows);
    let height = (rows as u16 + if rows == 0 { 3 } else { 2 })
        .min(editor_area.height.saturating_sub(2))
        .max(3);

    // Width: tag + label + detail, clamped to the editor and the screen.
    let mut width = 22u16;
    for item in popup.items.iter().take(max_rows.max(1) * 2) {
        let w = 2
            + 1
            + 1
            + item.label.len() as u16
            + if item.detail.is_empty() {
                0
            } else {
                item.detail.len() as u16 + 2
            };
        width = width.max(w);
    }
    let max_width = f.area().right().saturating_sub(editor_area.x + frame);
    let width = width.min(64).min(max_width).max(12.min(max_width));

    let below_y = editor_area.y + frame + visible_row as u16 + 1;
    let below_room = editor_area.bottom().saturating_sub(below_y);
    let y = if below_room >= height || below_y <= editor_area.y + frame + height {
        below_y.min(editor_area.bottom().saturating_sub(height))
    } else {
        (below_y.saturating_sub(height)).max(editor_area.y + frame)
    };
    let x = (editor_area.x + frame + gutter + x_off)
        .min(f.area().right().saturating_sub(width))
        .max(editor_area.x + frame);
    let area = Rect::new(x, y, width, height);

    f.render_widget(Clear, area);

    let title = if popup.loading {
        " Completion — loading catalog… "
    } else {
        " Completion (↑↓ select, Enter insert, Esc close) "
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if popup.items.is_empty() {
        let msg = popup.note.as_deref().unwrap_or(if popup.loading {
            "Loading catalog metadata…"
        } else {
            "No suggestions"
        });
        let p = Paragraph::new(msg).style(Style::default().fg(Color::DarkGray));
        f.render_widget(p, inner);
        return;
    }

    let visible_rows = inner.height as usize;
    let start = popup.scroll_offset(visible_rows);
    let mut lines: Vec<Line> = Vec::with_capacity(visible_rows);
    for (i, item) in popup
        .items
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_rows)
    {
        let selected = i == popup.selected;
        let tag_style = if selected {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().fg(Color::Yellow)
        };
        let label_style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let detail_style = if selected {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let mut spans = vec![
            Span::styled(
                format!("{} ", if selected { "▸" } else { " " }),
                if selected {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default()
                },
            ),
            Span::styled(format!("{} ", item.kind.tag()), tag_style),
            Span::styled(item.label.clone(), label_style),
        ];
        if !item.detail.is_empty() {
            spans.push(Span::styled(format!("  {}", item.detail), detail_style));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

pub fn render_history(session: &Session, f: &mut Frame, area: Rect) {
    let total = session.query_history.len();
    if total == 0 {
        let block = Block::default()
            .title(" SQL History Browser (Up/Down: navigate | Enter: insert into editor | Esc: exit) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow));
        let p = Paragraph::new("No history entries recorded yet.")
            .style(Style::default().fg(Color::DarkGray))
            .block(block);
        f.render_widget(p, area);
        return;
    }

    let cursor = session.history_cursor.min(total - 1);
    let avail_height = area.height.saturating_sub(2) as usize;
    let max_scroll = total.saturating_sub(avail_height.max(1));
    let mut scroll = session.history_scroll.min(max_scroll);
    if avail_height > 0 {
        if cursor < scroll {
            scroll = cursor;
        } else if cursor >= scroll + avail_height {
            scroll = cursor + 1 - avail_height;
        }
    }

    let block = Block::default()
        .title(format!(
            " SQL History [{}/{}] (↑/↓/PgUp/PgDn/Home/End: navigate | Enter: insert | Esc/q: exit) ",
            cursor + 1,
            total
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));

    let items: Vec<ListItem> = session
        .query_history
        .iter()
        .enumerate()
        .skip(scroll)
        .take(avail_height.max(1))
        .map(|(i, stmt)| {
            let style = if i == cursor {
                Style::default()
                    .bg(Color::Rgb(60, 60, 100))
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let prefix = if i == cursor { "▶ " } else { "  " };
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

    if total > avail_height.max(1) && area.height >= 5 {
        let mut vstate = ScrollbarState::new(max_scroll.max(1)).position(scroll);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(Color::Yellow)),
            area,
            &mut vstate,
        );
    }
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
    search: Option<&TableSearchHighlight>,
    f: &mut Frame,
    area: Rect,
) -> ResultsLayout {
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
        return ResultsLayout::default();
    }

    if let Some(qr) = query_row {
        if qr.is_error {
            let err_msg = qr.error_msg.as_deref().unwrap_or("Unknown error");
            let p = Paragraph::new(format!("ERROR:\n{}", err_msg))
                .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
                .block(block);
            f.render_widget(p, area);
            return ResultsLayout::default();
        }

        match result_format {
            ResultFormat::Markdown => {
                // Same responsive column window as Table mode (not a fixed
                // 10-col slice), so a wide screen shows more columns here
                // too and Left/Right scroll identically in every format.
                let total_cols = qr.columns.len();
                let avail_width = area.width.saturating_sub(4) as usize;
                let ideal = ideal_col_widths(qr);
                let range =
                    visible_col_range_fit(total_cols, col_scroll_offset, &ideal, avail_width.max(20));
                let start_col = range.start;
                let end_col = range.end;
                let visible_cols = &qr.columns[range.clone()];

                // Markdown text occupies header + separator + one line per
                // row inside the block, so visible rows = height - 4.
                let avail_height = area.height.saturating_sub(4) as usize;
                let top = scroll_offset.min(qr.rows.len().saturating_sub(1));
                let rows: Vec<&[String]> = qr
                    .rows
                    .iter()
                    .skip(top)
                    .take(avail_height)
                    .map(|r| row_window(r, &range))
                    .collect();

                let visible_rows = rows.len();
                let sel_rect =
                    selected_cell_rect(session.table_selection, qr.rows.len(), total_cols);
                // Exact per-cell highlight: the builder emits the same text
                // as `markdown_table`, only selected cells get styled.
                let md_lines = markdown_table_lines(
                    visible_cols,
                    &rows,
                    top,
                    start_col,
                    sel_rect,
                    search,
                );
                let counts = result_title_counts(
                    top,
                    visible_rows,
                    qr.rows.len(),
                    start_col,
                    end_col,
                    total_cols,
                    qr.truncated,
                );
                let sel_note =
                    selection_title_note(session.table_selection, qr.rows.len(), total_cols);
                let p = Paragraph::new(md_lines)
                    .style(Style::default().fg(Color::White))
                    .block(
                        Block::default()
                            .title(format!(" {} (Markdown){} ", counts, sel_note))
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Magenta)),
                    );
                f.render_widget(p, area);
                render_result_scrollbars(f, area, top, qr, start_col, end_col, avail_height);
                return ResultsLayout {
                    top_row: top,
                    start_col,
                    end_col,
                    col_starts: Vec::new(),
                    inner_width: area.width.saturating_sub(2),
                    visible_rows,
                    header_y: 1,
                    first_data_y: 3,
                };
            }
            ResultFormat::Ascii => {
                // Same responsive column window as Table mode.
                let total_cols = qr.columns.len();
                let avail_width = area.width.saturating_sub(4) as usize;
                let ideal = ideal_col_widths(qr);
                let range =
                    visible_col_range_fit(total_cols, col_scroll_offset, &ideal, avail_width.max(20));
                let start_col = range.start;
                let end_col = range.end;
                // The ASCII text draws its own borders + header + separator
                // inside the outer block (4 chrome lines), so visible data
                // rows = height - 2 (block) - 4 (inner chrome).
                let avail_height = area.height.saturating_sub(6) as usize;
                let top = scroll_offset.min(qr.rows.len().saturating_sub(1));
                let ascii_text = format_ascii_table(qr, range.clone(), top, avail_height);
                let visible_rows = qr.rows.len().saturating_sub(top).min(avail_height);
                let counts = result_title_counts(
                    top,
                    visible_rows,
                    qr.rows.len(),
                    start_col,
                    end_col,
                    total_cols,
                    qr.truncated,
                );
                let sel_rect =
                    selected_cell_rect(session.table_selection, qr.rows.len(), total_cols);
                let sel_note =
                    selection_title_note(session.table_selection, qr.rows.len(), total_cols);
                let mut ascii_lines: Vec<Line> = ascii_text
                    .lines()
                    .map(|l| Line::from(l.to_string()))
                    .collect();
                // Exact per-cell highlight: header is line 1, data from
                // line 3. Search matches tint too (current match wins over
                // the drag-selection so the aim stays visible).
                let has_search = search.is_some_and(|hl| !hl.query.is_empty());
                if sel_rect.is_some() || has_search {
                    let merge_tints = |row: Option<usize>| -> Vec<AsciiCellTint> {
                        (start_col..end_col)
                            .map(|c| {
                                let is_current = match search {
                                    Some(hl) if hl.has_current => {
                                        hl.current_row == row && hl.current_col == c
                                    }
                                    _ => false,
                                };
                                if is_current {
                                    return AsciiCellTint::SearchCurrent;
                                }
                                let hit = match search {
                                    Some(hl) if !hl.query.is_empty() => {
                                        let text = match row {
                                            None => qr.columns.get(c).map(String::as_str),
                                            Some(r) => qr
                                                .rows
                                                .get(r)
                                                .and_then(|rr| rr.get(c))
                                                .map(String::as_str),
                                        };
                                        text.is_some_and(|t| {
                                            cell_contains(t, &hl.query, hl.case_sensitive)
                                        })
                                    }
                                    _ => false,
                                };
                                if hit {
                                    return AsciiCellTint::SearchHit;
                                }
                                let selected = match sel_rect {
                                    Some(((r0, c0), (r1, c1))) => {
                                        c >= c0
                                            && c <= c1
                                            && match (row, r0, r1) {
                                                (None, None, _) => true,
                                                (Some(r), _, _) => {
                                                    r0.is_none_or(|s| r >= s)
                                                        && r1.is_none_or(|e| r <= e)
                                                }
                                                _ => false,
                                            }
                                    }
                                    None => false,
                                };
                                if selected {
                                    AsciiCellTint::Selected
                                } else {
                                    AsciiCellTint::Plain
                                }
                            })
                            .collect()
                    };
                    // Header line.
                    if let Some(line) = ascii_lines.get_mut(1) {
                        tint_ascii_cells(line, &merge_tints(None));
                    }
                    // Data lines.
                    for k in 0..visible_rows {
                        let abs = top + k;
                        if let Some(line) = ascii_lines.get_mut(3 + k) {
                            tint_ascii_cells(line, &merge_tints(Some(abs)));
                        }
                    }
                }
                let p = Paragraph::new(ascii_lines)
                    .style(Style::default().fg(Color::White))
                    .block(
                        Block::default()
                            .title(format!(" {} (ASCII){} ", counts, sel_note))
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Cyan)),
                    );
                f.render_widget(p, area);
                render_result_scrollbars(f, area, top, qr, start_col, end_col, avail_height);
                // ASCII grid lines look like `| cell | cell |`: first cell
                // starts 2 past the inner `|`, then width + 3 (` | `) each.
                let col_widths = ascii_col_widths(qr, &range, top, avail_height);
                let mut col_starts = Vec::with_capacity(end_col - start_col);
                let mut x: u16 = 3;
                for w in &col_widths {
                    col_starts.push(x);
                    x = x.saturating_add(*w as u16).saturating_add(3);
                }
                return ResultsLayout {
                    top_row: top,
                    start_col,
                    end_col,
                    col_starts,
                    inner_width: area.width.saturating_sub(2),
                    visible_rows,
                    header_y: 2,
                    first_data_y: 4,
                };
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
            return ResultsLayout::default();
        }

        let total_cols = qr.columns.len();
        // Responsive viewport: fit as many columns as the pane allows, so a
        // bigger screen shows a bigger table instead of a fixed 10-col slice
        // with dead space on the right.
        let avail_width = area.width.saturating_sub(4) as usize;
        let ideal = ideal_col_widths(qr);
        let range = visible_col_range_fit(total_cols, col_scroll_offset, &ideal, avail_width.max(20));
        let start_col = range.start;
        let end_col = range.end;
        let visible_cols: Vec<String> = qr.columns[range].to_vec();

        let total_rows = qr.rows.len();
        let avail_height = area.height.saturating_sub(4) as usize;

        // Active table drag-selection as an absolute cell rectangle, if any.
        // Rows are `None` for the header row.
        let sel_rect = selected_cell_rect(session.table_selection, total_rows, total_cols);
        let sel_style = Style::default()
            .bg(Color::Cyan)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD);
        let (search_match_style, search_current_style) = search_styles();
        let search_hit = |row: Option<usize>, col: usize| -> bool {
            match search {
                Some(hl) if !hl.query.is_empty() => match row {
                    None => {
                        qr.columns
                            .get(col)
                            .is_some_and(|h| cell_contains(h, &hl.query, hl.case_sensitive))
                    }
                    Some(r) => qr.rows.get(r).and_then(|rr| rr.get(col)).is_some_and(|c| {
                        cell_contains(c, &hl.query, hl.case_sensitive)
                    }),
                },
                _ => false,
            }
        };
        let search_is_current = |row: Option<usize>, col: usize| -> bool {
            match search {
                Some(hl) if hl.has_current => hl.current_row == row && hl.current_col == col,
                _ => false,
            }
        };
        let cell_selected = |abs_row: usize, abs_col: usize| -> bool {
            sel_rect.is_some_and(|((r0, c0), (r1, c1))| {
                abs_col >= c0
                    && abs_col <= c1
                    && r0.is_none_or(|s| abs_row >= s)
                    && r1.is_none_or(|e| abs_row <= e)
            })
        };
        let header_selected = |abs_col: usize| -> bool {
            sel_rect.is_some_and(|((r0, c0), (_, c1))| {
                r0.is_none() && abs_col >= c0 && abs_col <= c1
            })
        };

        let header = Row::new(
            visible_cols
                .iter()
                .enumerate()
                .map(|(k, h)| {
                    let cell = Cell::from(h.clone());
                    let abs_col = start_col + k;
                    if search_is_current(None, abs_col) {
                        cell.style(search_current_style)
                    } else if search_hit(None, abs_col) {
                        cell.style(search_match_style)
                    } else if header_selected(abs_col) {
                        cell.style(sel_style)
                    } else {
                        cell
                    }
                })
                .collect::<Vec<_>>(),
        )
        .style(
            Style::default()
                .bg(Color::Rgb(40, 40, 80))
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .height(1);

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
                let abs_row = scroll_offset + idx;
                let cells: Vec<Cell> = row
                    .iter()
                    .skip(start_col)
                    .take(end_col - start_col)
                    .enumerate()
                    .map(|(k, c)| {
                        let cell = Cell::from(clean_cell(c));
                        let abs_col = start_col + k;
                        if search_is_current(Some(abs_row), abs_col) {
                            cell.style(search_current_style)
                        } else if search_hit(Some(abs_row), abs_col) {
                            cell.style(search_match_style)
                        } else if cell_selected(abs_row, abs_col) {
                            cell.style(sel_style)
                        } else {
                            cell
                        }
                    })
                    .collect();
                Row::new(cells).style(Style::default().bg(bg).fg(Color::White))
            })
            .collect();

        // If result is truncated and we have space, show a "load more" footer row
        if qr.truncated && visible_rows.len() < avail_height {
            let hint = format!(
                " ↓ {} rows loaded — scroll or Ctrl+F for the next {} rows ",
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
        // Overflow arrows: visible at a glance, not just counts in the title.
        let data_rows = visible_rows.len();
        let counts = result_title_counts(
            scroll_offset,
            data_rows,
            total_rows,
            start_col,
            end_col,
            total_cols,
            qr.truncated,
        );
        let sel_note = selection_title_note(session.table_selection, total_rows, total_cols);
        let title = format!(
            " {} | {} rows, {} | {}ms{}{} ",
            counts,
            qr.total_fetched,
            format_bytes(qr.byte_count),
            qr.elapsed_ms,
            more_indicator,
            sel_note,
        );

        // Stretch columns to fill a wide screen instead of leaving dead
        // space: distribute leftover cells across the visible columns.
        let mut col_widths: Vec<usize> =
            ideal.iter().skip(start_col).take(end_col - start_col).cloned().collect();
        let used: usize = col_widths.iter().sum::<usize>() + col_widths.len();
        if avail_width > used && !col_widths.is_empty() {
            let mut extra = avail_width - used;
            // Round-robin so narrow columns grow too, capped at 60.
            while extra > 0 {
                let mut grown = false;
                for w in col_widths.iter_mut() {
                    if extra == 0 || *w >= 60 {
                        continue;
                    }
                    *w += 1;
                    extra -= 1;
                    grown = true;
                    if extra == 0 {
                        break;
                    }
                }
                if !grown {
                    break;
                }
            }
        }
        let widths: Vec<Constraint> =
            col_widths.iter().map(|w| Constraint::Length(*w as u16)).collect();

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
        render_result_scrollbars(
            f,
            area,
            scroll_offset,
            qr,
            start_col,
            end_col,
            avail_height,
        );
        // Column screen geometry for mouse hit-testing: first cell starts
        // one past the left border, columns are separated by one space.
        let mut col_starts = Vec::with_capacity(end_col - start_col);
        let mut x: u16 = 1;
        for w in &col_widths {
            col_starts.push(x);
            x = x.saturating_add(*w as u16).saturating_add(1);
        }
        ResultsLayout {
            top_row: scroll_offset,
            start_col,
            end_col,
            col_starts,
            inner_width: area.width.saturating_sub(2),
            visible_rows: qr
                .rows
                .len()
                .saturating_sub(scroll_offset)
                .min(avail_height),
            header_y: 1,
            first_data_y: 2,
        }
    } else {
        let p = Paragraph::new("No query executed yet. Type SQL and press F5.")
            .style(Style::default().fg(Color::DarkGray))
            .block(block);
        f.render_widget(p, area);
        ResultsLayout::default()
    }
}

/// Single-line, control-char-free cell text for the Table view.
///
/// Database strings can contain newlines/tabs (e.g. `VARCHAR2` with embedded
/// line breaks). A raw newline inside a `Row` would break the table layout,
/// so it is rendered as `⏎`; tabs become two spaces and other control chars
/// are dropped. Keeps the full value (ratatui truncates to the column width)
/// instead of breaking rows.
pub(crate) fn clean_cell(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.replace('\r', " ").replace('\n', " ⏎ ").replace('\t', "  ").chars() {
        if ch.is_control() {
            continue;
        }
        out.push(ch);
    }
    out
}

/// Ideal display width for every column: max of the header and a sample of
/// the loaded rows (cleaned, display-width aware). Clamped so one huge
/// `VARCHAR(4000)` can't eat the whole screen, but wide enough that normal
/// strings aren't truncated to the header width (the old bug: widths came
/// from `header.len()` only).
fn ideal_col_widths(qr: &QueryRow) -> Vec<usize> {
    let sample_rows = 60usize;
    qr.columns
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let mut w = UnicodeWidthStr::width(h.as_str()).max(8);
            for row in qr.rows.iter().take(sample_rows) {
                if let Some(cell) = row.get(i) {
                    w = w.max(UnicodeWidthStr::width(clean_cell(cell).as_str()));
                }
            }
            // +2 breathing room, clamp so a single CLOB doesn't dominate.
            (w + 2).clamp(10, 45)
        })
        .collect()
}

/// Column window that fits `avail_width` cells, starting at `start_col`.
/// Guarantees at least one column; on wide screens this shows far more than
/// the old fixed 10-column window, on narrow screens it shrinks to fit.
pub(crate) fn visible_col_range_fit(
    total_cols: usize,
    start_col: usize,
    ideal: &[usize],
    avail_width: usize,
) -> std::ops::Range<usize> {
    if total_cols == 0 {
        return 0..0;
    }
    let start = start_col.min(total_cols.saturating_sub(1));
    let mut used = 0usize;
    let mut end = start;
    for (i, w) in ideal.iter().enumerate().skip(start) {
        // +1 for the column separator.
        let need = w + 1;
        if end > start && used + need > avail_width {
            break;
        }
        used += need;
        end = i + 1;
        if end >= total_cols {
            break;
        }
    }
    if end == start {
        end = (start + 1).min(total_cols);
    }
    start..end
}

/// Panic-safe row window: clamps `range` to the row's actual length so a
/// ragged row can never panic an ASCII/Markdown slice.
fn row_window<'a>(row: &'a [String], range: &std::ops::Range<usize>) -> &'a [String] {
    let end = range.end.min(row.len());
    let start = range.start.min(end);
    &row[start..end]
}

/// What `render_table` actually laid out: the visible data window plus the
/// on-screen geometry needed for mouse hit-testing. `header_y` /
/// `first_data_y` are screen offsets from the draw area's y for the
/// header line and the first data row (Table 1/2, Markdown 1/3, ASCII
/// 2/4 — each format draws different chrome above its rows).
/// `col_starts` holds column starts for uniformly-padded views (Table
/// widget cells, ASCII grid); Markdown cells are unpadded, so its
/// columns are resolved per row at hit-test time.
#[derive(Debug, Clone, Default)]
pub struct ResultsLayout {
    /// First visible data-row index.
    pub top_row: usize,
    /// First visible column index.
    pub start_col: usize,
    /// One past the last visible column index.
    pub end_col: usize,
    /// Column start offsets from the draw area's x (one per visible
    /// column). Empty when cells can't be hit-tested this way.
    pub col_starts: Vec<u16>,
    /// Inner width (draw area minus borders), for clamping the last column.
    pub inner_width: u16,
    /// How many data rows are actually drawn.
    pub visible_rows: usize,
    /// Screen offset (from area.y) of the header line.
    pub header_y: u16,
    /// Screen offset (from area.y) of the first data row.
    pub first_data_y: u16,
}

/// Selected table cells clamped to the data, if any:
/// `((r0, c0), (r1, c1))` with rows `None` for the header row.
pub fn selected_cell_rect(
    sel: Option<TableSelection>,
    total_rows: usize,
    total_cols: usize,
) -> Option<TableRect> {
    sel?.clamp(total_rows, total_cols)
}

/// Compact selection note for result titles (` [sel r3-5 c2-4]`,
/// ` [sel hdr c1-2]`, ` [sel hdr-5 c2-4]`), empty when nothing selected.
pub fn selection_title_note(
    sel: Option<TableSelection>,
    total_rows: usize,
    total_cols: usize,
) -> String {
    match selected_cell_rect(sel, total_rows, total_cols) {
        Some(((r0, c0), (r1, c1))) => {
            let rows = match (r0, r1) {
                (None, None) => "hdr".to_string(),
                (None, Some(e)) => format!("hdr-{}", e + 1),
                (Some(s), Some(e)) => format!("r{}-{}", s + 1, e + 1),
                // Ordered + clamped, so a data start always has a data end.
                (Some(s), None) => format!("r{}", s + 1),
            };
            format!(" [sel {} c{}-{}]", rows, c0 + 1, c1 + 1)
        }
        None => String::new(),
    }
}

/// Shared `rows a-b of M / cols c-d of N` title core with `▲▼◀▶` overflow
/// arrows, used by all three result formats so scrolled-out content is
/// visible at a glance (not just raw counts).
pub(crate) fn result_title_counts(
    top: usize,
    shown_rows: usize,
    total_rows: usize,
    start_col: usize,
    end_col: usize,
    total_cols: usize,
    truncated: bool,
) -> String {
    let up = if top > 0 { "▲ " } else { "" };
    let down = if top + shown_rows < total_rows || truncated {
        " ▼"
    } else {
        ""
    };
    let left = if start_col > 0 { "◀ " } else { "" };
    let right = if end_col < total_cols { " ▶" } else { "" };
    format!(
        "{}rows {}-{} of {}{}{}cols {}-{} of {}{}",
        up,
        top + 1,
        (top + shown_rows).min(total_rows),
        total_rows,
        down,
        left,
        start_col + 1,
        end_col,
        total_cols,
        right
    )
}

/// Thin scrollbars shared by all three result formats: vertical when more
/// rows sit below the visible window, horizontal when more columns sit to
/// the right. `row_capacity` is how many data rows fit in the pane.
pub(crate) fn render_result_scrollbars(
    f: &mut Frame,
    area: Rect,
    top: usize,
    qr: &QueryRow,
    start_col: usize,
    end_col: usize,
    row_capacity: usize,
) {
    if qr.rows.len() > row_capacity.max(1) && area.height >= 5 {
        let mut vstate =
            ScrollbarState::new(qr.rows.len().saturating_sub(1)).position(top);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(Color::Cyan)),
            area,
            &mut vstate,
        );
    }
    if end_col < qr.columns.len() && area.height >= 3 {
        let mut hstate =
            ScrollbarState::new(qr.columns.len().saturating_sub(1)).position(start_col);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::HorizontalBottom)
                .thumb_style(Style::default().fg(Color::Cyan)),
            area,
            &mut hstate,
        );
    }
}

/// Escape one cell for Markdown: single-line it and escape pipes so one
/// `VARCHAR` can't break the table layout. Shared by the renderer and
/// the Markdown hit-tester so both agree on cell widths.
fn markdown_esc_cell(s: &str) -> String {
    clean_cell(s).replace('|', "\\|")
}

/// Column at x for uniformly-padded views (Table widget cells, ASCII
/// grid): the last column whose start is at/left of the cursor.
pub fn uniform_col_at(
    layout: &ResultsLayout,
    area: Rect,
    column: u16,
    total_cols: usize,
) -> Option<usize> {
    if layout.col_starts.is_empty() || total_cols == 0 {
        return None;
    }
    let rel_x = column.saturating_sub(area.x);
    let mut c = layout.start_col;
    for (i, &sx) in layout.col_starts.iter().enumerate() {
        if sx <= rel_x {
            c = layout.start_col + i;
        } else {
            break;
        }
    }
    Some(c.min(total_cols.saturating_sub(1)))
}

/// Column at x for the Markdown view: cells are unpadded, so boundaries
/// are rebuilt from the clicked row's (or header's) own escaped cell
/// widths. `abs_row` is `None` on the header line.
pub fn markdown_col_at(
    qr: &QueryRow,
    layout: &ResultsLayout,
    area: Rect,
    abs_row: Option<usize>,
    column: u16,
) -> Option<usize> {
    let end = layout.end_col.min(qr.columns.len());
    let start = layout.start_col.min(end);
    if start >= end {
        return None;
    }
    let cells: Vec<String> = match abs_row {
        None => row_window(&qr.columns, &(start..end))
            .iter()
            .map(|s| markdown_esc_cell(s))
            .collect(),
        Some(r) => row_window(qr.rows.get(r)?, &(start..end))
            .iter()
            .map(|s| markdown_esc_cell(s))
            .collect(),
    };
    if cells.is_empty() {
        return None;
    }
    // "| " prefix = inner border (1) + 2; cells joined by " | " (3).
    let rel_x = column.saturating_sub(area.x);
    let mut x: u16 = 3;
    let mut c = start;
    for (i, cell) in cells.iter().enumerate() {
        if x <= rel_x {
            c = start + i;
        } else {
            break;
        }
        x = x
            .saturating_add(UnicodeWidthStr::width(cell.as_str()) as u16)
            .saturating_add(3);
    }
    Some(c)
}

/// Cell tint precedence for the ASCII grid: the current search match beats
/// a plain search hit, which beats the mouse drag-selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AsciiCellTint {
    Plain,
    Selected,
    SearchHit,
    SearchCurrent,
}

/// Tint exact cells of an ASCII grid line (`| a | bb |`): every `|…|`
/// segment maps to one window column. Reassembles to byte-identical text.
fn tint_ascii_cells(line: &mut Line, tints: &[AsciiCellTint]) {
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let parts: Vec<&str> = text.split('|').collect();
    if parts.len() < 3 {
        return;
    }
    let base = Style::default().fg(Color::White);
    let sel_style = Style::default().bg(Color::Cyan).fg(Color::Black);
    let (search_match_style, search_current_style) = search_styles();
    let style_for = |t: AsciiCellTint| match t {
        AsciiCellTint::Plain => base,
        AsciiCellTint::Selected => sel_style,
        AsciiCellTint::SearchHit => search_match_style,
        AsciiCellTint::SearchCurrent => search_current_style,
    };
    let mut spans = vec![Span::styled("|", base)];
    for (i, part) in parts[1..parts.len() - 1].iter().enumerate() {
        let seg = format!("{}|", part);
        let tint = tints.get(i).copied().unwrap_or(AsciiCellTint::Plain);
        spans.push(Span::styled(seg, style_for(tint)));
    }
    *line = Line::from(spans);
}

/// Markdown table as styled lines with exact per-cell selection
/// highlight. Produces byte-for-byte the same text as `markdown_table`
/// (verified by test) — only span styles differ for selected cells.
/// `top`/`start_col` map the window to absolute indices; `sel` is the
/// absolute selection rectangle. `search` tints cells containing the
/// query (current match wins over selection so the aim stays visible).
fn markdown_table_lines(
    columns: &[String],
    rows: &[&[String]],
    top: usize,
    start_col: usize,
    sel: Option<TableRect>,
    search: Option<&TableSearchHighlight>,
) -> Vec<Line<'static>> {
    let base = Style::default().fg(Color::White);
    let sel_style = Style::default().bg(Color::Cyan).fg(Color::Black);
    let (search_match_style, search_current_style) = search_styles();
    let search_hit = |text: &str| -> bool {
        match search {
            Some(hl) if !hl.query.is_empty() => {
                cell_contains(text, &hl.query, hl.case_sensitive)
            }
            _ => false,
        }
    };
    let search_is_current = |row: Option<usize>, col: usize| -> bool {
        match search {
            Some(hl) if hl.has_current => hl.current_row == row && hl.current_col == col,
            _ => false,
        }
    };
    let cell_selected = |abs_row: Option<usize>, abs_col: usize| -> bool {
        sel.is_some_and(|((r0, c0), (r1, c1))| {
            if abs_col < c0 || abs_col > c1 {
                return false;
            }
            match (abs_row, r0, r1) {
                (None, None, _) => true,
                (Some(r), _, _) => {
                    r0.is_none_or(|s| r >= s) && r1.is_none_or(|e| r <= e)
                }
                _ => false,
            }
        })
    };
    let mut lines = Vec::with_capacity(rows.len() + 2);
    // Header line.
    {
        let mut spans = vec![Span::styled("| ", base)];
        for (k, col) in columns.iter().enumerate() {
            if k > 0 {
                spans.push(Span::styled(" | ", base));
            }
            let abs_col = start_col + k;
            let st = if search_is_current(None, abs_col) {
                search_current_style
            } else if search_hit(col) {
                search_match_style
            } else if cell_selected(None, abs_col) {
                sel_style
            } else {
                base
            };
            spans.push(Span::styled(markdown_esc_cell(col), st));
        }
        spans.push(Span::styled(" |", base));
        lines.push(Line::from(spans));
    }
    // Separator line, identical to `markdown_table`'s.
    lines.push(Line::from(Span::styled(
        format!(
            "|{}|",
            columns.iter().map(|_| "---").collect::<Vec<_>>().join("|")
        ),
        base,
    )));
    // Data rows.
    for (j, row) in rows.iter().enumerate() {
        let mut spans = vec![Span::styled("| ", base)];
        for (k, cell) in row.iter().enumerate() {
            if k > 0 {
                spans.push(Span::styled(" | ", base));
            }
            let abs = (Some(top + j), start_col + k);
            let st = if search_is_current(abs.0, abs.1) {
                search_current_style
            } else if search_hit(cell) {
                search_match_style
            } else if cell_selected(abs.0, abs.1) {
                sel_style
            } else {
                base
            };
            spans.push(Span::styled(markdown_esc_cell(cell), st));
        }
        spans.push(Span::styled(" |", base));
        lines.push(Line::from(spans));
    }
    lines
}

/// Markdown table over a column window + row slices. Shared by the Markdown
/// result format (windowed) and Ctrl+Y clipboard export (full range).
/// Pipes and newlines inside string values are escaped/collapsed so one
/// `VARCHAR` can't break the whole table layout.
pub(crate) fn markdown_table(columns: &[String], rows: &[&[String]]) -> String {
    let mut md = String::new();
    md.push_str("| ");
    md.push_str(
        &columns
            .iter()
            .map(|c| markdown_esc_cell(c))
            .collect::<Vec<_>>()
            .join(" | "),
    );
    md.push_str(" |\n|");
    for _ in columns {
        md.push_str("---|");
    }
    md.push('\n');
    for row in rows {
        md.push_str("| ");
        md.push_str(
            &row
                .iter()
                .map(|c| markdown_esc_cell(c))
                .collect::<Vec<_>>()
                .join(" | "),
        );
        md.push_str(" |\n");
    }
    md
}

/// Column widths for an ASCII page (display-width aware, cleaned cells).
/// Shared by the renderer and the hit-test geometry so both agree.
pub fn ascii_col_widths(
    qr: &QueryRow,
    range: &std::ops::Range<usize>,
    scroll_offset: usize,
    avail_height: usize,
) -> Vec<usize> {
    let mut col_widths = Vec::new();
    for i in range.clone() {
        let mut w = UnicodeWidthStr::width(clean_cell(&qr.columns[i]).as_str());
        for row in qr.rows.iter().skip(scroll_offset).take(avail_height) {
            for cell in row_window(row, &(i..i + 1)) {
                w = w.max(UnicodeWidthStr::width(clean_cell(cell).as_str()));
            }
        }
        col_widths.push(w);
    }
    col_widths
}

/// Format a result page as an ASCII table using display width (so wide /
/// multibyte characters align correctly). `range` is the caller's column
/// window (responsive fit when rendering, full `0..len` for clipboard
/// export); rows are sliced defensively so ragged data can't panic.
pub fn format_ascii_table(
    qr: &QueryRow,
    range: std::ops::Range<usize>,
    scroll_offset: usize,
    avail_height: usize,
) -> String {
    if qr.columns.is_empty() || qr.rows.is_empty() {
        return "No data to display".to_string();
    }

    let end = range.end.min(qr.columns.len());
    let start = range.start.min(end);
    let visible_range = start..end;

    let col_widths = ascii_col_widths(qr, &visible_range, scroll_offset, avail_height);

    // Pad `s` on the right to the display width `w`.
    fn padded(s: &str, w: usize) -> String {
        format!("{}{}", s, " ".repeat(w - UnicodeWidthStr::width(s) + 1))
    }

    let columns: Vec<String> = row_window(&qr.columns, &visible_range)
        .iter()
        .map(|s| clean_cell(s))
        .collect();
    let rows: Vec<Vec<String>> = qr
        .rows
        .iter()
        .skip(scroll_offset)
        .take(avail_height)
        .map(|r| {
            row_window(r, &visible_range)
                .iter()
                .map(|s| clean_cell(s))
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

pub(crate) fn format_bytes(bytes: usize) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

/// Layout state for the F2 session viewer (owned by `App`, same idea as
/// `editor_height` / `sidebar_split_pct` in the query tab).
#[derive(Debug, Clone, Copy)]
pub struct SessionViewLayout {
    /// Top list height as % of the view (15..85). Grown/shrunk with
    /// Alt+Up/Down while the List pane is focused.
    pub list_pct: u16,
    /// Middle SQL pane height in rows (3..30). Grown/shrunk with
    /// Alt+Up/Down while SQL (or Plan, inversely) is focused;
    /// Ctrl+Up/Down always targets SQL directly.
    pub sql_height: u16,
    /// Which pane has focus (Tab cycles, click sets). Drives the
    /// highlighted border and the Ctrl+Z maximize target.
    pub focus: SessionViewPane,
    /// When set, only this pane fills the view (Ctrl+Z / F11 toggles).
    pub maximized: Option<SessionViewPane>,
}

fn f2_border(focused: bool) -> Color {
    if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    }
}

/// Layout state for the F12 DB explorer (owned by `App`, same idea as
/// `SessionViewLayout`).
#[derive(Debug, Clone, Copy)]
pub struct ExplorerLayout {
    /// Tree share of the split as a % (25..85).
    pub split_pct: u16,
    /// Which pane has focus (Tab toggles).
    pub focus: ExplorerPane,
    /// When true, only the focused pane fills the view (Ctrl+Z / F11).
    pub maximized: bool,
}

pub fn render_session_overview(
    sm: &mut SessionManager,
    layout: SessionViewLayout,
    f: &mut Frame,
    area: Rect,
) -> (Rect, Rect, Rect) {
    // Maximized panes fill the whole view; hidden panes report empty areas
    // so mouse hit-testing ignores them.
    if let Some(which) = layout.maximized {
        match which {
            SessionViewPane::List => {
                render_f2_list(sm, layout, true, true, f, area);
                return (area, Rect::default(), Rect::default());
            }
            SessionViewPane::Sql => {
                let sess = sm.active_session();
                render_f2_sql(sess, true, true, f, area);
                return (Rect::default(), area, Rect::default());
            }
            SessionViewPane::Plan => {
                let sess = sm.active_session();
                render_f2_plan(sess, true, true, f, area);
                return (Rect::default(), Rect::default(), area);
            }
        }
    }

    let list_pct = layout.list_pct.clamp(15, 85);
    let sql_h = layout.sql_height.clamp(3, 30);
    let panes = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(list_pct), Constraint::Min(8)])
        .split(area);

    let list_area = panes[0];
    render_f2_list(
        sm,
        layout,
        layout.focus == SessionViewPane::List,
        false,
        f,
        list_area,
    );

    // --- detail panes: current SQL + explain plan ---
    let detail = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(sql_h), Constraint::Min(3)])
        .split(panes[1]);

    {
        let sess = sm.active_session();
        render_f2_sql(
            sess,
            layout.focus == SessionViewPane::Sql,
            false,
            f,
            detail[0],
        );
        render_f2_plan(
            sess,
            layout.focus == SessionViewPane::Plan,
            false,
            f,
            detail[1],
        );
    }

    (list_area, detail[0], detail[1])
}

#[allow(clippy::too_many_arguments)]
fn render_f2_list(
    sm: &mut SessionManager,
    layout: SessionViewLayout,
    focused: bool,
    maximized: bool,
    f: &mut Frame,
    area: Rect,
) {
    let db_type = sm.active_db_type();
    // Render from the visible (filtered/sorted) list — never hit the DB from
    // a draw call. The list is refreshed on open, automatically every
    // `session_refresh_secs` (via `tick_session_view()` in the main loop)
    // and manually with `R`.
    let db_sessions = sm.visible_session_list();
    let refresh_secs = sm.session_refresh_secs;
    let age_secs = sm.session_list_age_secs();
    let refresh_label = match (refresh_secs, age_secs) {
        (0, Some(age)) => format!("manual (R) · {}s ago", age),
        (0, None) => "manual (R)".to_string(),
        (secs, Some(age)) => format!("auto {}s · {}s ago · R refresh", secs, age),
        (secs, None) => format!("auto {}s · R refresh", secs),
    };
    let sess = sm.active_session();
    let cursor = sess
        .session_view_cursor
        .min(db_sessions.len().saturating_sub(1));
    let (active_only, sort_col, sort_desc) = (
        sess.session_view_active_only,
        sess.session_view_sort_col,
        sess.session_view_sort_desc,
    );
    let total_cached = sess.session_list_cache.len();
    let session_view_error = sess.session_view_error.clone();
    let list_loading = sess.session_list_loading;
    let (title_base, headers): (&str, Vec<&str>) = match db_type {
        DbType::Oracle => (
            "v$session",
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
            "pg_stat_activity",
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
    let filter_label = if active_only {
        format!(
            "ACTIVE only [{}/{}] (a: all)",
            db_sessions.len(),
            total_cached
        )
    } else {
        "all (a: ACTIVE)".to_string()
    };
    let sort_label = match sort_col {
        None => "unsorted (s: sort)".to_string(),
        Some(c) => {
            let name = headers.get(c).copied().unwrap_or("?");
            let arrow = if sort_desc { "▼" } else { "▲" };
            format!("{}{} (s: next, S: dir)", name, arrow)
        }
    };
    let zoom_hint = if maximized {
        " [MAX] Ctrl+Z restore"
    } else {
        " · Tab focus · Alt+↑/↓ grow/shrink focused · Ctrl+Z zoom"
    };
    let title = format!(
        " {} — ↑/↓ pick · Enter/e explain · r sql · R/F5 refresh · a: {} · s: {} · Esc back · {}{}{} ",
        title_base,
        filter_label,
        sort_label,
        refresh_label,
        if list_loading { " · ⟳" } else { "" },
        zoom_hint
    );
    let header_cells: Vec<String> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            if Some(i) == sort_col {
                format!("{}{}", h, if sort_desc { "▼" } else { "▲" })
            } else {
                h.to_string()
            }
        })
        .collect();
    let header = Row::new(header_cells).style(
        Style::default()
            .bg(Color::Rgb(40, 40, 60))
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );

    // Empty list: show why (privileges / not connected / ACTIVE filter)
    // instead of a bare table — an unexplained empty F2 looks like "no sessions".
    if db_sessions.is_empty() {
        let (err_title, body, style) = match &session_view_error {
            Some(err) => (
                format!(
                    " Sessions — error ({} · press R to retry, Esc to go back) ",
                    refresh_label
                ),
                err.clone(),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            None if list_loading => (
                format!(" Sessions — {} ", refresh_label),
                "⟳ Loading sessions…".to_string(),
                Style::default().fg(Color::Yellow),
            ),
            None if active_only && total_cached > 0 => (
                format!(" Sessions — {} ", refresh_label),
                format!(
                    "No ACTIVE sessions ({} total, press a to show all).",
                    total_cached
                ),
                Style::default().fg(Color::DarkGray),
            ),
            None => (
                format!(" Sessions — {} ", refresh_label),
                "No other sessions found.".to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        };
        let border = if session_view_error.is_some() {
            Color::Red
        } else {
            f2_border(focused)
        };
        let p = Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .style(style)
            .block(
                Block::default()
                    .title(err_title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border)),
            );
        f.render_widget(p, area);
        return;
    }
    // Keep the highlighted row visible.
    let avail = area.height.saturating_sub(4) as usize;
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
    .block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(f2_border(focused))),
    );

    f.render_widget(table, area);
    let _ = layout;
}

/// Wrap read-only SQL text to `width` display columns, preferring spaces.
/// Used by both the renderer and F2 key handling so scroll bounds match.
pub fn wrap_sql_lines(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for logical in text.split('\n') {
        let mut rest = logical;
        if rest.is_empty() {
            out.push(String::new());
            continue;
        }
        while UnicodeWidthStr::width(rest) > width {
            let mut w = 0usize;
            let mut cut = 0usize;
            let mut last_space = None;
            for (idx, ch) in rest.char_indices() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                if w + cw > width {
                    // If the overflow is a space, break before it so the
                    // current word still fits on this visual line.
                    if ch == ' ' {
                        last_space = Some(cut);
                    }
                    break;
                }
                w += cw;
                cut = idx + ch.len_utf8();
                if ch == ' ' {
                    last_space = Some(idx);
                }
            }
            // Always make progress (e.g. a wide char in a 1-column pane).
            if cut == 0 {
                cut = rest.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            }
            let split = last_space.filter(|&i| i > 0).unwrap_or(cut);
            out.push(rest[..split].trim_end().to_string());
            rest = rest[split..].trim_start_matches(' ');
        }
        out.push(rest.to_string());
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn render_f2_sql(session: &Session, focused: bool, maximized: bool, f: &mut Frame, area: Rect) {
    let sql_title = match &session.plan_for {
        Some(who) => {
            if maximized {
                format!(" Current SQL [MAX] — session {} (↑/↓/PgUp/PgDn: scroll · Home/End · r: reload · Ctrl+Z restore) ", who)
            } else {
                format!(" Current SQL — session {} (↑/↓/PgUp/PgDn scroll · Home/End · Alt+↑/↓ grow/shrink · Tab focus · Ctrl+Z zoom) ", who)
            }
        }
        None => {
            if maximized {
                " Current SQL [MAX] (↑/↓/PgUp/PgDn scroll · Home/End · Ctrl+Z restore) ".to_string()
            } else {
                " Current SQL (↑/↓/PgUp/PgDn scroll · Home/End · Alt+↑/↓ grow/shrink · Tab focus · Ctrl+Z zoom) ".to_string()
            }
        }
    };
    let sql_text = if session.sql_loading {
        "⟳ Loading SQL…"
    } else {
        session
            .plan_sql_text
            .as_deref()
            .unwrap_or("↑/↓ selects a session · its SQL loads here · Enter runs EXPLAIN")
    };
    let inner_w = area.width.saturating_sub(2) as usize;
    let view_h = area.height.saturating_sub(2) as usize;
    let lines = wrap_sql_lines(sql_text, inner_w);
    let max_scroll = lines.len().saturating_sub(view_h);
    let skip = session.sql_scroll.min(max_scroll);
    let visible: Vec<Line> = lines
        .into_iter()
        .skip(skip)
        .take(view_h.max(1))
        .map(Line::from)
        .collect();
    let sql_para = Paragraph::new(visible)
        .style(Style::default().fg(Color::White))
        .block(
            Block::default()
                .title(sql_title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(f2_border(focused))),
        );
    f.render_widget(sql_para, area);
}

/// Slice `s` from char offset `skip` (plan-pane horizontal scroll).
fn hslice(s: &str, skip: usize) -> String {
    if skip == 0 {
        return s.to_string();
    }
    s.chars().skip(skip).collect()
}

/// Bottom F2 pane: the stored explain plan (or a hint when none ran yet).
fn render_plan_pane(session: &Session, focused: bool, maximized: bool, f: &mut Frame, area: Rect) {
    let title = if maximized {
        " Explain plan [MAX] — ↑/↓/PgUp/PgDn: scroll · Home/End: top/bottom · ←/→: sideways · Ctrl+Z restore "
    } else {
        " Explain plan — Enter: run · ↑/↓/PgUp/PgDn: scroll · Home/End · ←/→ sideways · Alt+↑/↓ grow/shrink · Ctrl+Z zoom · Tab focus "
    };
    let block = || {
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(f2_border(focused)))
    };
    let Some(qr) = session.plan_result.as_ref() else {
        let msg = if session.plan_loading {
            "⟳ Running EXPLAIN…"
        } else if session.sql_loading {
            "⟳ Loading SQL first…"
        } else {
            "Press Enter to EXPLAIN the SQL above."
        };
        let p = Paragraph::new(msg)
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
    let view_h = area.height.saturating_sub(2) as usize;
    let max_scroll = lines.len().saturating_sub(view_h);
    let skip = session.plan_scroll.min(max_scroll);
    let visible: Vec<Line> = lines.into_iter().skip(skip).take(view_h.max(1)).collect();
    let p = Paragraph::new(visible)
        .style(Style::default().fg(Color::White))
        .block(block());
    f.render_widget(p, area);
}

fn render_f2_plan(session: &Session, focused: bool, maximized: bool, f: &mut Frame, area: Rect) {
    render_plan_pane(session, focused, maximized, f, area);
}

fn help_lines() -> Vec<Line<'static>> {
    vec![
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
        Line::from("  Ctrl+W              Close current tab (press twice when editor has SQL)"),
        Line::from("  Alt+1..9            Switch to session tab 1-9"),
        Line::from("  Alt+Left / Alt+Right  Previous / next session tab (Ctrl+Left/Right jumps by word in editor)"),
        Line::from("  Tab                 Switch active window (Editor / Results / Sidebar)"),
        Line::from("  Ctrl+Z / F11        Zoom / Maximize active panel"),
        Line::from("  Ctrl+B              Toggle connection/info sidebar"),
        Line::from("  Alt+Up / Alt+Down   Grow / shrink SQL editor height (or drag the editor/results splitter)"),
        Line::from("  Click tab bar       Switch session tab"),
        Line::from("  Mouse Drag Border   Resize panels: sidebar border, editor/results splitter"),
        Line::from("  Mouse Scroll        Scroll result table vertically (Shift+wheel: columns; wheel over editor scrolls it)"),
        Line::from("  Mouse drag editor   Select SQL text — release copies it to clipboard"),
        Line::from("  Middle-click        Paste selection at cursor (tmux style)"),
        Line::from("  Ctrl+V / Shift+Ins  Paste clipboard at cursor (OS clipboard, else frog's copy buffer — works over SSH)"),
        Line::from("  Ctrl+M / Alt+M        Toggle mouse capture — while OFF the editor"),
        Line::from("                      handles select/copy natively, gutter hidden for clean"),
        Line::from("                      SQL copy (or hold Shift+drag). Alt+M works on git-bash."),
        Line::from("  Alt+Y               Copy SQL statement to clipboard (no line numbers, OSC52)"),
        Line::from(""),
        Line::from(Span::styled(
            " ─ SQL Execution ─ ",
            Style::default().fg(Color::Cyan),
        )),
        Line::from("  Ctrl+Enter / Alt+Enter / Ctrl+J / F8 / F9  Execute statement under cursor"),
        Line::from("  F5 / Ctrl+R         Execute all statements as script"),
        Line::from("  Ctrl+:              Command bar (e.g. @file.sql, @@inc, clear)"),
        Line::from("  Ctrl+C / Esc          Cancel running query, else standard copy (selection, else statement/table)"),
        Line::from(""),
        Line::from(Span::styled(
            " ─ Results ─ ",
            Style::default().fg(Color::Cyan),
        )),
        Line::from("  Ctrl+F              Fetch next page of results (auto-loads when scrolling)"),
        Line::from("  Ctrl+Y              Contextual copy: table selection (else full table) in Results, drag-selection (else statement) in Editor"),
        Line::from("  Alt+Y               Copy SQL statement explicitly (never with line numbers)"),
        Line::from("  Mouse drag table    Select result cells — release copies in the view's format (TSV / Markdown with pipes / ASCII grid). Drag from the header row to copy column names"),
        Line::from("  Shift+drag          Terminal-native select+copy — works in Git Bash/mintty with no setup"),
        Line::from("                      (frog also copies via OS clipboard/helpers/OSC52; the status line names which worked)"),
        Line::from("  Ctrl+D              Cycle result format: Table → Markdown → ASCII"),
        Line::from("  Arrow keys / PgUp/PgDn  Scroll results"),
        Line::from(""),
        Line::from(Span::styled(
            " ─ SQL Editor ─ ",
            Style::default().fg(Color::Cyan),
        )),
        Line::from("  Arrow keys          Move cursor (Shift+wheel scrolls long lines sideways)"),
        Line::from("  Home / End          Start / end of line"),
        Line::from("  Ctrl+A / Ctrl+E     Start / end of line"),
        Line::from("  Ctrl+Left / Ctrl+Right  Jump one word left / right (Alt+B / Alt+F also work)"),
        Line::from("  Alt+Z / Alt+Shift+Z  Undo / redo edit"),
        Line::from("  Ctrl+K              Kill (delete) to end of line"),
        Line::from("  Ctrl+U              Delete to start of line"),
        Line::from("  Delete / Backspace  Delete character"),
        Line::from("  F3                  Open SQL history"),
        Line::from("  F4                  Toad-style Object Describe / DDL under cursor (table, index, view, package, …)"),
        Line::from("  Ctrl+Space          SQL completion popup (Ctrl+. fallback) — ↑/↓ select, Enter/Tab insert, Esc close"),
        Line::from(""),
        Line::from(Span::styled(
            " ─ Connection & Views ─ ",
            Style::default().fg(Color::Cyan),
        )),
        Line::from("  Ctrl+O              Open connection dialog (Type/Host/Port/Service|DB/User/Password)"),
        Line::from("    in dialog: Tab/Up/Down fields · Alt+N/P cycle config.yml saved connections · -c/--connection NAME picks one"),
        Line::from("    Postgres TLS: PGSSLMODE=require (FROG_SSLMODE alias; disable|prefer|allow|require|verify-ca|verify-full)"),
        Line::from("  F1                  This help screen"),
        Line::from("  F2                  Session browser — pick a session, Enter/e shows EXPLAIN plan"),
        Line::from("    in sessions: ↑/↓ pick · r reload SQL · R/F5 refresh · a ACTIVE-only filter · s cycle sort col · S direction"),
        Line::from("    panes: Tab focus (List/SQL/Plan) · Ctrl+Z/F11 zoom · Alt+↑/↓ grow/shrink focused pane · Ctrl+↑/↓ SQL size"),
        Line::from("    scroll: focus SQL/Plan then ↑↓/PgUp/PgDn · Home/End top/bottom · ←→ plan sideways · wheel scrolls pane under mouse"),
        Line::from("  F4                  Object DDL viewer — Ctrl+C/Y copy DDL · Enter/i load into editor · n new tab · r reload"),
        Line::from("  F6                  Performance & DBA dashboard — gauges/sparklines + curated DBA queries (locks, CPU, memory, waits)"),
        Line::from("    in perf: 1-0/↑↓ pick query · Enter/R refresh · a 5s live auto-refresh · v toggle SQL · s load SQL→editor · n→new tab"),
        Line::from("  F12                 DB explorer — schema/type folders, result-style preview, DDL (F4 opens full DDL)"),
        Line::from("    panes: Tab focus (Objects/Detail) · Ctrl+Z/F11 zoom · Alt+↑/↓ resize · ↑↓/PgUp/PgDn + ←→ scroll detail · Ctrl+F more rows · wheel scrolls"),
        Line::from("  Esc / q             Return to query editor"),
        Line::from("  Ctrl+Q              Quit application"),
        Line::from("  Ctrl+S              Incremental search (Emacs I-search) — type to find, Ctrl-S next, Ctrl-R prev, Enter/Esc done, Ctrl-G cancel"),
    ]
}

pub fn help_line_count() -> usize {
    help_lines().len()
}

pub fn render_help(scroll: usize, is_startup: bool, f: &mut Frame, area: Rect) {
    let lines = help_lines();
    let total = lines.len();
    let view_h = area.height.saturating_sub(2) as usize;
    let max_scroll = total.saturating_sub(view_h.max(1));
    let skip = scroll.min(max_scroll);
    let visible: Vec<Line<'static>> = lines
        .into_iter()
        .skip(skip)
        .take(view_h.max(1))
        .collect();
    let title = if is_startup {
        if max_scroll > 0 {
            " Welcome / Help (↑/↓/PgUp/PgDn: scroll · Any other key: start) ".to_string()
        } else {
            " Welcome / Help (Press any key to start) ".to_string()
        }
    } else if max_scroll > 0 {
        format!(
            " Help [{}-{}/{}] (↑/↓/PgUp/PgDn/Home/End: scroll · Ctrl+S: search · Esc/q/F1: close) ",
            skip + 1,
            (skip + visible.len()).min(total),
            total
        )
    } else {
        " Help (Ctrl+S: search · Esc/q/F1: close) ".to_string()
    };
    let p = Paragraph::new(visible).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );
    f.render_widget(p, area);
    if total > view_h.max(1) && area.height >= 5 {
        let mut vstate = ScrollbarState::new(max_scroll.max(1)).position(skip);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(Color::Yellow)),
            area,
            &mut vstate,
        );
    }
}

/// Plain-text help lines for incremental search (exact line indices matching `render_help`).
pub fn help_search_lines() -> Vec<String> {
    help_lines()
        .into_iter()
        .map(|l| {
            l.spans
                .into_iter()
                .map(|s| s.content.into_owned())
                .collect::<String>()
        })
        .collect()
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
            } else if i == 5 {
                // `masked_pw` uses one ASCII `*` per char, while `dlg.cursor`
                // is a byte offset into `dlg.password`: map bytes -> char count
                // so multibyte passwords position `▏` accurately and never slice
                // out of bounds.
                let mut byte_pos = dlg.cursor.min(dlg.password.len());
                while byte_pos > 0 && !dlg.password.is_char_boundary(byte_pos) {
                    byte_pos -= 1;
                }
                let p = dlg.password[..byte_pos].chars().count().min(value.len());
                Span::styled(
                    format!("{}▏{}", &value[..p], &value[p..]),
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
            " Tab/Up/Down: field ({}/{}) | Left/Right: cursor | Ctrl+U: clear | Alt+N/P: saved conn | Enter: connect | Esc: cancel ",
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

fn format_dialog_endpoint(dlg: &ConnectionDialog) -> String {
    match dlg.db_type {
        DbType::Oracle => format!(
            "Backend: oracle  {}:{}/{}",
            dlg.host,
            dlg.port,
            if dlg.service.is_empty() {
                dlg.database.as_str()
            } else {
                dlg.service.as_str()
            },
        ),
        DbType::Postgres => format!(
            "Backend: postgres  {}:{}/{}",
            dlg.host,
            dlg.port,
            if dlg.database.is_empty() {
                dlg.service.as_str()
            } else {
                dlg.database.as_str()
            },
        ),
    }
}

pub fn render_right_panel(session: &Session, f: &mut Frame, area: Rect) {
    let backend = if session.is_connected {
        if let Some(ref p) = session.connected_params {
            format!(
                "Backend: {}  {}:{}/{}",
                p.db_type,
                p.host,
                p.port,
                p.db_name()
            )
        } else {
            format_dialog_endpoint(&session.conn_dialog)
        }
    } else {
        format_dialog_endpoint(&session.conn_dialog)
    };
    let text = vec![
        Line::from(Span::styled(
            " Connections ",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(backend, Style::default().fg(Color::Cyan))),
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
    layout: ExplorerLayout,
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

    // Zoomed detail: the preview/DDL fills the whole view.
    if layout.maximized && layout.focus == ExplorerPane::Detail {
        render_explorer_detail(sm, result_format, true, true, f, area);
        return (Rect::default(), area);
    }

    let split_pct = layout.split_pct.clamp(25, 85);
    // Wide screens: tree | detail. Narrow: tree over detail.
    let wide = area.width >= 80;
    let (tree_area, detail_area, stacked) = if layout.maximized {
        // Zoomed tree: it gets the full view, detail hit-testing is disabled.
        (area, Rect::default(), false)
    } else if wide {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(split_pct),
                Constraint::Percentage(100 - split_pct),
            ])
            .split(area);
        (cols[0], cols[1], false)
    } else if area.height >= 14 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(split_pct), Constraint::Min(5)])
            .split(area);
        (rows[0], rows[1], true)
    } else {
        // Short window: the tree gets everything, preview hint in the title.
        (area, Rect::default(), false)
    };
    let show_detail = !layout.maximized && detail_area.height >= 4 && detail_area.width >= 20;

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
            sess.explorer_scroll = sess.explorer_scroll.min(total_rows.saturating_sub(1));
        }
        sess.explorer_scroll
    };
    let tree_view_h = tree_area.height.saturating_sub(2) as usize;

    render_explorer_tree(
        sm,
        f,
        tree_area,
        scroll,
        tree_view_h,
        show_detail || stacked,
        layout.focus == ExplorerPane::Tree,
        layout.maximized,
    );
    if show_detail {
        render_explorer_detail(
            sm,
            result_format,
            layout.focus == ExplorerPane::Detail,
            false,
            f,
            detail_area,
        );
    }
    (tree_area, detail_area)
}

/// Left pane of the explorer: the object tree itself.
#[allow(clippy::too_many_arguments)]
fn render_explorer_tree(
    sm: &SessionManager,
    f: &mut Frame,
    area: Rect,
    scroll: usize,
    view_h: usize,
    detail_hidden: bool,
    focused: bool,
    maximized: bool,
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
    let loading_note = if sess.explorer_struct_pending > 0 {
        " · ⟳"
    } else {
        ""
    };
    let title = if detail_hidden || maximized {
        if maximized {
            format!(
                " Objects [MAX] — Ctrl+Z restore · Tab focus · ↑↓ move · →/Enter expand · / filter{}{} ",
                filter_note, loading_note
            )
        } else {
            format!(
                " DB Explorer — ↑↓ move · →/Enter expand · F4 DDL · s SELECT→editor · d reload · / filter · r refresh · Tab pane · Ctrl+Z zoom · Esc back{}{} ",
                filter_note, loading_note
            )
        }
    } else {
        format!(
            " {}Objects ({} shown{}) · Tab pane · Ctrl+Z zoom{}{} ",
            if focused { "▸ " } else { "" },
            rows.len(),
            if sess.explorer_filter.is_empty() {
                String::new()
            } else {
                " · filtered".into()
            },
            filter_note,
            loading_note
        )
    };

    if !sess.explorer_loaded {
        let p = Paragraph::new("⟳ Loading schemas…")
            .style(Style::default().fg(Color::Yellow))
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(f2_border(focused))),
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
                    .border_style(Style::default().fg(f2_border(focused))),
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
            .border_style(Style::default().fg(f2_border(focused))),
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
fn explorer_tree_line(
    sess: &Session,
    row: &ExplorerRow,
    selected: bool,
    inner_w: usize,
) -> Line<'static> {
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
    focused: bool,
    maximized: bool,
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
        Some(ExplorerRow::Schema { idx }) => ExplorerDetail::Text(
            format!(" Detail — {} ", sess.explorer_schemas[*idx].name),
            {
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
            },
        ),
        Some(ExplorerRow::Group { sidx, gidx }) => ExplorerDetail::Text(
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
                    format!("{} in {}", g.map(|g| g.label()).unwrap_or("?"), s.name),
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
        ),
        Some(ExplorerRow::Table { sidx, gidx, tidx })
        | Some(ExplorerRow::Column {
            sidx, gidx, tidx, ..
        }) => explorer_object_detail(sess, *sidx, *gidx, *tidx),
    };

    match detail {
        ExplorerDetail::Text(title, content) => {
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
                    .title(format!(
                        " {}{} · ↑↓/PgUp/PgDn scroll ({} lines) · s SELECT→editor · d reload ",
                        title.trim(),
                        if maximized { " [MAX]" } else { "" },
                        total
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(f2_border(focused))),
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
                        .title(format!(
                            " {}{} · ↑↓/PgUp/PgDn scroll · ←→ cols ",
                            title.trim(),
                            if maximized { " [MAX]" } else { "" }
                        ))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(f2_border(focused))),
                );
                f.render_widget(p, chunks[0]);
                render_table(
                    sess,
                    &Some(preview),
                    TableViewState {
                        scroll_offset: sess.explorer_detail_scroll,
                        col_scroll_offset: sess.explorer_detail_col_scroll,
                        result_format,
                        focused,
                    },
                    None,
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
                        .title(format!(
                            " {}{} ",
                            title.trim(),
                            if maximized { "[MAX]" } else { "" }
                        ))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(f2_border(focused))),
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
    let (schema_name, t) = match sess.explorer_schemas.get(sidx).and_then(|s| {
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
    let title = {
        let more = if t.preview.as_ref().is_some_and(|p| p.truncated) {
            " · Ctrl+F more"
        } else {
            ""
        };
        format!(" Detail — {}.{} [{}]{} ", schema_name, t.name, t.kind, more)
    };
    let header = Line::from(Span::styled(
        format!("{}.{} — {}", schema_name, t.name, t.kind),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));

    // Column section for expandable objects.
    let mut top = vec![header];
    if let Some(err) = t.preview_error.as_ref() {
        top.push(Line::from(Span::styled(
            format!("⚠ Preview paging failed: {}", err),
            Style::default().fg(Color::Red),
        )));
    } else if t.preview_loading && t.preview.is_some() {
        top.push(Line::from(Span::styled(
            "⟳ Loading more rows…",
            Style::default().fg(Color::Yellow),
        )));
    }
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

    // Live preview in result-viewer style (same as executed SQL). Further
    // pages append in place.
    if let Some(qr) = t.preview.as_ref() {
        return D::Split(title, top, qr);
    }
    if t.has_preview() {
        top.push(Line::from(""));
        top.push(Line::from(if t.preview_loading {
            Span::styled("⟳ Loading preview…", Style::default().fg(Color::Yellow))
        } else {
            Span::styled("Preview pending…", Style::default().fg(Color::DarkGray))
        }));
    }
    D::Text(title, top)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::completion::{CompletionItem, CompletionPopup, ItemKind};
    use ratatui::backend::TestBackend;

    #[test]
    fn completion_popup_renders_inside_editor() {
        let mut sm = SessionManager::new();
        let mut session = sm.sessions.remove(0);
        session.editor.set_text("SELECT * FROM emp e WHERE e.");
        session.editor.cursor_col = session.editor.lines[0].len();
        session.completion = Some(CompletionPopup::new(
            vec![CompletionItem {
                label: "EMPNO".into(),
                insert_text: "EMPNO".into(),
                kind: ItemKind::Column,
                detail: "NUMBER".into(),
            }],
            28,
            0,
            30,
            false,
        ));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                render_completion_popup(
                    &session,
                    session.completion.as_ref().unwrap(),
                    true,
                    f,
                    area,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
        }
        assert!(text.contains("EMPNO"));
        assert!(text.contains("Completion"));
    }

    #[test]
    fn wider_pane_fits_more_table_columns() {
        let ideal = vec![12usize; 12];
        let narrow = visible_col_range_fit(12, 0, &ideal, 40);
        let wide = visible_col_range_fit(12, 0, &ideal, 200);
        assert!(wide.len() > narrow.len());
        assert_eq!(wide.start, 0);
    }

    fn sample_query_row() -> QueryRow {
        QueryRow {
            columns: vec!["ID".into(), "NAME".into(), "NOTE".into()],
            rows: (1..=8)
                .map(|i| vec![i.to_string(), format!("name{i}"), format!("note{i}")])
                .collect(),
            truncated: false,
            elapsed_ms: 7,
            is_error: false,
            error_msg: None,
            rows_affected: None,
            page_offset: 0,
            page_size: 100,
            byte_count: 0,
            total_fetched: 8,
        }
    }

    #[test]
    fn ascii_table_follows_scroll_and_column_window() {
        let qr = sample_query_row();
        // Vertical scroll skips the first rows.
        let scrolled = format_ascii_table(&qr, 0..3, 2, 3);
        assert!(scrolled.contains("name3"));
        assert!(!scrolled.contains("name1"));
        // Column window restricts to the requested slice.
        let windowed = format_ascii_table(&qr, 1..2, 0, 8);
        assert!(windowed.contains("NAME"));
        assert!(windowed.contains("name1"));
        assert!(!windowed.contains("NOTE"));
        assert!(!windowed.contains("note1"));
    }

    #[test]
    fn row_window_never_panics_on_ragged_rows() {
        let row = vec!["a".to_string(), "b".to_string()];
        assert_eq!(row_window(&row, &(0..5)), &["a".to_string(), "b".to_string()]);
        assert!(row_window(&row, &(5..9)).is_empty());
    }

    #[test]
    fn result_title_marks_overflow_directions() {
        let qr = sample_query_row();
        // Top-left, everything fits: no arrows.
        let plain = result_title_counts(0, 8, 8, 0, 3, 3, false);
        assert!(!plain.contains('▲') && !plain.contains('▼'));
        assert!(!plain.contains('◀') && !plain.contains('▶'));
        // Scrolled down with columns to the right: both arrows.
        let scrolled = result_title_counts(3, 3, 8, 1, 3, 5, false);
        assert!(scrolled.contains('▲') && scrolled.contains('▼'));
        assert!(scrolled.contains('◀') && scrolled.contains('▶'));
        // Truncated (more pages server-side) always hints ▼.
        let more = result_title_counts(0, 8, 8, 0, 3, 3, true);
        assert!(more.contains('▼'));
        let _ = qr;
    }

    #[test]
    fn selection_title_marks_header_selection() {
        let hdr_only = Some(TableSelection {
            anchor_row: None,
            anchor_col: 0,
            cursor_row: None,
            cursor_col: 2,
        });
        assert_eq!(
            selection_title_note(hdr_only, 8, 5),
            " [sel hdr c1-3]"
        );
        let hdr_plus_rows = Some(TableSelection {
            anchor_row: None,
            anchor_col: 1,
            cursor_row: Some(4),
            cursor_col: 3,
        });
        assert_eq!(
            selection_title_note(hdr_plus_rows, 8, 5),
            " [sel hdr-5 c2-4]"
        );
    }

    fn sample_layout() -> ResultsLayout {
        ResultsLayout {
            top_row: 0,
            start_col: 0,
            end_col: 3,
            col_starts: vec![1],
            inner_width: 78,
            visible_rows: 8,
            header_y: 1,
            first_data_y: 2,
        }
    }

    #[test]
    fn uniform_col_at_maps_table_cells() {
        // Table widget geometry: first cell at rel 1, +width+1 each.
        let layout = ResultsLayout {
            col_starts: vec![1, 9, 20],
            ..sample_layout()
        };
        let area = Rect::new(0, 5, 80, 24);
        assert_eq!(uniform_col_at(&layout, area, 0, 3), Some(0));
        assert_eq!(uniform_col_at(&layout, area, 2, 3), Some(0));
        assert_eq!(uniform_col_at(&layout, area, 10, 3), Some(1));
        assert_eq!(uniform_col_at(&layout, area, 79, 3), Some(2));
    }

    #[test]
    fn markdown_col_at_uses_per_row_widths() {
        // Markdown cells are unpadded: "| ID | NAME |" then data widths.
        let qr = sample_query_row();
        let area = Rect::new(0, 5, 80, 24);
        let layout = sample_layout();
        // Header line: "| ID(2) | NAME(4) |" → NAME starts at rel 3+2+3=8.
        assert_eq!(
            markdown_col_at(&qr, &layout, area, None, 4),
            Some(0)
        );
        assert_eq!(
            markdown_col_at(&qr, &layout, area, None, 9),
            Some(1)
        );
        // Data row 0: "| 1 | name1 |" → name1 starts at rel 3+1+3=7.
        assert_eq!(
            markdown_col_at(&qr, &layout, area, Some(0), 4),
            Some(0)
        );
        assert_eq!(
            markdown_col_at(&qr, &layout, area, Some(0), 8),
            Some(1)
        );
        // Missing row → None, ragged-safe.
        assert_eq!(markdown_col_at(&qr, &layout, area, Some(99), 8), None);
    }

    #[test]
    fn markdown_view_renders_scrolled_rows() {
        let qr = sample_query_row();
        let mut sm = SessionManager::new();
        let session = sm.sessions.remove(0);
        let view = TableViewState {
            scroll_offset: 5,
            col_scroll_offset: 0,
            result_format: ResultFormat::Markdown,
            focused: true,
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| {
                render_table(&session, &Some(&qr), view, None, f, f.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
        }
        // Scrolled window shows name6.., not the first rows; ▲ marks it.
        assert!(text.contains("name6"));
        assert!(!text.contains("name1"));
        assert!(text.contains('▲'));
    }

    #[test]
    fn markdown_lines_match_plain_table_without_selection() {
        let cols = vec!["A".to_string(), "B|C".to_string()];
        let r1 = vec!["x".to_string(), "y".to_string()];
        let rows = vec![r1.as_slice()];
        let plain = markdown_table(&cols, &rows);
        let lines = markdown_table_lines(&cols, &rows, 0, 0, None, None);
        let text: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        assert_eq!(text, plain);
        // No selection → nothing highlighted.
        assert!(lines.iter().flat_map(|l| l.spans.iter()).all(|s| {
            s.style.bg != Some(Color::Cyan)
        }));
    }

    #[test]
    fn markdown_lines_highlight_search_matches() {
        let cols = vec!["ID".to_string(), "NAME".to_string()];
        let r1 = vec!["1".to_string(), "ann".to_string()];
        let r2 = vec!["2".to_string(), "bob".to_string()];
        let rows = vec![r1.as_slice(), r2.as_slice()];
        // Case-insensitive "a" hits the NAME header and "ann".
        let hl = TableSearchHighlight {
            query: "a".to_string(),
            case_sensitive: false,
            current_row: None,
            current_col: 1,
            has_current: true,
        };
        let lines = markdown_table_lines(&cols, &rows, 0, 0, None, Some(&hl));
        let green: Vec<&str> = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.style.bg == Some(Color::Green))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(green, vec!["NAME"]);
        let yellow: Vec<&str> = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.style.bg == Some(Color::Yellow))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(yellow, vec!["ann"]);
        // Text itself is unchanged (byte-identical to the plain table).
        let text: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        assert_eq!(text, markdown_table(&cols, &rows));
    }

    #[test]
    fn markdown_lines_highlight_only_selected_cells() {
        let cols = vec!["ID".to_string(), "NAME".to_string()];
        let r1 = vec!["1".to_string(), "ann".to_string()];
        let r2 = vec!["2".to_string(), "bob".to_string()];
        let rows = vec![r1.as_slice(), r2.as_slice()];
        let sel = Some(TableSelection {
            anchor_row: Some(1),
            anchor_col: 0,
            cursor_row: Some(1),
            cursor_col: 1,
        });
        let rect = selected_cell_rect(sel, 2, 2);
        let lines = markdown_table_lines(&cols, &rows, 0, 0, rect, None);
        // Header line + separator + 2 rows.
        assert_eq!(lines.len(), 4);
        let cyan: Vec<&str> = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.style.bg == Some(Color::Cyan))
            .map(|s| s.content.as_ref())
            .collect();
        // Exactly the selected cells light up.
        assert_eq!(cyan, vec!["2", "bob"]);
    }

    #[test]
    fn ascii_segment_tint_reassembles_identically() {
        use AsciiCellTint as T;
        let mut line = Line::from("| OWNER   | ACTIVE |".to_string());
        let original: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        tint_ascii_cells(&mut line, &[T::Plain, T::Selected]);
        let rebuilt: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rebuilt, original);
        // Only the second cell block is tinted.
        let tinted: Vec<&str> = line
            .spans
            .iter()
            .filter(|s| s.style.bg == Some(Color::Cyan))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(tinted, vec![" ACTIVE |"]);
    }

    #[test]
    fn table_view_marks_active_cell_selection() {
        let qr = sample_query_row();
        let mut sm = SessionManager::new();
        let mut session = sm.sessions.remove(0);
        session.results.push(QueryRow {
            columns: qr.columns.clone(),
            rows: qr.rows.clone(),
            truncated: false,
            elapsed_ms: 7,
            is_error: false,
            error_msg: None,
            rows_affected: None,
            page_offset: 0,
            page_size: 100,
            byte_count: 0,
            total_fetched: 8,
        });
        session.table_selection = Some(TableSelection {
            anchor_row: Some(1),
            anchor_col: 0,
            cursor_row: Some(2),
            cursor_col: 1,
        });
        // `session` is moved below; build the view first (Copy state).
        let view = TableViewState {
            scroll_offset: 0,
            col_scroll_offset: 0,
            result_format: ResultFormat::Table,
            focused: true,
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|f| {
                render_table(&session, &session.results.last(), view, None, f, f.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
        }
        // Selection range is advertised in the title; selected rows render.
        assert!(text.contains("[sel r2-3 c1-2]"));
        assert!(text.contains("name2"));
    }
}
