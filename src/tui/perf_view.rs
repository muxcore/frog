//! Toad-style `F4` Object DDL viewer and `btop`-inspired `F6` Performance
//! & DBA Dashboard with lightweight terminal graphics.

use crate::db::perf::{
    classify_perf_cell, column_maxima, perf_queries_for, smooth_bar, CellBarKind, PerfSnapshot,
};
use crate::db::session_manager::{Session, SessionManager};
use crate::tui::app::ResultFormat;
use crate::tui::widgets::{
    clean_cell, format_bytes, overlay_line_search, render_result_scrollbars, render_table,
    result_title_counts, visible_col_range_fit, wrap_sql_lines, LineSearchHighlight, TableViewState,
};
use ratatui::{prelude::*, widgets::*};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Which pane has focus inside the `F6` Performance dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PerfPane {
    #[default]
    Queries,
    Results,
    Sql,
}

impl PerfPane {
    pub fn next(self, show_sql: bool) -> Self {
        match (self, show_sql) {
            (PerfPane::Queries, _) => PerfPane::Results,
            (PerfPane::Results, true) => PerfPane::Sql,
            (PerfPane::Results, false) => PerfPane::Queries,
            (PerfPane::Sql, _) => PerfPane::Queries,
        }
    }

    pub fn prev(self, show_sql: bool) -> Self {
        match (self, show_sql) {
            (PerfPane::Queries, true) => PerfPane::Sql,
            (PerfPane::Queries, false) => PerfPane::Results,
            (PerfPane::Results, _) => PerfPane::Queries,
            (PerfPane::Sql, _) => PerfPane::Results,
        }
    }
}

/// Layout parameters for the `F6` Performance view.
#[derive(Debug, Clone, Copy)]
pub struct PerfLayout {
    pub split_pct: u16,
    pub focus: PerfPane,
    pub maximized: bool,
}

fn pane_border(focused: bool) -> Color {
    if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    }
}

// ============================================================================
// F4: Toad-style Object Describe / DDL Viewer
// ============================================================================

/// Render the fullscreen `F4` Object DDL viewer.
///
/// Honors `show_line_numbers` (`Mouse:OFF` native selection mode): when false,
/// borders and gutter line numbers are hidden so terminal drag-selection copies
/// clean executable DDL. `search` tints all I-search hits (current match gets
/// the cursor tint), mirroring the editor/results highlighting.
pub fn render_object_ddl(
    session: &Session,
    show_line_numbers: bool,
    search: Option<&LineSearchHighlight>,
    f: &mut Frame,
    area: Rect,
) {
    let target_label = session
        .ddl_target
        .as_ref()
        .map(|t| t.display_label())
        .unwrap_or_else(|| "Object".into());

    if session.ddl_loading {
        let block = Block::default()
            .title(format!(
                " F4 Object DDL — ⟳ Resolving {}… (Esc/F4: cancel) ",
                target_label
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow));
        let p = Paragraph::new(format!(
            "\n  ⟳ Inspecting catalog & generating DDL script for '{}'…",
            target_label
        ))
        .style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .block(block);
        f.render_widget(p, area);
        return;
    }

    if let Some(ref err) = session.ddl_error {
        let block = Block::default()
            .title(format!(
                " F4 Object DDL — {} (r: retry · F4/Esc/q: back) ",
                target_label
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red));
        let p = Paragraph::new(format!("DDL lookup failed:\n\n{}", err))
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
            .block(block);
        f.render_widget(p, area);
        return;
    }

    let Some(ref obj) = session.ddl_object else {
        let block = Block::default()
            .title(" F4 Object DDL (F4/Esc/q: back) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        let p = Paragraph::new("No object selected. Put the cursor on an object name and press F4.")
            .style(Style::default().fg(Color::DarkGray))
            .block(block);
        f.render_widget(p, area);
        return;
    };

    // Mouse:OFF native-copy mode: render pure DDL lines with no chrome/gutter.
    if !show_line_numbers {
        let raw_lines: Vec<&str> = obj.ddl.lines().collect();
        let view_h = area.height as usize;
        let skip = session.ddl_scroll.min(raw_lines.len().saturating_sub(1));
        let lines: Vec<Line> = raw_lines
            .into_iter()
            .enumerate()
            .skip(skip)
            .take(view_h.max(1))
            .map(|(idx, l)| {
                let (visible, skipped) = hslice_with_offset(l, session.ddl_hscroll);
                let line = highlight_ddl_line(visible.clone());
                apply_ddl_search(line, &visible, idx, skipped, search)
            })
            .collect();
        f.render_widget(Paragraph::new(lines), area);
        return;
    }

    // Split a 3-row Toad-style object metadata banner above the DDL script when
    // the terminal has room.
    let (ddl_area, show_banner) = if area.height >= 10 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(5)])
            .split(area);

        let badge_color = kind_badge_color(&obj.kind);
        let mut banner_spans = vec![
            Span::styled(
                format!(" {} ", obj.kind),
                Style::default()
                    .bg(badge_color)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                format!("{}.{}", obj.schema, obj.name),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  ({}ms)", obj.elapsed_ms),
                Style::default().fg(Color::DarkGray),
            ),
        ];
        if let Some(ref sum) = obj.summary {
            banner_spans.push(Span::styled(
                format!("   │   {}", sum),
                Style::default().fg(Color::Cyan),
            ));
        }
        let banner = Paragraph::new(Line::from(banner_spans)).block(
            Block::default()
                .title(" F4 Object Describe (Toad-style) ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        );
        f.render_widget(banner, chunks[0]);
        (chunks[1], true)
    } else {
        (area, false)
    };

    let raw_lines: Vec<&str> = obj.ddl.lines().collect();
    let total_lines = raw_lines.len().max(1);
    let max_line_w = raw_lines
        .iter()
        .map(|l| UnicodeWidthStr::width(*l))
        .max()
        .unwrap_or(0);

    let title = if show_banner {
        format!(
            " DDL Script ({} lines) — Ctrl+C/Y: copy · Enter/i: →editor · n: →new tab · r: reload · ↑↓←→/PgUp/PgDn: scroll · F4/Esc: back ",
            total_lines
        )
    } else {
        format!(
            " [{}] {}.{} ({} lines) — Ctrl+C/Y: copy · Enter/i: →editor · n: new tab · F4/Esc: back ",
            obj.kind, obj.schema, obj.name, total_lines
        )
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(ddl_area);
    f.render_widget(block, ddl_area);

    let view_h = inner.height as usize;
    let max_scroll = total_lines.saturating_sub(view_h.max(1));
    let skip = session.ddl_scroll.min(max_scroll);

    let mut rendered: Vec<Line> = Vec::with_capacity(view_h.max(1));
    for (idx, raw) in raw_lines
        .iter()
        .enumerate()
        .skip(skip)
        .take(view_h.max(1))
    {
        let gutter = Span::styled(
            format!("{:4} │ ", idx + 1),
            Style::default().fg(Color::DarkGray),
        );
        let (visible, skipped) = hslice_with_offset(raw, session.ddl_hscroll);
        let mut line = apply_ddl_search(
            highlight_ddl_line(visible.clone()),
            &visible,
            idx,
            skipped,
            search,
        );
        line.spans.insert(0, gutter);
        rendered.push(line);
    }

    f.render_widget(Paragraph::new(rendered), inner);

    if total_lines > view_h.max(1) && ddl_area.height >= 5 {
        let mut vstate = ScrollbarState::new(max_scroll.max(1)).position(skip);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(Color::Cyan)),
            ddl_area,
            &mut vstate,
        );
    }
    let inner_code_w = inner.width.saturating_sub(7) as usize;
    if max_line_w > inner_code_w && ddl_area.height >= 3 {
        let max_h = max_line_w.saturating_sub(inner_code_w).max(1);
        let mut hstate = ScrollbarState::new(max_h).position(session.ddl_hscroll.min(max_h));
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::HorizontalBottom)
                .thumb_style(Style::default().fg(Color::Cyan)),
            ddl_area,
            &mut hstate,
        );
    }
}

