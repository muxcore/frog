use crate::cli::ConnectionParams;
use crate::db::connection::{DbConnection, DbSessionInfo, DbType, OracleConnection, QueryRow};
use crate::db::postgres::PgConnection;
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
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Only applied when the file is created: history is owner-only (0o600).
        opts.mode(0o600);
    }
    if let Ok(mut file) = opts.open(&path) {
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
/// Returns an error string on read failure, with a hint for missing files.
pub fn load_script_file(path: &Path) -> Result<Vec<String>, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Helpful hint: @ resolves from CWD, @@ resolves relative to the
            // including file's directory.
            return Err(format!(
                "File not found: '{}'.\n  Tried '{}' (@ resolves from the current working \
                 directory). Use '@@relative/path.sql' to resolve relative to the \
                 including script.",
                path.display(),
                path.display(),
            ));
        }
        Err(e) => return Err(format!("Failed to read file '{}': {}", path.display(), e)),
    };
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

/// Detect whether an (uppercased, trimmed) partial statement starts a PL/SQL
/// block (anonymous or named), in which case interior `;` are not terminators.
/// Note: `CREATE TABLE|VIEW|SEQUENCE|SYNONYM` are *not* PL/SQL — their `;` is a
/// normal terminator.
fn is_plsql_block_start(upper: &str) -> bool {
    if upper.starts_with("DECLARE") || upper.starts_with("BEGIN") {
        return true;
    }
    // CREATE [OR REPLACE] [EDITIONABLE|NONEDITIONABLE] PROCEDURE|FUNCTION|PACKAGE|...
    let mut words = upper.split_whitespace();
    if words.next() != Some("CREATE") {
        return false;
    }
    loop {
        match words.next() {
            None => return false,
            // Skip OR REPLACE / EDITIONABLE modifiers after CREATE.
            Some("OR") => continue,
            Some("REPLACE") | Some("EDITIONABLE") | Some("NONEDITIONABLE") => continue,
            Some("PROCEDURE" | "FUNCTION" | "PACKAGE" | "TRIGGER" | "TYPE" | "JAVA"
            | "LIBRARY") => return true,
            _ => return false,
        }
    }
}

/// Detect whether an (uppercased, trimmed) partial statement ends a PL/SQL
/// block, i.e. its final word is `END` (not merely a suffix like `'LEGEND'`).
fn ends_with_end_block(upper: &str) -> bool {
    upper.split_whitespace().next_back() == Some("END")
}

#[derive(Debug, Clone)]
pub struct SqlEditor {
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub scroll_offset: usize,
}

