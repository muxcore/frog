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
use std::time::Instant;
use unicode_width::UnicodeWidthChar;

/// Outcome of a clipboard copy: which channel actually landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyOutcome {
    /// Remote/local OS clipboard via arboard (X11/Wayland/macOS/Windows).
    Os,
    /// Terminal emulator clipboard via OSC52 (works over SSH).
    Osc52,
    /// Both channels landed.
    Both,
    /// Nothing worked.
    Failed,
}

/// Max bytes offered via OSC52 (terminals cap it, often ~700KB).
const MAX_OSC52_BYTES: usize = 500_000;

/// Normalize pasted text: `CRLF` -> single `LF`, lone `CR` (git-bash sends
/// CR-only line breaks) -> `LF`. Caller expands tabs / maps newlines per
/// target (editor newline vs single-line field).
fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Build the raw OSC52 (or tmux-wrapped) escape sequence for `text`.
/// Pure (no I/O) so it can be unit-tested. Returns None for empty/oversize.
fn build_osc52_sequence(text: &str, in_tmux: bool) -> Option<String> {
    if text.is_empty() || text.len() > MAX_OSC52_BYTES {
        return None;
    }
    let encoded = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        text.as_bytes(),
    );
    let osc52 = format!("\x1b]52;c;{}\x07", encoded);
    if in_tmux {
        Some(format!("\x1bPtmux;\x1b{}\x1b\\", osc52))
    } else {
        Some(osc52)
    }
}

/// Write `text` to the terminal emulator clipboard with OSC52.
///
/// Unlike `arboard` (which needs an X11/Wayland display on the machine frog
/// runs on), OSC52 rides the tty back to the *local* terminal, so it works
/// over `ssh` — including git-bash/mintty with `AllowSetSelection=true`
/// (and through tmux with `set -g set-clipboard on`, via DCS passthrough).
/// Terminals without OSC52 support silently ignore the sequence.
///
/// Returns true when the bytes were handed to the tty successfully (which
/// does *not* guarantee the terminal honored them).
fn write_osc52(text: &str) -> bool {
    use std::io::Write;
    let in_tmux =
        std::env::var_os("TMUX").is_some() || std::env::var_os("STY").is_some();
    let Some(sequence) = build_osc52_sequence(text, in_tmux) else {
        return false;
    };
    // Prefer /dev/tty (direct handle to the controlling terminal, bypasses
    // any stdout redirection by the TUI layer); fall back to stdout.
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        tty.write_all(sequence.as_bytes()).and_then(|_| tty.flush()).is_ok()
    } else {
        let mut out = io::stdout();
        out.write_all(sequence.as_bytes()).and_then(|_| out.flush()).is_ok()
    }
}

/// Copy `text` to the clipboard, preferring the OS clipboard and falling
/// back to OSC52 so `Ctrl+Y`/`Alt+Y` keep working over SSH/headless.
fn copy_to_clipboard(text: &str) -> CopyOutcome {
    let os_ok = arboard::Clipboard::new()
        .and_then(|mut cb| {
            cb.set_text(text.to_string())
                .map_err(|_| arboard::Error::ClipboardOccupied)
        })
        .is_ok();
    // Always offer OSC52 as well: when arboard targets a *remote* X server
    // (X forwarding) it won't reach the local Windows clipboard, while OSC52
    // will. Harmless locally.
    let osc_ok = write_osc52(text);
    match (os_ok, osc_ok) {
        (true, true) => CopyOutcome::Both,
        (true, false) => CopyOutcome::Os,
        (false, true) => CopyOutcome::Osc52,
        (false, false) => CopyOutcome::Failed,
    }
}

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

