use crate::cli::Config;
use crate::db::session_manager::{SessionManager, SessionMode};
use crate::tui::widgets::*;

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers, MouseEvent, MouseEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use std::io::{self, Stdout};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultFormat {
    Table,
    Markdown,
    Ascii,
}

impl Default for ResultFormat {
    fn default() -> Self {
        ResultFormat::Table
    }
}

pub struct App {
    config: Config,
    session_manager: SessionManager,
    should_quit: bool,
    show_startup_help: bool,
    hide_sidebar: bool,
    sidebar_split_pct: u16,
    editor_height: u16,
    is_resizing: bool,
    maximized_box: Option<MaximizedBox>,
    active_window: ActiveWindow,
    result_format: ResultFormat,
    editor_area: Rect,
    results_area: Rect,
    sidebar_area: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MaximizedBox {
    Editor,
    Results,
    Sidebar,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ActiveWindow {
    Editor,
    Results,
    Sidebar,
}

impl App {
    pub fn new(config: Config, session_manager: SessionManager) -> anyhow::Result<Self> {
        Ok(Self {
            config,
            session_manager,
            should_quit: false,
            show_startup_help: true,
            hide_sidebar: false,
            sidebar_split_pct: 25,
            editor_height: 12,
            is_resizing: false,
            maximized_box: None,
            active_window: ActiveWindow::Editor,
            result_format: ResultFormat::default(),
            editor_area: Rect::default(),
            results_area: Rect::default(),
            sidebar_area: Rect::default(),
        })
    }

    pub fn run(&mut self) -> anyhow::Result<()> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        let _ = execute!(
            stdout,
            EnterAlternateScreen,
            crossterm::event::EnableMouseCapture,
            crossterm::event::EnableBracketedPaste,
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            ),
            crossterm::cursor::Show
        );
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let cs = self.config.connect_string(None);
        let user = self.config.connection.user.clone();
        let pwd = self.config.connection.password.clone().unwrap_or_default();
        if !user.is_empty() {
            // Populate conn_dialog with CLI params so new sessions inherit them
            let session = self.session_manager.active_session_mut();
            session.conn_dialog.host = self.config.connection.host.clone();
            session.conn_dialog.port = self.config.connection.port.to_string();
            session.conn_dialog.service = self.config.connection.service.clone();
            session.conn_dialog.user = user.clone();
            session.conn_dialog.password = pwd.clone();
            self.session_manager.connect_active(&cs, &user, &pwd);
        }

        let res = self.run_loop(&mut terminal);

        let _ = execute!(
            terminal.backend_mut(),
            crossterm::event::PopKeyboardEnhancementFlags,
            LeaveAlternateScreen,
            crossterm::event::DisableMouseCapture,
            crossterm::event::DisableBracketedPaste,
            crossterm::cursor::Hide
        );
        disable_raw_mode()?;
        terminal.show_cursor()?;

        res
    }

