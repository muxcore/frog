use super::connection::{
    DbConnection, DbSessionInfo, DbType, QueryData, QueryRow, is_query_sql_for,
};
use std::sync::{Arc, Mutex};

pub struct PgConnection {
    client: Mutex<postgres::Client>,
    cancel_token: postgres::CancelToken,
    null_display: String,
}

fn clean_error_msg(msg: &str) -> String {
    // 57014 = query_canceled in Postgres.
    if msg.contains("57014")
        || msg.to_lowercase().contains("cancel")
        || msg.to_lowercase().contains("interrupted")
    {
        "Query cancelled".into()
    } else {
        msg.to_string()
    }
}

fn opt_string<T: ToString>(v: Option<T>, null_display: &str) -> String {
    v.map(|x| x.to_string())
        .unwrap_or_else(|| null_display.to_string())
}

/// Convert one cell to its display string, dispatching on the Postgres type.
/// Falls back to a `String` read for unknown types.
fn cell_to_string(row: &postgres::Row, idx: usize, null_display: &str) -> String {
    use postgres::types::Type;
    let ty = row.columns()[idx].type_().clone();
    if ty == Type::BOOL {
        let v: Result<Option<bool>, _> = row.try_get(idx);
        return v.map(|o| opt_string(o, null_display)).unwrap_or("(ERR)".into());
    }
    if ty == Type::INT2 {
        let v: Result<Option<i16>, _> = row.try_get(idx);
        return v.map(|o| opt_string(o, null_display)).unwrap_or("(ERR)".into());
    }
    if ty == Type::INT4 {
        let v: Result<Option<i32>, _> = row.try_get(idx);
        return v.map(|o| opt_string(o, null_display)).unwrap_or("(ERR)".into());
    }
    if ty == Type::INT8 {
        let v: Result<Option<i64>, _> = row.try_get(idx);
        return v.map(|o| opt_string(o, null_display)).unwrap_or("(ERR)".into());
    }
    if ty == Type::OID {
        let v: Result<Option<u32>, _> = row.try_get(idx);
        return v.map(|o| opt_string(o, null_display)).unwrap_or("(ERR)".into());
    }
    if ty == Type::FLOAT4 {
        let v: Result<Option<f32>, _> = row.try_get(idx);
        return v.map(|o| opt_string(o, null_display)).unwrap_or("(ERR)".into());
    }
    if ty == Type::FLOAT8 {
        let v: Result<Option<f64>, _> = row.try_get(idx);
        return v.map(|o| opt_string(o, null_display)).unwrap_or("(ERR)".into());
    }
    if ty == Type::BYTEA {
        let v: Result<Option<Vec<u8>>, _> = row.try_get(idx);
        return match v {
            Ok(Some(bytes)) => {
                let mut s = String::with_capacity(bytes.len() * 2 + 2);
                s.push_str("\\x");
                for b in bytes {
                    s.push_str(&format!("{:02x}", b));
                }
                s
            }
            Ok(None) => null_display.to_string(),
            Err(_) => "(ERR)".into(),
        };
    }
    if ty == Type::DATE {
        let v: Result<Option<chrono::NaiveDate>, _> = row.try_get(idx);
        return v.map(|o| opt_string(o, null_display)).unwrap_or("(ERR)".into());
    }
    if ty == Type::TIME {
        let v: Result<Option<chrono::NaiveTime>, _> = row.try_get(idx);
        return v.map(|o| opt_string(o, null_display)).unwrap_or("(ERR)".into());
    }
    if ty == Type::TIMESTAMP {
        let v: Result<Option<chrono::NaiveDateTime>, _> = row.try_get(idx);
        return v.map(|o| opt_string(o, null_display)).unwrap_or("(ERR)".into());
    }
    if ty == Type::TIMESTAMPTZ {
        let v: Result<Option<chrono::DateTime<chrono::Utc>>, _> = row.try_get(idx);
        return v.map(|o| opt_string(o, null_display)).unwrap_or("(ERR)".into());
    }
    if ty == Type::UUID {
        let v: Result<Option<uuid::Uuid>, _> = row.try_get(idx);
        return v.map(|o| opt_string(o, null_display)).unwrap_or("(ERR)".into());
    }
    if ty == Type::JSON || ty == Type::JSONB {
        let v: Result<Option<serde_json::Value>, _> = row.try_get(idx);
        return v
            .map(|o| opt_string(o, null_display))
            .unwrap_or("(ERR)".into());
    }
    // Text-like, numeric and everything else: read as String.
    let v: Result<Option<String>, _> = row.try_get(idx);
    match v {
        Ok(Some(s)) => s,
        Ok(None) => null_display.to_string(),
        Err(_) => "(ERR)".into(),
    }
}