impl ResultFormat {
    /// Table → Markdown → ASCII → Table…
    pub fn cycle(self) -> Self {
        match self {
            ResultFormat::Table => ResultFormat::Markdown,
            ResultFormat::Markdown => ResultFormat::Ascii,
            ResultFormat::Ascii => ResultFormat::Table,
        }
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
    explorer_area: Rect,
    explorer_detail_area: Rect,
    /// F2 session viewer: top list height as % of the view (15..85).
    /// Mirrors `editor_height` in the query tab — Alt+Up grows the list
    /// (shrinking SQL+plan), Alt+Down shrinks it (growing SQL+plan).
    session_list_pct: u16,
    /// F2 session viewer: middle SQL pane height in rows (3..30).
    /// Ctrl+Up grows it (shrinking plan), Ctrl+Down shrinks it (growing plan).
    session_sql_height: u16,
    /// F2 session viewer: which pane has focus (Tab cycles, click sets).
    /// Controls the maximize target and the highlighted border.
    session_view_focus: SessionViewPane,
    /// F2 session viewer: when set, only this pane fills the view
    /// (toggled with Ctrl+Z / F11, same as the query tab).
    session_view_maximized: Option<SessionViewPane>,
    /// Last F2 layout areas (for click-to-focus + splitter drag hit-testing).
    session_view_area: Rect,
    session_list_area: Rect,
    session_sql_area: Rect,
    session_plan_area: Rect,
    /// Which F2 splitter is being mouse-dragged, if any.
    session_resizing: Option<SessionResize>,
    /// Last explorer tree click (visible row, time) for double-click detect.
    last_explorer_click: Option<(usize, Instant)>,
    command_mode: bool,
    command_input: String,
    command_error: Option<String>,
    /// When set, the command bar is in confirm mode for this @file command.
    command_confirm: Option<String>,
    /// Whether frog captures mouse events. Toggled with Ctrl+M / Alt+M
    /// (tmux-style): while OFF, the terminal handles selection/copy natively
    /// and pastes land via bracketed paste (or plain keys on terminals like
    /// git-bash/mintty that don't emit bracketed paste).
    mouse_capture: bool,
    /// Whether the Kitty keyboard protocol was actually enabled. Only true
    /// when the terminal reports enhancement support — otherwise Ctrl+M /
    /// Ctrl+Enter stay ambiguous with Enter and need their Alt- fallbacks.
    keyboard_enhanced: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionViewPane {
    #[default]
    List,
    Sql,
    Plan,
}

impl SessionViewPane {
    pub fn next(self) -> Self {
        match self {
            SessionViewPane::List => SessionViewPane::Sql,
            SessionViewPane::Sql => SessionViewPane::Plan,
            SessionViewPane::Plan => SessionViewPane::List,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            SessionViewPane::List => SessionViewPane::Plan,
            SessionViewPane::Sql => SessionViewPane::List,
            SessionViewPane::Plan => SessionViewPane::Sql,
        }
    }
}

/// Which F2 horizontal splitter is being mouse-dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionResize {
    /// Boundary between the session list (top) and the SQL+plan detail.
    List,
    /// Boundary between the SQL pane and the explain-plan pane.
    Sql,
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
            explorer_area: Rect::default(),
            explorer_detail_area: Rect::default(),
            session_list_pct: 45,
            session_sql_height: 7,
            session_view_focus: SessionViewPane::List,
            session_view_maximized: None,
            session_view_area: Rect::default(),
            session_list_area: Rect::default(),
            session_sql_area: Rect::default(),
            session_plan_area: Rect::default(),
            session_resizing: None,
            last_explorer_click: None,
            command_mode: false,
            command_input: String::new(),
            command_error: None,
            command_confirm: None,
            mouse_capture: true,
            keyboard_enhanced: false,
        })
    }

    pub fn run(&mut self) -> anyhow::Result<()> {
        enable_raw_mode()?;
        // Only request Kitty keyboard enhancements when the terminal claims
        // support — mintty/git-bash and proxies don't, and pushing
        // unconditionally leaves Ctrl+M / Ctrl+Enter ambiguous anyway.
        // (crossterm docs: only kitty/foot/WezTerm/Alacritty et al. support it.)
        self.keyboard_enhanced =
            matches!(crossterm::terminal::supports_keyboard_enhancement(), Ok(true));
        let mut stdout = io::stdout();
        let _ = execute!(
            stdout,
            EnterAlternateScreen,
            crossterm::event::EnableMouseCapture,
            crossterm::event::EnableBracketedPaste,
            crossterm::cursor::Show
        );
        if self.keyboard_enhanced {
            let _ = execute!(
                stdout,
                crossterm::event::PushKeyboardEnhancementFlags(
                    crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                ),
            );
        }
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let params = self.config.connection.clone();
        let user = params.user.clone();
        // Always seed the connection dialog (Ctrl+O) from the startup connection
        // params (CLI flags / ORACLE_*/PG* env vars / config.yml), so it reflects them
        // even when no user is set — e.g. setting only host/service and typing
        // user+password in the dialog. Only auto-connect when we have
        // a user to connect as.
        {
            let session = self.session_manager.active_session_mut();
            session.conn_dialog.db_type = params.db_type;
            session.conn_dialog.host = params.host.clone();
            session.conn_dialog.port = params.port.to_string();
            session.conn_dialog.service = params.service.clone();
            session.conn_dialog.database = params.database.clone();
            session.conn_dialog.user = user.clone();
            session.conn_dialog.password = params.password.clone().unwrap_or_default();
        }
        if !user.is_empty() {
            self.session_manager.connect_active(&params);
        }

        let res = self.run_loop(&mut terminal);

        if self.keyboard_enhanced {
            let _ = execute!(
                terminal.backend_mut(),
                crossterm::event::PopKeyboardEnhancementFlags,
            );
        }
        let _ = execute!(
            terminal.backend_mut(),
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
            self.session_manager.poll_explorer();
            self.session_manager.poll_conn_result();
            self.session_manager.check_connections();
            self.session_manager.tick_statuses();
            self.session_manager.tick_session_view();

            terminal.draw(|f| self.ui(f))?;

            // git-bash/mintty doesn't emit bracketed paste — pastes arrive as
            // a burst of individual key events. Handle the first event after a
            // 50ms wait, then drain everything already queued so a multi-KB
            // paste doesn't take N frames (one draw per char over SSH).
            if event::poll(std::time::Duration::from_millis(50))? {
                self.handle_event(event::read()?);
                while event::poll(std::time::Duration::from_millis(0))? {
                    self.handle_event(event::read()?);
                }
            }
        }
        Ok(())
    }

    fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Key(key) => self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Paste(text) => self.paste_text(&text),
            _ => {}
        }
    }

    /// Insert pasted text into the active target (command bar, connection
    /// dialog field or SQL editor). Normalizes Windows (`\r\n`) and lone-`\r`
    /// (git-bash sends CR-only line breaks) to `\n`, and expands tabs to
    /// spaces (a literal Tab can't be typed — it cycles focus — and
    /// `insert_char` drops control chars, so pasted indentation would vanish).
    fn paste_text(&mut self, text: &str) {
        // Normalize once so `\r\n` becomes a single newline (not two) and a
        // lone `\r` still breaks the line instead of joining two lines.
        let normalized = normalize_paste(text);
        if self.command_mode {
            self.command_input.extend(
                normalized
                    .chars()
                    .filter(|c| !c.is_control())
                    .flat_map(|c| {
                        if c == '\n' {
                            // Single-line bar: join lines with a space.
                            vec![' ']
                        } else {
                            vec![c]
                        }
                    }),
            );
            return;
        }
        let session = self.session_manager.active_session_mut();
        if session.mode == SessionMode::ConnectionPickerDialog {
            for c in normalized.chars() {
                if c == '\n' || c == '\t' {
                    // Single-line field: skip newlines, expand tabs.
                    if c == '\t' {
                        for _ in 0..4 {
                            session.conn_dialog.insert_char(' ');
                        }
                    }
                    continue;
                }
                session.conn_dialog.insert_char(c);
            }
        } else {
            for c in normalized.chars() {
                if c == '\n' {
                    session.editor.newline();
                } else if c == '\t' {
                    for _ in 0..4 {
                        session.editor.insert_char(' ');
                    }
                } else {
                    session.editor.insert_char(c);
                }
            }
        }
    }

    /// Width of the editor line-number gutter. Zero while mouse capture is
    /// OFF: the gutter is hidden then so a terminal-native selection copies
    /// clean SQL without `  12 ` prefixes.
    fn editor_gutter_width(&self) -> u16 {
        if self.mouse_capture { 4 } else { 0 }
    }

    /// Toggle mouse capture (tmux-style). While OFF the terminal handles text
    /// selection/copy natively; bracketed paste still reaches frog.
    /// Bound to both Ctrl+M (needs Kitty CSI-u) and Alt+M (plain ESC m,
    /// survives git-bash/mintty and jump-host proxies).
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
            "Mouse capture ON (Alt+M to release for native copy)"
        } else {
            "Mouse:OFF — gutter hidden, drag to select clean SQL; Alt+M restores; paste via Shift+Ins/Ctrl+Shift+V"
        };
        self.session_manager.active_session_mut().set_status(msg);
    }

    /// Status text for a clipboard copy outcome (shared by Ctrl+Y / Alt+Y).
    fn copy_status(outcome: CopyOutcome, chars: usize, what: &str) -> String {
        match outcome {
            CopyOutcome::Os | CopyOutcome::Both => {
                format!("Copied {} ({} chars) to clipboard", what, chars)
            }
            CopyOutcome::Osc52 => format!(
                "Copied {} ({} chars) via OSC52 — if paste is empty, set AllowSetSelection=true in mintty / enable set-clipboard in tmux",
                what, chars
            ),
            CopyOutcome::Failed => format!(
                "Copy failed ({} chars): no OS clipboard and OSC52 refused — use Mouse:OFF (Alt+M) + terminal selection",
                chars
            ),
        }
    }

    /// Ctrl+Y: copy the last result set (no line numbers involved — the text
    /// is built from the data, never from screen cells).
    fn copy_results_to_clipboard(&mut self) {
        if self.session_manager.active_session().results.is_empty() {
            self.session_manager
                .active_session_mut()
                .set_status("No results to copy yet — run a query first (Alt+Enter/F8)");
            return;
        }
        let (text, chars) = {
            let session = self.session_manager.active_session();
            // Unwrap is safe: emptiness checked above.
            let last_res = session.results.last().expect("results non-empty");
            let mut text = String::new();
            match self.result_format {
                ResultFormat::Markdown => {
                    let refs: Vec<&[String]> =
                        last_res.rows.iter().map(|r| r.as_slice()).collect();
                    text.push_str(&markdown_table(&last_res.columns, &refs));
                }
                ResultFormat::Ascii => {
                    text.push_str(&format_ascii_table(last_res, 0, 0, last_res.rows.len()));
                }
                ResultFormat::Table => {
                    text.push_str(&last_res.columns.join("\t"));
                    text.push('\n');
                    for row in &last_res.rows {
                        text.push_str(&row.join("\t"));
                        text.push('\n');
                    }
                }
            }
            let n = text.chars().count();
            (text, n)
        };
        let outcome = copy_to_clipboard(&text);
        let msg = Self::copy_status(outcome, chars, "results");
        self.session_manager.active_session_mut().set_status(msg);
    }

    /// Alt+Y: copy the SQL under the cursor (statement, else full editor)
    /// without line numbers — built from `editor.lines`, never screen cells.
    fn copy_sql_to_clipboard(&mut self) {
        // Grab owned text first so the immutable borrow ends before the
        // mutable status update below.
        let text_opt = {
            let session = self.session_manager.active_session();
            if let Some(stmt) = session.editor.current_statement() {
                Some(stmt)
            } else {
                let t = session.editor.text();
                if t.trim().is_empty() {
                    None
                } else {
                    Some(t)
                }
            }
        };
        let Some(text) = text_opt else {
            self.session_manager
                .active_session_mut()
                .set_status("Editor empty — nothing to copy");
            return;
        };
        let chars = text.chars().count();
        let outcome = copy_to_clipboard(&text);
        let msg = Self::copy_status(outcome, chars, "SQL");
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

    #[cfg(not(target_os = "linux"))]
    fn read_primary_selection() -> Option<String> {
        arboard::Clipboard::new()
            .ok()?
            .get()
            .text()
            .ok()
            .filter(|t| !t.is_empty())
    }

    /// Whether `key` toggles mouse capture. Ctrl+M needs the Kitty CSI-u
    /// protocol; Alt+M (plain `ESC m`) survives git-bash/mintty and proxies,
    /// so both are accepted (case-insensitive, Shift-tolerant).
    fn is_mouse_toggle(key: event::KeyEvent) -> bool {
        if !matches!(key.code, KeyCode::Char('m') | KeyCode::Char('M')) {
            return false;
        }
        key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::ALT)
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        // F12 explorer owns its area fullscreen: clicks select, gutter /
        // double-click toggles, wheel scrolls the pane under the cursor.
        if self.session_manager.active_session().mode == SessionMode::DbExplorer
            && self.handle_explorer_mouse(mouse)
        {
            return;
        }
        // F2 session viewer owns its area fullscreen: click focuses a pane,
        // drag on a horizontal splitter resizes, wheel scrolls list/plan.
        if self.session_manager.active_session().mode == SessionMode::SessionView
            && self.handle_session_view_mouse(mouse)
        {
            return;
        }
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
                // Over SSH (no X display) arboard fails — tell the user what
                // to press instead of failing silently.
                if self.editor_area.contains(Position::new(mouse.column, mouse.row)) {
                    self.click_editor(mouse.column, mouse.row);
                    if let Some(text) = Self::read_primary_selection() {
                        self.paste_text(&text);
                    } else {
                        self.session_manager.active_session_mut().set_status(
                            "Middle-click paste unavailable here (no X clipboard) — use Shift+Ins / Ctrl+Shift+V",
                        );
                    }
                }
            }
            MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                self.is_resizing = false;
                self.session_resizing = None;
            }
            MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                if self.is_resizing {
                    let (w, _) = crossterm::terminal::size().unwrap_or((80, 24));
                    let width = w as usize;
                    let col = mouse.column as usize;
                    let pct = ((width.saturating_sub(col)) * 100) / width;
                    self.sidebar_split_pct = (pct as u16).clamp(10, 70);
                }
                if let Some(which) = self.session_resizing {
                    self.drag_session_splitter(which, mouse.row);
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
        let gutter = self.editor_gutter_width() as usize;
        let session = self.session_manager.active_session_mut();
        session.mode = SessionMode::Query;
        let raw_row = (row.saturating_sub(self.editor_area.y + 1)) as usize
            + session.editor.scroll_offset;
        let target_col =
            (column.saturating_sub(self.editor_area.x + 1 + gutter as u16)) as usize;
        let target_row = raw_row.min(session.editor.lines.len().saturating_sub(1));
        session.editor.cursor_row = target_row;
        session.editor.cursor_col =
            byte_offset_for_column(&session.editor.lines[target_row], target_col);
        session.editor.snap_cursor_to_char_boundary();
    }

    fn handle_key(&mut self, key: event::KeyEvent) {
        // The kitty keyboard protocol (when enabled) also delivers
        // release events; process press/repeat only.
        if key.kind == event::KeyEventKind::Release {
            return;
        }

        if self.show_startup_help {
            self.show_startup_help = false;
            return;
        }

        // Mouse toggle works in every mode (command bar, dialogs, browsers):
        // Alt+M is the git-bash-safe alias for Ctrl+M.
        if Self::is_mouse_toggle(key) {
            self.toggle_mouse_capture();
            return;
        }
        // Never trap the user: Ctrl+Q quits from anywhere (command bar,
        // dialogs, browsers), not just the query view.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q'))
        {
            self.should_quit = true;
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
        if mode == SessionMode::SessionView {
            self.handle_session_view(key);
            return;
        }
        if mode == SessionMode::DbExplorer {
            self.handle_db_explorer(key);
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
            // NOTE: mouse toggle (Ctrl+M / Alt+M) is handled at the top of
            // handle_key so it works in every mode, even without Kitty CSI-u.
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
            // Ctrl+Y: results (OS clipboard + OSC52 fallback for SSH).
            (KeyModifiers::CONTROL, KeyCode::Char('y'))
            | (KeyModifiers::CONTROL, KeyCode::Char('Y')) => {
                self.copy_results_to_clipboard();
                return;
            }
            // Alt+Y: SQL under cursor, never with line numbers (built from
            // editor.lines, plus OSC52 so it lands on the local clipboard
            // over SSH). Plain `ESC y` survives git-bash like Alt+M/Alt+Enter.
            (KeyModifiers::ALT, KeyCode::Char('y'))
            | (KeyModifiers::ALT, KeyCode::Char('Y')) => {
                self.copy_sql_to_clipboard();
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
                self.result_format = self.result_format.cycle();
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
                self.session_manager.enter_session_view();
                return;
            }
            (KeyModifiers::NONE, KeyCode::F(3)) => {
                self.session_manager.active_session_mut().mode = SessionMode::History;
                return;
            }
            (KeyModifiers::NONE, KeyCode::F(12)) => {
                // Toggle the DB explorer like a fullscreen browser (cf. F2).
                let mode = self.session_manager.active_session().mode.clone();
                if mode == SessionMode::DbExplorer {
                    self.session_manager.active_session_mut().mode = SessionMode::Query;
                    self.active_window = ActiveWindow::Editor;
                } else {
                    self.session_manager.enter_db_explorer();
                }
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
            SessionMode::SessionView
            | SessionMode::Help
            | SessionMode::History
            | SessionMode::DbExplorer
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

    /// TOAD-style session browser: pick a session row, preview its current
    /// SQL and run EXPLAIN on it.
    ///
    /// Pane layout mirrors the query tab: Tab cycles focus (List/SQL/Plan),
    /// Ctrl+Z/F11 maximizes the focused pane, Alt+Up/Down resizes the
    /// session list (Alt+Down grows SQL+plan), Ctrl+Up/Down resizes the
    /// SQL pane (Ctrl+Down grows the explain plan), and dragging a
    /// horizontal splitter resizes with the mouse.
    fn handle_session_view(&mut self, key: event::KeyEvent) {
        // Never trap the user: Ctrl+Q always quits.
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('q') {
            self.should_quit = true;
            return;
        }
        // Focus / zoom / resize — same as the query tab (Ctrl+Z/F11,
        // Alt+Up/Down). Checked before plain navigation because e.g.
        // Alt+Up shares KeyCode::Up with plain Up.
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Tab) => {
                self.session_view_focus = self.session_view_focus.next();
                // Keep maximize in sync when already zoomed: zoom follows focus.
                if self.session_view_maximized.is_some() {
                    self.session_view_maximized = Some(self.session_view_focus);
                }
                return;
            }
            (KeyModifiers::SHIFT, KeyCode::BackTab) | (KeyModifiers::NONE, KeyCode::BackTab) => {
                self.session_view_focus = self.session_view_focus.prev();
                if self.session_view_maximized.is_some() {
                    self.session_view_maximized = Some(self.session_view_focus);
                }
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('z'))
            | (KeyModifiers::CONTROL, KeyCode::Char('Z'))
            | (KeyModifiers::NONE, KeyCode::F(11)) => {
                self.session_view_maximized = match self.session_view_maximized {
                    None => Some(self.session_view_focus),
                    Some(_) => None,
                };
                return;
            }
            (KeyModifiers::ALT, KeyCode::Up) => {
                self.session_list_pct = (self.session_list_pct + 5).min(85);
                return;
            }
            (KeyModifiers::ALT, KeyCode::Down) => {
                self.session_list_pct = self.session_list_pct.saturating_sub(5).max(15);
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Up) => {
                self.session_sql_height = (self.session_sql_height + 1).min(30);
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Down) => {
                self.session_sql_height = self.session_sql_height.saturating_sub(1).max(3);
                return;
            }
            _ => {}
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                let session = self.session_manager.active_session_mut();
                session.mode = SessionMode::Query;
                self.active_window = ActiveWindow::Editor;
                self.session_resizing = None;
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => {
                self.session_manager.move_session_cursor(-1);
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
                self.session_manager.move_session_cursor(1);
            }
            KeyCode::Enter | KeyCode::Char('e') | KeyCode::Char('E') => {
                self.session_manager.explain_selected();
            }
            KeyCode::Char('r') => {
                self.session_manager.load_selected_sql();
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                // ACTIVE-only filter toggle (a again shows all).
                self.session_manager.toggle_session_active_filter();
            }
            KeyCode::Char('s') => {
                // Cycle sort column: 1st press sorts by 1st column,
                // 2nd press by 2nd column, … then back to DB order.
                self.session_manager.cycle_session_sort();
            }
            KeyCode::Char('S') => {
                // Toggle sort direction for the current sort column.
                self.session_manager.toggle_session_sort_dir();
            }
            KeyCode::Char('R') => {
                // Manual list refresh (auto-refresh interval is configurable,
                // see --session-refresh-secs / FROG_SESSION_REFRESH_SECS).
                self.session_manager.refresh_session_list();
                self.session_manager.load_selected_sql();
            }
            KeyCode::F(5) => {
                self.session_manager.refresh_session_list();
                self.session_manager.load_selected_sql();
            }
            KeyCode::PageUp => {
                let session = self.session_manager.active_session_mut();
                session.plan_scroll = session.plan_scroll.saturating_sub(10);
            }
            KeyCode::PageDown => {
                let session = self.session_manager.active_session_mut();
                session.plan_scroll = session.plan_scroll.saturating_add(10);
            }
            KeyCode::Home => {
                let session = self.session_manager.active_session_mut();
                session.plan_scroll = 0;
                session.plan_hscroll = 0;
            }
            KeyCode::Left => {
                let session = self.session_manager.active_session_mut();
                session.plan_hscroll = session.plan_hscroll.saturating_sub(8);
            }
            KeyCode::Right => {
                let session = self.session_manager.active_session_mut();
                session.plan_hscroll = session.plan_hscroll.saturating_add(8);
            }
            _ => {}
        }
    }

    /// F12 DB explorer: tree navigation + preview/DDL detail pane.
    fn handle_db_explorer(&mut self, key: event::KeyEvent) {
        // Never trap the user: Ctrl+Q always quits.
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('q') {
            self.should_quit = true;
            return;
        }
        // Same result style switch as the main viewer: the explorer preview
        // is rendered with the shared result viewer.
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('d') {
            self.result_format = self.result_format.cycle();
            return;
        }
        // Filter input captures most keystrokes until Enter/Esc.
        if self.session_manager.active_session().explorer_filtering {
            let session = self.session_manager.active_session_mut();
            match key.code {
                KeyCode::Esc => {
                    session.explorer_filter.clear();
                    session.explorer_filtering = false;
                    session.explorer_cursor = 0;
                    session.explorer_scroll = 0;
                }
                KeyCode::Enter => {
                    session.explorer_filtering = false;
                    session.explorer_cursor = 0;
                    session.explorer_scroll = 0;
                }
                KeyCode::Backspace => {
                    session.explorer_filter.pop();
                    session.explorer_cursor = 0;
                    session.explorer_scroll = 0;
                }
                KeyCode::Char(c) if !c.is_control() => {
                    session.explorer_filter.push(c);
                    session.explorer_cursor = 0;
                    session.explorer_scroll = 0;
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                let session = self.session_manager.active_session_mut();
                session.mode = SessionMode::Query;
                session.explorer_filtering = false;
                self.active_window = ActiveWindow::Editor;
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => {
                self.session_manager.move_explorer_cursor(-1);
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
                self.session_manager.move_explorer_cursor(1);
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('L') | KeyCode::Enter => {
                self.session_manager.toggle_explorer_cursor();
            }
            KeyCode::Char(' ') => {
                self.session_manager.toggle_explorer_cursor();
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('H') => {
                self.session_manager.collapse_explorer_cursor();
            }
            KeyCode::PageUp => {
                let session = self.session_manager.active_session_mut();
                session.explorer_detail_scroll =
                    session.explorer_detail_scroll.saturating_sub(10);
            }
            KeyCode::PageDown => {
                let session = self.session_manager.active_session_mut();
                session.explorer_detail_scroll =
                    session.explorer_detail_scroll.saturating_add(10);
            }
            KeyCode::Home => {
                let session = self.session_manager.active_session_mut();
                session.explorer_cursor = 0;
                session.explorer_scroll = 0;
                session.explorer_detail_scroll = 0;
                self.session_manager.maybe_load_detail_for_cursor();
            }
            KeyCode::End => {
                let len = self.session_manager.explorer_visible_rows().len();
                {
                    let session = self.session_manager.active_session_mut();
                    session.explorer_cursor = len.saturating_sub(1);
                    session.explorer_detail_scroll = 0;
                }
                self.session_manager.maybe_load_detail_for_cursor();
            }
            KeyCode::Char('/') => {
                let session = self.session_manager.active_session_mut();
                session.explorer_filtering = true;
            }
            KeyCode::Char('r') => self.session_manager.refresh_explorer_node(),
            KeyCode::Char('R') => self.session_manager.refresh_explorer(),
            // Reload the preview/DDL under the cursor.
            KeyCode::Char('d') | KeyCode::Char('D') => {
                self.session_manager.reload_explorer_detail()
            }
            // Copy SELECT * FROM <object> into the editor and go back.
            KeyCode::Char('s') | KeyCode::Char('S') => {
                if let Some(sql) = self.session_manager.explorer_sql_for_cursor() {
                    let session = self.session_manager.active_session_mut();
                    session.editor.set_text(&sql);
                    session.mode = SessionMode::Query;
                    self.active_window = ActiveWindow::Editor;
                    session.set_status(format!("Loaded into editor: {}", sql));
                } else {
                    self.session_manager.active_session_mut().set_status(
                        "Select a table/view/mview first (folders, code and indexes have no SELECT)",
                    );
                }
            }
            _ => {}
        }
    }

    /// Mouse inside the fullscreen explorer. Returns true when the event was
    /// consumed (i.e. we are in explorer mode with a usable layout).
    fn handle_explorer_mouse(&mut self, mouse: MouseEvent) -> bool {
        let tree = self.explorer_area;
        let detail = self.explorer_detail_area;
        if tree.width == 0 || tree.height == 0 {
            return false;
        }
        let pos = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                if detail.width > 0 && detail.contains(pos) {
                    let s = self.session_manager.active_session_mut();
                    s.explorer_detail_scroll = s.explorer_detail_scroll.saturating_add(3);
                } else if tree.contains(pos) {
                    let s = self.session_manager.active_session_mut();
                    s.explorer_scroll = s.explorer_scroll.saturating_add(3);
                }
                return true;
            }
            MouseEventKind::ScrollUp => {
                if detail.width > 0 && detail.contains(pos) {
                    let s = self.session_manager.active_session_mut();
                    s.explorer_detail_scroll = s.explorer_detail_scroll.saturating_sub(3);
                } else if tree.contains(pos) {
                    let s = self.session_manager.active_session_mut();
                    s.explorer_scroll = s.explorer_scroll.saturating_sub(3);
                }
                return true;
            }
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                if !tree.contains(pos) {
                    return false;
                }
                // Content starts one row below the block border.
                let rel = mouse.row.saturating_sub(tree.y + 1) as usize;
                let sess = self.session_manager.active_session();
                let total = self.session_manager.explorer_visible_rows().len();
                if rel >= total {
                    return true;
                }
                let idx = (sess.explorer_scroll + rel).min(total.saturating_sub(1));
                let gutter = mouse.column <= tree.x + 3;
                let now = Instant::now();
                let double = matches!(self.last_explorer_click, Some((last, t))
                    if last == idx && now.duration_since(t).as_millis() < 500);
                self.last_explorer_click = Some((idx, now));
                if gutter || double {
                    // Arrow click / double-click: select + expand/collapse.
                    {
                        let s = self.session_manager.active_session_mut();
                        s.explorer_cursor = idx;
                        s.explorer_detail_scroll = 0;
                    }
                    self.session_manager.toggle_explorer_cursor();
                } else {
                    // Single click: select + kick off preview/DDL.
                    self.session_manager.explorer_select(idx);
                }
                return true;
            }
            _ => {}
        }
        // In explorer mode clicks/wheel elsewhere are swallowed so the
        // underlying editor layout doesn't steal focus mid-browse.
        true
    }

    /// Mouse inside the F2 session viewer. Click focuses a pane, drag on a
    /// horizontal splitter resizes (same idea as the query-tab sidebar
    /// splitter), wheel scrolls the list / plan under the cursor.
    /// Returns true when the event was consumed.
    fn handle_session_view_mouse(&mut self, mouse: MouseEvent) -> bool {
        // While a pane is maximized the stored areas are stale for the
        // hidden panes — still allow wheel scrolling of the visible pane
        // and click/drag to restore? Keep it simple: when maximized, wheel
        // scrolls the visible pane, any drag stops, clicks are swallowed.
        if self.session_view_maximized.is_some() {
            match mouse.kind {
                MouseEventKind::ScrollDown => {
                    match self.session_view_focus {
                        SessionViewPane::List => {
                            self.session_manager.move_session_cursor(1);
                        }
                        SessionViewPane::Plan => {
                            let s = self.session_manager.active_session_mut();
                            s.plan_scroll = s.plan_scroll.saturating_add(3);
                        }
                        SessionViewPane::Sql => {}
                    }
                    return true;
                }
                MouseEventKind::ScrollUp => {
                    match self.session_view_focus {
                        SessionViewPane::List => {
                            self.session_manager.move_session_cursor(-1);
                        }
                        SessionViewPane::Plan => {
                            let s = self.session_manager.active_session_mut();
                            s.plan_scroll = s.plan_scroll.saturating_sub(3);
                        }
                        SessionViewPane::Sql => {}
                    }
                    return true;
                }
                MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                    self.session_resizing = None;
                    return true;
                }
                MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                    return true;
                }
                MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                    return true;
                }
                _ => return true,
            }
        }
        if self.session_view_area.width == 0 || self.session_view_area.height == 0 {
            return false;
        }
        let pos = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                if self.session_plan_area.contains(pos) {
                    let s = self.session_manager.active_session_mut();
                    s.plan_scroll = s.plan_scroll.saturating_add(3);
                } else if self.session_list_area.contains(pos) {
                    self.session_manager.move_session_cursor(1);
                }
                return true;
            }
            MouseEventKind::ScrollUp => {
                if self.session_plan_area.contains(pos) {
                    let s = self.session_manager.active_session_mut();
                    s.plan_scroll = s.plan_scroll.saturating_sub(3);
                } else if self.session_list_area.contains(pos) {
                    self.session_manager.move_session_cursor(-1);
                }
                return true;
            }
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                // Splitter hit-testing: the two panes abut, so the boundary
                // row belongs to both — treat ±1 row as a grab handle.
                let list_bottom = self
                    .session_list_area
                    .y
                    .saturating_add(self.session_list_area.height);
                let sql_bottom = self
                    .session_sql_area
                    .y
                    .saturating_add(self.session_sql_area.height);
                let in_h_range =
                    mouse.column >= self.session_view_area.x && mouse.column < self.session_view_area.x + self.session_view_area.width.max(1);
                if in_h_range && (mouse.row as i16 - list_bottom as i16).abs() <= 1 && self.session_list_area.height > 0 {
                    self.session_resizing = Some(SessionResize::List);
                    self.session_view_focus = SessionViewPane::List;
                    return true;
                }
                if in_h_range && (mouse.row as i16 - sql_bottom as i16).abs() <= 1 && self.session_sql_area.height > 0 {
                    self.session_resizing = Some(SessionResize::Sql);
                    self.session_view_focus = SessionViewPane::Sql;
                    return true;
                }
                if self.session_list_area.contains(pos) {
                    self.session_view_focus = SessionViewPane::List;
                    // Click-to-pick: move the session cursor to the clicked row.
                    let rel = mouse.row.saturating_sub(self.session_list_area.y + 1) as usize;
                    let total = self.session_manager.visible_session_list().len();
                    if total > 0 {
                        let sess = self.session_manager.active_session();
                        // Mirror the table's keep-cursor-visible math: visible
                        // rows = height - borders - header.
                        let avail = self.session_list_area.height.saturating_sub(4) as usize;
                        if avail > 0 && rel < avail {
                            let cursor = sess.session_view_cursor.min(total.saturating_sub(1));
                            let start = if cursor >= avail { cursor + 1 - avail } else { 0 };
                            let idx = (start + rel).min(total.saturating_sub(1));
                            if idx != cursor {
                                self.session_manager.active_session_mut().session_view_cursor = idx;
                                self.session_manager.load_selected_sql();
                            }
                        }
                    }
                    return true;
                }
                if self.session_sql_area.contains(pos) {
                    self.session_view_focus = SessionViewPane::Sql;
                    return true;
                }
                if self.session_plan_area.contains(pos) {
                    self.session_view_focus = SessionViewPane::Plan;
                    return true;
                }
                return false;
            }
            MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                self.session_resizing = None;
                return true;
            }
            MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                if let Some(which) = self.session_resizing {
                    self.drag_session_splitter(which, mouse.row);
                }
                return true;
            }
            _ => {}
        }
        true
    }

    /// Apply a vertical splitter drag in the F2 viewer.
    fn drag_session_splitter(&mut self, which: SessionResize, mouse_row: u16) {
        let total = self.session_view_area.height as usize;
        if total < 10 {
            return;
        }
        match which {
            SessionResize::List => {
                let top = self.session_view_area.y as usize;
                let row = mouse_row as usize;
                let list_h = row.saturating_sub(top);
                let pct = (list_h * 100 / total).clamp(15, 85) as u16;
                self.session_list_pct = pct;
            }
            SessionResize::Sql => {
                let sql_y = self.session_sql_area.y as usize;
                let row = mouse_row as usize;
                // Boundary row == sql_y + height, so height = row - sql_y.
                let h = row.saturating_sub(sql_y).clamp(3, 30) as u16;
                // Keep at least a few rows for the plan pane.
                let max_h = (total.saturating_sub(8)).clamp(3, 30) as u16;
                self.session_sql_height = h.min(max_h.max(3));
            }
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
                let params = dlg.to_params();
                session.mode = SessionMode::Query;
                self.session_manager.connect_active(&params);
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => dlg.clear_field(),
            KeyCode::Backspace => dlg.backspace(),
            KeyCode::Delete => dlg.delete(),
            KeyCode::Char(' ') if dlg.is_type_field() => dlg.toggle_db_type(),
            KeyCode::Char(c) => {
                if dlg.is_type_field() {
                    match c.to_ascii_lowercase() {
                        'o' => dlg.set_db_type(crate::db::DbType::Oracle),
                        'p' => dlg.set_db_type(crate::db::DbType::Postgres),
                        _ => {}
                    }
                } else {
                    dlg.insert_char(c)
                }
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

        render_tabs(
            &self.session_manager.sessions,
            self.session_manager.active_idx,
            f,
            chunks[0],
        );

        let mode = self.session_manager.active_session().mode.clone();

        match mode {
            SessionMode::SessionView => {
                let layout = SessionViewLayout {
                    list_pct: self.session_list_pct,
                    sql_height: self.session_sql_height,
                    focus: self.session_view_focus,
                    maximized: self.session_view_maximized,
                };
                let (list_a, sql_a, plan_a) =
                    render_session_overview(&mut self.session_manager, layout, f, chunks[1]);
                self.session_view_area = chunks[1];
                self.session_list_area = list_a;
                self.session_sql_area = sql_a;
                self.session_plan_area = plan_a;
            }
            SessionMode::DbExplorer => {
                let (tree, detail) = render_db_explorer(
                    &mut self.session_manager,
                    self.result_format,
                    f,
                    chunks[1],
                );
                self.explorer_area = tree;
                self.explorer_detail_area = detail;
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
                            self.mouse_capture,
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
                            self.mouse_capture,
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
                        let gutter = self.editor_gutter_width();
                        let max_text_width = area.width.saturating_sub(2 + gutter) as usize;
                        let cx = area.x
                            + 1
                            + gutter
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
            render_status_bar(
                session,
                self.active_window,
                self.session_view_focus,
                self.session_view_maximized,
                self.mouse_capture,
                f,
                chunks[2],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState};

    fn key(mods: KeyModifiers, code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    #[test]
    fn paste_normalizes_crlf_and_lone_cr() {
        assert_eq!(normalize_paste("a\r\nb"), "a\nb");
        assert_eq!(normalize_paste("a\rb"), "a\nb");
        assert_eq!(normalize_paste("a\nb"), "a\nb");
        assert_eq!(normalize_paste("a\r\n\r\nb"), "a\n\nb");
    }

    #[test]
    fn osc52_sequence_encodes_and_wraps_tmux() {
        // "hi" -> aGk=
        let seq = build_osc52_sequence("hi", false).expect("seq");
        assert_eq!(seq, "\x1b]52;c;aGk=\x07");
        let tmux = build_osc52_sequence("hi", true).expect("tmux seq");
        assert!(tmux.starts_with("\x1bPtmux;\x1b"), "tmux wrap: {:?}", tmux);
        assert!(tmux.ends_with("\x1b\\"), "tmux wrap: {:?}", tmux);
        assert!(tmux.contains("aGk="), "tmux wrap: {:?}", tmux);
    }

    #[test]
    fn osc52_rejects_empty_and_oversize() {
        assert!(build_osc52_sequence("", false).is_none());
        let big = "x".repeat(MAX_OSC52_BYTES + 1);
        assert!(build_osc52_sequence(&big, false).is_none());
    }

    #[test]
    fn mouse_toggle_accepts_ctrl_and_alt_forms() {
        assert!(App::is_mouse_toggle(key(
            KeyModifiers::CONTROL,
            KeyCode::Char('m')
        )));
        assert!(App::is_mouse_toggle(key(
            KeyModifiers::ALT,
            KeyCode::Char('m')
        )));
        assert!(App::is_mouse_toggle(key(
            KeyModifiers::ALT,
            KeyCode::Char('M')
        )));
        assert!(App::is_mouse_toggle(key(
            KeyModifiers::ALT | KeyModifiers::SHIFT,
            KeyCode::Char('M')
        )));
        assert!(!App::is_mouse_toggle(key(
            KeyModifiers::NONE,
            KeyCode::Char('m')
        )));
        assert!(!App::is_mouse_toggle(key(
            KeyModifiers::CONTROL,
            KeyCode::Char('y')
        )));
    }

    fn test_app() -> App {
        use crate::cli::{Config, ConnectionParams, UiConfig};
        use crate::db::DbType;
        let config = Config {
            connection: ConnectionParams {
                db_type: DbType::Oracle,
                host: String::new(),
                port: 1521,
                service: String::new(),
                database: String::new(),
                user: String::new(),
                password: None,
            },
            ui: UiConfig::default(),
            max_rows: 10000,
            autocommit: true,
            max_history: 1000,
            session_refresh_secs: 60,
            saved_connections: Vec::new(),
        };
        App::new(config, SessionManager::new()).expect("app")
    }

    #[test]
    fn session_view_pane_cycles() {
        assert_eq!(SessionViewPane::List.next(), SessionViewPane::Sql);
        assert_eq!(SessionViewPane::Sql.next(), SessionViewPane::Plan);
        assert_eq!(SessionViewPane::Plan.next(), SessionViewPane::List);
        assert_eq!(SessionViewPane::List.prev(), SessionViewPane::Plan);
        assert_eq!(SessionViewPane::Sql.prev(), SessionViewPane::List);
        assert_eq!(SessionViewPane::Plan.prev(), SessionViewPane::Sql);
    }

    #[test]
    fn session_view_alt_up_down_resizes_list() {
        let mut app = test_app();
        assert_eq!(app.session_list_pct, 45);
        app.handle_session_view(key(KeyModifiers::ALT, KeyCode::Up));
        assert_eq!(app.session_list_pct, 50);
        app.handle_session_view(key(KeyModifiers::ALT, KeyCode::Down));
        assert_eq!(app.session_list_pct, 45);
        // Clamps at 15..85.
        app.session_list_pct = 85;
        app.handle_session_view(key(KeyModifiers::ALT, KeyCode::Up));
        assert_eq!(app.session_list_pct, 85);
        app.session_list_pct = 15;
        app.handle_session_view(key(KeyModifiers::ALT, KeyCode::Down));
        assert_eq!(app.session_list_pct, 15);
    }

    #[test]
    fn session_view_ctrl_up_down_resizes_sql() {
        let mut app = test_app();
        assert_eq!(app.session_sql_height, 7);
        app.handle_session_view(key(KeyModifiers::CONTROL, KeyCode::Up));
        assert_eq!(app.session_sql_height, 8);
        app.handle_session_view(key(KeyModifiers::CONTROL, KeyCode::Down));
        assert_eq!(app.session_sql_height, 7);
        app.session_sql_height = 30;
        app.handle_session_view(key(KeyModifiers::CONTROL, KeyCode::Up));
        assert_eq!(app.session_sql_height, 30);
        app.session_sql_height = 3;
        app.handle_session_view(key(KeyModifiers::CONTROL, KeyCode::Down));
        assert_eq!(app.session_sql_height, 3);
    }

    #[test]
    fn session_view_tab_cycles_focus_and_ctrl_z_zooms() {
        let mut app = test_app();
        assert_eq!(app.session_view_focus, SessionViewPane::List);
        app.handle_session_view(key(KeyModifiers::NONE, KeyCode::Tab));
        assert_eq!(app.session_view_focus, SessionViewPane::Sql);
        app.handle_session_view(key(KeyModifiers::NONE, KeyCode::Tab));
        assert_eq!(app.session_view_focus, SessionViewPane::Plan);
        app.handle_session_view(key(KeyModifiers::SHIFT, KeyCode::BackTab));
        assert_eq!(app.session_view_focus, SessionViewPane::Sql);

        assert_eq!(app.session_view_maximized, None);
        app.handle_session_view(key(KeyModifiers::CONTROL, KeyCode::Char('z')));
        assert_eq!(
            app.session_view_maximized,
            Some(SessionViewPane::Sql)
        );
        app.handle_session_view(key(KeyModifiers::NONE, KeyCode::F(11)));
        assert_eq!(app.session_view_maximized, None);
    }

    #[test]
    fn session_view_plain_up_still_navigates() {
        // Alt+Up must resize (not navigate); plain Up navigates. With an
        // empty list the cursor stays 0 either way — the observable
        // difference is that plain Up leaves the layout untouched.
        let mut app = test_app();
        let before = app.session_list_pct;
        app.handle_session_view(key(KeyModifiers::NONE, KeyCode::Up));
        assert_eq!(app.session_list_pct, before);
        app.handle_session_view(key(KeyModifiers::ALT, KeyCode::Up));
        assert_eq!(app.session_list_pct, before + 5);
    }
}