    fn run_loop(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> anyhow::Result<()> {
        while !self.should_quit {
            self.session_manager.poll_result();
            self.session_manager.poll_conn_result();
            self.session_manager.check_connections();

            terminal.draw(|f| self.ui(f))?;

            if event::poll(std::time::Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(key) => self.handle_key(key),
                    Event::Mouse(mouse) => self.handle_mouse(mouse),
                    Event::Paste(text) => {
                        let session = self.session_manager.active_session_mut();
                        for c in text.chars() {
                            if c == '\n' {
                                session.editor.newline();
                            } else if c != '\r' {
                                session.editor.insert_char(c);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                let in_editor = self.editor_area.contains(Position::new(mouse.column, mouse.row));
                let in_results = self.results_area.contains(Position::new(mouse.column, mouse.row));
                let in_sidebar = self.sidebar_area.contains(Position::new(mouse.column, mouse.row));

                let splitter_x = self.sidebar_area.x;
                if !self.hide_sidebar && (mouse.column).abs_diff(splitter_x) <= 1 && mouse.row > 2 {
                    self.is_resizing = true;
                } else if in_editor {
                    self.active_window = ActiveWindow::Editor;
                    let session = self.session_manager.active_session_mut();
                    session.mode = SessionMode::Query;
                    let raw_row = (mouse.row.saturating_sub(self.editor_area.y + 1)) as usize
                        + session.editor.scroll_offset;
                    let line_num_width = 4usize;
                    let col = (mouse.column.saturating_sub(self.editor_area.x + 1 + line_num_width as u16)) as usize;
                    let target_row = raw_row.min(session.editor.lines.len().saturating_sub(1));
                    session.editor.cursor_row = target_row;
                    session.editor.cursor_col = col.min(session.editor.lines[target_row].len());
                } else if in_results {
                    self.active_window = ActiveWindow::Results;
                } else if in_sidebar {
                    self.active_window = ActiveWindow::Sidebar;
                }
            }
            MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                self.is_resizing = false;
            }
            MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                if self.is_resizing {
                    let (w, _) = crossterm::terminal::size().unwrap_or((80, 24));
                    let width = w as usize;
                    let col = mouse.column as usize;
                    let pct = ((width.saturating_sub(col)) * 100) / width;
                    self.sidebar_split_pct = (pct as u16).clamp(10, 70);
                }
            }
            MouseEventKind::ScrollDown => {
                if self.results_area.contains(Position::new(mouse.column, mouse.row)) {
                    let session = self.session_manager.active_session_mut();
                    session.scroll_offset = session.scroll_offset.saturating_add(3);
                }
            }
            MouseEventKind::ScrollUp => {
                if self.results_area.contains(Position::new(mouse.column, mouse.row)) {
                    let session = self.session_manager.active_session_mut();
                    session.scroll_offset = session.scroll_offset.saturating_sub(3);
                }
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, key: event::KeyEvent) {
        if self.show_startup_help {
            self.show_startup_help = false;
            return;
        }

        let mode = self.session_manager.active_session().mode.clone();

        if mode == SessionMode::ConnectionPickerDialog {
            self.handle_conn_dialog(key);
            return;
        }
        if mode == SessionMode::History {
            self.handle_history(key);
            return;
        }

        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('q')) => {
                self.should_quit = true;
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('t')) => {
                self.session_manager.add_session();
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
                let idx = self.session_manager.active_idx;
                self.session_manager.remove_session(idx);
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Right) | (KeyModifiers::ALT, KeyCode::Right) => {
                self.session_manager.switch_next();
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Left) | (KeyModifiers::ALT, KeyCode::Left) => {
                self.session_manager.switch_prev();
                return;
            }
            (KeyModifiers::ALT, KeyCode::Char('1')) => {
                self.session_manager.switch_to(0);
                return;
            }
            (KeyModifiers::ALT, KeyCode::Char('2')) => {
                self.session_manager.switch_to(1);
                return;
            }
            (KeyModifiers::ALT, KeyCode::Char('3')) => {
                self.session_manager.switch_to(2);
                return;
            }
            (KeyModifiers::ALT, KeyCode::Char('4')) => {
                self.session_manager.switch_to(3);
                return;
            }
            (KeyModifiers::ALT, KeyCode::Char('5')) => {
                self.session_manager.switch_to(4);
                return;
            }
            (KeyModifiers::ALT, KeyCode::Char('6')) => {
                self.session_manager.switch_to(5);
                return;
            }
            (KeyModifiers::ALT, KeyCode::Char('7')) => {
                self.session_manager.switch_to(6);
                return;
            }
            (KeyModifiers::ALT, KeyCode::Char('8')) => {
                self.session_manager.switch_to(7);
                return;
            }
            (KeyModifiers::ALT, KeyCode::Char('9')) => {
                self.session_manager.switch_to(8);
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('o')) => {
                self.session_manager.active_session_mut().mode = SessionMode::ConnectionPickerDialog;
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                self.session_manager.cancel_query();
                return;
            }
            (KeyModifiers::NONE, KeyCode::Esc) => {
                let session = self.session_manager.active_session();
                if session.pending_query {
                    self.session_manager.cancel_query();
                    return;
                }
            }
            (KeyModifiers::CONTROL, KeyCode::Char('b')) => {
                self.hide_sidebar = !self.hide_sidebar;
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('z')) | (KeyModifiers::NONE, KeyCode::F(11)) => {
                self.maximized_box = match self.maximized_box {
                    None => Some(match self.active_window {
                        ActiveWindow::Editor => MaximizedBox::Editor,
                        ActiveWindow::Results => MaximizedBox::Results,
                        ActiveWindow::Sidebar => MaximizedBox::Sidebar,
                    }),
                    Some(_) => None,
                };
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('y')) => {
                let session = self.session_manager.active_session();
                if let Some(last_res) = session.results.last() {
                    let mut text = String::new();
                    match self.result_format {
                        ResultFormat::Markdown => {
                            text.push_str("| ");
                            text.push_str(&last_res.columns.join(" | "));
                            text.push_str(" |\n|");
                            for _ in &last_res.columns {
                                text.push_str("---|");
                            }
                            text.push('\n');
                            for row in &last_res.rows {
                                text.push_str("| ");
                                text.push_str(&row.join(" | "));
                                text.push_str(" |\n");
                            }
                        }
                        ResultFormat::Ascii => {
                            // Calculate column widths
                            let mut col_widths: Vec<usize> = last_res.columns.iter().map(|c| c.len()).collect();
                            for row in &last_res.rows {
                                for (i, cell) in row.iter().enumerate() {
                                    if i < col_widths.len() {
                                        col_widths[i] = col_widths[i].max(cell.len());
                                    }
                                }
                            }
                            // Header
                            text.push_str("+");
                            for w in &col_widths {
                                text.push_str(&"-".repeat(w + 2));
                                text.push_str("+");
                            }
                            text.push('\n');
                            text.push_str("| ");
                            for (i, col) in last_res.columns.iter().enumerate() {
                                text.push_str(&format!("{:width$} | ", col, width = col_widths[i]));
                            }
                            text.push('\n');
                            text.push_str("+");
                            for w in &col_widths {
                                text.push_str(&"-".repeat(w + 2));
                                text.push_str("+");
                            }
                            text.push('\n');
                            // Rows
                            for row in &last_res.rows {
                                text.push_str("| ");
                                for (i, cell) in row.iter().enumerate() {
                                    if i < col_widths.len() {
                                        text.push_str(&format!("{:width$} | ", cell, width = col_widths[i]));
                                    }
                                }
                                text.push('\n');
                            }
                            text.push_str("+");
                            for w in &col_widths {
                                text.push_str(&"-".repeat(w + 2));
                                text.push_str("+");
                            }
                            text.push('\n');
                        }
                        ResultFormat::Table => {
                            // TSV fallback
                            text.push_str(&last_res.columns.join("\t"));
                            text.push('\n');
                            for row in &last_res.rows {
                                text.push_str(&row.join("\t"));
                                text.push('\n');
                            }
                        }
                    }
                    if let Ok(mut clipboard) = arboard::Clipboard::new() {
                        let _ = clipboard.set_text(text);
                    }
                }
                return;
            }
            (KeyModifiers::ALT, KeyCode::Up) => {
                self.editor_height = (self.editor_height + 2).min(40);
                return;
            }
            (KeyModifiers::ALT, KeyCode::Down) => {
                self.editor_height = self.editor_height.saturating_sub(2).max(4);
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('f')) => {
                self.session_manager.fetch_next_page();
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
                self.result_format = match self.result_format {
                    ResultFormat::Table => ResultFormat::Markdown,
                    ResultFormat::Markdown => ResultFormat::Ascii,
                    ResultFormat::Ascii => ResultFormat::Table,
                };
                return;
            }
            (KeyModifiers::NONE, KeyCode::Tab) | (KeyModifiers::SHIFT, KeyCode::BackTab) => {
                self.active_window = match self.active_window {
                    ActiveWindow::Editor => ActiveWindow::Results,
                    ActiveWindow::Results => {
                        if self.hide_sidebar {
                            ActiveWindow::Editor
                        } else {
                            ActiveWindow::Sidebar
                        }
                    }
                    ActiveWindow::Sidebar => ActiveWindow::Editor,
                };
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Enter)
            | (KeyModifiers::ALT, KeyCode::Enter)
            | (KeyModifiers::CONTROL, KeyCode::Char('j'))
            | (KeyModifiers::NONE, KeyCode::F(8))
            | (KeyModifiers::NONE, KeyCode::F(9)) => {
                // Execute statement under cursor
                let session = self.session_manager.active_session();
                if let Some(stmt) = session.editor.current_statement() {
                    self.session_manager.execute_query(&stmt);
                }
                return;
            }
            (KeyModifiers::NONE, KeyCode::F(5)) | (KeyModifiers::CONTROL, KeyCode::Char('r')) => {
                // Run script
                let session = self.session_manager.active_session();
                let stmts = session.editor.all_statements();
                if !stmts.is_empty() {
                    self.session_manager.execute_script(stmts);
                }
                return;
            }
            (KeyModifiers::NONE, KeyCode::F(1)) => {
                self.session_manager.active_session_mut().mode = SessionMode::Help;
                return;
            }
            (KeyModifiers::NONE, KeyCode::F(2)) => {
                self.session_manager.active_session_mut().mode = SessionMode::SessionView;
                return;
            }
            (KeyModifiers::NONE, KeyCode::F(3)) => {
                self.session_manager.active_session_mut().mode = SessionMode::History;
                return;
            }
            _ => {}
        }

        let session = self.session_manager.active_session_mut();

        match session.mode {
            SessionMode::Query => {
                match self.active_window {
                    ActiveWindow::Editor => match (key.modifiers, key.code) {
                        (KeyModifiers::CONTROL, KeyCode::Char('a')) => session.editor.cursor_col = 0,
                        (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
                            session.editor.cursor_col =
                                session.editor.lines[session.editor.cursor_row].len();
                        }
                        (KeyModifiers::CONTROL, KeyCode::Char('k')) => session.editor.kill_line(),
                        (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
                            session.editor.lines[session.editor.cursor_row].clear();
                            session.editor.cursor_col = 0;
                        }
                        (KeyModifiers::CONTROL, KeyCode::Char('d')) => session.editor.delete(),
                        (m, KeyCode::Char(c)) if m.is_empty() || m == KeyModifiers::SHIFT => {
                            if !c.is_control() {
                                session.editor.insert_char(c);
                            }
                        }
                        (m, KeyCode::Backspace) if m.is_empty() || m == KeyModifiers::SHIFT => session.editor.backspace(),
                        (m, KeyCode::Delete) if m.is_empty() || m == KeyModifiers::SHIFT => session.editor.delete(),
                        (m, KeyCode::Enter) if m.is_empty() || m == KeyModifiers::SHIFT => session.editor.newline(),
                        (_, KeyCode::Left) => session.editor.move_left(),
                        (_, KeyCode::Right) => session.editor.move_right(),
                        (_, KeyCode::Up) => session.editor.move_up(),
                        (_, KeyCode::Down) => session.editor.move_down(),
                        (_, KeyCode::Home) => session.editor.cursor_col = 0,
                        (_, KeyCode::End) => {
                            session.editor.cursor_col =
                                session.editor.lines[session.editor.cursor_row].len();
                        }
                        (_, KeyCode::PageUp) => {
                            for _ in 0..10 {
                                session.editor.move_up();
                            }
                        }
                        (_, KeyCode::PageDown) => {
                            for _ in 0..10 {
                                session.editor.move_down();
                            }
                        }
                        _ => {}
                    },
                    ActiveWindow::Results => match key.code {
                        KeyCode::Down => {
                            session.scroll_offset = session.scroll_offset.saturating_add(1)
                        }
                        KeyCode::Up => {
                            session.scroll_offset = session.scroll_offset.saturating_sub(1)
                        }
                        KeyCode::Right => {
                            session.col_scroll_offset = session.col_scroll_offset.saturating_add(1)
                        }
                        KeyCode::Left => {
                            session.col_scroll_offset = session.col_scroll_offset.saturating_sub(1)
                        }
                        KeyCode::PageDown => {
                            session.scroll_offset = session.scroll_offset.saturating_add(20)
                        }
                        KeyCode::PageUp => {
                            session.scroll_offset = session.scroll_offset.saturating_sub(20)
                        }
                        KeyCode::Esc => self.active_window = ActiveWindow::Editor,
                        _ => {}
                    },
                    ActiveWindow::Sidebar => {
                        if key.code == KeyCode::Esc {
                            self.active_window = ActiveWindow::Editor;
                        }
                    }
                }
            }
            SessionMode::Results | SessionMode::SessionView | SessionMode::Help | SessionMode::History | SessionMode::ConnectionPickerDialog => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                    session.mode = SessionMode::Query;
                    self.active_window = ActiveWindow::Editor;
                }
            }
        }
    }