fn rows_to_data(
    columns: Vec<String>,
    rows: Vec<postgres::Row>,
    max_rows: usize,
    null_display: &str,
) -> QueryData {
    let mut result_rows: Vec<Vec<String>> = Vec::new();
    let mut byte_count = 0usize;
    for row in rows {
        if result_rows.len() > max_rows {
            break;
        }
        let mut vals = Vec::with_capacity(columns.len());
        for i in 0..columns.len() {
            let s = cell_to_string(&row, i, null_display);
            byte_count += s.len();
            vals.push(s);
        }
        result_rows.push(vals);
    }
    let mut truncated = false;
    if result_rows.len() > max_rows {
        truncated = true;
        if let Some(extra) = result_rows.pop() {
            for val in extra {
                byte_count = byte_count.saturating_sub(val.len());
            }
        }
    }
    let total = result_rows.len();
    (columns, result_rows, truncated, total, byte_count)
}

impl PgConnection {
    pub fn connect(
        host: &str,
        port: u16,
        database: &str,
        user: &str,
        password: &str,
        null_display: &str,
    ) -> Result<Arc<Self>, anyhow::Error> {
        let mut cfg = postgres::Config::new();
        cfg.host(host);
        cfg.port(port);
        cfg.dbname(database);
        if !user.is_empty() {
            cfg.user(user);
        }
        if !password.is_empty() {
            cfg.password(password);
        }
        cfg.connect_timeout(std::time::Duration::from_secs(10));
        let client = cfg
            .connect(postgres::NoTls)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let cancel_token = client.cancel_token();
        Ok(Arc::new(Self {
            client: Mutex::new(client),
            cancel_token,
            null_display: null_display.to_string(),
        }))
    }

    fn do_query(
        &self,
        sql: &str,
        max_rows: usize,
        fetch_limit: Option<usize>,
    ) -> anyhow::Result<QueryData> {
        let mut client = self
            .client
            .lock()
            .map_err(|e| anyhow::anyhow!("connection locked: {}", e))?;
        let stmt = client
            .prepare(sql)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let col_names: Vec<String> =
            stmt.columns().iter().map(|c| c.name().to_string()).collect();
        if col_names.is_empty() {
            return Ok((vec!["Result".into()], vec![], false, 0, 0));
        }
        let limit = fetch_limit.unwrap_or(max_rows + 1);
        let rows: Vec<postgres::Row> = client
            .query(&stmt, &[])
            .map_err(|e| anyhow::anyhow!("{}", e))?
            .into_iter()
            .take(limit)
            .collect();
        drop(client);
        Ok(rows_to_data(col_names, rows, max_rows, &self.null_display))
    }

