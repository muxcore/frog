use crate::cli::ConnectionParams;
use crate::db::completion::{self, Clause, CompletionContext, CompletionPopup};
use crate::db::connection::{
    ColumnInfo, DbConnection, DbSessionInfo, DbType, OracleConnection, QueryRow, TableRef,
};
use crate::db::ddl::{self, DdlTarget, DescribedObject};
use crate::db::metadata::{self, MetaKind, MetaMsg, MetaRequest, MetadataStore};
use crate::db::perf::{self, PerfHistory, PerfSnapshot};
use crate::db::postgres::{pg_quote_ident, PgConnection};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

/// Maximum on-disk history file size before compacting oldest entries (2 MB).
const MAX_HISTORY_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_DISK_HISTORY_ENTRIES: usize = 1000;

/// Legacy history location next to the frog binary (used as read fallback so
/// upgrading users keep existing history, and as write fallback if `$HOME` is unavailable).
fn legacy_history_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("frog_history.txt")))
        .unwrap_or_else(|| PathBuf::from("frog_history.txt"))
}

/// Per-user history file path (`$XDG_STATE_HOME/frog/frog_history.txt` or
/// platform equivalent, overridable via `FROG_HISTORY_FILE`). Avoids silent
/// permission failures when `frog` is installed in `/usr/local/bin` or `/usr/bin`.
pub fn history_path() -> PathBuf {
    if let Some(custom) = std::env::var_os("FROG_HISTORY_FILE").filter(|s| !s.is_empty()) {
        return PathBuf::from(custom);
    }
    #[cfg(test)]
    {
        std::env::temp_dir().join(format!("frog_test_history_{}.txt", std::process::id()))
    }
    #[cfg(not(test))]
    {
        if let Some(base) = dirs::state_dir().or_else(dirs::data_local_dir) {
            return base.join("frog").join("frog_history.txt");
        }
        legacy_history_path()
    }
}

fn is_history_marker(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("-- frog@") else {
        return false;
    };
    let Some(ts) = rest.strip_suffix(';') else {
        return false;
    };
    ts.len() == 19
        && ts
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b' ' | b':'))
}

pub(crate) fn parse_history_contents(contents: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for line in contents.lines() {
        if is_history_marker(line) {
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

pub fn load_history() -> Vec<String> {
    let primary = history_path();
    let contents = match std::fs::read_to_string(&primary) {
        Ok(c) => c,
        Err(_) => {
            let legacy = legacy_history_path();
            if legacy != primary {
                match std::fs::read_to_string(&legacy) {
                    Ok(c) => c,
                    Err(_) => return Vec::new(),
                }
            } else {
                return Vec::new();
            }
        }
    };
    parse_history_contents(&contents)
}

fn ensure_history_parent(path: &Path) {
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        let _ = builder.create(parent);
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::create_dir_all(parent);
    }
}

fn open_history_append(path: &Path) -> std::io::Result<std::fs::File> {
    ensure_history_parent(path);
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Only applied when the file is created: history is owner-only (0o600).
        opts.mode(0o600);
    }
    opts.open(path)
}

fn maybe_compact_history_file(path: &Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= MAX_HISTORY_FILE_BYTES {
        return;
    }
    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };
    let entries = parse_history_contents(&contents);
    let keep_from = entries.len().saturating_sub(MAX_DISK_HISTORY_ENTRIES);
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let mut compacted = String::new();
    for stmt in &entries[keep_from..] {
        compacted.push_str(&format!("-- frog@{};\n{};\n", ts, stmt));
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(path) {
        use std::io::Write;
        let _ = f.write_all(compacted.as_bytes());
    }
}

pub fn append_history(sql: &str) {
    use std::io::Write;
    let primary = history_path();
    let entry = format!(
        "-- frog@{};\n{};\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        sql.trim().trim_end_matches(';')
    );
    if let Ok(mut file) = open_history_append(&primary) {
        let _ = file.write_all(entry.as_bytes());
        drop(file);
        maybe_compact_history_file(&primary);
        return;
    }
    let legacy = legacy_history_path();
    if legacy != primary {
        if let Ok(mut file) = open_history_append(&legacy) {
            let _ = file.write_all(entry.as_bytes());
            drop(file);
            maybe_compact_history_file(&legacy);
        }
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

#[derive(Debug, Clone)]
pub struct SqlEditor {
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub scroll_offset: usize,
    /// Horizontal scroll in display cells (for lines wider than the pane).
    pub col_offset: usize,
    undo_stack: Vec<(Vec<String>, usize, usize)>,
    redo_stack: Vec<(Vec<String>, usize, usize)>,
}

/// Rectangular cell selection in the result table. Row endpoints are
/// `None` for the header row (column names), `Some(r)` for absolute
/// data-row indices; columns are absolute indices. Built by mouse drag in
/// Table view, copied as header + TSV (no box borders) on release.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TableSelection {
    pub anchor_row: Option<usize>,
    pub anchor_col: usize,
    pub cursor_row: Option<usize>,
    pub cursor_col: usize,
}

impl TableSelection {
    /// Endpoints ordered so `start <= end` (row-major; the header sorts
    /// before every data row).
    pub fn ordered(&self) -> ((Option<usize>, usize), (Option<usize>, usize)) {
        let a = (self.anchor_row, self.anchor_col);
        let b = (self.cursor_row, self.cursor_col);
        if a <= b {
            (a, b)
        } else {
            (b, a)
        }
    }

    pub fn is_empty(&self) -> bool {
        let (a, b) = self.ordered();
        a == b
    }

    /// Clamp to the data dimensions. Rows stay `Option` (`None` = header);
    /// returns `None` for an empty selection or when there is nothing to
    /// copy (no columns, or neither header-selectable columns... ).
    /// With zero data rows a header-only selection still clamps fine.
    pub fn clamp(&self, total_rows: usize, total_cols: usize) -> Option<TableRect> {
        if self.is_empty() || total_cols == 0 {
            return None;
        }
        let ((r0, c0), (r1, c1)) = self.ordered();
        let c1 = c1.min(total_cols - 1);
        let c0 = c0.min(c1);
        if total_rows == 0 {
            return Some(((None, c0), (None, c1)));
        }
        let r1 = r1.map(|r| r.min(total_rows - 1));
        let max_r = r1.unwrap_or(total_rows - 1);
        let r0 = r0.map(|r| r.min(max_r));
        Some(((r0, c0), (r1, c1)))
    }
}

/// In-editor drag selection (row + byte-column endpoints, both kept on char
/// boundaries). Created by mouse-drag with capture ON, copied to the
/// clipboard on release — no terminal-native selection needed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EditorSelection {
    pub anchor_row: usize,
    pub anchor_col: usize,
    pub cursor_row: usize,
    pub cursor_col: usize,
}

impl EditorSelection {
    /// Endpoints ordered so `start <= end` (row-major).
    pub fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        let a = (self.anchor_row, self.anchor_col);
        let b = (self.cursor_row, self.cursor_col);
        if a <= b {
            (a, b)
        } else {
            (b, a)
        }
    }

    pub fn is_empty(&self) -> bool {
        let (a, b) = self.ordered();
        a == b
    }
}

/// Ordered, clamped table-selection endpoints: `((r0, c0), (r1, c1))`.
/// Rows are `None` for the header row.
pub type TableRect = ((Option<usize>, usize), (Option<usize>, usize));

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
            col_offset: 0,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        }
    }

    fn push_undo(&mut self) {
        // Coalesce keystroke undo: cap history at 100 snapshots.
        let snap = (self.lines.clone(), self.cursor_row, self.cursor_col);
        if self.undo_stack.last().is_some_and(|s| s == &snap) {
            return;
        }
        self.undo_stack.push(snap);
        if self.undo_stack.len() > 100 {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
    }

    /// Undo the last edit (Alt+Z). Returns true when something changed.
    pub fn undo(&mut self) -> bool {
        let Some((lines, row, col)) = self.undo_stack.pop() else {
            return false;
        };
        self.redo_stack
            .push((self.lines.clone(), self.cursor_row, self.cursor_col));
        self.lines = lines;
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cursor_row = row.min(self.lines.len().saturating_sub(1));
        self.cursor_col = col;
        self.snap_cursor_to_char_boundary();
        true
    }

    /// Redo an undone edit (Alt+Shift+Z). Returns true when something changed.
    pub fn redo(&mut self) -> bool {
        let Some((lines, row, col)) = self.redo_stack.pop() else {
            return false;
        };
        self.undo_stack
            .push((self.lines.clone(), self.cursor_row, self.cursor_col));
        self.lines = lines;
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cursor_row = row.min(self.lines.len().saturating_sub(1));
        self.cursor_col = col;
        self.snap_cursor_to_char_boundary();
        true
    }

    fn is_word_char(c: char) -> bool {
        c.is_alphanumeric() || matches!(c, '_' | '$' | '#')
    }

    /// Move one word left (Emacs Alt+B semantics, ASCII/Unicode aware).
    pub fn move_word_left(&mut self) {
        self.snap_cursor_to_char_boundary();
        // Step 1: skip separators (and newlines) backward.
        loop {
            if self.cursor_col > 0 {
                let line = &self.lines[self.cursor_row];
                let ch = line[..self.cursor_col].chars().next_back().unwrap_or(' ');
                if Self::is_word_char(ch) {
                    break;
                }
                let prev = line[..self.cursor_col]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                self.cursor_col = prev;
            } else if self.cursor_row > 0 {
                self.cursor_row -= 1;
                self.cursor_col = self.lines[self.cursor_row].len();
                // A newline is a separator: keep skipping (may land in words above).
                if self.cursor_col == 0 {
                    continue;
                }
            } else {
                return;
            }
        }
        // Step 2: skip the word itself backward to its start.
        loop {
            if self.cursor_col == 0 {
                return;
            }
            let line = &self.lines[self.cursor_row];
            let ch = line[..self.cursor_col].chars().next_back().unwrap_or(' ');
            if !Self::is_word_char(ch) {
                return;
            }
            let prev = line[..self.cursor_col]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.cursor_col = prev;
        }
    }

    /// Move one word right (Emacs Alt+F semantics).
    pub fn move_word_right(&mut self) {
        self.snap_cursor_to_char_boundary();
        let mut jumped_line = false;
        loop {
            let len = self.lines[self.cursor_row].len();
            if self.cursor_col < len {
                let line = &self.lines[self.cursor_row];
                let ch = line[self.cursor_col..].chars().next().unwrap_or(' ');
                let next = self.cursor_col + ch.len_utf8();
                self.cursor_col = next;
                if Self::is_word_char(ch) {
                    // Stop at end of word: next char is non-word or EOL.
                    let after = line[self.cursor_col..].chars().next();
                    if after.is_none_or(|a| !Self::is_word_char(a)) {
                        break;
                    }
                }
            } else if self.cursor_row + 1 < self.lines.len() {
                self.cursor_row += 1;
                self.cursor_col = 0;
                jumped_line = true;
                break;
            } else {
                break;
            }
        }
        if jumped_line {
            return;
        }
        // Skip one run of separators so repeated presses advance word by word.
        while let Some(ch) = self.lines[self.cursor_row][self.cursor_col..]
            .chars()
            .next()
            .filter(|c| !Self::is_word_char(*c))
        {
            self.cursor_col += ch.len_utf8();
            if let Some(next) = self.lines[self.cursor_row][self.cursor_col..].chars().next() {
                if Self::is_word_char(next) {
                    break;
                }
            } else {
                break;
            }
        }
    }

    /// Display-cell width of `line[..col]` (byte col -> terminal cells).
    pub fn visual_col(&self, row: usize, col: usize) -> usize {
        use unicode_width::UnicodeWidthStr;
        let line = self.lines.get(row).map(String::as_str).unwrap_or("");
        let col = col.min(line.len());
        UnicodeWidthStr::width(&line[..col])
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn set_text(&mut self, text: &str) {
        self.push_undo();
        self.lines = text.split('\n').map(|s| s.to_string()).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.scroll_offset = 0;
        self.col_offset = 0;
    }

    pub fn insert_char(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        self.push_undo();
        self.snap_cursor_to_char_boundary();
        let line = &mut self.lines[self.cursor_row];
        line.insert(self.cursor_col, c);
        self.cursor_col += c.len_utf8();
    }

    pub fn backspace(&mut self) {
        self.push_undo();
        self.snap_cursor_to_char_boundary();
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
        self.push_undo();
        self.snap_cursor_to_char_boundary();
        if self.cursor_col < self.lines[self.cursor_row].len() {
            let line = &mut self.lines[self.cursor_row];
            let rest = &line[self.cursor_col..];
            let next = self.cursor_col + rest.chars().next().map(|c| c.len_utf8()).unwrap_or(0);
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
        self.push_undo();
        self.snap_cursor_to_char_boundary();
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
        self.push_undo();
        self.snap_cursor_to_char_boundary();
        let rest = self.lines[self.cursor_row].split_off(self.cursor_col);
        self.lines.insert(self.cursor_row + 1, rest);
        self.cursor_row += 1;
        self.cursor_col = 0;
    }

    /// Delete the text range from `start` to `end` ((row, byte-col),
    /// `start <= end`) and park the cursor at `start`. Joins the remaining
    /// head/tail lines. Endpoints are clamped and floored to char
    /// boundaries, so a stale drag-selection can never panic.
    pub fn delete_range(&mut self, start: (usize, usize), end: (usize, usize)) {
        // No-op for empty ranges: don't pollute the undo stack.
        let (sr0, er0) = (start.0.min(end.0), start.0.max(end.0));
        if sr0 == er0 {
            let (a, b) = (start.1.min(end.1), start.1.max(end.1));
            if a == b {
                return;
            }
        }
        self.push_undo();
        let snap = |line: &str, mut col: usize| -> usize {
            col = col.min(line.len());
            while col > 0 && !line.is_char_boundary(col) {
                col -= 1;
            }
            col
        };
        let last = self.lines.len().saturating_sub(1);
        let (sr, er) = (start.0.min(last), end.0.min(last));
        if sr > er {
            return;
        }
        let sc = snap(&self.lines[sr], start.1);
        let ec = snap(&self.lines[er], end.1);
        if sr == er {
            let (a, b) = (sc.min(ec), sc.max(ec));
            self.lines[sr].replace_range(a..b, "");
        } else {
            let tail = self.lines[er][ec..].to_string();
            self.lines[sr].truncate(sc);
            self.lines[sr].push_str(&tail);
            self.lines.drain(sr + 1..=er);
            if self.lines.is_empty() {
                self.lines.push(String::new());
            }
        }
        self.cursor_row = sr.min(self.lines.len().saturating_sub(1));
        self.cursor_col = sc.min(self.lines[self.cursor_row].len());
        self.snap_cursor_to_char_boundary();
    }

    pub fn move_left(&mut self) {
        self.snap_cursor_to_char_boundary();
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
        self.snap_cursor_to_char_boundary();
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
        // Floor to the previous char boundary: the cursor may sit inside a
        // multi-byte char after a vertical move (kept byte column) onto a
        // line starting with e.g. a `│` border glyph pasted from a table.
        // (This was a hard crash: `delete`/`backspace` sliced at byte 1 of
        // a 3-byte char.)
        while self.cursor_col > 0
            && !self.lines[self.cursor_row].is_char_boundary(self.cursor_col)
        {
            self.cursor_col -= 1;
        }
    }

    pub fn move_up(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            let len = self.lines[self.cursor_row].len();
            if self.cursor_col > len {
                self.cursor_col = len;
            }
            self.snap_cursor_to_char_boundary();
        }
    }

    pub fn move_down(&mut self) {
        if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            let len = self.lines[self.cursor_row].len();
            if self.cursor_col > len {
                self.cursor_col = len;
            }
            self.snap_cursor_to_char_boundary();
        }
    }

    pub fn ensure_cursor_visible(&mut self, visible_height: usize) {
        self.ensure_cursor_visible_sized(visible_height, usize::MAX);
    }

    /// Keep the cursor inside a `visible_height` x `visible_width` viewport,
    /// adjusting both the vertical scroll and the horizontal display-cell
    /// offset (`col_offset`) so long lines scroll sideways instead of
    /// clipping.
    pub fn ensure_cursor_visible_sized(&mut self, visible_height: usize, visible_width: usize) {
        if visible_height > 0 {
            if self.cursor_row < self.scroll_offset {
                self.scroll_offset = self.cursor_row;
            } else if self.cursor_row >= self.scroll_offset + visible_height {
                self.scroll_offset = self.cursor_row + 1 - visible_height;
            }
        }
        if visible_width == usize::MAX || visible_width == 0 {
            return;
        }
        let vcol = self.visual_col(self.cursor_row, self.cursor_col);
        if vcol < self.col_offset {
            self.col_offset = vcol;
        } else if vcol >= self.col_offset + visible_width {
            self.col_offset = vcol + 1 - visible_width;
        }
    }

    pub fn cursor_char_offset(&self) -> usize {
        let mut off = 0;
        for (i, line) in self.lines.iter().enumerate() {
            if i == self.cursor_row {
                // Floor to a char boundary: the byte column may sit inside
                // a multi-byte char after a vertical move.
                let mut col = self.cursor_col.min(line.len());
                while col > 0 && !line.is_char_boundary(col) {
                    col -= 1;
                }
                off += col;
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
        crate::db::sql_scan::statement_ranges(text)
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
    pub is_connected: bool,
    pub connecting: bool,
    pub connect_error: Option<String>,
    pub query_history: Vec<String>,
    pub history_cursor: usize,
    pub history_scroll: usize,
    pub help_scroll: usize,
    pub results: Vec<QueryRow>,
    pub scroll_offset: usize,
    pub col_scroll_offset: usize,
    /// Rectangular cell selection in the result table (absolute data-row /
    /// column indices). Built by mouse drag in Table view; copied as
    /// header + TSV (no box borders) on release via `selected_table_text`.
    pub table_selection: Option<TableSelection>,
    pub editor: SqlEditor,
    /// Current mouse-drag text selection in the editor (None = no
    /// selection). Copied to the clipboard on mouse release.
    pub editor_selection: Option<EditorSelection>,
    pub mode: SessionMode,
    pub pending_query: bool,
    /// True while a further page of the current result set is being fetched
    /// (keeps the existing rows on screen instead of showing the query
    /// placeholder, and prevents duplicate page requests).
    pub paging: bool,
    pub conn_dialog: ConnectionDialog,
    /// Snapshot of `conn_dialog` taken when `Ctrl+O` opens so `Esc` cancels cleanly.
    pub conn_dialog_backup: Option<ConnectionDialog>,
    /// Endpoint parameters of the currently established connection (password scrubbed).
    pub connected_params: Option<ConnectionParams>,
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
    /// Monotonic connection sequence — bumped for each connect attempt so
    /// stale or post-close background connects are closed and ignored.
    pub conn_seq: u64,
    /// Serializes query execution on this session's connection so two spawned
    /// worker threads never call ODPI-C concurrently on the same connection
    /// (breaking is the only operation allowed from the UI thread).
    pub exec_lock: Arc<Mutex<()>>,
    /// F2 browser: index of the highlighted session row.
    pub session_view_cursor: usize,
    /// F2 browser: when true, only ACTIVE sessions are shown (`a` toggles).
    pub session_view_active_only: bool,
    /// F2 browser: sorted column index (0..SESSION_VIEW_COL_COUNT), None = DB order (`s` cycles).
    pub session_view_sort_col: Option<usize>,
    /// F2 browser: sort direction for `session_view_sort_col` (`S` toggles).
    pub session_view_sort_desc: bool,
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
    /// Vertical scroll offset of the F2 current-SQL pane.
    pub sql_scroll: usize,
    /// Last F2 list failure (e.g. missing V$SESSION privileges), shown in
    /// the browser instead of an unexplained empty table.
    pub session_view_error: Option<String>,
    /// Cached F2 session list (v$session / pg_stat_activity). Refreshed on
    /// demand and automatically every `SessionManager::session_refresh_secs`.
    /// Caching avoids hitting the database on every UI frame / cursor move —
    /// previously `fresh_session_list()` queried on each render (~20fps).
    pub session_list_cache: Vec<DbSessionInfo>,
    /// When `session_list_cache` was last refreshed.
    pub session_list_since: Option<std::time::Instant>,
    /// True while a background F2 list refresh is in flight.
    pub session_list_loading: bool,
    /// Monotonic request id for F2 list loads (stale responses dropped).
    pub session_list_req: u64,
    /// True while a background F2 current-SQL load is in flight.
    pub sql_loading: bool,
    /// Monotonic request id for F2 SQL loads (stale responses dropped).
    pub sql_req: u64,
    /// True while a background F2 EXPLAIN is running.
    pub plan_loading: bool,
    /// Monotonic request id for F2 EXPLAIN loads (stale responses dropped).
    pub plan_req: u64,
    /// F12 DB explorer: cached schema tree (lazy-loaded per level).
    pub explorer_schemas: Vec<ExplorerSchema>,
    /// Whether the top-level schema list was loaded at least once.
    pub explorer_loaded: bool,
    /// Number of in-flight background explorer structure loads
    /// (schemas / groups / objects / columns). The tree renders a
    /// loading indicator while nonzero.
    pub explorer_struct_pending: usize,
    /// Cursor into the flattened visible explorer rows.
    pub explorer_cursor: usize,
    /// Vertical scroll offset of the explorer tree pane.
    pub explorer_scroll: usize,
    /// Scroll of the detail/columns pane.
    pub explorer_detail_scroll: usize,
    /// Horizontal scroll of the detail preview table (first visible column).
    pub explorer_detail_col_scroll: usize,
    /// Substring filter (case-insensitive); empty = no filter.
    pub explorer_filter: String,
    /// Whether the filter input line is capturing keystrokes.
    pub explorer_filtering: bool,
    /// Last explorer load failure (shown in the browser).
    pub explorer_error: Option<String>,
    /// Cached catalog metadata for SQL completion (never queried on the
    /// keystroke path; filled by background workers).
    pub metadata: MetadataStore,
    /// Open completion popup, if any.
    pub completion: Option<CompletionPopup>,
    /// F4 Toad-style Object Describe / DDL target currently requested.
    pub ddl_target: Option<DdlTarget>,
    /// F4 resolved object + generated DDL script.
    pub ddl_object: Option<DescribedObject>,
    /// True while a background worker is resolving/generating F4 DDL.
    pub ddl_loading: bool,
    /// Last F4 lookup/DDL error.
    pub ddl_error: Option<String>,
    /// Vertical line scroll in the F4 DDL viewer.
    pub ddl_scroll: usize,
    /// Horizontal column scroll in the F4 DDL viewer.
    pub ddl_hscroll: usize,
    /// Mode to return to when closing F4 (`Query` or `DbExplorer`).
    pub ddl_return_mode: SessionMode,
    /// Monotonic request id for F4 DDL loads (stale responses dropped).
    pub ddl_req_seq: u64,
    /// F6 Performance dashboard: selected DBA query index.
    pub perf_cursor: usize,
    /// Vertical scroll in the F6 result table.
    pub perf_scroll: usize,
    /// Horizontal column scroll in the F6 result table.
    pub perf_col_scroll: usize,
    /// Vertical scroll in the F6 SQL query preview box.
    pub perf_sql_scroll: usize,
    /// Whether the underlying DBA SQL text pane is visible in F6 (`v` toggles).
    pub perf_show_sql: bool,
    /// Live 5-second `btop`-style auto-refresh in F6 (`a` toggles).
    pub perf_auto_refresh: bool,
    /// When the F6 telemetry/query was last refreshed.
    pub perf_last_refresh: Option<std::time::Instant>,
    /// Latest F6 top-bar health snapshot.
    pub perf_snapshot: Option<PerfSnapshot>,
    /// Error from the latest F6 snapshot attempt, if any.
    pub perf_snapshot_error: Option<String>,
    /// Rolling time-series history for F6 sparklines.
    pub perf_history: PerfHistory,
    /// Cached F6 query results keyed by query index.
    pub perf_results: HashMap<usize, QueryRow>,
    /// True while an F6 background refresh/query is running.
    pub perf_loading: bool,
    /// Shared latest request sequence so superseded rapid F6 cursor moves skip DB work.
    pub perf_latest_seq: Arc<AtomicU64>,
}

/// One schema/user node in the F12 explorer tree.
#[derive(Debug, Clone, Default)]
pub struct ExplorerSchema {
    pub name: String,
    pub expanded: bool,
    pub groups: Option<Vec<ExplorerGroup>>,
    pub load_error: Option<String>,
}

/// One type-folder node (Tables, Views, …) inside a schema. Only folders for
/// types the schema actually has are shown (from a dictionary count query).
#[derive(Debug, Clone)]
pub struct ExplorerGroup {
    /// Stable folder key: TABLES, VIEWS, MVIEWS, FOREIGN, INDEXES,
    /// SEQUENCES, FUNCTIONS, PROCEDURES, PACKAGES, TRIGGERS, TYPES.
    pub key: String,
    /// Dictionary object count (may exceed the loaded list when capped).
    pub count: u64,
    pub expanded: bool,
    pub objects: Option<Vec<ExplorerTable>>,
    pub load_error: Option<String>,
}

impl ExplorerGroup {
    fn new(key: String, count: u64) -> Self {
        Self {
            key,
            count,
            expanded: false,
            objects: None,
            load_error: None,
        }
    }

    pub fn label(&self) -> &'static str {
        explorer_group_label(&self.key)
    }
}

/// Display label for a type-folder key.
pub fn explorer_group_label(key: &str) -> &'static str {
    match key {
        "TABLES" => "Tables",
        "VIEWS" => "Views",
        "MVIEWS" => "Materialized Views",
        "FOREIGN" => "Foreign Tables",
        "INDEXES" => "Indexes",
        "SEQUENCES" => "Sequences",
        "FUNCTIONS" => "Functions",
        "PROCEDURES" => "Procedures",
        "PACKAGES" => "Packages",
        "TRIGGERS" => "Triggers",
        "TYPES" => "Types",
        _ => "Objects",
    }
}

/// Fixed folder order inside a schema (DataGrip-style).
fn explorer_group_order(key: &str) -> usize {
    match key {
        "TABLES" => 0,
        "VIEWS" => 1,
        "MVIEWS" => 2,
        "FOREIGN" => 3,
        "INDEXES" => 4,
        "SEQUENCES" => 5,
        "FUNCTIONS" => 6,
        "PROCEDURES" => 7,
        "PACKAGES" => 8,
        "TRIGGERS" => 9,
        "TYPES" => 10,
        _ => 99,
    }
}

/// One table/view/code node in the F12 explorer tree.
#[derive(Debug, Clone)]
pub struct ExplorerTable {
    pub name: String,
    /// "TABLE", "VIEW", or a code kind ("PROCEDURE", "FUNCTION", ...).
    pub kind: String,
    pub expanded: bool,
    pub columns: Option<Vec<ColumnInfo>>,
    pub load_error: Option<String>,
    /// Async 20-row preview for TABLE/VIEW (loaded on selection).
    pub preview: Option<QueryRow>,
    pub preview_loading: bool,
    /// Monotonic id for the latest preview request (first page or page-N), so
    /// a response from an older request can be dropped after a reload.
    pub preview_req: u64,
    /// Failure from loading a further preview page (kept separate from the
    /// already loaded rows so the table stays visible).
    pub preview_error: Option<String>,
    /// Async DDL/source for code objects (TABLE/VIEW synthesize on `d`).
    pub ddl: Option<String>,
    pub ddl_loading: bool,
    pub ddl_error: Option<String>,
}

impl ExplorerTable {
    fn new(name: String, kind: String) -> Self {
        Self {
            name,
            kind,
            expanded: false,
            columns: None,
            load_error: None,
            preview: None,
            preview_loading: false,
            preview_req: 0,
            preview_error: None,
            ddl: None,
            ddl_loading: false,
            ddl_error: None,
        }
    }

    /// Row-bearing objects get a 20-row preview (tables, views,
    /// materialized/foreign tables, sequences).
    pub fn has_preview(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "TABLE" | "VIEW" | "MVIEW" | "FOREIGN TABLE" | "SEQUENCE"
        )
    }

    /// Definition-bearing objects show DDL/source (code, indexes, sequences).
    pub fn has_ddl(&self) -> bool {
        !matches!(
            self.kind.as_str(),
            "TABLE" | "VIEW" | "MVIEW" | "FOREIGN TABLE"
        )
    }

    /// Expandable in the tree (column list underneath).
    pub fn is_expandable(&self) -> bool {
        !matches!(
            self.kind.as_str(),
            "PROCEDURE" | "FUNCTION" | "PACKAGE" | "PACKAGE BODY" | "TRIGGER" | "TYPE"
        )
    }
}