impl Default for SqlEditor {
    fn default() -> Self {
        Self::new()
    }
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
        self.cursor_col += c.len_utf8();
    }

    pub fn backspace(&mut self) {
        if self.cursor_col > 0 {
            let line = &mut self.lines[self.cursor_row];
            let before = &line[..self.cursor_col];
            let prev = before
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            line.replace_range(prev..self.cursor_col, "");
            self.cursor_col = prev;
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
            let line = &mut self.lines[self.cursor_row];
            let rest = &line[self.cursor_col..];
            let next = self.cursor_col
                + rest
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
            line.replace_range(self.cursor_col..next, "");
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
            let line = &self.lines[self.cursor_row];
            self.cursor_col = line[..self.cursor_col]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].len();
        }
    }

    pub fn move_right(&mut self) {
        let len = self.lines[self.cursor_row].len();
        if self.cursor_col < len {
            let line = &self.lines[self.cursor_row];
            self.cursor_col += line[self.cursor_col..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
        } else if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
    }

    pub fn snap_cursor_to_char_boundary(&mut self) {
        let len = self.lines[self.cursor_row].len();
        self.cursor_col = self.cursor_col.min(len);
        while self.cursor_col < len && !self.lines[self.cursor_row].is_char_boundary(self.cursor_col)
        {
            self.cursor_col += 1;
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
    ///
    /// The scanner is SQL-aware: it tracks single/double-quoted literals and
    /// `--` / `/* */` comments so a `;` inside them is not treated as a
    /// terminator. PL/SQL blocks (`DECLARE`/`BEGIN`/`CREATE ... PROCEDURE|...`)
    /// are kept as a single statement; the block terminates only at `END;`.
    /// Lines containing only `/` still act as a sqlplus-style terminator.
    pub fn statements_with_ranges(text: &str) -> Vec<(usize, usize, String)> {
        let mut out = Vec::new();
        let bytes = text.as_bytes();
        let len = bytes.len();
        let mut start = 0usize;
        let mut i = 0usize;
        let mut line_start = 0usize;

        // Scanner state.
        let mut in_single = false;
        let mut in_double = false;
        let mut in_line_comment = false;
        let mut in_block_comment = false;

        while i < len {
            let c = bytes[i];

            if in_line_comment {
                if c == b'\n' {
                    in_line_comment = false;
                    line_start = i + 1;
                }
                i += 1;
                continue;
            }
            if in_block_comment {
                if c == b'*' && i + 1 < len && bytes[i + 1] == b'/' {
                    in_block_comment = false;
                    i += 2;
                } else {
                    if c == b'\n' {
                        line_start = i + 1;
                    }
                    i += 1;
                }
                continue;
            }
            if in_single {
                if c == b'\'' {
                    in_single = false;
                } else if c == b'\n' {
                    line_start = i + 1;
                }
                i += 1;
                continue;
            }
            if in_double {
                if c == b'"' {
                    in_double = false;
                } else if c == b'\n' {
                    line_start = i + 1;
                }
                i += 1;
                continue;
            }

            // Top-level state.
            match c {
                b'\n' => {
                    let line = text[line_start..i].trim();
                    if line == "/" {
                        let stmt = text[start..line_start]
                            .trim()
                            .trim_end_matches(';')
                            .trim()
                            .to_string();
                        if !stmt.is_empty() {
                            out.push((start, i + 1, stmt));
                        }
                        start = i + 1;
                    }
                    line_start = i + 1;
                }
                b'-' if i + 1 < len && bytes[i + 1] == b'-' => {
                    in_line_comment = true;
                    i += 1;
                }
                b'/' if i + 1 < len && bytes[i + 1] == b'*' => {
                    in_block_comment = true;
                    i += 1;
                }
                b'\'' => in_single = true,
                b'"' => in_double = true,
                b';' => {
                    let acc = text[start..i].trim();
                    let upper = acc.to_uppercase();
                    let is_block = is_plsql_block_start(&upper);
                    if is_block && !ends_with_end_block(&upper) {
                        // Internal `;` inside a PL/SQL block — keep collecting.
                    } else {
                        let stmt = acc.trim_end_matches(';').trim().to_string();
                        if !stmt.is_empty() {
                            out.push((start, i + 1, stmt));
                        }
                        start = i + 1;
                        line_start = i + 1;
                    }
                }
                _ => {}
            }
            i += 1;
        }

        let tail = text[start..]
            .trim()
            .trim_end_matches(';')
            .trim()
            .to_string();
        if !tail.is_empty() && tail != "/" {
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
    pub conn: Option<Arc<dyn DbConnection>>,
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
    /// Page size for pagination (default 100).
    pub page_size: usize,
    /// Temporary status message for notifications (e.g., "No more rows").
    pub status_message: Option<String>,
    /// When `status_message` was last set, used to auto-expire stale notices.
    pub status_message_since: Option<std::time::Instant>,
    /// How long a status message is shown before auto-clearing.
    pub status_message_ttl: std::time::Duration,
    /// Monotonic job sequence — bumped for each new execution so results
    /// from an older (superseded) spawn can be ignored as stale.
    pub job_seq: u64,
    /// Serializes query execution on this session's connection so two spawned
    /// worker threads never call ODPI-C concurrently on the same connection
    /// (breaking is the only operation allowed from the UI thread).
    pub exec_lock: Arc<Mutex<()>>,
    /// F2 browser: index of the highlighted session row.
    pub session_view_cursor: usize,
    /// Full SQL text of the highlighted F2 row (loaded on demand).
    pub plan_sql_text: Option<String>,
    /// Which session `plan_sql_text`/`plan_result` belong to (`"<sid> <user>"`).
    pub plan_for: Option<String>,
    /// Last explain plan output for the highlighted F2 row.
    pub plan_result: Option<QueryRow>,
    /// Vertical scroll offset of the F2 plan pane.
    pub plan_scroll: usize,
    /// Horizontal (char) scroll offset of the F2 plan pane.
    pub plan_hscroll: usize,
    /// Last F2 list failure (e.g. missing V$SESSION privileges), shown in
    /// the browser instead of an unexplained empty table.
    pub session_view_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ConnectionDialog {
    pub db_type: DbType,
    pub host: String,
    pub port: String,
    pub service: String,
    pub database: String,
    pub user: String,
    pub password: String,
    pub active_field: usize,
    /// Byte offset of the insertion cursor within the active field.
    pub cursor: usize,
}

impl Default for ConnectionDialog {
    fn default() -> Self {
        Self {
            db_type: DbType::Oracle,
            host: String::from("localhost"),
            port: String::from("1521"),
            service: String::from("ORCL"),
            database: String::from("postgres"),
            user: String::new(),
            password: String::new(),
            active_field: 0,
            cursor: 0,
        }
    }
}

impl ConnectionDialog {
    /// Field order: 0 = Type, 1 = Host, 2 = Port, 3 = Service/Database,
    /// 4 = User, 5 = Password.
    pub const FIELD_COUNT: usize = 6;

    /// Whether the type selector row is active (not a text field).
    pub fn is_type_field(&self) -> bool {
        self.active_field == 0
    }

    /// Label for field 3, depending on the backend.
    pub fn db_name_label(&self) -> &'static str {
        match self.db_type {
            DbType::Oracle => "Service: ",
            DbType::Postgres => "Database:",
        }
    }

    /// Flip Oracle <-> Postgres. When the port still holds the old backend's
    /// default it is switched to the new backend's default as well.
    pub fn toggle_db_type(&mut self) {
        let new_type = match self.db_type {
            DbType::Oracle => DbType::Postgres,
            DbType::Postgres => DbType::Oracle,
        };
        self.set_db_type(new_type);
    }

    pub fn set_db_type(&mut self, db_type: DbType) {
        let old_default = self.db_type.default_port().to_string();
        self.db_type = db_type;
        if self.port == old_default {
            self.port = db_type.default_port().to_string();
        }
        if db_type == DbType::Postgres && self.database.is_empty() && !self.service.is_empty() {
            self.database = self.service.clone();
        }
        if db_type == DbType::Oracle && self.service.is_empty() && !self.database.is_empty() {
            self.service = self.database.clone();
        }
        self.cursor = 0;
    }

    fn field(&self) -> &String {
        match self.active_field {
            1 => &self.host,
            2 => &self.port,
            3 => match self.db_type {
                DbType::Oracle => &self.service,
                DbType::Postgres => &self.database,
            },
            4 => &self.user,
            5 => &self.password,
            _ => &self.host,
        }
    }

    fn field_mut(&mut self) -> &mut String {
        match self.active_field {
            1 => &mut self.host,
            2 => &mut self.port,
            3 => match self.db_type {
                DbType::Oracle => &mut self.service,
                DbType::Postgres => &mut self.database,
            },
            4 => &mut self.user,
            5 => &mut self.password,
            _ => &mut self.host,
        }
    }

    /// Snap the cursor to a valid position inside the active field.
    pub fn clamp_cursor(&mut self) {
        if self.is_type_field() {
            self.cursor = 0;
            return;
        }
        let len = self.field().len();
        let mut pos = self.cursor.min(len);
        while pos < len && !self.field().is_char_boundary(pos) {
            pos += 1;
        }
        self.cursor = pos;
    }

    /// Select a field by index (mod FIELD_COUNT) and park the cursor at its end.
    pub fn select_field(&mut self, idx: usize) {
        self.active_field = idx % Self::FIELD_COUNT;
        if self.is_type_field() {
            self.cursor = 0;
        } else {
            self.cursor = self.field().len();
        }
    }

    /// Insert a character at the cursor (digits only for the port field,
    /// no-op on the type selector row).
    pub fn insert_char(&mut self, c: char) {
        if self.is_type_field() {
            return;
        }
        if c.is_control() || (self.active_field == 2 && !c.is_ascii_digit()) {
            return;
        }
        self.clamp_cursor();
        let at = self.cursor;
        self.field_mut().insert(at, c);
        self.cursor = at + c.len_utf8();
    }

    /// Delete the character before the cursor.
    pub fn backspace(&mut self) {
        if self.is_type_field() {
            return;
        }
        self.clamp_cursor();
        let cur = self.cursor;
        if cur > 0 {
            let prev = self.field()[..cur]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.field_mut().replace_range(prev..cur, "");
            self.cursor = prev;
        }
    }

    /// Delete the character at the cursor.
    pub fn delete(&mut self) {
        if self.is_type_field() {
            return;
        }
        self.clamp_cursor();
        let next = self.field()[self.cursor..]
            .chars()
            .next()
            .map(|c| c.len_utf8())
            .unwrap_or(0);
        if next > 0 {
            let start = self.cursor;
            let end = start + next;
            self.field_mut().replace_range(start..end, "");
        }
    }

    /// Remove everything from the active field.
    pub fn clear_field(&mut self) {
        if self.is_type_field() {
            return;
        }
        self.field_mut().clear();
        self.cursor = 0;
    }

    pub fn cursor_left(&mut self) {
        if self.is_type_field() {
            self.toggle_db_type();
            return;
        }
        self.clamp_cursor();
        if self.cursor > 0 {
            let prev = self.field()[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.cursor = prev;
        }
    }

    pub fn cursor_right(&mut self) {
        if self.is_type_field() {
            self.toggle_db_type();
            return;
        }
        let s = self.field();
        let next = s[self.cursor..].chars().next().map(|c| c.len_utf8());
        if let Some(n) = next {
            self.cursor += n;
        }
    }

    pub fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    pub fn cursor_end(&mut self) {
        if self.is_type_field() {
            self.cursor = 0;
            return;
        }
        self.cursor = self.field().len();
    }

    /// Build connection parameters from the dialog contents.
    pub fn to_params(&self) -> ConnectionParams {
        let port = self.port.parse::<u16>().unwrap_or_else(|_| self.db_type.default_port());
        let (service, database) = match self.db_type {
            DbType::Oracle => {
                let service = if self.service.is_empty() {
                    self.database.clone()
                } else {
                    self.service.clone()
                };
                let service = if service.is_empty() { "ORCL".into() } else { service };
                let database = service.clone();
                (service, database)
            }
            DbType::Postgres => {
                let database = if self.database.is_empty() {
                    if self.service.is_empty() {
                        if self.user.is_empty() {
                            "postgres".into()
                        } else {
                            self.user.clone()
                        }
                    } else {
                        self.service.clone()
                    }
                } else {
                    self.database.clone()
                };
                let service = database.clone();
                (service, database)
            }
        };
        ConnectionParams {
            db_type: self.db_type,
            host: self.host.clone(),
            port,
            service,
            database,
            user: self.user.clone(),
            password: Some(self.password.clone()).filter(|p| !p.is_empty()),
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
    /// Set a status message and start its auto-expiry timer.
    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some(msg.into());
        self.status_message_since = Some(std::time::Instant::now());
    }

    /// Clear any status message whose TTL has elapsed.
    pub fn tick_status(&mut self) {
        if let Some(since) = self.status_message_since {
            if since.elapsed() >= self.status_message_ttl {
                self.status_message = None;
                self.status_message_since = None;
            }
        }
    }

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
            status_message_since: None,
            status_message_ttl: std::time::Duration::from_secs(5),
            job_seq: 0,
            exec_lock: Arc::new(Mutex::new(())),
            session_view_cursor: 0,
            plan_sql_text: None,
            plan_for: None,
            plan_result: None,
            plan_scroll: 0,
            plan_hscroll: 0,
            session_view_error: None,
        }
    }
}

pub struct SessionManager {
    pub sessions: Vec<Session>,
    pub active_idx: usize,
    next_id: AtomicUsize,
    result_rx: Receiver<(usize, u64, QueryRow)>,
    result_tx: Sender<(usize, u64, QueryRow)>,
    conn_result_rx: Receiver<(usize, bool, String, String)>,
    conn_result_tx: Sender<(usize, bool, String, String)>,
    conn_map: Arc<Mutex<HashMap<usize, Arc<dyn DbConnection>>>>,
    /// Maximum number of query-history entries kept per session.
    pub max_history: usize,
    /// Maximum number of result pages kept per session before trimming.
    pub max_results_per_session: usize,
    /// Whether new connections should run with autocommit enabled.
    pub autocommit: bool,
    /// Global soft cap on total rows fetched per query (from config/max_rows).
    pub max_rows: usize,
    /// Display string for NULL cells (from UiConfig.null_display).
    pub null_display: String,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let (ctx, crx) = mpsc::channel();
        let sessions = vec![Session::new(1)];
        Self {
            sessions,
            active_idx: 0,
            next_id: AtomicUsize::new(2),
            result_rx: rx,
            result_tx: tx,
            conn_result_rx: crx,
            conn_result_tx: ctx,
            conn_map: Arc::new(Mutex::new(HashMap::new())),
            max_history: 1000,
            max_results_per_session: 200,
            autocommit: true,
            max_rows: 10000,
            null_display: "(NULL)".into(),
        }
    }

    pub fn active_session(&self) -> &Session {
        &self.sessions[self.active_idx]
    }

    pub fn active_session_mut(&mut self) -> &mut Session {
        &mut self.sessions[self.active_idx]
    }

    /// Expire any status messages whose TTL has elapsed (called each frame).
    pub fn tick_statuses(&mut self) {
        for sess in &mut self.sessions {
            sess.tick_status();
        }
    }

    /// Bump the session's job sequence and hand back the new token, so a result
    /// produced by this generation can be distinguished from older ones.
    fn next_job_seq(&mut self, idx: usize) -> u64 {
        self.sessions[idx].job_seq += 1;
        self.sessions[idx].job_seq
    }

    /// Synthesize and enqueue an error result for a session (used by
    /// pre-execution failures like a bad `@file` or expansion error).
    fn emit_error(&mut self, idx: usize, msg: String) {
        let seq = self.next_job_seq(idx);
        let id = self.sessions[idx].id;
        self.sessions[idx].pending_query = false;
        self.sessions[idx].set_status(msg.clone());
        let result_tx = self.result_tx.clone();
        let page_size = self.sessions[idx].page_size;
        let err_row = QueryRow {
            columns: vec![],
            rows: vec![],
            truncated: false,
            row_count: 0,
            elapsed_ms: 0,
            is_error: true,
            error_msg: Some(msg),
            rows_affected: None,
            page_offset: 0,
            page_size,
            byte_count: 0,
            total_fetched: 0,
        };
        let _ = result_tx.send((id, seq, err_row));
    }

    /// Open a backend connection in a background thread and report back
    /// through the conn_result channel.
    fn spawn_connect(
        conn_map: Arc<Mutex<HashMap<usize, Arc<dyn DbConnection>>>>,
        conn_result_tx: Sender<(usize, bool, String, String)>,
        id: usize,
        params: ConnectionParams,
        autocommit: bool,
        null_display: String,
    ) {
        thread::spawn(move || {
            let name_str = params.display_name();
            let result: Result<Arc<dyn DbConnection>, anyhow::Error> = match params.db_type {
                DbType::Oracle => OracleConnection::connect_with_autocommit(
                    &params.oracle_connect_string(),
                    &params.user,
                    params.password.as_deref().unwrap_or(""),
                    autocommit,
                    &null_display,
                )
                .map(|c| c as Arc<dyn DbConnection>),
                DbType::Postgres => PgConnection::connect(
                    &params.host,
                    params.port,
                    &params.database,
                    &params.user,
                    params.password.as_deref().unwrap_or(""),
                    &null_display,
                )
                .map(|c| c as Arc<dyn DbConnection>),
            };
            match result {
                Ok(conn) => {
                    if let Ok(mut map) = conn_map.lock() {
                        map.insert(id, conn);
                    }
                    let _ = conn_result_tx.send((id, true, String::new(), name_str));
                }
                Err(e) => {
                    let msg = if params.db_type == DbType::Oracle {
                        OracleConnection::friendly_connect_error(&e)
                    } else {
                        e.to_string()
                    };
                    let _ = conn_result_tx.send((id, false, msg, name_str));
                }
            }
        });
    }

    pub fn add_session(&mut self) -> usize {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut session = Session::new(id);
        session.conn_dialog = self.sessions[self.active_idx].conn_dialog.clone();

        // If active session is connected, auto-connect the new session with same params
        let active = &self.sessions[self.active_idx];
        let should_connect = active.is_connected && !active.connecting;
        let params = active.conn_dialog.to_params();

        self.sessions.push(session);
        self.active_idx = self.sessions.len() - 1;
        let new_idx = self.active_idx;

        if should_connect && !params.user.is_empty() {
            let id = self.sessions[new_idx].id;
            Self::spawn_connect(
                self.conn_map.clone(),
                self.conn_result_tx.clone(),
                id,
                params,
                self.autocommit,
                self.null_display.clone(),
            );

            self.sessions[new_idx].connecting = true;
            self.sessions[new_idx].connect_error = None;
        }

        new_idx
    }

    pub fn remove_session(&mut self, idx: usize) {
        if self.sessions.len() <= 1 {
            return;
        }
        let removed_id = self.sessions[idx].id;
        self.sessions.remove(idx);
        if self.active_idx >= self.sessions.len() {
            self.active_idx = self.sessions.len() - 1;
        } else if idx < self.active_idx {
            // A tab before the active one was removed; keep pointing at the
            // same session.
            self.active_idx -= 1;
        }

        // Release the DB connection for the removed session so the underlying
        // DB session is closed promptly instead of lingering until app exit.
        if let Ok(mut map) = self.conn_map.lock() {
            if let Some(conn) = map.remove(&removed_id) {
                let _ = conn.close();
            }
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

    pub fn connect_active(&mut self, params: &ConnectionParams) {
        let id = self.sessions[self.active_idx].id;
        Self::spawn_connect(
            self.conn_map.clone(),
            self.conn_result_tx.clone(),
            id,
            params.clone(),
            self.autocommit,
            self.null_display.clone(),
        );

        self.sessions[self.active_idx].connecting = true;
        self.sessions[self.active_idx].connect_error = None;
    }

    /// Backend of the active session's connection if connected, else the
    /// backend selected in its connection dialog.
    pub fn active_db_type(&self) -> DbType {
        if let Ok(map) = self.conn_map.lock() {
            if let Some(conn) = map.get(&self.sessions[self.active_idx].id) {
                return conn.db_type();
            }
        }
        self.sessions[self.active_idx].conn_dialog.db_type
    }

    pub fn execute_query(&mut self, sql: &str) {
        // @file / @ file.sql directive: read the file and run it as a script,
        // honoring nested @file includes and aborting on the first error.
        if let Some(raw) = parse_file_directive(sql) {
            let path = PathBuf::from(raw.trim_start_matches("@@").trim());
            match load_script_file(&path) {
                Ok(statements) => {
                    self.sessions[self.active_idx].set_status(format!(
                        "Running script '{}' ({} statements)",
                        path.display(),
                        statements.len()
                    ));
                    self.execute_script(statements);
                }
                Err(err) => {
                    self.emit_error(self.active_idx, err.clone());
                }
            }
            return;
        }

        // last_sql (for pagination) is recorded per statement in execute_script.
        self.execute_script(vec![sql.to_string()]);
    }

    pub fn execute_script(&mut self, statements: Vec<String>) {
        // Expand any nested @file directives found among the statements before running.
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let statements = match expand_statements(statements, &cwd) {
            Ok(stmts) => stmts,
            Err(err) => {
                self.emit_error(self.active_idx, err);
                return;
            }
        };
        if statements.is_empty() {
            return;
        }
        let idx = self.active_idx;
        let seq = self.next_job_seq(idx);
        let id = self.sessions[idx].id;
        let conn_map = self.conn_map.clone();
        let result_tx = self.result_tx.clone();
        let page_size = self.sessions[idx].page_size;
        let exec_lock = self.sessions[idx].exec_lock.clone();

        self.sessions[idx].pending_query = true;

        // Persist every executed statement to disk history + session history
        // Also track last SELECT for pagination
        {
            let max_history = self.max_history;
            let sess = &mut self.sessions[self.active_idx];
            for sql in &statements {
                let trimmed = sql.trim().to_string();
                if !trimmed.is_empty() {
                    append_history(&trimmed);
                    sess.query_history.push(trimmed.clone());
                    if crate::db::connection::is_query_sql_for(
                        sess.conn_dialog.db_type,
                        &trimmed,
                    ) {
                        sess.last_sql = Some(trimmed);
                    }
                }
            }
            // Enforce the in-memory history cap (drop oldest entries).
            if sess.query_history.len() > max_history {
                let excess = sess.query_history.len() - max_history;
                sess.query_history.drain(0..excess);
            }
            // Park the cursor on the most recent entry.
            sess.history_cursor = sess.query_history.len().saturating_sub(1);
        }

        thread::spawn(move || {
            // Serialize on this session's connection so only one statement runs
            // at a time on it (avoids concurrent driver calls from duplicate spawns).
            let _guard = exec_lock.lock().ok();
            let conn_opt = conn_map.lock().ok().and_then(|m| m.get(&id).cloned());
            if let Some(conn) = conn_opt {
                let mut had_error = false;
                for (n, sql) in statements.iter().enumerate() {
                    if had_error {
                        break;
                    }
                    let result = conn.execute_query(sql, page_size);
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

                    let _ = result_tx.send((id, seq, result));
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
                let _ = result_tx.send((id, seq, err));
            }
        });
    }

    /// Fetch the next page of results for the last SELECT query.
    /// New rows are appended to the last QueryRow in session.results.
    pub fn fetch_next_page(&mut self) {
        let idx = self.active_idx;
        // Extract the data we need while the immutable borrow is alive, then
        // release it before taking mutable deps (job_seq, status_message).
        let (sql, current_offset, page_size, id) = {
            let sess = &self.sessions[idx];
            let sql = match &sess.last_sql {
                Some(s) => s.clone(),
                None => return,
            };
            let (current_offset, page_size) = match sess.results.last() {
                Some(r) if r.truncated && !r.is_error => {
                    (r.page_offset + r.rows.len(), r.page_size)
                }
                _ => {
                    self.sessions[idx].set_status("No more rows to fetch");
                    return;
                }
            };
            (sql, current_offset, page_size, sess.id)
        };

        // Honor the max_rows soft cap: never fetch past the total limit across pages.
        if self.max_rows > 0 && current_offset >= self.max_rows {
            self.sessions[idx].set_status(format!("Reached max_rows limit ({})", self.max_rows));
            self.sessions[idx].pending_query = false;
            return;
        }
        let allowed = if self.max_rows > 0 {
            // current_offset < max_rows is guaranteed by the check above.
            (self.max_rows - current_offset).min(page_size)
        } else {
            page_size
        };

        let seq = self.next_job_seq(idx);
        let conn_map = self.conn_map.clone();
        let result_tx = self.result_tx.clone();
        let exec_lock = self.sessions[idx].exec_lock.clone();

        self.sessions[idx].pending_query = true;

        thread::spawn(move || {
            let _guard = exec_lock.lock().ok();
            let conn_opt = conn_map.lock().ok().and_then(|m| m.get(&id).cloned());
            if let Some(conn) = conn_opt {
                let result = conn.execute_query_paged(&sql, current_offset, allowed);
                let _ = result_tx.send((id, seq, result));
            }
        });
    }

    /// Whether a backend error message indicates a user-requested cancellation.
    pub fn is_cancelled_msg(msg: &str) -> bool {
        msg == "Query cancelled"
            || msg.contains("ORA-01013")
            || msg.contains("ORA-01014")
            || msg.contains("57014")
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
        self.sessions[self.active_idx].set_status("Cancelling query…");

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
                let result = conn.cancel_query();
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

    pub fn poll_conn_result(&mut self) {
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
        }
    }

    pub fn poll_result(&mut self) {
        if let Ok((session_id, seq, qr)) = self.result_rx.try_recv() {
            let cap = self.max_results_per_session;
            if let Some(sess) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                // Ignore results from an outdated job (superseded by a newer
                // execution on the same session) — prevents interleaving/stale data.
                let is_stale = seq < sess.job_seq;
                if is_stale {
                    return;
                }
                sess.pending_query = false;

                // A cancellation is not an error — surface it as a status message
                // and clear the stale "Cancelling…" notice, instead of pushing a
                // red error row into the results.
                let cancelled = qr.is_error
                    && qr
                        .error_msg
                        .as_deref()
                        .map(Self::is_cancelled_msg)
                        .unwrap_or(false);
                if cancelled {
                    sess.set_status("Query cancelled");
                    return;
                }

                if qr.page_offset > 0 {
                    // Pagination continuation — append rows to the last result
                    if let Some(last) = sess.results.last_mut() {
                        if !qr.is_error {
                            last.rows.extend(qr.rows);
                            last.truncated = qr.truncated;
                            last.elapsed_ms += qr.elapsed_ms;
                            // Scroll to the newly loaded rows
                            sess.scroll_offset = last.rows.len().saturating_sub(1);
                        } else {
                            // A paged fetch failed: surface it as a status-line
                            // message instead of polluting the result list with a
                            // duplicate/error row on top of the existing data.
                            let msg = qr
                                .error_msg
                                .as_deref()
                                .unwrap_or("Failed to fetch more rows");
                            sess.set_status(format!("⚠ {}", msg));
                        }
                    }
                } else {
                    // Fresh query result
                    sess.results.push(qr);
                    sess.current_result_idx = sess.results.len().saturating_sub(1);
                    sess.scroll_offset = 0;
                }

                // Enforce the per-session results cap (drop oldest pages).
                if sess.results.len() > cap {
                    let excess = sess.results.len() - cap;
                    sess.results.drain(0..excess);
                    sess.current_result_idx = sess.current_result_idx.saturating_sub(excess);
                }
            }
        }
    }

    /// Query the backend for its session list. Failures (notably missing
    /// `V$SESSION` privileges on Oracle) are returned as a displayable
    /// message instead of being swallowed.
    pub fn get_session_info(&self) -> Result<Vec<DbSessionInfo>, String> {
        let id = self.sessions[self.active_idx].id;
        let conn = self
            .conn_map
            .lock()
            .ok()
            .and_then(|m| m.get(&id).cloned());
        match conn {
            Some(c) => c.query_sessions().map_err(|e| {
                if c.db_type() == DbType::Oracle {
                    OracleConnection::friendly_session_error(&e.to_string())
                } else {
                    e.to_string()
                }
            }),
            None => Err("Not connected. Use Ctrl+O to connect.".into()),
        }
    }

    /// Refresh the F2 list, recording any failure on the active session so
    /// the browser can show it (e.g. missing dictionary privileges).
    pub(crate) fn fresh_session_list(&mut self) -> Vec<DbSessionInfo> {
        let idx = self.active_idx;
        match self.get_session_info() {
            Ok(list) => {
                self.sessions[idx].session_view_error = None;
                list
            }
            Err(msg) => {
                self.sessions[idx].session_view_error = Some(msg);
                vec![]
            }
        }
    }

    /// Open the F2 session browser: reset selection/plan state and preload
    /// the current SQL of the first row (best effort).
    pub fn enter_session_view(&mut self) {
        let idx = self.active_idx;
        {
            let s = &mut self.sessions[idx];
            s.mode = SessionMode::SessionView;
            s.session_view_cursor = 0;
            s.plan_sql_text = None;
            s.plan_for = None;
            s.plan_result = None;
            s.plan_scroll = 0;
            s.plan_hscroll = 0;
            s.session_view_error = None;
        }
        self.load_selected_sql();
    }

    /// Move the F2 highlight and preload that row's SQL (best effort).
    pub fn move_session_cursor(&mut self, delta: isize) {
        let len = self.fresh_session_list().len();
        if len == 0 {
            return;
        }
        let idx = self.active_idx;
        let cur = self.sessions[idx].session_view_cursor as isize;
        let next = cur.saturating_add(delta).clamp(0, len as isize - 1) as usize;
        if next != self.sessions[idx].session_view_cursor {
            self.sessions[idx].session_view_cursor = next;
            self.load_selected_sql();
        }
    }

    /// Load the full SQL text of the highlighted F2 row into `plan_sql_text`.
    /// Clears any previous plan; failures surface as a status message.
    pub fn load_selected_sql(&mut self) {
        let idx = self.active_idx;
        let cursor = self.sessions[idx].session_view_cursor;
        let info = match self.fresh_session_list().into_iter().nth(cursor) {
            Some(info) => info,
            None => {
                let s = &mut self.sessions[idx];
                s.plan_sql_text = None;
                s.plan_result = None;
                s.plan_for = None;
                return;
            }
        };
        let label = format!("{} {}", info.sid, info.username);
        let id = self.sessions[idx].id;
        let conn = self
            .conn_map
            .lock()
            .ok()
            .and_then(|m| m.get(&id).cloned());
        let sess = &mut self.sessions[idx];
        sess.plan_for = Some(label);
        sess.plan_scroll = 0;
        sess.plan_hscroll = 0;
        sess.plan_result = None;
        match conn {
            Some(c) => match c.session_sql(&info) {
                Ok(text) => sess.plan_sql_text = Some(text),
                Err(e) => {
                    sess.plan_sql_text = None;
                    sess.set_status(format!("No SQL text: {}", e));
                }
            },
            None => {
                sess.plan_sql_text = None;
                sess.set_status("Not connected");
            }
        }
    }

    /// Run EXPLAIN for the highlighted F2 row's SQL and store the plan.
    pub fn explain_selected(&mut self) {
        let idx = self.active_idx;
        if self.sessions[idx].plan_sql_text.is_none() {
            self.load_selected_sql();
        }
        let (sql, max_rows) = {
            let sess = &self.sessions[idx];
            match sess.plan_sql_text.clone() {
                Some(t) if !t.trim().is_empty() => (t, sess.page_size),
                _ => return,
            }
        };
        let id = self.sessions[idx].id;
        let conn = self
            .conn_map
            .lock()
            .ok()
            .and_then(|m| m.get(&id).cloned());
        match conn {
            Some(c) => {
                let result = c.explain_plan(&sql, max_rows);
                let sess = &mut self.sessions[idx];
                sess.plan_result = Some(result);
                sess.plan_scroll = 0;
                sess.plan_hscroll = 0;
            }
            None => self.sessions[idx].set_status("Not connected"),
        }
    }

    pub fn check_connections(&mut self) {
        let conn_map = self.conn_map.lock().ok();
        if let Some(ref map) = conn_map {
            for session in &mut self.sessions {
                session.is_connected = map.contains_key(&session.id);
            }
        }
    }

    pub fn active_connection(&self) -> Option<Arc<dyn DbConnection>> {
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
        assert_eq!(
            parse_file_directive("@@sub/inc.sql"),
            Some("@@sub/inc.sql".into())
        );
        assert_eq!(parse_file_directive("select 1 from dual"), None);
        assert_eq!(parse_file_directive("@"), None);
        assert_eq!(parse_file_directive("   "), None);
    }

    #[test]
    fn conn_dialog_toggle_switches_port_default() {
        let mut dlg = ConnectionDialog::default();
        assert_eq!(dlg.db_type, DbType::Oracle);
        assert_eq!(dlg.port, "1521");
        dlg.toggle_db_type();
        assert_eq!(dlg.db_type, DbType::Postgres);
        assert_eq!(dlg.port, "5432");
        // Custom ports are left alone.
        dlg.port = "15432".into();
        dlg.toggle_db_type();
        assert_eq!(dlg.db_type, DbType::Oracle);
        assert_eq!(dlg.port, "15432");
    }

    #[test]
    fn conn_dialog_to_params_cross_fills_names() {
        let mut dlg = ConnectionDialog::default();
        dlg.set_db_type(DbType::Postgres);
        dlg.service = "myapp".into();
        dlg.database.clear();
        let params = dlg.to_params();
        assert_eq!(params.database, "myapp");
        assert_eq!(params.port, 5432);
    }

    #[test]
    fn expand_statements_plain_and_nested() {
        let cwd = std::env::temp_dir();
        let dir = cwd.join("frog_test_expand");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/inc.sql"), "select 2 from dual;").unwrap();
        std::fs::write(dir.join("main.sql"), "select 1 from dual;\n@@sub/inc.sql").unwrap();

        // @main resolves from cwd, and nested @@sub/inc.sql resolves relative to main.sql.
        let stmts = expand_statements(vec!["@main.sql".into()], &dir).unwrap();
        assert_eq!(stmts, vec!["select 1 from dual", "select 2 from dual"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expand_statements_missing_file_errors() {
        let res = expand_statements(
            vec!["@nonexistent_xyz.sql".into()],
            std::path::Path::new("/tmp"),
        );
        assert!(res.is_err());
    }

    #[test]
    fn is_cancelled_msg_detects_cancellation() {
        assert!(SessionManager::is_cancelled_msg("Query cancelled"));
        assert!(SessionManager::is_cancelled_msg(
            "ORA-01013: user requested cancel of current operation"
        ));
        assert!(SessionManager::is_cancelled_msg(
            "The operation was interrupted"
        ));
        assert!(!SessionManager::is_cancelled_msg(
            "ORA-00942: table or view does not exist"
        ));
        assert!(!SessionManager::is_cancelled_msg(
            "ORA-00001: unique constraint violated"
        ));
    }

    fn split(text: &str) -> Vec<String> {
        SqlEditor::statements_with_ranges(text)
            .into_iter()
            .map(|(_, _, s)| s)
            .collect()
    }

    #[test]
    fn splitter_respects_string_literals() {
        assert_eq!(
            split("select 'a;b' from dual;"),
            vec!["select 'a;b' from dual"]
        );
        assert_eq!(
            split("insert into t values ('x;y;z');"),
            vec!["insert into t values ('x;y;z')"]
        );
    }

    #[test]
    fn splitter_respects_comments() {
        // `;` inside a line comment is not a terminator; single statement produced.
        assert_eq!(
            split("select 1 -- ; ;\nfrom dual;"),
            vec!["select 1 -- ; ;\nfrom dual"]
        );
        let sql = "select /* ; ; */ 1 from dual;";
        assert_eq!(split(sql), vec!["select /* ; ; */ 1 from dual"]);
    }

    #[test]
    fn splitter_keeps_plsql_block_together() {
        // Interior `;` are kept; the block is one statement that terminates at END;
        let block = "BEGIN\n  x := 1;\n  dbms_output.put_line('hi');\nEND;\n/";
        assert_eq!(
            split(block),
            vec!["BEGIN\n  x := 1;\n  dbms_output.put_line('hi');\nEND"]
        );
    }

    #[test]
    fn splitter_does_not_treat_create_table_as_plsql() {
        assert_eq!(
            split("create table t (id number);\nselect 1 from dual;"),
            vec!["create table t (id number)", "select 1 from dual"]
        );
        // Named PL/SQL blocks still stay together.
        assert_eq!(
            split("CREATE OR REPLACE PROCEDURE p AS BEGIN x := 1; END;"),
            vec!["CREATE OR REPLACE PROCEDURE p AS BEGIN x := 1; END"]
        );
    }

    #[test]
    fn splitter_end_block_requires_exact_word() {
        // An identifier ending in "END" must not terminate a PL/SQL block...
        let sql = "BEGIN\n  x := LEGEND;\n  y := 2;\nEND;";
        assert_eq!(split(sql), vec![sql.trim_end_matches(';')]);
        // ...and a normal DML statement still splits normally.
        assert_eq!(
            split("select * from t where w = 'END'"),
            vec!["select * from t where w = 'END'"]
        );
    }

    #[test]
    fn splitter_splits_multiple_statements() {
        assert_eq!(
            split("select 1 from dual; select 2 from dual;"),
            vec!["select 1 from dual", "select 2 from dual"]
        );
    }

    #[test]
    fn editor_handles_multibyte_chars() {
        let mut ed = SqlEditor::new();
        ed.lines[0] = "from v".to_string();
        ed.cursor_col = "from v".len();

        ed.insert_char('§');
        assert_eq!(ed.lines[0], "from v§");
        assert!(ed.lines[0].is_char_boundary(ed.cursor_col));

        ed.insert_char('e');
        ed.insert_char('x');
        assert_eq!(ed.lines[0], "from v§ex");
        assert!(ed.lines[0].is_char_boundary(ed.cursor_col));

        ed.move_left();
        ed.move_left();
        ed.move_left();
        assert!(ed.lines[0].is_char_boundary(ed.cursor_col));

        ed.move_right();
        ed.move_right();
        ed.move_right();
        assert!(ed.lines[0].is_char_boundary(ed.cursor_col));

        ed.backspace();
        assert_eq!(ed.lines[0], "from v§e");
        assert!(ed.lines[0].is_char_boundary(ed.cursor_col));

        ed.move_left();
        ed.delete();
        assert_eq!(ed.lines[0], "from v§");
        assert!(ed.lines[0].is_char_boundary(ed.cursor_col));

        ed.move_left();
        ed.delete();
        assert_eq!(ed.lines[0], "from v");
        assert!(ed.lines[0].is_char_boundary(ed.cursor_col));
    }
}