    fn run_query(&self, sql: &str, max_rows: usize) -> QueryRow {
        let start = std::time::Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;
        let trimmed = sql.trim();

        if trimmed.eq_ignore_ascii_case("commit") || trimmed.eq_ignore_ascii_case("end") {
            return match self.simple_exec("COMMIT") {
                Ok(n) => {
                    let mut row = QueryRow::notice("COMMIT", elapsed(), max_rows);
                    row.row_count = n as usize;
                    row.rows_affected = Some(n);
                    row
                }
                Err(e) => QueryRow::error(clean_error_msg(&e.to_string()), elapsed(), 0, max_rows),
            };
        }
        if trimmed.eq_ignore_ascii_case("rollback") || trimmed.eq_ignore_ascii_case("abort") {
            return match self.simple_exec("ROLLBACK") {
                Ok(_) => QueryRow::notice("ROLLBACK", elapsed(), max_rows),
                Err(e) => QueryRow::error(clean_error_msg(&e.to_string()), elapsed(), 0, max_rows),
            };
        }

        if is_query_sql_for(DbType::Postgres, trimmed) {
            return match self.do_query(sql, max_rows, None) {
                Ok(data) => QueryRow::from_query(data, elapsed(), 0, max_rows),
                Err(e) => QueryRow::error(clean_error_msg(&e.to_string()), elapsed(), 0, max_rows),
            };
        }

        match self.simple_exec(sql) {
            Ok(affected) => {
                let mut row = QueryRow::notice(
                    &format!("Statement executed. Rows affected: {}", affected),
                    elapsed(),
                    max_rows,
                );
                row.row_count = affected as usize;
                row.rows_affected = Some(affected);
                row
            }
            Err(e) => QueryRow::error(clean_error_msg(&e.to_string()), elapsed(), 0, max_rows),
        }
    }