/// Background explorer results (20-row previews + DDL), keyed by session id
/// and fully-qualified object name so stale selections are ignored safely.
#[derive(Debug)]
pub enum ExplorerMsg {
    Preview {
        sid: usize,
        schema: String,
        table: String,
        kind: String,
        /// Per-object request sequence; stale responses are dropped.
        req: u64,
        result: QueryRow,
    },
    Ddl {
        sid: usize,
        schema: String,
        name: String,
        kind: String,
        result: Result<String, String>,
    },
}

/// One flattened, visible row of the explorer tree (tree order).
#[derive(Debug, Clone)]
pub enum ExplorerRow {
    Schema {
        idx: usize,
    },
    Group {
        sidx: usize,
        gidx: usize,
    },
    Table {
        sidx: usize,
        gidx: usize,
        tidx: usize,
    },
    Column {
        sidx: usize,
        gidx: usize,
        tidx: usize,
        cidx: usize,
    },
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

    /// Snap the cursor to a valid char boundary inside the active field.
    pub fn clamp_cursor(&mut self) {
        if self.is_type_field() {
            self.cursor = 0;
            return;
        }
        let len = self.field().len();
        let mut pos = self.cursor.min(len);
        while pos > 0 && !self.field().is_char_boundary(pos) {
            pos -= 1;
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
        self.clamp_cursor();
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
        let port = self
            .port
            .parse::<u16>()
            .unwrap_or_else(|_| self.db_type.default_port());
        let (service, database) = match self.db_type {
            DbType::Oracle => {
                let service = if self.service.is_empty() {
                    self.database.clone()
                } else {
                    self.service.clone()
                };
                let service = if service.is_empty() {
                    "ORCL".into()
                } else {
                    service
                };
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
    SessionView,
    Help,
    History,
    DbExplorer,
    ObjectDdl,
    Performance,
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

    /// Keep `history_cursor` clamped and inside a `visible_height`-row viewport.
    pub fn ensure_history_cursor_visible(&mut self, visible_height: usize) {
        let total = self.query_history.len();
        if total == 0 {
            self.history_cursor = 0;
            self.history_scroll = 0;
            return;
        }
        self.history_cursor = self.history_cursor.min(total - 1);
        if visible_height == 0 {
            self.history_scroll = self.history_scroll.min(total - 1);
            return;
        }
        if self.history_cursor < self.history_scroll {
            self.history_scroll = self.history_cursor;
        } else if self.history_cursor >= self.history_scroll + visible_height {
            self.history_scroll = self.history_cursor + 1 - visible_height;
        }
        let max_scroll = total.saturating_sub(visible_height);
        self.history_scroll = self.history_scroll.min(max_scroll);
    }

    /// If an editor drag-selection exists, delete its text and park the
    /// cursor at its start. Returns true when something was deleted.
    /// Used for type/paste-replaces-selection and Backspace/Delete.
    pub fn delete_selection(&mut self) -> bool {
        let sel = match self.editor_selection.take() {
            Some(s) if !s.is_empty() => s,
            _ => return false,
        };
        let ((sr, sc), (er, ec)) = sel.ordered();
        self.editor.delete_range((sr, sc), (er, ec));
        true
    }

    /// Header + data rows of the current table selection (owned, clamped
    /// to the data). The basis for every copy format: TSV keeps them raw,
    /// Markdown/ASCII go through their (escaping, padding) renderers.
    pub fn selected_table_cells(&self) -> Option<(Vec<String>, Vec<Vec<String>>)> {
        let sel = self.table_selection?;
        let qr = self.results.last()?;
        if qr.is_error || qr.columns.is_empty() {
            return None;
        }
        let ((r0, c0), (r1, c1)) = sel.clamp(qr.rows.len(), qr.columns.len())?;
        let header = qr.columns[c0..=c1].to_vec();
        let mut rows = Vec::new();
        if let Some(e) = r1 {
            let s = r0.unwrap_or(0);
            for row in &qr.rows[s..=e] {
                // Ragged-safe: missing cells copy as empty.
                rows.push(
                    (c0..=c1)
                        .map(|c| row.get(c).cloned().unwrap_or_default())
                        .collect(),
                );
            }
        }
        Some((header, rows))
    }

    /// Header + TSV text for the current table cell selection (selected
    /// columns, selected rows), or None when there is no non-empty
    /// selection. Selecting the header row copies just the column names;
    /// otherwise the header of the selected columns plus the data rows.
    /// Raw cell values — just columns and content, no box
    /// borders — like the Table branch of Ctrl+Y.
    pub fn selected_table_text(&self) -> Option<String> {
        let (header, rows) = self.selected_table_cells()?;
        let mut out = header.join("\t");
        for row in &rows {
            out.push('\n');
            out.push_str(&row.join("\t"));
        }
        Some(out)
    }

    /// Text covered by the current editor drag-selection, if any.
    /// Endpoints are clamped and floored to char boundaries, so a stale
    /// selection (lines edited mid-drag can't happen — edits clear it, but
    /// belt and braces) can never panic.
    pub fn selected_text(&self) -> Option<String> {
        let sel = self.editor_selection?;
        if self.editor.lines.is_empty() {
            return None;
        }
        let ((sr, sc), (er, ec)) = sel.ordered();
        if sr == er && sc == ec {
            return None;
        }
        let snap = |line: &str, mut col: usize| -> usize {
            col = col.min(line.len());
            while col > 0 && !line.is_char_boundary(col) {
                col -= 1;
            }
            col
        };
        let last = self.editor.lines.len() - 1;
        let (sr, er) = (sr.min(last), er.min(last));
        let (sr, er) = (sr.min(er), sr.max(er));
        if sr == er {
            let line = &self.editor.lines[sr];
            let (mut a, mut b) = (snap(line, sc), snap(line, ec));
            if a > b {
                std::mem::swap(&mut a, &mut b);
            }
            return Some(line[a..b].to_string());
        }
        let mut parts = Vec::with_capacity(er - sr + 1);
        for r in sr..=er {
            let line = &self.editor.lines[r];
            let a = if r == sr { snap(line, sc) } else { 0 };
            let b = if r == er { snap(line, ec) } else { line.len() };
            parts.push(line[a.min(b)..b.max(a)].to_string());
        }
        Some(parts.join("\n"))
    }

    fn new(id: usize) -> Self {
        let query_history = load_history();
        let history_cursor = query_history.len().saturating_sub(1);
        Self {
            id,
            name: format!("Session {}", id),
            is_connected: false,
            connecting: false,
            connect_error: None,
            query_history,
            history_cursor,
            history_scroll: 0,
            help_scroll: 0,
            results: Vec::new(),
            scroll_offset: 0,
            col_scroll_offset: 0,
            table_selection: None,
            editor: SqlEditor::new(),
            editor_selection: None,
            mode: SessionMode::Query,
            pending_query: false,
            paging: false,
            conn_dialog: ConnectionDialog::default(),
            conn_dialog_backup: None,
            connected_params: None,
            last_sql: None,
            page_size: 100,
            status_message: None,
            status_message_since: None,
            status_message_ttl: std::time::Duration::from_secs(5),
            job_seq: 0,
            conn_seq: 0,
            exec_lock: Arc::new(Mutex::new(())),
            session_view_cursor: 0,
            session_view_active_only: false,
            session_view_sort_col: None,
            session_view_sort_desc: false,
            plan_sql_text: None,
            plan_for: None,
            plan_result: None,
            plan_scroll: 0,
            plan_hscroll: 0,
            sql_scroll: 0,
            session_view_error: None,
            session_list_cache: Vec::new(),
            session_list_since: None,
            session_list_loading: false,
            session_list_req: 0,
            sql_loading: false,
            sql_req: 0,
            plan_loading: false,
            plan_req: 0,
            explorer_schemas: Vec::new(),
            explorer_loaded: false,
            explorer_struct_pending: 0,
            explorer_cursor: 0,
            explorer_scroll: 0,
            explorer_detail_scroll: 0,
            explorer_detail_col_scroll: 0,
            explorer_filter: String::new(),
            explorer_filtering: false,
            explorer_error: None,
            metadata: MetadataStore::default(),
            completion: None,
            ddl_target: None,
            ddl_object: None,
            ddl_loading: false,
            ddl_error: None,
            ddl_scroll: 0,
            ddl_hscroll: 0,
            ddl_return_mode: SessionMode::Query,
            ddl_req_seq: 0,
            perf_cursor: 0,
            perf_scroll: 0,
            perf_col_scroll: 0,
            perf_sql_scroll: 0,
            perf_show_sql: false,
            perf_auto_refresh: false,
            perf_last_refresh: None,
            perf_snapshot: None,
            perf_snapshot_error: None,
            perf_history: PerfHistory::default(),
            perf_results: HashMap::new(),
            perf_loading: false,
            perf_latest_seq: Arc::new(AtomicU64::new(0)),
        }
    }
}

/// Background F2 session-list refresh result.
#[derive(Debug)]
pub struct F2ListMsg {
    pub sid: usize,
    pub req: u64,
    pub result: Result<Vec<DbSessionInfo>, String>,
}

/// Background F2 current-SQL load result.
#[derive(Debug)]
pub struct F2SqlMsg {
    pub sid: usize,
    pub req: u64,
    pub label: String,
    pub result: Result<String, String>,
}

/// Background F2 EXPLAIN result.
#[derive(Debug)]
pub struct F2PlanMsg {
    pub sid: usize,
    pub req: u64,
    pub result: QueryRow,
}

/// Background F12 explorer structure result (schemas / groups / objects /
/// columns). Applied by name so concurrent expansions can't clobber each other.
#[derive(Debug)]
pub enum CatMsg {
    Schemas {
        sid: usize,
        result: Result<Vec<String>, String>,
    },
    Groups {
        sid: usize,
        schema: String,
        result: Result<Vec<(String, u64)>, String>,
    },
    Objects {
        sid: usize,
        schema: String,
        group: String,
        result: Result<Vec<(String, String)>, String>,
    },
    Columns {
        sid: usize,
        schema: String,
        name: String,
        kind: String,
        result: Result<Vec<ColumnInfo>, String>,
    },
}

/// Background F4 Object Describe / DDL result.
#[derive(Debug)]
pub struct DdlMsg {
    pub sid: usize,
    pub req: u64,
    pub result: Result<DescribedObject, String>,
}

/// Background F6 Performance snapshot + DBA query result.
#[derive(Debug)]
pub struct PerfMsg {
    pub sid: usize,
    pub req: u64,
    pub query_idx: usize,
    pub snapshot: Option<Result<PerfSnapshot, String>>,
    pub query_result: Option<QueryRow>,
}

/// `(session_id, job_seq, result_row, is_final_for_job)`
type ResultMsg = (usize, u64, QueryRow, bool);
/// `(session_id, conn_seq, conn_or_error, display_name, sanitized_params)`
type ConnResultMsg = (
    usize,
    u64,
    Result<Arc<dyn DbConnection>, String>,
    String,
    ConnectionParams,
);

pub struct SessionManager {
    pub sessions: Vec<Session>,
    pub active_idx: usize,
    next_id: AtomicUsize,
    result_rx: Receiver<ResultMsg>,
    result_tx: Sender<ResultMsg>,
    conn_result_rx: Receiver<ConnResultMsg>,
    conn_result_tx: Sender<ConnResultMsg>,
    explorer_rx: Receiver<ExplorerMsg>,
    explorer_tx: Sender<ExplorerMsg>,
    meta_rx: Receiver<MetaMsg>,
    meta_tx: Sender<MetaMsg>,
    ddl_rx: Receiver<DdlMsg>,
    ddl_tx: Sender<DdlMsg>,
    perf_rx: Receiver<PerfMsg>,
    perf_tx: Sender<PerfMsg>,
    f2list_rx: Receiver<F2ListMsg>,
    f2list_tx: Sender<F2ListMsg>,
    f2sql_rx: Receiver<F2SqlMsg>,
    f2sql_tx: Sender<F2SqlMsg>,
    f2plan_rx: Receiver<F2PlanMsg>,
    f2plan_tx: Sender<F2PlanMsg>,
    cat_rx: Receiver<CatMsg>,
    cat_tx: Sender<CatMsg>,
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
    /// F2 session browser auto-refresh interval, seconds (default 60, 0 = manual only).
    pub session_refresh_secs: u64,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Map an explorer load failure to a displayable message. Missing dictionary
/// privileges (the usual cause) get a GRANT hint, like the F2 browser does.
fn explorer_error_hint(msg: &str) -> String {
    if msg.contains("ORA-00942") || msg.contains("ORA-01031") {
        format!(
            "{}\nHint: the explorer needs dictionary access. Ask your DBA for e.g.:\n  \
             GRANT SELECT_CATALOG_ROLE TO <your_user>;",
            msg
        )
    } else {
        msg.to_string()
    }
}

/// Display text of one flattened explorer row (used for filtering).
fn explorer_row_label(sess: &Session, row: &ExplorerRow) -> String {
    match row {
        ExplorerRow::Schema { idx } => sess
            .explorer_schemas
            .get(*idx)
            .map(|s| s.name.clone())
            .unwrap_or_default(),
        ExplorerRow::Group { sidx, gidx } => sess
            .explorer_schemas
            .get(*sidx)
            .and_then(|s| s.groups.as_ref())
            .and_then(|g| g.get(*gidx))
            .map(|g| g.label().to_string())
            .unwrap_or_default(),
        ExplorerRow::Table { sidx, gidx, tidx } => sess
            .explorer_schemas
            .get(*sidx)
            .and_then(|s| s.groups.as_ref())
            .and_then(|g| g.get(*gidx))
            .and_then(|g| g.objects.as_ref())
            .and_then(|o| o.get(*tidx))
            .map(|t| format!("{} {}", t.name, t.kind))
            .unwrap_or_default(),
        ExplorerRow::Column {
            sidx,
            gidx,
            tidx,
            cidx,
        } => sess
            .explorer_schemas
            .get(*sidx)
            .and_then(|s| s.groups.as_ref())
            .and_then(|g| g.get(*gidx))
            .and_then(|g| g.objects.as_ref())
            .and_then(|o| o.get(*tidx))
            .and_then(|t| t.columns.as_ref())
            .and_then(|c| c.get(*cidx))
            .map(|c| format!("{} {}", c.name, c.data_type))
            .unwrap_or_default(),
    }
}

impl SessionManager {
    pub fn new() -> Self {        let (tx, rx) = mpsc::channel();
        let (ctx, crx) = mpsc::channel();
        let (etx, erx) = mpsc::channel();
        let (mtx, mrx) = mpsc::channel();
        let (dtx, drx) = mpsc::channel();
        let (ptx, prx) = mpsc::channel();
        let (f2ltx, f2lrx) = mpsc::channel();
        let (f2stx, f2srx) = mpsc::channel();
        let (f2ptx, f2prx) = mpsc::channel();
        let (cattx, catrx) = mpsc::channel();
        let sessions = vec![Session::new(1)];
        Self {
            sessions,
            active_idx: 0,
            next_id: AtomicUsize::new(2),
            result_rx: rx,
            result_tx: tx,
            conn_result_rx: crx,
            conn_result_tx: ctx,
            explorer_rx: erx,
            explorer_tx: etx,
            meta_rx: mrx,
            meta_tx: mtx,
            ddl_rx: drx,
            ddl_tx: dtx,
            perf_rx: prx,
            perf_tx: ptx,
            f2list_rx: f2lrx,
            f2list_tx: f2ltx,
            f2sql_rx: f2srx,
            f2sql_tx: f2stx,
            f2plan_rx: f2prx,
            f2plan_tx: f2ptx,
            cat_rx: catrx,
            cat_tx: cattx,
            conn_map: Arc::new(Mutex::new(HashMap::new())),
            max_history: 1000,
            max_results_per_session: 200,
            autocommit: true,
            max_rows: 10000,
            null_display: "(NULL)".into(),
            session_refresh_secs: 60,
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
            elapsed_ms: 0,
            is_error: true,
            error_msg: Some(msg),
            rows_affected: None,
            page_offset: 0,
            page_size,
            byte_count: 0,
            total_fetched: 0,
        };
        let _ = result_tx.send((id, seq, err_row, true));
    }

    /// Open a backend connection in a background thread and report back
    /// through the conn_result channel. Ownership stays in the channel until
    /// `poll_conn_result` validates that the session still exists and that
    /// `conn_seq` has not been superseded, preventing leaked or raced handles.
    fn spawn_connect(
        conn_result_tx: Sender<ConnResultMsg>,
        id: usize,
        conn_seq: u64,
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
                DbType::Postgres => PgConnection::connect_with_autocommit(
                    &params.host,
                    params.port,
                    &params.database,
                    &params.user,
                    params.password.as_deref().unwrap_or(""),
                    autocommit,
                    &null_display,
                )
                .map(|c| c as Arc<dyn DbConnection>),
            };
            let outcome = result.map_err(|e| {
                if params.db_type == DbType::Oracle {
                    OracleConnection::friendly_connect_error(&e)
                } else {
                    e.to_string()
                }
            });
            let mut sanitized = params;
            sanitized.password = None;
            if let Err(send_err) =
                conn_result_tx.send((id, conn_seq, outcome, name_str, sanitized))
            {
                //SessionManager dropped before connect finished: close cleanly.
                if let (_, _, Ok(conn), _, _) = send_err.0 {
                    let _ = conn.close();
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
            let sess = &mut self.sessions[new_idx];
            sess.conn_seq = sess.conn_seq.wrapping_add(1);
            let (id, conn_seq) = (sess.id, sess.conn_seq);
            sess.connecting = true;
            sess.connect_error = None;
            Self::spawn_connect(
                self.conn_result_tx.clone(),
                id,
                conn_seq,
                params,
                self.autocommit,
                self.null_display.clone(),
            );
        }

        new_idx
    }

    pub fn remove_session(&mut self, idx: usize) {
        if self.sessions.len() <= 1 || idx >= self.sessions.len() {
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
        let mut map = self.conn_map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(conn) = map.remove(&removed_id) {
            let _ = conn.close();
        }
    }

    pub fn switch_next(&mut self) {
        if self.sessions.len() > 1 {
            self.close_completion();
            self.active_idx = (self.active_idx + 1) % self.sessions.len();
        }
    }

    pub fn switch_prev(&mut self) {
        if self.sessions.len() > 1 {
            self.close_completion();
            self.active_idx = if self.active_idx == 0 {
                self.sessions.len() - 1
            } else {
                self.active_idx - 1
            };
        }
    }

    pub fn switch_to(&mut self, idx: usize) {
        if idx < self.sessions.len() {
            self.close_completion();
            self.active_idx = idx;
        }
    }

    pub fn connect_active(&mut self, params: &ConnectionParams) {
        let sess = &mut self.sessions[self.active_idx];
        sess.conn_seq = sess.conn_seq.wrapping_add(1);
        let (id, conn_seq) = (sess.id, sess.conn_seq);
        sess.connecting = true;
        sess.connect_error = None;
        Self::spawn_connect(
            self.conn_result_tx.clone(),
            id,
            conn_seq,
            params.clone(),
            self.autocommit,
            self.null_display.clone(),
        );
    }

    /// Backend of session `idx`'s live connection if connected, else the
    /// backend selected in its connection dialog.
    pub fn session_db_type(&self, idx: usize) -> DbType {
        let Some(sess) = self.sessions.get(idx) else {
            return DbType::Oracle;
        };
        let map = self.conn_map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(conn) = map.get(&sess.id) {
            return conn.db_type();
        }
        if let Some(ref p) = sess.connected_params {
            if sess.is_connected {
                return p.db_type;
            }
        }
        sess.conn_dialog.db_type
    }

    /// Backend of the active session's connection if connected, else the
    /// backend selected in its connection dialog.
    pub fn active_db_type(&self) -> DbType {
        self.session_db_type(self.active_idx)
    }

    pub fn execute_query(&mut self, sql: &str) {
        // Delegate `@file.sql` directives directly to `execute_script` so
        // `expand_statements` preserves the script's parent directory as the
        // base for nested `@@relative.sql` includes.
        let script_label = parse_file_directive(sql)
            .map(|raw| raw.trim_start_matches("@@").trim().to_string());
        self.execute_script_internal(vec![sql.to_string()], script_label.as_deref());
    }

    pub fn execute_script(&mut self, statements: Vec<String>) {
        self.execute_script_internal(statements, None);
    }

    fn execute_script_internal(&mut self, statements: Vec<String>, script_label: Option<&str>) {
        // Expand any `@file` / nested `@@file` directives before running.
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let statements = match expand_statements(statements, &cwd) {
            Ok(stmts) => stmts,
            Err(err) => {
                self.emit_error(self.active_idx, err);
                return;
            }
        };
        if let Some(label) = script_label {
            self.sessions[self.active_idx].set_status(format!(
                "Running script '{}' ({} statements)",
                label,
                statements.len()
            ));
        }
        if statements.is_empty() {
            return;
        }
        let idx = self.active_idx;
        // Schema-changing statements invalidate the completion cache. Bump the
        // generation now so in-flight metadata results cannot repopulate it.
        if statements.iter().any(|s| metadata::is_ddl(s)) {
            let sess = &mut self.sessions[idx];
            sess.metadata.invalidate();
            sess.completion = None;
        }
        let seq = self.next_job_seq(idx);
        let id = self.sessions[idx].id;
        let conn_map = self.conn_map.clone();
        let result_tx = self.result_tx.clone();
        let page_size = self.sessions[idx].page_size;
        let initial_limit = if self.max_rows > 0 {
            page_size.min(self.max_rows)
        } else {
            page_size
        };
        let exec_lock = self.sessions[idx].exec_lock.clone();

        self.sessions[idx].pending_query = true;
        self.sessions[idx].paging = false;

        // Persist every executed statement to disk history + session history
        // Also track last paginatable SELECT/WITH for pagination
        {
            let active_db_type = self.active_db_type();
            let max_history = self.max_history;
            let sess = &mut self.sessions[self.active_idx];
            for sql in &statements {
                let trimmed = sql.trim().to_string();
                if !trimmed.is_empty() {
                    append_history(&trimmed);
                    sess.query_history.push(trimmed.clone());
                    if crate::db::connection::is_paginatable_query_for(active_db_type, &trimmed) {
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
            let _guard = exec_lock.lock().unwrap_or_else(|e| e.into_inner());
            let conn_opt = conn_map
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&id)
                .cloned();
            if let Some(conn) = conn_opt {
                for (n, sql) in statements.iter().enumerate() {
                    let result = conn.execute_query(sql, initial_limit);
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

                    // Abort the script on cancellation or the first real error.
                    let abort = is_error || was_cancelled;
                    let done = abort || n + 1 == statements.len();
                    let _ = result_tx.send((id, seq, result, done));
                    if abort {
                        break;
                    }
                }
            } else {
                let err = QueryRow {
                    columns: vec![],
                    rows: vec![],
                    truncated: false,
                    elapsed_ms: 0,
                    is_error: true,
                    error_msg: Some("Not connected. Use Ctrl+O to connect.".into()),
                    rows_affected: None,
                    page_offset: 0,
                    page_size,
                    byte_count: 0,
                    total_fetched: 0,
                };
                let _ = result_tx.send((id, seq, err, true));
            }
        });
    }

    /// Fetch the next page of results for the last SELECT query.
    /// New rows are appended to the last QueryRow in session.results.
    pub fn fetch_next_page(&mut self) {
        let idx = self.active_idx;
        // Never overlap a page fetch with another page or a running query.
        if self.sessions[idx].pending_query || self.sessions[idx].paging {
            return;
        }
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

        self.sessions[idx].paging = true;

        thread::spawn(move || {
            let _guard = exec_lock.lock().unwrap_or_else(|e| e.into_inner());
            let conn_opt = conn_map
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&id)
                .cloned();
            match conn_opt {
                Some(conn) => {
                    let result = conn.execute_query_paged(&sql, current_offset, allowed);
                    let _ = result_tx.send((id, seq, result, true));
                }
                None => {
                    let err = QueryRow::error(
                        "Not connected. Use Ctrl+O to connect.".into(),
                        0,
                        current_offset,
                        allowed,
                    );
                    let _ = result_tx.send((id, seq, err, true));
                }
            }
        });
    }

    /// Auto-paging hook: when the user scrolls to the end of the loaded rows,
    /// fetch the next page in the background. Keeps the current rows visible
    /// (unlike a fresh query, which shows the execution placeholder).
    pub fn maybe_fetch_next_page(&mut self) {
        let idx = self.active_idx;
        let sess = &self.sessions[idx];
        if sess.pending_query || sess.paging {
            return;
        }
        let Some(last) = sess.results.last() else {
            return;
        };
        if last.is_error || !last.truncated {
            return;
        }
        // Prefetch a few rows before the very end for a smooth scroll.
        let trigger = last.rows.len().saturating_sub(10);
        if sess.scroll_offset < trigger {
            return;
        }
        self.fetch_next_page();
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

        if !self.sessions[self.active_idx].pending_query
            && !self.sessions[self.active_idx].paging
            && !self.sessions[self.active_idx].ddl_loading
            && !self.sessions[self.active_idx].perf_loading
        {
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
        while let Ok((id, seq, outcome, name, connected_params)) = self.conn_result_rx.try_recv() {
            let Some(idx) = self.sessions.iter().position(|s| s.id == id) else {
                // Session tab was closed while connecting: close the handle so
                // the DB session does not leak.
                if let Ok(conn) = outcome {
                    let _ = conn.close();
                }
                continue;
            };
            if self.sessions[idx].conn_seq != seq {
                // Superseded by a newer connect attempt on this session.
                if let Ok(conn) = outcome {
                    let _ = conn.close();
                }
                continue;
            };
            match outcome {
                Ok(conn) => {
                    let dialect = conn.db_type();
                    let old_conn = self
                        .conn_map
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(id, conn);
                    if let Some(old) = old_conn {
                        let _ = old.close();
                    }
                    let sess = &mut self.sessions[idx];
                    sess.connecting = false;
                    sess.is_connected = true;
                    sess.connect_error = None;
                    sess.name = name;
                    sess.connected_params = Some(connected_params);
                    // Fresh connection: drop any metadata/explorer/perf state from a previous one.
                    sess.metadata = MetadataStore::default();
                    sess.metadata.set_dialect(dialect);
                    sess.completion = None;
                    sess.explorer_loaded = false;
                    sess.explorer_schemas.clear();
                    sess.ddl_object = None;
                    sess.ddl_error = None;
                    sess.ddl_loading = false;
                    sess.perf_cursor = 0;
                    sess.perf_results.clear();
                    sess.perf_snapshot = None;
                    sess.perf_snapshot_error = None;
                    sess.perf_history = PerfHistory::default();
                    sess.perf_loading = false;
                    sess.perf_last_refresh = None;
                    self.request_bootstrap(idx);
                }
                Err(err) => {
                    let sess = &mut self.sessions[idx];
                    sess.connecting = false;
                    sess.connect_error = Some(err);
                }
            }
        }
    }

    pub fn poll_result(&mut self) {
        let cap = self.max_results_per_session;
        for _ in 0..32 {
            let Ok((session_id, seq, qr, done)) = self.result_rx.try_recv() else {
                break;
            };
            if let Some(sess) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                // Ignore results from an outdated job (superseded by a newer
                // execution on the same session) — prevents interleaving/stale data.
                if seq < sess.job_seq {
                    continue;
                }
                if done {
                    sess.pending_query = false;
                    sess.paging = false;
                }

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
                    sess.pending_query = false;
                    sess.paging = false;
                    sess.set_status("Query cancelled");
                    continue;
                }

                if qr.page_offset > 0 {
                    // Pagination continuation — append rows to the last result
                    if let Some(last) = sess.results.last_mut() {
                        if !qr.is_error {
                            last.rows.extend(qr.rows);
                            last.truncated = qr.truncated;
                            last.elapsed_ms += qr.elapsed_ms;
                            // Keep the viewport where it is: the user is
                            // usually at the bottom when auto-paging fires,
                            // and jumping to the absolute last row would skip
                            // the freshly loaded rows.
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
                    sess.scroll_offset = 0;
                    sess.col_scroll_offset = 0;
                    sess.table_selection = None;
                }

                // Enforce the per-session results cap (drop oldest pages).
                if sess.results.len() > cap {
                    let excess = sess.results.len() - cap;
                    sess.results.drain(0..excess);
                }
            }
        }
    }

    // -------------------------------------------------------------------
    // SQL completion (see docs/sql-completion.md)
    // -------------------------------------------------------------------

    /// Build a completion context for a session's editor at its cursor.
    fn completion_context(&self, idx: usize) -> (CompletionContext, usize, usize) {
        let ed = &self.sessions[idx].editor;
        let dialect = self.session_db_type(idx);
        let text = ed.text();
        let ctx = completion::analyze(&text, ed.cursor_char_offset(), dialect);
        (ctx, ed.cursor_row, ed.cursor_col)
    }

    /// Ctrl+Space: analyze the statement and open the popup with whatever is
    /// already cached. Missing metadata is requested in the background; the
    /// keystroke path never touches the database.
    pub fn open_completion(&mut self) {
        let idx = self.active_idx;
        if !self.sessions[idx].is_connected {
            self.sessions[idx].set_status("Completion needs a connection (Ctrl+O)");
            return;
        }
        let (ctx, row, col) = self.completion_context(idx);
        if ctx.in_literal {
            self.sessions[idx].set_status("No completion inside a string or comment");
            return;
        }
        self.sessions[idx].metadata.set_dialect(ctx.dialect);
        let items = completion::build_items(&ctx, &self.sessions[idx].metadata);
        let loading =
            !self.sessions[idx].metadata.bootstrapped() || self.sessions[idx].metadata.loading();
        let mut popup = CompletionPopup::new(items, ctx.word_start, row, col, loading);
        popup.prefix = ctx.prefix.clone();
        if popup.items.is_empty() && !loading {
            popup.note = Some("No suggestions".into());
        }
        self.sessions[idx].completion = Some(popup);
        self.ensure_completion_metadata(idx, &ctx);
    }

    /// Recompute the open popup after an edit or after metadata arrives.
    pub fn refresh_completion(&mut self) {
        let idx = self.active_idx;
        if self.sessions[idx].completion.is_none() {
            return;
        }
        let (ctx, row, col) = self.completion_context(idx);
        if ctx.in_literal {
            self.sessions[idx].completion = None;
            return;
        }
        let items = completion::build_items(&ctx, &self.sessions[idx].metadata);
        let loading =
            !self.sessions[idx].metadata.bootstrapped() || self.sessions[idx].metadata.loading();
        let popup = self.sessions[idx]
            .completion
            .as_mut()
            .expect("completion checked above");
        if popup.prefix != ctx.prefix {
            popup.selected = 0;
            popup.prefix = ctx.prefix.clone();
        }
        popup.items = items;
        popup.word_start = ctx.word_start;
        popup.cursor_row = row;
        popup.cursor_col = col;
        popup.loading = loading;
        popup.note = if popup.items.is_empty() && !loading {
            Some("No suggestions".into())
        } else {
            None
        };
        popup.selected = popup.selected.min(popup.items.len().saturating_sub(1));
        self.ensure_completion_metadata(idx, &ctx);
    }

    pub fn close_completion(&mut self) {
        self.sessions[self.active_idx].completion = None;
    }

    /// Move the popup highlight (Up/Down/Page*).
    pub fn move_completion(&mut self, delta: isize) {
        if let Some(p) = self.sessions[self.active_idx].completion.as_mut() {
            p.move_selection(delta);
        }
    }

    /// Insert the highlighted suggestion, replacing the word being typed.
    pub fn accept_completion(&mut self) {
        let idx = self.active_idx;
        let Some(popup) = self.sessions[idx].completion.take() else {
            return;
        };
        let Some(item) = popup.items.get(popup.selected) else {
            return;
        };
        let insert = item.insert_text.clone();
        let ed = &mut self.sessions[idx].editor;
        // The popup is closed on any cursor movement, but check the row anyway
        // so a stale popup can never edit the wrong line.
        if ed.cursor_row != popup.cursor_row {
            return;
        }
        let col = ed.cursor_col.min(ed.lines[ed.cursor_row].len());
        let start = popup.word_start.min(col);
        let line = &mut ed.lines[ed.cursor_row];
        if !line.is_char_boundary(start) || !line.is_char_boundary(col) {
            return;
        }
        line.replace_range(start..col, &insert);
        ed.cursor_col = start + insert.len();
    }

    /// Connection for a session index (metadata workers share it with user
    /// queries and serialize through the session's `exec_lock`).
    fn connection_for(&self, idx: usize) -> Option<Arc<dyn DbConnection>> {
        let id = self.sessions.get(idx)?.id;
        self.conn_map.lock().ok()?.get(&id).cloned()
    }

    fn request_bootstrap(&mut self, idx: usize) {
        let Some(conn) = self.connection_for(idx) else {
            return;
        };
        let sess = &mut self.sessions[idx];
        if !sess.metadata.pending.insert("*".into()) {
            return;
        }
        let (id, generation, exec_lock) =
            (sess.id, sess.metadata.generation, sess.exec_lock.clone());
        metadata::spawn_meta_job(
            conn,
            exec_lock,
            self.meta_tx.clone(),
            id,
            generation,
            MetaRequest::Bootstrap,
        );
    }

    fn request_objects(&mut self, idx: usize, schemas: Vec<String>) {
        let Some(conn) = self.connection_for(idx) else {
            return;
        };
        let dialect = self.sessions[idx].metadata.dialect;
        let id = self.sessions[idx].id;
        let generation = self.sessions[idx].metadata.generation;
        let exec_lock = self.sessions[idx].exec_lock.clone();
        for schema in schemas {
            let key = format!("o:{}", metadata::norm_ident(dialect, &schema, false));
            if !self.sessions[idx].metadata.pending.insert(key) {
                continue;
            }
            metadata::spawn_meta_job(
                conn.clone(),
                exec_lock.clone(),
                self.meta_tx.clone(),
                id,
                generation,
                MetaRequest::Objects {
                    schema: schema.clone(),
                },
            );
        }
    }

    fn request_columns(&mut self, idx: usize, refs: Vec<TableRef>) {
        if refs.is_empty() {
            return;
        }
        let Some(conn) = self.connection_for(idx) else {
            return;
        };
        let dialect = self.sessions[idx].metadata.dialect;
        let current_schema = self.sessions[idx].metadata.current_schema.clone();
        let id = self.sessions[idx].id;
        let generation = self.sessions[idx].metadata.generation;
        let exec_lock = self.sessions[idx].exec_lock.clone();

        let mut fresh: Vec<TableRef> = Vec::new();
        for r in refs {
            let schema = r.schema.as_deref().unwrap_or(&current_schema);
            let key = format!(
                "c:{}:{}",
                metadata::norm_ident(dialect, schema, false),
                metadata::norm_ident(dialect, &r.name, false)
            );
            if self.sessions[idx].metadata.pending.insert(key) {
                fresh.push(r);
            }
        }
        if fresh.is_empty() {
            return;
        }
        metadata::spawn_meta_job(
            conn,
            exec_lock,
            self.meta_tx.clone(),
            id,
            generation,
            MetaRequest::Columns {
                refs: fresh,
                current_schema,
            },
        );
    }

    /// Request whatever the current context needs, at most once per key while
    /// a request is in flight and never synchronously.
    fn ensure_completion_metadata(&mut self, idx: usize, ctx: &CompletionContext) {
        if !self.sessions[idx].is_connected {
            return;
        }
        if !self.sessions[idx].metadata.can_request() {
            return;
        }

        let mut want_bootstrap = false;
        let mut object_schemas: Vec<String> = Vec::new();
        let mut column_refs: Vec<TableRef> = Vec::new();

        {
            let store = &self.sessions[idx].metadata;
            let current = store.current_schema.clone();

            if !store.bootstrapped() {
                want_bootstrap = true;
            }

            let need_objects = match ctx.parts.len() {
                0 => matches!(ctx.clause, Clause::From | Clause::Join | Clause::Dml),
                1 => {
                    let q = &ctx.parts[0];
                    completion::find_ref(ctx, &q.text, q.quoted).is_none()
                        && store.find_schema(&q.text, q.quoted).is_some()
                }
                _ => false,
            };

            if need_objects {
                let schema = match ctx.parts.first() {
                    Some(q) => store.find_schema(&q.text, q.quoted).map(|s| s.to_string()),
                    None => (!current.is_empty()).then(|| current.clone()),
                };
                if let Some(schema) = schema {
                    if store.objects_need_load(&schema) {
                        object_schemas.push(schema);
                    }
                }
            }

            // Columns of every table in the statement (one batched query).
            for r in ctx.refs.iter().take(20) {
                if ctx
                    .ctes
                    .iter()
                    .any(|c| store.ident_eq(c, &r.name, r.quoted))
                {
                    continue;
                }
                let schema = match r.schema.as_deref() {
                    Some(s) => Some(s),
                    None if !current.is_empty() => Some(current.as_str()),
                    None => None,
                };
                let Some(schema) = schema else {
                    continue;
                };
                if store.columns_need_load(schema, &r.name) {
                    column_refs.push(r.clone());
                }
            }

            // `schema.table.` in a statement with no FROM clause yet.
            if ctx.parts.len() >= 2 {
                let schema_part = &ctx.parts[ctx.parts.len() - 2];
                let table_part = &ctx.parts[ctx.parts.len() - 1];
                if completion::find_ref(ctx, &schema_part.text, schema_part.quoted).is_none() {
                    if let Some(schema) = store.find_schema(&schema_part.text, schema_part.quoted) {
                        if store.columns_need_load(schema, &table_part.text) {
                            column_refs.push(TableRef {
                                schema: Some(schema.to_string()),
                                schema_quoted: schema_part.quoted,
                                name: table_part.text.clone(),
                                quoted: table_part.quoted,
                                alias: None,
                            });
                        }
                    }
                }
            }
        }

        if want_bootstrap {
            self.request_bootstrap(idx);
        }
        if !object_schemas.is_empty() {
            self.request_objects(idx, object_schemas);
        }
        if !column_refs.is_empty() {
            self.request_columns(idx, column_refs);
        }
    }

    /// Apply background metadata results. Stale generations are discarded so
    /// a DDL invalidation can never be overwritten by an older response.
    pub fn poll_metadata(&mut self) {
        let mut touched_active = false;
        while let Ok(msg) = self.meta_rx.try_recv() {
            let Some(idx) = self.sessions.iter().position(|s| s.id == msg.session_id) else {
                continue;
            };
            let sess = &mut self.sessions[idx];
            if msg.generation != sess.metadata.generation {
                continue;
            }
            match msg.kind {
                MetaKind::Bootstrap {
                    current_schema,
                    schemas,
                    objects,
                } => {
                    sess.metadata.pending.remove("*");
                    let mut ok = true;
                    match current_schema {
                        Ok(cs) => sess.metadata.current_schema = cs,
                        Err(e) => {
                            sess.metadata.mark_error(e);
                            ok = false;
                        }
                    }
                    match schemas {
                        Ok(v) => sess.metadata.put_schemas(v),
                        Err(e) => {
                            sess.metadata.mark_error(e);
                            ok = false;
                        }
                    }
                    match objects {
                        Ok((schema, objs)) => sess.metadata.put_objects(&schema, objs),
                        Err(e) => {
                            sess.metadata.mark_error(e);
                            ok = false;
                        }
                    }
                    if ok {
                        sess.metadata.mark_ok();
                    }
                }
                MetaKind::Objects { schema, objects } => {
                    let key = format!("o:{}", sess.metadata.schema_key(&schema));
                    sess.metadata.pending.remove(&key);
                    match objects {
                        Ok(objs) => {
                            sess.metadata.put_objects(&schema, objs);
                            sess.metadata.mark_ok();
                        }
                        Err(e) => sess.metadata.mark_error(e),
                    }
                }
                MetaKind::Columns {
                    requested,
                    found,
                    error,
                } => {
                    for key in &requested {
                        sess.metadata
                            .pending
                            .remove(&format!("c:{}:{}", key.0, key.1));
                    }
                    match error {
                        Some(e) => sess.metadata.mark_error(e),
                        None => {
                            for tc in found {
                                sess.metadata.put_columns(&tc.schema, &tc.table, tc.columns);
                            }
                            // Cache misses as empty so a nonexistent/unauthorized
                            // table is not queried on every keystroke.
                            for key in requested {
                                sess.metadata.mark_columns_loaded(key);
                            }
                            sess.metadata.mark_ok();
                        }
                    }
                }
            }
            if idx == self.active_idx {
                touched_active = true;
            }
        }
        if touched_active {
            self.refresh_completion();
        }
    }

    /// Run a short catalog/session operation on the active session's connection.
    /// Background workers use `active_connection()` + `exec_lock` directly so
    /// the UI thread never blocks; this helper is kept for parity in tests.
    #[allow(dead_code)]
    fn with_active_conn_sync<T>(
        &self,
        op: impl FnOnce(&dyn DbConnection) -> Result<T, String>,
    ) -> Result<T, String> {
        let sess = &self.sessions[self.active_idx];
        if sess.pending_query || sess.paging {
            return Err("Connection busy (query running) — wait or cancel with Esc.".into());
        }
        let id = sess.id;
        let exec_lock = sess.exec_lock.clone();
        let conn = self
            .conn_map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
            .ok_or_else(|| String::from("Not connected. Use Ctrl+O to connect."))?;
        let _guard = exec_lock.lock().unwrap_or_else(|e| e.into_inner());
        op(conn.as_ref())
    }

    /// Query the backend for its session list (background; use
    /// `request_session_list` — this sync variant is kept for tests).
    #[allow(dead_code)]
    pub fn get_session_info(&self) -> Result<Vec<DbSessionInfo>, String> {
        self.with_active_conn_sync(|c| {
            c.query_sessions().map_err(|e| {
                if c.db_type() == DbType::Oracle {
                    OracleConnection::friendly_session_error(&e.to_string())
                } else {
                    e.to_string()
                }
            })
        })
    }

    /// Number of visible columns in the F2 session browser (shared by
    /// Oracle + Postgres layouts, so `s` cycling is stable across backends).
    pub const SESSION_VIEW_COL_COUNT: usize = 8;

    /// Whether a session row counts as ACTIVE (Oracle `ACTIVE` / Postgres
    /// `active`, case-insensitive).
    pub fn session_info_is_active(info: &DbSessionInfo) -> bool {
        info.status.eq_ignore_ascii_case("active")
    }

    /// Short header label for a session-browser sort column (Oracle naming;
    /// Postgres maps to the same positions).
    pub fn session_view_col_label(col: usize) -> &'static str {
        match col {
            0 => "SID",
            1 => "Serial#",
            2 => "User",
            3 => "Status",
            4 => "Machine",
            5 => "Program",
            6 => "SQL_ID",
            7 => "Logon Time",
            _ => "?",
        }
    }

    fn compare_session_info(
        a: &DbSessionInfo,
        b: &DbSessionInfo,
        col: usize,
    ) -> std::cmp::Ordering {
        match col {
            0 => a.sid.cmp(&b.sid),
            1 => a.serial.cmp(&b.serial),
            2 => a
                .username
                .to_lowercase()
                .cmp(&b.username.to_lowercase())
                .then_with(|| a.username.cmp(&b.username)),
            3 => a
                .status
                .to_lowercase()
                .cmp(&b.status.to_lowercase())
                .then_with(|| a.status.cmp(&b.status)),
            4 => a
                .machine
                .to_lowercase()
                .cmp(&b.machine.to_lowercase())
                .then_with(|| a.machine.cmp(&b.machine)),
            5 => a
                .program
                .to_lowercase()
                .cmp(&b.program.to_lowercase())
                .then_with(|| a.program.cmp(&b.program)),
            6 => a
                .sql_id
                .to_lowercase()
                .cmp(&b.sql_id.to_lowercase())
                .then_with(|| a.sql_id.cmp(&b.sql_id)),
            7 => a.logon_time.cmp(&b.logon_time),
            _ => std::cmp::Ordering::Equal,
        }
    }

    /// Apply the active session's F2 filter + sort to a raw list.
    fn apply_session_view(&self, idx: usize, mut list: Vec<DbSessionInfo>) -> Vec<DbSessionInfo> {
        let sess = &self.sessions[idx];
        if sess.session_view_active_only {
            list.retain(Self::session_info_is_active);
        }
        if let Some(col) = sess.session_view_sort_col {
            let desc = sess.session_view_sort_desc;
            list.sort_by(|a, b| {
                let ord = Self::compare_session_info(a, b, col);
                if desc {
                    ord.reverse()
                } else {
                    ord
                }
            });
        }
        list
    }

    /// Clamp the F2 cursor into the visible (filtered/sorted) list.
    fn clamp_session_cursor(&mut self, idx: usize) {
        let visible_len = self.visible_session_list_len(idx);
        let sess = &mut self.sessions[idx];
        if visible_len == 0 {
            sess.session_view_cursor = 0;
        } else {
            sess.session_view_cursor = sess.session_view_cursor.min(visible_len - 1);
        }
    }

    fn visible_session_list_len(&self, idx: usize) -> usize {
        let sess = &self.sessions[idx];
        let mut len = 0;
        for info in &sess.session_list_cache {
            if sess.session_view_active_only && !Self::session_info_is_active(info) {
                continue;
            }
            len += 1;
        }
        len
    }

    /// Toggle the ACTIVE-only filter (`a`). Resets the cursor + reloads SQL.
    pub fn toggle_session_active_filter(&mut self) {
        let idx = self.active_idx;
        {
            let sess = &mut self.sessions[idx];
            sess.session_view_active_only = !sess.session_view_active_only;
            sess.session_view_cursor = 0;
            let msg = if sess.session_view_active_only {
                "Session browser: showing ACTIVE sessions only (a to show all)"
            } else {
                "Session browser: showing all sessions"
            };
            sess.set_status(msg);
        }
        self.clamp_session_cursor(idx);
        self.load_selected_sql();
    }

    /// Cycle the sort column (`s`): col0 → col1 → … → last → unsorted → col0…
    /// Each step sorts ascending; `S` toggles the direction.
    pub fn cycle_session_sort(&mut self) {
        let idx = self.active_idx;
        {
            let sess = &mut self.sessions[idx];
            sess.session_view_sort_col = match sess.session_view_sort_col {
                None => Some(0),
                Some(c) if c + 1 < Self::SESSION_VIEW_COL_COUNT => Some(c + 1),
                Some(_) => None,
            };
            sess.session_view_sort_desc = false;
            sess.session_view_cursor = 0;
            let msg = match sess.session_view_sort_col {
                None => "Session browser: cleared sorting (DB order)".to_string(),
                Some(c) => format!(
                    "Session browser: sorted by {} ▲ (s: next col, S: direction)",
                    Self::session_view_col_label(c)
                ),
            };
            sess.set_status(msg);
        }
        self.clamp_session_cursor(idx);
        self.load_selected_sql();
    }

    /// Toggle sort direction (`S`). No-op message when nothing is sorted.
    pub fn toggle_session_sort_dir(&mut self) {
        let idx = self.active_idx;
        {
            let sess = &mut self.sessions[idx];
            match sess.session_view_sort_col {
                None => {
                    sess.session_view_sort_col = Some(0);
                    sess.session_view_sort_desc = false;
                    sess.session_view_cursor = 0;
                    sess.set_status(format!(
                        "Session browser: sorted by {} ▲ (s: next col, S: direction)",
                        Self::session_view_col_label(0)
                    ));
                }
                Some(c) => {
                    sess.session_view_sort_desc = !sess.session_view_sort_desc;
                    sess.session_view_cursor = 0;
                    let arrow = if sess.session_view_sort_desc {
                        "▼"
                    } else {
                        "▲"
                    };
                    sess.set_status(format!(
                        "Session browser: sorted by {} {} (s: next col, S: direction)",
                        Self::session_view_col_label(c),
                        arrow
                    ));
                }
            }
        }
        self.clamp_session_cursor(idx);
        self.load_selected_sql();
    }

    /// Request an F2 list refresh in the background (never blocks the UI).
    /// Returns the currently cached visible list; the fresh list arrives via
    /// `poll_f2()` and auto-loads the cursor row's SQL. Duplicate requests
    /// while one is in flight are coalesced.
    pub fn refresh_session_list(&mut self) -> Vec<DbSessionInfo> {
        self.request_session_list();
        self.visible_session_list()
    }

    fn request_session_list(&mut self) {
        let idx = self.active_idx;
        let sess_id = self.sessions[idx].id;
        if self.sessions[idx].session_list_loading {
            return;
        }
        let Some(conn) = self.active_connection() else {
            let sess = &mut self.sessions[idx];
            sess.session_view_error = Some("Not connected. Use Ctrl+O to connect.".into());
            sess.session_list_cache.clear();
            sess.session_list_since = Some(std::time::Instant::now());
            sess.session_view_cursor = 0;
            return;
        };
        let exec_lock = self.sessions[idx].exec_lock.clone();
        let req = self.sessions[idx].session_list_req.wrapping_add(1);
        self.sessions[idx].session_list_req = req;
        self.sessions[idx].session_list_loading = true;
        let tx = self.f2list_tx.clone();
        thread::spawn(move || {
            let _guard = exec_lock.lock().unwrap_or_else(|e| e.into_inner());
            let res = conn.query_sessions().map_err(|e| {
                if conn.db_type() == DbType::Oracle {
                    OracleConnection::friendly_session_error(&e.to_string())
                } else {
                    e.to_string()
                }
            });
            let _ = tx.send(F2ListMsg {
                sid: sess_id,
                req,
                result: res,
            });
        });
    }

    /// Cached F2 list (no DB hit). Renderers and cursor movement must use
    /// this — never query the database from a draw call.
    pub fn cached_session_list(&self) -> Vec<DbSessionInfo> {
        self.sessions[self.active_idx].session_list_cache.clone()
    }

    /// Visible F2 list: cache + ACTIVE filter + column sort. Renderers and
    /// cursor movement must use this (never the raw cache) so the highlight
    /// always indexes the rows actually shown.
    pub fn visible_session_list(&self) -> Vec<DbSessionInfo> {
        let idx = self.active_idx;
        let raw = self.sessions[idx].session_list_cache.clone();
        self.apply_session_view(idx, raw)
    }

    /// Seconds since the last F2 list refresh (None = never refreshed).
    pub fn session_list_age_secs(&self) -> Option<u64> {
        self.sessions[self.active_idx]
            .session_list_since
            .map(|t| t.elapsed().as_secs())
    }

    /// Whether the cached F2 list is stale and due for auto-refresh.
    /// `session_refresh_secs == 0` disables auto-refresh (manual `R` only),
    /// except that a never-fetched list still counts as stale.
    pub fn session_list_stale(&self) -> bool {
        let sess = &self.sessions[self.active_idx];
        match sess.session_list_since {
            None => true,
            Some(since) => {
                if self.session_refresh_secs == 0 {
                    false
                } else {
                    since.elapsed() >= std::time::Duration::from_secs(self.session_refresh_secs)
                }
            }
        }
    }

    /// TTL-guarded F2 list: returns the cache immediately, kicking off a
    /// background refresh when stale. Renderers must use
    /// `visible_session_list()` instead to stay side-effect free.
    #[allow(dead_code)]
    pub(crate) fn fresh_session_list(&mut self) -> Vec<DbSessionInfo> {
        if self.session_list_stale() {
            self.request_session_list();
        }
        self.visible_session_list()
    }

    /// Periodic auto-refresh for the F2 browser. Called every frame from the
    /// TUI loop; only requests when the browser is open and the TTL expired.
    /// Never blocks: the refresh runs on a worker thread.
    pub fn tick_session_view(&mut self) {
        if self.sessions[self.active_idx].mode != SessionMode::SessionView {
            return;
        }
        if self.sessions[self.active_idx].pending_query
            || self.sessions[self.active_idx].paging
            || self.sessions[self.active_idx].session_list_loading
        {
            return;
        }
        if self.session_list_stale() {
            self.request_session_list();
        }
    }

    /// Open the F2 session browser: reset selection/plan state, request a
    /// background refresh; the cursor row's SQL loads when the list arrives.
    pub fn enter_session_view(&mut self) {
        let idx = self.active_idx;
        {
            let s = &mut self.sessions[idx];
            s.mode = SessionMode::SessionView;
            s.session_view_cursor = 0;
            s.session_view_active_only = false;
            s.session_view_sort_col = None;
            s.session_view_sort_desc = false;
            s.plan_sql_text = None;
            s.plan_for = None;
            s.plan_result = None;
            s.plan_scroll = 0;
            s.plan_hscroll = 0;
            s.sql_scroll = 0;
            s.session_view_error = None;
            s.sql_loading = false;
            s.plan_loading = false;
        }
        self.request_session_list();
    }

    /// Move the F2 highlight and preload that row's SQL in the background.
    /// Arrow keys never touch the database on the UI thread anymore.
    pub fn move_session_cursor(&mut self, delta: isize) {
        let len = self.visible_session_list().len();
        if len == 0 {
            // No rows yet (list still loading) — let the list arrival load SQL.
            return;
        }
        let idx = self.active_idx;
        let cur = self.sessions[idx].session_view_cursor as isize;
        let next = cur.saturating_add(delta).clamp(0, len as isize - 1) as usize;
        if next != self.sessions[idx].session_view_cursor {
            self.sessions[idx].session_view_cursor = next;
            self.load_selected_sql();
        } else if self.sessions[self.active_idx].plan_sql_text.is_none()
            && !self.sessions[self.active_idx].sql_loading
        {
            self.load_selected_sql();
        }
    }

    /// Load the full SQL text of the highlighted F2 row in the background.
    /// Clears any previous plan; failures surface as a status message when
    /// the worker result arrives. Reads from the visible (filtered/sorted)
    /// list.
    pub fn load_selected_sql(&mut self) {
        let (info, label) = {
            let idx = self.active_idx;
            let cursor = self.sessions[idx].session_view_cursor;
            match self.visible_session_list().get(cursor).cloned() {
                Some(info) => {
                    let label = format!("{} {}", info.sid, info.username);
                    (Some(info), Some(label))
                }
                None => (None, None),
            }
        };
        let Some(info) = info else {
            let idx = self.active_idx;
            let s = &mut self.sessions[idx];
            s.plan_sql_text = None;
            s.plan_result = None;
            s.plan_for = None;
            return;
        };
        let label = label.unwrap_or_default();
        let idx = self.active_idx;
        let Some(conn) = self.active_connection() else {
            let sess = &mut self.sessions[idx];
            sess.plan_sql_text = None;
            sess.plan_result = None;
            sess.plan_for = Some(label);
            sess.set_status("No SQL text: Not connected. Use Ctrl+O to connect.".to_string());
            return;
        };
        let exec_lock = self.sessions[idx].exec_lock.clone();
        let req = self.sessions[idx].sql_req.wrapping_add(1);
        let sid;
        {
            let sess = &mut self.sessions[idx];
            sess.sql_req = req;
            sess.sql_loading = true;
            sess.plan_for = Some(label.clone());
            sess.plan_scroll = 0;
            sess.plan_hscroll = 0;
            sess.sql_scroll = 0;
            sess.plan_result = None;
            sess.plan_sql_text = None;
            sid = sess.id;
        }
        let tx = self.f2sql_tx.clone();
        thread::spawn(move || {
            let _guard = exec_lock.lock().unwrap_or_else(|e| e.into_inner());
            let res = conn.session_sql(&info).map_err(|e| e.to_string());
            let _ = tx.send(F2SqlMsg {
                sid,
                req,
                label,
                result: res,
            });
        });
    }

    /// Run EXPLAIN for the highlighted F2 row's SQL in the background.
    pub fn explain_selected(&mut self) {
        let idx = self.active_idx;
        if self.sessions[idx].plan_sql_text.is_none() {
            if !self.sessions[idx].sql_loading {
                self.load_selected_sql();
                self.sessions[self.active_idx]
                    .set_status("Loading SQL first — press Enter again to EXPLAIN");
            }
            return;
        }
        if self.sessions[idx].plan_loading {
            return;
        }
        let (sql, max_rows) = {
            let sess = &self.sessions[idx];
            match sess.plan_sql_text.clone() {
                Some(t) if !t.trim().is_empty() => (t, sess.page_size),
                _ => return,
            }
        };
        let Some(conn) = self.active_connection() else {
            self.sessions[idx].set_status("Not connected. Use Ctrl+O to connect.");
            return;
        };
        let exec_lock = self.sessions[idx].exec_lock.clone();
        let req = self.sessions[idx].plan_req.wrapping_add(1);
        let sid;
        {
            let sess = &mut self.sessions[idx];
            sess.plan_req = req;
            sess.plan_loading = true;
            sid = sess.id;
        }
        let tx = self.f2plan_tx.clone();
        thread::spawn(move || {
            let _guard = exec_lock.lock().unwrap_or_else(|e| e.into_inner());
            let result = conn.explain_plan(&sql, max_rows);
            let _ = tx.send(F2PlanMsg { sid, req, result });
        });
    }

    /// Drain finished background F2 list / SQL / EXPLAIN results.
    /// Called every frame (like `poll_result`). Stale responses are dropped.
    pub fn poll_f2(&mut self) {
        for _ in 0..16 {
            let msg = match self.f2list_rx.try_recv() {
                Ok(m) => m,
                Err(_) => break,
            };
            let Some(sess) = self.sessions.iter_mut().find(|s| s.id == msg.sid) else {
                continue;
            };
            if sess.session_list_req != msg.req {
                continue;
            }
            sess.session_list_loading = false;
            match msg.result {
                Ok(list) => {
                    sess.session_view_error = None;
                    sess.session_list_cache = list;
                    sess.session_list_since = Some(std::time::Instant::now());
                }
                Err(err) => {
                    sess.session_view_error = Some(err);
                    sess.session_list_cache.clear();
                    sess.session_list_since = Some(std::time::Instant::now());
                    sess.session_view_cursor = 0;
                    continue;
                }
            }
        }
        // Clamp cursor after list updates (needs active session index).
        {
            let idx = self.active_idx;
            self.clamp_session_cursor(idx);
        }
        // If the list just arrived and no SQL is loaded yet, load it.
        let need_sql = {
            let sess = &self.sessions[self.active_idx];
            sess.mode == SessionMode::SessionView
                && !sess.session_list_cache.is_empty()
                && sess.plan_sql_text.is_none()
                && !sess.sql_loading
        };
        if need_sql {
            self.load_selected_sql();
        }
        for _ in 0..16 {
            let msg = match self.f2sql_rx.try_recv() {
                Ok(m) => m,
                Err(_) => break,
            };
            let Some(sess) = self.sessions.iter_mut().find(|s| s.id == msg.sid) else {
                continue;
            };
            if sess.sql_req != msg.req {
                continue;
            }
            sess.sql_loading = false;
            sess.plan_for = Some(msg.label);
            sess.plan_scroll = 0;
            sess.plan_hscroll = 0;
            sess.sql_scroll = 0;
            sess.plan_result = None;
            match msg.result {
                Ok(text) => sess.plan_sql_text = Some(text),
                Err(e) => {
                    sess.plan_sql_text = None;
                    sess.set_status(format!("No SQL text: {}", e));
                }
            }
        }
        for _ in 0..8 {
            let msg = match self.f2plan_rx.try_recv() {
                Ok(m) => m,
                Err(_) => break,
            };
            let Some(sess) = self.sessions.iter_mut().find(|s| s.id == msg.sid) else {
                continue;
            };
            if sess.plan_req != msg.req {
                continue;
            }
            sess.plan_loading = false;
            sess.plan_result = Some(msg.result);
            sess.plan_scroll = 0;
            sess.plan_hscroll = 0;
        }
    }

    // ---------------- F12 DB explorer ----------------

    /// Open the explorer (or re-enter it). Loads the schema list on first
    /// open; later opens reuse the cache until `r` refreshes it.
    pub fn enter_db_explorer(&mut self) {
        let idx = self.active_idx;
        {
            let s = &mut self.sessions[idx];
            s.mode = SessionMode::DbExplorer;
            s.explorer_filtering = false;
        }
        if !self.sessions[idx].explorer_loaded {
            self.refresh_explorer();
        } else {
            self.clamp_explorer_cursor();
        }
    }

    /// (Re)load the top-level schema list in the background (never blocks).
    pub fn refresh_explorer(&mut self) {
        let idx = self.active_idx;
        let Some(conn) = self.active_connection() else {
            let sess = &mut self.sessions[idx];
            sess.explorer_loaded = true;
            sess.explorer_error =
                Some("Not connected. Use Ctrl+O to connect.".into());
            return;
        };
        let exec_lock = self.sessions[idx].exec_lock.clone();
        let sid = self.sessions[idx].id;
        self.sessions[idx].explorer_struct_pending += 1;
        // Mark loaded so the tree renders a loading state instead of empty.
        self.sessions[idx].explorer_loaded = true;
        let tx = self.cat_tx.clone();
        thread::spawn(move || {
            let _guard = exec_lock.lock().unwrap_or_else(|e| e.into_inner());
            let result = conn.list_schemas().map_err(|e| e.to_string());
            let _ = tx.send(CatMsg::Schemas { sid, result });
        });
    }

    /// Refresh the node under the cursor (schema groups, group objects or
    /// object columns/detail), or the whole tree when sitting on nothing.
    pub fn refresh_explorer_node(&mut self) {
        let rows = self.explorer_visible_rows();
        let cur = self.sessions[self.active_idx].explorer_cursor;
        let Some(row) = rows.get(cur).cloned() else {
            self.refresh_explorer();
            return;
        };
        match row {
            ExplorerRow::Schema { idx } => {
                self.sessions[self.active_idx].explorer_schemas[idx].groups = None;
                self.sessions[self.active_idx].explorer_schemas[idx].load_error = None;
                self.sessions[self.active_idx].explorer_schemas[idx].expanded = true;
                self.load_schema_groups(idx);
            }
            ExplorerRow::Group { sidx, gidx } => {
                if let Some(g) = self.sessions[self.active_idx]
                    .explorer_schemas
                    .get_mut(sidx)
                    .and_then(|s| s.groups.as_mut())
                    .and_then(|g| g.get_mut(gidx))
                {
                    g.objects = None;
                    g.load_error = None;
                    g.expanded = true;
                }
                self.load_group_objects(sidx, gidx);
            }
            ExplorerRow::Table { sidx, gidx, tidx } => {
                self.reload_object_detail(sidx, gidx, tidx, true);
            }
            ExplorerRow::Column {
                sidx, gidx, tidx, ..
            } => {
                if let Some(t) = self.object_mut(sidx, gidx, tidx) {
                    t.columns = None;
                    t.load_error = None;
                    t.expanded = true;
                }
                self.load_table_columns(sidx, gidx, tidx);
            }
        }
        self.clamp_explorer_cursor();
    }

    /// Mutable access to one object node.
    fn object_mut(&mut self, sidx: usize, gidx: usize, tidx: usize) -> Option<&mut ExplorerTable> {
        self.sessions[self.active_idx]
            .explorer_schemas
            .get_mut(sidx)?
            .groups
            .as_mut()?
            .get_mut(gidx)?
            .objects
            .as_mut()?
            .get_mut(tidx)
    }

    /// (Re)load structure + detail of one object. `structure` also reloads
    /// the column list (used by `r`); cursor movement only triggers detail.
    fn reload_object_detail(&mut self, sidx: usize, gidx: usize, tidx: usize, structure: bool) {
        let (expandable, has_prev, has_ddl) = match self.sessions[self.active_idx]
            .explorer_schemas
            .get(sidx)
            .and_then(|s| s.groups.as_ref())
            .and_then(|g| g.get(gidx))
            .and_then(|g| g.objects.as_ref())
            .and_then(|o| o.get(tidx))
        {
            Some(t) => (t.is_expandable(), t.has_preview(), t.has_ddl()),
            None => return,
        };
        if structure {
            if let Some(t) = self.object_mut(sidx, gidx, tidx) {
                if expandable {
                    t.columns = None;
                    t.load_error = None;
                    t.expanded = true;
                }
                t.preview = None;
                t.ddl = None;
                t.ddl_error = None;
            }
            if expandable {
                self.load_table_columns(sidx, gidx, tidx);
            }
        }
        if has_prev {
            self.request_preview(sidx, gidx, tidx);
        }
        if has_ddl {
            self.request_ddl(sidx, gidx, tidx);
        }
    }

    /// Load the type-group folders of one schema in the background.
    fn load_schema_groups(&mut self, sidx: usize) {
        let schema = match self.sessions[self.active_idx].explorer_schemas.get(sidx) {
            Some(s) => s.name.clone(),
            None => return,
        };
        let Some(conn) = self.active_connection() else {
            return;
        };
        let exec_lock = self.sessions[self.active_idx].exec_lock.clone();
        let sid = self.sessions[self.active_idx].id;
        self.sessions[self.active_idx].explorer_struct_pending += 1;
        let tx = self.cat_tx.clone();
        thread::spawn(move || {
            let _guard = exec_lock.lock().unwrap_or_else(|e| e.into_inner());
            let result = conn
                .list_object_groups(&schema)
                .map_err(|e| e.to_string());
            let _ = tx.send(CatMsg::Groups {
                sid,
                schema,
                result,
            });
        });
    }

    /// Load the objects of one type-group folder in the background.
    fn load_group_objects(&mut self, sidx: usize, gidx: usize) {
        let (schema, key) = match self.sessions[self.active_idx]
            .explorer_schemas
            .get(sidx)
            .and_then(|s| s.groups.as_ref())
            .and_then(|g| g.get(gidx))
        {
            Some(g) => (
                self.sessions[self.active_idx].explorer_schemas[sidx]
                    .name
                    .clone(),
                g.key.clone(),
            ),
            None => return,
        };
        let Some(conn) = self.active_connection() else {
            return;
        };
        let exec_lock = self.sessions[self.active_idx].exec_lock.clone();
        let sid = self.sessions[self.active_idx].id;
        self.sessions[self.active_idx].explorer_struct_pending += 1;
        let tx = self.cat_tx.clone();
        thread::spawn(move || {
            let _guard = exec_lock.lock().unwrap_or_else(|e| e.into_inner());
            let result = conn
                .list_objects(&schema, &key)
                .map_err(|e| e.to_string());
            let _ = tx.send(CatMsg::Objects {
                sid,
                schema,
                group: key,
                result,
            });
        });
    }

    fn load_table_columns(&mut self, sidx: usize, gidx: usize, tidx: usize) {
        let (schema, name, kind) = match self.sessions[self.active_idx]
            .explorer_schemas
            .get(sidx)
            .and_then(|s| s.groups.as_ref())
            .and_then(|g| g.get(gidx))
            .and_then(|g| g.objects.as_ref())
            .and_then(|o| o.get(tidx))
        {
            Some(t) => (
                self.sessions[self.active_idx].explorer_schemas[sidx]
                    .name
                    .clone(),
                t.name.clone(),
                t.kind.clone(),
            ),
            None => return,
        };
        let Some(conn) = self.active_connection() else {
            return;
        };
        let exec_lock = self.sessions[self.active_idx].exec_lock.clone();
        let sid = self.sessions[self.active_idx].id;
        self.sessions[self.active_idx].explorer_struct_pending += 1;
        let tx = self.cat_tx.clone();
        thread::spawn(move || {
            let _guard = exec_lock.lock().unwrap_or_else(|e| e.into_inner());
            let result = conn
                .list_columns(&schema, &name, &kind)
                .map_err(|e| e.to_string());
            let _ = tx.send(CatMsg::Columns {
                sid,
                schema,
                name,
                kind,
                result,
            });
        });
    }

    /// Drain finished background explorer structure loads. Called every frame.
    /// Results are applied by name so concurrent expansions stay correct.
    pub fn poll_cat(&mut self) {
        for _ in 0..16 {
            let msg = match self.cat_rx.try_recv() {
                Ok(m) => m,
                Err(_) => break,
            };
            match msg {
                CatMsg::Schemas { sid, result } => {
                    let Some(sess) = self.sessions.iter_mut().find(|s| s.id == sid) else {
                        continue;
                    };
                    sess.explorer_struct_pending =
                        sess.explorer_struct_pending.saturating_sub(1);
                    match result {
                        Ok(names) => {
                            let prev = std::mem::take(&mut sess.explorer_schemas);
                            sess.explorer_schemas = names
                                .into_iter()
                                .map(|n| {
                                    let p = prev.iter().find(|p| p.name == n);
                                    ExplorerSchema {
                                        name: n,
                                        expanded: p.map(|p| p.expanded).unwrap_or(false),
                                        groups: p.and_then(|p| p.groups.clone()),
                                        load_error: None,
                                    }
                                })
                                .collect();
                            sess.explorer_loaded = true;
                            sess.explorer_error = None;
                            if sess.explorer_schemas.is_empty() {
                                sess.explorer_error = Some(
                                    "No schemas found (or no dictionary access).".into(),
                                );
                            }
                        }
                        Err(e) => {
                            sess.explorer_loaded = true;
                            sess.explorer_error = Some(explorer_error_hint(&e));
                        }
                    }
                }
                CatMsg::Groups { sid, schema, result } => {
                    let Some(sess) = self.sessions.iter_mut().find(|s| s.id == sid) else {
                        continue;
                    };
                    sess.explorer_struct_pending =
                        sess.explorer_struct_pending.saturating_sub(1);
                    let Some(entry) = sess
                        .explorer_schemas
                        .iter_mut()
                        .find(|s| s.name == schema)
                    else {
                        continue;
                    };
                    match result {
                        Ok(mut groups) => {
                            groups.sort_by(|a, b| {
                                explorer_group_order(&a.0)
                                    .cmp(&explorer_group_order(&b.0))
                                    .then(a.0.cmp(&b.0))
                            });
                            let prev = entry.groups.take().unwrap_or_default();
                            entry.groups = Some(
                                groups
                                    .into_iter()
                                    .map(|(key, count)| {
                                        let mut g = prev
                                            .iter()
                                            .find(|p| p.key == key)
                                            .cloned()
                                            .unwrap_or_else(|| ExplorerGroup::new(key, count));
                                        g.count = count;
                                        g.load_error = None;
                                        g
                                    })
                                    .collect(),
                            );
                            entry.load_error = None;
                            if entry.groups.as_ref().map(|g| g.is_empty()).unwrap_or(true) {
                                entry.load_error =
                                    Some(format!("Schema '{}' has no visible objects.", schema));
                            }
                        }
                        Err(e) => {
                            entry.groups = Some(Vec::new());
                            entry.load_error = Some(explorer_error_hint(&e));
                        }
                    }
                }
                CatMsg::Objects {
                    sid,
                    schema,
                    group,
                    result,
                } => {
                    let Some(sess) = self.sessions.iter_mut().find(|s| s.id == sid) else {
                        continue;
                    };
                    sess.explorer_struct_pending =
                        sess.explorer_struct_pending.saturating_sub(1);
                    let entry = sess
                        .explorer_schemas
                        .iter_mut()
                        .find(|s| s.name == schema)
                        .and_then(|s| s.groups.as_mut())
                        .and_then(|gs| gs.iter_mut().find(|g| g.key == group));
                    let Some(entry) = entry else { continue };
                    match result {
                        Ok(items) => {
                            let prev = entry.objects.take().unwrap_or_default();
                            entry.objects = Some(
                                items
                                    .into_iter()
                                    .take(2500)
                                    .map(|(name, kind)| {
                                        let mut t = prev
                                            .iter()
                                            .find(|p| p.name == name && p.kind == kind)
                                            .cloned()
                                            .unwrap_or_else(|| {
                                                ExplorerTable::new(name, kind)
                                            });
                                        t.load_error = None;
                                        t
                                    })
                                    .collect(),
                            );
                            entry.load_error = None;
                            if entry.objects.as_ref().map(|o| o.is_empty()).unwrap_or(true) {
                                entry.load_error = Some(format!(
                                    "No {} in schema '{}'.",
                                    explorer_group_label(&group).to_lowercase(),
                                    schema
                                ));
                            }
                        }
                        Err(e) => {
                            entry.objects = Some(Vec::new());
                            entry.load_error = Some(explorer_error_hint(&e));
                        }
                    }
                }
                CatMsg::Columns {
                    sid,
                    schema,
                    name,
                    kind: _,
                    result,
                } => {
                    let Some(sess) = self.sessions.iter_mut().find(|s| s.id == sid) else {
                        continue;
                    };
                    sess.explorer_struct_pending =
                        sess.explorer_struct_pending.saturating_sub(1);
                    let entry = sess
                        .explorer_schemas
                        .iter_mut()
                        .find(|s| s.name == schema)
                        .and_then(|s| s.groups.as_mut())
                        .and_then(|gs| {
                            gs.iter_mut().find_map(|g| {
                                g.objects.as_mut()?.iter_mut().find(|o| o.name == name)
                            })
                        });
                    let Some(entry) = entry else { continue };
                    match result {
                        Ok(cols) => {
                            entry.columns = Some(cols);
                            entry.load_error = None;
                        }
                        Err(e) => {
                            entry.columns = Some(Vec::new());
                            entry.load_error = Some(explorer_error_hint(&e));
                        }
                    }
                }
            }
        }
        let idx = self.active_idx;
        self.clamp_explorer_cursor();
        let _ = idx;
    }

    /// Flat visible rows in tree order, honoring expansion + text filter.
    /// Pure (no DB access) so it is safe to call every frame.
    pub fn explorer_visible_rows(&self) -> Vec<ExplorerRow> {
        let sess = &self.sessions[self.active_idx];
        let mut rows = Vec::new();
        for (sidx, schema) in sess.explorer_schemas.iter().enumerate() {
            rows.push(ExplorerRow::Schema { idx: sidx });
            if !schema.expanded {
                continue;
            }
            if let Some(groups) = schema.groups.as_ref() {
                for (gidx, group) in groups.iter().enumerate() {
                    rows.push(ExplorerRow::Group { sidx, gidx });
                    if !group.expanded {
                        continue;
                    }
                    if let Some(objects) = group.objects.as_ref() {
                        for (tidx, table) in objects.iter().enumerate() {
                            rows.push(ExplorerRow::Table { sidx, gidx, tidx });
                            if !table.expanded {
                                continue;
                            }
                            if let Some(cols) = table.columns.as_ref() {
                                for cidx in 0..cols.len() {
                                    rows.push(ExplorerRow::Column {
                                        sidx,
                                        gidx,
                                        tidx,
                                        cidx,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
        let filter = sess.explorer_filter.trim().to_lowercase();
        if filter.is_empty() {
            return rows;
        }
        // Keep matching rows plus the parents needed for tree context.
        let mut keep = vec![false; rows.len()];
        for (i, row) in rows.iter().enumerate() {
            if explorer_row_label(sess, row)
                .to_lowercase()
                .contains(&filter)
            {
                keep[i] = true;
                // Walk up to the root, keeping every ancestor.
                let mut j = i;
                while j > 0 {
                    j -= 1;
                    let parent = match (&rows[j], row) {
                        (ExplorerRow::Schema { idx }, ExplorerRow::Group { sidx, .. })
                            if idx == sidx =>
                        {
                            true
                        }
                        (ExplorerRow::Schema { idx }, ExplorerRow::Table { sidx, .. })
                            if idx == sidx =>
                        {
                            true
                        }
                        (ExplorerRow::Schema { idx }, ExplorerRow::Column { sidx, .. })
                            if idx == sidx =>
                        {
                            true
                        }
                        (
                            ExplorerRow::Group { sidx: ps, gidx: pg },
                            ExplorerRow::Table { sidx, gidx, .. },
                        ) if ps == sidx && pg == gidx => true,
                        (
                            ExplorerRow::Group { sidx: ps, gidx: pg },
                            ExplorerRow::Column { sidx, gidx, .. },
                        ) if ps == sidx && pg == gidx => true,
                        (
                            ExplorerRow::Table {
                                sidx: ps,
                                gidx: pg,
                                tidx: pt,
                            },
                            ExplorerRow::Column {
                                sidx, gidx, tidx, ..
                            },
                        ) if ps == sidx && pg == gidx && pt == tidx => true,
                        _ => false,
                    };
                    if parent {
                        keep[j] = true;
                    }
                }
            }
        }
        rows.into_iter()
            .enumerate()
            .filter(|(i, _)| keep[*i])
            .map(|(_, r)| r)
            .collect()
    }

    fn clamp_explorer_cursor(&mut self) {
        let len = self.explorer_visible_rows().len();
        let sess = &mut self.sessions[self.active_idx];
        if len == 0 {
            sess.explorer_cursor = 0;
            sess.explorer_scroll = 0;
            return;
        }
        sess.explorer_cursor = sess.explorer_cursor.min(len - 1);
    }

    /// Search label for one explorer row (same text as the filter uses).
    pub fn explorer_row_search_label(&self, row: &ExplorerRow) -> String {
        let sess = &self.sessions[self.active_idx];
        explorer_row_label(sess, row)
    }

    /// Move the highlight; auto-scrolls handled at render time.
    pub fn move_explorer_cursor(&mut self, delta: isize) {        let len = self.explorer_visible_rows().len() as isize;
        if len == 0 {
            return;
        }
        let cur = self.sessions[self.active_idx].explorer_cursor as isize;
        let next = cur.saturating_add(delta).clamp(0, len - 1) as usize;
        if next == self.sessions[self.active_idx].explorer_cursor {
            return;
        }
        self.sessions[self.active_idx].explorer_cursor = next;
        self.sessions[self.active_idx].explorer_detail_scroll = 0;
        self.sessions[self.active_idx].explorer_detail_col_scroll = 0;
        self.maybe_load_detail_for_cursor();
    }

    /// Shared read access to one object node.
    fn object_ref(&self, sidx: usize, gidx: usize, tidx: usize) -> Option<&ExplorerTable> {
        self.sessions[self.active_idx]
            .explorer_schemas
            .get(sidx)?
            .groups
            .as_ref()?
            .get(gidx)?
            .objects
            .as_ref()?
            .get(tidx)
    }

    /// Expand/collapse the row under the cursor, lazy-loading on first open.
    /// Non-expandable leaves (procedures/…) load their DDL instead.
    /// Returns true when the cursor sits on an expandable node.
    pub fn toggle_explorer_cursor(&mut self) -> bool {
        let rows = self.explorer_visible_rows();
        let cur = self.sessions[self.active_idx].explorer_cursor;
        let Some(row) = rows.get(cur).cloned() else {
            return false;
        };
        // Non-expandable leaves never expand — load their DDL instead.
        if let ExplorerRow::Table { sidx, gidx, tidx } = &row {
            let expandable = self
                .object_ref(*sidx, *gidx, *tidx)
                .map(|t| t.is_expandable())
                .unwrap_or(false);
            if !expandable {
                self.maybe_load_detail_for_cursor();
                return false;
            }
        }
        match row {
            ExplorerRow::Schema { idx } => {
                let expanded = self.sessions[self.active_idx].explorer_schemas[idx].expanded;
                if expanded {
                    self.sessions[self.active_idx].explorer_schemas[idx].expanded = false;
                } else {
                    let needs_load = self.sessions[self.active_idx].explorer_schemas[idx]
                        .groups
                        .is_none();
                    self.sessions[self.active_idx].explorer_schemas[idx].expanded = true;
                    if needs_load {
                        self.load_schema_groups(idx);
                    }
                }
                self.clamp_explorer_cursor();
                true
            }
            ExplorerRow::Group { sidx, gidx } => {
                let expanded = self.sessions[self.active_idx].explorer_schemas[sidx]
                    .groups
                    .as_ref()
                    .and_then(|g| g.get(gidx))
                    .map(|g| g.expanded)
                    .unwrap_or(false);
                if expanded {
                    if let Some(g) = self.sessions[self.active_idx].explorer_schemas[sidx]
                        .groups
                        .as_mut()
                        .and_then(|g| g.get_mut(gidx))
                    {
                        g.expanded = false;
                    }
                } else {
                    let needs_load = self.sessions[self.active_idx].explorer_schemas[sidx]
                        .groups
                        .as_ref()
                        .and_then(|g| g.get(gidx))
                        .map(|g| g.objects.is_none())
                        .unwrap_or(false);
                    if let Some(g) = self.sessions[self.active_idx].explorer_schemas[sidx]
                        .groups
                        .as_mut()
                        .and_then(|g| g.get_mut(gidx))
                    {
                        g.expanded = true;
                    }
                    if needs_load {
                        self.load_group_objects(sidx, gidx);
                    }
                }
                self.clamp_explorer_cursor();
                true
            }
            ExplorerRow::Table { sidx, gidx, tidx } => {
                let expanded = self
                    .object_ref(sidx, gidx, tidx)
                    .map(|t| t.expanded)
                    .unwrap_or(false);
                if expanded {
                    if let Some(t) = self.object_mut(sidx, gidx, tidx) {
                        t.expanded = false;
                    }
                } else {
                    let needs_load = self
                        .object_ref(sidx, gidx, tidx)
                        .map(|t| t.columns.is_none())
                        .unwrap_or(false);
                    if let Some(t) = self.object_mut(sidx, gidx, tidx) {
                        t.expanded = true;
                    }
                    if needs_load {
                        self.load_table_columns(sidx, gidx, tidx);
                    }
                }
                self.clamp_explorer_cursor();
                self.maybe_load_detail_for_cursor();
                true
            }
            ExplorerRow::Column { .. } => {
                self.maybe_load_detail_for_cursor();
                false
            }
        }
    }

    /// Collapse the row under the cursor (or jump to its parent).
    pub fn collapse_explorer_cursor(&mut self) {
        let rows = self.explorer_visible_rows();
        let cur = self.sessions[self.active_idx].explorer_cursor;
        let Some(row) = rows.get(cur).cloned() else {
            return;
        };
        match row {
            ExplorerRow::Schema { idx } => {
                self.sessions[self.active_idx].explorer_schemas[idx].expanded = false;
            }
            ExplorerRow::Group { sidx, gidx } => {
                let expanded = self.sessions[self.active_idx].explorer_schemas[sidx]
                    .groups
                    .as_ref()
                    .and_then(|g| g.get(gidx))
                    .map(|g| g.expanded)
                    .unwrap_or(false);
                if expanded {
                    if let Some(g) = self.sessions[self.active_idx].explorer_schemas[sidx]
                        .groups
                        .as_mut()
                        .and_then(|g| g.get_mut(gidx))
                    {
                        g.expanded = false;
                    }
                } else if let Some(pos) = rows[..cur]
                    .iter()
                    .rposition(|r| matches!(r, ExplorerRow::Schema { idx } if *idx == sidx))
                {
                    self.sessions[self.active_idx].explorer_cursor = pos;
                }
            }
            ExplorerRow::Table { sidx, gidx, tidx } => {
                let expanded = self
                    .object_ref(sidx, gidx, tidx)
                    .map(|t| t.expanded)
                    .unwrap_or(false);
                if expanded {
                    if let Some(t) = self.object_mut(sidx, gidx, tidx) {
                        t.expanded = false;
                    }
                } else if let Some(pos) = rows[..cur].iter().rposition(|r| {
                    matches!(r, ExplorerRow::Group { sidx: ps, gidx: pg }
                        if *ps == sidx && *pg == gidx)
                }) {
                    self.sessions[self.active_idx].explorer_cursor = pos;
                }
            }
            ExplorerRow::Column {
                sidx, gidx, tidx, ..
            } => {
                if let Some(pos) = rows[..cur].iter().rposition(|r| {
                    matches!(r, ExplorerRow::Table { sidx: ps, gidx: pg, tidx: pt }
                        if *ps == sidx && *pg == gidx && *pt == tidx)
                }) {
                    self.sessions[self.active_idx].explorer_cursor = pos;
                }
            }
        }
        self.clamp_explorer_cursor();
    }

    /// Mouse support: select a visible row WITHOUT toggling expansion
    /// (used to distinguish single-click select from arrow/double-click).
    pub fn explorer_select(&mut self, visible_idx: usize) {
        let len = self.explorer_visible_rows().len();
        if len == 0 {
            return;
        }
        self.sessions[self.active_idx].explorer_cursor = visible_idx.min(len - 1);
        self.sessions[self.active_idx].explorer_detail_scroll = 0;
        self.sessions[self.active_idx].explorer_detail_col_scroll = 0;
        self.maybe_load_detail_for_cursor();
    }

    /// (sidx, gidx, tidx) of the object under the cursor (tables/views, or
    /// a column's parent table). None on folders, schemas and empty trees.
    fn cursor_object_idx(&self) -> Option<(usize, usize, usize)> {
        let rows = self.explorer_visible_rows();
        match rows
            .get(self.sessions[self.active_idx].explorer_cursor)?
            .clone()
        {
            ExplorerRow::Table { sidx, gidx, tidx } => Some((sidx, gidx, tidx)),
            ExplorerRow::Column {
                sidx, gidx, tidx, ..
            } => Some((sidx, gidx, tidx)),
            _ => None,
        }
    }

    /// Build `SELECT * FROM schema.table` for the row under the cursor
    /// (row-bearing objects, or a column's parent) and put it into the
    /// editor. Returns None for folders, code, indexes and Oracle sequences.
    pub fn explorer_sql_for_cursor(&mut self) -> Option<String> {
        let (sidx, gidx, tidx) = self.cursor_object_idx()?;
        let sess = &self.sessions[self.active_idx];
        let schema = sess.explorer_schemas.get(sidx)?.name.clone();
        let obj = self.object_ref(sidx, gidx, tidx)?;
        let table = obj.name.clone();
        let db_type = self.active_db_type();
        // Only row-bearing relations (and PG sequences) make a SELECT.
        match (db_type, obj.kind.as_str()) {
            (DbType::Oracle, "TABLE" | "VIEW" | "MVIEW" | "FOREIGN TABLE") => Some(format!(
                "SELECT * FROM {}.{};",
                crate::db::connection::ora_ident(&schema),
                crate::db::connection::ora_ident(&table)
            )),
            (DbType::Postgres, "TABLE" | "VIEW" | "MVIEW" | "FOREIGN TABLE" | "SEQUENCE") => {
                Some(format!(
                    "SELECT * FROM {}.{};",
                    pg_quote_ident(&schema),
                    pg_quote_ident(&table)
                ))
            }
            _ => None,
        }
    }

    /// Rows fetched per preview page in the explorer detail pane. Scrolling
    /// to the end (or `Ctrl+F`) appends another page.
    pub const EXPLORER_PREVIEW_LIMIT: usize = 20;

    /// Base preview query for one object (no LIMIT/OFFSET — paging is applied
    /// by `execute_query_paged`), or None when it has no row preview
    /// (indexes; Oracle sequences use a properties query instead — see the
    /// SEQUENCE arm). Pure and unit-tested.
    fn explorer_preview_sql(
        db_type: DbType,
        schema: &str,
        name: &str,
        kind: &str,
    ) -> Option<String> {
        match kind {
            "TABLE" | "VIEW" | "MVIEW" | "FOREIGN TABLE" => Some(match db_type {
                DbType::Oracle => format!(
                    "SELECT * FROM {}.{}",
                    crate::db::connection::ora_ident(schema),
                    crate::db::connection::ora_ident(name)
                ),
                DbType::Postgres => crate::db::postgres::pg_relation_select(schema, name),
            }),
            // Sequences have no table data: show live properties instead.
            // (NEXTVAL would advance the sequence — never preview that way.)
            "SEQUENCE" => Some(match db_type {
                DbType::Oracle => format!(
                    "SELECT LAST_NUMBER, MIN_VALUE, MAX_VALUE, INCREMENT_BY, \
                     CYCLE_FLAG, CACHE_SIZE FROM ALL_SEQUENCES \
                     WHERE SEQUENCE_OWNER = '{}' AND SEQUENCE_NAME = '{}'",
                    schema.replace('\'', "''"),
                    name.replace('\'', "''")
                ),
                DbType::Postgres => crate::db::postgres::pg_relation_select(schema, name),
            }),
            _ => None,
        }
    }

    /// Kick off the background load matching the cursor: a 20-row preview
    /// for row-bearing objects, DDL/source for the rest. Cached results are
    /// reused; at most one flight per object.
    pub fn maybe_load_detail_for_cursor(&mut self) {
        let Some((sidx, gidx, tidx)) = self.cursor_object_idx() else {
            return;
        };
        let Some(obj) = self.object_ref(sidx, gidx, tidx) else {
            return;
        };
        let (has_prev, has_ddl) = (obj.has_preview(), obj.has_ddl());
        if has_prev {
            self.request_preview(sidx, gidx, tidx);
        }
        if has_ddl {
            self.request_ddl(sidx, gidx, tidx);
        }
    }

    /// Force a reload of the preview/DDL under the cursor (`d` key).
    pub fn reload_explorer_detail(&mut self) {
        let Some((sidx, gidx, tidx)) = self.cursor_object_idx() else {
            return;
        };
        if let Some(t) = self.object_mut(sidx, gidx, tidx) {
            t.preview = None;
            t.preview_loading = false;
            t.preview_error = None;
            t.ddl = None;
            t.ddl_error = None;
        }
        self.maybe_load_detail_for_cursor();
    }

    fn request_preview(&mut self, sidx: usize, gidx: usize, tidx: usize) {
        let obj = match self.object_ref(sidx, gidx, tidx) {
            Some(o) => o,
            None => return,
        };
        if obj.preview.is_some() || obj.preview_loading {
            return;
        }
        self.start_preview_page(sidx, gidx, tidx, 0);
    }

    /// Fetch the next page for the preview under the cursor. Used by an
    /// explicit `Ctrl+F`; scrolling uses [`Self::maybe_fetch_explorer_preview_page`].
    pub fn fetch_explorer_preview_page(&mut self) {
        let Some((sidx, gidx, tidx)) = self.cursor_object_idx() else {
            return;
        };
        let offset = {
            let obj = match self.object_ref(sidx, gidx, tidx) {
                Some(o) => o,
                None => return,
            };
            if obj.preview_loading {
                return;
            }
            match obj.preview.as_ref() {
                Some(p) if !p.is_error && p.truncated => p.page_offset + p.rows.len(),
                Some(_) => {
                    self.sessions[self.active_idx].set_status("No more rows to fetch");
                    return;
                }
                None => 0,
            }
        };
        self.start_preview_page(sidx, gidx, tidx, offset);
    }

    /// Auto-paging hook for the explorer detail pane: once the user scrolls
    /// into the last few loaded rows, append the next page in the background.
    pub fn maybe_fetch_explorer_preview_page(&mut self) {
        let Some((sidx, gidx, tidx)) = self.cursor_object_idx() else {
            return;
        };
        let (truncated, loading, loaded, failed) = {
            let obj = match self.object_ref(sidx, gidx, tidx) {
                Some(o) => o,
                None => return,
            };
            let Some(p) = obj.preview.as_ref() else {
                return;
            };
            (
                !p.is_error && p.truncated,
                obj.preview_loading,
                p.rows.len(),
                obj.preview_error.is_some(),
            )
        };
        if loading || !truncated || failed {
            return;
        }
        let scroll = self.sessions[self.active_idx].explorer_detail_scroll;
        if scroll + 10 < loaded {
            return;
        }
        if self.max_rows > 0 && loaded >= self.max_rows {
            self.sessions[self.active_idx]
                .set_status(format!("Reached max_rows limit ({})", self.max_rows));
            if let Some(obj) = self.object_mut(sidx, gidx, tidx) {
                if let Some(p) = obj.preview.as_mut() {
                    p.truncated = false;
                }
            }
            return;
        }
        let offset = {
            let Some(obj) = self.object_ref(sidx, gidx, tidx) else {
                return;
            };
            obj.preview
                .as_ref()
                .map(|p| p.page_offset + p.rows.len())
                .unwrap_or(0)
        };
        self.start_preview_page(sidx, gidx, tidx, offset);
    }

    /// Shared loader for the first and subsequent preview pages.
    fn start_preview_page(&mut self, sidx: usize, gidx: usize, tidx: usize, offset: usize) {
        let obj = match self.object_ref(sidx, gidx, tidx) {
            Some(o) => o.clone(),
            None => return,
        };
        let db_type = self.active_db_type();
        let schema = match self.sessions[self.active_idx].explorer_schemas.get(sidx) {
            Some(s) => s.name.clone(),
            None => return,
        };
        let sql = match Self::explorer_preview_sql(db_type, &schema, &obj.name, &obj.kind) {
            Some(s) => s,
            None => return,
        };
        let req = {
            let entry = match self.object_mut(sidx, gidx, tidx) {
                Some(e) => e,
                None => return,
            };
            if entry.preview_loading {
                return;
            }
            entry.preview_req = entry.preview_req.wrapping_add(1);
            entry.preview_loading = true;
            entry.preview_error = None;
            entry.preview_req
        };
        let sid = self.sessions[self.active_idx].id;
        let (name, kind) = (obj.name.clone(), obj.kind.clone());
        let conn = self.active_connection();
        let exec_lock = self.sessions[self.active_idx].exec_lock.clone();
        let tx = self.explorer_tx.clone();
        thread::spawn(move || {
            let result = match conn {
                Some(c) => {
                    // Serialize with editor queries on the same connection.
                    let _guard = exec_lock.lock().unwrap_or_else(|e| e.into_inner());
                    c.execute_query_paged(&sql, offset, Self::EXPLORER_PREVIEW_LIMIT)
                }
                None => QueryRow {
                    columns: vec![],
                    rows: vec![],
                    truncated: false,
                    elapsed_ms: 0,
                    is_error: true,
                    error_msg: Some("Not connected. Use Ctrl+O to connect.".into()),
                    rows_affected: None,
                    page_offset: offset,
                    page_size: Self::EXPLORER_PREVIEW_LIMIT,
                    byte_count: 0,
                    total_fetched: 0,
                },
            };
            let _ = tx.send(ExplorerMsg::Preview {
                sid,
                schema,
                table: name,
                kind,
                req,
                result,
            });
        });
    }

    fn request_ddl(&mut self, sidx: usize, gidx: usize, tidx: usize) {
        let obj = match self.object_ref(sidx, gidx, tidx) {
            Some(o) => o.clone(),
            None => return,
        };
        if !obj.has_ddl() {
            return;
        }
        let sess = &mut self.sessions[self.active_idx];
        let schema = match sess.explorer_schemas.get(sidx) {
            Some(s) => s.name.clone(),
            None => return,
        };
        let entry = match self.object_mut(sidx, gidx, tidx) {
            Some(e) => e,
            None => return,
        };
        if entry.ddl.is_some() || entry.ddl_loading {
            return;
        }
        entry.ddl_loading = true;
        entry.ddl_error = None;
        let sid = self.sessions[self.active_idx].id;
        let (name, kind) = (obj.name.clone(), obj.kind.clone());
        let conn = self.active_connection();
        let exec_lock = self.sessions[self.active_idx].exec_lock.clone();
        let tx = self.explorer_tx.clone();
        thread::spawn(move || {
            let result = match conn {
                Some(c) => {
                    let _guard = exec_lock.lock().unwrap_or_else(|e| e.into_inner());
                    c.object_ddl(&schema, &name, &kind)
                        // {:#} keeps the whole cause chain, not just the top line.
                        .map_err(|e| format!("{:#}", e))
                }
                None => Err("Not connected. Use Ctrl+O to connect.".into()),
            };
            let _ = tx.send(ExplorerMsg::Ddl {
                sid,
                schema,
                name,
                kind,
                result,
            });
        });
    }

    /// Mutable access to a cached object by (session id, schema, name,
    /// kind). Names alone don't identify objects — a table and a function
    /// may share one.
    fn find_cached_object<'a>(
        sessions: &'a mut [Session],
        sid: usize,
        schema: &str,
        name: &str,
        kind: &str,
    ) -> Option<&'a mut ExplorerTable> {
        let sess = sessions.iter_mut().find(|s| s.id == sid)?;
        for s in sess
            .explorer_schemas
            .iter_mut()
            .filter(|s| s.name == schema)
        {
            let Some(groups) = s.groups.as_mut() else {
                continue;
            };
            for g in groups.iter_mut() {
                let Some(objs) = g.objects.as_mut() else {
                    continue;
                };
                if let Some(e) = objs.iter_mut().find(|o| o.name == name && o.kind == kind) {
                    return Some(e);
                }
            }
        }
        None
    }

    /// Drain finished background preview/DDL loads into the tree cache.
    /// Called every frame (like `poll_result`).
    pub fn poll_explorer(&mut self) {
        for _ in 0..16 {
            let msg = match self.explorer_rx.try_recv() {
                Ok(m) => m,
                Err(_) => break,
            };
            match msg {
                ExplorerMsg::Preview {
                    sid,
                    schema,
                    table,
                    kind,
                    req,
                    result,
                } => {
                    if let Some(e) =
                        Self::find_cached_object(&mut self.sessions, sid, &schema, &table, &kind)
                    {
                        if e.preview_req != req {
                            // A newer request (reload/next page) superseded this
                            // one; leave its loading flag alone.
                            continue;
                        }
                        let expected = e.preview.as_ref().map(|p| p.page_offset + p.rows.len());
                        if result.page_offset == 0 {
                            // First page (or a reload): replace the preview.
                            e.preview_error = None;
                            e.preview = Some(result);
                            e.preview_loading = false;
                        } else if expected != Some(result.page_offset) {
                            // Stale continuation (object was reloaded while the
                            // request was in flight). Leave the in-flight flags
                            // alone so the newer load can finish.
                        } else if result.is_error {
                            let msg = result
                                .error_msg
                                .as_deref()
                                .unwrap_or("Failed to load more rows");
                            e.preview_error = Some(explorer_error_hint(msg));
                            e.preview_loading = false;
                        } else if let Some(p) = e.preview.as_mut() {
                            p.rows.extend(result.rows);
                            p.truncated = result.truncated;
                            p.elapsed_ms = p.elapsed_ms.saturating_add(result.elapsed_ms);
                            p.byte_count = p.byte_count.saturating_add(result.byte_count);
                            p.total_fetched = p.rows.len();
                            p.page_size = result.page_size;
                            e.preview_error = None;
                            e.preview_loading = false;
                        }
                    }
                }
                ExplorerMsg::Ddl {
                    sid,
                    schema,
                    name,
                    kind,
                    result,
                } => {
                    if let Some(e) =
                        Self::find_cached_object(&mut self.sessions, sid, &schema, &name, &kind)
                    {
                        match result {
                            Ok(text) => {
                                e.ddl = Some(text);
                                e.ddl_error = None;
                            }
                            Err(err) => {
                                e.ddl = None;
                                e.ddl_error = Some(explorer_error_hint(&err));
                            }
                        }
                        e.ddl_loading = false;
                    }
                }
            }
        }
    }

    pub fn check_connections(&mut self) {
        let map = self.conn_map.lock().unwrap_or_else(|e| e.into_inner());
        for session in &mut self.sessions {
            session.is_connected = map.contains_key(&session.id);
        }
    }

    pub fn active_connection(&self) -> Option<Arc<dyn DbConnection>> {
        let id = self.sessions[self.active_idx].id;
        let map = self.conn_map.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&id).cloned()
    }

    // ---------------- F4 Toad-style Object Describe / DDL ----------------

    /// Open the F4 DDL viewer for the identifier under the SQL editor cursor
    /// (or active single-identifier selection). Returns true when opened.
    pub fn open_ddl_at_cursor(&mut self) -> bool {
        let idx = self.active_idx;
        let dialect = self.active_db_type();
        let target_opt = {
            let sess = &self.sessions[idx];
            let text = sess.editor.text();
            let off = sess.editor.cursor_char_offset();
            let sel = sess.selected_text();
            ddl::extract_ddl_target(&text, off, sel.as_deref(), dialect)
        };
        let Some(target) = target_opt else {
            self.sessions[idx].set_status(
                "Put the cursor on an object name (table, index, view, package, …) and press F4",
            );
            return false;
        };
        if !self.sessions[idx].is_connected {
            self.sessions[idx].set_status("Not connected — press Ctrl+O to connect first");
            return false;
        }
        self.start_ddl_load(target, SessionMode::Query);
        true
    }

    /// Open the F4 DDL viewer for the object highlighted in the F12 DB explorer.
    pub fn open_ddl_from_explorer(&mut self) -> bool {
        let Some((sidx, gidx, tidx)) = self.cursor_object_idx() else {
            self.sessions[self.active_idx]
                .set_status("Select an object (table, index, view, routine, …) first");
            return false;
        };
        let (schema, name, kind) = {
            let sess = &self.sessions[self.active_idx];
            let Some(s) = sess.explorer_schemas.get(sidx) else {
                return false;
            };
            let Some(obj) = self.object_ref(sidx, gidx, tidx) else {
                return false;
            };
            (s.name.clone(), obj.name.clone(), obj.kind.clone())
        };
        let target = DdlTarget {
            schema: Some(schema),
            schema_quoted: true,
            name,
            name_quoted: true,
            kind_hint: Some(kind),
        };
        self.start_ddl_load(target, SessionMode::DbExplorer);
        true
    }

    /// Reload the currently open F4 DDL target from the database.
    pub fn reload_ddl(&mut self) {
        let idx = self.active_idx;
        let Some(target) = self.sessions[idx].ddl_target.clone() else {
            return;
        };
        let ret = self.sessions[idx].ddl_return_mode.clone();
        self.start_ddl_load(target, ret);
    }

    fn start_ddl_load(&mut self, target: DdlTarget, return_mode: SessionMode) {
        let idx = self.active_idx;
        let conn = self.active_connection();
        let (sid, req, exec_lock) = {
            let sess = &mut self.sessions[idx];
            sess.ddl_req_seq = sess.ddl_req_seq.wrapping_add(1);
            sess.ddl_target = Some(target.clone());
            sess.ddl_object = None;
            sess.ddl_error = None;
            sess.ddl_loading = true;
            sess.ddl_scroll = 0;
            sess.ddl_hscroll = 0;
            if sess.mode != SessionMode::ObjectDdl {
                sess.ddl_return_mode = return_mode;
            }
            sess.mode = SessionMode::ObjectDdl;
            (sess.id, sess.ddl_req_seq, sess.exec_lock.clone())
        };
        let tx = self.ddl_tx.clone();
        thread::spawn(move || {
            let result = match conn {
                Some(c) => {
                    let _guard = exec_lock.lock().unwrap_or_else(|e| e.into_inner());
                    c.describe_object(&target)
                        .map_err(|e| explorer_error_hint(&format!("{:#}", e)))
                }
                None => Err("Not connected. Use Ctrl+O to connect.".into()),
            };
            let _ = tx.send(DdlMsg { sid, req, result });
        });
    }

    /// Drain finished F4 DDL lookups onto their sessions.
    pub fn poll_ddl(&mut self) {
        while let Ok(msg) = self.ddl_rx.try_recv() {
            let Some(sess) = self.sessions.iter_mut().find(|s| s.id == msg.sid) else {
                continue;
            };
            if sess.ddl_req_seq != msg.req {
                continue;
            }
            sess.ddl_loading = false;
            match msg.result {
                Ok(obj) => {
                    sess.ddl_object = Some(obj);
                    sess.ddl_error = None;
                }
                Err(err) => {
                    if Self::is_cancelled_msg(&err) {
                        sess.ddl_error = Some("DDL lookup cancelled.".into());
                    } else {
                        sess.ddl_error = Some(err);
                    }
                    sess.ddl_object = None;
                }
            }
        }
    }

    // ---------------- F6 Performance & DBA Dashboard ----------------

    /// Auto-refresh cadence when live `a` mode is enabled in F6.
    pub const PERF_AUTO_REFRESH_SECS: u64 = 5;

    /// Enter the F6 Performance dashboard and kick off an initial snapshot +
    /// query run if connected.
    pub fn enter_performance(&mut self) {
        let idx = self.active_idx;
        let query_count = perf::perf_queries_for(self.active_db_type()).len();
        {
            let sess = &mut self.sessions[idx];
            sess.mode = SessionMode::Performance;
            if query_count > 0 && sess.perf_cursor >= query_count {
                sess.perf_cursor = 0;
            }
            sess.perf_scroll = 0;
            sess.perf_col_scroll = 0;
            sess.perf_sql_scroll = 0;
        }
        if self.sessions[idx].is_connected {
            self.refresh_performance(true);
        }
    }

    /// Move the F6 DBA query selection by `delta` and load/run the query if not
    /// already cached (rapid moves supersede earlier queued requests).
    pub fn move_perf_cursor(&mut self, delta: isize) {
        let count = perf::perf_queries_for(self.active_db_type()).len();
        if count == 0 {
            return;
        }
        let idx = self.active_idx;
        let cur = self.sessions[idx].perf_cursor as isize;
        let next = cur.saturating_add(delta).clamp(0, count as isize - 1) as usize;
        self.select_perf_query(next, false);
    }

    /// Jump directly to DBA query `query_idx`. When `force_run` is true (e.g.
    /// `1`..`0` keys or `Enter`), always re-executes against the DB; otherwise
    /// reuses a cached result if present.
    pub fn select_perf_query(&mut self, query_idx: usize, force_run: bool) {
        let count = perf::perf_queries_for(self.active_db_type()).len();
        if count == 0 {
            return;
        }
        let clamped = query_idx.min(count - 1);
        let idx = self.active_idx;
        let changed = self.sessions[idx].perf_cursor != clamped;
        {
            let sess = &mut self.sessions[idx];
            sess.perf_cursor = clamped;
            if changed {
                sess.perf_scroll = 0;
                sess.perf_col_scroll = 0;
                sess.perf_sql_scroll = 0;
            }
        }
        let cached = self.sessions[idx].perf_results.contains_key(&clamped);
        if force_run || !cached {
            self.refresh_performance(false);
        }
    }

    /// Toggle 5-second live `btop`-style auto-refresh (`a` in F6).
    pub fn toggle_perf_auto_refresh(&mut self) {
        let idx = self.active_idx;
        let on = {
            let sess = &mut self.sessions[idx];
            sess.perf_auto_refresh = !sess.perf_auto_refresh;
            let msg = if sess.perf_auto_refresh {
                format!(
                    "Performance live auto-refresh ON (every {}s — press a to pause)",
                    Self::PERF_AUTO_REFRESH_SECS
                )
            } else {
                "Performance live auto-refresh OFF (press R/Enter to refresh)".to_string()
            };
            sess.set_status(msg);
            sess.perf_auto_refresh
        };
        if on {
            self.refresh_performance(true);
        }
    }

    /// Periodic tick called from the main TUI loop: runs an auto-refresh when
    /// F6 is open, `perf_auto_refresh` is enabled, and the connection is idle.
    pub fn tick_perf(&mut self) {
        let idx = self.active_idx;
        let sess = &self.sessions[idx];
        if sess.mode != SessionMode::Performance
            || !sess.perf_auto_refresh
            || !sess.is_connected
            || sess.perf_loading
            || sess.pending_query
            || sess.paging
        {
            return;
        }
        let due = match sess.perf_last_refresh {
            None => true,
            Some(since) => {
                since.elapsed() >= std::time::Duration::from_secs(Self::PERF_AUTO_REFRESH_SECS)
            }
        };
        if due {
            self.refresh_performance(true);
        }
    }

    /// SQL text of the currently highlighted F6 DBA query.
    pub fn active_perf_query_sql(&self) -> Option<String> {
        let specs = perf::perf_queries_for(self.active_db_type());
        let cursor = self.sessions[self.active_idx].perf_cursor;
        specs.get(cursor).map(|q| format!("{};", q.sql.trim_end_matches(';')))
    }

    /// Spawn a background worker to run the selected F6 DBA query and
    /// optionally refresh the top health snapshot.
    pub fn refresh_performance(&mut self, include_snapshot: bool) {
        let idx = self.active_idx;
        let db_type = self.active_db_type();
        let specs = perf::perf_queries_for(db_type);
        if specs.is_empty() {
            return;
        }
        let query_idx = self.sessions[idx].perf_cursor.min(specs.len() - 1);
        let spec = specs[query_idx];
        let conn = self.active_connection();

        let (sid, req, latest_seq, exec_lock) = {
            let sess = &mut self.sessions[idx];
            let next_req = sess.perf_latest_seq.fetch_add(1, Ordering::SeqCst) + 1;
            sess.perf_loading = true;
            sess.perf_last_refresh = Some(std::time::Instant::now());
            (
                sess.id,
                next_req,
                sess.perf_latest_seq.clone(),
                sess.exec_lock.clone(),
            )
        };
        let tx = self.perf_tx.clone();

        thread::spawn(move || {
            let Some(c) = conn else {
                let _ = tx.send(PerfMsg {
                    sid,
                    req,
                    query_idx,
                    snapshot: include_snapshot
                        .then(|| Err("Not connected. Use Ctrl+O to connect.".into())),
                    query_result: Some(QueryRow::error(
                        "Not connected. Use Ctrl+O to connect.".into(),
                        0,
                        0,
                        200,
                    )),
                });
                return;
            };

            let _guard = exec_lock.lock().unwrap_or_else(|e| e.into_inner());
            // If the user already navigated to another query while we waited on
            // `exec_lock`, skip running the superseded query on the database.
            if latest_seq.load(Ordering::SeqCst) != req {
                return;
            }

            let snapshot = if include_snapshot {
                Some(c.collect_perf_snapshot().map_err(|e| format!("{:#}", e)))
            } else {
                None
            };

            let mut qr = c.execute_query(spec.sql, 200);
            if qr.is_error {
                let err_txt = qr.error_msg.as_deref().unwrap_or("");
                let missing_obj_or_priv = err_txt.contains("ORA-00942")
                    || err_txt.contains("ORA-01031")
                    || err_txt.contains("42P01")
                    || err_txt.contains("42501")
                    || err_txt.to_ascii_lowercase().contains("does not exist")
                    || err_txt.to_ascii_lowercase().contains("permission denied");
                if missing_obj_or_priv {
                    if let Some(fb) = spec.fallback_sql {
                        qr = c.execute_query(fb, 200);
                    }
                }
                if qr.is_error && c.db_type() == DbType::Oracle {
                    if let Some(msg) = qr.error_msg.take() {
                        qr.error_msg = Some(perf_oracle_error_hint(&msg));
                    }
                }
            }

            let _ = tx.send(PerfMsg {
                sid,
                req,
                query_idx,
                snapshot,
                query_result: Some(qr),
            });
        });
    }

    /// Drain finished F6 snapshot + DBA query results.
    pub fn poll_perf(&mut self) {
        while let Ok(msg) = self.perf_rx.try_recv() {
            let Some(sess) = self.sessions.iter_mut().find(|s| s.id == msg.sid) else {
                continue;
            };
            let current_latest = sess.perf_latest_seq.load(Ordering::SeqCst);
            if msg.req == current_latest {
                sess.perf_loading = false;
            }
            if let Some(snap_res) = msg.snapshot {
                match snap_res {
                    Ok(snap) => {
                        sess.perf_history.record(&snap);
                        sess.perf_snapshot = Some(snap);
                        sess.perf_snapshot_error = None;
                    }
                    Err(err) => {
                        if !Self::is_cancelled_msg(&err) {
                            sess.perf_snapshot_error = Some(err);
                        }
                    }
                }
            }
            if let Some(qr) = msg.query_result {
                let cancelled = qr.is_error
                    && qr
                        .error_msg
                        .as_deref()
                        .is_some_and(Self::is_cancelled_msg);
                if cancelled {
                    sess.set_status("Performance query cancelled");
                } else {
                    sess.perf_results.insert(msg.query_idx, qr);
                }
            }
        }
    }
}

fn perf_oracle_error_hint(msg: &str) -> String {
    if msg.contains("ORA-00942") || msg.contains("ORA-01031") {
        format!(
            "{}\n\nHint: Performance queries read V$ dynamic views (V$SESSION, V$SQL, \
             V$SESSTAT, V$PROCESS, V$SGAINFO). Ask your DBA for:\n  \
             GRANT SELECT_CATALOG_ROLE TO <your_user>;",
            msg
        )
    } else {
        msg.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::completion::{CompletionItem, CompletionPopup, ItemKind};
    use crate::db::connection::{MetaObject, MetaObjectKind};

    fn fake_group(key: &str, objects: Vec<ExplorerTable>) -> ExplorerGroup {
        ExplorerGroup {
            key: key.into(),
            count: objects.len() as u64,
            expanded: true,
            objects: Some(objects),
            load_error: None,
        }
    }

    fn fake_explorer() -> SessionManager {
        let mut sm = SessionManager::new();
        let mut emp = ExplorerTable::new("EMP".into(), "TABLE".into());
        emp.expanded = true;
        emp.columns = Some(vec![
            ColumnInfo {
                name: "EMPNO".into(),
                data_type: "NUMBER".into(),
                nullable: false,
            },
            ColumnInfo {
                name: "ENAME".into(),
                data_type: "VARCHAR2".into(),
                nullable: true,
            },
        ]);
        let dept = ExplorerTable::new("DEPT".into(), "TABLE".into());
        let emp_v = ExplorerTable::new("EMP_V".into(), "VIEW".into());
        let calc = ExplorerTable::new("CALC_BONUS".into(), "PROCEDURE".into());
        sm.sessions[0].explorer_schemas = vec![
            ExplorerSchema {
                name: "SCOTT".into(),
                expanded: true,
                groups: Some(vec![
                    fake_group("TABLES", vec![emp, dept]),
                    fake_group("VIEWS", vec![emp_v]),
                    fake_group("PROCEDURES", vec![calc]),
                ]),
                load_error: None,
            },
            ExplorerSchema {
                name: "HR".into(),
                expanded: false,
                groups: None,
                load_error: None,
            },
        ];
        sm.sessions[0].explorer_loaded = true;
        sm
    }

    #[test]
    fn open_completion_uses_cached_metadata() {
        let mut sm = SessionManager::new();
        {
            let sess = &mut sm.sessions[0];
            sess.is_connected = true;
            sess.conn_dialog.db_type = DbType::Oracle;
            sess.metadata.current_schema = "SCOTT".into();
            sess.metadata.put_schemas(vec!["SCOTT".into()]);
            sess.metadata.put_objects(
                "SCOTT",
                vec![MetaObject {
                    schema: "SCOTT".into(),
                    name: "EMP".into(),
                    kind: MetaObjectKind::Table,
                }],
            );
            sess.metadata.put_columns(
                "SCOTT",
                "EMP",
                vec![ColumnInfo {
                    name: "EMPNO".into(),
                    data_type: "NUMBER".into(),
                    nullable: false,
                }],
            );
            sess.editor.set_text("SELECT * FROM emp e WHERE e.");
            sess.editor.cursor_col = sess.editor.lines[0].len();
        }
        sm.open_completion();
        let popup = sm.sessions[0].completion.as_ref().expect("popup opened");
        assert!(!popup.loading);
        assert!(popup.items.iter().any(|i| i.label == "EMPNO"));
        sm.move_completion(1);
        sm.close_completion();
        assert!(sm.sessions[0].completion.is_none());
    }

    #[test]
    fn completion_accept_replaces_typed_word() {
        let mut sm = SessionManager::new();
        {
            let sess = &mut sm.sessions[0];
            sess.editor.set_text("SELECT * FROM emp e WHERE e.em");
            sess.editor.cursor_row = 0;
            sess.editor.cursor_col = sess.editor.lines[0].len();
            sess.completion = Some(CompletionPopup::new(
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
        }
        sm.accept_completion();
        assert_eq!(
            sm.sessions[0].editor.text(),
            "SELECT * FROM emp e WHERE e.EMPNO"
        );
        assert_eq!(sm.sessions[0].editor.cursor_col, 33);
        assert!(sm.sessions[0].completion.is_none());
    }

    #[test]
    fn completion_accept_ignores_stale_row() {
        let mut sm = SessionManager::new();
        {
            let sess = &mut sm.sessions[0];
            sess.editor.set_text("SELECT *\nFROM emp");
            sess.editor.cursor_row = 1;
            sess.editor.cursor_col = 3;
            sess.completion = Some(CompletionPopup::new(
                vec![CompletionItem {
                    label: "EMPNO".into(),
                    insert_text: "EMPNO".into(),
                    kind: ItemKind::Column,
                    detail: String::new(),
                }],
                0,
                0,
                0,
                false,
            ));
        }
        sm.accept_completion();
        assert_eq!(sm.sessions[0].editor.text(), "SELECT *\nFROM emp");
    }

    #[test]
    fn explorer_tree_flattens_in_order() {
        let sm = fake_explorer();
        let rows = sm.explorer_visible_rows();
        // SCOTT + Tables + EMP + 2 cols + DEPT + Views + EMP_V
        //   + Procedures + CALC_BONUS + HR = 11 rows.
        assert_eq!(rows.len(), 11);
        assert!(matches!(rows[0], ExplorerRow::Schema { idx: 0 }));
        assert!(matches!(rows[1], ExplorerRow::Group { sidx: 0, gidx: 0 }));
        assert!(matches!(
            rows[2],
            ExplorerRow::Table {
                sidx: 0,
                gidx: 0,
                tidx: 0
            }
        ));
        assert!(matches!(
            rows[3],
            ExplorerRow::Column {
                sidx: 0,
                gidx: 0,
                tidx: 0,
                cidx: 0
            }
        ));
        assert!(matches!(
            rows[5],
            ExplorerRow::Table {
                sidx: 0,
                gidx: 0,
                tidx: 1
            }
        ));
        assert!(matches!(rows[6], ExplorerRow::Group { sidx: 0, gidx: 1 }));
        assert!(matches!(rows[8], ExplorerRow::Group { sidx: 0, gidx: 2 }));
        assert!(matches!(rows[10], ExplorerRow::Schema { idx: 1 }));
    }

    #[test]
    fn explorer_filter_keeps_parents_for_context() {
        let mut sm = fake_explorer();
        sm.sessions[0].explorer_filter = "dept".into();
        let rows = sm.explorer_visible_rows();
        // DEPT matches + Tables folder + SCOTT for context; nothing else.
        assert_eq!(rows.len(), 3);
        assert!(matches!(rows[0], ExplorerRow::Schema { idx: 0 }));
        assert!(matches!(rows[1], ExplorerRow::Group { sidx: 0, gidx: 0 }));
        assert!(matches!(
            rows[2],
            ExplorerRow::Table {
                sidx: 0,
                gidx: 0,
                tidx: 1
            }
        ));
    }

    #[test]
    fn explorer_filter_matches_group_labels() {
        let mut sm = fake_explorer();
        sm.sessions[0].explorer_filter = "procedure".into();
        let rows = sm.explorer_visible_rows();
        // Procedures folder + CALC_BONUS + SCOTT for context.
        assert_eq!(rows.len(), 3);
        assert!(matches!(rows[1], ExplorerRow::Group { sidx: 0, gidx: 2 }));
    }

    #[test]
    fn explorer_object_predicates() {
        let proc = ExplorerTable::new("p".into(), "PROCEDURE".into());
        assert!(!proc.has_preview() && proc.has_ddl() && !proc.is_expandable());
        let tbl = ExplorerTable::new("t".into(), "TABLE".into());
        assert!(tbl.has_preview() && !tbl.has_ddl() && tbl.is_expandable());
        let idx = ExplorerTable::new("i".into(), "INDEX".into());
        assert!(!idx.has_preview() && idx.has_ddl() && idx.is_expandable());
        let seq = ExplorerTable::new("s".into(), "SEQUENCE".into());
        assert!(seq.has_preview() && seq.has_ddl() && seq.is_expandable());
        let mv = ExplorerTable::new("m".into(), "MVIEW".into());
        assert!(mv.has_preview() && !mv.has_ddl());
    }

    #[test]
    fn explorer_group_labels_and_order() {
        assert_eq!(explorer_group_label("TABLES"), "Tables");
        assert_eq!(explorer_group_label("MVIEWS"), "Materialized Views");
        assert!(explorer_group_order("TABLES") < explorer_group_order("VIEWS"));
        assert!(explorer_group_order("VIEWS") < explorer_group_order("INDEXES"));
        assert!(explorer_group_order("INDEXES") < explorer_group_order("FUNCTIONS"));
    }

    #[test]
    fn explorer_preview_sql_per_kind() {
        // Row-bearing relations use a base SELECT; paging adds LIMIT/OFFSET.
        let ora_tbl = SessionManager::explorer_preview_sql(DbType::Oracle, "SCOTT", "EMP", "TABLE");
        assert_eq!(ora_tbl.as_deref(), Some("SELECT * FROM SCOTT.EMP"));
        let pg_tbl =
            SessionManager::explorer_preview_sql(DbType::Postgres, "public", "emp", "MVIEW");
        assert_eq!(pg_tbl.as_deref(), Some("SELECT * FROM \"public\".\"emp\""));
        // Indexes have no row preview; code neither.
        assert_eq!(
            SessionManager::explorer_preview_sql(DbType::Postgres, "public", "i", "INDEX"),
            None
        );
        assert_eq!(
            SessionManager::explorer_preview_sql(DbType::Oracle, "SCOTT", "P", "PROCEDURE"),
            None
        );
        // Sequences preview properties (never NEXTVAL).
        let seq = SessionManager::explorer_preview_sql(DbType::Oracle, "SCOTT", "SEQ", "SEQUENCE")
            .unwrap();
        assert!(seq.contains("ALL_SEQUENCES"));
        assert!(!seq.contains("NEXTVAL"));
    }

    #[test]
    fn explorer_preview_pages_append_in_place() {
        fn page(offset: usize, vals: &[&str], truncated: bool) -> QueryRow {
            let rows: Vec<Vec<String>> = vals.iter().map(|v| vec![(*v).to_string()]).collect();
            QueryRow {
                columns: vec!["X".into()],
                total_fetched: rows.len(),
                rows,
                truncated,
                elapsed_ms: 1,
                is_error: false,
                error_msg: None,
                rows_affected: None,
                page_offset: offset,
                page_size: SessionManager::EXPLORER_PREVIEW_LIMIT,
                byte_count: 0,
            }
        }

        let mut sm = fake_explorer();
        let sid = sm.sessions[0].id;
        sm.explorer_tx
            .send(ExplorerMsg::Preview {
                sid,
                schema: "SCOTT".into(),
                table: "EMP".into(),
                kind: "TABLE".into(),
                req: 0,
                result: page(0, &["1", "2"], true),
            })
            .unwrap();
        sm.poll_explorer();
        {
            let e =
                SessionManager::find_cached_object(&mut sm.sessions, sid, "SCOTT", "EMP", "TABLE")
                    .unwrap();
            assert_eq!(e.preview.as_ref().unwrap().rows.len(), 2);
        }

        sm.explorer_tx
            .send(ExplorerMsg::Preview {
                sid,
                schema: "SCOTT".into(),
                table: "EMP".into(),
                kind: "TABLE".into(),
                req: 0,
                result: page(2, &["3"], false),
            })
            .unwrap();
        sm.poll_explorer();
        let e = SessionManager::find_cached_object(&mut sm.sessions, sid, "SCOTT", "EMP", "TABLE")
            .unwrap();
        let p = e.preview.as_ref().unwrap();
        assert_eq!(p.page_offset, 0);
        assert_eq!(p.rows.len(), 3);
        assert_eq!(p.rows[2], vec!["3".to_string()]);
        assert!(!p.truncated);
        assert!(!e.preview_loading);
    }

    #[test]
    fn explorer_sql_quotes_postgres_idents() {
        let mut sm = fake_explorer();
        sm.sessions[0].explorer_cursor = 2; // EMP table row.
                                            // Oracle: bare identifiers.
        sm.sessions[0].conn_dialog.db_type = DbType::Oracle;
        assert_eq!(
            sm.explorer_sql_for_cursor().as_deref(),
            Some("SELECT * FROM SCOTT.EMP;")
        );
        // Postgres: quoted identifiers.
        sm.sessions[0].conn_dialog.db_type = DbType::Postgres;
        assert_eq!(
            sm.explorer_sql_for_cursor().as_deref(),
            Some("SELECT * FROM \"SCOTT\".\"EMP\";")
        );
        // Schema rows have no SQL.
        sm.sessions[0].explorer_cursor = 0;
        assert_eq!(sm.explorer_sql_for_cursor(), None);
    }

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
    fn editor_word_jumps_and_undo() {
        let mut ed = SqlEditor::new();
        ed.set_text("select empno, ename from emp");
        ed.undo_stack.clear();
        ed.redo_stack.clear();
        ed.cursor_row = 0;
        ed.cursor_col = ed.lines[0].len();
        ed.move_word_left();
        assert_eq!(&ed.lines[0][ed.cursor_col..], "emp");
        ed.move_word_left();
        assert_eq!(&ed.lines[0][ed.cursor_col..], "from emp");
        ed.move_word_right();
        assert_eq!(&ed.lines[0][ed.cursor_col..], "emp");
        ed.move_word_right();
        assert_eq!(&ed.lines[0][ed.cursor_col..], "");

        // Undo / redo round-trips a word deletion.
        ed.cursor_col = "select ".len();
        ed.delete_range((0, "select ".len()), (0, "select empno".len()));
        assert_eq!(ed.lines[0], "select , ename from emp");
        assert!(ed.undo());
        assert_eq!(ed.lines[0], "select empno, ename from emp");
        assert!(ed.redo());
        assert_eq!(ed.lines[0], "select , ename from emp");
    }

    #[test]
    fn editor_hscroll_tracks_cursor() {
        let mut ed = SqlEditor::new();
        ed.set_text(&"x".repeat(100));
        ed.cursor_row = 0;
        ed.cursor_col = 100;
        ed.ensure_cursor_visible_sized(10, 20);
        assert_eq!(ed.col_offset, 81);
        ed.cursor_col = 0;
        ed.ensure_cursor_visible_sized(10, 20);
        assert_eq!(ed.col_offset, 0);
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

    #[test]
    fn table_drag_selection_extracts_tsv() {
        let mut sm = SessionManager::new();
        let sess = &mut sm.sessions[0];
        sess.results.push(QueryRow {
            columns: vec!["A".into(), "B".into(), "C".into()],
            rows: vec![
                vec!["a1".into(), "b1".into(), "c1".into()],
                vec!["a2".into(), "b2".into(), "c2".into()],
                vec!["a3".into(), "b3".into(), "c3".into()],
            ],
            truncated: false,
            elapsed_ms: 1,
            is_error: false,
            error_msg: None,
            rows_affected: None,
            page_offset: 0,
            page_size: 100,
            byte_count: 0,
            total_fetched: 3,
        });
        // Rect over B..C, rows 1..2 (0-based) — header + TSV, no borders.
        sess.table_selection = Some(TableSelection {
            anchor_row: Some(0),
            anchor_col: 1,
            cursor_row: Some(1),
            cursor_col: 2,
        });
        assert_eq!(
            sess.selected_table_text().as_deref(),
            Some("B\tC\nb1\tc1\nb2\tc2")
        );
        // Reversed endpoints give the same text.
        sess.table_selection = Some(TableSelection {
            anchor_row: Some(1),
            anchor_col: 2,
            cursor_row: Some(0),
            cursor_col: 1,
        });
        assert_eq!(
            sess.selected_table_text().as_deref(),
            Some("B\tC\nb1\tc1\nb2\tc2")
        );
        // Out-of-range endpoints clamp to the data, single cell = empty.
        sess.table_selection = Some(TableSelection {
            anchor_row: Some(0),
            anchor_col: 0,
            cursor_row: Some(99),
            cursor_col: 99,
        });
        assert_eq!(
            sess.selected_table_text().as_deref(),
            Some("A\tB\tC\na1\tb1\tc1\na2\tb2\tc2\na3\tb3\tc3")
        );
        sess.table_selection = Some(TableSelection {
            anchor_row: Some(1),
            anchor_col: 1,
            cursor_row: Some(1),
            cursor_col: 1,
        });
        assert_eq!(sess.selected_table_text(), None);
        // Header-row drag copies just the column names.
        sess.table_selection = Some(TableSelection {
            anchor_row: None,
            anchor_col: 0,
            cursor_row: None,
            cursor_col: 1,
        });
        assert_eq!(sess.selected_table_text().as_deref(), Some("A\tB"));
        // Header-through-data drag copies names plus those rows.
        sess.table_selection = Some(TableSelection {
            anchor_row: None,
            anchor_col: 1,
            cursor_row: Some(0),
            cursor_col: 2,
        });
        assert_eq!(
            sess.selected_table_text().as_deref(),
            Some("B\tC\nb1\tc1")
        );
        sess.table_selection = None;
        assert_eq!(sess.selected_table_text(), None);
    }

    #[test]
    fn editor_drag_selection_extracts_and_deletes() {
        let mut sm = SessionManager::new();
        let sess = &mut sm.sessions[0];
        sess.editor.lines = vec![
            "select *".to_string(),
            "from all_tables".to_string(),
            "where rownum < 10".to_string(),
        ];
        // Single-line selection: "all_tables".
        sess.editor_selection = Some(EditorSelection {
            anchor_row: 1,
            anchor_col: 5,
            cursor_row: 1,
            cursor_col: 15,
        });
        assert_eq!(sess.selected_text().as_deref(), Some("all_tables"));
        // Reversed endpoints give the same text.
        sess.editor_selection = Some(EditorSelection {
            anchor_row: 1,
            anchor_col: 15,
            cursor_row: 1,
            cursor_col: 5,
        });
        assert_eq!(sess.selected_text().as_deref(), Some("all_tables"));
        // Multi-line selection joins with newlines.
        sess.editor_selection = Some(EditorSelection {
            anchor_row: 0,
            anchor_col: 7,
            cursor_row: 1,
            cursor_col: 4,
        });
        assert_eq!(sess.selected_text().as_deref(), Some("*\nfrom"));
        // Empty selection yields nothing.
        sess.editor_selection = Some(EditorSelection {
            anchor_row: 0,
            anchor_col: 3,
            cursor_row: 0,
            cursor_col: 3,
        });
        assert_eq!(sess.selected_text(), None);
        // Deleting a range joins head and tail, parks cursor at start.
        sess.editor_selection = Some(EditorSelection {
            anchor_row: 0,
            anchor_col: 7,
            cursor_row: 1,
            cursor_col: 4,
        });
        assert!(sess.delete_selection());
        assert_eq!(sess.editor.lines[0], "select  all_tables");
        assert_eq!(sess.editor.lines.len(), 2);
        assert_eq!((sess.editor.cursor_row, sess.editor.cursor_col), (0, 7));
        assert_eq!(sess.editor_selection, None);
        assert!(!sess.delete_selection());
    }

    #[test]
    fn editor_vertical_move_onto_border_glyph_stays_on_boundary() {
        // Regression: crash from the screenshot — `delete` panicked at
        // `&line[cursor..]` with "byte index 1 is not a char boundary, it is
        // inside '│'". A vertical move kept the byte column (1) onto a line
        // starting with a 3-byte box glyph, then Delete sliced mid-char.
        let mut ed = SqlEditor::new();
        ed.lines = vec!["ab".to_string(), "│select".to_string()];
        ed.cursor_row = 0;
        ed.cursor_col = 1;
        ed.move_down();
        assert!(ed.lines[1].is_char_boundary(ed.cursor_col));
        // Must not panic.
        ed.delete();
        ed.backspace();
        ed.newline();
        ed.kill_line();
        for line in &ed.lines {
            assert!(line.is_char_boundary(ed.cursor_col.min(line.len())));
        }
    }

    fn fake_session_info(sid: i32, username: &str, status: &str) -> DbSessionInfo {
        DbSessionInfo {
            sid,
            serial: sid * 10,
            username: username.into(),
            status: status.into(),
            machine: "host".into(),
            program: "prog".into(),
            sql_id: String::new(),
            logon_time: format!("2024-01-01 00:00:{:02}", sid.rem_euclid(60)),
        }
    }

    #[test]
    fn session_active_filter_toggles() {
        let mut sm = SessionManager::new();
        sm.sessions[0].session_list_cache = vec![
            fake_session_info(1, "SCOTT", "ACTIVE"),
            fake_session_info(2, "HR", "INACTIVE"),
            fake_session_info(3, "SYS", "active"),
        ];
        assert_eq!(sm.visible_session_list().len(), 3);
        sm.toggle_session_active_filter();
        assert!(sm.sessions[0].session_view_active_only);
        let visible = sm.visible_session_list();
        assert_eq!(visible.len(), 2);
        assert!(visible.iter().all(SessionManager::session_info_is_active));
        sm.toggle_session_active_filter();
        assert!(!sm.sessions[0].session_view_active_only);
        assert_eq!(sm.visible_session_list().len(), 3);
    }

    #[test]
    fn session_sort_cycles_columns() {
        let mut sm = SessionManager::new();
        sm.sessions[0].session_list_cache = vec![
            fake_session_info(30, "C", "INACTIVE"),
            fake_session_info(10, "A", "ACTIVE"),
            fake_session_info(20, "B", "ACTIVE"),
        ];
        // Unsorted = DB order.
        assert_eq!(
            sm.visible_session_list()
                .iter()
                .map(|s| s.sid)
                .collect::<Vec<_>>(),
            vec![30, 10, 20]
        );
        // 1st s sorts by 1st column (SID asc).
        sm.cycle_session_sort();
        assert_eq!(sm.sessions[0].session_view_sort_col, Some(0));
        assert_eq!(
            sm.visible_session_list()
                .iter()
                .map(|s| s.sid)
                .collect::<Vec<_>>(),
            vec![10, 20, 30]
        );
        // 2nd s sorts by 2nd column (Serial# asc — mirrors SID here).
        sm.cycle_session_sort();
        assert_eq!(sm.sessions[0].session_view_sort_col, Some(1));
        assert_eq!(
            sm.visible_session_list()
                .iter()
                .map(|s| s.sid)
                .collect::<Vec<_>>(),
            vec![10, 20, 30]
        );
        // 3rd s sorts by 3rd column (User asc).
        sm.cycle_session_sort();
        assert_eq!(sm.sessions[0].session_view_sort_col, Some(2));
        assert_eq!(
            sm.visible_session_list()
                .iter()
                .map(|s| s.username.clone())
                .collect::<Vec<_>>(),
            vec!["A", "B", "C"]
        );
        // Direction toggle reverses.
        sm.toggle_session_sort_dir();
        assert!(sm.sessions[0].session_view_sort_desc);
        assert_eq!(
            sm.visible_session_list()
                .iter()
                .map(|s| s.username.clone())
                .collect::<Vec<_>>(),
            vec!["C", "B", "A"]
        );
        // Cycling through the rest wraps back to unsorted (DB order).
        for _ in 2..SessionManager::SESSION_VIEW_COL_COUNT {
            sm.cycle_session_sort();
        }
        assert_eq!(sm.sessions[0].session_view_sort_col, None);
        assert_eq!(
            sm.visible_session_list()
                .iter()
                .map(|s| s.sid)
                .collect::<Vec<_>>(),
            vec![30, 10, 20]
        );
    }

    #[test]
    fn poll_result_keeps_pending_query_until_final_script_statement() {
        let mut sm = SessionManager::new();
        let id = sm.sessions[0].id;
        sm.sessions[0].job_seq = 1;
        sm.sessions[0].pending_query = true;

        // Statement 1 of 2 finishes (`done = false`): session must stay pending.
        let r1 = QueryRow::notice("Statement 1", 1, 100);
        sm.result_tx.send((id, 1, r1, false)).unwrap();
        sm.poll_result();
        assert!(sm.sessions[0].pending_query);
        assert_eq!(sm.sessions[0].results.len(), 1);

        // Statement 2 of 2 finishes (`done = true`): pending_query clears.
        let r2 = QueryRow::notice("Statement 2", 1, 100);
        sm.result_tx.send((id, 1, r2, true)).unwrap();
        sm.poll_result();
        assert!(!sm.sessions[0].pending_query);
        assert_eq!(sm.sessions[0].results.len(), 2);
    }

    #[test]
    fn conn_dialog_cursor_right_clamps_stale_cursor_safely() {
        let mut dlg = ConnectionDialog {
            active_field: 1, // Host
            host: "äb".into(),
            cursor: 99, // stale / out-of-bounds
            ..ConnectionDialog::default()
        };
        dlg.cursor_right();
        assert_eq!(dlg.cursor, dlg.host.len());

        // Mid-char byte index snaps cleanly to a valid UTF-8 boundary.
        dlg.cursor = 1; // inside 2-byte 'ä'
        dlg.cursor_right();
        assert!(dlg.host.is_char_boundary(dlg.cursor));
    }

    #[test]
    fn execute_query_at_subdir_resolves_nested_relative_includes() {
        let dir = std::env::temp_dir().join(format!("frog_test_at_subdir_{}", std::process::id()));
        let sub = dir.join("subdir");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("part.sql"), "select 2 from dual;").unwrap();
        std::fs::write(sub.join("setup.sql"), "select 1 from dual;\n@@part.sql").unwrap();

        let mut sm = SessionManager::new();
        sm.sessions[0].query_history.clear();
        let script_directive = format!("@{}", sub.join("setup.sql").display());
        sm.execute_query(&script_directive);

        // Both statements from setup.sql and its sibling @@part.sql were expanded.
        assert_eq!(
            sm.sessions[0].query_history,
            vec!["select 1 from dual", "select 2 from dual"]
        );
        assert!(sm.sessions[0]
            .status_message
            .as_deref()
            .unwrap_or("")
            .contains("2 statements"));

        let _ = std::fs::remove_file(history_path());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_parser_ignores_inline_frog_comments_without_timestamp() {
        let raw = "-- frog@2026-11-27 12:00:00;\nSELECT 1\n-- frog@note inside sql\nFROM dual;\n\
                   -- frog@2026-11-27 12:01:00;\nSELECT 2 FROM dual;\n";
        let parsed = parse_history_contents(raw);
        assert_eq!(
            parsed,
            vec![
                "SELECT 1\n-- frog@note inside sql\nFROM dual",
                "SELECT 2 FROM dual"
            ]
        );
    }

    #[test]
    fn explorer_preview_sql_quotes_mixed_case_oracle_identifiers() {
        assert_eq!(
            SessionManager::explorer_preview_sql(DbType::Oracle, "SCOTT", "myTable", "TABLE")
                .as_deref(),
            Some("SELECT * FROM SCOTT.\"myTable\"")
        );
        assert_eq!(
            SessionManager::explorer_preview_sql(DbType::Oracle, "SCOTT", "EMP", "TABLE")
                .as_deref(),
            Some("SELECT * FROM SCOTT.EMP")
        );
    }

    #[test]
    fn history_scroll_tracks_cursor_into_view() {
        let mut sm = SessionManager::new();
        let sess = &mut sm.sessions[0];
        sess.query_history = (0..50).map(|i| format!("SELECT {}", i)).collect();
        sess.history_cursor = 49;
        sess.history_scroll = 0;
        sess.ensure_history_cursor_visible(10);
        assert_eq!(sess.history_scroll, 40);
        sess.history_cursor = 5;
        sess.ensure_history_cursor_visible(10);
        assert_eq!(sess.history_scroll, 5);
    }
}
