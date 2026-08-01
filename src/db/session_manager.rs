use crate::db::connection::{DbSessionInfo, OracleConnection, QueryRow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

/// History file stored next to the frog binary.
pub fn history_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("frog_history.txt")))
        .unwrap_or_else(|| PathBuf::from("frog_history.txt"))
}

pub fn load_history() -> Vec<String> {
    let path = history_path();
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut buf = String::new();
    for line in contents.lines() {
        if line.starts_with("-- frog@") {
            let stmt = buf.trim().trim_end_matches(';').to_string();
            if !stmt.is_empty() {
                out.push(stmt);
            }
            buf.clear();
        } else {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    let stmt = buf.trim().trim_end_matches(';').to_string();
    if !stmt.is_empty() {
        out.push(stmt);
    }
    out
}

pub fn append_history(sql: &str) {
    let path = history_path();
    let entry = format!(
        "-- frog@{};\n{};\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        sql.trim().trim_end_matches(';')
    );
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::io::Write;
        let _ = file.write_all(entry.as_bytes());
    }
}

/// Parse a statement for a sqlplus-style `@file.sql` (or `@ file.sql`) directive.
/// Returns the raw path portion (before `;`, whitespace trimmed). `@@` keeps the
/// `@@` prefix so callers know it resolves relative to the including file.
pub fn parse_file_directive(stmt: &str) -> Option<String> {
    let t = stmt.trim();
    if !t.starts_with('@') {
        return None;
    }
    let inner = t.trim_end_matches(';').trim();
    let inner = inner.trim_start_matches('@');
    let inner = inner.trim_start();
    if inner.is_empty() {
        return None;
    }
    // Distinguish `@@name` from `@name`.
    let double = t.starts_with("@@");
    let name = inner.trim().trim_end_matches(';').trim();
    if name.is_empty() {
        return None;
    }
    Some(if double {
        format!("@@{}", name)
    } else {
        name.to_string()
    })
}

/// Load a .sql file and return its split statements.
/// Returns an error string on read failure.
pub fn load_script_file(path: &Path) -> Result<Vec<String>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read file '{}': {}", path.display(), e))?;
    Ok(SqlEditor::statements_with_ranges(&content)
        .into_iter()
        .map(|(_, _, s)| s)
        .collect())
}