fn kind_badge_color(kind: &str) -> Color {
    match kind.to_ascii_uppercase().as_str() {
        "TABLE" | "PARTITIONED TABLE" | "FOREIGN TABLE" => Color::Cyan,
        "INDEX" => Color::Yellow,
        "VIEW" | "MVIEW" | "MATERIALIZED VIEW" => Color::Green,
        "PACKAGE" | "PACKAGE BODY" | "PROCEDURE" | "FUNCTION" => Color::Magenta,
        "SEQUENCE" => Color::Blue,
        "TRIGGER" => Color::LightRed,
        _ => Color::White,
    }
}

/// Horizontal slice plus the byte offset skipped from the original line,
/// so absolute search columns can be mapped into the visible slice.
fn hslice_with_offset(s: &str, skip_cols: usize) -> (String, usize) {
    if skip_cols == 0 {
        return (s.to_string(), 0);
    }
    let mut skipped_cols = 0usize;
    let mut skipped_bytes = 0usize;
    let mut out = String::new();
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(1);
        if skipped_cols < skip_cols {
            skipped_cols += w;
            skipped_bytes += ch.len_utf8();
            continue;
        }
        out.push(ch);
    }
    (out, skipped_bytes)
}

/// Tint I-search hits inside an already syntax-highlighted DDL line.
/// Every occurrence of the query gets the match tint; the occurrence on
/// the current match row at the (hscroll-adjusted) column gets the
/// cursor tint.
fn apply_ddl_search(
    line: Line<'static>,
    visible: &str,
    abs_line: usize,
    skipped_bytes: usize,
    search: Option<&LineSearchHighlight>,
) -> Line<'static> {
    let Some(hl) = search else {
        return line;
    };
    if hl.query.is_empty() {
        return line;
    }
    let current_col = if hl.has_current && hl.current_line == abs_line {
        hl.current_col.checked_sub(skipped_bytes)
    } else {
        None
    };
    overlay_line_search(line, visible, &hl.query, hl.case_sensitive, current_col)
}

