use super::connection::{
    ColumnInfo, DbConnection, DbSessionInfo, DbType, QueryData, QueryRow,
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

/// Verbose connection failure: what was attempted (never the password),
/// the underlying cause, and a hint for the usual suspects.
pub fn friendly_connect_error(
    host: &str,
    port: u16,
    database: &str,
    user: &str,
    cause: &str,
) -> String {
    let who = if user.is_empty() { "<no user>" } else { user };
    let mut out = format!(
        "Could not connect to postgres://{}@{}:{}/{}\n  cause: {}",
        who, host, port, database, cause
    );
    let lower = cause.to_lowercase();
    let hint = if lower.contains("connection refused") {
        "Is Postgres running and listening there? Try `pg_isready -h <host> -p <port>` \
         and check listen_addresses / the port (PGHOST/PGPORT)."
    } else if lower.contains("password authentication failed")
        || lower.contains("authentication failed")
    {
        "Wrong user or password (PGUSER/PGPASSWORD). The role must exist and be \
         allowed to log in."
    } else if lower.contains("does not exist") && lower.contains("database") {
        "No such database — check the name (PGDATABASE/-D); it is case-sensitive."
    } else if lower.contains("no pg_hba.conf entry") {
        "The server refused this host/user/database/auth-method combination. The DBA \
         must allow it in pg_hba.conf (e.g. `host <db> <user> <addr> scram-sha-256`)."
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "Network timeout — host unreachable or a firewall is blocking the port."
    } else if lower.contains("could not translate")
        || lower.contains("name resolution")
        || lower.contains("nodename nor servname")
        || lower.contains("failed to lookup")
    {
        "Hostname does not resolve — check PGHOST."
    } else if lower.contains("ssl") {
        "TLS problem — frog v1 connects without TLS (NoTls); the server must accept \
         non-SSL connections (sslmode allow/disable equivalent)."
    } else {
        ""
    };
    if !hint.is_empty() {
        out.push_str("\n\nHint: ");
        out.push_str(hint);
    }
    out
}



impl PgConnection {

    /// Run one statement through the simple query protocol. Every value arrives
    /// as server-formatted text, so ALL Postgres types (numeric, money,
    /// intervals, uuids, json, arrays, …) display correctly with no per-type
    /// binary decoding. Returns the display data plus the completion count.
    fn do_simple(&self, sql: &str, max_rows: usize) -> anyhow::Result<(QueryData, u64)> {
        let mut client = self
            .client
            .lock()
            .map_err(|e| anyhow::anyhow!("connection locked: {}", e))?;
        let messages = client
            .simple_query(sql)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let mut columns: Vec<String> = Vec::new();
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut byte_count = 0usize;
        let mut truncated = false;
        let mut completed = 0u64;
        for msg in messages {
            match msg {
                postgres::SimpleQueryMessage::RowDescription(desc) => {
                    columns = desc.iter().map(|c| c.name().to_string()).collect();
                }
                postgres::SimpleQueryMessage::Row(row) => {
                    // Keep one look-ahead row to detect truncation.
                    if rows.len() > max_rows {
                        truncated = true;
                        continue;
                    }
                    let mut vals = Vec::with_capacity(row.len());
                    for i in 0..row.len() {
                        let s = match row.try_get(i) {
                            Ok(Some(v)) => v.to_string(),
                            _ => self.null_display.clone(),
                        };
                        byte_count += s.len();
                        vals.push(s);
                    }
                    rows.push(vals);
                }
            postgres::SimpleQueryMessage::CommandComplete(n) => completed = n,
            _ => {}
        }
        }
        if rows.len() > max_rows {
            truncated = true;
            if let Some(extra) = rows.pop() {
                for val in extra {
                    byte_count = byte_count.saturating_sub(val.len());
                }
            }
        }
        let total = rows.len();
        Ok(((columns, rows, truncated, total, byte_count), completed))
    }
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
        let client = cfg.connect(postgres::NoTls).map_err(|e| {
            anyhow::anyhow!("{}", friendly_connect_error(host, port, database, user, &e.to_string()))
        })?;
        let cancel_token = client.cancel_token();
        Ok(Arc::new(Self {
            client: Mutex::new(client),
            cancel_token,
            null_display: null_display.to_string(),
        }))
    }

    fn run_query(&self, sql: &str, max_rows: usize) -> QueryRow {
        let start = std::time::Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;
        let trimmed = sql.trim();

        if trimmed.eq_ignore_ascii_case("commit") || trimmed.eq_ignore_ascii_case("end") {
            return match self.do_simple("COMMIT", 1) {
                Ok(_) => QueryRow::notice("COMMIT", elapsed(), max_rows),
                Err(e) => QueryRow::error(clean_error_msg(&e.to_string()), elapsed(), 0, max_rows),
            };
        }
        if trimmed.eq_ignore_ascii_case("rollback") || trimmed.eq_ignore_ascii_case("abort") {
            return match self.do_simple("ROLLBACK", 1) {
                Ok(_) => QueryRow::notice("ROLLBACK", elapsed(), max_rows),
                Err(e) => QueryRow::error(clean_error_msg(&e.to_string()), elapsed(), 0, max_rows),
            };
        }

        // One path for everything: statements returning rows (SELECT, but
        // also INSERT/UPDATE/DELETE ... RETURNING, EXPLAIN, SHOW, …) display
        // as tables; the rest become "rows affected" notices from the
        // completion tag.
        match self.do_simple(sql, max_rows) {
            Ok((data, completed)) => {
                if data.0.is_empty() {
                    let mut row = QueryRow::notice(
                        &format!("Statement executed. Rows affected: {}", completed),
                        elapsed(),
                        max_rows,
                    );
                    row.row_count = completed as usize;
                    row.rows_affected = Some(completed);
                    row
                } else {
                    QueryRow::from_query(data, elapsed(), 0, max_rows)
                }
            }
            Err(e) => QueryRow::error(clean_error_msg(&e.to_string()), elapsed(), 0, max_rows),
        }
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
        match self.do_simple(&paged_sql, page_size) {
            Ok((data, _)) => QueryRow::from_query(
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
        match self.do_simple(&pg_explain_sql(inner), max_rows) {
            Ok((data, _)) => QueryRow::from_query(data, elapsed(), 0, max_rows),
            Err(e) => QueryRow::error(clean_error_msg(&e.to_string()), elapsed(), 0, max_rows),
        }
    }

    fn explorer_schemas(&self) -> anyhow::Result<Vec<String>> {
        let mut client = self
            .client
            .lock()
            .map_err(|e| anyhow::anyhow!("connection locked: {}", e))?;
        let stmt = client
            .prepare(
                "SELECT nspname FROM pg_namespace \
                 WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema' \
                 ORDER BY CASE WHEN nspname = 'public' THEN 0 ELSE 1 END, nspname LIMIT 500",
            )
            .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
        let rows = client
            .query(&stmt, &[])
            .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
        let mut out = Vec::new();
        for row in rows {
            let n: String = row.try_get(0).unwrap_or_default();
            if !n.trim().is_empty() {
                out.push(n);
            }
        }
        Ok(out)
    }

    fn explorer_groups(&self, schema: &str) -> anyhow::Result<Vec<(String, u64)>> {
        let mut client = self
            .client
            .lock()
            .map_err(|e| anyhow::anyhow!("connection locked: {}", e))?;
        // Relation-type folders from pg_catalog (covers tables, views,
        // matviews, foreign tables, indexes, sequences). relkinds:
        // r/p = table, v = view, m = matview, f = foreign, i = index,
        // S = sequence.
        let stmt = client
            .prepare(
                "SELECT CASE c.relkind WHEN 'v' THEN 'VIEWS' WHEN 'm' THEN 'MVIEWS' \
                 WHEN 'f' THEN 'FOREIGN' WHEN 'i' THEN 'INDEXES' \
                 WHEN 'S' THEN 'SEQUENCES' ELSE 'TABLES' END, COUNT(*) \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relkind IN ('r','p','v','m','f','i','S') \
                 GROUP BY 1",
            )
            .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
        let rows = client
            .query(&stmt, &[&schema])
            .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
        let mut out = Vec::new();
        for row in rows {
            let g: String = row.try_get(0).unwrap_or_default();
            let n: i64 = row.try_get(1).unwrap_or(0);
            if !g.is_empty() && n > 0 {
                out.push((g, n as u64));
            }
        }
        // Routine folders from information_schema (only visible ones).
        let stmt = client
            .prepare(
                "SELECT CASE WHEN routine_type = 'PROCEDURE' THEN 'PROCEDURES' \
                 ELSE 'FUNCTIONS' END, COUNT(*) \
                 FROM information_schema.routines WHERE routine_schema = $1 GROUP BY 1",
            )
            .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
        let rows = client
            .query(&stmt, &[&schema])
            .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
        for row in rows {
            let g: String = row.try_get(0).unwrap_or_default();
            let n: i64 = row.try_get(1).unwrap_or(0);
            if !g.is_empty() && n > 0 {
                out.push((g, n as u64));
            }
        }
        Ok(out)
    }

    fn explorer_objects(&self, schema: &str, group: &str) -> anyhow::Result<Vec<(String, String)>> {
        let mut client = self
            .client
            .lock()
            .map_err(|e| anyhow::anyhow!("connection locked: {}", e))?;
        // Every folder query takes the schema as its single $1 parameter.
        let sql = match group {
            "TABLES" => {
                "SELECT c.relname, 'TABLE' FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relkind IN ('r','p') \
                 ORDER BY 1 LIMIT 2000"
            }
            "VIEWS" => {
                "SELECT c.relname, 'VIEW' FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relkind = 'v' \
                 ORDER BY 1 LIMIT 2000"
            }
            "MVIEWS" => {
                "SELECT c.relname, 'MVIEW' FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relkind = 'm' \
                 ORDER BY 1 LIMIT 2000"
            }
            "FOREIGN" => {
                "SELECT c.relname, 'FOREIGN TABLE' FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relkind = 'f' \
                 ORDER BY 1 LIMIT 2000"
            }
            "INDEXES" => {
                "SELECT c.relname, 'INDEX' FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relkind = 'i' \
                 ORDER BY 1 LIMIT 2000"
            }
            "SEQUENCES" => {
                "SELECT c.relname, 'SEQUENCE' FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relkind = 'S' \
                 ORDER BY 1 LIMIT 2000"
            }
            "FUNCTIONS" => {
                "SELECT routine_name, 'FUNCTION' FROM information_schema.routines \
                 WHERE routine_schema = $1 AND routine_type = 'FUNCTION' \
                 ORDER BY 1 LIMIT 500"
            }
            "PROCEDURES" => {
                "SELECT routine_name, 'PROCEDURE' FROM information_schema.routines \
                 WHERE routine_schema = $1 AND routine_type = 'PROCEDURE' \
                 ORDER BY 1 LIMIT 500"
            }
            _ => return Ok(Vec::new()),
        };
        let stmt = client
            .prepare(sql)
            .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
        let rows = client
            .query(&stmt, &[&schema])
            .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
        let mut out = Vec::new();
        for row in rows {
            let n: String = row.try_get(0).unwrap_or_default();
            let k: String = row.try_get(1).unwrap_or_default();
            if !n.trim().is_empty() {
                out.push((n, if k.is_empty() { "TABLE".into() } else { k }));
            }
        }
        Ok(out)
    }

    fn explorer_ddl(&self, schema: &str, name: &str, kind: &str) -> anyhow::Result<String> {
        let mut client = self
            .client
            .lock()
            .map_err(|e| anyhow::anyhow!("connection locked: {}", e))?;
        match kind {
            // pg_get_viewdef reconstructs the query behind views AND
            // materialized views.
            "VIEW" | "MVIEW" => {
                let stmt = client
                    .prepare(
                        "SELECT pg_get_viewdef(c.oid, true) FROM pg_class c \
                         JOIN pg_namespace n ON n.oid = c.relnamespace \
                         WHERE n.nspname = $1 AND c.relname = $2",
                    )
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                let rows = client
                    .query(&stmt, &[&schema, &name])
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                match rows.into_iter().next() {
                    Some(row) => {
                        let def: String = row.try_get(0).unwrap_or_default();
                        if def.trim().is_empty() {
                            Err(anyhow::anyhow!(
                                "no definition for {}.{} (no access?)",
                                schema,
                                name
                            ))
                        } else if kind == "MVIEW" {
                            Ok(format!(
                                "CREATE MATERIALIZED VIEW {}.{} AS\n{}",
                                pg_quote_ident(schema),
                                pg_quote_ident(name),
                                def
                            ))
                        } else {
                            Ok(format!(
                                "CREATE OR REPLACE VIEW {}.{} AS\n{}",
                                pg_quote_ident(schema),
                                pg_quote_ident(name),
                                def
                            ))
                        }
                    }
                    None => Err(anyhow::anyhow!("view {}.{} not found", schema, name)),
                }
            }
            "TABLE" | "FOREIGN TABLE" => {
                // Synthesize CREATE TABLE from pg_attribute (same lock).
                let stmt = client
                    .prepare(
                        "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), \
                         NOT a.attnotnull FROM pg_catalog.pg_attribute a \
                         JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
                         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                         WHERE n.nspname = $1 AND c.relname = $2 \
                         AND a.attnum > 0 AND NOT a.attisdropped \
                         ORDER BY a.attnum LIMIT 1000",
                    )
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                let rows = client
                    .query(&stmt, &[&schema, &name])
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                if rows.is_empty() {
                    return Err(anyhow::anyhow!(
                        "no columns visible for {}.{} (no access?)",
                        schema,
                        name
                    ));
                }
                let mut ddl = format!(
                    "CREATE TABLE {}.{} (\n",
                    pg_quote_ident(schema),
                    pg_quote_ident(name)
                );
                for (i, row) in rows.iter().enumerate() {
                    let cn: String = row.try_get(0).unwrap_or_default();
                    let ct: String = row.try_get(1).unwrap_or_default();
                    let nu: bool = row.try_get(2).unwrap_or(true);
                    ddl.push_str(&format!(
                        "  {} {}{}",
                        pg_quote_ident(&cn),
                        if ct.is_empty() { "?" } else { &ct },
                        if nu { "" } else { " NOT NULL" }
                    ));
                    if i + 1 < rows.len() {
                        ddl.push(',');
                    }
                    ddl.push('\n');
                }
                ddl.push_str(");");
                Ok(ddl)
            }
            "INDEX" => {
                let stmt = client
                    .prepare(
                        "SELECT pg_get_indexdef(c.oid) FROM pg_class c \
                         JOIN pg_namespace n ON n.oid = c.relnamespace \
                         WHERE n.nspname = $1 AND c.relname = $2",
                    )
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                let rows = client
                    .query(&stmt, &[&schema, &name])
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                match rows.into_iter().next() {
                    Some(row) => {
                        let def: String = row.try_get(0).unwrap_or_default();
                        if def.trim().is_empty() {
                            Err(anyhow::anyhow!(
                                "no index definition for {}.{} (no access?)",
                                schema,
                                name
                            ))
                        } else {
                            Ok(format!("{};", def.trim_end_matches(';')))
                        }
                    }
                    None => Err(anyhow::anyhow!("index {}.{} not found", schema, name)),
                }
            }
            "SEQUENCE" => {
                // pg_sequences reports NUMERIC (not bigint) values, so cast
                // to text — reading them as i64 fails with a db error.
                let stmt = client
                    .prepare(
                        "SELECT start_value::text, minimum_value::text, \
                         maximum_value::text, increment_by::text, \
                         cycle_option::text, cache_size::text FROM pg_sequences \
                         WHERE schemaname = $1 AND sequencename = $2",
                    )
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                let rows = client
                    .query(&stmt, &[&schema, &name])
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                match rows.into_iter().next() {
                    Some(row) => {
                        let start: String = row.try_get(0).unwrap_or_else(|_| "1".into());
                        let min: String = row.try_get(1).unwrap_or_else(|_| "1".into());
                        let max: String = row.try_get(2).unwrap_or_default();
                        let inc: String = row.try_get(3).unwrap_or_else(|_| "1".into());
                        let cycle: String = row.try_get(4).unwrap_or_default();
                        let cache: String = row.try_get(5).unwrap_or_else(|_| "1".into());
                        let cycled = matches!(
                            cycle.to_ascii_lowercase().as_str(),
                            "t" | "true" | "yes" | "y" | "1"
                        );
                        Ok(format!(
                            "CREATE SEQUENCE {}.{}\n  START WITH {}\n  INCREMENT BY {}\n  \
                             MINVALUE {}\n  MAXVALUE {}\n  {}\n  CACHE {};",
                            pg_quote_ident(schema),
                            pg_quote_ident(name),
                            start,
                            inc,
                            min,
                            max,
                            if cycled { "CYCLE" } else { "NO CYCLE" },
                            cache
                        ))
                    }
                    None => Err(anyhow::anyhow!("sequence {}.{} not found", schema, name)),
                }
            }
            _ => {
                // Functions/procedures: first overload wins (note it).
                let stmt = client
                    .prepare(
                        "SELECT pg_get_functiondef(p.oid) FROM pg_proc p \
                         JOIN pg_namespace n ON n.oid = p.pronamespace \
                         WHERE n.nspname = $1 AND p.proname = $2 \
                         ORDER BY p.oid LIMIT 2",
                    )
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                let rows = client
                    .query(&stmt, &[&schema, &name])
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                let mut it = rows.into_iter();
                match it.next() {
                    Some(row) => {
                        let def: String = row.try_get(0).unwrap_or_default();
                        if def.trim().is_empty() {
                            Err(anyhow::anyhow!(
                                "no source for {} {}.{} (no access?)",
                                kind,
                                schema,
                                name
                            ))
                        } else if it.next().is_some() {
                            Ok(format!(
                                "-- NOTE: {}.{} is overloaded; showing first overload.\n{}",
                                schema, name, def
                            ))
                        } else {
                            Ok(def)
                        }
                    }
                    None => Err(anyhow::anyhow!(
                        "no source for {} {}.{} (no access?)",
                        kind,
                        schema,
                        name
                    )),
                }
            }
        }
    }

    fn explorer_columns(
        &self,
        schema: &str,
        table: &str,
        _kind: &str,
    ) -> anyhow::Result<Vec<ColumnInfo>> {
        let mut client = self
            .client
            .lock()
            .map_err(|e| anyhow::anyhow!("connection locked: {}", e))?;
        // pg_attribute (not information_schema) so materialized views and
        // foreign tables report columns too; format_type renders e.g.
        // 'character varying(50)' / 'timestamp with time zone'.
        let stmt = client
            .prepare(
                "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), \
                 NOT a.attnotnull FROM pg_catalog.pg_attribute a \
                 JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2 \
                 AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum LIMIT 1000",
            )
            .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
        let rows = client
            .query(&stmt, &[&schema, &table])
            .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
        let mut out = Vec::new();
        for row in rows {
            let n: String = row.try_get(0).unwrap_or_default();
            let t: String = row.try_get(1).unwrap_or_default();
            let nu: bool = row.try_get(2).unwrap_or(true);
            if n.trim().is_empty() {
                continue;
            }
            out.push(ColumnInfo {
                name: n,
                data_type: if t.is_empty() { "?".into() } else { t },
                nullable: nu,
            });
        }
        Ok(out)
    }
}

/// Quote a Postgres identifier (schema/table/column) with double quotes.
pub(crate) fn pg_quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Render a driver error with everything the server told us. A bare
/// `to_string()` on some driver errors is just "db error", which hides the
/// SQLSTATE + message needed to diagnose dictionary-query failures.
pub(crate) fn verbose_pg_error(e: &postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => {
            let mut out = format!("{}: {}", db.code().code(), db.message());
            if let Some(d) = db.detail() {
                if !d.is_empty() {
                    out.push_str("\nDetail: ");
                    out.push_str(d);
                }
            }
            if let Some(h) = db.hint() {
                if !h.is_empty() {
                    out.push_str("\nHint: ");
                    out.push_str(h);
                }
            }
            out
        }
        None => e.to_string(),
    }
}

/// Build a 20-row preview query for the explorer detail pane.
pub(crate) fn pg_preview_sql(schema: &str, table: &str, limit: usize) -> String {
    format!(
        "SELECT * FROM {}.{} LIMIT {}",
        pg_quote_ident(schema),
        pg_quote_ident(table),
        limit
    )
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
    fn list_schemas(&self) -> anyhow::Result<Vec<String>> {
        self.explorer_schemas()
    }
    fn list_columns(&self, schema: &str, name: &str, kind: &str) -> anyhow::Result<Vec<ColumnInfo>> {
        self.explorer_columns(schema, name, kind)
    }
    fn list_object_groups(&self, schema: &str) -> anyhow::Result<Vec<(String, u64)>> {
        self.explorer_groups(schema)
    }
    fn list_objects(&self, schema: &str, group: &str) -> anyhow::Result<Vec<(String, String)>> {
        self.explorer_objects(schema, group)
    }
    fn object_ddl(&self, schema: &str, name: &str, kind: &str) -> anyhow::Result<String> {
        self.explorer_ddl(schema, name, kind)
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
    fn pg_ident_quoting_and_preview() {
        assert_eq!(pg_quote_ident("public"), "\"public\"");
        assert_eq!(pg_quote_ident("we\"ird"), "\"we\"\"ird\"");
        assert_eq!(
            pg_preview_sql("public", "emp", 20),
            "SELECT * FROM \"public\".\"emp\" LIMIT 20"
        );
    }

    #[test]
    fn pg_explain_builder() {
        assert_eq!(pg_explain_sql("select 1;"), "EXPLAIN select 1");
        assert!(pg_explain_sql("  select * from t  ").starts_with("EXPLAIN select"));
    }

    #[test]
    fn pg_connect_error_is_verbose_without_password() {
        let msg = friendly_connect_error(
            "db1",
            5432,
            "myapp",
            "bob",
            "connection refused (os error 111)",
        );
        assert!(msg.contains("postgres://bob@db1:5432/myapp"));
        assert!(msg.contains("connection refused"));
        assert!(msg.contains("pg_isready"));
        assert!(!msg.contains("s3cret"));
    }

    #[test]
    fn pg_connect_error_hints() {
        let auth = friendly_connect_error("h", 1, "d", "u", "password authentication failed");
        assert!(auth.contains("PGUSER/PGPASSWORD"));
        let db = friendly_connect_error("h", 1, "d", "u", "database \"x\" does not exist");
        assert!(db.contains("PGDATABASE"));
        let hba = friendly_connect_error("h", 1, "d", "u", "no pg_hba.conf entry for host");
        assert!(hba.contains("pg_hba.conf"));
        let other = friendly_connect_error("h", 1, "d", "u", "some weird thing");
        assert!(other.contains("some weird thing"));
        let nouser = friendly_connect_error("h", 1, "d", "", "boom");
        assert!(nouser.contains("<no user>"));
    }
}
