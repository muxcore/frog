//! Emacs-style incremental search (I-search) core.
//!
//! Pure logic (no I/O, no ratatui) so it can be unit-tested:
//! smart-case matching, substring search returning byte offsets,
//! and match types shared between `app.rs` (navigation) and
//! `widgets.rs` (highlighting).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchDirection {
    #[default]
    Forward,
    Backward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchTarget {
    Editor,
    Results,
    Sidebar,
    ExplorerTree,
    ExplorerDetail,
    SessionList,
    SessionSql,
    SessionPlan,
    Ddl,
    PerfQueries,
    PerfResults,
    PerfSql,
    History,
    Help,
    None,
}

/// One match. Navigation interprets it per target; highlighting uses the
/// row/col information.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMatch {
    /// SQL editor: byte-column start + byte length on `row`.
    Editor { row: usize, col: usize, len: usize },
    /// Result-style table: header (`None`) or data row + column.
    TableCell { row: Option<usize>, col: usize },
    /// Plain wrapped/unwrapped text pane: display line + byte col/len.
    Line { line: usize, col: usize, len: usize },
    /// Flat list panes (explorer tree, session list, history, ...).
    ListRow { row: usize },
}

/// Snapshot of every scroll/cursor position the search can move, so
/// `Ctrl-G` can abort back to the exact pre-search view.
#[derive(Debug, Clone, Default)]
pub struct SearchOrigin {
    pub editor_row: usize,
    pub editor_col: usize,
    pub editor_scroll: usize,
    pub results_scroll: usize,
    pub results_col_scroll: usize,
    pub explorer_cursor: usize,
    pub explorer_scroll: usize,
    pub explorer_detail_scroll: usize,
    pub explorer_detail_col_scroll: usize,
    pub session_cursor: usize,
    pub sql_scroll: usize,
    pub plan_scroll: usize,
    pub plan_hscroll: usize,
    pub ddl_scroll: usize,
    pub ddl_hscroll: usize,
    pub perf_cursor: usize,
    pub perf_scroll: usize,
    pub perf_col_scroll: usize,
    pub perf_sql_scroll: usize,
    pub history_cursor: usize,
}

#[derive(Debug, Clone)]
pub struct SearchState {
    pub query: String,
    pub direction: SearchDirection,
    pub target: SearchTarget,
    pub matches: Vec<SearchMatch>,
    pub current: usize,
    pub wrapped: bool,
    pub failing: bool,
    pub origin: SearchOrigin,
}

impl SearchState {
    pub fn new(target: SearchTarget, origin: SearchOrigin) -> Self {
        Self {
            query: String::new(),
            direction: SearchDirection::Forward,
            target,
            matches: Vec::new(),
            current: 0,
            wrapped: false,
            failing: false,
            origin,
        }
    }

    pub fn total(&self) -> usize {
        self.matches.len()
    }

    pub fn case_sensitive(&self) -> bool {
        is_case_sensitive(&self.query)
    }
}

/// Emacs smart-case: all-lowercase queries match case-insensitively,
/// any uppercase letter makes the search case-sensitive.
pub fn is_case_sensitive(query: &str) -> bool {
    query.chars().any(|c| c.is_uppercase())
}

/// `true` when `haystack` contains `query` under smart-case rules.
/// Empty query never matches (Emacs shows no failure, just no jump).
pub fn line_matches(haystack: &str, query: &str) -> bool {
    if query.is_empty() {
        return false;
    }
    if is_case_sensitive(query) {
        haystack.contains(query)
    } else {
        haystack.to_lowercase().contains(&query.to_lowercase())
    }
}

/// Byte offsets of every (possibly overlapping? no — non-overlapping like
/// `str::match_indices`) occurrence of `needle` in `hay`.
///
/// Case-insensitive path lowercases both sides first. For ASCII (the
/// overwhelmingly common SQL/data case) offsets coincide exactly; for
/// exotic Unicode where lowercasing changes byte length the offset is
/// floored to a char boundary by callers.
pub fn find_substring_indices(hay: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() || hay.is_empty() {
        return Vec::new();
    }
    if is_case_sensitive(needle) {
        hay.match_indices(needle)
            .map(|(off, m)| (off, m.len()))
            .collect()
    } else {
        let hay_lower = hay.to_lowercase();
        let needle_lower = needle.to_lowercase();
        if needle_lower.is_empty() {
            return Vec::new();
        }
        hay_lower
            .match_indices(needle_lower.as_str())
            .map(|(off, m)| (off, m.len()))
            .collect()
    }
}

/// All matches in editor lines, in row-major order.
pub fn find_editor_matches(lines: &[String], query: &str) -> Vec<SearchMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (row, line) in lines.iter().enumerate() {
        for (col, len) in find_substring_indices(line, query) {
            // Floor to char boundaries so exotic Unicode can't produce a
            // panic when the cursor jumps there.
            let col = floor_char_boundary(line, col);
            let len = len.min(line.len().saturating_sub(col));
            out.push(SearchMatch::Editor { row, col, len });
        }
    }
    out
}

