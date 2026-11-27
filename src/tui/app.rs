use crate::cli::Config;
use crate::db::connection::QueryRow;
use crate::db::session_manager::{
    EditorSelection, SessionManager, SessionMode, TableSelection,
};
use crate::tui::perf_view::{render_object_ddl, render_performance, PerfLayout, PerfPane};
use crate::tui::search::{
    find_editor_matches, find_line_matches, find_list_matches, find_table_matches,
    SearchDirection, SearchMatch, SearchOrigin, SearchState, SearchTarget,
};
use crate::tui::widgets::*;

use crate::db::session_manager::ConnectionDialog;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers, MouseEvent, MouseEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use ratatui::widgets::Clear;
use std::io::{self, Stdout};
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Restore terminal modes unconditionally (normal exit, early `?`, or panic).
fn restore_terminal_state(keyboard_enhanced: bool) {
    let mut stdout = io::stdout();
    if keyboard_enhanced {
        let _ = execute!(stdout, crossterm::event::PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        stdout,
        LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste,
        crossterm::cursor::Show
    );
    let _ = disable_raw_mode();
}

struct TerminalGuard {
    keyboard_enhanced: bool,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal_state(self.keyboard_enhanced);
    }
}
#[derive(Debug, Clone, Default)]
struct CopyOutcome {
    /// arboard reached the OS clipboard (X11/Wayland/macOS/Windows).
    os: bool,
    /// An external helper reached the clipboard; its name when it did.
    command: Option<String>,
    /// Terminal emulator clipboard via OSC52 (works over SSH).
    osc52: bool,
}

/// Max bytes offered via OSC52 (terminals cap it, often ~700KB).
const MAX_OSC52_BYTES: usize = 500_000;

/// Apply a signed scroll delta, clamped to `max` (inclusive).
fn scroll_by(current: usize, delta: isize, max: usize) -> usize {
    let next = if delta >= 0 {
        current.saturating_add(delta as usize)
    } else {
        current.saturating_sub(delta.unsigned_abs())
    };
    next.min(max)
}

/// Sidebar share after adapting to the terminal width: the info panel has a
/// roughly fixed content width, so on a big monitor a flat 25% would waste
/// table columns. Pure (no I/O) so it can be unit-tested.
fn effective_sidebar_pct(term_width: u16, configured: u16) -> u16 {
    let cap = if term_width >= 220 {
        12
    } else if term_width >= 180 {
        15
    } else if term_width >= 140 {
        20
    } else {
        70
    };
    configured.min(cap).clamp(10, 70)
}

/// Editor height after adapting to the terminal height: on short screens the
/// configured height would leave the table with ~4 rows, so shrink the
/// editor and give the rows back to the results. On tall screens the editor
/// stays as configured and all extra rows go to the table.
fn effective_editor_height(main_height: u16, configured: u16) -> u16 {
    let min_results = 8u16;
    let max_editor = main_height.saturating_sub(min_results).max(4);
    configured.min(max_editor).max(4)
}

/// Pin result scroll offsets into the loaded data. Scroll keys and the wheel
/// only ever grow the offsets (`saturating_add`), so without this, scrolling
/// past the end accumulates a huge offset and the opposite direction feels
/// dead for dozens of presses — in every result format. Pure (no I/O) so it
/// can be unit-tested.
fn clamp_scroll_offsets(
    scroll_offset: usize,
    col_scroll_offset: usize,
    total_rows: usize,
    total_cols: usize,
) -> (usize, usize) {
    (
        scroll_offset.min(total_rows.saturating_sub(1)),
        col_scroll_offset.min(total_cols.saturating_sub(1)),
    )
}
/// Normalize pasted text: `CRLF` -> single `LF`, lone `CR` (git-bash sends
/// CR-only line breaks) -> `LF`. Caller expands tabs / maps newlines per
/// target (editor newline vs single-line field).
fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// The character a plain text key contributes, or `None` for anything else
/// (shortcuts, cursor keys, function keys, key releases).
fn text_of_key(key: event::KeyEvent) -> Option<char> {
    if key.kind == event::KeyEventKind::Release {
        return None;
    }
    match (key.modifiers, key.code) {
        (m, KeyCode::Char(c)) if (m.is_empty() || m == KeyModifiers::SHIFT) && !c.is_control() => {
            Some(c)
        }
        (m, KeyCode::Enter) if m.is_empty() => Some('\n'),
        (m, KeyCode::Tab) if m.is_empty() => Some('\t'),
        _ => None,
    }
}

/// Reassemble a burst of key events into pasted text. Returns `None` as soon
/// as the burst contains anything that isn't plain text, so real shortcuts
/// and non-text input are never swallowed by the paste path.
fn text_from_key_burst(events: &[Event]) -> Option<String> {
    let mut text = String::new();
    for ev in events {
        let Event::Key(key) = ev else { return None };
        text.push(text_of_key(*key)?);
    }
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Build the raw OSC52 (or tmux-wrapped) escape sequence for `text`.
/// Pure (no I/O) so it can be unit-tested. Returns None for empty/oversize.
fn build_osc52_sequence(text: &str, in_tmux: bool) -> Option<String> {
    if text.is_empty() || text.len() > MAX_OSC52_BYTES {
        return None;
    }
    let encoded =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, text.as_bytes());
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
    let in_tmux = std::env::var_os("TMUX").is_some() || std::env::var_os("STY").is_some();
    let Some(sequence) = build_osc52_sequence(text, in_tmux) else {
        return false;
    };
    // Prefer /dev/tty (direct handle to the controlling terminal, bypasses
    // any stdout redirection by the TUI layer); fall back to stdout.
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        tty.write_all(sequence.as_bytes())
            .and_then(|_| tty.flush())
            .is_ok()
    } else {
        let mut out = io::stdout();
        out.write_all(sequence.as_bytes())
            .and_then(|_| out.flush())
            .is_ok()
    }
}

/// Whether frog runs inside WSL (where `clip.exe` reaches the Windows
/// clipboard even without an X/Wayland display). Cached after first check so
/// plain Linux doesn't re-read `/proc/sys/kernel/osrelease` on every copy.
#[cfg(target_os = "linux")]
fn is_wsl() -> bool {
    static WSL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *WSL.get_or_init(|| {
        std::env::var_os("WSL_DISTRO_NAME").is_some()
            || std::env::var_os("WSL_INTEROP").is_some()
            || std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .map(|s| {
                    let s = s.to_ascii_lowercase();
                    s.contains("microsoft") || s.contains("wsl")
                })
                .unwrap_or(false)
    })
}

#[cfg(not(target_os = "linux"))]
fn is_wsl() -> bool {
    false
}

/// Whether this process is attached to a remote SSH session (the OS clipboard
/// then belongs to the *remote* host, so OSC52 is what reaches the user).
fn is_ssh_session() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some()
}

