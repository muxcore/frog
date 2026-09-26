use oracle::Connection;
use std::str::FromStr;
use std::sync::Arc;

/// Which database backend a session talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DbType {
    #[default]
    Oracle,
    Postgres,
}

impl std::fmt::Display for DbType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbType::Oracle => write!(f, "oracle"),
            DbType::Postgres => write!(f, "postgres"),
        }
    }
}

impl FromStr for DbType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "oracle" => Ok(DbType::Oracle),
            "postgres" | "postgresql" | "pg" => Ok(DbType::Postgres),
            other => Err(format!(
                "unknown db type '{}' (expected 'oracle' or 'postgres')",
                other
            )),
        }
    }
}

impl DbType {
    /// Default TCP port for the backend.
    pub fn default_port(self) -> u16 {
        match self {
            DbType::Oracle => 1521,
            DbType::Postgres => 5432,
        }
    }

    /// Short label used in session names / status UI.
    pub fn short_label(self) -> &'static str {
        match self {
            DbType::Oracle => "ora",
            DbType::Postgres => "pg",
        }
    }
}

/// Backend-agnostic database connection. Implemented by Oracle and Postgres
/// connections so sessions can hold either behind an `Arc<dyn DbConnection>`.
pub trait DbConnection: Send + Sync {
    fn execute_query(&self, sql: &str, max_rows: usize) -> QueryRow;
    fn execute_query_paged(&self, sql: &str, page_offset: usize, page_size: usize) -> QueryRow;
    fn cancel_query(&self) -> anyhow::Result<()>;
    fn close(&self) -> anyhow::Result<()>;
    fn query_sessions(&self) -> anyhow::Result<Vec<DbSessionInfo>>;
    fn db_type(&self) -> DbType;
    /// Full text of the SQL currently executed by the given session entry
    /// (TOAD-style session browser detail). Errors when the session is idle
    /// or its SQL text is not visible to us.
    fn session_sql(&self, info: &DbSessionInfo) -> anyhow::Result<String>;
    /// Explain plan for an arbitrary SQL statement, as a displayable result.
    fn explain_plan(&self, sql: &str, max_rows: usize) -> QueryRow;
}

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
    pub(crate) fn error(msg: String, elapsed_ms: u64, page_offset: usize, page_size: usize) -> Self {
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

    pub(crate) fn from_query(
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

    pub(crate) fn notice(text: &str, elapsed_ms: u64, page_size: usize) -> Self {
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
pub(crate) type QueryData = (Vec<String>, Vec<Vec<String>>, bool, usize, usize);

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
    is_query_sql_for(DbType::Oracle, trimmed)
}

pub fn is_query_sql_for(db_type: DbType, trimmed: &str) -> bool {
    let upper = strip_leading_comments(trimmed).to_uppercase();
    match db_type {
        DbType::Oracle => {
            upper.starts_with("SELECT")
                || upper.starts_with("WITH")
                || upper.starts_with("DESCRIBE")
                || upper.starts_with("EXPLAIN")
        }
        DbType::Postgres => {
            upper.starts_with("SELECT")
                || upper.starts_with("WITH")
                || upper.starts_with("VALUES")
                || upper.starts_with("TABLE")
                || upper.starts_with("SHOW")
                || upper.starts_with("DESCRIBE")
                || upper.starts_with("EXPLAIN")
                // INSERT/UPDATE/DELETE ... RETURNING yields rows in Postgres.
                || upper.contains("RETURNING")
        }
    }
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
    pub fn close(&self) -> anyhow::Result<()> {
        self.conn.close().map_err(|e| anyhow::anyhow!("{}", e))
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

    /// Map an F2 browser query failure to a message. Missing dictionary
    /// privileges (the usual cause of an empty F2) get a GRANT hint.
    pub fn friendly_session_error(msg: &str) -> String {
        if msg.contains("ORA-00942") || msg.contains("ORA-01031") {
            format!(
                "{}\n\nHint: F2 needs SELECT on V_$SESSION (and V_$SQL for SQL text). \
                 Ask your DBA for e.g.:\n  GRANT SELECT_CATALOG_ROLE TO <your_user>;\n\
                 -- or minimally:\n  GRANT SELECT ON V_$SESSION TO <your_user>;\n  \
                 GRANT SELECT ON V_$SQL TO <your_user>;",
                msg
            )
        } else {
            msg.to_string()
        }
    }

    pub fn execute_query(&self, sql: &str, max_rows: usize) -> QueryRow {
        let start = std::time::Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;
        let trimmed = sql.trim();

        if trimmed.eq_ignore_ascii_case("commit") {
            return match self.conn.commit() {
                Ok(_) => QueryRow::notice("COMMIT", elapsed(), max_rows),
                Err(e) => QueryRow::error(e.to_string(), elapsed(), 0, max_rows),
            };
        }

        if trimmed.eq_ignore_ascii_case("rollback") {
            return match self.conn.rollback() {
                Ok(_) => QueryRow::notice("ROLLBACK", elapsed(), max_rows),
                Err(e) => QueryRow::error(e.to_string(), elapsed(), 0, max_rows),
            };
        }

        if is_query_sql(trimmed) {
            return match Self::do_query(self, sql, max_rows, None) {
                Ok(data) => QueryRow::from_query(data, elapsed(), 0, max_rows),
                Err(e) => QueryRow::error(clean_error(&e), elapsed(), 0, max_rows),
            };
        }

        match self.conn.execute(sql, &[]) {
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
        &self,
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

        match Self::do_query(self, &paged_sql, page_size, Some("RN")) {
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

    pub fn cancel_query(&self) -> anyhow::Result<()> {
        self.conn
            .break_execution()
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    pub fn query_v_session(&self) -> Result<Vec<DbSessionInfo>, anyhow::Error> {
        let sql = r#"
            SELECT s.sid, s.serial#, s.username, s.status, s.osuser,
                   s.machine, s.program, s.sql_id, s.prev_sql_id,
                   TO_CHAR(s.logon_time, 'YYYY-MM-DD HH24:MI:SS') AS logon_time
            FROM v$session s
            WHERE s.type != 'BACKGROUND'
              AND s.sid != SYS_CONTEXT('USERENV', 'SID')
            ORDER BY s.logon_time DESC
        "#;
        let mut stmt = self.conn.statement(sql).build()?;
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

    /// Full SQL text for a session entry, resolved through `V$SQL` by SQL_ID.
    pub fn session_sql_text(&self, info: &DbSessionInfo) -> anyhow::Result<String> {
        let id = info.sql_id.trim();
        if id.is_empty() {
            return Err(anyhow::anyhow!(
                "session {}/{} has no current SQL (idle)",
                info.sid,
                info.serial
            ));
        }
        if !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(anyhow::anyhow!("unexpected SQL_ID value"));
        }
        let sql = format!(
            "SELECT SQL_FULLTEXT, SQL_TEXT FROM V$SQL WHERE SQL_ID = '{}' AND ROWNUM = 1",
            id
        );
        let mut stmt = self
            .conn
            .statement(&sql)
            .build()
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let rows = stmt.query(&[]).map_err(|e| anyhow::anyhow!("{}", e))?;
        for row_result in rows {
            let row = row_result.map_err(|e| anyhow::anyhow!("{}", e))?;
            // SQL_FULLTEXT is a CLOB; fall back to the (1000-char) SQL_TEXT.
            let full: Option<String> = row.get("SQL_FULLTEXT").unwrap_or(None);
            if let Some(t) = full.filter(|t| !t.trim().is_empty()) {
                return Ok(t);
            }
            let short: Option<String> = row.get("SQL_TEXT").unwrap_or(None);
            if let Some(t) = short.filter(|t| !t.trim().is_empty()) {
                return Ok(t);
            }
        }
        Err(anyhow::anyhow!(
            "SQL text for SQL_ID '{}' not found in V$SQL (aged out or no access)",
            id
        ))
    }

    /// Explain plan via `EXPLAIN PLAN` + `DBMS_XPLAN.DISPLAY`, using a unique
    /// statement id so concurrent frog explains don't clobber each other.
    pub fn explain_plan_for(&self, sql: &str, max_rows: usize) -> QueryRow {
        let start = std::time::Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;
        let inner = sql.trim().trim_end_matches(';').trim();
        if inner.is_empty() {
            return QueryRow::error("Nothing to explain.".into(), elapsed(), 0, max_rows);
        }
        let stmt_id = oracle_statement_id();
        let explain_sql = oracle_explain_sql(inner, &stmt_id);
        if let Err(e) = self.conn.execute(&explain_sql, &[]) {
            return QueryRow::error(clean_error(&e), elapsed(), 0, max_rows);
        }
        let display_sql = format!(
            "SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY(NULL, '{}', 'TYPICAL'))",
            stmt_id
        );
        let result = match Self::do_query(self, &display_sql, max_rows, None) {
            Ok(data) => QueryRow::from_query(data, elapsed(), 0, max_rows),
            Err(e) => QueryRow::error(clean_error(&e), elapsed(), 0, max_rows),
        };
        // Best-effort cleanup so PLAN_TABLE doesn't fill up.
        let _ = self.conn.execute(
            &format!(
                "DELETE FROM PLAN_TABLE WHERE STATEMENT_ID = '{}'",
                stmt_id
            ),
            &[],
        );
        result
    }
}

/// Unique `EXPLAIN PLAN` statement id (`FROG_<pid>_<counter>`, ≤ 30 chars).
pub(crate) fn oracle_statement_id() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("FROG{}_{}", std::process::id() % 100000, n % 1000000)
}

/// Build the `EXPLAIN PLAN ... FOR <sql>` statement (pure, testable).
pub(crate) fn oracle_explain_sql(inner_sql: &str, stmt_id: &str) -> String {
    format!(
        "EXPLAIN PLAN SET STATEMENT_ID = '{}' FOR {}",
        stmt_id, inner_sql
    )
}

impl DbConnection for OracleConnection {
    fn execute_query(&self, sql: &str, max_rows: usize) -> QueryRow {
        OracleConnection::execute_query(self, sql, max_rows)
    }
    fn execute_query_paged(&self, sql: &str, page_offset: usize, page_size: usize) -> QueryRow {
        OracleConnection::execute_query_paged(self, sql, page_offset, page_size)
    }
    fn cancel_query(&self) -> anyhow::Result<()> {
        OracleConnection::cancel_query(self)
    }
    fn close(&self) -> anyhow::Result<()> {
        OracleConnection::close(self)
    }
    fn query_sessions(&self) -> anyhow::Result<Vec<DbSessionInfo>> {
        self.query_v_session()
    }
    fn session_sql(&self, info: &DbSessionInfo) -> anyhow::Result<String> {
        self.session_sql_text(info)
    }
    fn explain_plan(&self, sql: &str, max_rows: usize) -> QueryRow {
        self.explain_plan_for(sql, max_rows)
    }
    fn db_type(&self) -> DbType {
        DbType::Oracle
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

    #[test]
    fn oracle_explain_builders() {
        let id = oracle_statement_id();
        assert!(id.starts_with("FROG"));
        assert!(id.len() <= 30);
        assert!(id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
        // Uniqueness within a process.
        assert_ne!(id, oracle_statement_id());
        let stmt = oracle_explain_sql("select 1 from dual", "FROG1_2");
        assert_eq!(
            stmt,
            "EXPLAIN PLAN SET STATEMENT_ID = 'FROG1_2' FOR select 1 from dual"
        );
    }

    #[test]
    fn session_error_maps_missing_privileges() {
        let msg = OracleConnection::friendly_session_error("ORA-00942: table or view does not exist");
        assert!(msg.contains("ORA-00942"));
        assert!(msg.contains("GRANT SELECT_CATALOG_ROLE"));
        assert!(msg.contains("V_$SESSION"));
        let msg = OracleConnection::friendly_session_error("ORA-01031: insufficient privileges");
        assert!(msg.contains("GRANT SELECT ON V_$SQL"));
        // Unrelated errors pass through untouched.
        let plain = "ORA-12170: TNS:Connect timeout occurred";
        assert_eq!(OracleConnection::friendly_session_error(plain), plain);
    }
}