/// All cells (header + data) containing the query, in row-major order.
/// Header sorts before every data row so `next` visits column names first.
pub fn find_table_matches(
    columns: &[String],
    rows: &[Vec<String>],
    query: &str,
) -> Vec<SearchMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (c, h) in columns.iter().enumerate() {
        if line_matches(h, query) {
            out.push(SearchMatch::TableCell { row: None, col: c });
        }
    }
    for (r, row) in rows.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            if c < columns.len() && line_matches(cell, query) {
                out.push(SearchMatch::TableCell {
                    row: Some(r),
                    col: c,
                });
            }
        }
    }
    out
}

/// All matches in plain text lines (DDL, SQL panes, help, ...).
pub fn find_line_matches(lines: &[String], query: &str) -> Vec<SearchMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (line_idx, line) in lines.iter().enumerate() {
        for (col, len) in find_substring_indices(line, query) {
            let col = floor_char_boundary(line, col);
            out.push(SearchMatch::Line {
                line: line_idx,
                col,
                len: len.min(line.len().saturating_sub(col)),
            });
        }
    }
    out
}

/// Row indices whose label contains the query.
pub fn find_list_matches(labels: &[String], query: &str) -> Vec<SearchMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    labels
        .iter()
        .enumerate()
        .filter(|(_, l)| line_matches(l, query))
        .map(|(i, _)| SearchMatch::ListRow { row: i })
        .collect()
}

fn floor_char_boundary(s: &str, mut off: usize) -> usize {
    off = off.min(s.len());
    while off > 0 && !s.is_char_boundary(off) {
        off -= 1;
    }
    off
}

/// Order index of `m` for forward/backward stepping from an anchor.
/// Returns the first match at/after (forward) or at/before (backward)
/// the anchor position, or 0 / last when wrapping.
pub fn anchor_index_editor(
    matches: &[SearchMatch],
    anchor_row: usize,
    anchor_col: usize,
    forward: bool,
) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    if forward {
        for (i, m) in matches.iter().enumerate() {
            if let SearchMatch::Editor { row, col, .. } = *m {
                if (row, col) >= (anchor_row, anchor_col) {
                    return Some(i);
                }
            }
        }
        Some(0)
    } else {
        for (i, m) in matches.iter().enumerate().rev() {
            if let SearchMatch::Editor { row, col, .. } = *m {
                if (row, col) <= (anchor_row, anchor_col) {
                    return Some(i);
                }
            }
        }
        Some(matches.len() - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_case_rules() {
        assert!(!is_case_sensitive("select"));
        assert!(!is_case_sensitive("select 123 _"));
        assert!(is_case_sensitive("Select"));
        assert!(is_case_sensitive("SELECT"));
    }

    #[test]
    fn case_insensitive_contains() {
        assert!(line_matches("SELECT * FROM t", "select"));
        // Smart-case: uppercase query is case-sensitive.
        assert!(!line_matches("select", "SELECT"));
        assert!(line_matches("SELECT", "SELECT"));
        assert!(!line_matches("select", "SELECT x"));
        assert!(!line_matches("abc", ""));
    }

    #[test]
    fn finds_editor_matches_in_order() {
        let lines = vec!["select a".to_string(), "FROM t".to_string()];
        let m = find_editor_matches(&lines, "select");
        // case-insensitive: matches line 0 only... "FROM t" has no select
        assert_eq!(m.len(), 1);
        assert_eq!(
            m[0],
            SearchMatch::Editor {
                row: 0,
                col: 0,
                len: 6
            }
        );
        let m2 = find_editor_matches(&lines, "o");
        // 'o' in "select a"? no. in "FROM t"? 'O' -> case-insensitive yes at col 2
        assert_eq!(m2.len(), 1);
    }

    #[test]
    fn table_header_sorts_first() {
        let cols = vec!["name".to_string(), "id".to_string()];
        let rows = vec![vec!["bob".to_string(), "1".to_string()]];
        let m = find_table_matches(&cols, &rows, "name");
        assert_eq!(
            m,
            vec![SearchMatch::TableCell {
                row: None,
                col: 0
            }]
        );
        let m2 = find_table_matches(&cols, &rows, "bob");
        assert_eq!(
            m2,
            vec![SearchMatch::TableCell {
                row: Some(0),
                col: 0
            }]
        );
    }

    #[test]
    fn anchor_picks_next_and_prev() {
        let lines = vec!["aa aa aa".to_string()];
        let m = find_editor_matches(&lines, "aa");
        assert_eq!(m.len(), 3);
        // Matches at cols 0, 3, 6.
        assert_eq!(anchor_index_editor(&m, 0, 4, true), Some(2));
        assert_eq!(anchor_index_editor(&m, 0, 4, false), Some(1));
        assert_eq!(anchor_index_editor(&m, 0, 100, true), Some(0)); // wrap
        assert_eq!(anchor_index_editor(&m, 0, 0, false), Some(0));
    }
}
