use oracle::Connection;
use std::sync::Arc;

pub struct OracleConnection {
    pub conn: Connection,
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

impl OracleConnection {
    pub fn connect(connect_string: &str, user: &str, password: &str) -> Result<Arc<Self>, anyhow::Error> {
        let conn = Connection::connect(user, password, connect_string)?;
        Ok(Arc::new(Self { conn }))
    }

    pub fn execute_query(this: &Self, sql: &str, max_rows: usize) -> QueryRow {
        let start = std::time::Instant::now();
        let trimmed = sql.trim();

        if trimmed.eq_ignore_ascii_case("commit") {
            return match this.conn.commit() {
                Ok(_) => QueryRow {
                    columns: vec!["Result".into()],
                    rows: vec![vec!["COMMIT".into()]],
                    truncated: false,
                    row_count: 1,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    is_error: false,
                    error_msg: None,
                    rows_affected: None,
                    page_offset: 0,
                    page_size: max_rows,
                    byte_count: 0,
                    total_fetched: 0,
                },
                Err(e) => QueryRow {
                    columns: vec![],
                    rows: vec![],
                    truncated: false,
                    row_count: 0,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    is_error: true,
                    error_msg: Some(e.to_string()),
                    rows_affected: None,
                    page_offset: 0,
                    page_size: max_rows,
                    byte_count: 0,
                    total_fetched: 0,
                },
            };
        }

        if trimmed.eq_ignore_ascii_case("rollback") {
            return match this.conn.rollback() {
                Ok(_) => QueryRow {
                    columns: vec!["Result".into()],
                    rows: vec![vec!["ROLLBACK".into()]],
                    truncated: false,
                    row_count: 1,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    is_error: false,
                    error_msg: None,
                    rows_affected: None,
                    page_offset: 0,
                    page_size: max_rows,
                    byte_count: 0,
                    total_fetched: 0,
                },
                Err(e) => QueryRow {
                    columns: vec![],
                    rows: vec![],
                    truncated: false,
                    row_count: 0,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    is_error: true,
                    error_msg: Some(e.to_string()),
                    rows_affected: None,
                    page_offset: 0,
                    page_size: max_rows,
                    byte_count: 0,
                    total_fetched: 0,
                },
            };
        }

        let is_query = trimmed.to_uppercase().starts_with("SELECT")
            || trimmed.to_uppercase().starts_with("WITH")
            || trimmed.to_uppercase().starts_with("DESCRIBE")
            || trimmed.to_uppercase().starts_with("EXPLAIN");

        if is_query {
            match Self::do_query(this, sql, max_rows, 0) {
                Ok((cols, rows, truncated, total_fetched, byte_count)) => QueryRow {
                    columns: cols,
                    rows,
                    truncated,
                    row_count: total_fetched,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    rows_affected: None,
                    is_error: false,
                    error_msg: None,
                    page_offset: 0,
                    page_size: max_rows,
                    byte_count,
                    total_fetched,
                },
                Err(e) => {
                    let msg = e.to_string();
                    let clean = if msg.contains("ORA-01013") || msg.to_lowercase().contains("cancel") {
                        "Query cancelled".into()
                    } else {
                        msg
                    };
                    QueryRow {
                    columns: vec![],
                    rows: vec![],
                    truncated: false,
                    row_count: 0,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    is_error: true,
                    error_msg: Some(clean),
                    rows_affected: None,
                    page_offset: 0,
                    page_size: max_rows,
                    byte_count: 0,
                    total_fetched: 0,
                }},
            }
        } else {
            match this.conn.execute(sql, &[]) {
                Ok(stmt) => {
                    let affected = stmt.row_count().unwrap_or(0);
                    QueryRow {
                        columns: vec!["Result".into()],
                        rows: vec![vec![format!("Statement executed. Rows affected: {}", affected)]],
                        truncated: false,
                        row_count: affected as usize,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        is_error: false,
                        error_msg: None,
                        rows_affected: Some(affected),
                        page_offset: 0,
                        page_size: max_rows,
                    byte_count: 0,
                    total_fetched: 0,
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    let clean = if msg.contains("ORA-01013") || msg.to_lowercase().contains("cancel") {
                        "Query cancelled".into()
                    } else {
                        msg
                    };
                    QueryRow {
                    columns: vec![],
                    rows: vec![],
                    truncated: false,
                    row_count: 0,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    is_error: true,
                    error_msg: Some(clean),
                    rows_affected: None,
                    page_offset: 0,
                    page_size: max_rows,
                    byte_count: 0,
                    total_fetched: 0,
                }},
            }
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
        let end_row = page_offset + page_size;
        let fetch_limit = end_row + 1;

        let paged_sql = if inner_sql.to_uppercase().starts_with("WITH ") {
            format!(
                "{} OFFSET {} ROWS FETCH NEXT {} ROWS ONLY",
                inner_sql, page_offset, page_size + 1
            )
        } else {
            format!(
                "SELECT * FROM (SELECT a.*, ROWNUM AS rn FROM ({}) a WHERE ROWNUM <= {}) WHERE rn > {}",
                inner_sql, fetch_limit, page_offset
            )
        };

        match Self::do_query_paged(this, &paged_sql, page_size) {
            Ok((cols, rows, truncated, total_fetched, byte_count)) => QueryRow {
                columns: cols,
                rows,
                truncated,
                row_count: total_fetched,
                elapsed_ms: start.elapsed().as_millis() as u64,
                rows_affected: None,
                is_error: false,
                error_msg: None,
                page_offset,
                page_size,
                byte_count,
                total_fetched,
            },
            Err(e) => {
                let msg = e.to_string();
                let clean = if msg.contains("ORA-01013") || msg.to_lowercase().contains("cancel") {
                    "Query cancelled".into()
                } else {
                    msg
                };
                QueryRow {
                columns: vec![],
                rows: vec![],
                truncated: false,
                row_count: 0,
                elapsed_ms: start.elapsed().as_millis() as u64,
                is_error: true,
                error_msg: Some(clean),
                rows_affected: None,
                page_offset,
                page_size,
                    byte_count: 0,
                    total_fetched: 0,
            }
        },
        }
    }
    fn do_query(
        this: &Self,
        sql: &str,
        max_rows: usize,
        _page_offset: usize,
    ) -> Result<(Vec<String>, Vec<Vec<String>>, bool, usize, usize), oracle::Error> {
        let mut stmt = this.conn.statement(sql).build()?;
        let rows = stmt.query(&[])?;

        let col_info = rows.column_info();
        let col_names: Vec<String> = col_info.iter().map(|c| c.name().to_string()).collect();
        if col_names.is_empty() {
            return Ok((vec!["Result".into()], vec![], false, 0, 0));
        }

        let mut result_rows: Vec<Vec<String>> = Vec::new();
        let mut truncated = false;
        let mut byte_count = 0usize;
        let mut fetch_error = None;

        for row_result in rows {
            if result_rows.len() >= max_rows + 1 {
                truncated = true;
                break;
            }
            match row_result {
                Ok(row) => {
                    let mut row_vals = Vec::new();
                    for i in 0..col_names.len() {
                        let val: Result<Option<String>, _> = row.get(i);
                        match val {
                            Ok(Some(s)) => {
                                byte_count += s.len();
                                row_vals.push(s)
                            },
                            Ok(None) => row_vals.push("(NULL)".into()),
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

        if result_rows.is_empty() && fetch_error.is_some() {
            return Err(fetch_error.unwrap());
        }

        if result_rows.len() > max_rows {
            truncated = true;
            result_rows.truncate(max_rows);
        }

        let total_fetched = result_rows.len();
        Ok((col_names, result_rows, truncated, total_fetched, byte_count))
    }

    fn do_query_paged(
        this: &Self,
        sql: &str,
        max_rows: usize,
    ) -> Result<(Vec<String>, Vec<Vec<String>>, bool, usize, usize), oracle::Error> {
        let mut stmt = this.conn.statement(sql).build()?;
        let rows = stmt.query(&[])?;

        let col_info = rows.column_info();
        let all_names: Vec<String> = col_info.iter().map(|c| c.name().to_string()).collect();
        if all_names.is_empty() {
            return Ok((vec!["Result".into()], vec![], false, 0, 0));
        }

        let col_names: Vec<String> = all_names
            .iter()
            .filter(|n| *n != "RN")
            .cloned()
            .collect();
        let rn_idx = all_names.iter().position(|n| n == "RN");

        let mut result_rows: Vec<Vec<String>> = Vec::new();
        let mut truncated = false;
        let mut byte_count = 0usize;
        let mut fetch_error = None;

        for row_result in rows {
            if result_rows.len() >= max_rows + 1 {
                truncated = true;
                break;
            }
            match row_result {
                Ok(row) => {
                    let mut row_vals = Vec::new();
                    for i in 0..all_names.len() {
                        if Some(i) == rn_idx {
                            continue;
                        }
                        let val: Result<Option<String>, _> = row.get(i);
                        match val {
                            Ok(Some(s)) => {
                                byte_count += s.len();
                                row_vals.push(s)
                            },
                            Ok(None) => row_vals.push("(NULL)".into()),
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

        if result_rows.is_empty() && fetch_error.is_some() {
            return Err(fetch_error.unwrap());
        }

        // If we read max_rows + 1 rows, there are more - truncate and mark as truncated
        if result_rows.len() > max_rows {
            truncated = true;
            // Remove the extra row and adjust byte_count
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
        this.conn.break_execution().map_err(|e| anyhow::anyhow!("{}", e))
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