/// Run one clipboard helper, feeding `text` on stdin.
///
/// Stdin is written on a short-lived thread so a large (>64 KB) copy to a
/// hung helper can never deadlock the TUI loop. When `allow_detach` is true
/// (for a user-configured `FROG_CLIPBOARD_CMD` such as a reverse SSH bridge),
/// a still-running helper after the 400 ms grace period is reaped in the
/// background and treated as success. For built-in local helpers, timing out
/// kills the child and returns `false` so the next fallback runs.
fn run_clipboard_command(program: &str, args: &[&str], text: &str, allow_detach: bool) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let bytes = text.as_bytes().to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
    }
    let deadline = Instant::now() + Duration::from_millis(400);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) if allow_detach => {
                // Slow custom bridge: let it finish in the background and reap
                // it so it doesn't linger as a zombie for the life of the TUI.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return true;
            }
            _ => {
                // Killing closes the pipe read end, unblocking the writer
                // thread immediately if it was stalled on a full pipe.
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Put `text` on the clipboard through an external helper and return the
/// helper's name on success.
///
/// This covers the places `arboard` cannot reach: WSL (`win32yank.exe` /
/// `clip.exe` talk to the Windows clipboard), a headless Linux box with an
/// X/Wayland helper installed, and user-supplied bridges (`FROG_CLIPBOARD_CMD`
/// or `defaults.clipboard_command`) such as a reverse SSH tunnel back to the
/// local machine.
fn write_via_command(custom_cmd: Option<&str>, text: &str) -> Option<String> {
    // Explicit override wins: a user-provided bridge can reach the real
    // clipboard in setups we cannot detect. If it fails, keep going so a
    // working built-in helper can still land the text.
    if let Some(custom) = custom_cmd.map(str::trim).filter(|s| !s.is_empty()) {
        #[cfg(windows)]
        let ok = run_clipboard_command("cmd", &["/C", custom], text, true);
        #[cfg(not(windows))]
        let ok = run_clipboard_command("sh", &["-c", custom], text, true);
        if ok {
            return Some("FROG_CLIPBOARD_CMD".to_string());
        }
    }

    // WSL/Windows interop: this reaches the Windows clipboard directly, even
    // when no display exists and mintty blocks OSC52. Prefer `win32yank.exe`
    // because it preserves UTF-8 (box-drawing chars, non-ASCII text), whereas
    // `clip.exe` decodes stdin using the Windows OEM/ANSI codepage.
    if cfg!(windows) || is_wsl() {
        let win_helpers: &[(&str, &[&str])] =
            &[("win32yank.exe", &["-i", "--crlf"]), ("clip.exe", &[])];
        for (program, args) in win_helpers {
            if run_clipboard_command(program, args, text, false) {
                return Some((*program).to_string());
            }
        }
    }

    // Desktop helpers, used when arboard cannot reach the display. Without a
    // display they can only fail, so don't pay for three doomed spawns.
    #[cfg(target_os = "linux")]
    let have_display =
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
    #[cfg(not(target_os = "linux"))]
    let have_display = true;
    if !have_display {
        return None;
    }

    #[cfg(target_os = "macos")]
    let candidates: &[(&str, &[&str])] = &[("pbcopy", &[])];
    #[cfg(target_os = "linux")]
    let candidates: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let candidates: &[(&str, &[&str])] = &[];
    for (program, args) in candidates {
        if run_clipboard_command(program, args, text, false) {
            return Some((*program).to_string());
        }
    }
    None
}

/// Copy `text` to the clipboard through every channel we have: the OS
/// clipboard, an external helper, and OSC52 (so it can reach the local
/// terminal over SSH).
fn copy_to_clipboard(
    clipboard: Option<&mut arboard::Clipboard>,
    custom_cmd: Option<&str>,
    text: &str,
) -> CopyOutcome {
    let os = clipboard
        .map(|cb| cb.set_text(text.to_string()).is_ok())
        .unwrap_or(false);
    // On WSL arboard may hit an X server while the real clipboard is the
    // Windows one, so prefer the helper there. Otherwise only fall back to a
    // helper when arboard failed (or when the user configured one).
    let explicit_command = custom_cmd.map(|s| !s.trim().is_empty()).unwrap_or(false);
    let command = if os && !is_wsl() && !explicit_command {
        None
    } else {
        write_via_command(custom_cmd, text)
    };
    // Always offer OSC52 as well: when arboard targets a *remote* X server
    // (X forwarding) it won't reach the local Windows clipboard, while OSC52
    // will. Harmless locally.
    let osc52 = write_osc52(text);
    CopyOutcome { os, command, osc52 }
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
    tabs_area: Rect,
    editor_area: Rect,
    results_area: Rect,
    sidebar_area: Rect,
    /// True while the horizontal editor/results splitter is being dragged.
    resizing_editor: bool,
    /// Two-step tab close: first Ctrl+W arms `(tab_idx, time)`, second
    /// within 5s actually closes. Prevents losing SQL on a stray keypress.
    pending_close_tab: Option<(usize, Instant)>,
    explorer_area: Rect,
    explorer_detail_area: Rect,
    /// F12 DB explorer: which pane has focus (Tab toggles Tree/Detail).
    explorer_focus: ExplorerPane,
    /// F12 DB explorer: only the focused pane fills the view
    /// (Ctrl+Z / F11 toggles, same as the query tab and F2).
    explorer_maximized: bool,
    /// F12 DB explorer: tree share of the split as a % (25..85).
    explorer_split_pct: u16,
    /// F2 session viewer: top list height as % of the view (15..85).
    /// Grown/shrunk with Alt+Up/Down while the List pane is focused
    /// (Ctrl+Z zooms any pane like the query tab).
    session_list_pct: u16,
    /// F2 session viewer: middle SQL pane height in rows (3..30).
    /// Grown/shrunk with Alt+Up/Down while SQL (or Plan, inversely) is
    /// focused; Ctrl+Up/Down always targets SQL directly as an alias.
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
    /// F6 Performance dashboard: focused pane (`Queries`, `Results`, `Sql`).
    perf_focus: PerfPane,
    /// F6 Performance dashboard: whether the focused pane is maximized (`Ctrl+Z`/`F11`).
    perf_maximized: bool,
    /// F6 Performance dashboard: left query list width share as % (22..65).
    perf_split_pct: u16,
    /// Last rendered F6 pane rectangles (for mouse click-to-focus / wheel scroll).
    perf_queries_area: Rect,
    perf_results_area: Rect,
    perf_sql_area: Rect,
    /// Last rendered F4 DDL area.
    ddl_area: Rect,
    /// Last rendered F1/startup Help area.
    help_area: Rect,
    /// Last rendered F3 History area.
    history_area: Rect,
    command_mode: bool,
    command_input: String,
    command_error: Option<String>,
    /// When set, the command bar is in confirm mode for this @file command.
    command_confirm: Option<String>,
    /// Whether a left-button editor drag is currently building a selection.
    selecting_editor: bool,
    /// Whether a left-button table drag is currently building a cell selection.
    selecting_table: bool,
    /// Frog's own copy buffer: every in-app copy lands here too, so paste
    /// works even when the OS clipboard is unreachable. Typical over SSH:
    /// the copy rides OSC52 to the *local* terminal, but arboard can only
    /// read the *remote* clipboard — without this fallback, a table just
    /// copied could never be pasted back.
    copy_buffer: String,
    /// Long-lived arboard handle. On X11/Wayland the clipboard content is
    /// served by a background thread owned by this instance; dropping it
    /// right after `set_text` makes the selection vanish (unless a clipboard
    /// manager happens to save it), so keep it alive for the whole session.
    clipboard: Option<arboard::Clipboard>,
    /// Table geometry from the last render (for mouse hit-testing).
    results_layout: ResultsLayout,
    /// Whether frog captures mouse events. Toggled with Ctrl+M / Alt+M
    /// (tmux-style): while OFF, the terminal handles selection/copy natively
    /// and pastes land via bracketed paste (or plain keys on terminals like
    /// git-bash/mintty that don't emit bracketed paste).
    mouse_capture: bool,
    /// Whether the Kitty keyboard protocol was actually enabled. Only true
    /// when the terminal reports enhancement support — otherwise Ctrl+M /
    /// Ctrl+Enter stay ambiguous with Enter and need their Alt- fallbacks.
    keyboard_enhanced: bool,
    /// Emacs-style incremental search (Ctrl-S). `None` when inactive.
    search: Option<SearchState>,
    /// Last non-empty search query, reused when starting a search and
    /// immediately pressing Ctrl-S again with an empty query.
    last_search_query: String,
    /// Set on terminal resize: the next frame does a full clear first so
    /// no stale cells survive (some terminals/proxies drop the diff).
    needs_clear: bool,
    /// Last rendered fullscreen view. When it changes, the content area is
    /// cleared first so a shorter view can't leave ghost characters from
    /// the previous frame behind.
    last_view_key: Option<(SessionMode, u8)>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExplorerPane {
    #[default]
    Tree,
    Detail,
}

impl ExplorerPane {
    pub fn next(self) -> Self {
        match self {
            ExplorerPane::Tree => ExplorerPane::Detail,
            ExplorerPane::Detail => ExplorerPane::Tree,
        }
    }

    pub fn prev(self) -> Self {
        // Two panes: previous is the same as next.
        self.next()
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

/// Which content a contextual copy (Ctrl+Y) grabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyTarget {
    Results,
    TableSelection,
    SqlSelection,
    SqlStatement,
}

/// Contextual copy dispatch: the focused pane decides. Results (and
/// Sidebar) prefer an active table drag-selection, else the full table;
/// the Editor prefers the drag-selection, else the statement at the
/// cursor (with several statements, only the one under the cursor).
/// Pure, so it can be unit-tested.
fn copy_target_for(
    focus: ActiveWindow,
    editor_selection: bool,
    table_selection: bool,
) -> CopyTarget {
    match focus {
        ActiveWindow::Editor if editor_selection => CopyTarget::SqlSelection,
        ActiveWindow::Editor => CopyTarget::SqlStatement,
        _ if table_selection => CopyTarget::TableSelection,
        _ => CopyTarget::Results,
    }
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
            tabs_area: Rect::default(),
            editor_area: Rect::default(),
            results_area: Rect::default(),
            sidebar_area: Rect::default(),
            resizing_editor: false,
            pending_close_tab: None,
            explorer_area: Rect::default(),
            explorer_detail_area: Rect::default(),
            explorer_focus: ExplorerPane::Tree,
            explorer_maximized: false,
            explorer_split_pct: 55,
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
            perf_focus: PerfPane::Queries,
            perf_maximized: false,
            perf_split_pct: 32,
            perf_queries_area: Rect::default(),
            perf_results_area: Rect::default(),
            perf_sql_area: Rect::default(),
            ddl_area: Rect::default(),
            help_area: Rect::default(),
            history_area: Rect::default(),
            command_mode: false,
            command_input: String::new(),
            command_error: None,
            command_confirm: None,
            selecting_editor: false,
            selecting_table: false,
            copy_buffer: String::new(),
            clipboard: arboard::Clipboard::new().ok(),
            results_layout: ResultsLayout::default(),
            mouse_capture: true,
            keyboard_enhanced: false,
            search: None,
            last_search_query: String::new(),
            needs_clear: false,
            last_view_key: None,
        })
    }

    pub fn run(&mut self) -> anyhow::Result<()> {
        enable_raw_mode()?;
        // Only request Kitty keyboard enhancements when the terminal claims
        // support — mintty/git-bash and proxies don't, and pushing
        // unconditionally leaves Ctrl+M / Ctrl+Enter ambiguous anyway.
        // (crossterm docs: only kitty/foot/WezTerm/Alacritty et al. support it.)
        self.keyboard_enhanced = matches!(
            crossterm::terminal::supports_keyboard_enhancement(),
            Ok(true)
        );
        let kb_enhanced = self.keyboard_enhanced;
        let _guard = TerminalGuard {
            keyboard_enhanced: kb_enhanced,
        };
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal_state(kb_enhanced);
            prev_hook(info);
        }));

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
        let _ = terminal.show_cursor();
        // `_guard` restores raw mode, alternate screen, mouse, paste, and keyboard flags on drop.
        res
    }

    fn run_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> anyhow::Result<()> {
        while !self.should_quit {
            self.session_manager.poll_result();
            self.session_manager.poll_explorer();
            self.session_manager.poll_ddl();
            self.session_manager.poll_perf();
            self.session_manager.poll_f2();
            self.session_manager.poll_cat();
            self.session_manager.poll_conn_result();
            self.session_manager.poll_metadata();
            self.session_manager.check_connections();
            self.session_manager.tick_statuses();
            self.session_manager.tick_session_view();
            self.session_manager.tick_perf();

            terminal.draw(|f| self.ui(f))?;

            // Terminals behind jump proxies don't always report resizes, and
            // dropped escape sequences can leave ghost cells behind: a full
            // clear forces the next frame to repaint everything.
            if self.needs_clear {
                let _ = terminal.clear();
                self.needs_clear = false;
                terminal.draw(|f| self.ui(f))?;
            }

            // git-bash/mintty doesn't emit bracketed paste — pastes arrive as
            // a burst of individual key events. When a text key lands, collect
            // everything queued plus a short 2ms tail (SSH may split the write)
            // so the batch handler can reassemble it into one paste. Non-text
            // events (mouse drags/moves, shortcuts) drain immediately with 0ms
            // wait so a high-polling-rate mouse can never starve rendering.
            if event::poll(Duration::from_millis(50))? {
                let first = event::read()?;
                let tail = match first {
                    Event::Key(k) if text_of_key(k).is_some() => Duration::from_millis(2),
                    _ => Duration::from_millis(0),
                };
                let mut batch = vec![first];
                let deadline = Instant::now() + Duration::from_millis(50);
                while batch.len() < 4096 && Instant::now() < deadline && event::poll(tail)? {
                    batch.push(event::read()?);
                }
                self.handle_events(batch);
            }
        }
        Ok(())
    }

    fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Key(key) => self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Resize(_, _) => {
                // Window moves/resizes on some terminals (git-bash/mintty
                // behind a jump proxy) arrive as bare resizes or not at
                // all; force a full repaint so stale cells can't linger.
                self.needs_clear = true;
            }
            Event::Paste(text) => {
                if self.search.is_some() {
                    self.search_push_text(&text);
                } else {
                    self.paste_text(&text)
                }
            }
            _ => {}
        }
    }

    /// Handle all events read in one loop tick. When a terminal (or a proxy)
    /// lacks bracketed paste, a paste arrives as a burst of plain key events.
    /// Reassemble that burst and run it through the normal paste path, so a
    /// multi-line paste can't execute a statement line by line and a pasted
    /// tab can't cycle focus. Anything that isn't plain text (a shortcut,
    /// mouse event, key release, ...) falls back to per-event handling.
    fn handle_events(&mut self, events: Vec<Event>) {
        // While searching, a key burst is query text, not an editor paste.
        if self.search.is_some() && events.len() > 1 {
            if let Some(text) = text_from_key_burst(&events) {
                self.search_push_text(&text);
                return;
            }
        }
        if events.len() > 1 && self.paste_target_is_text() {
            if let Some(text) = text_from_key_burst(&events) {
                self.paste_text(&text);
                return;
            }
        }
        for ev in events {
            self.handle_event(ev);
        }
    }

    /// Whether the current focus can receive pasted text (command bar, SQL
    /// editor or connection dialog). Other fullscreen views and confirmation
    /// prompts are not text-input targets.
    fn paste_target_is_text(&self) -> bool {
        if self.show_startup_help {
            return false;
        }
        if self.command_mode {
            return self.command_confirm.is_none();
        }
        matches!(
            self.session_manager.active_session().mode,
            SessionMode::Query | SessionMode::ConnectionPickerDialog
        )
    }

    /// Insert pasted text into the active target (command bar, connection
    /// dialog field or SQL editor). Normalizes Windows (`\r\n`) and lone-`\r`
    /// (git-bash sends CR-only line breaks) to `\n`, and expands tabs to
    /// spaces (a literal Tab can't be typed — it cycles focus — and
    /// `insert_char` drops control chars, so pasted indentation would vanish).
    fn paste_text(&mut self, text: &str) {
        // Pasted text changes the word under the popup; close it rather than
        // accept into a range computed before the paste.
        self.session_manager.close_completion();
        // Normalize once so `\r\n` becomes a single newline (not two) and a
        // lone `\r` still breaks the line instead of joining two lines.
        let normalized = normalize_paste(text);
        if self.command_mode {
            self.command_input
                .extend(
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
            // A paste always lands in the editor: move focus there so the
            // caret is visible and following keystrokes aren't dropped by a
            // read-only pane (e.g. pasting while the results table has focus).
            self.active_window = ActiveWindow::Editor;
            // Pasting replaces the drag-selection, like a GUI editor.
            session.delete_selection();
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
        if self.mouse_capture {
            4
        } else {
            0
        }
    }

    /// Toggle mouse capture (tmux-style). While OFF the terminal handles text
    /// selection/copy natively; bracketed paste still reaches frog.
    /// Bound to both Ctrl+M (needs Kitty CSI-u) and Alt+M (plain ESC m,
    /// survives git-bash/mintty and jump-host proxies).
    fn toggle_mouse_capture(&mut self) {
        use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
        self.mouse_capture = !self.mouse_capture;
        // Frog-side drag-selections can't survive the mode switch (no more
        // drag events in one mode, double highlight in the other).
        self.selecting_editor = false;
        self.selecting_table = false;
        {
            let s = self.session_manager.active_session_mut();
            s.editor_selection = None;
            s.table_selection = None;
        }
        let mut stdout = io::stdout();
        let _ = if self.mouse_capture {
            execute!(stdout, EnableMouseCapture)
        } else {
            execute!(stdout, DisableMouseCapture)
        };
        let msg = if self.mouse_capture {
            "Mouse capture ON (Alt+M for native copy)"
        } else {
            "Mouse OFF — borderless editor, drag to select clean SQL; Ctrl+V / Shift+Ins pastes, Alt+M restores"
        };
        self.session_manager.active_session_mut().set_status(msg);
    }

    /// Clamp the active session's result scroll offsets to the loaded rows
    /// and columns. Runs every frame before the result viewer draws, so all
    /// three formats (Table/Markdown/ASCII) stay in range even after
    /// overscroll, a format switch, or shrinking data.
    fn clamp_result_scroll(&mut self) {
        let (total_rows, total_cols) =
            match self.session_manager.active_session().results.last() {
                Some(qr) if !qr.is_error => (qr.rows.len(), qr.columns.len()),
                _ => return,
            };
        let s = self.session_manager.active_session_mut();
        let (row, col) =
            clamp_scroll_offsets(s.scroll_offset, s.col_scroll_offset, total_rows, total_cols);
        s.scroll_offset = row;
        s.col_scroll_offset = col;
    }

    /// Copy `text` to every clipboard we have (OS + helpers + OSC52) and
    /// remember it in the internal buffer so Ctrl+V works even when the OS
    /// clipboard is unreachable (typical over SSH).
    fn copy_and_remember(&mut self, text: &str) -> CopyOutcome {
        let outcome = copy_to_clipboard(
            self.clipboard.as_mut(),
            self.config.clipboard_command.as_deref(),
            text,
        );
        self.copy_buffer = text.to_string();
        outcome
    }

    /// Status text for a clipboard copy outcome (shared by Ctrl+Y / Alt+Y).
    fn copy_status(outcome: CopyOutcome, chars: usize, what: &str) -> String {
        Self::copy_status_for(outcome, chars, what, is_ssh_session())
    }

    fn copy_status_for(outcome: CopyOutcome, chars: usize, what: &str, ssh: bool) -> String {
        if let Some(helper) = outcome.command.as_deref() {
            return format!(
                "Copied {} ({} chars) to clipboard via {}",
                what, chars, helper
            );
        }
        // Over SSH the OS clipboard belongs to the remote host; OSC52 is the
        // channel that can reach the user's terminal, so report OSC52 first
        // over SSH (and whenever no OS clipboard succeeded locally).
        if outcome.osc52 && (ssh || !outcome.os) {
            let hint = if ssh {
                "if paste is empty, Shift+drag to copy natively (mintty: AllowSetSelection)"
            } else {
                "if paste is empty, Shift+drag or enable OSC52 (mintty: AllowSetSelection; tmux: set-clipboard on)"
            };
            return format!("Copied {} ({} chars) via OSC52 — {}", what, chars, hint);
        }
        if outcome.os {
            return format!("Copied {} ({} chars) to clipboard", what, chars);
        }
        format!(
            "Copy failed ({} chars): no OS clipboard, helper or OSC52 — use Alt+M (Mouse:OFF) + terminal selection",
            chars
        )
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
                    let refs: Vec<&[String]> = last_res.rows.iter().map(|r| r.as_slice()).collect();
                    text.push_str(&markdown_table(&last_res.columns, &refs));
                }
                ResultFormat::Ascii => {
                    let full = 0..last_res.columns.len();
                    text.push_str(&format_ascii_table(last_res, full, 0, last_res.rows.len()));
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
        let outcome = self.copy_and_remember(&text);
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
        let outcome = self.copy_and_remember(&text);
        let msg = Self::copy_status(outcome, chars, "SQL");
        self.session_manager.active_session_mut().set_status(msg);
    }

    /// Read the Linux primary selection (middle-click clipboard), falling back
    /// to the regular clipboard.
    #[cfg(target_os = "linux")]
    fn read_primary_selection(clipboard: Option<&mut arboard::Clipboard>) -> Option<String> {
        use arboard::{GetExtLinux, LinuxClipboardKind};
        let cb = clipboard?;
        if let Ok(text) = cb.get().clipboard(LinuxClipboardKind::Primary).text() {
            if !text.is_empty() {
                return Some(text);
            }
        }
        cb.get().text().ok().filter(|t| !t.is_empty())
    }

    #[cfg(not(target_os = "linux"))]
    fn read_primary_selection(clipboard: Option<&mut arboard::Clipboard>) -> Option<String> {
        clipboard?.get().text().ok().filter(|t| !t.is_empty())
    }

    /// Whether `key` toggles mouse capture. Ctrl+M needs the Kitty CSI-u
    /// protocol; Alt+M (plain `ESC m`) survives git-bash/mintty and proxies,
    /// so both are accepted (case-insensitive, Shift-tolerant).
    fn is_mouse_toggle(key: event::KeyEvent) -> bool {
        if !matches!(key.code, KeyCode::Char('m') | KeyCode::Char('M')) {
            return false;
        }
        key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::ALT)
    }

    /// Explicit keyboard paste: Ctrl+V (incl. Ctrl+Shift+V, which arrives as
    /// Ctrl+V / Ctrl+Shift+V) and Shift+Insert read the OS clipboard and
    /// insert it via the same path as bracketed paste. Works with mouse
    /// capture ON or OFF — the easy-paste counterpart to Mouse:OFF copy.
    fn is_paste_key(key: event::KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
        {
            return true;
        }
        matches!(key.code, KeyCode::Insert)
    }

    /// Paste the clipboard content at the active target. Prefers the live
    /// OS clipboard (so externally copied text wins); falls back to
    /// frog's own copy buffer — without this, anything copied over SSH
    /// (OSC52 → local terminal) could never be pasted back, because
    /// arboard only sees the *remote* clipboard there.
    fn paste_from_clipboard(&mut self) {
        // While incremental search is active the clipboard lands in the
        // search query, not the editor.
        if self.search.is_some() {
            let text = self
                .clipboard
                .as_mut()
                .and_then(|cb| cb.get().text().ok())
                .filter(|t| !t.is_empty())
                .or_else(|| {
                    if self.copy_buffer.is_empty() {
                        None
                    } else {
                        Some(self.copy_buffer.clone())
                    }
                });
            match text {
                Some(t) => self.search_push_text(&t),
                None => self
                    .session_manager
                    .active_session_mut()
                    .set_status("Clipboard empty/unavailable"),
            }
            return;
        }
        let text = self
            .clipboard
            .as_mut()
            .and_then(|cb| cb.get().text().ok())
            .filter(|t| !t.is_empty())
            .or_else(|| {
                if self.copy_buffer.is_empty() {
                    None
                } else {
                    Some(self.copy_buffer.clone())
                }
            });
        match text {
            Some(t) => {
                if self.session_manager.active_session().mode == SessionMode::Query {
                    self.active_window = ActiveWindow::Editor;
                }
                self.paste_text(&t)
            }
            None => self
                .session_manager
                .active_session_mut()
                .set_status("Clipboard empty/unavailable — use Shift+Ins or bracketed paste"),
        }
    }

    // ================= Incremental search (Emacs Ctrl-S) =================

    /// Which buffer the search applies to, from the current mode + focus.
    fn search_target_for_current_view(&self) -> SearchTarget {
        let session = self.session_manager.active_session();
        match session.mode {
            SessionMode::Query => match self.active_window {
                ActiveWindow::Editor => SearchTarget::Editor,
                ActiveWindow::Results => SearchTarget::Results,
                ActiveWindow::Sidebar => SearchTarget::Sidebar,
            },
            SessionMode::SessionView => match self.session_view_focus {
                SessionViewPane::List => SearchTarget::SessionList,
                SessionViewPane::Sql => SearchTarget::SessionSql,
                SessionViewPane::Plan => SearchTarget::SessionPlan,
            },
            SessionMode::DbExplorer => match self.explorer_focus {
                ExplorerPane::Tree => SearchTarget::ExplorerTree,
                ExplorerPane::Detail => SearchTarget::ExplorerDetail,
            },
            SessionMode::ObjectDdl => SearchTarget::Ddl,
            SessionMode::Performance => match self.perf_focus {
                PerfPane::Queries => SearchTarget::PerfQueries,
                PerfPane::Results => SearchTarget::PerfResults,
                PerfPane::Sql => SearchTarget::PerfSql,
            },
            SessionMode::History => SearchTarget::History,
            SessionMode::Help => SearchTarget::Help,
            SessionMode::ConnectionPickerDialog => SearchTarget::None,
        }
    }

    fn search_origin_snapshot(&self) -> SearchOrigin {
        let s = self.session_manager.active_session();
        SearchOrigin {
            editor_row: s.editor.cursor_row,
            editor_col: s.editor.cursor_col,
            editor_scroll: s.editor.scroll_offset,
            results_scroll: s.scroll_offset,
            results_col_scroll: s.col_scroll_offset,
            explorer_cursor: s.explorer_cursor,
            explorer_scroll: s.explorer_scroll,
            explorer_detail_scroll: s.explorer_detail_scroll,
            explorer_detail_col_scroll: s.explorer_detail_col_scroll,
            session_cursor: s.session_view_cursor,
            sql_scroll: s.sql_scroll,
            plan_scroll: s.plan_scroll,
            plan_hscroll: s.plan_hscroll,
            ddl_scroll: s.ddl_scroll,
            ddl_hscroll: s.ddl_hscroll,
            perf_cursor: s.perf_cursor,
            perf_scroll: s.perf_scroll,
            perf_col_scroll: s.perf_col_scroll,
            perf_sql_scroll: s.perf_sql_scroll,
            history_cursor: s.history_cursor,
            history_scroll: s.history_scroll,
            help_scroll: s.help_scroll,
        }
    }

    fn restore_search_origin(&mut self, origin: &SearchOrigin) {
        let s = self.session_manager.active_session_mut();
        s.editor.cursor_row = origin.editor_row.min(s.editor.lines.len().saturating_sub(1));
        s.editor.cursor_col = origin.editor_col;
        s.editor.snap_cursor_to_char_boundary();
        s.editor.scroll_offset = origin.editor_scroll;
        s.scroll_offset = origin.results_scroll;
        s.col_scroll_offset = origin.results_col_scroll;
        s.explorer_cursor = origin.explorer_cursor;
        s.explorer_scroll = origin.explorer_scroll;
        s.explorer_detail_scroll = origin.explorer_detail_scroll;
        s.explorer_detail_col_scroll = origin.explorer_detail_col_scroll;
        s.session_view_cursor = origin.session_cursor;
        s.sql_scroll = origin.sql_scroll;
        s.plan_scroll = origin.plan_scroll;
        s.plan_hscroll = origin.plan_hscroll;
        s.ddl_scroll = origin.ddl_scroll;
        s.ddl_hscroll = origin.ddl_hscroll;
        s.perf_cursor = origin.perf_cursor;
        s.perf_scroll = origin.perf_scroll;
        s.perf_col_scroll = origin.perf_col_scroll;
        s.perf_sql_scroll = origin.perf_sql_scroll;
        s.history_cursor = origin.history_cursor;
        s.history_scroll = origin.history_scroll;
        s.help_scroll = origin.help_scroll;
    }

    /// Start Emacs-style incremental search in the current view.
    /// Any in-progress completion popup is closed so typing goes to the query.
    fn search_start(&mut self) {
        let target = self.search_target_for_current_view();
        if target == SearchTarget::None {
            self.session_manager
                .active_session_mut()
                .set_status("Search not available in this dialog");
            return;
        }
        self.session_manager.close_completion();
        // Any keystroke exits command-bar confirmations; search takes over.
        self.command_mode = false;
        self.command_confirm = None;
        let origin = self.search_origin_snapshot();
        let mut st = SearchState::new(target, origin);
        // Emacs nicety: a bare `Ctrl-S Ctrl-S` repeats the last query.
        if self.last_search_query.is_empty() {
            st.query.clear();
        } else {
            // Start empty; `search_next` with an empty query restores it.
            st.query.clear();
        }
        st.direction = SearchDirection::Forward;
        self.search = Some(st);
        self.search_recompute(true);
    }

    /// Rebuild the match list for the current query and jump to the
    /// appropriate match. `from_origin` jumps from the saved cursor
    /// position (first keystroke); otherwise keeps the current index
    /// clamped into the new list.
    fn search_recompute(&mut self, from_origin: bool) {
        let (target, query, direction, origin_editor) = match self.search.as_ref() {
            Some(st) => (
                st.target,
                st.query.clone(),
                st.direction,
                (st.origin.editor_row, st.origin.editor_col),
            ),
            None => return,
        };
        if query.is_empty() {
            if let Some(st) = self.search.as_mut() {
                st.matches.clear();
                st.current = 0;
                st.failing = false;
                st.wrapped = false;
            }
            return;
        }
        let matches = self.search_compute_matches(target, &query);
        let failing = matches.is_empty();
        // Compute the anchor index without holding the mutable borrow.
        let anchor = Self::search_anchor_index_static(&matches, target, direction, origin_editor);
        if let Some(st) = self.search.as_mut() {
            st.matches = matches;
            st.failing = failing;
            if failing {
                st.current = 0;
                st.wrapped = false;
            } else if from_origin {
                // Jump to the first match at/after the origin position for
                // the editor (true Emacs semantics); other views jump to
                // the first (forward) or last (backward) match.
                st.current = anchor;
                st.wrapped = false;
            } else {
                st.current = st.current.min(st.matches.len().saturating_sub(1));
            }
            if !self.search.as_ref().unwrap().query.is_empty() {
                self.last_search_query = self.search.as_ref().unwrap().query.clone();
            }
        }
        if !failing {
            let idx = self.search.as_ref().map(|s| s.current).unwrap_or(0);
            self.search_goto(idx);
        }
    }

    /// Index to jump to on the first keystroke: editor honors the origin
    /// cursor (next match forward / previous backward), every other view
    /// starts at the first (forward) or last (backward) match.
    fn search_anchor_index_static(
        matches: &[SearchMatch],
        target: SearchTarget,
        direction: SearchDirection,
        origin_editor: (usize, usize),
    ) -> usize {
        if matches.is_empty() {
            return 0;
        }
        let forward = direction == SearchDirection::Forward;
        match target {
            SearchTarget::Editor => {
                crate::tui::search::anchor_index_editor(
                    matches,
                    origin_editor.0,
                    origin_editor.1,
                    forward,
                )
                .unwrap_or(0)
            }
            _ => {
                if forward {
                    0
                } else {
                    matches.len() - 1
                }
            }
        }
    }

    /// Build the match list for `target` + `query` from live session data.
    fn search_compute_matches(
        &self,
        target: SearchTarget,
        query: &str,
    ) -> Vec<SearchMatch> {
        if query.is_empty() {
            return Vec::new();
        }
        let session = self.session_manager.active_session();
        match target {
            SearchTarget::Editor => find_editor_matches(&session.editor.lines, query),
            SearchTarget::Results => match session.results.last() {
                Some(qr) if !qr.is_error => {
                    find_table_matches(&qr.columns, &qr.rows, query)
                }
                _ => Vec::new(),
            },
            SearchTarget::Sidebar => {
                let labels = if session.is_connected {
                    if let Some(ref p) = session.connected_params {
                        vec![
                            format!("{:?}", p.db_type),
                            p.host.clone(),
                            p.service.clone(),
                            p.database.clone(),
                            p.user.clone(),
                            "Connected".to_string(),
                        ]
                    } else {
                        vec![
                            format!("{:?}", session.conn_dialog.db_type),
                            session.conn_dialog.host.clone(),
                            session.conn_dialog.service.clone(),
                            session.conn_dialog.database.clone(),
                            session.conn_dialog.user.clone(),
                            "Connected".to_string(),
                        ]
                    }
                } else {
                    vec![
                        format!("{:?}", session.conn_dialog.db_type),
                        session.conn_dialog.host.clone(),
                        session.conn_dialog.service.clone(),
                        session.conn_dialog.database.clone(),
                        session.conn_dialog.user.clone(),
                        "Disconnected".to_string(),
                    ]
                };
                find_list_matches(&labels, query)
            }
            SearchTarget::ExplorerTree => {
                let rows = self.session_manager.explorer_visible_rows();
                let labels: Vec<String> = rows
                    .iter()
                    .map(|r| self.session_manager.explorer_row_search_label(r))
                    .collect();
                find_list_matches(&labels, query)
            }
            SearchTarget::ExplorerDetail => self.search_explorer_detail_matches(query),
            SearchTarget::SessionList => {
                let list = self.session_manager.visible_session_list();
                let labels: Vec<String> = list
                    .iter()
                    .map(|d| {
                        format!(
                            "{} {} {} {} {} {} {} {}",
                            d.sid,
                            d.serial,
                            d.username,
                            d.status,
                            d.machine,
                            d.program,
                            d.sql_id,
                            d.logon_time
                        )
                    })
                    .collect();
                find_list_matches(&labels, query)
            }
            SearchTarget::SessionSql => {
                let text = session
                    .plan_sql_text
                    .as_deref()
                    .unwrap_or_default();
                let lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
                let lines = if lines.is_empty() {
                    vec![String::new()]
                } else {
                    lines
                };
                find_line_matches(&lines, query)
            }
            SearchTarget::SessionPlan => {
                let mut lines: Vec<String> = Vec::new();
                if let Some(qr) = session.plan_result.as_ref() {
                    if !qr.is_error {
                        if !qr.columns.is_empty() {
                            lines.push(qr.columns.join(" | "));
                        }
                        for row in &qr.rows {
                            lines.push(row.join(" | "));
                        }
                    } else if let Some(e) = qr.error_msg.as_deref() {
                        lines.extend(e.lines().map(|s| s.to_string()));
                    }
                }
                if lines.is_empty() {
                    return Vec::new();
                }
                find_line_matches(&lines, query)
            }
            SearchTarget::Ddl => match session.ddl_object.as_ref() {
                Some(obj) => {
                    let lines: Vec<String> =
                        obj.ddl.lines().map(|s| s.to_string()).collect();
                    find_line_matches(&lines, query)
                }
                None => Vec::new(),
            },
            SearchTarget::PerfQueries => {
                let specs = crate::db::perf::perf_queries_for(
                    self.session_manager.active_db_type(),
                );
                let labels: Vec<String> = specs
                    .iter()
                    .map(|q| format!("{} {}", q.title, q.sql))
                    .collect();
                find_list_matches(&labels, query)
            }
            SearchTarget::PerfResults => match session.perf_results.get(&session.perf_cursor) {
                Some(qr) if !qr.is_error => {
                    find_table_matches(&qr.columns, &qr.rows, query)
                }
                _ => Vec::new(),
            },
            SearchTarget::PerfSql => {
                let specs = crate::db::perf::perf_queries_for(
                    self.session_manager.active_db_type(),
                );
                let sql = specs
                    .get(session.perf_cursor)
                    .map(|q| q.sql)
                    .unwrap_or_default();
                let lines: Vec<String> = sql.lines().map(|s| s.to_string()).collect();
                find_line_matches(&lines, query)
            }
            SearchTarget::History => {
                let labels = session.query_history.clone();
                find_list_matches(&labels, query)
            }
            SearchTarget::Help => {
                let labels = crate::tui::widgets::help_search_lines();
                find_list_matches(&labels, query)
            }
            SearchTarget::None => Vec::new(),
        }
    }

    fn search_explorer_detail_matches(&self, query: &str) -> Vec<SearchMatch> {
        let session = self.session_manager.active_session();
        let rows = self.session_manager.explorer_visible_rows();
        let row = match rows.get(session.explorer_cursor) {
            Some(r) => r.clone(),
            None => return Vec::new(),
        };
        // Row-bearing object with a loaded preview: search its table.
        if let crate::db::session_manager::ExplorerRow::Table { sidx, gidx, tidx } = row {
            let preview = session
                .explorer_schemas
                .get(sidx)
                .and_then(|s| s.groups.as_ref())
                .and_then(|g| g.get(gidx))
                .and_then(|g| g.objects.as_ref())
                .and_then(|o| o.get(tidx))
                .and_then(|t| t.preview.as_ref());
            if let Some(qr) = preview {
                if !qr.is_error {
                    return find_table_matches(&qr.columns, &qr.rows, query);
                }
            }
            // Code object with loaded DDL/source: search its lines.
            let ddl = session
                .explorer_schemas
                .get(sidx)
                .and_then(|s| s.groups.as_ref())
                .and_then(|g| g.get(gidx))
                .and_then(|g| g.objects.as_ref())
                .and_then(|o| o.get(tidx))
                .and_then(|t| t.ddl.as_ref());
            if let Some(text) = ddl {
                let lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
                return find_line_matches(&lines, query);
            }
        }
        // Fallback: search the detail pane's text lines (schema/group info).
        let labels = vec![self.session_manager.explorer_row_search_label(&row)];
        find_list_matches(&labels, query)
    }

    /// Move the view to match `idx` (with wrapping handled by callers).
    fn search_goto(&mut self, idx: usize) {
        let (target, m) = match self.search.as_ref() {
            Some(st) if !st.matches.is_empty() => {
                let i = idx.min(st.matches.len() - 1);
                (st.target, st.matches[i])
            }
            _ => return,
        };
        // Update current first so highlight follows even if scroll clamps.
        if let Some(st) = self.search.as_mut() {
            st.current = idx.min(st.matches.len().saturating_sub(1));
        }
        let help_max = self.help_max_scroll();
        let session = self.session_manager.active_session_mut();
        match (target, m) {
            (SearchTarget::Editor, SearchMatch::Editor { row, col, .. }) => {
                session.editor.cursor_row = row.min(session.editor.lines.len().saturating_sub(1));
                session.editor.cursor_col = col;
                session.editor.snap_cursor_to_char_boundary();
                session.editor_selection = None;
            }
            (SearchTarget::Results, SearchMatch::TableCell { row, col }) => {
                match row {
                    None => {
                        session.scroll_offset = 0;
                        session.col_scroll_offset = col;
                    }
                    Some(r) => {
                        session.scroll_offset = r;
                        session.col_scroll_offset = col;
                    }
                }
                // Keep an old table drag-selection from hiding the match.
                session.table_selection = None;
            }
            (SearchTarget::ExplorerTree, SearchMatch::ListRow { row }) => {
                session.explorer_cursor = row;
            }
            (SearchTarget::ExplorerDetail, SearchMatch::TableCell { row, col }) => {
                match row {
                    None => {
                        session.explorer_detail_scroll = 0;
                        session.explorer_detail_col_scroll = col;
                    }
                    Some(r) => {
                        session.explorer_detail_scroll = r;
                        session.explorer_detail_col_scroll = col;
                    }
                }
            }
            (SearchTarget::ExplorerDetail, SearchMatch::ListRow { .. }) => {
                // Single-label detail: nothing to scroll, match shown in bar.
            }
            (SearchTarget::ExplorerDetail, SearchMatch::Line { line, col, .. }) => {
                session.explorer_detail_scroll = line;
                session.explorer_detail_col_scroll = col / 8;
            }
            (SearchTarget::SessionList, SearchMatch::ListRow { row }) => {
                // `visible_session_list` order matches cursor order.
                session.session_view_cursor = row;
                self.session_view_focus = SessionViewPane::List;
            }
            (SearchTarget::SessionSql, SearchMatch::Line { line, .. }) => {
                session.sql_scroll = line;
                self.session_view_focus = SessionViewPane::Sql;
            }
            (SearchTarget::SessionPlan, SearchMatch::Line { line, col, .. }) => {
                session.plan_scroll = line;
                session.plan_hscroll = col / 2;
                self.session_view_focus = SessionViewPane::Plan;
            }
            (SearchTarget::Ddl, SearchMatch::Line { line, col, .. }) => {
                session.ddl_scroll = line;
                session.ddl_hscroll = col / 2;
            }
            (SearchTarget::PerfQueries, SearchMatch::ListRow { row }) => {
                session.perf_cursor = row;
                self.perf_focus = PerfPane::Queries;
            }
            (SearchTarget::PerfResults, SearchMatch::TableCell { row, col }) => {
                match row {
                    None => {
                        session.perf_scroll = 0;
                        session.perf_col_scroll = col;
                    }
                    Some(r) => {
                        session.perf_scroll = r;
                        session.perf_col_scroll = col;
                    }
                }
                self.perf_focus = PerfPane::Results;
            }
            (SearchTarget::PerfSql, SearchMatch::Line { line, .. }) => {
                session.perf_sql_scroll = line;
                self.perf_focus = PerfPane::Sql;
            }
            (SearchTarget::History, SearchMatch::ListRow { row }) => {
                session.history_cursor = row;
            }
            (SearchTarget::Help, SearchMatch::ListRow { row }) => {
                session.help_scroll = row.min(help_max);
            }
            // Sidebar / fallback: count only, nothing to move.
            _ => {}
        }
    }

    fn search_next(&mut self, forward: bool) {
        // Empty query + repeated Ctrl-S reuses the last search (Emacs-style).
        if self
            .search
            .as_ref()
            .map(|s| s.query.is_empty())
            .unwrap_or(false)
        {
            if self.last_search_query.is_empty() {
                return;
            }
            let q = self.last_search_query.clone();
            if let Some(st) = self.search.as_mut() {
                st.query = q;
                st.direction = if forward {
                    SearchDirection::Forward
                } else {
                    SearchDirection::Backward
                };
            }
            self.search_recompute(true);
            return;
        }
        let len = self.search.as_ref().map(|s| s.matches.len()).unwrap_or(0);
        if len == 0 {
            return;
        }
        let cur = match self.search.as_ref() {
            Some(st) => st.current,
            None => return,
        };
        let want_forward = forward;
        let mut next = if want_forward {
            cur + 1
        } else if cur == 0 {
            len - 1
        } else {
            cur - 1
        };
        let mut wrapped = false;
        if want_forward {
            if next >= len {
                next = 0;
                wrapped = true;
            }
        } else if cur == 0 {
            wrapped = true;
        }
        if let Some(st) = self.search.as_mut() {
            st.direction = if want_forward {
                SearchDirection::Forward
            } else {
                SearchDirection::Backward
            };
            if wrapped {
                st.wrapped = true;
            }
        }
        self.search_goto(next);
        if wrapped {
            let msg = if len == 1 {
                "1 match".to_string()
            } else {
                format!("Wrapped ({} matches)", len)
            };
            self.session_manager.active_session_mut().set_status(msg);
        }
    }

    fn search_push_text(&mut self, text: &str) {
        let normalized = normalize_paste(text);
        let mut added = false;
        if let Some(st) = self.search.as_mut() {
            for c in normalized.chars() {
                if c == '\n' || c == '\t' {
                    continue;
                }
                if c.is_control() {
                    continue;
                }
                st.query.push(c);
                added = true;
            }
            if added {
                st.wrapped = false;
            }
        }
        if added {
            // Typing re-anchors to the first match at/after the origin
            // (Emacs forward semantics) instead of keeping a stale index.
            self.search_recompute(true);
        }
    }

    fn search_backspace(&mut self) {
        if let Some(st) = self.search.as_mut() {
            st.query.pop();
            st.wrapped = false;
        }
        self.search_recompute(true);
    }

    fn search_exit(&mut self, abort: bool) {
        if abort {
            if let Some(st) = self.search.take() {
                self.restore_search_origin(&st.origin);
            }
            self.session_manager
                .active_session_mut()
                .set_status("Search cancelled (Ctrl-G)");
        } else {
            if let Some(st) = self.search.take() {
                if !st.query.is_empty() {
                    self.last_search_query = st.query.clone();
                    let msg = if st.matches.is_empty() {
                        format!("Search: '{}' not found", st.query)
                    } else {
                        format!(
                            "Search: '{}' ({}/{})",
                            st.query,
                            st.current + 1,
                            st.matches.len()
                        )
                    };
                    self.session_manager.active_session_mut().set_status(msg);
                }
            }
        }
    }

    /// Handle a key while incremental search is active.
    /// Returns `true` when the key was consumed by the search.
    fn handle_search_key(&mut self, key: event::KeyEvent) -> bool {
        // Ctrl-S: next match forward. Ctrl-R: previous match backward
        // (hijacked from "run script" while searching).
        if key.modifiers == KeyModifiers::CONTROL
            && matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S'))
        {
            self.search_next(true);
            return true;
        }
        if key.modifiers == KeyModifiers::CONTROL
            && matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R'))
        {
            self.search_next(false);
            return true;
        }
        // Ctrl-G: abort back to the pre-search cursor/scroll position.
        if key.modifiers == KeyModifiers::CONTROL
            && matches!(key.code, KeyCode::Char('g') | KeyCode::Char('G'))
        {
            self.search_exit(true);
            return true;
        }
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Enter) | (KeyModifiers::NONE, KeyCode::Esc) => {
                self.search_exit(false);
                true
            }
            (m, KeyCode::Backspace) if m.is_empty() || m == KeyModifiers::SHIFT => {
                self.search_backspace();
                true
            }
            (KeyModifiers::CONTROL, KeyCode::Char('u')) | (KeyModifiers::CONTROL, KeyCode::Char('U')) => {
                if let Some(st) = self.search.as_mut() {
                    st.query.clear();
                    st.wrapped = false;
                }
                self.search_recompute(true);
                true
            }
            (m, KeyCode::Char(c))
                if (m.is_empty() || m == KeyModifiers::SHIFT) && !c.is_control() =>
            {
                if let Some(st) = self.search.as_mut() {
                    st.query.push(c);
                    st.wrapped = false;
                }
                // Typing re-anchors from the origin (Emacs semantics).
                self.search_recompute(true);
                true
            }
            _ => {
                // Any other key exits the search (keeping the match) and is
                // then processed normally (e.g. F2/F12/Enter on a found row).
                self.search_exit(false);
                false
            }
        }
    }

    fn search_target_label(target: SearchTarget) -> &'static str {
        match target {
            SearchTarget::Editor => "SQL editor",
            SearchTarget::Results => "results",
            SearchTarget::Sidebar => "sidebar",
            SearchTarget::ExplorerTree => "explorer",
            SearchTarget::ExplorerDetail => "detail",
            SearchTarget::SessionList => "sessions",
            SearchTarget::SessionSql => "SQL",
            SearchTarget::SessionPlan => "plan",
            SearchTarget::Ddl => "DDL",
            SearchTarget::PerfQueries => "queries",
            SearchTarget::PerfResults => "results",
            SearchTarget::PerfSql => "SQL",
            SearchTarget::History => "history",
            SearchTarget::Help => "help",
            SearchTarget::None => "view",
        }
    }

    fn editor_search_highlight(&self) -> Option<crate::tui::widgets::EditorSearchHighlight> {
        let st = self.search.as_ref()?;
        if st.target != SearchTarget::Editor || st.query.is_empty() {
            return None;
        }
        let (row, col, len) = match st.matches.get(st.current) {
            Some(SearchMatch::Editor { row, col, len }) => (*row, *col, *len),
            _ => (0, 0, 0),
        };
        Some(crate::tui::widgets::EditorSearchHighlight {
            query: st.query.clone(),
            case_sensitive: st.case_sensitive(),
            current_row: row,
            current_col: col,
            current_len: len,
            has_current: !st.matches.is_empty(),
        })
    }

    fn table_search_highlight(&self) -> Option<crate::tui::widgets::TableSearchHighlight> {
        let st = self.search.as_ref()?;
        if st.target != SearchTarget::Results || st.query.is_empty() {
            return None;
        }
        let (r, c) = match st.matches.get(st.current) {
            Some(SearchMatch::TableCell { row, col }) => (*row, *col),
            _ => (None, 0),
        };
        Some(crate::tui::widgets::TableSearchHighlight {
            query: st.query.clone(),
            case_sensitive: st.case_sensitive(),
            current_row: r,
            current_col: c,
            has_current: !st.matches.is_empty(),
        })
    }

    /// Short single-line context of the current match for the search bar,
    /// so the aim is readable even where the in-view highlight can't be
    /// (limited palettes, relayed sessions). Truncated to ~48 chars.
    fn search_current_snippet(&self) -> Option<String> {
        const MAX: usize = 48;
        let st = self.search.as_ref()?;
        if st.query.is_empty() || st.matches.is_empty() {
            return None;
        }
        let m = st.matches[st.current.min(st.matches.len() - 1)];
        let session = self.session_manager.active_session();
        let raw: String = match m {
            SearchMatch::Editor { row, .. } => {
                session.editor.lines.get(row).cloned().unwrap_or_default()
            }
            SearchMatch::TableCell { row, col } => {
                if st.target == SearchTarget::ExplorerDetail {
                    // Preview-table match: row label is enough context.
                    let rows = self.session_manager.explorer_visible_rows();
                    let label = rows
                        .get(session.explorer_cursor)
                        .map(|r| self.session_manager.explorer_row_search_label(r))
                        .unwrap_or_default();
                    match row {
                        None => label,
                        Some(r) => format!("{} [row {}]", label, r + 1),
                    }
                } else {
                    let qr = match st.target {
                        SearchTarget::Results => session.results.last(),
                        SearchTarget::PerfResults => {
                            session.perf_results.get(&session.perf_cursor)
                        }
                        _ => None,
                    }?;
                    if qr.is_error {
                        return None;
                    }
                    let col_name = qr.columns.get(col).cloned().unwrap_or_default();
                    let cell = match row {
                        None => String::from("(header)"),
                        Some(r) => qr
                            .rows
                            .get(r)
                            .and_then(|rr| rr.get(col))
                            .cloned()
                            .unwrap_or_default(),
                    };
                    format!("{}: {}", col_name, cell)
                }
            }
            SearchMatch::Line { line, .. } => match st.target {
                SearchTarget::SessionSql => session
                    .plan_sql_text
                    .as_deref()
                    .unwrap_or_default()
                    .lines()
                    .nth(line)
                    .unwrap_or_default()
                    .to_string(),
                SearchTarget::SessionPlan => session
                    .plan_result
                    .as_ref()
                    .and_then(|qr| {
                        if line == 0 && !qr.columns.is_empty() {
                            Some(qr.columns.join(" | "))
                        } else {
                            let r = line.saturating_sub(1);
                            qr.rows.get(r).map(|rr| rr.join(" | "))
                        }
                    })
                    .unwrap_or_default(),
                SearchTarget::Ddl => session
                    .ddl_object
                    .as_ref()
                    .and_then(|o| o.ddl.lines().nth(line))
                    .unwrap_or_default()
                    .to_string(),
                SearchTarget::PerfSql => crate::db::perf::perf_queries_for(
                    self.session_manager.active_db_type(),
                )
                .get(session.perf_cursor)
                .and_then(|q| q.sql.lines().nth(line))
                .unwrap_or_default()
                .to_string(),
                _ => {
                    // Explorer detail text / other line views.
                    self.search_explorer_detail_line(line)
                        .unwrap_or_default()
                }
            },
            SearchMatch::ListRow { row } => self.search_list_label(st.target, row),
        };
        let single: String = raw
            .replace(['\r', '\n', '\t'], " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if single.is_empty() {
            return None;
        }
        let mut out: String = single.chars().take(MAX).collect();
        if single.chars().count() > MAX {
            out.push('…');
        }
        Some(out)
    }

    /// Display label for a flat-list match (explorer tree, session list,
    /// history, perf queries, help).
    fn search_list_label(&self, target: SearchTarget, row: usize) -> String {
        let session = self.session_manager.active_session();
        match target {
            SearchTarget::ExplorerTree => self
                .session_manager
                .explorer_visible_rows()
                .get(row)
                .map(|r| self.session_manager.explorer_row_search_label(r))
                .unwrap_or_default(),
            SearchTarget::SessionList => self
                .session_manager
                .visible_session_list()
                .get(row)
                .map(|d| format!("{} {} {}", d.sid, d.username, d.status))
                .unwrap_or_default(),
            SearchTarget::History => session
                .query_history
                .get(row)
                .cloned()
                .unwrap_or_default(),
            SearchTarget::PerfQueries => crate::db::perf::perf_queries_for(
                self.session_manager.active_db_type(),
            )
            .get(row)
            .map(|q| q.title.to_string())
            .unwrap_or_default(),
            SearchTarget::Help => crate::tui::widgets::help_search_lines()
                .get(row)
                .cloned()
                .unwrap_or_default(),
            _ => format!("row {}", row + 1),
        }
    }

    /// One Explorer-detail text line for snippet purposes (mirrors the
    /// fallback label used for matching).
    fn search_explorer_detail_line(&self, _line: usize) -> Option<String> {
        let session = self.session_manager.active_session();
        let rows = self.session_manager.explorer_visible_rows();
        rows.get(session.explorer_cursor)
            .map(|r| self.session_manager.explorer_row_search_label(r))
    }

    /// Identity of the current fullscreen view: mode plus which panes are
    /// maximized. Used to repaint the content area on view switches.
    fn view_key(&self) -> (SessionMode, u8) {
        let mut flags = 0u8;
        if self.maximized_box.is_some() {
            flags |= 1;
        }
        if self.explorer_maximized {
            flags |= 2;
        }
        if self.session_view_maximized.is_some() {
            flags |= 4;
        }
        if self.perf_maximized {
            flags |= 8;
        }
        (
            self.session_manager.active_session().mode.clone(),
            flags,
        )
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.show_startup_help {
            let max = self.help_max_scroll();
            match mouse.kind {
                MouseEventKind::ScrollDown => {
                    let s = self.session_manager.active_session_mut();
                    s.help_scroll = scroll_by(s.help_scroll, 3, max);
                }
                MouseEventKind::ScrollUp => {
                    let s = self.session_manager.active_session_mut();
                    s.help_scroll = s.help_scroll.saturating_sub(3);
                }
                _ => {}
            }
            return;
        }
        // A click exits incremental search (keeping the match) so the
        // click can relocate focus/cursor normally.
        if self.search.is_some()
            && matches!(
                mouse.kind,
                MouseEventKind::Down(_) | MouseEventKind::Drag(_)
            )
        {
            self.search_exit(false);
        }
        // Clicking anywhere relocates focus/cursor; a completion popup anchored
        // to the old position must not accept into the wrong range.
        if self.session_manager.active_session().completion.is_some()
            && matches!(mouse.kind, MouseEventKind::Down(_))
        {
            self.session_manager.close_completion();
        }
        // F1 Help viewer owns its area fullscreen.
        if self.session_manager.active_session().mode == SessionMode::Help
            && self.handle_help_mouse(mouse)
        {
            return;
        }
        // F3 History viewer owns its area fullscreen.
        if self.session_manager.active_session().mode == SessionMode::History
            && self.handle_history_mouse(mouse)
        {
            return;
        }
        // F4 Object DDL viewer owns its area fullscreen.
        if self.session_manager.active_session().mode == SessionMode::ObjectDdl
            && self.handle_object_ddl_mouse(mouse)
        {
            return;
        }
        // F6 Performance dashboard owns its area fullscreen.
        if self.session_manager.active_session().mode == SessionMode::Performance
            && self.handle_performance_mouse(mouse)
        {
            return;
        }
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
                // Tab bar click: switch directly to the clicked session tab.
                if self.tabs_area.contains(Position::new(mouse.column, mouse.row)) {
                    let n = self.session_manager.sessions.len();
                    if n > 0 && self.tabs_area.width > 0 {
                        let rel = mouse.column.saturating_sub(self.tabs_area.x) as usize;
                        let idx = (rel * n / self.tabs_area.width.max(1) as usize).min(n - 1);
                        self.session_manager.switch_to(idx);
                    }
                    return;
                }
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
                } else if self.maximized_box.is_none()
                    && self.editor_area.height > 0
                    && self.results_area.height > 0
                    && mouse.column >= self.editor_area.x
                    && mouse.column < self.editor_area.x + self.editor_area.width.max(1)
                    && (mouse.row as i16 - self.editor_area.bottom() as i16).abs() <= 1
                {
                    // Horizontal splitter between the SQL editor and results.
                    self.resizing_editor = true;
                } else if in_editor {
                    self.click_editor(mouse.column, mouse.row);
                    // Start a drag-selection: a plain click just places the
                    // cursor (empty selection, cleared on release).
                    let s = self.session_manager.active_session_mut();
                    let (r, c) = (s.editor.cursor_row, s.editor.cursor_col);
                    s.editor_selection = Some(EditorSelection {
                        anchor_row: r,
                        anchor_col: c,
                        cursor_row: r,
                        cursor_col: c,
                    });
                    self.selecting_editor = true;
                } else if in_results {
                    self.active_window = ActiveWindow::Results;
                    // Start a drag-selection in any result format: header
                    // clicks select column names, data clicks select cells.
                    // A plain click just focuses (empty selection, cleared
                    // on release). Clicks on borders/separators do nothing.
                    if mouse.row > self.results_area.y {
                        if let Some((r, c)) =
                            self.results_cell_to_cursor(mouse.column, mouse.row)
                        {
                            self.session_manager.active_session_mut().table_selection =
                                Some(TableSelection {
                                    anchor_row: r,
                                    anchor_col: c,
                                    cursor_row: r,
                                    cursor_col: c,
                                });
                            self.selecting_table = true;
                        }
                    }
                } else if in_sidebar {
                    self.active_window = ActiveWindow::Sidebar;
                }
            }
            MouseEventKind::Down(crossterm::event::MouseButton::Middle) => {
                // tmux-style middle-click paste: place the cursor where the
                // user clicked in the editor, then insert the primary selection.
                // Over SSH (no X display) arboard fails — tell the user what
                // to press instead of failing silently.
                if self
                    .editor_area
                    .contains(Position::new(mouse.column, mouse.row))
                {
                    self.click_editor(mouse.column, mouse.row);
                    if let Some(text) = Self::read_primary_selection(self.clipboard.as_mut()) {
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
                self.resizing_editor = false;
                self.session_resizing = None;
                // End of an editor drag: copy a non-empty selection to the
                // clipboard (OS + OSC52, so it works over SSH). A plain
                // click leaves an empty selection, which clears itself.
                if self.selecting_editor {
                    self.selecting_editor = false;
                    if !self.copy_editor_selection() {
                        self.session_manager.active_session_mut().editor_selection = None;
                    }
                }
                // End of a table drag: same, but header + TSV with no box
                // borders — just the columns and the content.
                if self.selecting_table {
                    self.selecting_table = false;
                    if !self.copy_table_selection() {
                        self.session_manager.active_session_mut().table_selection = None;
                    }
                }
            }
            MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                if self.is_resizing {
                    let (w, _) = crossterm::terminal::size().unwrap_or((80, 24));
                    let width = w as usize;
                    let col = mouse.column as usize;
                    let pct = ((width.saturating_sub(col)) * 100) / width;
                    self.sidebar_split_pct = (pct as u16).clamp(10, 70);
                }
                if self.resizing_editor {
                    // Drag the editor/results boundary: grow/shrink editor.
                    let new_h = mouse.row.saturating_sub(self.editor_area.y).saturating_add(1).clamp(4, 40);
                    self.editor_height = new_h;
                }
                if let Some(which) = self.session_resizing {
                    self.drag_session_splitter(which, mouse.row);
                }
                // Extend the editor drag-selection and follow it with the
                // text cursor.
                if self.selecting_editor {
                    let (r, c) = self.editor_cell_to_cursor(mouse.column, mouse.row);
                    let s = self.session_manager.active_session_mut();
                    s.editor.cursor_row = r;
                    s.editor.cursor_col = c;
                    s.editor.snap_cursor_to_char_boundary();
                    if let Some(sel) = s.editor_selection.as_mut() {
                        sel.cursor_row = s.editor.cursor_row;
                        sel.cursor_col = s.editor.cursor_col;
                    }
                }
                // Extend the table cell selection.
                if self.selecting_table {
                    if let Some((r, c)) =
                        self.results_cell_to_cursor(mouse.column, mouse.row)
                    {
                        if let Some(sel) = self
                            .session_manager
                            .active_session_mut()
                            .table_selection
                            .as_mut()
                        {
                            sel.cursor_row = r;
                            sel.cursor_col = c;
                        }
                    }
                }
            }
            MouseEventKind::ScrollDown => {
                if self
                    .editor_area
                    .contains(Position::new(mouse.column, mouse.row))
                {
                    // Mouse wheel over the SQL editor scrolls it vertically;
                    // Shift+wheel scrolls long lines horizontally.
                    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                        let s = self.session_manager.active_session_mut();
                        s.editor.col_offset = s.editor.col_offset.saturating_add(4);
                    } else {
                        let s = self.session_manager.active_session_mut();
                        s.editor.scroll_offset = s.editor.scroll_offset.saturating_add(3);
                    }
                } else if self
                    .results_area
                    .contains(Position::new(mouse.column, mouse.row))
                {
                    // Shift+wheel scrolls columns (reveals ▶), plain wheel
                    // scrolls rows (reveals ▼).
                    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                        let session = self.session_manager.active_session_mut();
                        session.col_scroll_offset =
                            session.col_scroll_offset.saturating_add(1);
                    } else {
                        let session = self.session_manager.active_session_mut();
                        session.scroll_offset = session.scroll_offset.saturating_add(3);
                        self.session_manager.maybe_fetch_next_page();
                    }
                }
            }
            MouseEventKind::ScrollUp
                if self
                    .editor_area
                    .contains(Position::new(mouse.column, mouse.row)) =>
            {
                if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                    let s = self.session_manager.active_session_mut();
                    s.editor.col_offset = s.editor.col_offset.saturating_sub(4);
                } else {
                    let s = self.session_manager.active_session_mut();
                    s.editor.scroll_offset = s.editor.scroll_offset.saturating_sub(3);
                }
            }
            MouseEventKind::ScrollUp
                if self
                    .results_area
                    .contains(Position::new(mouse.column, mouse.row)) =>
            {
                if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                    let session = self.session_manager.active_session_mut();
                    session.col_scroll_offset =
                        session.col_scroll_offset.saturating_sub(1);
                } else {
                    let session = self.session_manager.active_session_mut();
                    session.scroll_offset = session.scroll_offset.saturating_sub(3);
                }
            }
            _ => {}
        }
    }

    /// Convert a mouse cell into an editor (row, byte-col) position, using
    /// the same border/gutter/scroll math as rendering. Always lands on a
    /// char boundary. Shared by click-to-place and drag-selection.
    fn editor_cell_to_cursor(&self, column: u16, row: u16) -> (usize, usize) {
        let gutter = self.editor_gutter_width() as usize;
        let frame: u16 = if self.mouse_capture { 1 } else { 0 };
        let session = self.session_manager.active_session();
        let raw_row =
            (row.saturating_sub(self.editor_area.y + frame)) as usize + session.editor.scroll_offset;
        let target_col =
            (column.saturating_sub(self.editor_area.x + frame + gutter as u16)) as usize;
        // The viewport starts `col_offset` display cells into the line.
        let visual_col = target_col + session.editor.col_offset;
        let target_row = raw_row.min(session.editor.lines.len().saturating_sub(1));
        (
            target_row,
            byte_offset_for_column(&session.editor.lines[target_row], visual_col),
        )
    }

    /// Focus the editor and move the text cursor to the clicked position.
    /// Borderless (Mouse:OFF) editors have no 1-cell frame, so the offset
    /// skips the border only when borders are drawn.
    fn click_editor(&mut self, column: u16, row: u16) {
        let (target_row, target_col) = self.editor_cell_to_cursor(column, row);
        self.active_window = ActiveWindow::Editor;
        let session = self.session_manager.active_session_mut();
        session.mode = SessionMode::Query;
        session.editor.cursor_row = target_row;
        session.editor.cursor_col = target_col;
        session.editor.snap_cursor_to_char_boundary();
    }

    /// Ctrl+Y: copy what the focused pane shows.
    fn copy_focused_to_clipboard(&mut self) {
        let (editor_sel, table_sel) = {
            let s = self.session_manager.active_session();
            (
                s.selected_text().is_some(),
                s.selected_table_text().is_some(),
            )
        };
        match copy_target_for(self.active_window, editor_sel, table_sel) {
            CopyTarget::Results => self.copy_results_to_clipboard(),
            CopyTarget::TableSelection => {
                self.copy_table_selection();
            }
            CopyTarget::SqlSelection => {
                self.copy_editor_selection();
            }
            CopyTarget::SqlStatement => self.copy_sql_to_clipboard(),
        }
    }

    /// Convert a mouse cell into a result-table (row, column) position
    /// using the last frame's render geometry. Works in every result
    /// format — the layout records each format's header/data line offsets
    /// (Table 1/2, Markdown 1/3, ASCII 2/4), and columns resolve through
    /// the uniform grid (Table/ASCII) or per-row Markdown cell widths.
    /// Returns `None` on chrome (borders, separators) or when there is no
    /// data under the cursor. Rows are `None` on the header line.
    fn results_cell_to_cursor(&self, column: u16, row: u16) -> Option<(Option<usize>, usize)> {
        let layout = &self.results_layout;
        let area = self.results_area;
        let session = self.session_manager.active_session();
        let qr = session.results.last()?;
        if qr.is_error || qr.columns.is_empty() {
            return None;
        }
        let total_cols = qr.columns.len();
        let y_rel = row.saturating_sub(area.y);
        let abs_row: Option<usize> = if y_rel == layout.header_y {
            None
        } else {
            let rel = y_rel.checked_sub(layout.first_data_y)?;
            if qr.rows.is_empty() {
                return None;
            }
            let visible = layout
                .visible_rows
                .min(qr.rows.len().saturating_sub(layout.top_row));
            if visible == 0 {
                return None;
            }
            let rel = rel as usize;
            Some(layout.top_row + rel.min(visible.saturating_sub(1)))
        };
        let abs_col = match self.result_format {
            ResultFormat::Markdown => {
                markdown_col_at(qr, layout, area, abs_row, column)?
            }
            _ => uniform_col_at(layout, area, column, total_cols)?,
        };
        Some((abs_row, abs_col))
    }

    /// Copy the current table drag-selection to the clipboard (OS +
    /// OSC52, like Ctrl+Y). The text matches the current view format, so
    /// what you paste looks like what you selected: TSV in Table view
    /// (header + rows, no borders), Markdown with its `|` vertical lines
    /// kept in Markdown view, the ASCII grid in ASCII view. Returns false
    /// when there is no (non-empty) selection.
    fn copy_table_selection(&mut self) -> bool {
        let text = {
            let session = self.session_manager.active_session();
            let Some((header, rows)) = session.selected_table_cells() else {
                return false;
            };
            match self.result_format {
                ResultFormat::Table => {
                    let mut out = header.join("\t");
                    for row in &rows {
                        out.push('\n');
                        out.push_str(&row.join("\t"));
                    }
                    out
                }
                ResultFormat::Markdown => {
                    let refs: Vec<&[String]> =
                        rows.iter().map(|r| r.as_slice()).collect();
                    markdown_table(&header, &refs)
                }
                ResultFormat::Ascii => {
                    if rows.is_empty() {
                        // Header-only selection: no grid to draw, just names.
                        header.join("\t")
                    } else {
                        let w = header.len();
                        let h = rows.len();
                        let tmp = QueryRow {
                            columns: header,
                            rows,
                            truncated: false,
                            elapsed_ms: 0,
                            is_error: false,
                            error_msg: None,
                            rows_affected: None,
                            page_offset: 0,
                            page_size: h,
                            byte_count: 0,
                            total_fetched: h,
                        };
                        format_ascii_table(&tmp, 0..w, 0, h)
                    }
                }
            }
        };
        let chars = text.chars().count();
        let outcome = self.copy_and_remember(&text);
        let msg = Self::copy_status(outcome, chars, "table selection");
        self.session_manager.active_session_mut().set_status(msg);
        true
    }

    /// Copy the current editor drag-selection to the clipboard (OS +
    /// OSC52, like Ctrl+Y / Alt+Y). Returns false when there is no
    /// (non-empty) selection.
    fn copy_editor_selection(&mut self) -> bool {
        let text = match self.session_manager.active_session().selected_text() {
            Some(t) => t,
            None => return false,
        };
        let chars = text.chars().count();
        let outcome = self.copy_and_remember(&text);
        let msg = Self::copy_status(outcome, chars, "selection");
        self.session_manager.active_session_mut().set_status(msg);
        true
    }

    fn handle_key(&mut self, key: event::KeyEvent) {
        // The kitty keyboard protocol (when enabled) also delivers
        // release events; process press/repeat only.
        if key.kind == event::KeyEventKind::Release {
            return;
        }

        if self.show_startup_help {
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q'))
            {
                self.should_quit = true;
                return;
            }
            let max = self.help_max_scroll();
            match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Up) => {
                    let s = self.session_manager.active_session_mut();
                    s.help_scroll = s.help_scroll.saturating_sub(1);
                    return;
                }
                (KeyModifiers::NONE, KeyCode::Down) => {
                    let s = self.session_manager.active_session_mut();
                    s.help_scroll = scroll_by(s.help_scroll, 1, max);
                    return;
                }
                (KeyModifiers::NONE, KeyCode::PageUp) => {
                    let s = self.session_manager.active_session_mut();
                    s.help_scroll = s.help_scroll.saturating_sub(15);
                    return;
                }
                (KeyModifiers::NONE, KeyCode::PageDown) => {
                    let s = self.session_manager.active_session_mut();
                    s.help_scroll = scroll_by(s.help_scroll, 15, max);
                    return;
                }
                _ => {
                    // Dismiss the splash AND keep processing this same key so
                    // the first Ctrl+O / F-key / shortcut isn't swallowed.
                    self.show_startup_help = false;
                    self.session_manager.active_session_mut().help_scroll = 0;
                }
            }
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
        // Incremental search (Emacs I-search) takes over the key stream
        // while active: Ctrl-S / Ctrl-R step, printable chars extend the
        // query, Enter/Esc keep the match, Ctrl-G aborts. Paste lands in
        // the query, not the editor.
        if self.search.is_some() {
            if Self::is_paste_key(key) {
                self.paste_from_clipboard();
                return;
            }
            if self.handle_search_key(key) {
                return;
            }
            // `handle_search_key` exited the search (keeping the match);
            // fall through so the key works normally (e.g. Enter on a row).
        }
        // Explicit keyboard paste (Ctrl+V / Shift+Ins) works in the command
        // bar, the connection dialog and the SQL editor — both with mouse
        // capture ON and OFF.
        if Self::is_paste_key(key) {
            self.paste_from_clipboard();
            return;
        }

        if self.command_mode {
            self.handle_command_key(key);
            return;
        }

        // Ctrl-S starts incremental search in the current view (editor,
        // results, explorer, F2 panes, DDL, perf, history, help).
        if key.modifiers == KeyModifiers::CONTROL
            && matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S'))
        {
            self.search_start();
            return;
        }

        let mode = self.session_manager.active_session().mode.clone();

        if mode == SessionMode::ConnectionPickerDialog {
            // Alt+N / Alt+P cycles config.yml `connections[]` entries.
            if key.modifiers.contains(KeyModifiers::ALT)
                && matches!(key.code, KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('p') | KeyCode::Char('P'))
            {
                let dir = match key.code {
                    KeyCode::Char('n') | KeyCode::Char('N') => 1isize,
                    _ => -1isize,
                };
                if self.cycle_saved_connection(dir) {
                    return;
                }
            }
            self.handle_conn_dialog(key);
            return;
        }
        if mode == SessionMode::Help {
            self.handle_help(key);
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
        if mode == SessionMode::ObjectDdl {
            self.handle_object_ddl(key);
            return;
        }
        if mode == SessionMode::Performance {
            self.handle_performance(key);
            return;
        }

        // Completion popup: Ctrl+Space opens it; while open, navigation and
        // accept keys take precedence over editor movement / focus cycling.
        let popup_open = self.session_manager.active_session().completion.is_some();
        if mode == SessionMode::Query && self.active_window == ActiveWindow::Editor {
            if Self::is_completion_key(key) {
                self.session_manager.open_completion();
                return;
            }
            if popup_open && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) {
                match key.code {
                    KeyCode::Esc => {
                        self.session_manager.close_completion();
                        return;
                    }
                    KeyCode::Up => {
                        self.session_manager.move_completion(-1);
                        return;
                    }
                    KeyCode::Down => {
                        self.session_manager.move_completion(1);
                        return;
                    }
                    KeyCode::PageUp => {
                        self.session_manager.move_completion(-8);
                        return;
                    }
                    KeyCode::PageDown => {
                        self.session_manager.move_completion(8);
                        return;
                    }
                    KeyCode::Enter | KeyCode::Tab => {
                        self.session_manager.accept_completion();
                        return;
                    }
                    _ => {}
                }
            }
        } else if popup_open {
            self.session_manager.close_completion();
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
                self.session_manager.close_completion();
                self.command_mode = true;
                self.command_input.clear();
                self.command_error = None;
                self.command_confirm = None;
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('t')) => {
                self.session_manager.close_completion();
                self.session_manager.add_session();
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
                let idx = self.session_manager.active_idx;
                if self.session_manager.sessions.len() <= 1 {
                    return;
                }
                // Two-step close when the tab holds unsaved work: first
                // press arms, second press within 5s closes.
                let needs_confirm = {
                    let s = self.session_manager.active_session();
                    !s.editor.text().trim().is_empty()
                        || s.pending_query
                        || s.paging
                        || s.connecting
                };
                if needs_confirm {
                    let now = Instant::now();
                    let armed = matches!(self.pending_close_tab, Some((i, t))
                        if i == idx && now.duration_since(t).as_secs() < 5);
                    if !armed {
                        self.pending_close_tab = Some((idx, now));
                        self.session_manager.active_session_mut().set_status(
                            "Press Ctrl+W again to close this tab (editor has SQL)",
                        );
                        return;
                    }
                }
                self.pending_close_tab = None;
                self.session_manager.remove_session(idx);
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Right) | (KeyModifiers::ALT, KeyCode::Right) => {
                // In the SQL editor Ctrl+Right jumps by word (readline-style);
                // Alt+Right always switches tabs. Elsewhere both switch tabs.
                let in_editor = self.session_manager.active_session().mode
                    == SessionMode::Query
                    && self.active_window == ActiveWindow::Editor;
                if in_editor && key.modifiers.contains(KeyModifiers::CONTROL) {
                    let s = self.session_manager.active_session_mut();
                    s.editor_selection = None;
                    s.editor.move_word_right();
                    return;
                }
                self.session_manager.switch_next();
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Left) | (KeyModifiers::ALT, KeyCode::Left) => {
                let in_editor = self.session_manager.active_session().mode
                    == SessionMode::Query
                    && self.active_window == ActiveWindow::Editor;
                if in_editor && key.modifiers.contains(KeyModifiers::CONTROL) {
                    let s = self.session_manager.active_session_mut();
                    s.editor_selection = None;
                    s.editor.move_word_left();
                    return;
                }
                self.session_manager.switch_prev();
                return;
            }
            (KeyModifiers::ALT, KeyCode::Char(c @ '1'..='9')) => {
                self.session_manager.switch_to(c as usize - '1' as usize);
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('o')) => {
                self.session_manager.close_completion();
                let session = self.session_manager.active_session_mut();
                session.conn_dialog_backup = Some(session.conn_dialog.clone());
                let field = session.conn_dialog.active_field;
                session.conn_dialog.select_field(field);
                session.mode = SessionMode::ConnectionPickerDialog;
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                // Standard copy key with a safety valve: a running query
                // still wins (cancel it, so runaway queries stay stoppable);
                // otherwise this is the same contextual copy as Ctrl+Y —
                // selection first, else the statement/table the focused
                // pane shows.
                // (`cancel_query` is a silent no-op when idle.)
                if self.session_manager.active_session().pending_query {
                    self.session_manager.cancel_query();
                } else {
                    self.copy_focused_to_clipboard();
                }
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
            // Ctrl+Y: contextual copy — the focused pane decides. Results
            // (or Sidebar) copies the table drag-selection if present, else
            // the full table; the Editor copies the drag-selection if
            // present, else the SQL statement at the cursor (with multiple
            // statements, only the one under the cursor). Alt+Y always
            // copies SQL explicitly.
            (KeyModifiers::CONTROL, KeyCode::Char('y'))
            | (KeyModifiers::CONTROL, KeyCode::Char('Y')) => {
                self.copy_focused_to_clipboard();
                return;
            }
            // Alt+Y: SQL under cursor, never with line numbers (built from
            // editor.lines, plus OSC52 so it lands on the local clipboard
            // over SSH). Plain `ESC y` survives git-bash like Alt+M/Alt+Enter.
            (KeyModifiers::ALT, KeyCode::Char('y')) | (KeyModifiers::ALT, KeyCode::Char('Y')) => {
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
                self.session_manager.close_completion();
                let s = self.session_manager.active_session_mut();
                s.help_scroll = 0;
                s.mode = SessionMode::Help;
                return;
            }
            (KeyModifiers::NONE, KeyCode::F(2)) => {
                self.session_manager.close_completion();
                self.session_manager.enter_session_view();
                return;
            }
            (KeyModifiers::NONE, KeyCode::F(3)) => {
                self.session_manager.close_completion();
                self.session_manager.active_session_mut().mode = SessionMode::History;
                return;
            }
            (KeyModifiers::NONE, KeyCode::F(4)) => {
                self.session_manager.close_completion();
                self.session_manager.open_ddl_at_cursor();
                return;
            }
            (KeyModifiers::NONE, KeyCode::F(6)) => {
                self.session_manager.close_completion();
                self.perf_focus = PerfPane::Queries;
                self.perf_maximized = false;
                self.session_manager.enter_performance();
                return;
            }
            (KeyModifiers::NONE, KeyCode::F(12)) => {
                self.session_manager.close_completion();
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

        // Typing a printable character (or a single-character paste) while a
        // read-only pane has focus starts editing SQL instead of being
        // silently dropped. Bare Enter/Tab are intentionally excluded so
        // browsing Results/Sidebar never accidentally inserts newlines.
        // Multi-key paste bursts are reassembled earlier in `handle_events`.
        if mode == SessionMode::Query
            && matches!(
                self.active_window,
                ActiveWindow::Results | ActiveWindow::Sidebar
            )
        {
            if let (m, KeyCode::Char(c)) = (key.modifiers, key.code) {
                if (m.is_empty() || m == KeyModifiers::SHIFT) && !c.is_control() {
                    self.active_window = ActiveWindow::Editor;
                    let session = self.session_manager.active_session_mut();
                    session.delete_selection();
                    session.editor.insert_char(c);
                    return;
                }
            }
        }

        let session = self.session_manager.active_session_mut();

        match session.mode {
            SessionMode::Query => match self.active_window {
                ActiveWindow::Editor => match (key.modifiers, key.code) {
                    (KeyModifiers::CONTROL, KeyCode::Char('a')) => {
                        session.editor_selection = None;
                        session.editor.cursor_col = 0;
                    }
                    // Undo (Alt+Z) / redo (Alt+Shift+Z).
                    (KeyModifiers::ALT, KeyCode::Char('z')) => {
                        session.editor_selection = None;
                        if !session.editor.undo() {
                            session.set_status("Nothing to undo");
                        }
                    }
                    (KeyModifiers::ALT, KeyCode::Char('Z')) => {
                        session.editor_selection = None;
                        if !session.editor.redo() {
                            session.set_status("Nothing to redo");
                        }
                    }
                    // Emacs word jumps (don't clash with tab switching).
                    (KeyModifiers::ALT, KeyCode::Char('b'))
                    | (KeyModifiers::ALT, KeyCode::Char('B')) => {
                        session.editor_selection = None;
                        session.editor.move_word_left();
                    }
                    (KeyModifiers::ALT, KeyCode::Char('f'))
                    | (KeyModifiers::ALT, KeyCode::Char('F')) => {
                        session.editor_selection = None;
                        session.editor.move_word_right();
                    }
                    (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
                        session.editor_selection = None;
                        session.editor.cursor_col =
                            session.editor.lines[session.editor.cursor_row].len();
                    }
                    (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
                        session.editor_selection = None;
                        session.editor.kill_line();
                    }
                    (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
                        session.editor_selection = None;
                        session.editor.lines[session.editor.cursor_row].clear();
                        session.editor.cursor_col = 0;
                    }
                    (KeyModifiers::CONTROL, KeyCode::Char('d')) => session.editor.delete(),
                    (m, KeyCode::Char(c)) if m.is_empty() || m == KeyModifiers::SHIFT => {
                        if !c.is_control() {
                            // Typing replaces the drag-selection, like a GUI editor.
                            session.delete_selection();
                            session.editor.insert_char(c);
                        }
                    }
                    (m, KeyCode::Backspace) if m.is_empty() || m == KeyModifiers::SHIFT => {
                        if !session.delete_selection() {
                            session.editor.backspace();
                        }
                    }
                    (m, KeyCode::Delete) if m.is_empty() || m == KeyModifiers::SHIFT => {
                        if !session.delete_selection() {
                            session.editor.delete();
                        }
                    }
                    (m, KeyCode::Enter) if m.is_empty() || m == KeyModifiers::SHIFT => {
                        session.delete_selection();
                        session.editor.newline();
                    }
                    (m, KeyCode::Esc) if m.is_empty() => {
                        session.editor_selection = None;
                    }
                    (_, KeyCode::Left) => {
                        session.editor_selection = None;
                        session.editor.move_left();
                    }
                    (_, KeyCode::Right) => {
                        session.editor_selection = None;
                        session.editor.move_right();
                    }
                    (_, KeyCode::Up) => {
                        session.editor_selection = None;
                        session.editor.move_up();
                    }
                    (_, KeyCode::Down) => {
                        session.editor_selection = None;
                        session.editor.move_down();
                    }
                    (_, KeyCode::Home) => {
                        session.editor_selection = None;
                        session.editor.cursor_col = 0;
                    }
                    (_, KeyCode::End) => {
                        session.editor_selection = None;
                        session.editor.cursor_col =
                            session.editor.lines[session.editor.cursor_row].len();
                    }
                    (_, KeyCode::PageUp) => {
                        session.editor_selection = None;
                        for _ in 0..10 {
                            session.editor.move_up();
                        }
                    }
                    (_, KeyCode::PageDown) => {
                        session.editor_selection = None;
                        for _ in 0..10 {
                            session.editor.move_down();
                        }
                    }
                    _ => {}
                },
                ActiveWindow::Results => {
                    match key.code {
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
                        KeyCode::Esc => {
                            // First Esc clears an active table drag-selection
                            // (and stays in Results); next Esc goes to Editor.
                            let had_selection = session.selected_table_text().is_some();
                            session.table_selection = None;
                            if !had_selection {
                                self.active_window = ActiveWindow::Editor;
                            }
                        }
                        _ => {}
                    }
                    if matches!(key.code, KeyCode::Down | KeyCode::PageDown) {
                        self.session_manager.maybe_fetch_next_page();
                    }
                }
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
            | SessionMode::ObjectDdl
            | SessionMode::Performance
            | SessionMode::ConnectionPickerDialog => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                    session.mode = SessionMode::Query;
                    self.active_window = ActiveWindow::Editor;
                }
            }
        }

        // Typing while the popup is open: recompute for the new prefix. The
        // catalog is never touched here; only the in-memory store is read.
        // Any other key (cursor movement, ...) closes the popup so its
        // word_start can never go stale.
        if popup_open && mode == SessionMode::Query && self.active_window == ActiveWindow::Editor {
            if Self::is_editor_edit_key(key) {
                self.session_manager.refresh_completion();
            } else {
                self.session_manager.close_completion();
            }
        }
    }

    /// Ctrl+Space (and Ctrl+@ / Ctrl+., which survive terminals that map the
    /// NUL byte differently) opens the completion popup. A bare NUL is also
    /// accepted because some terminals drop the Ctrl modifier for Ctrl+Space.
    fn is_completion_key(key: event::KeyEvent) -> bool {
        matches!(
            (key.modifiers, key.code),
            (
                KeyModifiers::CONTROL,
                KeyCode::Char(' ')
                    | KeyCode::Char('\0')
                    | KeyCode::Char('@')
                    | KeyCode::Char('.')
                    | KeyCode::Null
            ) | (KeyModifiers::NONE, KeyCode::Char('\0') | KeyCode::Null)
        )
    }

    /// Keys that change the editor text and therefore refresh the popup.
    fn is_editor_edit_key(key: event::KeyEvent) -> bool {
        match (key.modifiers, key.code) {
            (m, KeyCode::Char(c)) => (m.is_empty() || m == KeyModifiers::SHIFT) && !c.is_control(),
            (m, KeyCode::Backspace) | (m, KeyCode::Delete) => {
                m.is_empty() || m == KeyModifiers::SHIFT
            }
            _ => false,
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
                self.command_error = None;
                self.command_input.pop();
            }
            KeyCode::Char(c) if !c.is_control() => {
                self.command_error = None;
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
                // Keep the command bar open with the typed text so the inline
                // error in `render_command_bar` is actually visible and editable.
                self.command_mode = true;
                self.command_input = cmd.to_string();
                self.command_error = Some(format!(
                    "Unknown command: '{}' (try @file.sql or clear)",
                    cmd
                ));
            }
        }
    }

    fn help_max_scroll(&self) -> usize {
        let total = crate::tui::widgets::help_line_count();
        let view_h = self.help_area.height.saturating_sub(2).max(1) as usize;
        total.saturating_sub(view_h)
    }

    fn handle_help(&mut self, key: event::KeyEvent) {
        let max = self.help_max_scroll();
        let session = self.session_manager.active_session_mut();
        match key.code {
            KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('q') | KeyCode::Char('Q') => {
                session.mode = SessionMode::Query;
                self.active_window = ActiveWindow::Editor;
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => {
                session.help_scroll = session.help_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
                session.help_scroll = scroll_by(session.help_scroll, 1, max);
            }
            KeyCode::PageUp => {
                session.help_scroll = session.help_scroll.saturating_sub(15);
            }
            KeyCode::PageDown => {
                session.help_scroll = scroll_by(session.help_scroll, 15, max);
            }
            KeyCode::Home => {
                session.help_scroll = 0;
            }
            KeyCode::End => {
                session.help_scroll = max;
            }
            _ => {}
        }
    }

    fn handle_help_mouse(&mut self, mouse: MouseEvent) -> bool {
        let max = self.help_max_scroll();
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                let s = self.session_manager.active_session_mut();
                s.help_scroll = scroll_by(s.help_scroll, 3, max);
            }
            MouseEventKind::ScrollUp => {
                let s = self.session_manager.active_session_mut();
                s.help_scroll = s.help_scroll.saturating_sub(3);
            }
            _ => {}
        }
        true
    }

    fn handle_history(&mut self, key: event::KeyEvent) {
        let vis_h = self.history_area.height.saturating_sub(2) as usize;
        let session = self.session_manager.active_session_mut();
        let len = session.query_history.len();
        match key.code {
            KeyCode::Esc | KeyCode::F(3) | KeyCode::Char('q') | KeyCode::Char('Q') => {
                session.mode = SessionMode::Query;
                self.active_window = ActiveWindow::Editor;
                return;
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => {
                session.history_cursor = session.history_cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
                if len > 0 {
                    session.history_cursor = (session.history_cursor + 1).min(len - 1);
                }
            }
            KeyCode::PageUp => {
                session.history_cursor = session.history_cursor.saturating_sub(15);
            }
            KeyCode::PageDown => {
                if len > 0 {
                    session.history_cursor = (session.history_cursor + 15).min(len - 1);
                }
            }
            KeyCode::Home => {
                session.history_cursor = 0;
            }
            KeyCode::End => {
                session.history_cursor = len.saturating_sub(1);
            }
            KeyCode::Enter => {
                if let Some(stmt) = session.query_history.get(session.history_cursor).cloned() {
                    session.editor.set_text(&stmt);
                    session.mode = SessionMode::Query;
                    self.active_window = ActiveWindow::Editor;
                }
                return;
            }
            _ => {}
        }
        session.ensure_history_cursor_visible(vis_h);
    }

    fn handle_history_mouse(&mut self, mouse: MouseEvent) -> bool {
        let vis_h = self.history_area.height.saturating_sub(2) as usize;
        let session = self.session_manager.active_session_mut();
        let len = session.query_history.len();
        if len == 0 {
            return true;
        }
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                session.history_cursor = (session.history_cursor + 3).min(len - 1);
                session.ensure_history_cursor_visible(vis_h);
            }
            MouseEventKind::ScrollUp => {
                session.history_cursor = session.history_cursor.saturating_sub(3);
                session.ensure_history_cursor_visible(vis_h);
            }
            MouseEventKind::Down(crossterm::event::MouseButton::Left)
                if self
                    .history_area
                    .contains(Position::new(mouse.column, mouse.row)) =>
            {
                let rel = mouse.row.saturating_sub(self.history_area.y + 1) as usize;
                if rel < vis_h {
                    let idx = (session.history_scroll + rel).min(len - 1);
                    session.history_cursor = idx;
                    session.ensure_history_cursor_visible(vis_h);
                }
            }
            _ => {}
        }
        true
    }

    /// TOAD-style session browser: pick a session row, preview its current
    /// SQL and run EXPLAIN on it.
    ///
    /// Pane layout mirrors the query tab: Tab cycles focus (List/SQL/Plan),
    /// Ctrl+Z/F11 maximizes the focused pane, Alt+Up grows the focused pane
    /// and Alt+Down shrinks it (so every pane — List, Current SQL, Plan —
    /// can be extended), Ctrl+Up/Down always sizes SQL directly, and
    /// dragging a horizontal splitter resizes with the mouse.
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
                self.grow_session_focused_pane();
                return;
            }
            (KeyModifiers::ALT, KeyCode::Down) => {
                self.shrink_session_focused_pane();
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
                match self.session_view_focus {
                    SessionViewPane::List => self.session_manager.move_session_cursor(-1),
                    SessionViewPane::Sql => self.scroll_session_sql(-1),
                    SessionViewPane::Plan => self.scroll_session_plan(-1),
                }
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
                match self.session_view_focus {
                    SessionViewPane::List => self.session_manager.move_session_cursor(1),
                    SessionViewPane::Sql => self.scroll_session_sql(1),
                    SessionViewPane::Plan => self.scroll_session_plan(1),
                }
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
                // Clear SQL so the fresh list auto-loads the cursor row's SQL
                // when it arrives (see poll_f2).
                {
                    let s = self.session_manager.active_session_mut();
                    s.plan_sql_text = None;
                    s.plan_result = None;
                }
                self.session_manager.refresh_session_list();
            }
            KeyCode::F(5) => {
                {
                    let s = self.session_manager.active_session_mut();
                    s.plan_sql_text = None;
                    s.plan_result = None;
                }
                self.session_manager.refresh_session_list();
            }
            KeyCode::F(6) => {
                self.perf_focus = PerfPane::Queries;
                self.perf_maximized = false;
                self.session_manager.enter_performance();
            }
            KeyCode::PageUp => match self.session_view_focus {
                SessionViewPane::List => self.session_manager.move_session_cursor(-10),
                SessionViewPane::Sql => self.scroll_session_sql(-10),
                SessionViewPane::Plan => self.scroll_session_plan(-10),
            },
            KeyCode::PageDown => match self.session_view_focus {
                SessionViewPane::List => self.session_manager.move_session_cursor(10),
                SessionViewPane::Sql => self.scroll_session_sql(10),
                SessionViewPane::Plan => self.scroll_session_plan(10),
            },
            KeyCode::Home => match self.session_view_focus {
                SessionViewPane::List => self.session_view_set_cursor(0),
                SessionViewPane::Sql => {
                    self.session_manager.active_session_mut().sql_scroll = 0;
                }
                SessionViewPane::Plan => {
                    let s = self.session_manager.active_session_mut();
                    s.plan_scroll = 0;
                    s.plan_hscroll = 0;
                }
            },
            KeyCode::End => match self.session_view_focus {
                SessionViewPane::List => {
                    let len = self.session_manager.visible_session_list().len();
                    self.session_view_set_cursor(len.saturating_sub(1));
                }
                SessionViewPane::Sql => {
                    let max = self.session_sql_max_scroll();
                    self.session_manager.active_session_mut().sql_scroll = max;
                }
                SessionViewPane::Plan => {
                    let max = self.session_plan_max_scroll();
                    self.session_manager.active_session_mut().plan_scroll = max;
                }
            },
            KeyCode::Left if self.session_view_focus == SessionViewPane::Plan => {
                self.scroll_session_plan_horizontal(-8);
            }
            KeyCode::Right if self.session_view_focus == SessionViewPane::Plan => {
                self.scroll_session_plan_horizontal(8);
            }
            _ => {}
        }
    }

    fn session_view_set_cursor(&mut self, idx: usize) {
        let len = self.session_manager.visible_session_list().len();
        if len == 0 {
            return;
        }
        let idx = idx.min(len - 1);
        if self.session_manager.active_session().session_view_cursor == idx {
            return;
        }
        self.session_manager
            .active_session_mut()
            .session_view_cursor = idx;
        self.session_manager.load_selected_sql();
    }

    fn session_sql_max_scroll(&self) -> usize {
        let sess = self.session_manager.active_session();
        let text = sess.plan_sql_text.as_deref().unwrap_or("");
        let width = self.session_sql_area.width.saturating_sub(2) as usize;
        let height = self.session_sql_area.height.saturating_sub(2) as usize;
        wrap_sql_lines(text, width).len().saturating_sub(height)
    }

    fn session_plan_max_scroll(&self) -> usize {
        let sess = self.session_manager.active_session();
        let lines = match sess.plan_result.as_ref() {
            Some(qr) if !qr.is_error && !qr.columns.is_empty() => qr.rows.len() + 1,
            _ => 0,
        };
        let height = self.session_plan_area.height.saturating_sub(2) as usize;
        lines.saturating_sub(height)
    }

    fn scroll_session_sql(&mut self, delta: isize) {
        let max = self.session_sql_max_scroll();
        let s = self.session_manager.active_session_mut();
        s.sql_scroll = scroll_by(s.sql_scroll, delta, max);
    }

    fn scroll_session_plan(&mut self, delta: isize) {
        let max = self.session_plan_max_scroll();
        let s = self.session_manager.active_session_mut();
        s.plan_scroll = scroll_by(s.plan_scroll, delta, max);
    }

    fn scroll_session_plan_horizontal(&mut self, delta: isize) {
        let s = self.session_manager.active_session_mut();
        s.plan_hscroll = scroll_by(s.plan_hscroll, delta, usize::MAX);
    }

    /// Grow whichever F2 pane has focus (Alt+Up). List grows its % share,
    /// SQL grows its row height, Plan grows by taking a row from SQL (then
    /// from the list once SQL is at its minimum).
    fn grow_session_focused_pane(&mut self) {
        match self.session_view_focus {
            SessionViewPane::List => {
                self.session_list_pct = (self.session_list_pct + 5).min(85);
            }
            SessionViewPane::Sql => {
                self.session_sql_height = (self.session_sql_height + 1).min(30);
            }
            SessionViewPane::Plan => {
                if self.session_sql_height > 3 {
                    self.session_sql_height -= 1;
                } else if self.session_list_pct > 15 {
                    self.session_list_pct = self.session_list_pct.saturating_sub(5).max(15);
                }
            }
        }
    }

    /// Shrink whichever F2 pane has focus (Alt+Down). Inverse of
    /// `grow_session_focused_pane`: List shrinks its % share, SQL shrinks
    /// its row height (giving space to Plan), Plan shrinks by handing a
    /// row back to SQL (then to the list once SQL is at its maximum).
    fn shrink_session_focused_pane(&mut self) {
        match self.session_view_focus {
            SessionViewPane::List => {
                self.session_list_pct = self.session_list_pct.saturating_sub(5).max(15);
            }
            SessionViewPane::Sql => {
                self.session_sql_height = self.session_sql_height.saturating_sub(1).max(3);
            }
            SessionViewPane::Plan => {
                if self.session_sql_height < 30 {
                    self.session_sql_height += 1;
                } else if self.session_list_pct < 85 {
                    self.session_list_pct = (self.session_list_pct + 5).min(85);
                }
            }
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
        // Explicit next page for the detailed preview (auto-paging also
        // triggers when scrolling near the end of the loaded rows).
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('f') {
            self.session_manager.fetch_explorer_preview_page();
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
        // Focus / zoom / resize — same as the query tab and the F2 viewer.
        // Checked before plain navigation because Alt+Up shares KeyCode::Up.
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Tab) => {
                self.explorer_focus = self.explorer_focus.next();
                return;
            }
            (KeyModifiers::SHIFT, KeyCode::BackTab) | (KeyModifiers::NONE, KeyCode::BackTab) => {
                self.explorer_focus = self.explorer_focus.prev();
                return;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('z'))
            | (KeyModifiers::CONTROL, KeyCode::Char('Z'))
            | (KeyModifiers::NONE, KeyCode::F(11)) => {
                self.explorer_maximized = !self.explorer_maximized;
                return;
            }
            (KeyModifiers::ALT, KeyCode::Up) => {
                self.explorer_split_pct = (self.explorer_split_pct + 5).min(85);
                return;
            }
            (KeyModifiers::ALT, KeyCode::Down) => {
                self.explorer_split_pct = self.explorer_split_pct.saturating_sub(5).max(25);
                return;
            }
            _ => {}
        }
        let detail_focused = self.explorer_focus == ExplorerPane::Detail;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                let session = self.session_manager.active_session_mut();
                session.mode = SessionMode::Query;
                session.explorer_filtering = false;
                self.active_window = ActiveWindow::Editor;
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => {
                if detail_focused {
                    let s = self.session_manager.active_session_mut();
                    s.explorer_detail_scroll = s.explorer_detail_scroll.saturating_sub(1);
                } else {
                    self.session_manager.move_explorer_cursor(-1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
                if detail_focused {
                    {
                        let s = self.session_manager.active_session_mut();
                        s.explorer_detail_scroll = s.explorer_detail_scroll.saturating_add(1);
                    }
                    self.session_manager.maybe_fetch_explorer_preview_page();
                } else {
                    self.session_manager.move_explorer_cursor(1);
                }
            }
            KeyCode::PageUp => {
                if detail_focused {
                    let s = self.session_manager.active_session_mut();
                    s.explorer_detail_scroll = s.explorer_detail_scroll.saturating_sub(10);
                } else {
                    self.session_manager.move_explorer_cursor(-10);
                }
            }
            KeyCode::PageDown => {
                if detail_focused {
                    {
                        let s = self.session_manager.active_session_mut();
                        s.explorer_detail_scroll = s.explorer_detail_scroll.saturating_add(10);
                    }
                    self.session_manager.maybe_fetch_explorer_preview_page();
                } else {
                    self.session_manager.move_explorer_cursor(10);
                }
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('H') => {
                if detail_focused {
                    let s = self.session_manager.active_session_mut();
                    s.explorer_detail_col_scroll = s.explorer_detail_col_scroll.saturating_sub(1);
                } else {
                    self.session_manager.collapse_explorer_cursor();
                }
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('L') => {
                if detail_focused {
                    let s = self.session_manager.active_session_mut();
                    s.explorer_detail_col_scroll = s.explorer_detail_col_scroll.saturating_add(1);
                } else {
                    self.session_manager.toggle_explorer_cursor();
                }
            }
            KeyCode::Enter => {
                if !detail_focused {
                    self.session_manager.toggle_explorer_cursor();
                }
            }
            KeyCode::Char(' ') => {
                if !detail_focused {
                    self.session_manager.toggle_explorer_cursor();
                }
            }
            KeyCode::Home => {
                if detail_focused {
                    let s = self.session_manager.active_session_mut();
                    s.explorer_detail_scroll = 0;
                    s.explorer_detail_col_scroll = 0;
                } else {
                    let session = self.session_manager.active_session_mut();
                    session.explorer_cursor = 0;
                    session.explorer_scroll = 0;
                    session.explorer_detail_scroll = 0;
                    session.explorer_detail_col_scroll = 0;
                    self.session_manager.maybe_load_detail_for_cursor();
                }
            }
            KeyCode::End => {
                if !detail_focused {
                    let len = self.session_manager.explorer_visible_rows().len();
                    {
                        let session = self.session_manager.active_session_mut();
                        session.explorer_cursor = len.saturating_sub(1);
                        session.explorer_detail_scroll = 0;
                        session.explorer_detail_col_scroll = 0;
                    }
                    self.session_manager.maybe_load_detail_for_cursor();
                }
            }
            KeyCode::Char('/') => {
                self.explorer_focus = ExplorerPane::Tree;
                let session = self.session_manager.active_session_mut();
                session.explorer_filtering = true;
            }
            KeyCode::Char('r') => self.session_manager.refresh_explorer_node(),
            KeyCode::Char('R') => self.session_manager.refresh_explorer(),
            // Reload the preview/DDL under the cursor.
            KeyCode::Char('d') | KeyCode::Char('D') => {
                self.session_manager.reload_explorer_detail()
            }
            // Toad-style F4 inside F12: open full DDL script for the highlighted object.
            KeyCode::F(4) => {
                self.session_manager.open_ddl_from_explorer();
            }
            KeyCode::F(6) => {
                self.perf_focus = PerfPane::Queries;
                self.perf_maximized = false;
                self.session_manager.enter_performance();
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
        if tree.width == 0 && tree.height == 0 && detail.width == 0 && detail.height == 0 {
            return false;
        }
        let pos = Position::new(mouse.column, mouse.row);
        let in_tree = tree.width > 0 && tree.height > 0 && tree.contains(pos);
        let in_detail = detail.width > 0 && detail.height > 0 && detail.contains(pos);
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                if in_detail {
                    self.explorer_focus = ExplorerPane::Detail;
                    let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
                    {
                        let s = self.session_manager.active_session_mut();
                        if shift {
                            s.explorer_detail_col_scroll =
                                s.explorer_detail_col_scroll.saturating_add(3);
                        } else {
                            s.explorer_detail_scroll = s.explorer_detail_scroll.saturating_add(3);
                        }
                    }
                    if !shift {
                        self.session_manager.maybe_fetch_explorer_preview_page();
                    }
                } else if in_tree {
                    self.explorer_focus = ExplorerPane::Tree;
                    let s = self.session_manager.active_session_mut();
                    s.explorer_scroll = s.explorer_scroll.saturating_add(3);
                }
                return true;
            }
            MouseEventKind::ScrollUp => {
                if in_detail {
                    self.explorer_focus = ExplorerPane::Detail;
                    let s = self.session_manager.active_session_mut();
                    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                        s.explorer_detail_col_scroll =
                            s.explorer_detail_col_scroll.saturating_sub(3);
                    } else {
                        s.explorer_detail_scroll = s.explorer_detail_scroll.saturating_sub(3);
                    }
                } else if in_tree {
                    self.explorer_focus = ExplorerPane::Tree;
                    let s = self.session_manager.active_session_mut();
                    s.explorer_scroll = s.explorer_scroll.saturating_sub(3);
                }
                return true;
            }
            MouseEventKind::ScrollLeft => {
                if in_detail {
                    self.explorer_focus = ExplorerPane::Detail;
                    let s = self.session_manager.active_session_mut();
                    s.explorer_detail_col_scroll = s.explorer_detail_col_scroll.saturating_sub(3);
                }
                return true;
            }
            MouseEventKind::ScrollRight => {
                if in_detail {
                    self.explorer_focus = ExplorerPane::Detail;
                    let s = self.session_manager.active_session_mut();
                    s.explorer_detail_col_scroll = s.explorer_detail_col_scroll.saturating_add(3);
                }
                return true;
            }
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                if in_detail {
                    self.explorer_focus = ExplorerPane::Detail;
                    return true;
                }
                if !in_tree {
                    return true;
                }
                self.explorer_focus = ExplorerPane::Tree;
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
                        s.explorer_detail_col_scroll = 0;
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

    // ---------------- F4 Toad-style Object DDL Handlers ----------------

    fn ddl_max_scroll(&self) -> usize {
        let sess = self.session_manager.active_session();
        let total_lines = sess
            .ddl_object
            .as_ref()
            .map(|o| o.ddl.lines().count())
            .unwrap_or(0);
        let view_h = self.ddl_area.height.saturating_sub(5).max(1) as usize;
        total_lines.saturating_sub(view_h)
    }

    fn copy_ddl_to_clipboard(&mut self) {
        let info_opt = {
            let sess = self.session_manager.active_session();
            sess.ddl_object
                .as_ref()
                .map(|o| (format!("{}.{}", o.schema, o.name), o.ddl.clone()))
        };
        let Some((qual, ddl)) = info_opt else {
            self.session_manager
                .active_session_mut()
                .set_status("No DDL loaded yet to copy");
            return;
        };
        let chars = ddl.chars().count();
        let outcome = self.copy_and_remember(&ddl);
        let msg = Self::copy_status(outcome, chars, &format!("{} DDL", qual));
        self.session_manager.active_session_mut().set_status(msg);
    }

    fn handle_object_ddl(&mut self, key: event::KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('q') | KeyCode::Char('Q') => {
                    self.should_quit = true;
                    return;
                }
                KeyCode::Char('c') | KeyCode::Char('C') => {
                    if self.session_manager.active_session().ddl_loading {
                        self.session_manager.cancel_query();
                    } else {
                        self.copy_ddl_to_clipboard();
                    }
                    return;
                }
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.copy_ddl_to_clipboard();
                    return;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc | KeyCode::F(4) | KeyCode::Char('q') | KeyCode::Char('Q') => {
                if self.session_manager.active_session().ddl_loading {
                    self.session_manager.cancel_query();
                }
                let sess = self.session_manager.active_session_mut();
                let ret = sess.ddl_return_mode.clone();
                sess.mode = if matches!(ret, SessionMode::DbExplorer) {
                    SessionMode::DbExplorer
                } else {
                    SessionMode::Query
                };
                if sess.mode == SessionMode::Query {
                    self.active_window = ActiveWindow::Editor;
                }
            }
            KeyCode::Char('c') | KeyCode::Char('C') => {
                self.copy_ddl_to_clipboard();
            }
            KeyCode::Enter
            | KeyCode::Char('i')
            | KeyCode::Char('I')
            | KeyCode::Char('s')
            | KeyCode::Char('S') => {
                let ddl_opt = self
                    .session_manager
                    .active_session()
                    .ddl_object
                    .as_ref()
                    .map(|o| (format!("{}.{}", o.schema, o.name), o.ddl.clone()));
                if let Some((qual, ddl)) = ddl_opt {
                    let sess = self.session_manager.active_session_mut();
                    sess.editor.set_text(&ddl);
                    sess.mode = SessionMode::Query;
                    self.active_window = ActiveWindow::Editor;
                    sess.set_status(format!("Loaded {} DDL into SQL editor", qual));
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                let ddl_opt = self
                    .session_manager
                    .active_session()
                    .ddl_object
                    .as_ref()
                    .map(|o| (format!("{}.{}", o.schema, o.name), o.ddl.clone()));
                if let Some((qual, ddl)) = ddl_opt {
                    // Return the origin tab to its prior mode first, then open the new tab.
                    {
                        let origin = self.session_manager.active_session_mut();
                        origin.mode = origin.ddl_return_mode.clone();
                    }
                    self.session_manager.add_session();
                    let new_sess = self.session_manager.active_session_mut();
                    new_sess.editor.set_text(&ddl);
                    new_sess.mode = SessionMode::Query;
                    self.active_window = ActiveWindow::Editor;
                    new_sess.set_status(format!("Opened {} DDL in new session tab", qual));
                }
            }
            KeyCode::Char('r') | KeyCode::Char('R') | KeyCode::F(5) => {
                self.session_manager.reload_ddl();
            }
            KeyCode::F(2) => {
                self.session_manager.enter_session_view();
            }
            KeyCode::F(6) => {
                self.perf_focus = PerfPane::Queries;
                self.perf_maximized = false;
                self.session_manager.enter_performance();
            }
            KeyCode::F(12) => {
                self.session_manager.enter_db_explorer();
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => {
                let s = self.session_manager.active_session_mut();
                s.ddl_scroll = s.ddl_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
                let max = self.ddl_max_scroll();
                let s = self.session_manager.active_session_mut();
                s.ddl_scroll = scroll_by(s.ddl_scroll, 1, max);
            }
            KeyCode::PageUp => {
                let s = self.session_manager.active_session_mut();
                s.ddl_scroll = s.ddl_scroll.saturating_sub(20);
            }
            KeyCode::PageDown => {
                let max = self.ddl_max_scroll();
                let s = self.session_manager.active_session_mut();
                s.ddl_scroll = scroll_by(s.ddl_scroll, 20, max);
            }
            KeyCode::Home => {
                let s = self.session_manager.active_session_mut();
                s.ddl_scroll = 0;
                s.ddl_hscroll = 0;
            }
            KeyCode::End => {
                let max = self.ddl_max_scroll();
                self.session_manager.active_session_mut().ddl_scroll = max;
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('H') => {
                let s = self.session_manager.active_session_mut();
                s.ddl_hscroll = s.ddl_hscroll.saturating_sub(8);
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('L') => {
                let s = self.session_manager.active_session_mut();
                s.ddl_hscroll = s.ddl_hscroll.saturating_add(8);
            }
            _ => {}
        }
    }

    fn handle_object_ddl_mouse(&mut self, mouse: MouseEvent) -> bool {
        let max = self.ddl_max_scroll();
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
                let s = self.session_manager.active_session_mut();
                if shift {
                    s.ddl_hscroll = s.ddl_hscroll.saturating_add(6);
                } else {
                    s.ddl_scroll = scroll_by(s.ddl_scroll, 3, max);
                }
            }
            MouseEventKind::ScrollUp => {
                let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
                let s = self.session_manager.active_session_mut();
                if shift {
                    s.ddl_hscroll = s.ddl_hscroll.saturating_sub(6);
                } else {
                    s.ddl_scroll = s.ddl_scroll.saturating_sub(3);
                }
            }
            MouseEventKind::ScrollLeft => {
                let s = self.session_manager.active_session_mut();
                s.ddl_hscroll = s.ddl_hscroll.saturating_sub(6);
            }
            MouseEventKind::ScrollRight => {
                let s = self.session_manager.active_session_mut();
                s.ddl_hscroll = s.ddl_hscroll.saturating_add(6);
            }
            _ => {}
        }
        true
    }

    // ---------------- F6 Performance & DBA Dashboard Handlers ----------------

    fn perf_result_bounds(&self) -> (usize, usize) {
        let sess = self.session_manager.active_session();
        match sess.perf_results.get(&sess.perf_cursor) {
            Some(qr) if !qr.is_error => (
                qr.rows.len().saturating_sub(1),
                qr.columns.len().saturating_sub(1),
            ),
            _ => (0, 0),
        }
    }

    fn copy_perf_to_clipboard(&mut self) {
        if self.perf_focus == PerfPane::Sql {
            if let Some(sql) = self.session_manager.active_perf_query_sql() {
                let chars = sql.chars().count();
                let outcome = self.copy_and_remember(&sql);
                let msg = Self::copy_status(outcome, chars, "diagnostic SQL");
                self.session_manager.active_session_mut().set_status(msg);
            }
            return;
        }

        let text_opt = {
            let sess = self.session_manager.active_session();
            sess.perf_results.get(&sess.perf_cursor).and_then(|qr| {
                if qr.is_error || qr.columns.is_empty() {
                    return None;
                }
                let text = match self.result_format {
                    ResultFormat::Markdown => {
                        let refs: Vec<&[String]> = qr.rows.iter().map(|r| r.as_slice()).collect();
                        markdown_table(&qr.columns, &refs)
                    }
                    ResultFormat::Ascii => {
                        format_ascii_table(qr, 0..qr.columns.len(), 0, qr.rows.len())
                    }
                    ResultFormat::Table => {
                        let mut out = qr.columns.join("\t");
                        out.push('\n');
                        for row in &qr.rows {
                            out.push_str(&row.join("\t"));
                            out.push('\n');
                        }
                        out
                    }
                };
                Some(text)
            })
        };
        let Some(text) = text_opt else {
            self.session_manager
                .active_session_mut()
                .set_status("No performance results to copy yet");
            return;
        };
        let chars = text.chars().count();
        let outcome = self.copy_and_remember(&text);
        let msg = Self::copy_status(outcome, chars, "performance results");
        self.session_manager.active_session_mut().set_status(msg);
    }

    fn handle_performance(&mut self, key: event::KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('q') | KeyCode::Char('Q') => {
                    self.should_quit = true;
                    return;
                }
                KeyCode::Char('d') | KeyCode::Char('D') => {
                    self.result_format = self.result_format.cycle();
                    return;
                }
                KeyCode::Char('z') | KeyCode::Char('Z') => {
                    self.perf_maximized = !self.perf_maximized;
                    return;
                }
                KeyCode::Char('c') | KeyCode::Char('C') => {
                    if self.session_manager.active_session().perf_loading {
                        self.session_manager.cancel_query();
                    } else {
                        self.copy_perf_to_clipboard();
                    }
                    return;
                }
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.copy_perf_to_clipboard();
                    return;
                }
                _ => {}
            }
        }

        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Tab) => {
                let show_sql = self.session_manager.active_session().perf_show_sql;
                self.perf_focus = self.perf_focus.next(show_sql);
                return;
            }
            (KeyModifiers::SHIFT, KeyCode::BackTab) | (KeyModifiers::NONE, KeyCode::BackTab) => {
                let show_sql = self.session_manager.active_session().perf_show_sql;
                self.perf_focus = self.perf_focus.prev(show_sql);
                return;
            }
            (KeyModifiers::NONE, KeyCode::F(11)) => {
                self.perf_maximized = !self.perf_maximized;
                return;
            }
            (KeyModifiers::ALT, KeyCode::Up) => {
                self.perf_split_pct = (self.perf_split_pct + 4).min(65);
                return;
            }
            (KeyModifiers::ALT, KeyCode::Down) => {
                self.perf_split_pct = self.perf_split_pct.saturating_sub(4).max(22);
                return;
            }
            _ => {}
        }

        match key.code {
            KeyCode::Esc | KeyCode::F(6) | KeyCode::Char('q') | KeyCode::Char('Q') => {
                self.session_manager.active_session_mut().mode = SessionMode::Query;
                self.active_window = ActiveWindow::Editor;
            }
            KeyCode::F(1) => {
                self.session_manager.active_session_mut().mode = SessionMode::Help;
            }
            KeyCode::F(2) => {
                self.session_manager.enter_session_view();
            }
            KeyCode::F(12) => {
                self.session_manager.enter_db_explorer();
            }
            KeyCode::Char(c @ '1'..='9') => {
                let idx = (c as usize) - ('1' as usize);
                self.session_manager.select_perf_query(idx, true);
            }
            KeyCode::Char('0') => {
                self.session_manager.select_perf_query(9, true);
            }
            KeyCode::Enter | KeyCode::Char('r') => {
                self.session_manager.refresh_performance(false);
            }
            KeyCode::Char('R') | KeyCode::F(5) => {
                self.session_manager.refresh_performance(true);
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                self.session_manager.toggle_perf_auto_refresh();
            }
            KeyCode::Char('v') | KeyCode::Char('V') => {
                let show = {
                    let s = self.session_manager.active_session_mut();
                    s.perf_show_sql = !s.perf_show_sql;
                    s.perf_sql_scroll = 0;
                    s.perf_show_sql
                };
                if !show && self.perf_focus == PerfPane::Sql {
                    self.perf_focus = PerfPane::Results;
                }
            }
            KeyCode::Char('s')
            | KeyCode::Char('S')
            | KeyCode::Char('i')
            | KeyCode::Char('I') => {
                if let Some(sql) = self.session_manager.active_perf_query_sql() {
                    let sess = self.session_manager.active_session_mut();
                    sess.editor.set_text(&sql);
                    sess.mode = SessionMode::Query;
                    self.active_window = ActiveWindow::Editor;
                    sess.set_status("Loaded diagnostic SQL into editor");
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                if let Some(sql) = self.session_manager.active_perf_query_sql() {
                    self.session_manager.active_session_mut().mode = SessionMode::Query;
                    self.session_manager.add_session();
                    let new_sess = self.session_manager.active_session_mut();
                    new_sess.editor.set_text(&sql);
                    new_sess.mode = SessionMode::Query;
                    self.active_window = ActiveWindow::Editor;
                    new_sess.set_status("Opened diagnostic SQL in new session tab");
                }
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => match self.perf_focus {
                PerfPane::Queries => self.session_manager.move_perf_cursor(-1),
                PerfPane::Results => {
                    let s = self.session_manager.active_session_mut();
                    s.perf_scroll = s.perf_scroll.saturating_sub(1);
                }
                PerfPane::Sql => {
                    let s = self.session_manager.active_session_mut();
                    s.perf_sql_scroll = s.perf_sql_scroll.saturating_sub(1);
                }
            },
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => match self.perf_focus {
                PerfPane::Queries => self.session_manager.move_perf_cursor(1),
                PerfPane::Results => {
                    let (max_r, _) = self.perf_result_bounds();
                    let s = self.session_manager.active_session_mut();
                    s.perf_scroll = scroll_by(s.perf_scroll, 1, max_r);
                }
                PerfPane::Sql => {
                    let s = self.session_manager.active_session_mut();
                    s.perf_sql_scroll = s.perf_sql_scroll.saturating_add(1);
                }
            },
            KeyCode::PageUp => match self.perf_focus {
                PerfPane::Queries => self.session_manager.move_perf_cursor(-5),
                PerfPane::Results => {
                    let s = self.session_manager.active_session_mut();
                    s.perf_scroll = s.perf_scroll.saturating_sub(15);
                }
                PerfPane::Sql => {
                    let s = self.session_manager.active_session_mut();
                    s.perf_sql_scroll = s.perf_sql_scroll.saturating_sub(5);
                }
            },
            KeyCode::PageDown => match self.perf_focus {
                PerfPane::Queries => self.session_manager.move_perf_cursor(5),
                PerfPane::Results => {
                    let (max_r, _) = self.perf_result_bounds();
                    let s = self.session_manager.active_session_mut();
                    s.perf_scroll = scroll_by(s.perf_scroll, 15, max_r);
                }
                PerfPane::Sql => {
                    let s = self.session_manager.active_session_mut();
                    s.perf_sql_scroll = s.perf_sql_scroll.saturating_add(5);
                }
            },
            KeyCode::Home => match self.perf_focus {
                PerfPane::Queries => self.session_manager.select_perf_query(0, false),
                PerfPane::Results => {
                    let s = self.session_manager.active_session_mut();
                    s.perf_scroll = 0;
                    s.perf_col_scroll = 0;
                }
                PerfPane::Sql => {
                    self.session_manager.active_session_mut().perf_sql_scroll = 0;
                }
            },
            KeyCode::End => match self.perf_focus {
                PerfPane::Queries => {
                    let total =
                        crate::db::perf::perf_queries_for(self.session_manager.active_db_type())
                            .len();
                    if total > 0 {
                        self.session_manager.select_perf_query(total - 1, false);
                    }
                }
                PerfPane::Results => {
                    let (max_r, _) = self.perf_result_bounds();
                    self.session_manager.active_session_mut().perf_scroll = max_r;
                }
                PerfPane::Sql => {}
            },
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('H')
                if self.perf_focus == PerfPane::Results =>
            {
                let s = self.session_manager.active_session_mut();
                s.perf_col_scroll = s.perf_col_scroll.saturating_sub(1);
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('L')
                if self.perf_focus == PerfPane::Results =>
            {
                let (_, max_c) = self.perf_result_bounds();
                let s = self.session_manager.active_session_mut();
                s.perf_col_scroll = scroll_by(s.perf_col_scroll, 1, max_c);
            }
            _ => {}
        }
    }

    fn handle_performance_mouse(&mut self, mouse: MouseEvent) -> bool {
        let pos = Position::new(mouse.column, mouse.row);
        let in_q = self.perf_queries_area.width > 0
            && self.perf_queries_area.height > 0
            && self.perf_queries_area.contains(pos);
        let in_r = self.perf_results_area.width > 0
            && self.perf_results_area.height > 0
            && self.perf_results_area.contains(pos);
        let in_s = self.perf_sql_area.width > 0
            && self.perf_sql_area.height > 0
            && self.perf_sql_area.contains(pos);

        match mouse.kind {
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                if in_q {
                    self.perf_focus = PerfPane::Queries;
                    let rel = mouse.row.saturating_sub(self.perf_queries_area.y + 1) as usize;
                    let count =
                        crate::db::perf::perf_queries_for(self.session_manager.active_db_type())
                            .len();
                    let inner_h = self.perf_queries_area.height.saturating_sub(2) as usize;
                    let list_h = if inner_h >= 8 { inner_h - 2 } else { inner_h };
                    if rel < list_h && count > 0 {
                        let cursor = self.session_manager.active_session().perf_cursor;
                        let start = if list_h == 0 || cursor < list_h {
                            0
                        } else {
                            cursor + 1 - list_h
                        };
                        let idx = (start + rel).min(count - 1);
                        self.session_manager.select_perf_query(idx, false);
                    }
                } else if in_r {
                    self.perf_focus = PerfPane::Results;
                } else if in_s {
                    self.perf_focus = PerfPane::Sql;
                }
            }
            MouseEventKind::ScrollDown => {
                if in_q {
                    self.perf_focus = PerfPane::Queries;
                    self.session_manager.move_perf_cursor(1);
                } else if in_r {
                    self.perf_focus = PerfPane::Results;
                    let (max_r, max_c) = self.perf_result_bounds();
                    let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
                    let s = self.session_manager.active_session_mut();
                    if shift {
                        s.perf_col_scroll = scroll_by(s.perf_col_scroll, 2, max_c);
                    } else {
                        s.perf_scroll = scroll_by(s.perf_scroll, 3, max_r);
                    }
                } else if in_s {
                    self.perf_focus = PerfPane::Sql;
                    let s = self.session_manager.active_session_mut();
                    s.perf_sql_scroll = s.perf_sql_scroll.saturating_add(2);
                }
            }
            MouseEventKind::ScrollUp => {
                if in_q {
                    self.perf_focus = PerfPane::Queries;
                    self.session_manager.move_perf_cursor(-1);
                } else if in_r {
                    self.perf_focus = PerfPane::Results;
                    let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
                    let s = self.session_manager.active_session_mut();
                    if shift {
                        s.perf_col_scroll = s.perf_col_scroll.saturating_sub(2);
                    } else {
                        s.perf_scroll = s.perf_scroll.saturating_sub(3);
                    }
                } else if in_s {
                    self.perf_focus = PerfPane::Sql;
                    let s = self.session_manager.active_session_mut();
                    s.perf_sql_scroll = s.perf_sql_scroll.saturating_sub(2);
                }
            }
            _ => {}
        }
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
                        SessionViewPane::Sql => self.scroll_session_sql(3),
                        SessionViewPane::Plan => {
                            if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                                self.scroll_session_plan_horizontal(3);
                            } else {
                                self.scroll_session_plan(3);
                            }
                        }
                    }
                    return true;
                }
                MouseEventKind::ScrollUp => {
                    match self.session_view_focus {
                        SessionViewPane::List => {
                            self.session_manager.move_session_cursor(-1);
                        }
                        SessionViewPane::Sql => self.scroll_session_sql(-3),
                        SessionViewPane::Plan => {
                            if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                                self.scroll_session_plan_horizontal(-3);
                            } else {
                                self.scroll_session_plan(-3);
                            }
                        }
                    }
                    return true;
                }
                MouseEventKind::ScrollLeft => {
                    if self.session_view_focus == SessionViewPane::Plan {
                        self.scroll_session_plan_horizontal(-3);
                    }
                    return true;
                }
                MouseEventKind::ScrollRight => {
                    if self.session_view_focus == SessionViewPane::Plan {
                        self.scroll_session_plan_horizontal(3);
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
                    self.session_view_focus = SessionViewPane::Plan;
                    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                        self.scroll_session_plan_horizontal(3);
                    } else {
                        self.scroll_session_plan(3);
                    }
                } else if self.session_sql_area.contains(pos) {
                    self.session_view_focus = SessionViewPane::Sql;
                    self.scroll_session_sql(3);
                } else if self.session_list_area.contains(pos) {
                    self.session_view_focus = SessionViewPane::List;
                    self.session_manager.move_session_cursor(1);
                }
                return true;
            }
            MouseEventKind::ScrollUp => {
                if self.session_plan_area.contains(pos) {
                    self.session_view_focus = SessionViewPane::Plan;
                    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                        self.scroll_session_plan_horizontal(-3);
                    } else {
                        self.scroll_session_plan(-3);
                    }
                } else if self.session_sql_area.contains(pos) {
                    self.session_view_focus = SessionViewPane::Sql;
                    self.scroll_session_sql(-3);
                } else if self.session_list_area.contains(pos) {
                    self.session_view_focus = SessionViewPane::List;
                    self.session_manager.move_session_cursor(-1);
                }
                return true;
            }
            MouseEventKind::ScrollLeft => {
                if self.session_plan_area.contains(pos) {
                    self.session_view_focus = SessionViewPane::Plan;
                    self.scroll_session_plan_horizontal(-3);
                }
                return true;
            }
            MouseEventKind::ScrollRight => {
                if self.session_plan_area.contains(pos) {
                    self.session_view_focus = SessionViewPane::Plan;
                    self.scroll_session_plan_horizontal(3);
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
                let in_h_range = mouse.column >= self.session_view_area.x
                    && mouse.column
                        < self.session_view_area.x + self.session_view_area.width.max(1);
                if in_h_range
                    && (mouse.row as i16 - list_bottom as i16).abs() <= 1
                    && self.session_list_area.height > 0
                {
                    self.session_resizing = Some(SessionResize::List);
                    self.session_view_focus = SessionViewPane::List;
                    return true;
                }
                if in_h_range
                    && (mouse.row as i16 - sql_bottom as i16).abs() <= 1
                    && self.session_sql_area.height > 0
                {
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
                            let start = if cursor >= avail {
                                cursor + 1 - avail
                            } else {
                                0
                            };
                            let idx = (start + rel).min(total.saturating_sub(1));
                            if idx != cursor {
                                self.session_manager
                                    .active_session_mut()
                                    .session_view_cursor = idx;
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

    /// Cycle config.yml `connections[]` into the open dialog (Alt+N / Alt+P).
    /// Returns true when a saved entry was loaded.
    fn cycle_saved_connection(&mut self, dir: isize) -> bool {
        if self.config.saved_connections.is_empty() {
            self.session_manager
                .active_session_mut()
                .set_status("No saved connections in config.yml connections[]");
            return true;
        }
        let cur_idx = {
            let dlg = &self.session_manager.active_session().conn_dialog;
            self.config.saved_connections.iter().position(|e| {
                let db = e.db_type.unwrap_or(crate::db::DbType::Oracle);
                db == dlg.db_type && e.host == dlg.host && e.user == dlg.user
            })
        };
        let n = self.config.saved_connections.len() as isize;
        let next = match cur_idx {
            Some(i) => (i as isize + dir).rem_euclid(n) as usize,
            None => {
                if dir > 0 {
                    0
                } else {
                    n as usize - 1
                }
            }
        };
        let entry = self.config.saved_connections[next].clone();
        {
            let sess = self.session_manager.active_session_mut();
            let dlg = &mut sess.conn_dialog;
            dlg.set_db_type(entry.db_type.unwrap_or(crate::db::DbType::Oracle));
            dlg.host = entry.host.clone();
            dlg.port = entry.port.to_string();
            dlg.service = entry.service.clone();
            dlg.database = entry
                .database
                .clone()
                .unwrap_or_else(|| entry.service.clone());
            if dlg.db_type == crate::db::DbType::Oracle && dlg.service.is_empty() {
                dlg.service = "ORCL".into();
            }
            if dlg.db_type == crate::db::DbType::Postgres && dlg.database.is_empty() {
                dlg.database = "postgres".into();
            }
            dlg.user = entry.user.clone();
            dlg.password = entry.password.clone().unwrap_or_default();
            let field = dlg.active_field;
            dlg.select_field(field);
            sess.set_status(format!(
                "Loaded saved connection '{}' ({}/{}) — Alt+N/P cycles",
                entry.name,
                next + 1,
                n
            ));
        }
        true
    }

    fn handle_conn_dialog(&mut self, key: event::KeyEvent) {
        let session = self.session_manager.active_session_mut();
        match key.code {
            KeyCode::Esc => {
                if let Some(prev) = session.conn_dialog_backup.take() {
                    session.conn_dialog = prev;
                }
                session.mode = SessionMode::Query;
                return;
            }
            KeyCode::Enter => {
                let params = session.conn_dialog.to_params();
                session.conn_dialog_backup = None;
                session.mode = SessionMode::Query;
                self.session_manager.connect_active(&params);
                return;
            }
            _ => {}
        }

        let dlg = &mut session.conn_dialog;
        match key.code {
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
            KeyCode::Home => dlg.cursor_home(),
            KeyCode::Char('a') if key.modifiers == KeyModifiers::CONTROL => dlg.cursor_home(),
            KeyCode::End => dlg.cursor_end(),
            KeyCode::Char('e') if key.modifiers == KeyModifiers::CONTROL => dlg.cursor_end(),
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
            self.help_area = size;
            let scroll = self.session_manager.active_session().help_scroll;
            render_help(scroll, true, f, size);
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
        self.tabs_area = chunks[0];

        let mode = self.session_manager.active_session().mode.clone();

        // View switches (and maximize toggles) can leave ghost characters
        // from the previous frame when a terminal or proxy drops part of
        // the diff: clear the content area once so the view repaints fully.
        let view_key = self.view_key();
        if self.last_view_key.as_ref() != Some(&view_key) {
            f.render_widget(Clear, chunks[1]);
            self.last_view_key = Some(view_key);
        }

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
                let layout = ExplorerLayout {
                    split_pct: self.explorer_split_pct,
                    focus: self.explorer_focus,
                    maximized: self.explorer_maximized,
                };
                let (tree, detail) = render_db_explorer(
                    &mut self.session_manager,
                    self.result_format,
                    layout,
                    f,
                    chunks[1],
                );
                self.explorer_area = tree;
                self.explorer_detail_area = detail;
            }
            SessionMode::ObjectDdl => {
                let session = self.session_manager.active_session();
                render_object_ddl(session, self.mouse_capture, f, chunks[1]);
                self.ddl_area = chunks[1];
            }
            SessionMode::Performance => {
                let layout = PerfLayout {
                    split_pct: self.perf_split_pct,
                    focus: self.perf_focus,
                    maximized: self.perf_maximized,
                };
                let (qa, ra, sa) = render_performance(
                    &self.session_manager,
                    self.result_format,
                    layout,
                    f,
                    chunks[1],
                );
                self.perf_queries_area = qa;
                self.perf_results_area = ra;
                self.perf_sql_area = sa;
            }
            SessionMode::Help => {
                self.help_area = chunks[1];
                let scroll = self.session_manager.active_session().help_scroll;
                render_help(scroll, false, f, chunks[1]);
            }
            SessionMode::History => {
                self.history_area = chunks[1];
                let vis_h = chunks[1].height.saturating_sub(2) as usize;
                self.session_manager
                    .active_session_mut()
                    .ensure_history_cursor_visible(vis_h);
                let session = self.session_manager.active_session();
                render_history(session, f, chunks[1]);
            }
            SessionMode::ConnectionPickerDialog => {
                let session = self.session_manager.active_session();
                render_connection_dialog(session, f, chunks[1]);
            }
            _ => {
                // Pin scroll offsets into the loaded data every frame: keys
                // and wheel only grow them, so overscroll would otherwise
                // make the opposite direction feel dead (in any format).
                self.clamp_result_scroll();
                // Fresh table geometry from this frame's render (for mouse
                // hit-testing); stored after the last `session` use below.
                let mut new_results_layout: Option<ResultsLayout> = None;
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
                        // Responsive split: the sidebar is a fixed info panel,
                        // so on a wide monitor it must not eat 25% of the
                        // table width. Cap its share as the screen grows, so
                        // the result viewer gets the extra columns.
                        let sidebar_pct = effective_sidebar_pct(
                            size.width,
                            self.sidebar_split_pct,
                        );
                        let left_pct = 100 - sidebar_pct;
                        let (left, right) = if self.hide_sidebar {
                            (chunks[1], Rect::default())
                        } else {
                            let h_split = Layout::default()
                                .direction(Direction::Horizontal)
                                .constraints([
                                    Constraint::Percentage(left_pct),
                                    Constraint::Percentage(sidebar_pct),
                                ])
                                .split(chunks[1]);
                            (h_split[0], h_split[1])
                        };

                        // On short screens a fixed 12-row editor would starve
                        // the table; shrink it so results keep at least ~8
                        // rows. On tall screens the editor stays fixed and
                        // every extra row goes to the table.
                        let editor_h =
                            effective_editor_height(chunks[1].height, self.editor_height);
                        let main_chunks = Layout::default()
                            .direction(Direction::Vertical)
                            .constraints([
                                Constraint::Length(editor_h),
                                Constraint::Min(4),
                            ])
                            .split(left);

                        self.editor_area = main_chunks[0];
                        self.results_area = main_chunks[1];
                        self.sidebar_area = right;
                    }
                }

                if self.editor_area.height > 0 {
                    let frame = if self.mouse_capture { 2 } else { 0 };
                    let vis_height = self.editor_area.height.saturating_sub(frame) as usize;
                    let gutter = self.editor_gutter_width();
                    let vis_width = self
                        .editor_area
                        .width
                        .saturating_sub(frame * 2 + gutter)
                        as usize;
                    self.session_manager
                        .active_session_mut()
                        .editor
                        .ensure_cursor_visible_sized(vis_height, vis_width.max(1));
                }

                let session = self.session_manager.active_session();
                let editor_hl = self.editor_search_highlight();
                let table_hl = self.table_search_highlight();

                match self.maximized_box {
                    Some(MaximizedBox::Editor) => {
                        render_editor(
                            session,
                            self.active_window == ActiveWindow::Editor,
                            self.mouse_capture,
                            editor_hl.as_ref(),
                            f,
                            self.editor_area,
                        );
                        if self.active_window == ActiveWindow::Editor {
                            if let Some(popup) = session.completion.as_ref() {
                                render_completion_popup(
                                    session,
                                    popup,
                                    self.mouse_capture,
                                    f,
                                    self.editor_area,
                                );
                            }
                        }
                    }
                    Some(MaximizedBox::Results) => {
                        let current_res = session.results.last();
                        new_results_layout = Some(render_table(
                            session,
                            &current_res,
                            TableViewState {
                                scroll_offset: session.scroll_offset,
                                col_scroll_offset: session.col_scroll_offset,
                                result_format: self.result_format,
                                focused: self.active_window == ActiveWindow::Results,
                            },
                            table_hl.as_ref(),
                            f,
                            self.results_area,
                        ));
                    }
                    Some(MaximizedBox::Sidebar) => {
                        render_right_panel(session, f, self.sidebar_area);
                    }
                    None => {
                        render_editor(
                            session,
                            self.active_window == ActiveWindow::Editor,
                            self.mouse_capture,
                            editor_hl.as_ref(),
                            f,
                            self.editor_area,
                        );
                        if self.active_window == ActiveWindow::Editor {
                            if let Some(popup) = session.completion.as_ref() {
                                render_completion_popup(
                                    session,
                                    popup,
                                    self.mouse_capture,
                                    f,
                                    self.editor_area,
                                );
                            }
                        }
                        let current_res = session.results.last();
                        new_results_layout = Some(render_table(
                            session,
                            &current_res,
                            TableViewState {
                                scroll_offset: session.scroll_offset,
                                col_scroll_offset: session.col_scroll_offset,
                                result_format: self.result_format,
                                focused: self.active_window == ActiveWindow::Results,
                            },
                            table_hl.as_ref(),
                            f,
                            self.results_area,
                        ));
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
                    let frame: u16 = if self.mouse_capture { 1 } else { 0 };
                    let vis_height = area.height.saturating_sub(frame * 2) as usize;
                    let row = session.editor.cursor_row;
                    let scroll = session.editor.scroll_offset;
                    if row >= scroll && row < scroll + vis_height.max(1) {
                        let gutter = self.editor_gutter_width();
                        let max_text_width =
                            area.width.saturating_sub(frame * 2 + gutter) as usize;
                        let line = session
                            .editor
                            .lines
                            .get(row)
                            .map(String::as_str)
                            .unwrap_or("");
                        let mut col_bytes = session.editor.cursor_col.min(line.len());
                        while col_bytes > 0 && !line.is_char_boundary(col_bytes) {
                            col_bytes -= 1;
                        }
                        let visual_col = UnicodeWidthStr::width(&line[..col_bytes]);
                        let rel = visual_col.saturating_sub(session.editor.col_offset);
                        let cx = area.x + frame + gutter + rel.min(max_text_width) as u16;
                        let cy = area.y + frame + (row - scroll) as u16;
                        f.set_cursor_position(Position::new(cx, cy));
                    }
                }
                // Last `session` use is behind us — publish this frame's
                // table geometry for mouse hit-testing.
                if let Some(layout) = new_results_layout.take() {
                    self.results_layout = layout;
                }
            }
        }

        let session = self.session_manager.active_session();
        if self.search.is_some() {
            let (query, forward, current, total, failing, wrapped, target) = match self
                .search
                .as_ref()
            {
                Some(st) => (
                    st.query.clone(),
                    st.direction == SearchDirection::Forward,
                    st.current,
                    st.matches.len(),
                    st.failing,
                    st.wrapped,
                    st.target,
                ),
                None => (String::new(), true, 0, 0, false, false, SearchTarget::None),
            };
            render_search_bar(
                &query,
                forward,
                current,
                total,
                failing,
                wrapped,
                Self::search_target_label(target),
                self.search_current_snippet().as_deref(),
                f,
                chunks[2],
            );
            // Put the terminal cursor at the end of the query.
            let bar = chunks[2];
            let prefix_w = format!(" I-search {} [{}]: ", if forward { "forward" } else { "backward" }, Self::search_target_label(target)).chars().count() as u16;
            let cx = (bar.x + prefix_w + query.chars().count() as u16 + 1)
                .min(bar.right().saturating_sub(1));
            f.set_cursor_position(Position::new(cx, bar.y));
        } else if self.command_mode {
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
                self.explorer_focus,
                self.explorer_maximized,
                self.perf_focus,
                self.perf_maximized,
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
    fn scroll_offsets_clamp_into_loaded_data() {
        // In-range offsets pass through untouched.
        assert_eq!(clamp_scroll_offsets(3, 1, 10, 4), (3, 1));
        // Overscroll (keys/wheel only grow the offsets) pins to the last
        // row/column instead of accumulating invisibly.
        assert_eq!(clamp_scroll_offsets(1000, 99, 10, 4), (9, 3));
        // Empty data pins to zero.
        assert_eq!(clamp_scroll_offsets(5, 5, 0, 0), (0, 0));
    }

    #[test]
    fn paste_normalizes_crlf_and_lone_cr() {
        assert_eq!(normalize_paste("a\r\nb"), "a\nb");
        assert_eq!(normalize_paste("a\rb"), "a\nb");
        assert_eq!(normalize_paste("a\nb"), "a\nb");
        assert_eq!(normalize_paste("a\r\n\r\nb"), "a\n\nb");
    }

    #[test]
    fn key_burst_reassembles_into_pasted_text() {
        // git-bash/mintty pastes arrive as plain key events; Enter/Tab must
        // become text (not execute/cycle focus) and Shift still types caps.
        let burst = vec![
            Event::Key(key(KeyModifiers::NONE, KeyCode::Char('s'))),
            Event::Key(key(KeyModifiers::NONE, KeyCode::Char('e'))),
            Event::Key(key(KeyModifiers::NONE, KeyCode::Char('l'))),
            Event::Key(key(KeyModifiers::NONE, KeyCode::Enter)),
            Event::Key(key(KeyModifiers::NONE, KeyCode::Tab)),
            Event::Key(key(KeyModifiers::SHIFT, KeyCode::Char('X'))),
        ];
        assert_eq!(text_from_key_burst(&burst).as_deref(), Some("sel\n\tX"));
    }

    #[test]
    fn paste_burst_while_results_focused_lands_in_editor() {
        // Regression: in git-bash/mintty a paste is a key burst, and while
        // the results table had focus every character was dropped. The burst
        // must land in the editor, with tabs/newlines as text.
        let mut app = test_app();
        app.show_startup_help = false;
        app.active_window = ActiveWindow::Results;
        let burst = vec![
            Event::Key(key(KeyModifiers::NONE, KeyCode::Char('s'))),
            Event::Key(key(KeyModifiers::NONE, KeyCode::Char('e'))),
            Event::Key(key(KeyModifiers::NONE, KeyCode::Char('l'))),
            Event::Key(key(KeyModifiers::NONE, KeyCode::Enter)),
            Event::Key(key(KeyModifiers::NONE, KeyCode::Tab)),
            Event::Key(key(KeyModifiers::NONE, KeyCode::Char('x'))),
        ];
        app.handle_events(burst);
        assert_eq!(app.active_window, ActiveWindow::Editor);
        assert_eq!(
            app.session_manager.active_session().editor.text(),
            "sel\n    x"
        );
    }

    #[test]
    fn single_key_while_results_focused_edits_sql() {
        // A one-character paste (or plain typing) arrives as a lone key
        // event; it must focus the editor and insert instead of vanishing.
        let mut app = test_app();
        app.show_startup_help = false;
        app.active_window = ActiveWindow::Results;
        app.handle_event(Event::Key(key(KeyModifiers::NONE, KeyCode::Char('x'))));
        assert_eq!(app.active_window, ActiveWindow::Editor);
        assert_eq!(app.session_manager.active_session().editor.text(), "x");
    }

    #[test]
    fn bare_enter_while_results_focused_does_not_edit_sql() {
        // Browsing Results and pressing Enter should stay in Results and must
        // not silently insert a newline into the SQL editor.
        let mut app = test_app();
        app.show_startup_help = false;
        app.active_window = ActiveWindow::Results;
        app.handle_event(Event::Key(key(KeyModifiers::NONE, KeyCode::Enter)));
        assert_eq!(app.active_window, ActiveWindow::Results);
        assert_eq!(app.session_manager.active_session().editor.text(), "");
    }

    #[test]
    fn command_confirm_prompt_is_not_treated_as_paste_target() {
        let mut app = test_app();
        app.show_startup_help = false;
        app.command_mode = true;
        assert!(app.paste_target_is_text());
        app.command_confirm = Some("quit".into());
        assert!(!app.paste_target_is_text());
    }

    #[test]
    fn key_burst_rejects_shortcuts_and_non_text() {
        // A real shortcut in the batch means it isn't a paste: handle the
        // events one by one instead of swallowing them as text.
        let shortcut = vec![
            Event::Key(key(KeyModifiers::NONE, KeyCode::Char('a'))),
            Event::Key(key(KeyModifiers::CONTROL, KeyCode::Char('c'))),
        ];
        assert!(text_from_key_burst(&shortcut).is_none());

        let cursor = vec![
            Event::Key(key(KeyModifiers::NONE, KeyCode::Char('a'))),
            Event::Key(key(KeyModifiers::NONE, KeyCode::Left)),
        ];
        assert!(text_from_key_burst(&cursor).is_none());

        // Key releases (Kitty protocol) are not text.
        let mut release = key(KeyModifiers::NONE, KeyCode::Char('a'));
        release.kind = KeyEventKind::Release;
        assert!(text_from_key_burst(&[Event::Key(release)]).is_none());

        // Mouse events can be queued in the same drain.
        let mixed = vec![
            Event::Key(key(KeyModifiers::NONE, KeyCode::Char('a'))),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
        ];
        assert!(text_from_key_burst(&mixed).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn clipboard_helper_receives_text_and_reports_exit() {
        // Exercises the external-helper path without touching a real clipboard.
        assert!(run_clipboard_command(
            "sh",
            &["-c", "cat >/dev/null"],
            "hello",
            false,
        ));
        assert!(!run_clipboard_command(
            "sh",
            &["-c", "exit 3"],
            "hello",
            false,
        ));
        assert!(!run_clipboard_command(
            "frog-no-such-clipboard-helper",
            &[],
            "hello",
            false,
        ));
        // A hung built-in helper must time out and return false (so the next
        // fallback can run) rather than hanging or falsely claiming success.
        assert!(!run_clipboard_command(
            "sh",
            &["-c", "sleep 10"],
            "hello",
            false,
        ));
    }

    #[test]
    fn copy_status_names_the_backend_and_failure() {
        let os = CopyOutcome {
            os: true,
            command: None,
            osc52: false,
        };
        assert_eq!(
            App::copy_status_for(os.clone(), 3, "table", false),
            "Copied table (3 chars) to clipboard"
        );

        let helper = CopyOutcome {
            os: false,
            command: Some("clip.exe".into()),
            osc52: false,
        };
        assert_eq!(
            App::copy_status_for(helper, 3, "table", false),
            "Copied table (3 chars) to clipboard via clip.exe"
        );

        let osc = CopyOutcome {
            os: false,
            command: None,
            osc52: true,
        };
        let msg = App::copy_status_for(osc, 3, "table", false);
        assert!(msg.starts_with("Copied table (3 chars) via OSC52"), "{msg}");
        assert!(msg.contains("Shift"), "{msg}");

        // Over SSH, OSC52 is reported even when a remote OS clipboard also succeeded.
        let both = CopyOutcome {
            os: true,
            command: None,
            osc52: true,
        };
        let ssh_msg = App::copy_status_for(both.clone(), 3, "table", true);
        assert!(
            ssh_msg.starts_with("Copied table (3 chars) via OSC52"),
            "{ssh_msg}"
        );
        assert_eq!(
            App::copy_status_for(both, 3, "table", false),
            "Copied table (3 chars) to clipboard"
        );

        let failed = CopyOutcome::default();
        assert!(App::copy_status_for(failed, 3, "table", false).starts_with("Copy failed"));
    }

    #[test]
    fn contextual_copy_dispatch_follows_focus() {
        use CopyTarget::*;
        // Editor: selection wins, else the statement at the cursor.
        assert_eq!(
            copy_target_for(ActiveWindow::Editor, true, false),
            SqlSelection
        );
        assert_eq!(
            copy_target_for(ActiveWindow::Editor, false, true),
            SqlStatement
        );
        // Results/Sidebar: table selection wins, else the full table.
        assert_eq!(
            copy_target_for(ActiveWindow::Results, false, true),
            TableSelection
        );
        assert_eq!(
            copy_target_for(ActiveWindow::Results, true, false),
            Results
        );
        assert_eq!(
            copy_target_for(ActiveWindow::Sidebar, false, false),
            Results
        );
    }

    #[test]
    fn wide_monitor_gives_table_more_width() {
        // Same 25% config: a big monitor caps the sidebar so the table keeps
        // the extra columns instead of the info panel.
        assert_eq!(effective_sidebar_pct(90, 25), 25);
        assert_eq!(effective_sidebar_pct(150, 25), 20);
        assert_eq!(effective_sidebar_pct(190, 25), 15);
        assert_eq!(effective_sidebar_pct(230, 25), 12);
    }

    #[test]
    fn short_screen_shrinks_editor_for_table_rows() {
        // Tall screen: editor untouched, extra rows go to the table.
        assert_eq!(effective_editor_height(50, 12), 12);
        // Short screen: editor shrinks so results keep ~8 rows.
        assert_eq!(effective_editor_height(16, 12), 8);
        assert!(effective_editor_height(12, 12) >= 4);
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
    fn completion_key_accepts_terminal_variants() {
        assert!(App::is_completion_key(key(
            KeyModifiers::CONTROL,
            KeyCode::Char(' ')
        )));
        assert!(App::is_completion_key(key(
            KeyModifiers::CONTROL,
            KeyCode::Char('.')
        )));
        assert!(App::is_completion_key(key(
            KeyModifiers::NONE,
            KeyCode::Null
        )));
        assert!(!App::is_completion_key(key(
            KeyModifiers::NONE,
            KeyCode::Char(' ')
        )));
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
            clipboard_command: None,
            saved_connections: Vec::new(),
        };
        App::new(config, SessionManager::new()).expect("app")
    }

    fn test_plan(rows: usize) -> crate::db::connection::QueryRow {
        crate::db::connection::QueryRow {
            columns: vec!["PLAN".into()],
            rows: (0..rows).map(|i| vec![format!("row {}", i)]).collect(),
            truncated: false,
            elapsed_ms: 0,
            is_error: false,
            error_msg: None,
            rows_affected: None,
            page_offset: 0,
            page_size: 100,
            byte_count: 0,
            total_fetched: rows,
        }
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
        app.session_view_focus = SessionViewPane::List;
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
    fn session_view_alt_up_down_resizes_focused_sql() {
        let mut app = test_app();
        app.session_view_focus = SessionViewPane::Sql;
        assert_eq!(app.session_sql_height, 7);
        app.handle_session_view(key(KeyModifiers::ALT, KeyCode::Up));
        assert_eq!(app.session_sql_height, 8);
        // List share is untouched when SQL is focused.
        assert_eq!(app.session_list_pct, 45);
        app.handle_session_view(key(KeyModifiers::ALT, KeyCode::Down));
        assert_eq!(app.session_sql_height, 7);
        app.session_sql_height = 30;
        app.handle_session_view(key(KeyModifiers::ALT, KeyCode::Up));
        assert_eq!(app.session_sql_height, 30);
        app.session_sql_height = 3;
        app.handle_session_view(key(KeyModifiers::ALT, KeyCode::Down));
        assert_eq!(app.session_sql_height, 3);
    }

    #[test]
    fn session_view_alt_up_down_grows_and_shrinks_plan() {
        let mut app = test_app();
        app.session_view_focus = SessionViewPane::Plan;
        // Grow plan: takes a row from SQL first.
        app.handle_session_view(key(KeyModifiers::ALT, KeyCode::Up));
        assert_eq!(app.session_sql_height, 6);
        assert_eq!(app.session_list_pct, 45);
        // Shrink plan: hands the row back to SQL.
        app.handle_session_view(key(KeyModifiers::ALT, KeyCode::Down));
        assert_eq!(app.session_sql_height, 7);
        // Once SQL is at minimum, growing the plan shrinks the list.
        app.session_sql_height = 3;
        app.handle_session_view(key(KeyModifiers::ALT, KeyCode::Up));
        assert_eq!(app.session_sql_height, 3);
        assert_eq!(app.session_list_pct, 40);
        // Once SQL is at maximum, shrinking the plan grows the list.
        app.session_sql_height = 30;
        app.session_list_pct = 45;
        app.handle_session_view(key(KeyModifiers::ALT, KeyCode::Down));
        assert_eq!(app.session_sql_height, 30);
        assert_eq!(app.session_list_pct, 50);
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
        assert_eq!(app.session_view_maximized, Some(SessionViewPane::Sql));
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

    #[test]
    fn wrap_sql_lines_prefers_spaces_and_fits_width() {
        assert_eq!(
            wrap_sql_lines("aaaa bbbb cccc", 9),
            vec!["aaaa bbbb".to_string(), "cccc".to_string()]
        );
        assert_eq!(wrap_sql_lines("", 8), vec![String::new()]);
        let long = "x".repeat(25);
        let lines = wrap_sql_lines(&long, 10);
        assert_eq!(lines, vec!["x".repeat(10), "x".repeat(10), "x".repeat(5)]);
    }

    #[test]
    fn session_view_scrolls_focused_sql_and_plan() {
        let mut app = test_app();
        app.session_sql_area = Rect::new(0, 0, 20, 6); // inner: 18 cols, 4 rows
        app.session_plan_area = Rect::new(0, 0, 40, 6); // inner: 4 rows

        let sql = format!(
            "SELECT {} FROM dual",
            (0..40)
                .map(|i| format!("column_{}", i))
                .collect::<Vec<_>>()
                .join(", ")
        );
        app.session_manager.active_session_mut().plan_sql_text = Some(sql);
        app.session_view_focus = SessionViewPane::Sql;
        let sql_max = app.session_sql_max_scroll();
        assert!(sql_max > 0);

        app.handle_session_view(key(KeyModifiers::NONE, KeyCode::Down));
        assert_eq!(app.session_manager.active_session().sql_scroll, 1);
        app.handle_session_view(key(KeyModifiers::NONE, KeyCode::PageDown));
        assert_eq!(app.session_manager.active_session().sql_scroll, 11);
        app.handle_session_view(key(KeyModifiers::NONE, KeyCode::End));
        assert_eq!(app.session_manager.active_session().sql_scroll, sql_max);
        app.handle_session_view(key(KeyModifiers::NONE, KeyCode::PageUp));
        assert_eq!(
            app.session_manager.active_session().sql_scroll,
            sql_max.saturating_sub(10)
        );
        app.handle_session_view(key(KeyModifiers::NONE, KeyCode::Home));
        assert_eq!(app.session_manager.active_session().sql_scroll, 0);

        app.session_manager.active_session_mut().plan_result = Some(test_plan(20));
        app.session_view_focus = SessionViewPane::Plan;
        assert_eq!(app.session_plan_max_scroll(), 17);
        app.handle_session_view(key(KeyModifiers::NONE, KeyCode::Down));
        assert_eq!(app.session_manager.active_session().plan_scroll, 1);
        app.handle_session_view(key(KeyModifiers::NONE, KeyCode::PageDown));
        assert_eq!(app.session_manager.active_session().plan_scroll, 11);
        app.handle_session_view(key(KeyModifiers::NONE, KeyCode::End));
        assert_eq!(app.session_manager.active_session().plan_scroll, 17);
        app.handle_session_view(key(KeyModifiers::NONE, KeyCode::Right));
        assert_eq!(app.session_manager.active_session().plan_hscroll, 8);
        app.handle_session_view(key(KeyModifiers::NONE, KeyCode::Left));
        assert_eq!(app.session_manager.active_session().plan_hscroll, 0);
        app.handle_session_view(key(KeyModifiers::NONE, KeyCode::Home));
        assert_eq!(app.session_manager.active_session().plan_scroll, 0);

        // List focus keeps the SQL/plan scroll offsets untouched.
        app.session_view_focus = SessionViewPane::List;
        app.handle_session_view(key(KeyModifiers::NONE, KeyCode::PageDown));
        assert_eq!(app.session_manager.active_session().plan_scroll, 0);
        assert_eq!(app.session_manager.active_session().sql_scroll, 0);
    }

    #[test]
    fn explorer_tab_cycles_focus_and_ctrl_z_zooms() {
        let mut app = test_app();
        assert_eq!(app.explorer_focus, ExplorerPane::Tree);
        app.handle_db_explorer(key(KeyModifiers::NONE, KeyCode::Tab));
        assert_eq!(app.explorer_focus, ExplorerPane::Detail);
        app.handle_db_explorer(key(KeyModifiers::NONE, KeyCode::Tab));
        assert_eq!(app.explorer_focus, ExplorerPane::Tree);
        app.handle_db_explorer(key(KeyModifiers::SHIFT, KeyCode::BackTab));
        assert_eq!(app.explorer_focus, ExplorerPane::Detail);

        assert!(!app.explorer_maximized);
        app.handle_db_explorer(key(KeyModifiers::CONTROL, KeyCode::Char('z')));
        assert!(app.explorer_maximized);
        app.handle_db_explorer(key(KeyModifiers::NONE, KeyCode::F(11)));
        assert!(!app.explorer_maximized);
    }

    #[test]
    fn explorer_detail_focus_scrolls_preview_rows_and_columns() {
        let mut app = test_app();
        app.explorer_focus = ExplorerPane::Detail;
        app.handle_db_explorer(key(KeyModifiers::NONE, KeyCode::Down));
        app.handle_db_explorer(key(KeyModifiers::NONE, KeyCode::PageDown));
        assert_eq!(
            app.session_manager.active_session().explorer_detail_scroll,
            11
        );
        app.handle_db_explorer(key(KeyModifiers::NONE, KeyCode::Right));
        app.handle_db_explorer(key(KeyModifiers::NONE, KeyCode::Right));
        assert_eq!(
            app.session_manager
                .active_session()
                .explorer_detail_col_scroll,
            2
        );
        app.handle_db_explorer(key(KeyModifiers::NONE, KeyCode::Home));
        let sess = app.session_manager.active_session();
        assert_eq!(sess.explorer_detail_scroll, 0);
        assert_eq!(sess.explorer_detail_col_scroll, 0);

        // Tree focus: arrows navigate the tree, never the detail scroll.
        app.explorer_focus = ExplorerPane::Tree;
        app.handle_db_explorer(key(KeyModifiers::NONE, KeyCode::Down));
        assert_eq!(
            app.session_manager.active_session().explorer_detail_scroll,
            0
        );
    }

    #[test]
    fn explorer_alt_up_down_resizes_split() {
        let mut app = test_app();
        assert_eq!(app.explorer_split_pct, 55);
        app.handle_db_explorer(key(KeyModifiers::ALT, KeyCode::Up));
        assert_eq!(app.explorer_split_pct, 60);
        app.handle_db_explorer(key(KeyModifiers::ALT, KeyCode::Down));
        assert_eq!(app.explorer_split_pct, 55);
        app.explorer_split_pct = 85;
        app.handle_db_explorer(key(KeyModifiers::ALT, KeyCode::Up));
        assert_eq!(app.explorer_split_pct, 85);
        app.explorer_split_pct = 25;
        app.handle_db_explorer(key(KeyModifiers::ALT, KeyCode::Down));
        assert_eq!(app.explorer_split_pct, 25);
    }

    #[test]
    fn f4_object_ddl_viewer_loads_into_editor_and_new_tab() {
        use crate::db::ddl::DescribedObject;

        let mut app = test_app();
        app.show_startup_help = false;
        {
            let sess = app.session_manager.active_session_mut();
            sess.mode = SessionMode::ObjectDdl;
            sess.ddl_return_mode = SessionMode::DbExplorer;
            sess.ddl_object = Some(DescribedObject {
                schema: "SCOTT".into(),
                name: "IDX_EMP_DEPT".into(),
                kind: "INDEX".into(),
                ddl: "CREATE INDEX SCOTT.IDX_EMP_DEPT ON SCOTT.EMP (DEPTNO);".into(),
                summary: None,
                elapsed_ms: 4,
            });
        }

        // Pressing `n` opens the DDL in a brand-new session tab and restores origin's mode.
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Char('n')));
        assert_eq!(app.session_manager.sessions.len(), 2);
        assert_eq!(app.session_manager.active_idx, 1);
        assert_eq!(
            app.session_manager.sessions[0].mode,
            SessionMode::DbExplorer
        );
        assert_eq!(
            app.session_manager.active_session().editor.text(),
            "CREATE INDEX SCOTT.IDX_EMP_DEPT ON SCOTT.EMP (DEPTNO);"
        );

        // Switch back to tab 0, reopen DDL, and press `i` to insert into current editor.
        app.session_manager.switch_to(0);
        app.session_manager.active_session_mut().mode = SessionMode::ObjectDdl;
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Char('i')));
        assert_eq!(app.session_manager.active_session().mode, SessionMode::Query);
        assert_eq!(
            app.session_manager.active_session().editor.text(),
            "CREATE INDEX SCOTT.IDX_EMP_DEPT ON SCOTT.EMP (DEPTNO);"
        );
    }

    #[test]
    fn f6_performance_dashboard_navigates_toggles_and_sends_sql_to_editor() {
        let mut app = test_app();
        app.show_startup_help = false;

        // F6 enters Performance mode.
        app.handle_key(key(KeyModifiers::NONE, KeyCode::F(6)));
        assert_eq!(
            app.session_manager.active_session().mode,
            SessionMode::Performance
        );
        assert_eq!(app.perf_focus, PerfPane::Queries);

        // Number shortcut `2` selects query #2 (index 1: Top CPU Sessions).
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Char('2')));
        assert_eq!(app.session_manager.active_session().perf_cursor, 1);

        // `v` toggles the SQL preview pane; Tab cycles Queries -> Results -> Sql -> Queries.
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Char('v')));
        assert!(app.session_manager.active_session().perf_show_sql);
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Tab));
        assert_eq!(app.perf_focus, PerfPane::Results);
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Tab));
        assert_eq!(app.perf_focus, PerfPane::Sql);
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Tab));
        assert_eq!(app.perf_focus, PerfPane::Queries);

        // `a` toggles live 5s auto-refresh.
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Char('a')));
        assert!(app.session_manager.active_session().perf_auto_refresh);

        // `s` sends the diagnostic SQL to the SQL editor and returns to Query mode.
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Char('s')));
        assert_eq!(app.session_manager.active_session().mode, SessionMode::Query);
        assert!(app
            .session_manager
            .active_session()
            .editor
            .text()
            .contains("CPU"));
    }

    #[test]
    fn isearch_finds_in_editor_and_steps() {
        let mut app = test_app();
        app.show_startup_help = false;
        app.session_manager.active_session_mut().editor.set_text("select a\nfrom t\nselect b");
        app.session_manager.active_session_mut().editor.cursor_row = 0;
        app.session_manager.active_session_mut().editor.cursor_col = 0;
        // Ctrl-S starts search in the editor.
        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::Char('s')));
        assert!(app.search.is_some());
        // Type "select": jumps to first match at/after origin (row 0).
        for c in "select".chars() {
            app.handle_key(key(KeyModifiers::NONE, KeyCode::Char(c)));
        }
        let st = app.search.as_ref().expect("search");
        assert_eq!(st.query, "select");
        assert_eq!(st.matches.len(), 2);
        assert_eq!(app.session_manager.active_session().editor.cursor_row, 0);
        // Ctrl-S steps to the second match (row 2).
        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::Char('s')));
        assert_eq!(app.session_manager.active_session().editor.cursor_row, 2);
        // Ctrl-R steps back to the first match.
        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::Char('r')));
        assert_eq!(app.session_manager.active_session().editor.cursor_row, 0);
        // Enter keeps the match and exits.
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Enter));
        assert!(app.search.is_none());
    }

    #[test]
    fn isearch_snippet_shows_current_match_context() {
        let mut app = test_app();
        app.show_startup_help = false;
        app.session_manager
            .active_session_mut()
            .editor
            .set_text("select a\nfrom t");
        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::Char('s')));
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Char('s')));
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Char('e')));
        let snip = app.search_current_snippet().expect("snippet");
        assert!(snip.contains("select"), "{snip}");
    }

    #[test]
    fn isearch_ctrl_g_aborts_to_origin() {
        let mut app = test_app();
        app.show_startup_help = false;
        app.session_manager.active_session_mut().editor.set_text("select a\nfrom t");
        app.session_manager.active_session_mut().editor.cursor_row = 1;
        app.session_manager.active_session_mut().editor.cursor_col = 0;
        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::Char('s')));
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Char('s')));
        assert_eq!(app.session_manager.active_session().editor.cursor_row, 0);
        // Ctrl-G restores the pre-search cursor.
        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::Char('g')));
        assert!(app.search.is_none());
        assert_eq!(app.session_manager.active_session().editor.cursor_row, 1);
    }

    #[test]
    fn command_bar_keeps_bar_open_on_unknown_command_error() {
        let mut app = test_app();
        app.show_startup_help = false;
        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::Char(':')));
        assert!(app.command_mode);
        for c in "bogus".chars() {
            app.handle_key(key(KeyModifiers::NONE, KeyCode::Char(c)));
        }
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Enter));
        // Command bar stays open so `command_error` is rendered inline.
        assert!(app.command_mode);
        assert_eq!(app.command_input, "bogus");
        assert!(app
            .command_error
            .as_deref()
            .unwrap_or("")
            .contains("Unknown command"));

        // Editing clears the stale error, and Esc closes cleanly.
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Backspace));
        assert!(app.command_error.is_none());
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Esc));
        assert!(!app.command_mode);
    }

    #[test]
    fn conn_dialog_esc_restores_previous_settings_and_home_end_work() {
        let mut app = test_app();
        app.show_startup_help = false;
        {
            let sess = app.session_manager.active_session_mut();
            sess.conn_dialog.db_type = crate::db::DbType::Oracle;
            sess.conn_dialog.host = "ora.prod".into();
            sess.conn_dialog.port = "1521".into();
        }
        // Open Ctrl+O, toggle Type to Postgres and edit Host, then press Esc.
        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::Char('o')));
        assert_eq!(
            app.session_manager.active_session().mode,
            SessionMode::ConnectionPickerDialog
        );
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Char(' '))); // toggle -> Postgres
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Tab)); // Host field
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Home));
        assert_eq!(app.session_manager.active_session().conn_dialog.cursor, 0);
        app.handle_key(key(KeyModifiers::NONE, KeyCode::End));
        assert_eq!(app.session_manager.active_session().conn_dialog.cursor, 8);
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Char('X')));
        assert_eq!(
            app.session_manager.active_session().conn_dialog.db_type,
            crate::db::DbType::Postgres
        );
        // Esc cancels all edits.
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Esc));
        let dlg = &app.session_manager.active_session().conn_dialog;
        assert_eq!(dlg.db_type, crate::db::DbType::Oracle);
        assert_eq!(dlg.host, "ora.prod");
        assert_eq!(dlg.port, "1521");
    }

    #[test]
    fn conn_dialog_alt_n_cycles_saved_connections() {
        use crate::cli::ConnectionEntry;

        let mut app = test_app();
        app.show_startup_help = false;
        app.config.saved_connections = vec![
            ConnectionEntry {
                name: "prod".into(),
                host: "db1".into(),
                port: 1521,
                service: "ORCL".into(),
                user: "scott".into(),
                password: None,
                db_type: Some(crate::db::DbType::Oracle),
                database: None,
            },
            ConnectionEntry {
                name: "pg-dev".into(),
                host: "db2".into(),
                port: 5432,
                service: String::new(),
                user: "bob".into(),
                password: None,
                db_type: Some(crate::db::DbType::Postgres),
                database: Some("myapp".into()),
            },
        ];
        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::Char('o')));
        app.handle_key(key(KeyModifiers::ALT, KeyCode::Char('n')));
        assert_eq!(
            app.session_manager.active_session().conn_dialog.host,
            "db1"
        );
        app.handle_key(key(KeyModifiers::ALT, KeyCode::Char('n')));
        let dlg = &app.session_manager.active_session().conn_dialog;
        assert_eq!(dlg.host, "db2");
        assert_eq!(dlg.db_type, crate::db::DbType::Postgres);
        app.handle_key(key(KeyModifiers::ALT, KeyCode::Char('p')));
        assert_eq!(
            app.session_manager.active_session().conn_dialog.host,
            "db1"
        );
    }

    #[test]
    fn ctrl_w_requires_confirm_when_editor_has_sql() {
        let mut app = test_app();
        app.show_startup_help = false;
        app.session_manager.add_session();
        assert_eq!(app.session_manager.sessions.len(), 2);
        app.session_manager
            .active_session_mut()
            .editor
            .set_text("select 1");
        // First Ctrl+W arms, second closes.
        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::Char('w')));
        assert_eq!(app.session_manager.sessions.len(), 2);
        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::Char('w')));
        assert_eq!(app.session_manager.sessions.len(), 1);
    }

    #[test]
    fn editor_ctrl_arrows_jump_words_not_tabs() {
        let mut app = test_app();
        app.show_startup_help = false;
        app.session_manager
            .active_session_mut()
            .editor
            .set_text("select empno from emp");
        {
            let s = app.session_manager.active_session_mut();
            s.editor.cursor_row = 0;
            s.editor.cursor_col = s.editor.lines[0].len();
        }
        let tabs_before = app.session_manager.active_idx;
        app.handle_key(key(KeyModifiers::CONTROL, KeyCode::Left));
        assert_eq!(app.session_manager.active_idx, tabs_before);
        assert_eq!(
            &app.session_manager.active_session().editor.lines[0]
                [app.session_manager.active_session().editor.cursor_col..],
            "emp"
        );
    }

    #[test]
    fn help_and_history_views_scroll_full_content() {
        use ratatui::backend::TestBackend;

        let mut app = test_app();
        app.show_startup_help = false;
        app.help_area = Rect::new(0, 0, 80, 20);
        app.handle_key(key(KeyModifiers::NONE, KeyCode::F(1)));
        assert_eq!(app.session_manager.active_session().mode, SessionMode::Help);
        app.handle_key(key(KeyModifiers::NONE, KeyCode::End));
        let max_help = app.help_max_scroll();
        assert!(max_help > 20);
        assert_eq!(app.session_manager.active_session().help_scroll, max_help);
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Home));
        assert_eq!(app.session_manager.active_session().help_scroll, 0);
        app.handle_key(key(KeyModifiers::NONE, KeyCode::Esc));

        // Populate 60 history entries and verify F3 renders the newest entry at bottom.
        {
            let sess = app.session_manager.active_session_mut();
            sess.query_history = (1..=60).map(|i| format!("SELECT {}", i)).collect();
            sess.history_cursor = 59;
            sess.history_scroll = 0;
        }
        app.handle_key(key(KeyModifiers::NONE, KeyCode::F(3)));
        let mut terminal = Terminal::new(TestBackend::new(80, 15)).unwrap();
        terminal.draw(|f| app.ui(f)).unwrap();
        let buf = terminal.backend().buffer();
        let mut rendered = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                rendered.push_str(buf[(x, y)].symbol());
            }
        }
        assert!(rendered.contains("SELECT 60"), "rendered:\n{rendered}");
    }
}