/// Lightweight single-line SQL syntax highlighter for DDL scripts.
fn highlight_ddl_line(line: String) -> Line<'static> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("--") {
        return Line::from(Span::styled(
            line,
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::ITALIC),
        ));
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    let mut i = 0usize;

    while i < len {
        let c = chars[i];
        // Inline `--` comment.
        if c == '-' && i + 1 < len && chars[i + 1] == '-' {
            let rest: String = chars[i..].iter().collect();
            spans.push(Span::styled(
                rest,
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::ITALIC),
            ));
            break;
        }
        // Single-quoted string literal.
        if c == '\'' {
            let start = i;
            i += 1;
            while i < len {
                if chars[i] == '\'' {
                    if i + 1 < len && chars[i + 1] == '\'' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            let lit: String = chars[start..i].iter().collect();
            spans.push(Span::styled(lit, Style::default().fg(Color::Yellow)));
            continue;
        }
        // Double-quoted identifier.
        if c == '"' {
            let start = i;
            i += 1;
            while i < len {
                if chars[i] == '"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            let ident: String = chars[start..i].iter().collect();
            spans.push(Span::styled(ident, Style::default().fg(Color::White)));
            continue;
        }
        // Word / keyword / type.
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            i += 1;
            while i < len
                && (chars[i].is_ascii_alphanumeric() || matches!(chars[i], '_' | '$' | '#'))
            {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let upper = word.to_ascii_uppercase();
            let style = if is_ddl_keyword(&upper) {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if is_ddl_type(&upper) {
                Style::default().fg(Color::LightBlue)
            } else {
                Style::default().fg(Color::White)
            };
            spans.push(Span::styled(word, style));
            continue;
        }
        // Numbers.
        if c.is_ascii_digit() {
            let start = i;
            i += 1;
            while i < len && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            let num: String = chars[start..i].iter().collect();
            spans.push(Span::styled(num, Style::default().fg(Color::Magenta)));
            continue;
        }
        // Whitespace / punctuation run.
        let start = i;
        i += 1;
        while i < len
            && !chars[i].is_ascii_alphabetic()
            && !chars[i].is_ascii_digit()
            && !matches!(chars[i], '_' | '\'' | '"' | '-')
        {
            i += 1;
        }
        let plain: String = chars[start..i].iter().collect();
        spans.push(Span::styled(plain, Style::default().fg(Color::Gray)));
    }

    if spans.is_empty() {
        Line::from("")
    } else {
        Line::from(spans)
    }
}

fn is_ddl_keyword(upper: &str) -> bool {
    matches!(
        upper,
        "CREATE"
            | "OR"
            | "REPLACE"
            | "FORCE"
            | "TABLE"
            | "FOREIGN"
            | "VIEW"
            | "MATERIALIZED"
            | "INDEX"
            | "UNIQUE"
            | "BITMAP"
            | "SEQUENCE"
            | "SYNONYM"
            | "PUBLIC"
            | "PACKAGE"
            | "BODY"
            | "PROCEDURE"
            | "FUNCTION"
            | "TRIGGER"
            | "TYPE"
            | "ENUM"
            | "AS"
            | "IS"
            | "ON"
            | "FOR"
            | "CONSTRAINT"
            | "PRIMARY"
            | "KEY"
            | "REFERENCES"
            | "CHECK"
            | "DEFAULT"
            | "NOT"
            | "NULL"
            | "GENERATED"
            | "ALWAYS"
            | "BY"
            | "IDENTITY"
            | "PARTITION"
            | "TABLESPACE"
            | "INCREMENT"
            | "MINVALUE"
            | "MAXVALUE"
            | "START"
            | "WITH"
            | "CACHE"
            | "CYCLE"
            | "NOCYCLE"
            | "NO"
            | "BUILD"
            | "IMMEDIATE"
            | "REFRESH"
            | "DEMAND"
            | "COMMENT"
            | "COLUMN"
            | "DELETE"
            | "CASCADE"
            | "SET"
            | "BEGIN"
            | "END"
            | "DECLARE"
            | "RETURN"
            | "RETURNS"
            | "LANGUAGE"
            | "SELECT"
            | "FROM"
            | "WHERE"
            | "JOIN"
            | "LEFT"
            | "RIGHT"
            | "INNER"
            | "GROUP"
            | "ORDER"
            | "ASC"
            | "DESC"
    )
}

fn is_ddl_type(upper: &str) -> bool {
    matches!(
        upper,
        "NUMBER"
            | "INTEGER"
            | "INT"
            | "BIGINT"
            | "SMALLINT"
            | "NUMERIC"
            | "DECIMAL"
            | "FLOAT"
            | "DOUBLE"
            | "REAL"
            | "VARCHAR"
            | "VARCHAR2"
            | "NVARCHAR2"
            | "CHAR"
            | "NCHAR"
            | "CHARACTER"
            | "VARYING"
            | "TEXT"
            | "CLOB"
            | "BLOB"
            | "RAW"
            | "BYTEA"
            | "DATE"
            | "TIMESTAMP"
            | "TIMESTAMPTZ"
            | "INTERVAL"
            | "BOOLEAN"
            | "BOOL"
            | "JSON"
            | "JSONB"
            | "UUID"
            | "XMLTYPE"
    )
}

// ============================================================================
// F6: Performance & DBA Dashboard (`btop` + Toad style)
// ============================================================================

/// Render the `F6` Performance & DBA Dashboard and return `(queries_area,
/// results_area, sql_area)` for mouse hit-testing.
pub fn render_performance(
    sm: &SessionManager,
    result_format: ResultFormat,
    layout: PerfLayout,
    f: &mut Frame,
    area: Rect,
) -> (Rect, Rect, Rect) {
    if area.height < 6 || area.width < 28 {
        let p = Paragraph::new("Window too small for Performance view — enlarge or press Esc.")
            .style(Style::default().fg(Color::Yellow))
            .block(
                Block::default()
                    .title(" F6 Performance ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            );
        f.render_widget(p, area);
        return (Rect::default(), Rect::default(), Rect::default());
    }

    let db_type = sm.active_db_type();
    let sess = sm.active_session();
    let specs = perf_queries_for(db_type);
    let cursor = sess.perf_cursor.min(specs.len().saturating_sub(1));

    // Maximized pane fills the whole view.
    if layout.maximized {
        match layout.focus {
            PerfPane::Queries => {
                render_perf_query_list(sess, specs, cursor, true, true, f, area);
                return (area, Rect::default(), Rect::default());
            }
            PerfPane::Results => {
                render_perf_results_pane(sess, specs, cursor, result_format, true, true, f, area);
                return (Rect::default(), area, Rect::default());
            }
            PerfPane::Sql => {
                render_perf_sql_pane(sess, specs, cursor, true, true, f, area);
                return (Rect::default(), Rect::default(), area);
            }
        }
    }

    // Top `btop`-style graphical telemetry strip when screen is tall enough.
    let body_area = if area.height >= 16 {
        let vchunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(5), Constraint::Min(10)])
            .split(area);
        render_telemetry_strip(sess, db_type, f, vchunks[0]);
        vchunks[1]
    } else {
        area
    };

    let split_pct = layout.split_pct.clamp(22, 65);
    let (list_area, right_area) = if body_area.width >= 90 {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(split_pct),
                Constraint::Percentage(100 - split_pct),
            ])
            .split(body_area);
        (cols[0], cols[1])
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(8), Constraint::Min(6)])
            .split(body_area);
        (rows[0], rows[1])
    };

    render_perf_query_list(
        sess,
        specs,
        cursor,
        layout.focus == PerfPane::Queries,
        false,
        f,
        list_area,
    );

    let (results_area, sql_area) = if sess.perf_show_sql && right_area.height >= 12 {
        let rsplit = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(6), Constraint::Length(7)])
            .split(right_area);
        render_perf_sql_pane(
            sess,
            specs,
            cursor,
            layout.focus == PerfPane::Sql,
            false,
            f,
            rsplit[1],
        );
        (rsplit[0], rsplit[1])
    } else {
        (right_area, Rect::default())
    };

    render_perf_results_pane(
        sess,
        specs,
        cursor,
        result_format,
        layout.focus == PerfPane::Results,
        false,
        f,
        results_area,
    );

    (list_area, results_area, sql_area)
}

