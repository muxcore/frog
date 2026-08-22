use crate::cli::Config;
use crate::db::session_manager::{SessionManager, SessionMode};
use crate::tui::widgets::*;

use crate::db::session_manager::ConnectionDialog;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers, MouseEvent, MouseEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use std::io::{self, Stdout};
use unicode_width::UnicodeWidthChar;

/// Convert a screen-column offset into a byte offset on `line`, walking chars
/// by display width so multibyte / wide characters position correctly.
fn byte_offset_for_column(line: &str, target_col: usize) -> usize {
    let mut cells = 0usize;
    let mut byte_off = 0usize;
    for ch in line.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(1);
        if cells + w > target_col {
            break;
        }
        cells += w;
        byte_off += ch.len_utf8();
    }
    byte_off
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResultFormat {
    #[default]
    Table,
    Markdown,
    Ascii,
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
    command_mode: bool,
    command_input: String,
    command_error: Option<String>,
    /// When set, the command bar is in confirm mode for this @file command.
    command_confirm: Option<String>,
    /// Whether frog captures mouse events. Toggled with Ctrl+M (tmux-style):
    /// while OFF, the terminal handles selection/copy natively and pastes land
    /// via bracketed paste.
    mouse_capture: bool,
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
            command_mode: false,
            command_input: String::new(),
            command_error: None,
            command_confirm: None,
            mouse_capture: true,
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

        let cs = self.config.connect_string();
        let user = self.config.connection.user.clone();
        let pwd = self.config.connection.password.clone().unwrap_or_default();
        // Always seed the connection dialog (Ctrl+O) from the startup connection
        // params (CLI flags / ORACLE_* env vars / config.yml), so it reflects them
        // even when no user is set — e.g. setting only ORACLE_HOST/ORACLE_SERVICE
        // and typing user+password in the dialog. Only auto-connect when we have
        // a user to connect as.
        {
            let session = self.session_manager.active_session_mut();
            session.conn_dialog.host = self.config.connection.host.clone();
            session.conn_dialog.port = self.config.connection.port.to_string();
            session.conn_dialog.service = self.config.connection.service.clone();
            session.conn_dialog.user = user.clone();
            session.conn_dialog.password = pwd.clone();
        }
        if !user.is_empty() {
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

    fn run_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> anyhow::Result<()> {
        while !self.should_quit {
            self.session_manager.poll_result();
            self.session_manager.poll_conn_result();
            self.session_manager.check_connections();
            self.session_manager.tick_statuses();

            terminal.draw(|f| self.ui(f))?;

            if event::poll(std::time::Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(key) => self.handle_key(key),
                    Event::Mouse(mouse) => self.handle_mouse(mouse),
                    Event::Paste(text) => self.paste_text(&text),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// Insert pasted text into the active target (command bar, connection
    /// dialog field or SQL editor).
    fn paste_text(&mut self, text: &str) {
        if self.command_mode {
            self.command_input
                .extend(text.chars().filter(|c| !c.is_control()));
            return;
        }
        let session = self.session_manager.active_session_mut();
        if session.mode == SessionMode::ConnectionPickerDialog {
            for c in text.chars() {
                session.conn_dialog.insert_char(c);
            }
        } else {
            for c in text.chars() {
                if c == '\n' {
                    session.editor.newline();
                } else if c != '\r' {
                    session.editor.insert_char(c);
                }
            }
        }
    }

    /// Toggle mouse capture (tmux-style). While OFF the terminal handles text
    /// selection/copy natively; bracketed paste still reaches frog.
    fn toggle_mouse_capture(&mut self) {
        use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
        self.mouse_capture = !self.mouse_capture;
        let mut stdout = io::stdout();
        let _ = if self.mouse_capture {
            execute!(stdout, EnableMouseCapture)
        } else {
            execute!(stdout, DisableMouseCapture)
        };
        let msg = if self.mouse_capture {
            "Mouse capture ON"
        } else {
            "Mouse capture OFF — select to copy with the terminal; middle-click/Ctrl+Shift+V pastes"
        };
        self.session_manager.active_session_mut().set_status(msg);
    }

    /// Read the Linux primary selection (middle-click clipboard), falling back
    /// to the regular clipboard.
    #[cfg(target_os = "linux")]
    fn read_primary_selection() -> Option<String> {
        use arboard::{GetExtLinux, LinuxClipboardKind};
        let mut cb = arboard::Clipboard::new().ok()?;
        if let Ok(text) = cb.get().clipboard(LinuxClipboardKind::Primary).text() {
            if !text.is_empty() {
                return Some(text);
            }
        }
        cb.get().text().ok().filter(|t| !t.is_empty())
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                let in_editor = self
                    .editor_area
                    .contains(Position::new(mouse.column, mouse.row));
                let in_results = self
                    .results_area
                    .contains(Position::new(mouse.column, mouse.row));
                let in_sidebar = self
                    .sidebar_area
                    .contains(Position::new(mouse.column, mouse.row));

                let splitter_x = self.sidebar_area.x;
                if !self.hide_sidebar && (mouse.column).abs_diff(splitter_x) <= 1 && mouse.row > 2 {
                    self.is_resizing = true;
                } else if in_editor {
                    self.click_editor(mouse.column, mouse.row);
                } else if in_results {
                    self.active_window = ActiveWindow::Results;
                } else if in_sidebar {
                    self.active_window = ActiveWindow::Sidebar;
                }
            }
            MouseEventKind::Down(crossterm::event::MouseButton::Middle) => {
                // tmux-style middle-click paste: place the cursor where the
                // user clicked in the editor, then insert the primary selection.
                if self.editor_area.contains(Position::new(mouse.column, mouse.row)) {
                    self.click_editor(mouse.column, mouse.row);
                    if let Some(text) = Self::read_primary_selection() {
                        self.paste_text(&text);
                    }
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
                if self
                    .results_area
                    .contains(Position::new(mouse.column, mouse.row))
                {
                    let session = self.session_manager.active_session_mut();
                    session.scroll_offset = session.scroll_offset.saturating_add(3);
                }
            }
            MouseEventKind::ScrollUp
                if self
                    .results_area
                    .contains(Position::new(mouse.column, mouse.row)) =>
            {
                let session = self.session_manager.active_session_mut();
                session.scroll_offset = session.scroll_offset.saturating_sub(3);
            }
            _ => {}
        }
    }

    /// Focus the editor and move the text cursor to the clicked position.
    fn click_editor(&mut self, column: u16, row: u16) {
        self.active_window = ActiveWindow::Editor;
        let session = self.session_manager.active_session_mut();
        session.mode = SessionMode::Query;
        let raw_row = (row.saturating_sub(self.editor_area.y + 1)) as usize
            + session.editor.scroll_offset;
        let line_num_width = 4usize;
        let target_col =
            (column.saturating_sub(self.editor_area.x + 1 + line_num_width as u16)) as usize;
        let target_row = raw_row.min(session.editor.lines.len().saturating_sub(1));
        session.editor.cursor_row = target_row;
        session.editor.cursor_col =
            byte_offset_for_column(&session.editor.lines[target_row], target_col);
        session.editor.snap_cursor_to_char_boundary();
    }

    fn handle_key(&mut self, key: event::KeyEvent) {
        // The kitty keyboard protocol (enabled at startup) also delivers
        // release events; process press/repeat only.
        if key.kind == event::KeyEventKind::Release {
            return;
        }

        if self.show_startup_help {
            self.show_startup_help = false;
            return;
        }

        if self.command_mode {
            self.handle_command_key(key);
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
            // Enter tmux-style command bar (Ctrl+:). Full-width '；' and
            // Ctrl+; are also accepted for convenience on non-US layouts.
            (KeyModifiers::CONTROL, KeyCode::Char(':'))
            | (KeyModifiers::CONTROL, KeyCode::Char(';'))
            | (KeyModifiers::CONTROL, KeyCode::Char('：')) => {
                self.command_mode = true;
                self.command_input.clear();
                self.command_error = None;
                self.command_confirm = None;
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
            (KeyModifiers::ALT, KeyCode::Char(c @ '1'..='9')) => {
                self.session_manager.switch_to(c as usize - '1' as usize);
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('o')) => {
                let session = self.session_manager.active_session_mut();
                let field = session.conn_dialog.active_field;
                session.conn_dialog.select_field(field);
                session.mode = SessionMode::ConnectionPickerDialog;
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
            // tmux-style: toggle mouse capture so the terminal's native
            // selection/copy can be used (Ctrl+M needs a CSI-u terminal, same
            // as Ctrl+Enter).
            (KeyModifiers::CONTROL, KeyCode::Char('m')) => {
                self.toggle_mouse_capture();
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
                            text.push_str(&format_ascii_table(
                                last_res,
                                0,
                                0,
                                last_res.rows.len(),
                            ));
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
            SessionMode::Query => match self.active_window {
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
                    (m, KeyCode::Backspace) if m.is_empty() || m == KeyModifiers::SHIFT => {
                        session.editor.backspace()
                    }
                    (m, KeyCode::Delete) if m.is_empty() || m == KeyModifiers::SHIFT => {
                        session.editor.delete()
                    }
                    (m, KeyCode::Enter) if m.is_empty() || m == KeyModifiers::SHIFT => {
                        session.editor.newline()
                    }
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
                    KeyCode::Up => session.scroll_offset = session.scroll_offset.saturating_sub(1),
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
            },
            SessionMode::Results
            | SessionMode::SessionView
            | SessionMode::Help
            | SessionMode::History
            | SessionMode::ConnectionPickerDialog => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                    session.mode = SessionMode::Query;
                    self.active_window = ActiveWindow::Editor;
                }
            }
        }
    }

    /// Handle input while the tmux-style command bar is open (Ctrl+:).
    fn handle_command_key(&mut self, key: event::KeyEvent) {
        // Confirmation prompt: y/Enter run, n/Esc cancel.
        if self.command_confirm.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    let cmd = self.command_confirm.take().unwrap();
                    self.command_mode = false;
                    self.command_input.clear();
                    self.command_error = None;
                    self.run_command(&cmd, true);
                }
                _ => {
                    self.command_confirm = None;
                    self.command_mode = false;
                    self.command_input.clear();
                    self.command_error = None;
                }
            }
            return;
        }

        match key.code {
            KeyCode::Esc => {
                self.command_mode = false;
                self.command_input.clear();
                self.command_error = None;
            }
            KeyCode::Enter => {
                let cmd = self.command_input.trim().to_string();
                self.command_mode = false;
                self.command_input.clear();
                self.command_error = None;
                if !cmd.is_empty() {
                    self.run_command(&cmd, false);
                }
            }
            KeyCode::Backspace => {
                self.command_input.pop();
            }
            KeyCode::Char(c) if !c.is_control() => {
                self.command_input.push(c);
            }
            _ => {}
        }
    }

    /// Execute a command typed into the command bar. `confirmed` is true when
    /// the user already accepted the "Run? [y/N]" prompt for this command.
    fn run_command(&mut self, cmd: &str, confirmed: bool) {
        let lower = cmd.trim().to_lowercase();

        // @file / @ file.sql — run a SQL script file (handled by execute_query).
        if lower.starts_with('@') {
            let path = cmd.trim_start_matches('@').trim();
            // Confirm running files with a non-.sql extension.
            if !path.ends_with(".sql") && !confirmed {
                self.command_mode = true;
                self.command_confirm = Some(cmd.to_string());
                self.command_input.clear();
                self.command_error = None;
                return;
            }
            let session = self.session_manager.active_session_mut();
            session.set_status(format!("Running script '{}'...", path));
            self.session_manager.execute_query(cmd);
            return;
        }

        match lower.as_str() {
            "clear" | "cls" => {
                let session = self.session_manager.active_session_mut();
                session.results.clear();
                session.set_status("Results cleared");
            }
            _ => {
                self.command_error = Some(format!("Unknown command: '{}'", cmd));
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
            }
            KeyCode::Tab | KeyCode::Down => {
                let next = (dlg.active_field + 1) % ConnectionDialog::FIELD_COUNT;
                dlg.select_field(next);
            }
            KeyCode::BackTab | KeyCode::Up => {
                let prev = if dlg.active_field == 0 {
                    ConnectionDialog::FIELD_COUNT - 1
                } else {
                    dlg.active_field - 1
                };
                dlg.select_field(prev);
            }
            KeyCode::Left => dlg.cursor_left(),
            KeyCode::Right => dlg.cursor_right(),
            KeyCode::Home | KeyCode::Char('a') if key.modifiers == KeyModifiers::CONTROL => {
                dlg.cursor_home()
            }
            KeyCode::End | KeyCode::Char('e') if key.modifiers == KeyModifiers::CONTROL => {
                dlg.cursor_end()
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
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => dlg.clear_field(),
            KeyCode::Backspace => dlg.backspace(),
            KeyCode::Delete => dlg.delete(),
            KeyCode::Char(c) => dlg.insert_char(c),
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

        render_tabs(
            &self.session_manager.sessions,
            self.session_manager.active_idx,
            f,
            chunks[0],
        );

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
                            .constraints([
                                Constraint::Length(self.editor_height),
                                Constraint::Min(4),
                            ])
                            .split(left);

                        self.editor_area = main_chunks[0];
                        self.results_area = main_chunks[1];
                        self.sidebar_area = right;
                    }
                }

                if self.editor_area.height > 2 {
                    let vis_height = self.editor_area.height.saturating_sub(2) as usize;
                    self.session_manager
                        .active_session_mut()
                        .editor
                        .ensure_cursor_visible(vis_height);
                }

                let session = self.session_manager.active_session();

                match self.maximized_box {
                    Some(MaximizedBox::Editor) => {
                        render_editor(
                            session,
                            self.active_window == ActiveWindow::Editor,
                            f,
                            self.editor_area,
                        );
                    }
                    Some(MaximizedBox::Results) => {
                        let current_res = session.results.last();
                        render_table(
                            session,
                            &current_res,
                            TableViewState {
                                scroll_offset: session.scroll_offset,
                                col_scroll_offset: session.col_scroll_offset,
                                result_format: self.result_format,
                                focused: self.active_window == ActiveWindow::Results,
                            },
                            f,
                            self.results_area,
                        );
                    }
                    Some(MaximizedBox::Sidebar) => {
                        render_right_panel(session, f, self.sidebar_area);
                    }
                    None => {
                        render_editor(
                            session,
                            self.active_window == ActiveWindow::Editor,
                            f,
                            self.editor_area,
                        );
                        let current_res = session.results.last();
                        render_table(
                            session,
                            &current_res,
                            TableViewState {
                                scroll_offset: session.scroll_offset,
                                col_scroll_offset: session.col_scroll_offset,
                                result_format: self.result_format,
                                focused: self.active_window == ActiveWindow::Results,
                            },
                            f,
                            self.results_area,
                        );
                        if !self.hide_sidebar {
                            render_right_panel(session, f, self.sidebar_area);
                        }
                    }
                }

                // Place the real (blinking) terminal cursor inside the editor
                if self.active_window == ActiveWindow::Editor
                    && (self.maximized_box.is_none()
                        || self.maximized_box == Some(MaximizedBox::Editor))
                {
                    let area = self.editor_area;
                    let vis_height = area.height.saturating_sub(2) as usize;
                    let row = session.editor.cursor_row;
                    let scroll = session.editor.scroll_offset;
                    if row >= scroll && row < scroll + vis_height {
                        let line_num_width = 4u16;
                        let max_text_width = area.width.saturating_sub(2 + line_num_width) as usize;
                        let cx = area.x
                            + 1
                            + line_num_width
                            + session.editor.cursor_col.min(max_text_width) as u16;
                        let cy = area.y + 1 + (row - scroll) as u16;
                        f.set_cursor_position(Position::new(cx, cy));
                    }
                }
            }
        }

        let session = self.session_manager.active_session();
        if self.command_mode {
            render_command_bar(
                &self.command_input,
                self.command_error.as_deref(),
                self.command_confirm.as_deref(),
                f,
                chunks[2],
            );
            // Put the terminal cursor after the ": " prompt inside the bar.
            let bar = chunks[2];
            let cx = (bar.x + 2 + self.command_input.chars().count() as u16)
                .min(bar.right().saturating_sub(1));
            f.set_cursor_position(Position::new(cx, bar.y));
        } else {
            render_status_bar(session, self.active_window, self.mouse_capture, f, chunks[2]);
        }
    }
}
