use oracle::Connection;
use std::sync::Arc;

pub struct OracleConnection {
    pub conn: Connection,
    /// Display string used for `NULL` cell values (from UiConfig).
    pub null_display: String,
}

#[derive(Debug, Clone)]
pub struct QueryRow {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub truncated: bool,
    pub row_count: usize,
    pub elapsed_ms: u64,
    pub is_error: bool,
    pub error_msg: Option<String>,
    pub rows_affected: Option<u64>,
    /// The row-offset this result page started from (0 for the first page).
    pub page_offset: usize,
    /// Page size used when fetching this result.
    pub page_size: usize,
    /// Total byte count of all row data (for debug display).
    pub byte_count: usize,
    /// Total rows actually fetched from the database (before truncation / paging display).
    pub total_fetched: usize,
}

impl QueryRow {
    fn error(msg: String, elapsed_ms: u64, page_offset: usize, page_size: usize) -> Self {
        Self {
            columns: vec![],
            rows: vec![],
            truncated: false,
            row_count: 0,
            elapsed_ms,
            is_error: true,
            error_msg: Some(msg),
            rows_affected: None,
            page_offset,
            page_size,
            byte_count: 0,
            total_fetched: 0,
        }
    }

    fn from_query(
        data: QueryData,
        elapsed_ms: u64,
        page_offset: usize,
        page_size: usize,
    ) -> Self {
        let (columns, rows, truncated, total_fetched, byte_count) = data;
        Self {
            columns,
            rows,
            truncated,
            row_count: total_fetched,
            elapsed_ms,
            is_error: false,
            error_msg: None,
            rows_affected: None,
            page_offset,
            page_size,
            byte_count,
            total_fetched,
        }
    }

    fn notice(text: &str, elapsed_ms: u64, page_size: usize) -> Self {
        Self {
            columns: vec!["Result".into()],
            rows: vec![vec![text.to_string()]],
            truncated: false,
            row_count: 1,
            elapsed_ms,
            is_error: false,
            error_msg: None,
            rows_affected: None,
            page_offset: 0,
            page_size,
            byte_count: 0,
            total_fetched: 0,
        }
    }
}

/// (columns, rows, truncated, total_fetched, byte_count)
type QueryData = (Vec<String>, Vec<Vec<String>>, bool, usize, usize);

/// Map an Oracle error to a message, collapsing user cancellations.
fn clean_error(e: &oracle::Error) -> String {
    let msg = e.to_string();
    if msg.contains("ORA-01013") || msg.to_lowercase().contains("cancel") {
        "Query cancelled".into()
    } else {
        msg
    }
}

/// Skip leading whitespace and SQL comments (`-- ...` / `/* ... */`) so
/// statement-type detection sees the first real keyword. Returns the
/// remaining text, or "" if the input is only comments/whitespace.
pub fn strip_leading_comments(mut sql: &str) -> &str {
    loop {
        sql = sql.trim_start();
        if let Some(rest) = sql.strip_prefix("--") {
            sql = rest.split_once('\n').map(|(_, after)| after).unwrap_or("");
        } else if let Some(rest) = sql.strip_prefix("/*") {
            match rest.find("*/") {
                Some(idx) => sql = &rest[idx + 2..],
                None => return "",
            }
        } else {
            return sql;
        }
    }
}

fn is_query_sql(trimmed: &str) -> bool {
    let upper = strip_leading_comments(trimmed).to_uppercase();
    upper.starts_with("SELECT")
        || upper.starts_with("WITH")
        || upper.starts_with("DESCRIBE")
        || upper.starts_with("EXPLAIN")
}

impl OracleConnection {
    pub fn connect_with_autocommit(
        connect_string: &str,
        user: &str,
        password: &str,
        autocommit: bool,
        null_display: &str,
    ) -> Result<Arc<Self>, anyhow::Error> {
        let mut conn = Connection::connect(user, password, connect_string)?;
        if autocommit {
            conn.set_autocommit(true);
        }
        Ok(Arc::new(Self {
            conn,
            null_display: null_display.to_string(),
        }))
    }

    /// Explicitly close the underlying Oracle connection (best-effort).
    pub fn close(this: &Self) -> anyhow::Result<()> {
        this.conn.close().map_err(|e| anyhow::anyhow!("{}", e))
    }