/// Top 4-card `btop`-style telemetry strip: Sessions/Sparkline, Locks, Wait
/// Profile stacked bar, and Memory/Cache Hit gauge.
fn render_telemetry_strip(
    sess: &Session,
    db_type: crate::db::DbType,
    f: &mut Frame,
    area: Rect,
) {
    let cards = if area.width >= 110 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(25),
                Constraint::Percentage(24),
                Constraint::Percentage(26),
                Constraint::Percentage(25),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area)
    };

    let snap = sess.perf_snapshot.clone().unwrap_or_default();
    let has_snap = sess.perf_snapshot.is_some();

    // --- Card 1: Sessions & Activity Sparkline ---
    render_sessions_card(sess, &snap, has_snap, f, cards[0]);

    // --- Card 2: Locks & Contention ---
    render_locks_card(sess, &snap, has_snap, db_type, f, cards[1]);

    if cards.len() >= 4 {
        // --- Card 3: Active Wait Profile Stacked Bar ---
        render_waits_card(&snap, has_snap, f, cards[2]);

        // --- Card 4: Memory & Cache Efficiency ---
        render_memory_card(&snap, has_snap, sess.perf_snapshot_error.as_deref(), f, cards[3]);
    }
}

fn render_sessions_card(
    sess: &Session,
    snap: &PerfSnapshot,
    has_snap: bool,
    f: &mut Frame,
    area: Rect,
) {
    let live_tag = if sess.perf_loading {
        "⟳"
    } else if sess.perf_auto_refresh {
        "● LIVE 5s"
    } else {
        "○ manual"
    };
    let block = Block::default()
        .title(format!(" Sessions ({}) ", live_tag))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width < 8 {
        return;
    }

    if !has_snap {
        let msg = if !sess.is_connected {
            "Not connected (Ctrl+O)"
        } else if let Some(ref e) = sess.perf_snapshot_error {
            e.lines().next().unwrap_or("Snapshot unavailable")
        } else {
            "⟳ Sampling sessions…"
        };
        f.render_widget(
            Paragraph::new(msg).style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let ratio = if snap.total_sessions == 0 {
        0.0
    } else {
        (snap.active_sessions as f64 / snap.total_sessions as f64).clamp(0.0, 1.0)
    };
    let bar_color = if ratio >= 0.75 {
        Color::Red
    } else if ratio >= 0.40 {
        Color::Yellow
    } else {
        Color::Green
    };
    let bar = smooth_bar(ratio, 8);
    let top_line = Line::from(vec![
        Span::styled(
            format!("Act {}/{} ", snap.active_sessions, snap.total_sessions),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(bar, Style::default().fg(bar_color)),
        Span::styled(
            format!(" max {}s", snap.max_active_secs),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    f.render_widget(Paragraph::new(top_line), rows[0]);

    if rows[1].height > 0 {
        let data = sess.perf_history.active_slice();
        let max_v = data.iter().copied().max().unwrap_or(1).max(4);
        let spark = Sparkline::default()
            .data(&data)
            .max(max_v)
            .style(Style::default().fg(Color::Cyan));
        f.render_widget(spark, rows[1]);
    }
}

fn render_locks_card(
    sess: &Session,
    snap: &PerfSnapshot,
    has_snap: bool,
    db_type: crate::db::DbType,
    f: &mut Frame,
    area: Rect,
) {
    let border = if has_snap && snap.blocked_sessions > 0 {
        Color::Red
    } else {
        Color::DarkGray
    };
    let block = Block::default()
        .title(" Locks & Contention ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width < 8 {
        return;
    }

    if !has_snap {
        f.render_widget(
            Paragraph::new("—").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let lock_span = if snap.blocked_sessions > 0 {
        Span::styled(
            format!(
                "⚠ {} BLOCKED ({} blk)",
                snap.blocked_sessions,
                snap.blocking_sessions.max(1)
            ),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            "✔ 0 Blocked Locks",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    };

    let mut top_spans = vec![lock_span];
    if db_type == crate::db::DbType::Postgres {
        let idle_color = if snap.idle_in_tx_sessions > 0 {
            Color::Yellow
        } else {
            Color::DarkGray
        };
        top_spans.push(Span::styled(
            format!(" · IdleTx:{}", snap.idle_in_tx_sessions),
            Style::default().fg(idle_color),
        ));
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    f.render_widget(Paragraph::new(Line::from(top_spans)), rows[0]);

    if rows[1].height > 0 {
        let data = sess.perf_history.blocked_slice();
        let any_blocked = data.iter().any(|&x| x > 0);
        if any_blocked {
            let max_v = data.iter().copied().max().unwrap_or(1).max(2);
            let spark = Sparkline::default()
                .data(&data)
                .max(max_v)
                .style(Style::default().fg(Color::Red));
            f.render_widget(spark, rows[1]);
        } else {
            f.render_widget(
                Paragraph::new("No lock contention history")
                    .style(Style::default().fg(Color::DarkGray)),
                rows[1],
            );
        }
    }
}

fn render_waits_card(snap: &PerfSnapshot, has_snap: bool, f: &mut Frame, area: Rect) {
    let block = Block::default()
        .title(" CPU & Wait Profile ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width < 8 {
        return;
    }

    let total_active: u64 = snap.wait_breakdown.iter().map(|(_, c)| *c).sum();
    if !has_snap || total_active == 0 {
        let lines = vec![
            Line::from(Span::styled(
                "░".repeat(inner.width.min(24) as usize),
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                if has_snap {
                    "Idle — 0 active sessions"
                } else {
                    "—"
                },
                Style::default().fg(Color::DarkGray),
            )),
        ];
        f.render_widget(Paragraph::new(lines), inner);
        return;
    }

    // Proportional multi-color stacked bar across the card width.
    let bar_w = inner.width.saturating_sub(1).clamp(6, 36) as usize;
    let mut bar_spans: Vec<Span> = Vec::new();
    let mut used = 0usize;
    for (i, (cls, cnt)) in snap.wait_breakdown.iter().enumerate() {
        let seg = if i + 1 == snap.wait_breakdown.len() {
            bar_w.saturating_sub(used)
        } else {
            (((cnt * bar_w as u64) as f64 / total_active as f64).round() as usize)
                .max(1)
                .min(bar_w.saturating_sub(used))
        };
        if seg == 0 {
            continue;
        }
        used += seg;
        let (ch, color) = wait_class_visual(cls);
        bar_spans.push(Span::styled(
            ch.to_string().repeat(seg),
            Style::default().fg(color),
        ));
    }
    if used < bar_w {
        bar_spans.push(Span::styled(
            "░".repeat(bar_w - used),
            Style::default().fg(Color::DarkGray),
        ));
    }

    let mut legend: Vec<Span> = Vec::new();
    for (i, (cls, cnt)) in snap.wait_breakdown.iter().take(3).enumerate() {
        if i > 0 {
            legend.push(Span::raw("  "));
        }
        let (_, color) = wait_class_visual(cls);
        let short = short_wait_label(cls);
        legend.push(Span::styled("■", Style::default().fg(color)));
        legend.push(Span::styled(
            format!("{}:{}", short, cnt),
            Style::default().fg(Color::White),
        ));
    }

    let lines = vec![Line::from(bar_spans), Line::from(legend)];
    f.render_widget(Paragraph::new(lines), inner);
}

fn wait_class_visual(cls: &str) -> (char, Color) {
    let up = cls.to_ascii_uppercase();
    if up == "CPU" {
        ('█', Color::Green)
    } else if up.contains("I/O") || up == "IO" {
        ('▓', Color::Blue)
    } else if up.contains("APPLICATION") || up.contains("LOCK") {
        ('█', Color::Red)
    } else if up.contains("CONCURRENCY") || up.contains("BUFFER") {
        ('▒', Color::Magenta)
    } else if up.contains("COMMIT") {
        ('▓', Color::Yellow)
    } else {
        ('▒', Color::Cyan)
    }
}

fn short_wait_label(cls: &str) -> &str {
    match cls {
        "User I/O" => "I/O",
        "System I/O" => "SysIO",
        "Application" => "Lock",
        "Concurrency" => "Conc",
        "Configuration" => "Cfg",
        "Administrative" => "Admin",
        other => other,
    }
}

fn render_memory_card(
    snap: &PerfSnapshot,
    has_snap: bool,
    snap_err: Option<&str>,
    f: &mut Frame,
    area: Rect,
) {
    let block = Block::default()
        .title(" Memory & Buffer Cache ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width < 8 {
        return;
    }

    if !has_snap {
        let msg = snap_err
            .and_then(|e| e.lines().next())
            .unwrap_or("—");
        f.render_widget(
            Paragraph::new(msg).style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let mem_line = if let (Some(used), Some(total)) = (snap.mem_used_mb, snap.mem_total_mb) {
        let r = if total > 0.0 {
            (used / total).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let col = if r >= 0.88 {
            Color::Red
        } else if r >= 0.70 {
            Color::Yellow
        } else {
            Color::Magenta
        };
        Line::from(vec![
            Span::styled(smooth_bar(r, 6), Style::default().fg(col)),
            Span::raw(" "),
            Span::styled(snap.mem_detail.clone(), Style::default().fg(Color::White)),
        ])
    } else {
        Line::from(Span::styled(
            if snap.mem_detail.is_empty() {
                "Memory stats unavailable".to_string()
            } else {
                snap.mem_detail.clone()
            },
            Style::default().fg(Color::White),
        ))
    };

    let hit_line = match snap.cache_hit_pct {
        Some(pct) => {
            let p = pct.clamp(0.0, 100.0);
            let col = if p >= 95.0 {
                Color::Green
            } else if p >= 85.0 {
                Color::Yellow
            } else {
                Color::Red
            };
            Line::from(vec![
                Span::styled("Hit ", Style::default().fg(Color::DarkGray)),
                Span::styled(smooth_bar(p / 100.0, 8), Style::default().fg(col)),
                Span::styled(
                    format!(" {:>5.1}%", p),
                    Style::default().fg(col).add_modifier(Modifier::BOLD),
                ),
            ])
        }
        None => Line::from(Span::styled(
            "Cache Hit: n/a",
            Style::default().fg(Color::DarkGray),
        )),
    };

    f.render_widget(Paragraph::new(vec![mem_line, hit_line]), inner);
}

#[allow(clippy::too_many_arguments)]
fn render_perf_query_list(
    sess: &Session,
    specs: &[crate::db::perf::PerfQuerySpec],
    cursor: usize,
    focused: bool,
    maximized: bool,
    f: &mut Frame,
    area: Rect,
) {
    let title = format!(
        " {}DBA Queries (1-0/↑↓ pick · Enter run · s→editor){} ",
        if focused { "▸ " } else { "" },
        if maximized { " [MAX]" } else { "" }
    );
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(pane_border(focused)));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width < 6 {
        return;
    }

    // Reserve 2 bottom lines for the selected query's description when tall enough.
    let (list_rect, desc_rect) = if inner.height >= 8 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(4), Constraint::Length(2)])
            .split(inner);
        (chunks[0], Some(chunks[1]))
    } else {
        (inner, None)
    };

    let avail = list_rect.height as usize;
    let start = if avail == 0 || cursor < avail {
        0
    } else {
        cursor + 1 - avail
    };

    let mut items: Vec<Line> = Vec::with_capacity(avail);
    for (idx, spec) in specs.iter().enumerate().skip(start).take(avail.max(1)) {
        let selected = idx == cursor;
        let key_num = if idx == 9 { 0 } else { idx + 1 };
        let has_cache = sess.perf_results.contains_key(&idx);
        let badge_col = perf_badge_color(spec.badge);

        let row_bg = if selected {
            Color::Rgb(55, 58, 95)
        } else {
            Color::Reset
        };
        let title_fg = if selected {
            Color::Yellow
        } else {
            Color::White
        };

        let spans = vec![
            Span::styled(
                if selected { "▶ " } else { "  " },
                Style::default().fg(Color::Cyan).bg(row_bg),
            ),
            Span::styled(
                format!("[{}] ", key_num),
                Style::default().fg(Color::DarkGray).bg(row_bg),
            ),
            Span::styled(
                format!("{:<4}", spec.badge),
                Style::default()
                    .fg(badge_col)
                    .bg(row_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", Style::default().bg(row_bg)),
            Span::styled(
                spec.title,
                Style::default()
                    .fg(title_fg)
                    .bg(row_bg)
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(
                if has_cache { " ●" } else { "" },
                Style::default().fg(Color::Green).bg(row_bg),
            ),
        ];
        items.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(items), list_rect);

    if let (Some(drect), Some(spec)) = (desc_rect, specs.get(cursor)) {
        let desc = Paragraph::new(format!("ⓘ {}", spec.description))
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(desc, drect);
    }
}

fn perf_badge_color(badge: &str) -> Color {
    match badge {
        "LOCK" => Color::LightRed,
        "CPU" => Color::Green,
        "SQL" => Color::Cyan,
        "MEM" | "PGA" => Color::Magenta,
        "WAIT" => Color::Yellow,
        "TBS" | "SIZE" | "I/O" => Color::LightBlue,
        _ => Color::White,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_perf_results_pane(
    sess: &Session,
    specs: &[crate::db::perf::PerfQuerySpec],
    cursor: usize,
    result_format: ResultFormat,
    focused: bool,
    maximized: bool,
    f: &mut Frame,
    area: Rect,
) {
    let query_title = specs.get(cursor).map(|s| s.title).unwrap_or("Results");
    let qr_opt = sess.perf_results.get(&cursor);

    let auto_note = if sess.perf_auto_refresh {
        "auto:5s(a)"
    } else {
        "auto:off(a)"
    };
    let sql_note = if sess.perf_show_sql {
        "v:hide SQL"
    } else {
        "v:show SQL"
    };

    if qr_opt.is_none() {
        let msg = if !sess.is_connected {
            "Not connected — press Ctrl+O to connect, then press F6."
        } else if sess.perf_loading {
            "⟳ Running DBA diagnostic query…"
        } else {
            "Press Enter or R to run this diagnostic query."
        };
        let block = Block::default()
            .title(format!(
                " {} ({} · R refresh · {} · s→editor · F6/Esc back) ",
                query_title, auto_note, sql_note
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(pane_border(focused)));
        let style = if sess.perf_loading {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        f.render_widget(Paragraph::new(msg).style(style).block(block), area);
        return;
    }

    let qr = qr_opt.unwrap();

    // Markdown / ASCII modes reuse the shared table renderer cleanly.
    if result_format != ResultFormat::Table {
        render_table(
            sess,
            &Some(qr),
            TableViewState {
                scroll_offset: sess.perf_scroll,
                col_scroll_offset: sess.perf_col_scroll,
                result_format,
                focused,
            },
            None,
            f,
            area,
        );
        return;
    }

    if qr.is_error {
        let block = Block::default()
            .title(format!(
                " {} — Error (R: retry · s: open SQL in editor · F6/Esc: back) ",
                query_title
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red));
        let p = Paragraph::new(qr.error_msg.as_deref().unwrap_or("Unknown error"))
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
            .block(block);
        f.render_widget(p, area);
        return;
    }

    if qr.columns.is_empty() || qr.rows.is_empty() {
        let empty_hint = if specs.get(cursor).map(|s| s.badge) == Some("LOCK") {
            "✔ Zero blocking or waiting sessions right now."
        } else {
            "No rows returned right now."
        };
        let block = Block::default()
            .title(format!(
                " {} | 0 rows | {}ms ({} · R refresh · {} · s→editor) ",
                query_title, qr.elapsed_ms, auto_note, sql_note
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(pane_border(focused)));
        f.render_widget(
            Paragraph::new(empty_hint)
                .style(Style::default().fg(Color::Green))
                .block(block),
            area,
        );
        return;
    }

    // Build graphical cell widths + bars for Table mode!
    let col_max = column_maxima(&qr.columns, &qr.rows);
    let ideal: Vec<usize> = qr
        .columns
        .iter()
        .enumerate()
        .map(|(ci, h)| {
            let mut w = UnicodeWidthStr::width(h.as_str()).max(6);
            for row in qr.rows.iter().take(60) {
                if let Some(raw) = row.get(ci) {
                    let rendered = match classify_perf_cell(h, raw, col_max[ci]) {
                        Some((bar_txt, _)) => bar_txt,
                        None => clean_cell(raw),
                    };
                    w = w.max(UnicodeWidthStr::width(rendered.as_str()));
                }
            }
            (w + 2).clamp(8, 48)
        })
        .collect();

    let total_cols = qr.columns.len();
    let total_rows = qr.rows.len();
    let avail_width = area.width.saturating_sub(4) as usize;
    let avail_height = area.height.saturating_sub(4) as usize;
    let col_off = sess.perf_col_scroll.min(total_cols.saturating_sub(1));
    let row_off = sess.perf_scroll.min(total_rows.saturating_sub(1));
    let range = visible_col_range_fit(total_cols, col_off, &ideal, avail_width.max(20));
    let start_col = range.start;
    let end_col = range.end;

    let header = Row::new(
        qr.columns[range.clone()]
            .iter()
            .map(|h| Cell::from(h.clone()))
            .collect::<Vec<_>>(),
    )
    .style(
        Style::default()
            .bg(Color::Rgb(40, 40, 80))
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )
    .height(1);

    let visible_rows: Vec<Row> = qr
        .rows
        .iter()
        .skip(row_off)
        .take(avail_height)
        .enumerate()
        .map(|(idx, row)| {
            let bg = if idx % 2 == 0 {
                Color::Rgb(18, 18, 22)
            } else {
                Color::Rgb(24, 24, 30)
            };
            let cells: Vec<Cell> = (start_col..end_col)
                .map(|ci| {
                    let raw = row.get(ci).map(String::as_str).unwrap_or("");
                    let col_name = &qr.columns[ci];
                    if let Some((bar_txt, kind)) = classify_perf_cell(col_name, raw, col_max[ci]) {
                        let fg = match kind {
                            CellBarKind::HotPct(p) if p >= 85.0 => Color::Red,
                            CellBarKind::HotPct(p) if p >= 60.0 => Color::Yellow,
                            CellBarKind::HotPct(_) => Color::Cyan,
                            CellBarKind::GoodPct(p) if p >= 90.0 => Color::Green,
                            CellBarKind::GoodPct(p) if p >= 70.0 => Color::Yellow,
                            CellBarKind::GoodPct(_) => Color::Red,
                            CellBarKind::Relative { ratio } if ratio >= 0.80 => Color::LightRed,
                            CellBarKind::Relative { ratio } if ratio >= 0.45 => Color::Yellow,
                            CellBarKind::Relative { .. } => Color::Cyan,
                        };
                        Cell::from(bar_txt).style(Style::default().fg(fg).bg(bg))
                    } else {
                        Cell::from(clean_cell(raw)).style(Style::default().fg(Color::White).bg(bg))
                    }
                })
                .collect();
            Row::new(cells).style(Style::default().bg(bg))
        })
        .collect();

    let shown_rows = visible_rows.len();
    let counts = result_title_counts(
        row_off,
        shown_rows,
        total_rows,
        start_col,
        end_col,
        total_cols,
        qr.truncated,
    );
    let spin = if sess.perf_loading { "⟳ " } else { "" };
    let max_tag = if maximized { " [MAX]" } else { "" };
    let title = format!(
        " {}{}{}{} | {} | {} | {}ms · {} · {} · s→editor · n→tab ",
        if focused { "▸ " } else { "" },
        spin,
        query_title,
        max_tag,
        counts,
        format_bytes(qr.byte_count),
        qr.elapsed_ms,
        auto_note,
        sql_note,
    );

    let mut col_widths: Vec<usize> = ideal[start_col..end_col].to_vec();
    let used: usize = col_widths.iter().sum::<usize>() + col_widths.len();
    if avail_width > used && !col_widths.is_empty() {
        let mut extra = avail_width - used;
        while extra > 0 {
            let mut grown = false;
            for w in col_widths.iter_mut() {
                if extra == 0 || *w >= 64 {
                    continue;
                }
                *w += 1;
                extra -= 1;
                grown = true;
            }
            if !grown {
                break;
            }
        }
    }
    let widths: Vec<Constraint> = col_widths
        .iter()
        .map(|w| Constraint::Length(*w as u16))
        .collect();

    let table = Table::new(visible_rows, widths).header(header).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(pane_border(focused))),
    );
    f.render_widget(table, area);
    render_result_scrollbars(f, area, row_off, qr, start_col, end_col, avail_height);
}

fn render_perf_sql_pane(
    sess: &Session,
    specs: &[crate::db::perf::PerfQuerySpec],
    cursor: usize,
    focused: bool,
    maximized: bool,
    f: &mut Frame,
    area: Rect,
) {
    let sql = specs.get(cursor).map(|s| s.sql).unwrap_or("");
    let inner_w = area.width.saturating_sub(2) as usize;
    let view_h = area.height.saturating_sub(2) as usize;
    let wrapped = wrap_sql_lines(sql, inner_w.max(10));
    let max_scroll = wrapped.len().saturating_sub(view_h.max(1));
    let skip = sess.perf_sql_scroll.min(max_scroll);
    let lines: Vec<Line> = wrapped
        .into_iter()
        .skip(skip)
        .take(view_h.max(1))
        .map(highlight_ddl_line)
        .collect();

    let title = format!(
        " {}Diagnostic SQL{} (s: load into editor · n: new tab · Ctrl+C/Y: copy · v: hide) ",
        if focused { "▸ " } else { "" },
        if maximized { " [MAX]" } else { "" }
    );
    let p = Paragraph::new(lines).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(pane_border(focused))),
    );
    f.render_widget(p, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(test)]
    use crate::db::connection::QueryRow;
    use crate::db::ddl::{DdlTarget, DescribedObject};
    use ratatui::backend::TestBackend;

    #[test]
    fn object_ddl_viewer_renders_banner_and_script() {
        let mut sm = SessionManager::new();
        let mut session = sm.sessions.remove(0);
        session.ddl_target = Some(DdlTarget {
            schema: Some("SCOTT".into()),
            schema_quoted: false,
            name: "EMP".into(),
            name_quoted: false,
            kind_hint: None,
        });
        session.ddl_object = Some(DescribedObject {
            schema: "SCOTT".into(),
            name: "EMP".into(),
            kind: "TABLE".into(),
            ddl: "CREATE TABLE SCOTT.EMP (\n  EMPNO NUMBER(4) NOT NULL\n);".into(),
            summary: Some("Tablespace: USERS · Est. Rows: 14".into()),
            elapsed_ms: 12,
        });

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|f| render_object_ddl(&session, true, None, f, f.area()))
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf[(x, y)].symbol());
            }
        }
        assert!(text.contains("SCOTT.EMP"));
        assert!(text.contains("CREATE TABLE SCOTT.EMP"));
        assert!(text.contains("Tablespace: USERS"));
    }

    #[test]
    fn performance_dashboard_renders_gauges_sparkline_and_inline_bars() {
        let mut sm = SessionManager::new();
        {
            let sess = &mut sm.sessions[0];
            sess.is_connected = true;
            let snap = PerfSnapshot {
                total_sessions: 40,
                active_sessions: 8,
                idle_in_tx_sessions: 0,
                blocked_sessions: 1,
                blocking_sessions: 1,
                max_active_secs: 25,
                cache_hit_pct: Some(98.7),
                mem_used_mb: Some(1536.0),
                mem_total_mb: Some(2048.0),
                mem_detail: "SGA 1200M · PGA 336M".into(),
                wait_breakdown: vec![("CPU".into(), 5), ("User I/O".into(), 3)],
                elapsed_ms: 9,
            };
            sess.perf_history.record(&snap);
            sess.perf_snapshot = Some(snap);
            sess.perf_cursor = 1; // Top CPU Consuming Sessions
            sess.perf_results.insert(
                1,
                QueryRow {
                    columns: vec!["SID".into(), "USERNAME".into(), "CPU_SEC".into(), "SHARE_PCT".into()],
                    rows: vec![vec!["101".into(), "SCOTT".into(), "42.5".into(), "85.0".into()]],
                    truncated: false,
                    elapsed_ms: 11,
                    is_error: false,
                    error_msg: None,
                    rows_affected: None,
                    page_offset: 0,
                    page_size: 200,
                    byte_count: 20,
                    total_fetched: 1,
                },
            );
        }

        let layout = PerfLayout {
            split_pct: 32,
            focus: PerfPane::Queries,
            maximized: false,
        };
        let mut terminal = Terminal::new(TestBackend::new(130, 30)).unwrap();
        terminal
            .draw(|f| {
                render_performance(&sm, ResultFormat::Table, layout, f, f.area());
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf[(x, y)].symbol());
            }
        }
        assert!(text.contains("Act 8/40"));
        assert!(text.contains("1 BLOCKED"));
        assert!(text.contains("98.7%"));
        assert!(text.contains("Top CPU Consuming Sessions"));
        assert!(text.contains("85.0%"));
        assert!(text.contains('█'));
    }
}
