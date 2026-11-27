use crate::db::ddl::{
    format_oracle_col_type, format_rich_create_table, DdlTarget, DescribedObject, RichColumn,
    TableConstraintDdl,
};
use crate::db::perf::PerfSnapshot;
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
}

/// Column metadata for the F12 DB explorer tree and completion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
}

/// Kind of a dictionary object, kept small on purpose: this is what the
/// completion popup shows as a tag and uses for ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaObjectKind {
    Table,
    View,
    MView,
    Synonym,
    Sequence,
    Procedure,
    Function,
    Package,
    Type,
    Other,
}

impl MetaObjectKind {
    pub fn tag(self) -> &'static str {
        match self {
            Self::Table => "T",
            Self::View => "V",
            Self::MView => "M",
            Self::Synonym => "S",
            Self::Sequence => "Q",
            Self::Procedure => "P",
            Self::Function => "F",
            Self::Package => "K",
            Self::Type => "Y",
            Self::Other => "?",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::View => "view",
            Self::MView => "materialized view",
            Self::Synonym => "synonym",
            Self::Sequence => "sequence",
            Self::Procedure => "procedure",
            Self::Function => "function",
            Self::Package => "package",
            Self::Type => "type",
            Self::Other => "object",
        }
    }

    pub fn from_oracle(object_type: &str) -> Self {
        match object_type.to_ascii_uppercase().as_str() {
            "TABLE" | "FOREIGN TABLE" => Self::Table,
            "VIEW" => Self::View,
            "MATERIALIZED VIEW" => Self::MView,
            "SYNONYM" => Self::Synonym,
            "SEQUENCE" => Self::Sequence,
            "PROCEDURE" => Self::Procedure,
            "FUNCTION" => Self::Function,
            "PACKAGE" | "PACKAGE BODY" => Self::Package,
            "TYPE" => Self::Type,
            _ => Self::Other,
        }
    }

    /// True for objects that can appear in a FROM clause.
    pub fn is_relation(self) -> bool {
        matches!(self, Self::Table | Self::View | Self::MView | Self::Synonym)
    }
}

/// One dictionary object, ready for completion. `schema` is the resolved owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaObject {
    pub schema: String,
    pub name: String,
    pub kind: MetaObjectKind,
}

/// A table/view reference parsed out of the current SQL statement.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct TableRef {
    pub schema: Option<String>,
    pub schema_quoted: bool,
    pub name: String,
    pub quoted: bool,
    pub alias: Option<String>,
}

impl TableRef {
    pub fn qualified_name(&self) -> String {
        match &self.schema {
            Some(s) => format!("{}.{}", s, self.name),
            None => self.name.clone(),
        }
    }
}