/// Recursively expand `@file` and `@@file` directives into their statements.
/// `@@` is resolved relative to the directory of the including file.
/// Returns an error string if any nested file cannot be read.
fn expand_statements(statements: Vec<String>, cwd: &Path) -> Result<Vec<String>, String> {
    fn expand_into(
        stmts: Vec<String>,
        base_dir: &Path,
        cwd: &Path,
        out: &mut Vec<String>,
        depth: usize,
    ) -> Result<(), String> {
        if depth > 32 {
            return Err(format!(
                "Max include depth exceeded while including files in '{}'",
                base_dir.display()
            ));
        }
        for stmt in stmts {
            match parse_file_directive(&stmt) {
                Some(raw) => {
                    let is_sibling = raw.starts_with("@@");
                    let raw_path = raw.trim_start_matches("@@").trim();
                    let path = PathBuf::from(raw_path);
                    let base = if is_sibling { base_dir } else { cwd };
                    let resolved = if path.is_absolute() {
                        path
                    } else {
                        base.join(path)
                    };
                    let nested = load_script_file(&resolved)?;
                    let parent = resolved
                        .parent()
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|| base_dir.to_path_buf());
                    let base_for_nested = parent.as_path();
                    expand_into(nested, base_for_nested, cwd, out, depth + 1)?;
                }
                None => out.push(stmt),
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    expand_into(statements, cwd, cwd, &mut out, 0)?;
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct SqlEditor {
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub scroll_offset: usize,
}

impl SqlEditor {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            scroll_offset: 0,
        }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn set_text(&mut self, text: &str) {
        self.lines = text.split('\n').map(|s| s.to_string()).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.scroll_offset = 0;
    }

    pub fn insert_char(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        let line = &mut self.lines[self.cursor_row];
        line.insert(self.cursor_col, c);
        self.cursor_col += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor_col > 0 {
            self.lines[self.cursor_row].remove(self.cursor_col - 1);
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            let cur = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].len();
            self.lines[self.cursor_row].push_str(&cur);
        }
        if self.lines.is_empty() {
            self.lines.push(String::new());
            self.cursor_row = 0;
            self.cursor_col = 0;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor_col < self.lines[self.cursor_row].len() {
            self.lines[self.cursor_row].remove(self.cursor_col);
        } else if self.cursor_row + 1 < self.lines.len() {
            let next_line = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next_line);
        }
        if self.lines.is_empty() {
            self.lines.push(String::new());
            self.cursor_row = 0;
            self.cursor_col = 0;
        }
    }

    pub fn kill_line(&mut self) {
        if self.cursor_col < self.lines[self.cursor_row].len() {
            self.lines[self.cursor_row].truncate(self.cursor_col);
        } else if self.cursor_row + 1 < self.lines.len() {
            let next_line = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next_line);
        } else if self.lines.len() > 1 {
            self.lines.remove(self.cursor_row);
            if self.cursor_row >= self.lines.len() {
                self.cursor_row = self.lines.len().saturating_sub(1);
            }
            self.cursor_col = self.lines[self.cursor_row].len();
        }
        if self.lines.is_empty() {
            self.lines.push(String::new());
            self.cursor_row = 0;
            self.cursor_col = 0;
        }
    }

    pub fn newline(&mut self) {
        let rest = self.lines[self.cursor_row].split_off(self.cursor_col);
        self.lines.insert(self.cursor_row + 1, rest);
        self.cursor_row += 1;
        self.cursor_col = 0;
    }

    pub fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].len();
        }
    }

    pub fn move_right(&mut self) {
        let len = self.lines[self.cursor_row].len();
        if self.cursor_col < len {
            self.cursor_col += 1;
        } else if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
    }

    pub fn move_up(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            let len = self.lines[self.cursor_row].len();
            if self.cursor_col > len {
                self.cursor_col = len;
            }
        }
    }

    pub fn move_down(&mut self) {
        if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            let len = self.lines[self.cursor_row].len();
            if self.cursor_col > len {
                self.cursor_col = len;
            }
        }
    }

    pub fn ensure_cursor_visible(&mut self, visible_height: usize) {
        if visible_height == 0 {
            return;
        }
        if self.cursor_row < self.scroll_offset {
            self.scroll_offset = self.cursor_row;
        } else if self.cursor_row >= self.scroll_offset + visible_height {
            self.scroll_offset = self.cursor_row + 1 - visible_height;
        }
    }

    fn cursor_char_offset(&self) -> usize {
        let mut off = 0;
        for (i, line) in self.lines.iter().enumerate() {
            if i == self.cursor_row {
                off += self.cursor_col.min(line.len());
                break;
            }
            off += line.len() + 1;
        }
        off
    }

    /// Split the full editor text into SQL statements with char ranges.
    /// Splits at ';' and at lines containing only '/'.
    pub fn statements_with_ranges(text: &str) -> Vec<(usize, usize, String)> {
        let mut out = Vec::new();
        let bytes = text.as_bytes();
        let mut start = 0usize;
        let mut i = 0usize;
        let mut line_start = 0usize;
        while i < bytes.len() {
            if bytes[i] == b'\n' {
                let line = text[line_start..i].trim();
                if line == "/" {
                    let stmt = text[start..line_start].trim().trim_end_matches(';').trim().to_string();
                    if !stmt.is_empty() {
                        out.push((start, i + 1, stmt));
                    }
                    start = i + 1;
                }
                line_start = i + 1;
            } else if bytes[i] == b';' {
                let stmt = text[start..i].trim().to_string();
                if !stmt.is_empty() {
                    out.push((start, i + 1, stmt));
                }
                start = i + 1;
                line_start = i + 1;
            }
            i += 1;
        }
        let tail = text[start..].trim().to_string();
        if !tail.is_empty() {
            out.push((start, text.len(), tail));
        }
        out
    }

    /// The statement the cursor currently points at.
    pub fn current_statement(&self) -> Option<String> {
        let text = self.text();
        let off = self.cursor_char_offset();
        let stmts = Self::statements_with_ranges(&text);
        if stmts.is_empty() {
            return None;
        }
        for (s, e, stmt) in &stmts {
            if off >= *s && off < *e {
                return Some(stmt.clone());
            }
        }
        // cursor in whitespace between statements: take next statement, else last
        for (s, _e, stmt) in &stmts {
            if off < *s {
                return Some(stmt.clone());
            }
        }
        stmts.last().map(|(_, _, t)| t.clone())
    }

    /// Row range of the statement the cursor is in (for highlighting).
    pub fn current_statement_rows(&self) -> Option<(usize, usize)> {
        let text = self.text();
        let off = self.cursor_char_offset();
        let stmts = Self::statements_with_ranges(&text);
        if stmts.is_empty() {
            return None;
        }
        let (cs, ce) = {
            let mut found: Option<(usize, usize)> = None;
            for (s, e, _) in &stmts {
                if off >= *s && off < *e {
                    found = Some((*s, *e));
                    break;
                }
            }
            if found.is_none() {
                for (s, e, _) in &stmts {
                    if off < *s {
                        found = Some((*s, *e));
                        break;
                    }
                }
            }
            found.or_else(|| stmts.last().map(|(s, e, _)| (*s, *e)))
        }?;

        let mut row_start = 0usize;
        let mut row_end = 0usize;
        let mut cum = 0usize;
        for (i, line) in self.lines.iter().enumerate() {
            let line_start = cum;
            let line_end = cum + line.len();
            if line_start <= cs && cs <= line_end {
                row_start = i;
            }
            if line_start < ce && ce <= line_end + 1 {
                row_end = i;
            }
            cum = line_end + 1;
        }
        Some((row_start, row_end.max(row_start)))
    }

    pub fn all_statements(&self) -> Vec<String> {
        Self::statements_with_ranges(&self.text())
            .into_iter()
            .map(|(_, _, s)| s)
            .collect()
    }
}