    fn simple_exec(&self, sql: &str) -> anyhow::Result<u64> {
        let mut client = self
            .client
            .lock()
            .map_err(|e| anyhow::anyhow!("connection locked: {}", e))?;
        client
            .execute(sql, &[])
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    fn run_query_paged(&self, sql: &str, page_offset: usize, page_size: usize) -> QueryRow {
        let start = std::time::Instant::now();
        let inner_sql = sql.trim().trim_end_matches(';').trim();
        let paged_sql = format!(
            "SELECT * FROM ({}) AS _frog_sub LIMIT {} OFFSET {}",
            inner_sql,
            page_size + 1,
            page_offset
        );
        match self.do_query(&paged_sql, page_size, Some(page_size + 1)) {
            Ok(data) => QueryRow::from_query(
                data,
                start.elapsed().as_millis() as u64,
                page_offset,
                page_size,
            ),
            Err(e) => QueryRow::error(
                clean_error_msg(&e.to_string()),
                start.elapsed().as_millis() as u64,
                page_offset,
                page_size,
            ),
        }
    }

    fn fetch_backend_query(&self, pid: i32) -> anyhow::Result<String> {
        let mut client = self
            .client
            .lock()
            .map_err(|e| anyhow::anyhow!("connection locked: {}", e))?;
        let stmt = client
            .prepare("SELECT COALESCE(query, '') FROM pg_stat_activity WHERE pid = $1")
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let rows = client
            .query(&stmt, &[&pid])
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        match rows.into_iter().next() {
            Some(row) => {
                let q: String = row.try_get(0).unwrap_or_default();
                if q.trim().is_empty() {
                    Err(anyhow::anyhow!(
                        "backend {} has no current query (idle)",
                        pid
                    ))
                } else {
                    Ok(q)
                }
            }
            None => Err(anyhow::anyhow!("backend {} is gone", pid)),
        }
    }

    fn run_explain(&self, sql: &str, max_rows: usize) -> QueryRow {
        let start = std::time::Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;
        let inner = sql.trim().trim_end_matches(';').trim();
        if inner.is_empty() {
            return QueryRow::error("Nothing to explain.".into(), elapsed(), 0, max_rows);
        }
        match self.do_query(&pg_explain_sql(inner), max_rows, None) {
            Ok(data) => QueryRow::from_query(data, elapsed(), 0, max_rows),
            Err(e) => QueryRow::error(clean_error_msg(&e.to_string()), elapsed(), 0, max_rows),
        }
    }
}

/// Build the `EXPLAIN <sql>` statement (pure, testable).
pub(crate) fn pg_explain_sql(inner_sql: &str) -> String {
    format!(
        "EXPLAIN {}",
        inner_sql.trim().trim_end_matches(';').trim()
    )
}

impl DbConnection for PgConnection {
    fn execute_query(&self, sql: &str, max_rows: usize) -> QueryRow {
        self.run_query(sql, max_rows)
    }
    fn execute_query_paged(&self, sql: &str, page_offset: usize, page_size: usize) -> QueryRow {
        self.run_query_paged(sql, page_offset, page_size)
    }
    fn cancel_query(&self) -> anyhow::Result<()> {
        self.cancel_token
            .cancel_query(postgres::NoTls)
            .map_err(|e| anyhow::anyhow!("{}", e))
    }
    fn close(&self) -> anyhow::Result<()> {
        // The TCP connection closes when the last Arc<PgConnection> is
        // dropped (on session remove); there is no &self close in the
        // postgres crate (close() consumes the client).
        Ok(())
    }
    fn query_sessions(&self) -> anyhow::Result<Vec<DbSessionInfo>> {
        let sql = "SELECT pid, usename, COALESCE(state,''), COALESCE(client_addr::text,''), \
                   COALESCE(application_name,''), COALESCE(NULLIF(query,''),''), \
                   backend_start::text \
                   FROM pg_stat_activity WHERE datname = current_database() \
                   AND pid <> pg_backend_pid() ORDER BY backend_start DESC";
        let mut client = self
            .client
            .lock()
            .map_err(|e| anyhow::anyhow!("connection locked: {}", e))?;
        let stmt = client
            .prepare(sql)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let rows = client
            .query(&stmt, &[])
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let mut out = Vec::new();
        for row in rows {
            let pid: i32 = row.try_get(0).unwrap_or(0);
            let username: String = row.try_get(1).unwrap_or_default();
            let state: String = row.try_get(2).unwrap_or_default();
            let client_addr: String = row.try_get(3).unwrap_or_default();
            let app: String = row.try_get(4).unwrap_or_default();
            let query: String = row.try_get(5).unwrap_or_default();
            let started: String = row.try_get(6).unwrap_or_default();
            let short_query: String = query.replace('\n', " ").chars().take(40).collect();
            out.push(DbSessionInfo {
                sid: pid,
                serial: 0,
                username,
                status: if state.is_empty() { "?".into() } else { state },
                osuser: client_addr.clone(),
                machine: client_addr,
                program: app,
                sql_id: short_query,
                prev_sql_id: String::new(),
                logon_time: started,
            });
        }
        Ok(out)
    }
    fn session_sql(&self, info: &DbSessionInfo) -> anyhow::Result<String> {
        self.fetch_backend_query(info.sid)
    }
    fn explain_plan(&self, sql: &str, max_rows: usize) -> QueryRow {
        self.run_explain(sql, max_rows)
    }
    fn db_type(&self) -> DbType {
        DbType::Postgres
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connection::is_query_sql_for;

    #[test]
    fn pg_statement_detection() {
        assert!(is_query_sql_for(DbType::Postgres, "select 1"));
        assert!(is_query_sql_for(DbType::Postgres, "with x as (select 1) select * from x"));
        assert!(is_query_sql_for(DbType::Postgres, "values (1), (2)"));
        assert!(is_query_sql_for(DbType::Postgres, "table my_table"));
        assert!(is_query_sql_for(DbType::Postgres, "show server_version"));
        assert!(is_query_sql_for(
            DbType::Postgres,
            "insert into t (a) values (1) returning id"
        ));
        assert!(!is_query_sql_for(DbType::Postgres, "update t set a = 1"));
        assert!(!is_query_sql_for(DbType::Postgres, "-- c\ndelete from t"));
    }

    #[test]
    fn pg_connect_refused_returns_error() {
        // Port 1 is (almost) certainly closed: this exercises the connect
        // plumbing without needing a live server.
        let res = PgConnection::connect("127.0.0.1", 1, "postgres", "u", "p", "(NULL)");
        assert!(res.is_err());
    }

    #[test]
    fn pg_explain_builder() {
        assert_eq!(pg_explain_sql("select 1;"), "EXPLAIN select 1");
        assert!(pg_explain_sql("  select * from t  ").starts_with("EXPLAIN select"));
    }
}