/// Columns resolved for one table reference. `schema`/`table` are the
/// dictionary spellings, which may differ from the query spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableColumns {
    pub schema: String,
    pub table: String,
    pub columns: Vec<ColumnInfo>,
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
    /// F12 DB explorer: schemas/users visible to us (capped for big DBs).
    fn list_schemas(&self) -> anyhow::Result<Vec<String>>;
    /// F12 DB explorer: type-group folders present in one schema, as
    /// (GROUP_KEY, object_count) — only non-empty groups. Keys: TABLES,
    /// VIEWS, MVIEWS, FOREIGN, INDEXES, SEQUENCES, FUNCTIONS, PROCEDURES,
    /// PACKAGES, TRIGGERS, TYPES.
    fn list_object_groups(&self, schema: &str) -> anyhow::Result<Vec<(String, u64)>>;
    /// F12 DB explorer: objects of one type group as (object_name, kind)
    /// where kind is e.g. "TABLE", "VIEW", "MVIEW", "INDEX", "PROCEDURE".
    fn list_objects(&self, schema: &str, group: &str) -> anyhow::Result<Vec<(String, String)>>;
    /// F12 DB explorer: columns of a table-like object or index, in order.
    fn list_columns(&self, schema: &str, name: &str, kind: &str)
        -> anyhow::Result<Vec<ColumnInfo>>;
    /// F12 DB explorer: DDL/source text for any object (tables synthesize
    /// CREATE TABLE from columns; views/procedures read the dictionary).
    fn object_ddl(&self, schema: &str, name: &str, kind: &str) -> anyhow::Result<String>;
    /// F4 Toad-style Describe: resolve `[schema.]object` across tables, indexes,
    /// views, mviews, sequences, packages, routines, triggers, types and synonyms,
    /// returning its resolved metadata and full DDL script.
    fn describe_object(&self, target: &DdlTarget) -> anyhow::Result<DescribedObject>;
    /// F6 Performance dashboard: lightweight health snapshot (sessions, locks,
    /// wait breakdown, memory & cache hit ratio).
    fn collect_perf_snapshot(&self) -> anyhow::Result<PerfSnapshot>;

    // -- Completion metadata (cached by MetadataStore; never called on the
    // -- keystroke path) ----------------------------------------------------

    /// The schema unqualified names resolve in. Oracle: `CURRENT_SCHEMA`
    /// (which `ALTER SESSION` can change, unlike `USER`). Postgres: the first
    /// entry of `search_path`.
    fn current_schema(&self) -> anyhow::Result<String>;

    /// All completion-relevant objects of one schema in a single query.
    /// (Calling `list_objects` once per type folder would be ~10 round trips.)
    fn list_schema_objects(&self, schema: &str) -> anyhow::Result<Vec<MetaObject>>;

    /// Columns for a small set of table references, batched into as few
    /// round trips as possible. Returns an entry (possibly empty columns) for
    /// every requested reference that could be resolved.
    fn table_columns_batch(
        &self,
        refs: &[TableRef],
        current_schema: &str,
    ) -> anyhow::Result<Vec<TableColumns>>;
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
    pub(crate) fn error(
        msg: String,
        elapsed_ms: u64,
        page_offset: usize,
        page_size: usize,
    ) -> Self {
        Self {
            columns: vec![],
            rows: vec![],
            truncated: false,
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

/// Assemble `CREATE TABLE <qualified> (...)` from column metadata. `quote`
/// wraps identifiers (identity on Oracle, double quotes on Postgres).
#[allow(dead_code)]
pub(crate) fn format_create_table(
    qualified: &str,
    cols: &[ColumnInfo],
    quote: impl Fn(&str) -> String,
) -> String {
    let mut ddl = format!("CREATE TABLE {} (\n", qualified);
    for (i, c) in cols.iter().enumerate() {
        ddl.push_str(&format!(
            "  {} {}{}",
            quote(&c.name),
            if c.data_type.is_empty() {
                "?"
            } else {
                &c.data_type
            },
            if c.nullable { "" } else { " NOT NULL" }
        ));
        if i + 1 < cols.len() {
            ddl.push(',');
        }
        ddl.push('\n');
    }
    ddl.push_str(");");
    ddl
}

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

fn starts_with_keyword(upper: &str, kw: &str) -> bool {
    match upper.strip_prefix(kw) {
        Some(rest) => rest
            .chars()
            .next()
            .map(|c| !c.is_alphanumeric() && !matches!(c, '_' | '$' | '#'))
            .unwrap_or(true),
        None => false,
    }
}

/// Strip leading comments/whitespace and trailing `;`/whitespace for command matching.
pub(crate) fn clean_command_word(sql: &str) -> &str {
    strip_leading_comments(sql).trim_end().trim_end_matches(';').trim()
}

fn is_query_sql(trimmed: &str) -> bool {
    is_query_sql_for(DbType::Oracle, trimmed)
}

pub fn is_query_sql_for(db_type: DbType, trimmed: &str) -> bool {
    let upper = strip_leading_comments(trimmed).to_uppercase();
    match db_type {
        DbType::Oracle => {
            starts_with_keyword(&upper, "SELECT")
                || starts_with_keyword(&upper, "WITH")
                || starts_with_keyword(&upper, "DESCRIBE")
                || starts_with_keyword(&upper, "EXPLAIN")
        }
        DbType::Postgres => {
            starts_with_keyword(&upper, "SELECT")
                || starts_with_keyword(&upper, "WITH")
                || starts_with_keyword(&upper, "VALUES")
                || starts_with_keyword(&upper, "TABLE")
                || starts_with_keyword(&upper, "SHOW")
                || starts_with_keyword(&upper, "DESCRIBE")
                || starts_with_keyword(&upper, "EXPLAIN")
                // INSERT/UPDATE/DELETE ... RETURNING yields rows in Postgres.
                || upper.contains("RETURNING")
        }
    }
}

/// Whether `sql` can safely be wrapped in a subquery / `OFFSET ... FETCH` to
/// load subsequent pages. Unlike [`is_query_sql_for`], DML with `RETURNING`,
/// `EXPLAIN`, `SHOW`, and `DESCRIBE` are excluded so scrolling never re-runs
/// mutations or wraps non-subquery commands in `SELECT * FROM (...)`.
pub fn is_paginatable_query_for(db_type: DbType, trimmed: &str) -> bool {
    let upper = strip_leading_comments(trimmed).to_uppercase();
    match db_type {
        DbType::Oracle => {
            starts_with_keyword(&upper, "SELECT") || starts_with_keyword(&upper, "WITH")
        }
        DbType::Postgres => {
            starts_with_keyword(&upper, "SELECT")
                || starts_with_keyword(&upper, "WITH")
                || starts_with_keyword(&upper, "VALUES")
                || starts_with_keyword(&upper, "TABLE")
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
        let cmd = clean_command_word(sql);

        if cmd.eq_ignore_ascii_case("commit") {
            return match self.conn.commit() {
                Ok(_) => QueryRow::notice("COMMIT", elapsed(), max_rows),
                Err(e) => QueryRow::error(e.to_string(), elapsed(), 0, max_rows),
            };
        }

        if cmd.eq_ignore_ascii_case("rollback") {
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
                row.rows_affected = Some(affected);
                row
            }
            Err(e) => QueryRow::error(clean_error(&e), elapsed(), 0, max_rows),
        }
    }

    pub fn execute_query_paged(&self, sql: &str, page_offset: usize, page_size: usize) -> QueryRow {
        let start = std::time::Instant::now();
        let (paged_sql, skip_col) = oracle_paged_sql(sql, page_offset, page_size);

        match Self::do_query(self, &paged_sql, page_size, skip_col) {
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
            SELECT s.sid, s.serial#, s.username, s.status,
                   s.machine, s.program, s.sql_id,
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
                machine: row.get("MACHINE").unwrap_or_default(),
                program: row.get("PROGRAM").unwrap_or_default(),
                sql_id: row.get("SQL_ID").unwrap_or_default(),
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

    /// F12 explorer: schemas visible to us, capped so huge DBs stay usable.
    pub fn list_schemas(&self) -> anyhow::Result<Vec<String>> {
        let mut stmt = self
            .conn
            .statement("SELECT USERNAME FROM ALL_USERS ORDER BY USERNAME")
            .build()
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let rows = stmt.query(&[]).map_err(|e| anyhow::anyhow!("{}", e))?;
        let mut out = Vec::new();
        for row_result in rows {
            let row = row_result.map_err(|e| anyhow::anyhow!("{}", e))?;
            let name: String = row.get(0).unwrap_or_default();
            if !name.trim().is_empty() {
                out.push(name);
            }
            if out.len() >= 500 {
                break;
            }
        }
        Ok(out)
    }

    /// F12 explorer: which type-group folders one schema has, with counts.
    pub fn list_object_groups(&self, schema: &str) -> anyhow::Result<Vec<(String, u64)>> {
        let sql = "SELECT OBJECT_TYPE, COUNT(*) FROM ALL_OBJECTS WHERE OWNER = :1 \
                   AND OBJECT_TYPE IN ('TABLE','VIEW','MATERIALIZED VIEW','INDEX', \
                   'SEQUENCE','PROCEDURE','FUNCTION','PACKAGE','PACKAGE BODY', \
                   'TRIGGER','TYPE') GROUP BY OBJECT_TYPE";
        let mut stmt = self
            .conn
            .statement(sql)
            .build()
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let owned = schema.to_string();
        let rows = stmt
            .query(&[&owned])
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        // Several dictionary types fold into one explorer folder.
        let mut counts: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
        for row_result in rows {
            let row = row_result.map_err(|e| anyhow::anyhow!("{}", e))?;
            let otype: String = row.get(0).unwrap_or_default();
            let n: i64 = row.get(1).unwrap_or(0);
            let key = match otype.as_str() {
                "TABLE" => "TABLES",
                "VIEW" => "VIEWS",
                "MATERIALIZED VIEW" => "MVIEWS",
                "INDEX" => "INDEXES",
                "SEQUENCE" => "SEQUENCES",
                "PROCEDURE" => "PROCEDURES",
                "FUNCTION" => "FUNCTIONS",
                "PACKAGE" | "PACKAGE BODY" => "PACKAGES",
                "TRIGGER" => "TRIGGERS",
                "TYPE" => "TYPES",
                _ => continue,
            };
            *counts.entry(key).or_insert(0) += n.max(0) as u64;
        }
        Ok(counts
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect())
    }

    /// F12 explorer: objects of one type-group folder, ordered by name.
    pub fn list_objects(&self, schema: &str, group: &str) -> anyhow::Result<Vec<(String, String)>> {
        let owned = schema.to_string();
        let sql = match group {
            "TABLES" => {
                "SELECT TABLE_NAME, 'TABLE' FROM ALL_TABLES WHERE OWNER = :1 ORDER BY 1"
            }
            "VIEWS" => {
                "SELECT VIEW_NAME, 'VIEW' FROM ALL_VIEWS WHERE OWNER = :1 ORDER BY 1"
            }
            "MVIEWS" => {
                "SELECT MVIEW_NAME, 'MVIEW' FROM ALL_MVIEWS WHERE OWNER = :1 ORDER BY 1"
            }
            "INDEXES" => {
                "SELECT INDEX_NAME, 'INDEX' FROM ALL_INDEXES WHERE OWNER = :1 ORDER BY 1"
            }
            "SEQUENCES" => {
                "SELECT SEQUENCE_NAME, 'SEQUENCE' FROM ALL_SEQUENCES WHERE SEQUENCE_OWNER = :1 ORDER BY 1"
            }
            "PACKAGES" => {
                "SELECT OBJECT_NAME, OBJECT_TYPE FROM ALL_OBJECTS WHERE OWNER = :1 \
                 AND OBJECT_TYPE IN ('PACKAGE','PACKAGE BODY') ORDER BY 1"
            }
            "PROCEDURES" => {
                "SELECT OBJECT_NAME, 'PROCEDURE' FROM ALL_OBJECTS WHERE OWNER = :1 \
                 AND OBJECT_TYPE = 'PROCEDURE' ORDER BY 1"
            }
            "FUNCTIONS" => {
                "SELECT OBJECT_NAME, 'FUNCTION' FROM ALL_OBJECTS WHERE OWNER = :1 \
                 AND OBJECT_TYPE = 'FUNCTION' ORDER BY 1"
            }
            "TRIGGERS" => {
                "SELECT OBJECT_NAME, 'TRIGGER' FROM ALL_OBJECTS WHERE OWNER = :1 \
                 AND OBJECT_TYPE = 'TRIGGER' ORDER BY 1"
            }
            "TYPES" => {
                "SELECT OBJECT_NAME, 'TYPE' FROM ALL_OBJECTS WHERE OWNER = :1 \
                 AND OBJECT_TYPE = 'TYPE' ORDER BY 1"
            }
            _ => return Ok(Vec::new()),
        };
        let mut stmt = self
            .conn
            .statement(sql)
            .build()
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let rows = stmt
            .query(&[&owned])
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let mut out = Vec::new();
        for row_result in rows {
            let row = row_result.map_err(|e| anyhow::anyhow!("{}", e))?;
            let name: String = row.get(0).unwrap_or_default();
            let kind: String = row.get(1).unwrap_or_default();
            if !name.trim().is_empty() {
                out.push((name, kind));
            }
            if out.len() >= 2000 {
                break;
            }
        }
        Ok(out)
    }

    /// F12 explorer: key columns of one index (position + sort order).
    fn list_index_columns(&self, schema: &str, index: &str) -> anyhow::Result<Vec<ColumnInfo>> {
        let sql = "SELECT COLUMN_NAME, COLUMN_POSITION, DESCEND FROM ALL_IND_COLUMNS \
                   WHERE INDEX_OWNER = :1 AND INDEX_NAME = :2 ORDER BY COLUMN_POSITION";
        let mut stmt = self
            .conn
            .statement(sql)
            .build()
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let owned = schema.to_string();
        let iname = index.to_string();
        let rows = stmt
            .query(&[&owned, &iname])
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let mut out = Vec::new();
        for row_result in rows {
            let row = row_result.map_err(|e| anyhow::anyhow!("{}", e))?;
            let name: String = row.get(0).unwrap_or_default();
            let pos: i32 = row.get(1).unwrap_or(0);
            let descend: String = row.get(2).unwrap_or_default();
            if name.trim().is_empty() {
                continue;
            }
            out.push(ColumnInfo {
                name,
                data_type: format!(
                    "KEY #{}{}",
                    pos,
                    if descend.eq_ignore_ascii_case("DESC") {
                        " DESC"
                    } else {
                        ""
                    }
                ),
                nullable: true,
            });
            if out.len() >= 100 {
                break;
            }
        }
        Ok(out)
    }

    /// F12 explorer & F4 Describe: DDL/source for one object. Tables synthesize
    /// a rich CREATE TABLE (columns with precision/length/defaults, constraints,
    /// indexes & comments); views read ALL_VIEWS; code reads ALL_SOURCE.
    pub fn object_ddl(&self, schema: &str, name: &str, kind: &str) -> anyhow::Result<String> {
        match kind.to_ascii_uppercase().as_str() {
            "TABLE" | "FOREIGN TABLE" => self.table_ddl_rich(schema, name),
            "VIEW" => {
                let sql = "SELECT TEXT FROM ALL_VIEWS WHERE OWNER = :1 AND VIEW_NAME = :2";
                let mut stmt = self
                    .conn
                    .statement(sql)
                    .build()
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                let owned = schema.to_string();
                let tname = name.to_string();
                let rows = stmt
                    .query(&[&owned, &tname])
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                for row_result in rows {
                    let row = row_result.map_err(|e| anyhow::anyhow!("{}", e))?;
                    let text: String = row.get(0).unwrap_or_default();
                    if !text.trim().is_empty() {
                        let cols = self.list_columns(schema, name, "VIEW").unwrap_or_default();
                        let col_list = if cols.is_empty() {
                            String::new()
                        } else {
                            let names: Vec<String> =
                                cols.iter().map(|c| ora_ident(&c.name)).collect();
                            format!(" ({})", names.join(", "))
                        };
                        let clean_body = text.trim_end().trim_end_matches(';');
                        return Ok(format!(
                            "CREATE OR REPLACE FORCE VIEW {}.{}{}\nAS\n{};",
                            ora_ident(schema),
                            ora_ident(name),
                            col_list,
                            clean_body
                        ));
                    }
                }
                Err(anyhow::anyhow!(
                    "view text for {}.{} not visible (no access?)",
                    schema,
                    name
                ))
            }
            "INDEX" => self.index_ddl(schema, name),
            "SEQUENCE" => self.sequence_ddl(schema, name),
            "SYNONYM" => self.synonym_ddl(schema, name),
            "MVIEW" | "MATERIALIZED VIEW" => {
                let sql = "SELECT BUILD_MODE, REFRESH_MODE, REFRESH_METHOD, QUERY \
                           FROM ALL_MVIEWS WHERE OWNER = :1 AND MVIEW_NAME = :2";
                let mut stmt = self
                    .conn
                    .statement(sql)
                    .build()
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                let owned = schema.to_string();
                let tname = name.to_string();
                let rows = stmt
                    .query(&[&owned, &tname])
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                for row_result in rows {
                    let row = row_result.map_err(|e| anyhow::anyhow!("{}", e))?;
                    let build: String = row.get(0).unwrap_or_default();
                    let rmode: String = row.get(1).unwrap_or_default();
                    let rmethod: String = row.get(2).unwrap_or_default();
                    let text: String = row.get(3).unwrap_or_default();
                    if !text.trim().is_empty() {
                        let clean_body = text.trim_end().trim_end_matches(';');
                        let mut ddl = format!(
                            "CREATE MATERIALIZED VIEW {}.{}\n  BUILD {}\n  REFRESH {} ON {}\nAS\n{};",
                            ora_ident(schema),
                            ora_ident(name),
                            if build.is_empty() { "IMMEDIATE" } else { &build },
                            if rmethod.is_empty() { "FORCE" } else { &rmethod },
                            if rmode.is_empty() { "DEMAND" } else { &rmode },
                            clean_body
                        );
                        let idx_stmts = self.table_indexes_ddl(&owned, &tname, &[]).unwrap_or_default();
                        if !idx_stmts.is_empty() {
                            ddl.push_str("\n\n-- Indexes\n");
                            ddl.push_str(&idx_stmts.join("\n"));
                        }
                        return Ok(ddl);
                    }
                }
                Err(anyhow::anyhow!(
                    "materialized view text for {}.{} not visible (no access?)",
                    schema,
                    name
                ))
            }
            "PACKAGE" => {
                let spec = self.source_ddl(schema, name, "PACKAGE")?;
                match self.source_ddl(schema, name, "PACKAGE BODY") {
                    Ok(body) => Ok(format!("{}\n\n{}", spec, body)),
                    Err(_) => Ok(spec),
                }
            }
            other => self.source_ddl(schema, name, other),
        }
    }

    fn source_ddl(&self, schema: &str, name: &str, otype: &str) -> anyhow::Result<String> {
        let sql = "SELECT TEXT FROM ALL_SOURCE WHERE OWNER = :1 AND NAME = :2 \
                   AND TYPE = :3 ORDER BY LINE";
        let mut stmt = self
            .conn
            .statement(sql)
            .build()
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let owned = schema.to_string();
        let oname = name.to_string();
        let otype_up = otype.to_uppercase();
        let rows = stmt
            .query(&[&owned, &oname, &otype_up])
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let mut out = String::new();
        let mut lines = 0usize;
        for row_result in rows {
            let row = row_result.map_err(|e| anyhow::anyhow!("{}", e))?;
            let text: String = row.get(0).unwrap_or_default();
            out.push_str(&text);
            if !text.ends_with('\n') {
                out.push('\n');
            }
            lines += 1;
            if lines >= 10_000 {
                out.push_str("-- ... truncated at 10000 lines ...\n");
                break;
            }
        }
        if out.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "no source for {} {}.{} (need SELECT on ALL_SOURCE?)",
                otype,
                schema,
                name
            ));
        }
        let trimmed = out.trim_start();
        let upper_head = trimmed.to_ascii_uppercase();
        let full = if upper_head.starts_with("CREATE") {
            trimmed.to_string()
        } else {
            format!("CREATE OR REPLACE {}", trimmed)
        };
        let mut clean = full.trim_end().to_string();
        if !clean.ends_with("\n/") {
            clean.push_str("\n/");
        }
        Ok(clean)
    }

    fn table_ddl_rich(&self, schema: &str, name: &str) -> anyhow::Result<String> {
        let sql = "SELECT COLUMN_NAME, DATA_TYPE, DATA_LENGTH, CHAR_LENGTH, CHAR_USED, \
                          DATA_PRECISION, DATA_SCALE, NULLABLE, DATA_DEFAULT \
                   FROM ALL_TAB_COLUMNS \
                   WHERE OWNER = :1 AND TABLE_NAME = :2 ORDER BY COLUMN_ID";
        let mut stmt = self
            .conn
            .statement(sql)
            .build()
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let owned = schema.to_string();
        let tname = name.to_string();
        let rows = stmt
            .query(&[&owned, &tname])
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        let mut cols = Vec::new();
        for row_result in rows {
            let row = row_result.map_err(|e| anyhow::anyhow!("{}", e))?;
            let cname: String = row.get(0).unwrap_or_default();
            if cname.trim().is_empty() {
                continue;
            }
            let dtype: String = row.get(1).unwrap_or_default();
            let dlen: Option<i64> = row.get(2).unwrap_or(None);
            let clen: Option<i64> = row.get(3).unwrap_or(None);
            let cused: Option<String> = row.get(4).unwrap_or(None);
            let prec: Option<i64> = row.get(5).unwrap_or(None);
            let scale: Option<i64> = row.get(6).unwrap_or(None);
            let nullable: String = row.get(7).unwrap_or_default();
            let ddefault: Option<String> = row.get(8).unwrap_or(None);

            cols.push(RichColumn {
                name: cname,
                full_type: format_oracle_col_type(
                    &dtype,
                    dlen,
                    clen,
                    cused.as_deref(),
                    prec,
                    scale,
                ),
                default_expr: ddefault
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                nullable: nullable.trim().eq_ignore_ascii_case("Y"),
            });
            if cols.len() >= 1000 {
                break;
            }
        }

        if cols.is_empty() {
            return Err(anyhow::anyhow!(
                "no columns visible for {}.{} (no access?)",
                schema,
                name
            ));
        }

        let (constraints, constraint_indexes) = self
            .table_constraints_ddl(&owned, &tname)
            .unwrap_or_default();

        let mut extra = Vec::new();
        if let Ok(idx_ddls) = self.table_indexes_ddl(&owned, &tname, &constraint_indexes) {
            if !idx_ddls.is_empty() {
                extra.push("-- Indexes".to_string());
                extra.extend(idx_ddls);
            }
        }
        if let Ok(comments) = self.table_comments_ddl(&owned, &tname) {
            if !comments.is_empty() {
                extra.push("-- Comments".to_string());
                extra.extend(comments);
            }
        }

        let qualified = format!("{}.{}", ora_ident(&owned), ora_ident(&tname));
        Ok(format_rich_create_table(
            "TABLE",
            &qualified,
            &cols,
            &constraints,
            None,
            &extra,
            ora_ident,
        ))
    }

    fn table_constraints_ddl(
        &self,
        owner: &str,
        table: &str,
    ) -> anyhow::Result<(Vec<TableConstraintDdl>, Vec<String>)> {
        let sql = "\
            SELECT c.constraint_name, c.constraint_type, c.generated, \
                   c.r_owner, c.r_constraint_name, c.delete_rule, c.index_name, \
                   cc.column_name \
            FROM all_constraints c \
            JOIN all_cons_columns cc \
              ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name \
            WHERE c.owner = :1 AND c.table_name = :2 \
              AND c.constraint_type IN ('P', 'U', 'R') \
            ORDER BY CASE c.constraint_type WHEN 'P' THEN 0 WHEN 'U' THEN 1 ELSE 2 END, \
                     c.constraint_name, cc.position";
        let mut stmt = self
            .conn
            .statement(sql)
            .build()
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let rows = stmt
            .query(&[&owner, &table])
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        struct RawCon {
            name: String,
            ctype: String,
            generated: bool,
            r_owner: String,
            r_con: String,
            delete_rule: String,
            cols: Vec<String>,
        }

        let mut list: Vec<RawCon> = Vec::new();
        let mut backing_indexes: Vec<String> = Vec::new();

        for row_result in rows {
            let row = row_result.map_err(|e| anyhow::anyhow!("{}", e))?;
            let cname: String = row.get(0).unwrap_or_default();
            let ctype: String = row.get(1).unwrap_or_default();
            let gen_flag: String = row.get(2).unwrap_or_default();
            let r_owner: String = row.get(3).unwrap_or_default();
            let r_con: String = row.get(4).unwrap_or_default();
            let del_rule: String = row.get(5).unwrap_or_default();
            let idx_name: String = row.get(6).unwrap_or_default();
            let col_name: String = row.get(7).unwrap_or_default();

            if !idx_name.trim().is_empty() && !backing_indexes.contains(&idx_name) {
                backing_indexes.push(idx_name);
            }
            if let Some(last) = list.last_mut().filter(|x| x.name == cname) {
                last.cols.push(ora_ident(&col_name));
            } else {
                list.push(RawCon {
                    name: cname,
                    ctype,
                    generated: gen_flag.eq_ignore_ascii_case("GENERATED NAME"),
                    r_owner,
                    r_con,
                    delete_rule: del_rule,
                    cols: vec![ora_ident(&col_name)],
                });
            }
        }

        let mut out = Vec::new();
        for c in list {
            let opt_name = if c.generated || c.name.starts_with("SYS_") {
                None
            } else {
                Some(c.name.clone())
            };
            let cols_str = c.cols.join(", ");
            let def = match c.ctype.as_str() {
                "P" => format!("PRIMARY KEY ({})", cols_str),
                "U" => format!("UNIQUE ({})", cols_str),
                "R" => {
                    let target = self
                        .resolve_fk_target(&c.r_owner, &c.r_con)
                        .unwrap_or_else(|| format!("{}.{}", c.r_owner, c.r_con));
                    let on_del = match c.delete_rule.to_ascii_uppercase().as_str() {
                        "CASCADE" => " ON DELETE CASCADE",
                        "SET NULL" => " ON DELETE SET NULL",
                        _ => "",
                    };
                    format!("FOREIGN KEY ({}) REFERENCES {}{}", cols_str, target, on_del)
                }
                _ => continue,
            };
            out.push(TableConstraintDdl {
                name: opt_name,
                definition: def,
            });
        }
        Ok((out, backing_indexes))
    }

    fn resolve_fk_target(&self, r_owner: &str, r_con: &str) -> Option<String> {
        if r_owner.is_empty() || r_con.is_empty() {
            return None;
        }
        let sql = "SELECT TABLE_NAME, COLUMN_NAME FROM ALL_CONS_COLUMNS \
                   WHERE OWNER = :1 AND CONSTRAINT_NAME = :2 ORDER BY POSITION";
        let mut stmt = self.conn.statement(sql).build().ok()?;
        let rows = stmt.query(&[&r_owner, &r_con]).ok()?;
        let mut tname = String::new();
        let mut cols = Vec::new();
        for row in rows.flatten() {
            let tn: String = row.get(0).unwrap_or_default();
            let cn: String = row.get(1).unwrap_or_default();
            if tname.is_empty() {
                tname = tn;
            }
            if !cn.is_empty() {
                cols.push(ora_ident(&cn));
            }
        }
        if tname.is_empty() {
            None
        } else {
            Some(format!(
                "{}.{} ({})",
                ora_ident(r_owner),
                ora_ident(&tname),
                cols.join(", ")
            ))
        }
    }

    fn table_indexes_ddl(
        &self,
        owner: &str,
        table: &str,
        skip_names: &[String],
    ) -> anyhow::Result<Vec<String>> {
        let sql = "SELECT OWNER, INDEX_NAME, GENERATED FROM ALL_INDEXES \
                   WHERE TABLE_OWNER = :1 AND TABLE_NAME = :2 \
                     AND INDEX_TYPE != 'LOB' \
                   ORDER BY INDEX_NAME";
        let mut stmt = self
            .conn
            .statement(sql)
            .build()
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let rows = stmt
            .query(&[&owner, &table])
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let mut out = Vec::new();
        for row_result in rows {
            let row = row_result.map_err(|e| anyhow::anyhow!("{}", e))?;
            let iowner: String = row.get(0).unwrap_or_default();
            let iname: String = row.get(1).unwrap_or_default();
            let generated: String = row.get(2).unwrap_or_default();
            if skip_names.iter().any(|s| s == &iname) && generated.eq_ignore_ascii_case("Y") {
                continue;
            }
            if skip_names.iter().any(|s| s == &iname) {
                continue;
            }
            if let Ok(ddl) = self.index_ddl(&iowner, &iname) {
                out.push(ddl);
            }
        }
        Ok(out)
    }

    fn table_comments_ddl(&self, owner: &str, table: &str) -> anyhow::Result<Vec<String>> {
        let mut out = Vec::new();
        if let Ok(mut stmt) = self
            .conn
            .statement("SELECT COMMENTS FROM ALL_TAB_COMMENTS WHERE OWNER = :1 AND TABLE_NAME = :2")
            .build()
        {
            if let Ok(rows) = stmt.query(&[&owner, &table]) {
                for row in rows.flatten() {
                    let c: String = row.get(0).unwrap_or_default();
                    if !c.trim().is_empty() {
                        out.push(format!(
                            "COMMENT ON TABLE {}.{} IS '{}';",
                            ora_ident(owner),
                            ora_ident(table),
                            c.replace('\'', "''")
                        ));
                    }
                }
            }
        }
        if let Ok(mut stmt) = self
            .conn
            .statement(
                "SELECT COLUMN_NAME, COMMENTS FROM ALL_COL_COMMENTS \
                 WHERE OWNER = :1 AND TABLE_NAME = :2 AND COMMENTS IS NOT NULL \
                 ORDER BY COLUMN_NAME",
            )
            .build()
        {
            if let Ok(rows) = stmt.query(&[&owner, &table]) {
                for row in rows.flatten() {
                    let col: String = row.get(0).unwrap_or_default();
                    let c: String = row.get(1).unwrap_or_default();
                    if !col.trim().is_empty() && !c.trim().is_empty() {
                        out.push(format!(
                            "COMMENT ON COLUMN {}.{}.{} IS '{}';",
                            ora_ident(owner),
                            ora_ident(table),
                            ora_ident(&col),
                            c.replace('\'', "''")
                        ));
                    }
                }
            }
        }
        Ok(out)
    }

    fn synonym_ddl(&self, schema: &str, name: &str) -> anyhow::Result<String> {
        let sql = "SELECT TABLE_OWNER, TABLE_NAME, DB_LINK FROM ALL_SYNONYMS \
                   WHERE OWNER = :1 AND SYNONYM_NAME = :2";
        let mut stmt = self
            .conn
            .statement(sql)
            .build()
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let owned = schema.to_string();
        let sname = name.to_string();
        let rows = stmt
            .query(&[&owned, &sname])
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        if let Some(row_res) = rows.into_iter().next() {
            let row = row_res.map_err(|e| anyhow::anyhow!("{}", e))?;
            let towner: String = row.get(0).unwrap_or_default();
            let tname: String = row.get(1).unwrap_or_default();
            let dblink: String = row.get(2).unwrap_or_default();
            let pub_kw = if owned == "PUBLIC" { "PUBLIC " } else { "" };
            let syn_qual = if owned == "PUBLIC" {
                ora_ident(&sname)
            } else {
                format!("{}.{}", ora_ident(&owned), ora_ident(&sname))
            };
            let target_qual = if towner.is_empty() {
                ora_ident(&tname)
            } else {
                format!("{}.{}", ora_ident(&towner), ora_ident(&tname))
            };
            let link = if dblink.is_empty() {
                String::new()
            } else {
                format!("@{}", dblink)
            };
            return Ok(format!(
                "CREATE OR REPLACE {}SYNONYM {} FOR {}{};",
                pub_kw, syn_qual, target_qual, link
            ));
        }
        Err(anyhow::anyhow!(
            "synonym {}.{} not visible",
            schema,
            name
        ))
    }

    /// F12 explorer: columns of one table-like object (or the key columns
    /// of an index), in ordinal order.
    pub fn list_columns(
        &self,
        schema: &str,
        table: &str,
        kind: &str,
    ) -> anyhow::Result<Vec<ColumnInfo>> {
        if kind == "INDEX" {
            return self.list_index_columns(schema, table);
        }
        let sql = "SELECT COLUMN_NAME, DATA_TYPE, NULLABLE FROM ALL_TAB_COLUMNS \
                   WHERE OWNER = :1 AND TABLE_NAME = :2 ORDER BY COLUMN_ID";
        let mut stmt = self
            .conn
            .statement(sql)
            .build()
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let owned = schema.to_string();
        let tname = table.to_string();
        let rows = stmt
            .query(&[&owned, &tname])
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let mut out = Vec::new();
        for row_result in rows {
            let row = row_result.map_err(|e| anyhow::anyhow!("{}", e))?;
            let name: String = row.get(0).unwrap_or_default();
            let dtype: String = row.get(1).unwrap_or_default();
            let nullable: String = row.get(2).unwrap_or_default();
            if name.trim().is_empty() {
                continue;
            }
            out.push(ColumnInfo {
                name,
                data_type: if dtype.is_empty() { "?".into() } else { dtype },
                nullable: nullable.trim().eq_ignore_ascii_case("Y"),
            });
            if out.len() >= 1000 {
                break;
            }
        }
        Ok(out)
    }

    /// Completion: schema that unqualified names resolve in.
    pub fn current_schema(&self) -> anyhow::Result<String> {
        let mut stmt = self
            .conn
            .statement("SELECT SYS_CONTEXT('USERENV','CURRENT_SCHEMA') FROM dual")
            .build()
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let rows = stmt.query(&[]).map_err(|e| anyhow::anyhow!("{}", e))?;
        for row_result in rows {
            let row = row_result.map_err(|e| anyhow::anyhow!("{}", e))?;
            let name: String = row.get(0).unwrap_or_default();
            if !name.trim().is_empty() {
                return Ok(name);
            }
        }
        Err(anyhow::anyhow!("CURRENT_SCHEMA is not visible"))
    }

    /// Completion: all object names of one schema in one query.
    pub fn list_schema_objects(&self, schema: &str) -> anyhow::Result<Vec<MetaObject>> {
        let sql = "SELECT * FROM (SELECT OBJECT_NAME, OBJECT_TYPE FROM ALL_OBJECTS \
                   WHERE OWNER = :1 \
                   AND OBJECT_TYPE IN ('TABLE','VIEW','MATERIALIZED VIEW','SYNONYM', \
                   'SEQUENCE','PROCEDURE','FUNCTION','PACKAGE','TYPE') \
                   AND OBJECT_NAME NOT LIKE 'BIN$%' \
                   ORDER BY OBJECT_NAME) WHERE ROWNUM <= 20000";
        let mut stmt = self
            .conn
            .statement(sql)
            .build()
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let owner = schema.to_string();
        let rows = stmt
            .query(&[&owner])
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let mut out = Vec::new();
        for row_result in rows {
            let row = row_result.map_err(|e| anyhow::anyhow!("{}", e))?;
            let name: String = row.get(0).unwrap_or_default();
            let otype: String = row.get(1).unwrap_or_default();
            if name.trim().is_empty() {
                continue;
            }
            out.push(MetaObject {
                schema: schema.to_string(),
                name,
                kind: MetaObjectKind::from_oracle(&otype),
            });
        }
        Ok(out)
    }

    /// Completion: columns for the tables referenced by the current statement,
    /// in as few queries as possible. Own-schema tables use `USER_TAB_COLUMNS`
    /// (no privilege filter); other schemas use `ALL_TAB_COLUMNS` with an
    /// owner predicate. Both are capped.
    pub fn table_columns_batch(
        &self,
        refs: &[TableRef],
        current_schema: &str,
    ) -> anyhow::Result<Vec<TableColumns>> {
        use oracle::sql_type::ToSql;
        use std::collections::BTreeMap;

        // owner (dictionary spelling) -> requested table names (dictionary spelling)
        let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for r in refs {
            let owner = match &r.schema {
                Some(s) if r.schema_quoted => s.clone(),
                Some(s) => s.to_uppercase(),
                None => current_schema.to_string(),
            };
            let table = if r.quoted {
                r.name.clone()
            } else {
                r.name.to_uppercase()
            };
            let entry = groups.entry(owner).or_default();
            if !entry.iter().any(|t| t == &table) {
                entry.push(table);
            }
        }

        let mut out = Vec::new();
        for (owner, tables) in groups {
            if tables.is_empty() {
                continue;
            }
            let is_current = owner.eq_ignore_ascii_case(current_schema);
            let (sql, binds_owner) = if is_current {
                let placeholders: Vec<String> =
                    (1..=tables.len()).map(|i| format!(":{}", i)).collect();
                (
                    format!(
                        "SELECT * FROM (SELECT TABLE_NAME, COLUMN_NAME, DATA_TYPE, NULLABLE, \
                         COLUMN_ID FROM USER_TAB_COLUMNS WHERE TABLE_NAME IN ({}) \
                         ORDER BY TABLE_NAME, COLUMN_ID) WHERE ROWNUM <= 20000",
                        placeholders.join(",")
                    ),
                    false,
                )
            } else {
                let placeholders: Vec<String> =
                    (2..=tables.len() + 1).map(|i| format!(":{}", i)).collect();
                (
                    format!(
                        "SELECT * FROM (SELECT TABLE_NAME, COLUMN_NAME, DATA_TYPE, NULLABLE, \
                         COLUMN_ID FROM ALL_TAB_COLUMNS WHERE OWNER = :1 AND TABLE_NAME IN ({}) \
                         ORDER BY TABLE_NAME, COLUMN_ID) WHERE ROWNUM <= 20000",
                        placeholders.join(",")
                    ),
                    true,
                )
            };

            let mut owned: Vec<String> = Vec::new();
            if binds_owner {
                owned.push(owner.clone());
            }
            owned.extend(tables.iter().cloned());
            let params: Vec<&dyn ToSql> = owned.iter().map(|s| s as &dyn ToSql).collect();

            let mut stmt = self
                .conn
                .statement(&sql)
                .build()
                .map_err(|e| anyhow::anyhow!("{}", e))?;
            let rows = stmt.query(&params).map_err(|e| anyhow::anyhow!("{}", e))?;

            let mut found: BTreeMap<String, Vec<ColumnInfo>> = BTreeMap::new();
            for row_result in rows {
                let row = row_result.map_err(|e| anyhow::anyhow!("{}", e))?;
                let table: String = row.get(0).unwrap_or_default();
                let name: String = row.get(1).unwrap_or_default();
                let dtype: String = row.get(2).unwrap_or_default();
                let nullable: String = row.get(3).unwrap_or_default();
                if table.trim().is_empty() || name.trim().is_empty() {
                    continue;
                }
                found.entry(table).or_default().push(ColumnInfo {
                    name,
                    data_type: if dtype.is_empty() { "?".into() } else { dtype },
                    nullable: nullable.trim().eq_ignore_ascii_case("Y"),
                });
            }
            for table in tables {
                let cols = found.remove(&table).unwrap_or_default();
                out.push(TableColumns {
                    schema: owner.clone(),
                    table,
                    columns: cols,
                });
            }
        }
        Ok(out)
    }

    /// F12 explorer & F4 Describe: CREATE INDEX reconstructed from ALL_INDEXES,
    /// ALL_IND_COLUMNS and ALL_IND_EXPRESSIONS (handles bitmap & function-based
    /// indexes with no DBMS_METADATA privileges needed).
    fn index_ddl(&self, schema: &str, index: &str) -> anyhow::Result<String> {
        let sql = "SELECT TABLE_OWNER, TABLE_NAME, UNIQUENESS, INDEX_TYPE, TABLESPACE_NAME \
                   FROM ALL_INDEXES WHERE OWNER = :1 AND INDEX_NAME = :2";
        let mut stmt = self
            .conn
            .statement(sql)
            .build()
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let owned = schema.to_string();
        let iname = index.to_string();
        let rows = stmt
            .query(&[&owned, &iname])
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let (towner, tname, unique, idx_type, tbs) = match rows.into_iter().next() {
            Some(row_result) => {
                let row = row_result.map_err(|e| anyhow::anyhow!("{}", e))?;
                let to: String = row.get(0).unwrap_or_default();
                let tn: String = row.get(1).unwrap_or_default();
                let uq: String = row.get(2).unwrap_or_default();
                let it: String = row.get(3).unwrap_or_default();
                let ts: String = row.get(4).unwrap_or_default();
                (to, tn, uq.eq_ignore_ascii_case("UNIQUE"), it, ts)
            }
            None => {
                return Err(anyhow::anyhow!(
                    "index {}.{} not visible (no access?)",
                    schema,
                    index
                ))
            }
        };
        let cols = self.list_index_columns(schema, index)?;
        if cols.is_empty() {
            return Err(anyhow::anyhow!(
                "no key columns visible for index {}.{}",
                schema,
                index
            ));
        }

        // Function-based index expressions (SYS_NC...$ synthetic column names).
        let mut exprs: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
        if idx_type.to_ascii_uppercase().contains("FUNCTION") {
            if let Ok(mut estmt) = self
                .conn
                .statement(
                    "SELECT COLUMN_POSITION, COLUMN_EXPRESSION FROM ALL_IND_EXPRESSIONS \
                     WHERE INDEX_OWNER = :1 AND INDEX_NAME = :2",
                )
                .build()
            {
                if let Ok(erows) = estmt.query(&[&owned, &iname]) {
                    for erow in erows.flatten() {
                        let pos: i32 = erow.get(0).unwrap_or(0);
                        let expr: String = erow.get(1).unwrap_or_default();
                        if pos > 0 && !expr.trim().is_empty() {
                            exprs.insert(pos as usize, expr.trim().to_string());
                        }
                    }
                }
            }
        }

        let keys: Vec<String> = cols
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let base = exprs
                    .get(&(i + 1))
                    .cloned()
                    .unwrap_or_else(|| ora_ident(&c.name));
                if c.data_type.ends_with(" DESC") {
                    format!("{} DESC", base)
                } else {
                    base
                }
            })
            .collect();
        let modifier = if unique {
            "UNIQUE "
        } else if idx_type.to_ascii_uppercase().contains("BITMAP") {
            "BITMAP "
        } else {
            ""
        };
        let tbs_clause = if tbs.trim().is_empty() {
            String::new()
        } else {
            format!(" TABLESPACE {}", ora_ident(tbs.trim()))
        };
        Ok(format!(
            "CREATE {}INDEX {}.{} ON {}.{} ({}){};",
            modifier,
            ora_ident(&owned),
            ora_ident(&iname),
            ora_ident(&towner),
            ora_ident(&tname),
            keys.join(", "),
            tbs_clause
        ))
    }

    /// F12 explorer: CREATE SEQUENCE from ALL_SEQUENCES (numbers come back
    /// as strings so huge MAXVALUEs can't overflow).
    fn sequence_ddl(&self, schema: &str, name: &str) -> anyhow::Result<String> {
        let sql = "SELECT MIN_VALUE, MAX_VALUE, INCREMENT_BY, CYCLE_FLAG, CACHE_SIZE \
                   FROM ALL_SEQUENCES WHERE SEQUENCE_OWNER = :1 AND SEQUENCE_NAME = :2";
        let mut stmt = self
            .conn
            .statement(sql)
            .build()
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let owned = schema.to_string();
        let sname = name.to_string();
        let rows = stmt
            .query(&[&owned, &sname])
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        match rows.into_iter().next() {
            Some(row_result) => {
                let row = row_result.map_err(|e| anyhow::anyhow!("{}", e))?;
                let min: String = row.get(0).unwrap_or_default();
                let max: String = row.get(1).unwrap_or_default();
                let inc: String = row.get(2).unwrap_or_default();
                let cycle: String = row.get(3).unwrap_or_default();
                let cache: String = row.get(4).unwrap_or_default();
                Ok(format!(
                    "CREATE SEQUENCE {}.{}\n  INCREMENT BY {}\n  MINVALUE {}\n  \
                     MAXVALUE {}\n  {}CYCLE\n  CACHE {};",
                    ora_ident(schema),
                    ora_ident(name),
                    inc,
                    min,
                    max,
                    if cycle.eq_ignore_ascii_case("Y") {
                        ""
                    } else {
                        "NO"
                    },
                    cache
                ))
            }
            None => Err(anyhow::anyhow!(
                "sequence {}.{} not visible (no access?)",
                schema,
                name
            )),
        }
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
            &format!("DELETE FROM PLAN_TABLE WHERE STATEMENT_ID = '{}'", stmt_id),
            &[],
        );
        result
    }

    /// F4 Toad-style Describe: resolve `[schema.]object` (including synonyms,
    /// cross-schema indexes/views/tables, and `package.member` calls) and
    /// generate its full DDL script.
    pub fn describe_object(&self, target: &DdlTarget) -> anyhow::Result<DescribedObject> {
        let start = std::time::Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;

        let cur_schema = self.current_schema().unwrap_or_default();
        let norm_name = if target.name_quoted {
            target.name.clone()
        } else {
            target.name.to_uppercase()
        };

        if let (Some(s), Some(k)) = (target.schema.as_deref(), target.kind_hint.as_deref()) {
            let owner = if target.schema_quoted {
                s.to_string()
            } else {
                s.to_uppercase()
            };
            let ddl = self.object_ddl(&owner, &norm_name, k)?;
            let summary = self.oracle_object_summary(&owner, &norm_name, k);
            return Ok(DescribedObject {
                schema: owner,
                name: norm_name,
                kind: k.to_uppercase(),
                ddl,
                summary,
                elapsed_ms: elapsed(),
            });
        }

        let mut synonym_prefix: Option<String> = None;
        let resolved: Option<(String, String, String)> = match target.schema.as_deref() {
            Some(s) => {
                let owner = if target.schema_quoted {
                    s.to_string()
                } else {
                    s.to_uppercase()
                };
                if let Some((kind, _)) = self.find_in_schema(&owner, &norm_name) {
                    Some((owner, norm_name.clone(), kind))
                } else if !cur_schema.is_empty() {
                    // Could be `pkg_name.member_name` in CURRENT_SCHEMA or a synonym to a package.
                    if let Some((kind, _)) = self
                        .find_in_schema(&cur_schema, &owner)
                        .filter(|(k, _)| k == "PACKAGE")
                    {
                        Some((cur_schema.clone(), owner, kind))
                    } else if let Some((syn_owner, t_owner, t_name, dblink)) =
                        self.resolve_synonym(&cur_schema, &owner)
                    {
                        if dblink.is_empty() {
                            if let Some((kind, _)) = self
                                .find_in_schema(&t_owner, &t_name)
                                .filter(|(k, _)| k == "PACKAGE")
                            {
                                if let Ok(sddl) = self.synonym_ddl(&syn_owner, &owner) {
                                    synonym_prefix = Some(format!(
                                        "-- Synonym {}.{} -> {}.{}\n{}",
                                        syn_owner, owner, t_owner, t_name, sddl
                                    ));
                                }
                                Some((t_owner, t_name, kind))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            None => {
                // 1. Current schema direct object.
                let direct = if !cur_schema.is_empty() {
                    self.find_in_schema(&cur_schema, &norm_name)
                        .map(|(k, _)| (cur_schema.clone(), norm_name.clone(), k))
                } else {
                    None
                };

                match direct {
                    Some((o, n, k)) if k != "SYNONYM" => Some((o, n, k)),
                    _ => {
                        // 2. Private or PUBLIC synonym -> unwrap to target object when local!
                        if let Some((syn_owner, t_owner, t_name, dblink)) =
                            self.resolve_synonym(&cur_schema, &norm_name)
                        {
                            let sddl = self.synonym_ddl(&syn_owner, &norm_name).ok();
                            if dblink.is_empty() && !t_owner.is_empty() {
                                if let Some((tkind, _)) = self.find_in_schema(&t_owner, &t_name) {
                                    if let Some(s) = sddl {
                                        synonym_prefix = Some(format!(
                                            "-- Synonym {}.{} -> {}.{}\n{}",
                                            syn_owner, norm_name, t_owner, t_name, s
                                        ));
                                    }
                                    Some((t_owner, t_name, tkind))
                                } else {
                                    Some((syn_owner, norm_name.clone(), "SYNONYM".into()))
                                }
                            } else {
                                Some((syn_owner, norm_name.clone(), "SYNONYM".into()))
                            }
                        } else {
                            // 3. Cross-schema search across ALL_OBJECTS (e.g. an unqualified index or view).
                            self.find_anywhere(&norm_name)
                        }
                    }
                }
            }
        };

        let Some((owner, obj_name, kind)) = resolved else {
            return Err(anyhow::anyhow!(
                "Object '{}' not found in ALL_OBJECTS (check spelling or privileges)",
                target.display_label()
            ));
        };

        let mut ddl = self.object_ddl(&owner, &obj_name, &kind)?;
        if let Some(prefix) = synonym_prefix {
            ddl = format!("{}\n\n{}", prefix, ddl);
        }
        let summary = self.oracle_object_summary(&owner, &obj_name, &kind);

        Ok(DescribedObject {
            schema: owner,
            name: obj_name,
            kind,
            ddl,
            summary,
            elapsed_ms: elapsed(),
        })
    }

    fn find_in_schema(&self, owner: &str, name: &str) -> Option<(String, String)> {
        let sql = "\
            SELECT OBJECT_TYPE, STATUS FROM ALL_OBJECTS \
            WHERE OWNER = :1 AND OBJECT_NAME = :2 \
              AND OBJECT_TYPE IN ( \
                'TABLE','VIEW','MATERIALIZED VIEW','INDEX','SEQUENCE', \
                'PACKAGE','PROCEDURE','FUNCTION','TRIGGER','TYPE','SYNONYM' \
              ) \
            ORDER BY CASE OBJECT_TYPE \
              WHEN 'TABLE' THEN 1 \
              WHEN 'VIEW' THEN 2 \
              WHEN 'MATERIALIZED VIEW' THEN 3 \
              WHEN 'INDEX' THEN 4 \
              WHEN 'SEQUENCE' THEN 5 \
              WHEN 'PACKAGE' THEN 6 \
              WHEN 'PROCEDURE' THEN 7 \
              WHEN 'FUNCTION' THEN 8 \
              WHEN 'TRIGGER' THEN 9 \
              WHEN 'TYPE' THEN 10 \
              ELSE 11 END";
        let mut stmt = self.conn.statement(sql).build().ok()?;
        let rows = stmt.query(&[&owner, &name]).ok()?;
        let row = rows.into_iter().next()?.ok()?;
        let otype: String = row.get(0).unwrap_or_default();
        let status: String = row.get(1).unwrap_or_default();
        if otype.is_empty() {
            None
        } else {
            Some((otype, status))
        }
    }

    fn resolve_synonym(
        &self,
        cur_schema: &str,
        syn_name: &str,
    ) -> Option<(String, String, String, String)> {
        let sql = "\
            SELECT OWNER, TABLE_OWNER, TABLE_NAME, NVL(DB_LINK, '') \
            FROM ALL_SYNONYMS \
            WHERE SYNONYM_NAME = :1 AND OWNER IN (:2, 'PUBLIC') \
            ORDER BY CASE WHEN OWNER = :2 THEN 0 ELSE 1 END";
        let mut stmt = self.conn.statement(sql).build().ok()?;
        let rows = stmt.query(&[&syn_name, &cur_schema]).ok()?;
        let row = rows.into_iter().next()?.ok()?;
        Some((
            row.get(0).unwrap_or_default(),
            row.get(1).unwrap_or_default(),
            row.get(2).unwrap_or_default(),
            row.get(3).unwrap_or_default(),
        ))
    }

    fn find_anywhere(&self, name: &str) -> Option<(String, String, String)> {
        let sql = "\
            SELECT OWNER, OBJECT_NAME, OBJECT_TYPE FROM ALL_OBJECTS \
            WHERE OBJECT_NAME = :1 \
              AND OBJECT_TYPE IN ( \
                'TABLE','VIEW','MATERIALIZED VIEW','INDEX','SEQUENCE', \
                'PACKAGE','PROCEDURE','FUNCTION','TRIGGER','TYPE','SYNONYM' \
              ) \
            ORDER BY CASE WHEN OWNER IN ('SYS','SYSTEM') THEN 1 ELSE 0 END, \
                     CASE OBJECT_TYPE \
                       WHEN 'TABLE' THEN 1 WHEN 'VIEW' THEN 2 WHEN 'INDEX' THEN 3 \
                       WHEN 'PACKAGE' THEN 4 ELSE 5 END, \
                     OWNER";
        let mut stmt = self.conn.statement(sql).build().ok()?;
        let rows = stmt.query(&[&name]).ok()?;
        let row = rows.into_iter().next()?.ok()?;
        let owner: String = row.get(0).unwrap_or_default();
        let oname: String = row.get(1).unwrap_or_default();
        let otype: String = row.get(2).unwrap_or_default();
        if owner.is_empty() || otype.is_empty() {
            None
        } else {
            Some((owner, oname, otype))
        }
    }

    fn oracle_object_summary(&self, owner: &str, name: &str, kind: &str) -> Option<String> {
        match kind.to_ascii_uppercase().as_str() {
            "TABLE" => {
                let sql = "SELECT TABLESPACE_NAME, NUM_ROWS, PARTITIONED, \
                           TO_CHAR(LAST_ANALYZED, 'YYYY-MM-DD HH24:MI') \
                           FROM ALL_TABLES WHERE OWNER = :1 AND TABLE_NAME = :2";
                let mut stmt = self.conn.statement(sql).build().ok()?;
                let row = stmt.query(&[&owner, &name]).ok()?.next()?.ok()?;
                let tbs: String = row.get(0).unwrap_or_default();
                let nrows: Option<i64> = row.get(1).unwrap_or(None);
                let part: String = row.get(2).unwrap_or_default();
                let analyzed: String = row.get(3).unwrap_or_default();
                let mut parts = Vec::new();
                if !tbs.is_empty() {
                    parts.push(format!("Tablespace: {}", tbs));
                }
                if let Some(n) = nrows {
                    parts.push(format!("Est. Rows: {}", n));
                }
                if part.eq_ignore_ascii_case("YES") {
                    parts.push("Partitioned".into());
                }
                if !analyzed.is_empty() {
                    parts.push(format!("Analyzed: {}", analyzed));
                }
                (!parts.is_empty()).then(|| parts.join(" · "))
            }
            "INDEX" => {
                let sql = "SELECT TABLE_OWNER, TABLE_NAME, INDEX_TYPE, UNIQUENESS, STATUS, \
                           NUM_ROWS, LEAF_BLOCKS \
                           FROM ALL_INDEXES WHERE OWNER = :1 AND INDEX_NAME = :2";
                let mut stmt = self.conn.statement(sql).build().ok()?;
                let row = stmt.query(&[&owner, &name]).ok()?.next()?.ok()?;
                let to: String = row.get(0).unwrap_or_default();
                let tn: String = row.get(1).unwrap_or_default();
                let it: String = row.get(2).unwrap_or_default();
                let uq: String = row.get(3).unwrap_or_default();
                let st: String = row.get(4).unwrap_or_default();
                let nr: Option<i64> = row.get(5).unwrap_or(None);
                let lb: Option<i64> = row.get(6).unwrap_or(None);
                let mut parts = vec![format!("On {}.{}", to, tn), format!("{} ({})", it, uq)];
                if !st.is_empty() {
                    parts.push(format!("Status: {}", st));
                }
                if let Some(n) = nr {
                    parts.push(format!("Rows: {}", n));
                }
                if let Some(b) = lb {
                    parts.push(format!("Leaf Blocks: {}", b));
                }
                Some(parts.join(" · "))
            }
            _ => {
                let sql = "SELECT STATUS, TO_CHAR(LAST_DDL_TIME, 'YYYY-MM-DD HH24:MI') \
                           FROM ALL_OBJECTS WHERE OWNER = :1 AND OBJECT_NAME = :2 AND ROWNUM = 1";
                let mut stmt = self.conn.statement(sql).build().ok()?;
                let row = stmt.query(&[&owner, &name]).ok()?.next()?.ok()?;
                let st: String = row.get(0).unwrap_or_default();
                let ddl_t: String = row.get(1).unwrap_or_default();
                let mut parts = Vec::new();
                if !st.is_empty() {
                    parts.push(format!("Status: {}", st));
                }
                if !ddl_t.is_empty() {
                    parts.push(format!("Last DDL: {}", ddl_t));
                }
                (!parts.is_empty()).then(|| parts.join(" · "))
            }
        }
    }

    /// F6 Performance dashboard: live Oracle session/wait/memory/cache snapshot.
    pub fn collect_perf_snapshot(&self) -> anyhow::Result<PerfSnapshot> {
        let start = std::time::Instant::now();
        let mut snap = PerfSnapshot::default();

        let sess_sql = "\
            SELECT
              COUNT(*),
              NVL(SUM(CASE WHEN status = 'ACTIVE' THEN 1 ELSE 0 END), 0),
              NVL(SUM(CASE WHEN blocking_session IS NOT NULL THEN 1 ELSE 0 END), 0),
              COUNT(DISTINCT blocking_session),
              NVL(MAX(CASE WHEN status = 'ACTIVE' THEN last_call_et ELSE 0 END), 0)
            FROM v$session
            WHERE type != 'BACKGROUND'
              AND sid != SYS_CONTEXT('USERENV', 'SID')";
        let mut stmt = self
            .conn
            .statement(sess_sql)
            .build()
            .map_err(|e| anyhow::anyhow!("{}", Self::friendly_session_error(&e.to_string())))?;
        let rows = stmt
            .query(&[])
            .map_err(|e| anyhow::anyhow!("{}", Self::friendly_session_error(&e.to_string())))?;
        if let Some(row_res) = rows.into_iter().next() {
            let row = row_res
                .map_err(|e| anyhow::anyhow!("{}", Self::friendly_session_error(&e.to_string())))?;
            snap.total_sessions = row.get::<_, i64>(0).unwrap_or(0).max(0) as u64;
            snap.active_sessions = row.get::<_, i64>(1).unwrap_or(0).max(0) as u64;
            snap.blocked_sessions = row.get::<_, i64>(2).unwrap_or(0).max(0) as u64;
            snap.blocking_sessions = row.get::<_, i64>(3).unwrap_or(0).max(0) as u64;
            snap.max_active_secs = row.get::<_, i64>(4).unwrap_or(0).max(0) as u64;
        }

        // Wait-class breakdown of active sessions (same V$SESSION privilege).
        if let Ok(mut wstmt) = self
            .conn
            .statement(
                "SELECT CASE WHEN state != 'WAITING' THEN 'CPU' ELSE wait_class END, COUNT(*) \
                 FROM v$session \
                 WHERE type != 'BACKGROUND' AND status = 'ACTIVE' \
                   AND sid != SYS_CONTEXT('USERENV', 'SID') \
                 GROUP BY CASE WHEN state != 'WAITING' THEN 'CPU' ELSE wait_class END \
                 ORDER BY 2 DESC",
            )
            .build()
        {
            if let Ok(wrows) = wstmt.query(&[]) {
                for row in wrows.flatten() {
                    let cls: String = row.get(0).unwrap_or_default();
                    let cnt: i64 = row.get(1).unwrap_or(0);
                    if !cls.is_empty() && cnt > 0 {
                        snap.wait_breakdown.push((cls, cnt as u64));
                    }
                }
            }
        }

        // Optional: SGA + PGA memory (gracefully skipped if V$SGA / V$PGASTAT not granted).
        let mut sga_mb: Option<f64> = None;
        if let Ok(mut mstmt) = self
            .conn
            .statement("SELECT ROUND(SUM(value) / 1048576, 1) FROM v$sga")
            .build()
        {
            if let Ok(mrows) = mstmt.query(&[]) {
                if let Some(Ok(r)) = mrows.into_iter().next() {
                    sga_mb = r.get::<_, f64>(0).ok();
                }
            }
        }
        let mut pga_used: Option<f64> = None;
        let mut pga_target: Option<f64> = None;
        if let Ok(mut pstmt) = self
            .conn
            .statement(
                "SELECT name, ROUND(value / 1048576, 1) FROM v$pgastat \
                 WHERE name IN ('total PGA inuse', 'total PGA allocated', 'aggregate PGA target parameter')",
            )
            .build()
        {
            if let Ok(prows) = pstmt.query(&[]) {
                let mut alloc = None;
                for r in prows.flatten() {
                    let k: String = r.get(0).unwrap_or_default();
                    let v: f64 = r.get(1).unwrap_or(0.0);
                    match k.as_str() {
                        "total PGA inuse" => pga_used = Some(v),
                        "total PGA allocated" => alloc = Some(v),
                        "aggregate PGA target parameter" if v > 0.0 => pga_target = Some(v),
                        _ => {}
                    }
                }
                if pga_used.is_none() {
                    pga_used = alloc;
                }
            }
        }
        match (sga_mb, pga_used) {
            (Some(sga), Some(pga)) => {
                let pga_cap = pga_target.unwrap_or(pga);
                snap.mem_used_mb = Some(sga + pga);
                snap.mem_total_mb = Some(sga + pga_cap.max(pga));
                snap.mem_detail = format!("SGA {:.0}M · PGA {:.0}M", sga, pga);
            }
            (Some(sga), None) => {
                snap.mem_used_mb = Some(sga);
                snap.mem_detail = format!("SGA {:.0} MB", sga);
            }
            (None, Some(pga)) => {
                snap.mem_used_mb = Some(pga);
                snap.mem_total_mb = pga_target;
                snap.mem_detail = format!("PGA {:.0} MB", pga);
            }
            (None, None) => {
                snap.mem_detail = "SGA/PGA: no V$SGA access".into();
            }
        }

        // Optional: Buffer cache hit ratio from V$SYSSTAT.
        if let Ok(mut cstmt) = self
            .conn
            .statement(
                "SELECT ROUND((1 - (SUM(CASE WHEN name = 'physical reads' THEN value ELSE 0 END) / \
                 NULLIF(SUM(CASE WHEN name IN ('db block gets', 'consistent gets') THEN value ELSE 0 END), 0))) * 100, 1) \
                 FROM v$sysstat WHERE name IN ('db block gets', 'consistent gets', 'physical reads')",
            )
            .build()
        {
            if let Ok(crows) = cstmt.query(&[]) {
                if let Some(Ok(r)) = crows.into_iter().next() {
                    snap.cache_hit_pct = r.get::<_, f64>(0).ok().filter(|v| v.is_finite());
                }
            }
        }

        snap.elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(snap)
    }
}

/// Quote an Oracle identifier only when it isn't a plain uppercase SQL identifier.
pub(crate) fn ora_ident(name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, '_' | '$' | '#'));
    if plain {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// Build the paged Oracle SQL and optional helper column to strip from output.
///
/// CTE queries (`WITH ...`) use `OFFSET ... FETCH NEXT ...` directly and have
/// no synthetic row-number column (passing `Some("RN")` there previously
/// dropped any real user column named `RN`). Regular `SELECT` queries use a
/// `ROWNUM AS FROG_RN__` wrapper to avoid colliding with user columns named `RN`.
pub(crate) fn oracle_paged_sql(
    sql: &str,
    page_offset: usize,
    page_size: usize,
) -> (String, Option<&'static str>) {
    let inner_sql = sql.trim().trim_end_matches(';').trim();
    let upper = strip_leading_comments(inner_sql).to_uppercase();
    let extra_fetch = page_size.saturating_add(1);
    if starts_with_keyword(&upper, "WITH") {
        (
            format!(
                "{} OFFSET {} ROWS FETCH NEXT {} ROWS ONLY",
                inner_sql, page_offset, extra_fetch
            ),
            None,
        )
    } else {
        let fetch_limit = page_offset.saturating_add(extra_fetch);
        (
            format!(
                "SELECT * FROM (SELECT a.*, ROWNUM AS FROG_RN__ FROM ({}) a WHERE ROWNUM <= {}) WHERE FROG_RN__ > {}",
                inner_sql, fetch_limit, page_offset
            ),
            Some("FROG_RN__"),
        )
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
    fn list_schemas(&self) -> anyhow::Result<Vec<String>> {
        self.list_schemas()
    }
    fn list_object_groups(&self, schema: &str) -> anyhow::Result<Vec<(String, u64)>> {
        self.list_object_groups(schema)
    }
    fn list_objects(&self, schema: &str, group: &str) -> anyhow::Result<Vec<(String, String)>> {
        self.list_objects(schema, group)
    }
    fn list_columns(
        &self,
        schema: &str,
        name: &str,
        kind: &str,
    ) -> anyhow::Result<Vec<ColumnInfo>> {
        self.list_columns(schema, name, kind)
    }
    fn object_ddl(&self, schema: &str, name: &str, kind: &str) -> anyhow::Result<String> {
        self.object_ddl(schema, name, kind)
    }
    fn describe_object(&self, target: &DdlTarget) -> anyhow::Result<DescribedObject> {
        OracleConnection::describe_object(self, target)
    }
    fn collect_perf_snapshot(&self) -> anyhow::Result<PerfSnapshot> {
        OracleConnection::collect_perf_snapshot(self)
    }
    fn current_schema(&self) -> anyhow::Result<String> {
        OracleConnection::current_schema(self)
    }
    fn list_schema_objects(&self, schema: &str) -> anyhow::Result<Vec<MetaObject>> {
        OracleConnection::list_schema_objects(self, schema)
    }
    fn table_columns_batch(
        &self,
        refs: &[TableRef],
        current_schema: &str,
    ) -> anyhow::Result<Vec<TableColumns>> {
        OracleConnection::table_columns_batch(self, refs, current_schema)
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
    pub machine: String,
    pub program: String,
    pub sql_id: String,
    pub logon_time: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_leading_comments_for_query_detection() {
        assert!(is_query_sql("-- note\nselect 1 from dual"));
        assert!(is_query_sql(
            "/* hi */ with x as (select 1 from dual) select * from x"
        ));
        assert!(is_query_sql(
            "-- a\n-- b\n/* c; */\nselect * from all_tables;"
        ));
        assert!(!is_query_sql("-- note\nupdate t set x = 1"));
        assert!(strip_leading_comments("-- only comment").is_empty());
        assert!(strip_leading_comments("/* unterminated").is_empty());
        assert_eq!(strip_leading_comments("-- ; --\nselect 1"), "select 1");
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
        let msg =
            OracleConnection::friendly_session_error("ORA-00942: table or view does not exist");
        assert!(msg.contains("ORA-00942"));
        assert!(msg.contains("GRANT SELECT_CATALOG_ROLE"));
        assert!(msg.contains("V_$SESSION"));
        let msg = OracleConnection::friendly_session_error("ORA-01031: insufficient privileges");
        assert!(msg.contains("GRANT SELECT ON V_$SQL"));
        // Unrelated errors pass through untouched.
        let plain = "ORA-12170: TNS:Connect timeout occurred";
        assert_eq!(OracleConnection::friendly_session_error(plain), plain);
    }

    #[test]
    fn oracle_paged_sql_preserves_user_rn_and_handles_multiline_with() {
        let (cte, skip) = oracle_paged_sql("/* c */\nWITH\nt AS (SELECT 1 AS rn FROM dual) SELECT * FROM t;", 20, 10);
        assert!(cte.ends_with("OFFSET 20 ROWS FETCH NEXT 11 ROWS ONLY"));
        assert_eq!(skip, None);

        let (sel, skip) = oracle_paged_sql("SELECT rn FROM t;", 20, 10);
        assert!(sel.contains("ROWNUM AS FROG_RN__"));
        assert_eq!(skip, Some("FROG_RN__"));
    }

    #[test]
    fn paginatable_excludes_dml_returning_and_clean_command_strips_comments() {
        assert!(is_query_sql_for(
            DbType::Postgres,
            "INSERT INTO t VALUES (1) RETURNING id"
        ));
        assert!(!is_paginatable_query_for(
            DbType::Postgres,
            "INSERT INTO t VALUES (1) RETURNING id"
        ));
        assert!(is_paginatable_query_for(DbType::Postgres, "SELECT\n1"));
        assert!(!is_query_sql_for(DbType::Postgres, "TABLESPACE foo"));
        assert_eq!(clean_command_word("-- tx end\n  COMMIT ; "), "COMMIT");
    }
}