pub struct Session {
    pub id: usize,
    pub name: String,
    pub conn: Option<Arc<OracleConnection>>,
    pub is_connected: bool,
    pub connecting: bool,
    pub connect_error: Option<String>,
    pub query_history: Vec<String>,
    pub history_cursor: usize,
    pub results: Vec<QueryRow>,
    pub current_result_idx: usize,
    pub scroll_offset: usize,
    pub col_scroll_offset: usize,
    pub editor: SqlEditor,
    pub mode: SessionMode,
    pub pending_query: bool,
    pub conn_dialog: ConnectionDialog,
    /// The last SELECT SQL executed — used for fetching additional pages.
    pub last_sql: Option<String>,
    /// Page size for pagination (default 200).
    pub page_size: usize,
    /// Temporary status message for notifications (e.g., "No more rows").
    pub status_message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ConnectionDialog {
    pub host: String,
    pub port: String,
    pub service: String,
    pub user: String,
    pub password: String,
    pub active_field: usize,
}

impl Default for ConnectionDialog {
    fn default() -> Self {
        Self {
            host: String::from("localhost"),
            port: String::from("1521"),
            service: String::from("ORCL"),
            user: String::new(),
            password: String::new(),
            active_field: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionMode {
    Query,
    Results,
    SessionView,
    Help,
    History,
    ConnectionPickerDialog,
}

impl Session {
    fn new(id: usize) -> Self {
        Self {
            id,
            name: format!("Session {}", id),
            conn: None,
            is_connected: false,
            connecting: false,
            connect_error: None,
            query_history: load_history(),
            history_cursor: 0,
            results: Vec::new(),
            current_result_idx: 0,
            scroll_offset: 0,
            col_scroll_offset: 0,
            editor: SqlEditor::new(),
            mode: SessionMode::Query,
            pending_query: false,
            conn_dialog: ConnectionDialog::default(),
            last_sql: None,
            page_size: 100,
            status_message: None,
        }
    }
}

pub struct SessionManager {
    pub sessions: Vec<Session>,
    pub active_idx: usize,
    next_id: AtomicUsize,
    result_rx: Receiver<(usize, QueryRow)>,
    result_tx: Sender<(usize, QueryRow)>,
    conn_result_rx: Receiver<(usize, bool, String, String)>,
    conn_result_tx: Sender<(usize, bool, String, String)>,
    conn_map: Arc<Mutex<HashMap<usize, Arc<OracleConnection>>>>,
}

impl SessionManager {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let (ctx, crx) = mpsc::channel();
        let mut sessions = Vec::new();
        sessions.push(Session::new(1));
        Self {
            sessions,
            active_idx: 0,
            next_id: AtomicUsize::new(2),
            result_rx: rx,
            result_tx: tx,
            conn_result_rx: crx,
            conn_result_tx: ctx,
            conn_map: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn active_session(&self) -> &Session {
        &self.sessions[self.active_idx]
    }

    pub fn active_session_mut(&mut self) -> &mut Session {
        &mut self.sessions[self.active_idx]
    }

    pub fn add_session(&mut self) -> usize {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut session = Session::new(id);
        session.conn_dialog = self.sessions[self.active_idx].conn_dialog.clone();
        
        // If active session is connected, auto-connect the new session with same params
        let active = &self.sessions[self.active_idx];
        let should_connect = active.is_connected && !active.connecting;
        let cs = format!("//{}:{}/{}", active.conn_dialog.host, active.conn_dialog.port, active.conn_dialog.service);
        let user = active.conn_dialog.user.clone();
        let password = active.conn_dialog.password.clone();
        
        self.sessions.push(session);
        self.active_idx = self.sessions.len() - 1;
        let new_idx = self.active_idx;
        
        if should_connect && !user.is_empty() && !password.is_empty() {
            let id = self.sessions[new_idx].id;
            let cs_str = cs.clone();
            let user_str = user.clone();
            let password_str = password.clone();
            let name_str = format!("{}@{}", user_str, cs_str.trim_start_matches("//"));
            let conn_map = self.conn_map.clone();
            let conn_result_tx = self.conn_result_tx.clone();

            self.sessions[new_idx].connecting = true;
            self.sessions[new_idx].connect_error = None;

            thread::spawn(move || {
                match OracleConnection::connect(&cs_str, &user_str, &password_str) {
                    Ok(conn) => {
                        if let Ok(mut map) = conn_map.lock() {
                            map.insert(id, conn);
                        }
                        let _ = conn_result_tx.send((id, true, String::new(), name_str));
                    }
                    Err(e) => {
                        let _ = conn_result_tx.send((id, false, e.to_string(), name_str));
                    }
                }
            });
        }
        
        new_idx
    }

    pub fn remove_session(&mut self, idx: usize) {
        if self.sessions.len() <= 1 {
            return;
        }
        self.sessions.remove(idx);
        if self.active_idx >= self.sessions.len() {
            self.active_idx = self.sessions.len() - 1;
        }
    }

    pub fn switch_next(&mut self) {
        if self.sessions.len() > 1 {
            self.active_idx = (self.active_idx + 1) % self.sessions.len();
        }
    }

    pub fn switch_prev(&mut self) {
        if self.sessions.len() > 1 {
            self.active_idx = if self.active_idx == 0 {
                self.sessions.len() - 1
            } else {
                self.active_idx - 1
            };
        }
    }

    pub fn switch_to(&mut self, idx: usize) {
        if idx < self.sessions.len() {
            self.active_idx = idx;
        }
    }

    pub fn connect_active(&mut self, cs: &str, user: &str, password: &str) {
        let id = self.sessions[self.active_idx].id;
        let cs_str = cs.to_string();
        let user_str = user.to_string();
        let password = password.to_string();
        let name_str = format!("{}@{}", user_str, cs_str.trim_start_matches("//"));
        let conn_map = self.conn_map.clone();
        let conn_result_tx = self.conn_result_tx.clone();

        self.sessions[self.active_idx].connecting = true;
        self.sessions[self.active_idx].connect_error = None;

        thread::spawn(move || {
            match OracleConnection::connect(&cs_str, &user_str, &password) {
                Ok(conn) => {
                    if let Ok(mut map) = conn_map.lock() {
                        map.insert(id, conn);
                    }
                    let _ = conn_result_tx.send((id, true, String::new(), name_str));
                }
                Err(e) => {
                    let _ = conn_result_tx.send((id, false, e.to_string(), name_str));
                }
            }
        });
    }

    pub fn execute_query(&mut self, sql: &str) {
        // @file / @ file.sql directive: read the file and run it as a script,
        // honoring nested @file includes and aborting on the first error.
        if let Some(raw) = parse_file_directive(sql) {
            let path = PathBuf::from(raw.trim_start_matches("@@").trim());
            match load_script_file(&path) {
                Ok(statements) => {
                    self.sessions[self.active_idx].status_message = Some(format!(
                        "Running script '{}' ({} statements)",
                        path.display(),
                        statements.len()
                    ));
                    self.execute_script(statements);
                }
                Err(err) => {
                    self.sessions[self.active_idx].pending_query = false;
                    self.sessions[self.active_idx].status_message = Some(err.clone());
                    let id = self.sessions[self.active_idx].id;
                    let result_tx = self.result_tx.clone();
                    let page_size = self.sessions[self.active_idx].page_size;
                    let err_row = QueryRow {
                        columns: vec![],
                        rows: vec![],
                        truncated: false,
                        row_count: 0,
                        elapsed_ms: 0,
                        is_error: true,
                        error_msg: Some(err),
                        rows_affected: None,
                        page_offset: 0,
                        page_size,
                        byte_count: 0,
                        total_fetched: 0,
                    };
                    let _ = result_tx.send((id, err_row));
                }
            }
            return;
        }

        // Save SQL for pagination if it's a SELECT
        let trimmed = sql.trim().to_uppercase();
        if trimmed.starts_with("SELECT") || trimmed.starts_with("WITH") {
            self.sessions[self.active_idx].last_sql = Some(sql.trim().to_string());
        }
        self.execute_script(vec![sql.to_string()]);
    }

    pub fn execute_script(&mut self, statements: Vec<String>) {
        // Expand any nested @file directives found among the statements before running.
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let statements = match expand_statements(statements, &cwd) {
            Ok(stmts) => stmts,
            Err(err) => {
                self.sessions[self.active_idx].pending_query = false;
                self.sessions[self.active_idx].status_message = Some(err.clone());
                let id = self.sessions[self.active_idx].id;
                let result_tx = self.result_tx.clone();
                let page_size = self.sessions[self.active_idx].page_size;
                let err_row = QueryRow {
                    columns: vec![],
                    rows: vec![],
                    truncated: false,
                    row_count: 0,
                    elapsed_ms: 0,
                    is_error: true,
                    error_msg: Some(err),
                    rows_affected: None,
                    page_offset: 0,
                    page_size,
                    byte_count: 0,
                    total_fetched: 0,
                };
                let _ = result_tx.send((id, err_row));
                return;
            }
        };
        if statements.is_empty() {
            return;
        }
        let id = self.sessions[self.active_idx].id;
        let conn_map = self.conn_map.clone();
        let result_tx = self.result_tx.clone();
        let page_size = self.sessions[self.active_idx].page_size;

        self.sessions[self.active_idx].pending_query = true;

        // Persist every executed statement to disk history + session history
        // Also track last SELECT for pagination
        {
            let sess = &mut self.sessions[self.active_idx];
            for sql in &statements {
                let trimmed = sql.trim().to_string();
                if !trimmed.is_empty() {
                    append_history(&trimmed);
                    sess.query_history.push(trimmed.clone());
                    let up = trimmed.to_uppercase();
                    if up.starts_with("SELECT") || up.starts_with("WITH") {
                        sess.last_sql = Some(trimmed);
                    }
                }
            }
            sess.history_cursor = sess.query_history.len();
        }

        thread::spawn(move || {
            let conn_opt = conn_map.lock().ok().and_then(|m| m.get(&id).cloned());
            if let Some(conn) = conn_opt {
                let mut had_error = false;
                for (n, sql) in statements.iter().enumerate() {
                    if had_error {
                        break;
                    }
                    let result = OracleConnection::execute_query(&*conn, sql, page_size);
                    let was_cancelled = result.is_error
                        && result
                            .error_msg
                            .as_deref()
                            .map(Self::is_cancelled_msg)
                            .unwrap_or(false);

                    // Log each error clearly, prefixing with the statement index.
                    let is_error = result.is_error;
                    let result = if is_error && !was_cancelled {
                        let src = sql.replace('\n', " ").trim().to_string();
                        let msg = result
                            .error_msg
                            .as_deref()
                            .map(String::from)
                            .unwrap_or_else(|| "Unknown error".into());
                        QueryRow {
                            error_msg: Some(format!(
                                "Statement {} of {} failed\n  SQL: {}\n  {}",
                                n + 1,
                                statements.len(),
                                src,
                                msg
                            )),
                            ..result
                        }
                    } else {
                        result
                    };

                    let _ = result_tx.send((id, result));
                    // Abort the script on the first real error.
                    if is_error && !was_cancelled {
                        had_error = true;
                    }
                    if was_cancelled {
                        break;
                    }
                }
            } else {
                let err = QueryRow {
                    columns: vec![],
                    rows: vec![],
                    truncated: false,
                    row_count: 0,
                    elapsed_ms: 0,
                    is_error: true,
                    error_msg: Some("Not connected. Use Ctrl+O to connect.".into()),
                    rows_affected: None,
                    page_offset: 0,
                    page_size,
                    byte_count: 0,
                    total_fetched: 0,
                };
                let _ = result_tx.send((id, err));
            }
        });
    }

    /// Fetch the next page of results for the last SELECT query.
    /// New rows are appended to the last QueryRow in session.results.
    pub fn fetch_next_page(&mut self) {
        let sess = &self.sessions[self.active_idx];
        let sql = match &sess.last_sql {
            Some(s) => s.clone(),
            None => return,
        };
        // Only fetch if last result is truncated
        let (current_offset, page_size) = match sess.results.last() {
            Some(r) if r.truncated && !r.is_error => (r.page_offset + r.rows.len(), r.page_size),
            _ => {
                self.sessions[self.active_idx].status_message = Some("No more rows to fetch".into());
                return;
            }
        };

        let id = sess.id;
        let conn_map = self.conn_map.clone();
        let result_tx = self.result_tx.clone();

        self.sessions[self.active_idx].pending_query = true;

        thread::spawn(move || {
            let conn_opt = conn_map.lock().ok().and_then(|m| m.get(&id).cloned());
            if let Some(conn) = conn_opt {
                let result = OracleConnection::execute_query_paged(&*conn, &sql, current_offset, page_size);
                let _ = result_tx.send((id, result));
            }
        });
    }

    /// Whether an Oracle error message indicates a user-requested cancellation.
    pub fn is_cancelled_msg(msg: &str) -> bool {
        msg == "Query cancelled"
            || msg.contains("ORA-01013")
            || msg.contains("ORA-01014")
            || msg.to_lowercase().contains("cancel")
            || msg.to_lowercase().contains("interrupted")
    }

    pub fn cancel_query(&mut self) {
        let id = self.sessions[self.active_idx].id;

        if !self.sessions[self.active_idx].pending_query {
            return;
        }

        // Take the connection out of the map before interrupting it so a
        // concurrent reconnect can't race with the break.
        self.sessions[self.active_idx].status_message = Some("Cancelling query…".into());

        let conn = {
            let map = match self.conn_map.lock() {
                Ok(m) => m,
                Err(_) => {
                    self.sessions[self.active_idx].status_message =
                        Some("Cannot cancel: connection map locked".into());
                    return;
                }
            };
            map.get(&id).cloned()
        };

        match conn {
            Some(conn) => {
                let result = OracleConnection::cancel_query(conn.as_ref());
                match result {
                    Ok(()) => {
                        self.sessions[self.active_idx].status_message =
                            Some("Query cancellation requested".into());
                    }
                    Err(e) => {
                        self.sessions[self.active_idx].status_message =
                            Some(format!("Cancel failed: {}", e));
                    }
                }
            }
            None => {
                self.sessions[self.active_idx].status_message =
                    Some("Cannot cancel: not connected".into());
            }
        }
    }

    pub fn poll_conn_result(&mut self) -> Option<(usize, bool, String)> {
        if let Ok((id, success, err, name)) = self.conn_result_rx.try_recv() {
            if let Some(sess) = self.sessions.iter_mut().find(|s| s.id == id) {
                sess.connecting = false;
                if success {
                    sess.is_connected = true;
                    sess.connect_error = None;
                    sess.name = name;
                } else {
                    sess.connect_error = Some(err);
                }
            }
            Some((id, success, String::new()))
        } else {
            None
        }
    }

    pub fn poll_result(&mut self) -> Option<(usize, QueryRow)> {
        if let Ok(result) = self.result_rx.try_recv() {
            let session_id = result.0;
            let qr = result.1.clone();
            if let Some(sess) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                sess.pending_query = false;

                // A cancellation is not an error — surface it as a status message
                // and clear the stale "Cancelling…" notice, instead of pushing a
                // red error row into the results.
                let cancelled = qr.is_error
                    && qr.error_msg.as_deref().map(|m| Self::is_cancelled_msg(m)).unwrap_or(false);
                if cancelled {
                    sess.status_message = Some("Query cancelled".into());
                    return Some(result);
                }

                if qr.page_offset > 0 {
                    // Pagination continuation — append rows to the last result
                    if let Some(last) = sess.results.last_mut() {
                        if !qr.is_error {
                            last.rows.extend(qr.rows.clone());
                            last.truncated = qr.truncated;
                            last.elapsed_ms += qr.elapsed_ms;
                            // Scroll to the newly loaded rows
                            sess.scroll_offset = last.rows.len().saturating_sub(1);
                        } else {
                            // Push error result so user can see it
                            sess.results.push(qr.clone());
                            sess.current_result_idx = sess.results.len().saturating_sub(1);
                        }
                    }
                } else {
                    // Fresh query result
                    sess.results.push(qr.clone());
                    sess.current_result_idx = sess.results.len().saturating_sub(1);
                    sess.scroll_offset = 0;
                }
            }
            Some(result)
        } else {
            None
        }
    }

    pub fn get_session_info(&self) -> Vec<DbSessionInfo> {
        let id = self.sessions[self.active_idx].id;
        if let Ok(map) = self.conn_map.lock() {
            if let Some(conn) = map.get(&id) {
                if let Ok(info) = OracleConnection::query_v_session(conn.as_ref()) {
                    return info;
                }
            }
        }
        vec![]
    }

    pub fn check_connections(&mut self) {
        let conn_map = self.conn_map.lock().ok();
        if let Some(ref map) = conn_map {
            for session in &mut self.sessions {
                session.is_connected = map.contains_key(&session.id);
            }
        }
    }

    pub fn active_connection(&self) -> Option<Arc<OracleConnection>> {
        let id = self.sessions[self.active_idx].id;
        let map = self.conn_map.lock().ok()?;
        map.get(&id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_file_directive_basic() {
        assert_eq!(parse_file_directive("@file.sql"), Some("file.sql".into()));
        assert_eq!(parse_file_directive("@ file.sql;"), Some("file.sql".into()));
        assert_eq!(parse_file_directive("@@sub/inc.sql"), Some("@@sub/inc.sql".into()));
        assert_eq!(parse_file_directive("select 1 from dual"), None);
        assert_eq!(parse_file_directive("@"), None);
        assert_eq!(parse_file_directive("   "), None);
    }

    #[test]
    fn expand_statements_plain_and_nested() {
        let cwd = std::env::temp_dir();
        let dir = cwd.join("frog_test_expand");
        std::fs::create_dir_all(&dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/inc.sql"), "select 2 from dual;").unwrap();
        std::fs::write(
            dir.join("main.sql"),
            "select 1 from dual;\n@@sub/inc.sql",
        )
        .unwrap();

        // @main resolves from cwd, and nested @@sub/inc.sql resolves relative to main.sql.
        let stmts = expand_statements(vec!["@main.sql".into()], &dir).unwrap();
        assert_eq!(stmts, vec!["select 1 from dual", "select 2 from dual"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expand_statements_missing_file_errors() {
        let res = expand_statements(vec!["@nonexistent_xyz.sql".into()], &std::path::Path::new("/tmp"));
        assert!(res.is_err());
    }

    #[test]
    fn is_cancelled_msg_detects_cancellation() {
        assert!(SessionManager::is_cancelled_msg("Query cancelled"));
        assert!(SessionManager::is_cancelled_msg("ORA-01013: user requested cancel of current operation"));
        assert!(SessionManager::is_cancelled_msg("The operation was interrupted"));
        assert!(!SessionManager::is_cancelled_msg("ORA-00942: table or view does not exist"));
        assert!(!SessionManager::is_cancelled_msg("ORA-00001: unique constraint violated"));
    }
}