    /// Augment a connect error with a friendly hint when the Oracle client
    /// library isn't loadable (DPI-1047 — missing `libclntsh.so` / LD_LIBRARY_PATH).
    pub fn friendly_connect_error(err: &anyhow::Error) -> String {
        let msg = err.to_string();
        if msg.contains("DPI-1047") || msg.contains("libclntsh") {
            format!(
                "{}\n\nHint: Oracle Instant Client (libclntsh.so) is required. \
                 Install it and set LD_LIBRARY_PATH accordingly.",
                msg
            )
        } else {
            msg
        }
    }

    pub fn execute_query(this: &Self, sql: &str, max_rows: usize) -> QueryRow {
        let start = std::time::Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;
        let trimmed = sql.trim();

        if trimmed.eq_ignore_ascii_case("commit") {
            return match this.conn.commit() {
                Ok(_) => QueryRow::notice("COMMIT", elapsed(), max_rows),
                Err(e) => QueryRow::error(e.to_string(), elapsed(), 0, max_rows),
            };
        }

        if trimmed.eq_ignore_ascii_case("rollback") {
            return match this.conn.rollback() {
                Ok(_) => QueryRow::notice("ROLLBACK", elapsed(), max_rows),
                Err(e) => QueryRow::error(e.to_string(), elapsed(), 0, max_rows),
            };
        }

        if is_query_sql(trimmed) {
            return match Self::do_query(this, sql, max_rows, None) {
                Ok(data) => QueryRow::from_query(data, elapsed(), 0, max_rows),
                Err(e) => QueryRow::error(clean_error(&e), elapsed(), 0, max_rows),
            };
        }

        match this.conn.execute(sql, &[]) {
            Ok(stmt) => {
                let affected = stmt.row_count().unwrap_or(0);
                let mut row = QueryRow::notice(
                    &format!("Statement executed. Rows affected: {}", affected),
                    elapsed(),
                    max_rows,
                );
                row.row_count = affected as usize;
                row.rows_affected = Some(affected);
                row
            }
            Err(e) => QueryRow::error(clean_error(&e), elapsed(), 0, max_rows),
        }
    }

    pub fn execute_query_paged(
        this: &Self,
        sql: &str,
        page_offset: usize,
        page_size: usize,
    ) -> QueryRow {
        let start = std::time::Instant::now();
        let inner_sql = sql.trim().trim_end_matches(';').trim();
        // Fetch one extra row so we can tell whether more data follows.
        let fetch_limit = page_offset + page_size + 1;

        let paged_sql = if strip_leading_comments(inner_sql)
            .to_uppercase()
            .starts_with("WITH ")
        {
            format!(
                "{} OFFSET {} ROWS FETCH NEXT {} ROWS ONLY",
                inner_sql,
                page_offset,
                page_size + 1
            )
        } else {
            format!(
                "SELECT * FROM (SELECT a.*, ROWNUM AS rn FROM ({}) a WHERE ROWNUM <= {}) WHERE rn > {}",
                inner_sql, fetch_limit, page_offset
            )
        };

        match Self::do_query(this, &paged_sql, page_size, Some("RN")) {
            Ok(data) => QueryRow::from_query(
                data,
                start.elapsed().as_millis() as u64,
                page_offset,
                page_size,
            ),
            Err(e) => QueryRow::error(
                clean_error(&e),
                start.elapsed().as_millis() as u64,
                page_offset,
                page_size,
            ),
        }
    }

    /// Run a query and stringify up to `max_rows` (+ one look-ahead row used to
    /// set `truncated`). When `skip_column` is given (ROWNUM pagination helper),
    /// that column is dropped from the output.
    fn do_query(
        this: &Self,
        sql: &str,
        max_rows: usize,
        skip_column: Option<&str>,
    ) -> Result<QueryData, oracle::Error> {
        let mut stmt = this.conn.statement(sql).build()?;
        let rows = stmt.query(&[])?;

        let col_info = rows.column_info();
        let all_names: Vec<String> = col_info.iter().map(|c| c.name().to_string()).collect();
        if all_names.is_empty() {
            return Ok((vec!["Result".into()], vec![], false, 0, 0));
        }

        let skip_idx = skip_column.and_then(|n| all_names.iter().position(|c| c == n));
        let col_names: Vec<String> = all_names
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != skip_idx)
            .map(|(_, n)| n.clone())
            .collect();