    fn handle_history(&mut self, key: event::KeyEvent) {
        let session = self.session_manager.active_session_mut();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                session.mode = SessionMode::Query;
            }
            KeyCode::Up => {
                session.history_cursor = session.history_cursor.saturating_sub(1);
            }
            KeyCode::Down => {
                if session.history_cursor + 1 < session.query_history.len() {
                    session.history_cursor += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(stmt) = session.query_history.get(session.history_cursor).cloned() {
                    session.editor.set_text(&stmt);
                    session.mode = SessionMode::Query;
                    self.active_window = ActiveWindow::Editor;
                }
            }
            _ => {}
        }
    }

    fn handle_conn_dialog(&mut self, key: event::KeyEvent) {
        let session = self.session_manager.active_session_mut();
        let dlg = &mut session.conn_dialog;

        match key.code {
            KeyCode::Esc => {
                session.mode = SessionMode::Query;
                return;
            }
            KeyCode::Tab | KeyCode::Down => {
                dlg.active_field = (dlg.active_field + 1) % 5;
                return;
            }
            KeyCode::BackTab | KeyCode::Up => {
                dlg.active_field = if dlg.active_field == 0 { 4 } else { dlg.active_field - 1 };
                return;
            }
            KeyCode::Enter => {
                let host = dlg.host.clone();
                let port = dlg.port.clone();
                let service = dlg.service.clone();
                let user = dlg.user.clone();
                let password = dlg.password.clone();
                let cs = format!("//{}:{}/{}", host, port, service);
                session.mode = SessionMode::Query;
                self.session_manager.connect_active(&cs, &user, &password);
                return;
            }
            KeyCode::Char(c) => {
                let field = match dlg.active_field {
                    0 => &mut dlg.host,
                    1 => &mut dlg.port,
                    2 => &mut dlg.service,
                    3 => &mut dlg.user,
                    4 => &mut dlg.password,
                    _ => return,
                };
                field.push(c);
            }
            KeyCode::Backspace => {
                let field = match dlg.active_field {
                    0 => &mut dlg.host,
                    1 => &mut dlg.port,
                    2 => &mut dlg.service,
                    3 => &mut dlg.user,
                    4 => &mut dlg.password,
                    _ => return,
                };
                field.pop();
            }
            _ => {}
        }
    }

    fn ui(&mut self, f: &mut Frame) {
        let size = f.area();

        if self.show_startup_help {
            render_help(f, size);
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(5),
                Constraint::Length(1),
            ])
            .split(size);

        render_tabs(&self.session_manager.sessions, self.session_manager.active_idx, f, chunks[0]);

        let mode = self.session_manager.active_session().mode.clone();

        match mode {
            SessionMode::SessionView => {
                render_session_overview(&self.session_manager, f, chunks[1]);
            }
            SessionMode::Help => {
                render_help(f, chunks[1]);
            }
            SessionMode::History => {
                let session = self.session_manager.active_session();
                render_history(session, f, chunks[1]);
            }
            SessionMode::ConnectionPickerDialog => {
                let session = self.session_manager.active_session();
                render_connection_dialog(session, f, chunks[1]);
            }
            _ => {
                match self.maximized_box {
                    Some(MaximizedBox::Editor) => {
                        self.editor_area = chunks[1];
                    }
                    Some(MaximizedBox::Results) => {
                        self.results_area = chunks[1];
                    }
                    Some(MaximizedBox::Sidebar) => {
                        self.sidebar_area = chunks[1];
                    }
                    None => {
                        let left_pct = 100 - self.sidebar_split_pct;
                        let (left, right) = if self.hide_sidebar {
                            (chunks[1], Rect::default())
                        } else {
                            let h_split = Layout::default()
                                .direction(Direction::Horizontal)
                                .constraints([
                                    Constraint::Percentage(left_pct),
                                    Constraint::Percentage(self.sidebar_split_pct),
                                ])
                                .split(chunks[1]);
                            (h_split[0], h_split[1])
                        };

                        let main_chunks = Layout::default()
                            .direction(Direction::Vertical)
                            .constraints([Constraint::Length(self.editor_height), Constraint::Min(4)])
                            .split(left);

                        self.editor_area = main_chunks[0];
                        self.results_area = main_chunks[1];
                        self.sidebar_area = right;
                    }
                }

                if self.editor_area.height > 2 {
                    let vis_height = self.editor_area.height.saturating_sub(2) as usize;
                    self.session_manager.active_session_mut().editor.ensure_cursor_visible(vis_height);
                }

                let session = self.session_manager.active_session();

                match self.maximized_box {
                    Some(MaximizedBox::Editor) => {
                        render_editor(session, self.active_window == ActiveWindow::Editor, f, self.editor_area);
                    }
                    Some(MaximizedBox::Results) => {
                        let current_res = session.results.last();
                        render_table(session, &current_res, session.scroll_offset, session.col_scroll_offset, f, self.results_area, self.result_format, self.active_window == ActiveWindow::Results);
                    }
                    Some(MaximizedBox::Sidebar) => {
                        render_right_panel(session, f, self.sidebar_area);
                    }
                    None => {
                        render_editor(session, self.active_window == ActiveWindow::Editor, f, self.editor_area);
                        let current_res = session.results.last();
                        render_table(session, &current_res, session.scroll_offset, session.col_scroll_offset, f, self.results_area, self.result_format, self.active_window == ActiveWindow::Results);
                        if !self.hide_sidebar {
                            render_right_panel(session, f, self.sidebar_area);
                        }
                    }
                }

                // Place the real (blinking) terminal cursor inside the editor
                if self.active_window == ActiveWindow::Editor
                    && (self.maximized_box.is_none() || self.maximized_box == Some(MaximizedBox::Editor))
                {
                    let area = self.editor_area;
                    let vis_height = area.height.saturating_sub(2) as usize;
                    let row = session.editor.cursor_row;
                    let scroll = session.editor.scroll_offset;
                    if row >= scroll && row < scroll + vis_height {
                        let line_num_width = 4u16;
                        let max_text_width = area.width.saturating_sub(2 + line_num_width) as usize;
                        let cx = area.x + 1 + line_num_width + session.editor.cursor_col.min(max_text_width) as u16;
                        let cy = area.y + 1 + (row - scroll) as u16;
                        f.set_cursor_position(Position::new(cx, cy));
                    }
                }
            }
        }

        let session = self.session_manager.active_session();
        render_status_bar(session, self.active_window, f, chunks[2]);
    }
}