        let mut result_rows: Vec<Vec<String>> = Vec::new();
        let mut truncated = false;
        let mut byte_count = 0usize;
        let mut fetch_error = None;

        for row_result in rows {
            if result_rows.len() > max_rows {
                truncated = true;
                break;
            }
            match row_result {
                Ok(row) => {
                    let mut row_vals = Vec::new();
                    for i in 0..all_names.len() {
                        if Some(i) == skip_idx {
                            continue;
                        }
                        let val: Result<Option<String>, _> = row.get(i);
                        match val {
                            Ok(Some(s)) => {
                                byte_count += s.len();
                                row_vals.push(s)
                            }
                            Ok(None) => row_vals.push(this.null_display.clone()),
                            Err(_) => row_vals.push("(ERR)".into()),
                        }
                    }
                    result_rows.push(row_vals);
                }
                Err(e) => {
                    fetch_error = Some(e);
                    break;
                }
            }
        }

        if result_rows.is_empty() {
            if let Some(e) = fetch_error {
                return Err(e);
            }
        }

        // Drop the look-ahead row, adjusting byte_count to match.
        if result_rows.len() > max_rows {
            truncated = true;
            if let Some(extra_row) = result_rows.pop() {
                for val in extra_row {
                    byte_count = byte_count.saturating_sub(val.len());
                }
            }
        }

        let total_fetched = result_rows.len();
        Ok((col_names, result_rows, truncated, total_fetched, byte_count))
    }

    pub fn cancel_query(this: &Self) -> anyhow::Result<()> {
        this.conn
            .break_execution()
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    pub fn query_v_session(this: &Self) -> Result<Vec<DbSessionInfo>, anyhow::Error> {
        let sql = r#"
            SELECT s.sid, s.serial#, s.username, s.status, s.osuser,
                   s.machine, s.program, s.sql_id, s.prev_sql_id,
                   TO_CHAR(s.logon_time, 'YYYY-MM-DD HH24:MI:SS') AS logon_time
            FROM v$session s
            WHERE s.type != 'BACKGROUND'
              AND s.sid != SYS_CONTEXT('USERENV', 'SID')
            ORDER BY s.logon_time DESC
        "#;
        let mut stmt = this.conn.statement(sql).build()?;
        let rows = stmt.query(&[])?;
        let mut sessions = Vec::new();

        for row_result in rows {
            let row = row_result?;
            sessions.push(DbSessionInfo {
                sid: row.get("SID").unwrap_or(0),
                serial: row.get("SERIAL#").unwrap_or(0),
                username: row.get("USERNAME").unwrap_or_default(),
                status: row.get("STATUS").unwrap_or_default(),
                osuser: row.get("OSUSER").unwrap_or_default(),
                machine: row.get("MACHINE").unwrap_or_default(),
                program: row.get("PROGRAM").unwrap_or_default(),
                sql_id: row.get("SQL_ID").unwrap_or_default(),
                prev_sql_id: row.get("PREV_SQL_ID").unwrap_or_default(),
                logon_time: row.get("LOGON_TIME").unwrap_or_default(),
            });
        }

        Ok(sessions)
    }
}

#[derive(Debug, Clone, Default)]
pub struct DbSessionInfo {
    pub sid: i32,
    pub serial: i32,
    pub username: String,
    pub status: String,
    pub osuser: String,
    pub machine: String,
    pub program: String,
    pub sql_id: String,
    pub prev_sql_id: String,
    pub logon_time: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_leading_comments_for_query_detection() {
        assert!(is_query_sql("-- note\nselect 1 from dual"));
        assert!(is_query_sql("/* hi */ with x as (select 1 from dual) select * from x"));
        assert!(is_query_sql(
            "-- a\n-- b\n/* c; */\nselect * from all_tables;"
        ));
        assert!(!is_query_sql("-- note\nupdate t set x = 1"));
        assert!(strip_leading_comments("-- only comment").is_empty());
        assert!(strip_leading_comments("/* unterminated").is_empty());
        assert_eq!(
            strip_leading_comments("-- ; --\nselect 1"),
            "select 1"
        );
    }
}
