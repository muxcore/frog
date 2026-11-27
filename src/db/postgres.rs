use super::connection::{
    ColumnInfo, DbConnection, DbSessionInfo, DbType, MetaObject, MetaObjectKind, QueryData,
    QueryRow, TableColumns, TableRef,
};
use super::ddl::{
    format_rich_create_table, DdlTarget, DescribedObject, RichColumn, TableConstraintDdl,
};
use super::perf::PerfSnapshot;
use crate::db::sql_scan::{self, TokenKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub struct PgConnection {
    client: Mutex<postgres::Client>,
    cancel_token: postgres::CancelToken,
    null_display: String,
    autocommit: bool,
    in_tx: AtomicBool,
    use_tls: bool,
}

/// Postgres SSL mode, mirroring libpq `sslmode` (`PGSSLMODE`).
/// `FROG_SSLMODE` is accepted as an alias. Default `disable` preserves
/// frog's historical plaintext behavior; `require`/`verify-ca`/`verify-full`
/// connect over TLS, `prefer`/`allow` try TLS first and fall back to NoTls.
pub(crate) fn pg_sslmode() -> String {
    crate::cli::env_get_public("PGSSLMODE")
        .or_else(|| crate::cli::env_get_public("FROG_SSLMODE"))
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "disable".into())
}

fn sslmode_wants_tls(mode: &str) -> bool {
    matches!(
        mode,
        "require" | "verify-ca" | "verify-full" | "prefer" | "allow"
    )
}

fn sslmode_fallback_allowed(mode: &str) -> bool {
    matches!(mode, "prefer" | "allow" | "prefer-verify")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PgTxAction {
    ExplicitBegin,
    ExplicitCommit { chain: bool },
    ExplicitRollback { chain: bool },
    ImplicitBegin,
    None,
}

/// Classify how a statement interacts with Postgres transactions when
/// `autocommit` is disabled (`-n` / `FROG_NO_AUTOCOMMIT`).
///
/// Mutating statements (`INSERT`/`UPDATE`/`DELETE`/`MERGE`/DDL/`SELECT ... FOR UPDATE`)
/// get an implicit `BEGIN`, while plain `SELECT`s stay outside a transaction so
/// interactive browsing never holds an `idle in transaction` snapshot, and
/// commands forbidden inside transactions (`VACUUM`, `CREATE INDEX CONCURRENTLY`,
/// `CREATE/DROP DATABASE`) execute cleanly on their own.
pub(crate) fn classify_pg_tx_action(sql: &str) -> PgTxAction {
    let all = sql_scan::tokenize(sql);
    let toks: Vec<_> = all
        .iter()
        .filter(|t| t.kind != TokenKind::Comment)
        .collect();
    if toks.is_empty() {
        return PgTxAction::None;
    }
    let word = |idx: usize| -> Option<String> {
        toks.get(idx)
            .filter(|t| {
                !t.is_quoted() && matches!(t.kind, TokenKind::Keyword | TokenKind::Ident { .. })
            })
            .map(|t| t.text.to_ascii_uppercase())
    };
    let has_kw = |kw: &str| -> bool {
        toks.iter().any(|t| {
            !t.is_quoted()
                && matches!(t.kind, TokenKind::Keyword | TokenKind::Ident { .. })
                && t.text.eq_ignore_ascii_case(kw)
        })
    };

    let Some(w0) = word(0) else {
        return PgTxAction::None;
    };
    match w0.as_str() {
        "BEGIN" => PgTxAction::ExplicitBegin,
        "START" if word(1).as_deref() == Some("TRANSACTION") => PgTxAction::ExplicitBegin,
        "COMMIT" | "END" => PgTxAction::ExplicitCommit {
            chain: has_kw("CHAIN") && !has_kw("NO"),
        },
        "ROLLBACK" | "ABORT" => {
            if word(1).as_deref() == Some("TO") {
                PgTxAction::None
            } else {
                PgTxAction::ExplicitRollback {
                    chain: has_kw("CHAIN") && !has_kw("NO"),
                }
            }
        }
        "SAVEPOINT" | "RELEASE" => PgTxAction::None,
        "VACUUM" | "CLUSTER" | "DISCARD" | "SHOW" | "EXPLAIN" | "DESCRIBE" | "VALUES" | "SET"
        | "RESET" | "LISTEN" | "UNLISTEN" | "NOTIFY" | "ANALYZE" | "CHECKPOINT" | "LOAD" => {
            PgTxAction::None
        }
        "REINDEX" if has_kw("CONCURRENTLY") => PgTxAction::None,
        "CREATE" | "DROP" | "ALTER" => {
            if has_kw("CONCURRENTLY")
                || matches!(
                    word(1).as_deref(),
                    Some("DATABASE" | "TABLESPACE" | "SYSTEM" | "SUBSCRIPTION")
                )
            {
                PgTxAction::None
            } else {
                PgTxAction::ImplicitBegin
            }
        }
        "SELECT" | "TABLE" | "WITH" => {
            let locks_rows = (0..toks.len().saturating_sub(1)).any(|i| {
                word(i).as_deref() == Some("FOR")
                    && matches!(
                        word(i + 1).as_deref(),
                        Some("UPDATE" | "SHARE" | "NO" | "KEY")
                    )
            });
            if has_kw("INSERT")
                || has_kw("UPDATE")
                || has_kw("DELETE")
                || has_kw("MERGE")
                || has_kw("INTO")
                || locks_rows
            {
                PgTxAction::ImplicitBegin
            } else {
                PgTxAction::None
            }
        }
        _ => PgTxAction::ImplicitBegin,
    }
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
        "TLS problem — check PGSSLMODE (disable/prefer/require/verify-ca/verify-full; \
          FROG_SSLMODE is an alias). frog connects plaintext by default; managed \
          Postgres with `hostssl` entries needs PGSSLMODE=require."
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
        self.do_simple_tx(sql, max_rows, false)
    }

    fn do_simple_tx(
        &self,
        sql: &str,
        max_rows: usize,
        implicit_begin: bool,
    ) -> anyhow::Result<(QueryData, u64)> {
        let mut client = self
            .client
            .lock()
            .map_err(|e| anyhow::anyhow!("connection locked: {}", e))?;
        let started_here = if implicit_begin && !self.in_tx.load(Ordering::SeqCst) {
            client
                .batch_execute("BEGIN")
                .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
            self.in_tx.store(true, Ordering::SeqCst);
            true
        } else {
            false
        };
        let messages = match client.simple_query(sql) {
            Ok(msgs) => msgs,
            Err(e) => {
                // If this statement itself triggered the implicit BEGIN and
                // failed immediately, roll back the empty aborted transaction so
                // the session does not stay stuck in SQLSTATE 25P02.
                if started_here {
                    let _ = client.batch_execute("ROLLBACK");
                    self.in_tx.store(false, Ordering::SeqCst);
                }
                return Err(anyhow::anyhow!("{}", verbose_pg_error(&e)));
            }
        };
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
        Self::connect_with_autocommit(host, port, database, user, password, true, null_display)
    }

    pub fn connect_with_autocommit(
        host: &str,
        port: u16,
        database: &str,
        user: &str,
        password: &str,
        autocommit: bool,
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
        let mode = pg_sslmode();
        let wants_tls = sslmode_wants_tls(&mode);
        let (client, use_tls) = if !wants_tls {
            let client = cfg.connect(postgres::NoTls).map_err(|e| {
                anyhow::anyhow!(
                    "{}",
                    friendly_connect_error(host, port, database, user, &e.to_string())
                )
            })?;
            (client, false)
        } else {
            let mk_tls = || {
                let inner = native_tls::TlsConnector::new()
                    .map_err(|e| anyhow::anyhow!("TLS connector setup failed: {}", e))?;
                Ok::<_, anyhow::Error>(postgres_native_tls::MakeTlsConnector::new(inner))
            };
            match cfg.connect(mk_tls()?) {
                Ok(client) => (client, true),
                Err(e) if sslmode_fallback_allowed(&mode) => {
                    let client = cfg.connect(postgres::NoTls).map_err(|e2| {
                        anyhow::anyhow!(
                            "{}\n(TLS attempt also failed: {})",
                            friendly_connect_error(host, port, database, user, &e2.to_string()),
                            e
                        )
                    })?;
                    (client, false)
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "{}",
                        friendly_connect_error(host, port, database, user, &e.to_string())
                    ));
                }
            }
        };
        let cancel_token = client.cancel_token();
        Ok(Arc::new(Self {
            client: Mutex::new(client),
            cancel_token,
            null_display: null_display.to_string(),
            autocommit,
            in_tx: AtomicBool::new(false),
            use_tls,
        }))
    }

    fn run_query(&self, sql: &str, max_rows: usize) -> QueryRow {
        let start = std::time::Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;
        let cmd = super::connection::clean_command_word(sql);

        if cmd.eq_ignore_ascii_case("commit") || cmd.eq_ignore_ascii_case("end") {
            let res = self.do_simple("COMMIT", 1);
            self.in_tx.store(false, Ordering::SeqCst);
            return match res {
                Ok(_) => QueryRow::notice("COMMIT", elapsed(), max_rows),
                Err(e) => QueryRow::error(clean_error_msg(&e.to_string()), elapsed(), 0, max_rows),
            };
        }
        if cmd.eq_ignore_ascii_case("rollback") || cmd.eq_ignore_ascii_case("abort") {
            let res = self.do_simple("ROLLBACK", 1);
            self.in_tx.store(false, Ordering::SeqCst);
            return match res {
                Ok(_) => QueryRow::notice("ROLLBACK", elapsed(), max_rows),
                Err(e) => QueryRow::error(clean_error_msg(&e.to_string()), elapsed(), 0, max_rows),
            };
        }

        let tx_action = classify_pg_tx_action(sql);
        let implicit_begin = !self.autocommit && tx_action == PgTxAction::ImplicitBegin;

        // One path for everything: statements returning rows (SELECT, but
        // also INSERT/UPDATE/DELETE ... RETURNING, EXPLAIN, SHOW, …) display
        // as tables; the rest become "rows affected" notices from the
        // completion tag.
        match self.do_simple_tx(sql, max_rows, implicit_begin) {
            Ok((data, completed)) => {
                match tx_action {
                    PgTxAction::ExplicitBegin => self.in_tx.store(true, Ordering::SeqCst),
                    PgTxAction::ExplicitCommit { chain }
                    | PgTxAction::ExplicitRollback { chain } => {
                        self.in_tx.store(chain, Ordering::SeqCst);
                    }
                    PgTxAction::ImplicitBegin | PgTxAction::None => {}
                }
                if data.0.is_empty() {
                    let mut row = QueryRow::notice(
                        &format!("Statement executed. Rows affected: {}", completed),
                        elapsed(),
                        max_rows,
                    );
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
        Self::ddl_with_client(&mut client, schema, name, kind)
    }

    fn ddl_with_client(
        client: &mut postgres::Client,
        schema: &str,
        name: &str,
        kind: &str,
    ) -> anyhow::Result<String> {
        match kind.to_ascii_uppercase().as_str() {
            // pg_get_viewdef reconstructs the query behind views AND materialized views.
            "VIEW" | "MVIEW" | "MATERIALIZED VIEW" => {
                let stmt = client
                    .prepare(
                        "SELECT c.oid::bigint, pg_get_viewdef(c.oid, true) FROM pg_class c \
                         JOIN pg_namespace n ON n.oid = c.relnamespace \
                         WHERE n.nspname = $1 AND c.relname = $2",
                    )
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                let rows = client
                    .query(&stmt, &[&schema, &name])
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                match rows.into_iter().next() {
                    Some(row) => {
                        let oid: i64 = row.try_get(0).unwrap_or(0);
                        let def: String = row.try_get(1).unwrap_or_default();
                        if def.trim().is_empty() {
                            return Err(anyhow::anyhow!(
                                "no definition for {}.{} (no access?)",
                                schema,
                                name
                            ));
                        }
                        let is_mv = kind.eq_ignore_ascii_case("MVIEW")
                            || kind.eq_ignore_ascii_case("MATERIALIZED VIEW");
                        let mut out = if is_mv {
                            format!(
                                "CREATE MATERIALIZED VIEW {}.{} AS\n{}",
                                pg_quote_ident(schema),
                                pg_quote_ident(name),
                                def.trim_end()
                            )
                        } else {
                            format!(
                                "CREATE OR REPLACE VIEW {}.{} AS\n{}",
                                pg_quote_ident(schema),
                                pg_quote_ident(name),
                                def.trim_end()
                            )
                        };
                        if !out.ends_with(';') {
                            out.push(';');
                        }
                        if is_mv && oid > 0 {
                            let idx_ddls = Self::relation_indexes_ddl(client, oid);
                            if !idx_ddls.is_empty() {
                                out.push_str("\n\n-- Indexes\n");
                                out.push_str(&idx_ddls.join("\n"));
                            }
                        }
                        Ok(out)
                    }
                    None => Err(anyhow::anyhow!("view {}.{} not found", schema, name)),
                }
            }
            "TABLE" | "PARTITIONED TABLE" | "FOREIGN TABLE" => {
                let rel_stmt = client
                    .prepare(
                        "SELECT c.oid::bigint, c.relkind::text, \
                                CASE WHEN c.relkind = 'p' THEN pg_get_partkeydef(c.oid) ELSE '' END, \
                                COALESCE(obj_description(c.oid, 'pg_class'), '') \
                         FROM pg_catalog.pg_class c \
                         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                         WHERE n.nspname = $1 AND c.relname = $2",
                    )
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                let rel_rows = client
                    .query(&rel_stmt, &[&schema, &name])
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                let Some(rel_row) = rel_rows.into_iter().next() else {
                    return Err(anyhow::anyhow!("table {}.{} not found", schema, name));
                };
                let rel_oid: i64 = rel_row.try_get(0).unwrap_or(0);
                let relkind: String = rel_row.try_get(1).unwrap_or_default();
                let part_key: String = rel_row.try_get(2).unwrap_or_default();
                let tbl_comment: String = rel_row.try_get(3).unwrap_or_default();

                let col_stmt = client
                    .prepare(
                        "SELECT a.attname, \
                                pg_catalog.format_type(a.atttypid, a.atttypmod), \
                                NOT a.attnotnull, \
                                COALESCE(pg_catalog.pg_get_expr(ad.adbin, ad.adrelid), ''), \
                                a.attidentity::text, \
                                COALESCE(col_description(a.attrelid, a.attnum), '') \
                         FROM pg_catalog.pg_attribute a \
                         LEFT JOIN pg_catalog.pg_attrdef ad \
                           ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum \
                         WHERE a.attrelid = $1::oid \
                           AND a.attnum > 0 AND NOT a.attisdropped \
                         ORDER BY a.attnum LIMIT 1000",
                    )
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                let rows = client
                    .query(&col_stmt, &[&(rel_oid as u32)])
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                if rows.is_empty() {
                    return Err(anyhow::anyhow!(
                        "no columns visible for {}.{} (no access?)",
                        schema,
                        name
                    ));
                }

                let mut cols = Vec::with_capacity(rows.len());
                let mut col_comments = Vec::new();
                for row in &rows {
                    let cname: String = row.try_get(0).unwrap_or_default();
                    let mut ctype: String = row.try_get(1).unwrap_or_default();
                    let nullable: bool = row.try_get(2).unwrap_or(true);
                    let def_expr: String = row.try_get(3).unwrap_or_default();
                    let identity: String = row.try_get(4).unwrap_or_default();
                    let ccomment: String = row.try_get(5).unwrap_or_default();

                    if identity == "a" {
                        ctype.push_str(" GENERATED ALWAYS AS IDENTITY");
                    } else if identity == "d" {
                        ctype.push_str(" GENERATED BY DEFAULT AS IDENTITY");
                    }
                    cols.push(RichColumn {
                        name: cname.clone(),
                        full_type: ctype,
                        default_expr: if identity.is_empty() && !def_expr.trim().is_empty() {
                            Some(def_expr)
                        } else {
                            None
                        },
                        nullable,
                    });
                    if !ccomment.trim().is_empty() {
                        col_comments.push(format!(
                            "COMMENT ON COLUMN {}.{}.{} IS '{}';",
                            pg_quote_ident(schema),
                            pg_quote_ident(name),
                            pg_quote_ident(&cname),
                            ccomment.replace('\'', "''")
                        ));
                    }
                }

                // Table constraints (PK, UNIQUE, FK, CHECK).
                let mut constraints = Vec::new();
                if let Ok(con_stmt) = client.prepare(
                    "SELECT conname, pg_get_constraintdef(oid, true) \
                     FROM pg_constraint \
                     WHERE conrelid = $1::oid AND contype IN ('p','u','f','c') \
                     ORDER BY CASE contype WHEN 'p' THEN 0 WHEN 'u' THEN 1 WHEN 'f' THEN 2 ELSE 3 END, conname",
                ) {
                    if let Ok(con_rows) = client.query(&con_stmt, &[&(rel_oid as u32)]) {
                        for r in con_rows {
                            let cname: String = r.try_get(0).unwrap_or_default();
                            let cdef: String = r.try_get(1).unwrap_or_default();
                            if !cdef.trim().is_empty() {
                                constraints.push(TableConstraintDdl {
                                    name: (!cname.is_empty()).then_some(cname),
                                    definition: cdef,
                                });
                            }
                        }
                    }
                }

                let mut extra = Vec::new();
                let idx_ddls = Self::relation_indexes_ddl(client, rel_oid);
                if !idx_ddls.is_empty() {
                    extra.push("-- Indexes".to_string());
                    extra.extend(idx_ddls);
                }
                if !tbl_comment.trim().is_empty() || !col_comments.is_empty() {
                    extra.push("-- Comments".to_string());
                    if !tbl_comment.trim().is_empty() {
                        extra.push(format!(
                            "COMMENT ON TABLE {}.{} IS '{}';",
                            pg_quote_ident(schema),
                            pg_quote_ident(name),
                            tbl_comment.replace('\'', "''")
                        ));
                    }
                    extra.extend(col_comments);
                }

                let table_kw = if relkind == "f" || kind.eq_ignore_ascii_case("FOREIGN TABLE") {
                    "FOREIGN TABLE"
                } else {
                    "TABLE"
                };
                let qualified = format!("{}.{}", pg_quote_ident(schema), pg_quote_ident(name));
                Ok(format_rich_create_table(
                    table_kw,
                    &qualified,
                    &cols,
                    &constraints,
                    (!part_key.trim().is_empty()).then_some(part_key.as_str()),
                    &extra,
                    pg_quote_ident,
                ))
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
            "TRIGGER" => {
                let stmt = client
                    .prepare(
                        "SELECT pg_get_triggerdef(t.oid, true) \
                         FROM pg_trigger t \
                         JOIN pg_class c ON c.oid = t.tgrelid \
                         JOIN pg_namespace n ON n.oid = c.relnamespace \
                         WHERE n.nspname = $1 AND t.tgname = $2 AND NOT t.tgisinternal \
                         ORDER BY t.oid",
                    )
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                let rows = client
                    .query(&stmt, &[&schema, &name])
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                let defs: Vec<String> = rows
                    .into_iter()
                    .filter_map(|r| r.try_get::<_, String>(0).ok())
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| format!("{};", s.trim_end_matches(';')))
                    .collect();
                if defs.is_empty() {
                    Err(anyhow::anyhow!("trigger {}.{} not found", schema, name))
                } else {
                    Ok(defs.join("\n\n"))
                }
            }
            "TYPE" => {
                let stmt = client
                    .prepare(
                        "SELECT t.oid::bigint, t.typtype::text \
                         FROM pg_type t \
                         JOIN pg_namespace n ON n.oid = t.typnamespace \
                         WHERE n.nspname = $1 AND t.typname = $2",
                    )
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                let rows = client
                    .query(&stmt, &[&schema, &name])
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                let Some(row) = rows.into_iter().next() else {
                    return Err(anyhow::anyhow!("type {}.{} not found", schema, name));
                };
                let oid: i64 = row.try_get(0).unwrap_or(0);
                let typtype: String = row.try_get(1).unwrap_or_default();
                let qual = format!("{}.{}", pg_quote_ident(schema), pg_quote_ident(name));
                if typtype == "e" {
                    let estmt = client
                        .prepare(
                            "SELECT enumlabel FROM pg_enum WHERE enumtypid = $1::oid \
                             ORDER BY enumsortorder",
                        )
                        .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                    let erows = client
                        .query(&estmt, &[&(oid as u32)])
                        .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                    let labels: Vec<String> = erows
                        .into_iter()
                        .filter_map(|r| r.try_get::<_, String>(0).ok())
                        .map(|l| format!("  '{}'", l.replace('\'', "''")))
                        .collect();
                    Ok(format!(
                        "CREATE TYPE {} AS ENUM (\n{}\n);",
                        qual,
                        labels.join(",\n")
                    ))
                } else {
                    Ok(format!("-- Postgres type {} (typtype = '{}')", qual, typtype))
                }
            }
            _ => {
                // Functions/procedures: emit all overloads cleanly.
                let stmt = client
                    .prepare(
                        "SELECT pg_get_functiondef(p.oid) FROM pg_proc p \
                         JOIN pg_namespace n ON n.oid = p.pronamespace \
                         WHERE n.nspname = $1 AND p.proname = $2 \
                         ORDER BY p.oid LIMIT 10",
                    )
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                let rows = client
                    .query(&stmt, &[&schema, &name])
                    .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
                let defs: Vec<String> = rows
                    .into_iter()
                    .filter_map(|r| r.try_get::<_, String>(0).ok())
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| {
                        let t = s.trim_end();
                        if t.ends_with(';') {
                            t.to_string()
                        } else {
                            format!("{};", t)
                        }
                    })
                    .collect();
                if defs.is_empty() {
                    Err(anyhow::anyhow!(
                        "no source for {} {}.{} (no access?)",
                        kind,
                        schema,
                        name
                    ))
                } else if defs.len() > 1 {
                    let total = defs.len();
                    let parts: Vec<String> = defs
                        .into_iter()
                        .enumerate()
                        .map(|(i, d)| format!("-- Overload {} of {}\n{}", i + 1, total, d))
                        .collect();
                    Ok(parts.join("\n\n"))
                } else {
                    Ok(defs.into_iter().next().unwrap_or_default())
                }
            }
        }
    }

    fn relation_indexes_ddl(client: &mut postgres::Client, rel_oid: i64) -> Vec<String> {
        let Ok(stmt) = client.prepare(
            "SELECT pg_get_indexdef(i.indexrelid) \
             FROM pg_index i \
             JOIN pg_class ic ON ic.oid = i.indexrelid \
             WHERE i.indrelid = $1::oid \
               AND NOT i.indisprimary \
               AND NOT EXISTS ( \
                 SELECT 1 FROM pg_constraint c \
                 WHERE c.conindid = i.indexrelid AND c.contype IN ('p','u') \
               ) \
             ORDER BY ic.relname",
        ) else {
            return Vec::new();
        };
        let Ok(rows) = client.query(&stmt, &[&(rel_oid as u32)]) else {
            return Vec::new();
        };
        rows.into_iter()
            .filter_map(|r| r.try_get::<_, String>(0).ok())
            .filter(|s| !s.trim().is_empty())
            .map(|s| format!("{};", s.trim_end_matches(';')))
            .collect()
    }

    /// F4 Toad-style Describe for Postgres: resolves relations (tables, indexes,
    /// views, matviews, sequences, foreign tables) via `search_path` / `pg_class`,
    /// plus functions/procedures, triggers and custom types.
    fn describe_pg_object(&self, target: &DdlTarget) -> anyhow::Result<DescribedObject> {
        let start = std::time::Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;
        let mut client = self
            .client
            .lock()
            .map_err(|e| anyhow::anyhow!("connection locked: {}", e))?;

        let norm_name = if target.name_quoted {
            target.name.clone()
        } else {
            target.name.to_lowercase()
        };

        if let (Some(s), Some(k)) = (target.schema.as_deref(), target.kind_hint.as_deref()) {
            let ns = if target.schema_quoted {
                s.to_string()
            } else {
                s.to_lowercase()
            };
            let ddl = Self::ddl_with_client(&mut client, &ns, &norm_name, k)?;
            let summary = Self::pg_object_summary(&mut client, &ns, &norm_name, k);
            return Ok(DescribedObject {
                schema: ns,
                name: norm_name,
                kind: k.to_uppercase(),
                ddl,
                summary,
                elapsed_ms: elapsed(),
            });
        }

        let resolved = match target.schema.as_deref() {
            Some(s) => {
                let ns = if target.schema_quoted {
                    s.to_string()
                } else {
                    s.to_lowercase()
                };
                Self::find_pg_in_schema(&mut client, &ns, &norm_name)
                    .map(|(n, k)| (ns, n, k))
            }
            None => Self::find_pg_unqualified(&mut client, target, &norm_name),
        };

        let Some((schema, obj_name, kind)) = resolved else {
            return Err(anyhow::anyhow!(
                "Object '{}' not found in PostgreSQL catalog (check spelling or search_path)",
                target.display_label()
            ));
        };

        let ddl = Self::ddl_with_client(&mut client, &schema, &obj_name, &kind)?;
        let summary = Self::pg_object_summary(&mut client, &schema, &obj_name, &kind);

        Ok(DescribedObject {
            schema,
            name: obj_name,
            kind,
            ddl,
            summary,
            elapsed_ms: elapsed(),
        })
    }

    fn relkind_label(relkind: &str) -> &'static str {
        match relkind {
            "r" => "TABLE",
            "p" => "PARTITIONED TABLE",
            "v" => "VIEW",
            "m" => "MVIEW",
            "f" => "FOREIGN TABLE",
            "i" | "I" => "INDEX",
            "S" => "SEQUENCE",
            _ => "TABLE",
        }
    }

    fn find_pg_in_schema(
        client: &mut postgres::Client,
        schema: &str,
        name: &str,
    ) -> Option<(String, String)> {
        // 1. Relation in pg_class (table, view, mview, index, sequence, foreign table).
        if let Ok(stmt) = client.prepare(
            "SELECT c.relname, c.relkind::text FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 \
               AND c.relkind IN ('r','p','v','m','f','i','I','S')",
        ) {
            if let Ok(rows) = client.query(&stmt, &[&schema, &name]) {
                if let Some(r) = rows.into_iter().next() {
                    let rn: String = r.try_get(0).unwrap_or_default();
                    let rk: String = r.try_get(1).unwrap_or_default();
                    if !rn.is_empty() {
                        return Some((rn, Self::relkind_label(&rk).to_string()));
                    }
                }
            }
        }
        // 2. Function / procedure.
        if let Ok(stmt) = client.prepare(
            "SELECT p.proname, CASE p.prokind WHEN 'p' THEN 'PROCEDURE' ELSE 'FUNCTION' END \
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = $1 AND p.proname = $2 LIMIT 1",
        ) {
            if let Ok(rows) = client.query(&stmt, &[&schema, &name]) {
                if let Some(r) = rows.into_iter().next() {
                    let pn: String = r.try_get(0).unwrap_or_default();
                    let pk: String = r.try_get(1).unwrap_or_default();
                    if !pn.is_empty() {
                        return Some((pn, pk));
                    }
                }
            }
        }
        // 3. Trigger on any table in this schema.
        if let Ok(stmt) = client.prepare(
            "SELECT t.tgname FROM pg_trigger t \
             JOIN pg_class c ON c.oid = t.tgrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND t.tgname = $2 AND NOT t.tgisinternal LIMIT 1",
        ) {
            if let Ok(rows) = client.query(&stmt, &[&schema, &name]) {
                if let Some(r) = rows.into_iter().next() {
                    let tn: String = r.try_get(0).unwrap_or_default();
                    if !tn.is_empty() {
                        return Some((tn, "TRIGGER".into()));
                    }
                }
            }
        }
        // 4. Enum/domain/composite type.
        if let Ok(stmt) = client.prepare(
            "SELECT t.typname FROM pg_type t \
             JOIN pg_namespace n ON n.oid = t.typnamespace \
             WHERE n.nspname = $1 AND t.typname = $2 AND t.typtype IN ('e','d') LIMIT 1",
        ) {
            if let Ok(rows) = client.query(&stmt, &[&schema, &name]) {
                if let Some(r) = rows.into_iter().next() {
                    let tn: String = r.try_get(0).unwrap_or_default();
                    if !tn.is_empty() {
                        return Some((tn, "TYPE".into()));
                    }
                }
            }
        }
        None
    }

    fn find_pg_unqualified(
        client: &mut postgres::Client,
        target: &DdlTarget,
        norm_name: &str,
    ) -> Option<(String, String, String)> {
        // 1. Resolve via `to_regclass` so search_path is honored identically to SQL.
        let reg_arg = if target.name_quoted {
            pg_quote_ident(&target.name)
        } else {
            target.name.clone()
        };
        if let Ok(stmt) = client.prepare(
            "SELECT n.nspname, c.relname, c.relkind::text \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.oid = to_regclass($1)",
        ) {
            if let Ok(rows) = client.query(&stmt, &[&reg_arg]) {
                if let Some(r) = rows.into_iter().next() {
                    let ns: String = r.try_get(0).unwrap_or_default();
                    let rn: String = r.try_get(1).unwrap_or_default();
                    let rk: String = r.try_get(2).unwrap_or_default();
                    if !ns.is_empty() && !rn.is_empty() {
                        return Some((ns, rn, Self::relkind_label(&rk).to_string()));
                    }
                }
            }
        }

        // 2. Routine on search_path (or any user schema).
        if let Ok(stmt) = client.prepare(
            "SELECT n.nspname, p.proname, \
                    CASE p.prokind WHEN 'p' THEN 'PROCEDURE' ELSE 'FUNCTION' END \
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE p.proname = $1 AND n.nspname NOT IN ('pg_catalog','information_schema') \
             ORDER BY CASE WHEN n.nspname = ANY(current_schemas(false)) THEN 0 ELSE 1 END, p.oid \
             LIMIT 1",
        ) {
            if let Ok(rows) = client.query(&stmt, &[&norm_name]) {
                if let Some(r) = rows.into_iter().next() {
                    let ns: String = r.try_get(0).unwrap_or_default();
                    let pn: String = r.try_get(1).unwrap_or_default();
                    let pk: String = r.try_get(2).unwrap_or_default();
                    if !ns.is_empty() && !pn.is_empty() {
                        return Some((ns, pn, pk));
                    }
                }
            }
        }

        // 3. Relation in any non-system schema (catches indexes/tables outside search_path).
        if let Ok(stmt) = client.prepare(
            "SELECT n.nspname, c.relname, c.relkind::text \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relname = $1 \
               AND c.relkind IN ('r','p','v','m','f','i','I','S') \
               AND n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema' \
             ORDER BY CASE WHEN n.nspname = 'public' THEN 0 ELSE 1 END, n.nspname \
             LIMIT 1",
        ) {
            if let Ok(rows) = client.query(&stmt, &[&norm_name]) {
                if let Some(r) = rows.into_iter().next() {
                    let ns: String = r.try_get(0).unwrap_or_default();
                    let rn: String = r.try_get(1).unwrap_or_default();
                    let rk: String = r.try_get(2).unwrap_or_default();
                    if !ns.is_empty() && !rn.is_empty() {
                        return Some((ns, rn, Self::relkind_label(&rk).to_string()));
                    }
                }
            }
        }

        // 4. Trigger in any user schema.
        if let Ok(stmt) = client.prepare(
            "SELECT n.nspname, t.tgname FROM pg_trigger t \
             JOIN pg_class c ON c.oid = t.tgrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE t.tgname = $1 AND NOT t.tgisinternal \
             ORDER BY CASE WHEN n.nspname = ANY(current_schemas(false)) THEN 0 ELSE 1 END \
             LIMIT 1",
        ) {
            if let Ok(rows) = client.query(&stmt, &[&norm_name]) {
                if let Some(r) = rows.into_iter().next() {
                    let ns: String = r.try_get(0).unwrap_or_default();
                    let tn: String = r.try_get(1).unwrap_or_default();
                    if !ns.is_empty() && !tn.is_empty() {
                        return Some((ns, tn, "TRIGGER".into()));
                    }
                }
            }
        }

        None
    }

    fn pg_object_summary(
        client: &mut postgres::Client,
        schema: &str,
        name: &str,
        kind: &str,
    ) -> Option<String> {
        match kind.to_ascii_uppercase().as_str() {
            "TABLE" | "PARTITIONED TABLE" | "MVIEW" | "MATERIALIZED VIEW" => {
                let stmt = client
                    .prepare(
                        "SELECT pg_size_pretty(pg_total_relation_size(c.oid)), \
                                pg_size_pretty(pg_table_size(c.oid)), \
                                pg_size_pretty(pg_indexes_size(c.oid)), \
                                GREATEST(c.reltuples::bigint, 0) \
                         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                         WHERE n.nspname = $1 AND c.relname = $2",
                    )
                    .ok()?;
                let row = client.query(&stmt, &[&schema, &name]).ok()?.into_iter().next()?;
                let total_sz: String = row.try_get(0).unwrap_or_default();
                let tbl_sz: String = row.try_get(1).unwrap_or_default();
                let idx_sz: String = row.try_get(2).unwrap_or_default();
                let tuples: i64 = row.try_get(3).unwrap_or(0);
                Some(format!(
                    "Total: {} (Heap: {}, Indexes: {}) · Est. Rows: {}",
                    total_sz, tbl_sz, idx_sz, tuples
                ))
            }
            "INDEX" => {
                let stmt = client
                    .prepare(
                        "SELECT tc.relname, i.indisunique, i.indisvalid, \
                                pg_size_pretty(pg_relation_size(ic.oid)) \
                         FROM pg_index i \
                         JOIN pg_class ic ON ic.oid = i.indexrelid \
                         JOIN pg_class tc ON tc.oid = i.indrelid \
                         JOIN pg_namespace n ON n.oid = ic.relnamespace \
                         WHERE n.nspname = $1 AND ic.relname = $2",
                    )
                    .ok()?;
                let row = client.query(&stmt, &[&schema, &name]).ok()?.into_iter().next()?;
                let tname: String = row.try_get(0).unwrap_or_default();
                let uniq: bool = row.try_get(1).unwrap_or(false);
                let valid: bool = row.try_get(2).unwrap_or(true);
                let sz: String = row.try_get(3).unwrap_or_default();
                Some(format!(
                    "On {}.{} · {} · {} · Size: {}",
                    schema,
                    tname,
                    if uniq { "UNIQUE" } else { "NON-UNIQUE" },
                    if valid { "VALID" } else { "INVALID" },
                    sz
                ))
            }
            _ => None,
        }
    }

    /// F6 Performance dashboard: live Postgres activity/locks/waits/cache snapshot.
    fn collect_pg_perf_snapshot(&self) -> anyhow::Result<PerfSnapshot> {
        let start = std::time::Instant::now();
        let mut client = self
            .client
            .lock()
            .map_err(|e| anyhow::anyhow!("connection locked: {}", e))?;

        let mut snap = PerfSnapshot::default();

        let act_stmt = client
            .prepare(
                "SELECT
                   COUNT(*)::bigint,
                   COUNT(*) FILTER (WHERE state = 'active')::bigint,
                   COUNT(*) FILTER (WHERE state LIKE 'idle in transaction%')::bigint,
                   COUNT(*) FILTER (WHERE cardinality(pg_blocking_pids(pid)) > 0)::bigint,
                   COALESCE(MAX(EXTRACT(EPOCH FROM (clock_timestamp() - query_start))::bigint)
                            FILTER (WHERE state = 'active'), 0)::bigint
                 FROM pg_stat_activity
                 WHERE datname = current_database()
                   AND pid <> pg_backend_pid()",
            )
            .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
        if let Some(row) = client
            .query(&act_stmt, &[])
            .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?
            .into_iter()
            .next()
        {
            snap.total_sessions = row.try_get::<_, i64>(0).unwrap_or(0).max(0) as u64;
            snap.active_sessions = row.try_get::<_, i64>(1).unwrap_or(0).max(0) as u64;
            snap.idle_in_tx_sessions = row.try_get::<_, i64>(2).unwrap_or(0).max(0) as u64;
            snap.blocked_sessions = row.try_get::<_, i64>(3).unwrap_or(0).max(0) as u64;
            snap.max_active_secs = row.try_get::<_, i64>(4).unwrap_or(0).max(0) as u64;
        }

        if snap.blocked_sessions > 0 {
            if let Ok(bstmt) = client.prepare(
                "SELECT COUNT(DISTINCT bp.pid)::bigint \
                 FROM pg_stat_activity w \
                 CROSS JOIN LATERAL unnest(pg_blocking_pids(w.pid)) AS bp(pid) \
                 WHERE w.datname = current_database()",
            ) {
                if let Ok(brows) = client.query(&bstmt, &[]) {
                    if let Some(r) = brows.into_iter().next() {
                        snap.blocking_sessions = r.try_get::<_, i64>(0).unwrap_or(0).max(0) as u64;
                    }
                }
            }
        }

        if let Ok(wstmt) = client.prepare(
            "SELECT COALESCE(wait_event_type, 'CPU'), COUNT(*)::bigint \
             FROM pg_stat_activity \
             WHERE datname = current_database() \
               AND pid <> pg_backend_pid() \
               AND state = 'active' \
             GROUP BY 1 ORDER BY 2 DESC",
        ) {
            if let Ok(wrows) = client.query(&wstmt, &[]) {
                for r in wrows {
                    let cls: String = r.try_get(0).unwrap_or_default();
                    let cnt: i64 = r.try_get(1).unwrap_or(0);
                    if !cls.is_empty() && cnt > 0 {
                        snap.wait_breakdown.push((cls, cnt as u64));
                    }
                }
            }
        }

        if let Ok(dstmt) = client.prepare(
            "SELECT
               ROUND(COALESCE(blks_hit::numeric * 100.0 / NULLIF(blks_hit + blks_read, 0), 100.0), 1)::text,
               ROUND(pg_database_size(current_database())::numeric / 1048576.0, 1)::text,
               COALESCE((SELECT pg_size_pretty(setting::bigint * CASE unit
                 WHEN '8kB' THEN 8192 WHEN 'kB' THEN 1024 WHEN 'MB' THEN 1048576 ELSE 1 END)
                 FROM pg_settings WHERE name = 'shared_buffers'), '?')
             FROM pg_stat_database
             WHERE datname = current_database()",
        ) {
            if let Ok(drows) = client.query(&dstmt, &[]) {
                if let Some(r) = drows.into_iter().next() {
                    let hit_s: String = r.try_get(0).unwrap_or_default();
                    let db_mb_s: String = r.try_get(1).unwrap_or_default();
                    let shbuf: String = r.try_get(2).unwrap_or_default();
                    snap.cache_hit_pct = hit_s.parse::<f64>().ok();
                    let db_mb = db_mb_s.parse::<f64>().ok();
                    snap.mem_used_mb = db_mb;
                    snap.mem_detail = match db_mb {
                        Some(mb) => format!("shared_buffers {} · DB {:.0}M", shbuf, mb),
                        None => format!("shared_buffers {}", shbuf),
                    };
                }
            }
        }

        snap.elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(snap)
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

    /// Completion: first schema on `search_path`.
    pub fn current_schema(&self) -> anyhow::Result<String> {
        let mut client = self
            .client
            .lock()
            .map_err(|e| anyhow::anyhow!("connection locked: {}", e))?;
        let row = client
            .query_one("SELECT current_schema()", &[])
            .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
        let name: Option<String> = row.try_get(0).unwrap_or(None);
        name.filter(|n| !n.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("search_path is empty: no current schema"))
    }

    /// Completion: all object names of one schema in one query.
    pub fn list_schema_objects(&self, schema: &str) -> anyhow::Result<Vec<MetaObject>> {
        let mut client = self
            .client
            .lock()
            .map_err(|e| anyhow::anyhow!("connection locked: {}", e))?;
        let stmt = client
            .prepare(
                "SELECT * FROM ( \
                   SELECT c.relname AS name, \
                          CASE c.relkind WHEN 'r' THEN 'TABLE' WHEN 'p' THEN 'TABLE' \
                               WHEN 'v' THEN 'VIEW' WHEN 'm' THEN 'MVIEW' \
                               WHEN 'f' THEN 'FOREIGN TABLE' WHEN 'S' THEN 'SEQUENCE' \
                               ELSE 'OTHER' END AS kind \
                   FROM pg_catalog.pg_class c \
                   JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                   WHERE n.nspname = $1 AND c.relkind IN ('r','p','v','m','f','S') \
                   UNION ALL \
                   SELECT p.proname, \
                          CASE p.prokind WHEN 'p' THEN 'PROCEDURE' ELSE 'FUNCTION' END \
                   FROM pg_catalog.pg_proc p \
                   JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
                   WHERE n.nspname = $1 \
                 ) q ORDER BY 1 LIMIT 20000",
            )
            .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
        let rows = client
            .query(&stmt, &[&schema])
            .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
        let mut out = Vec::new();
        for row in rows {
            let name: String = row.try_get(0).unwrap_or_default();
            let kind: String = row.try_get(1).unwrap_or_default();
            if name.trim().is_empty() {
                continue;
            }
            out.push(MetaObject {
                schema: schema.to_string(),
                name,
                kind: MetaObjectKind::from_oracle(&kind),
            });
        }
        Ok(out)
    }

    /// Completion: columns for the tables referenced by the current statement
    /// in one round trip. `WITH ORDINALITY` maps rows back to the exact ref
    /// (so search_path / quoted names resolve exactly like SQL would).
    pub fn table_columns_batch(
        &self,
        refs: &[TableRef],
        current_schema: &str,
    ) -> anyhow::Result<Vec<TableColumns>> {
        if refs.is_empty() {
            return Ok(Vec::new());
        }
        let mut client = self
            .client
            .lock()
            .map_err(|e| anyhow::anyhow!("connection locked: {}", e))?;
        let stmt = client
            .prepare(
                "SELECT t.ord, n.nspname, c.relname, a.attname, \
                        pg_catalog.format_type(a.atttypid, a.atttypmod), a.attnotnull \
                 FROM unnest($1::text[]) WITH ORDINALITY AS t(rel, ord) \
                 JOIN pg_catalog.pg_class c ON c.oid = to_regclass(t.rel) \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid \
                 WHERE a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY t.ord, a.attnum LIMIT 20000",
            )
            .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;
        let keys: Vec<String> = refs.iter().map(pg_regclass_arg).collect();
        let rows = client
            .query(&stmt, &[&keys])
            .map_err(|e| anyhow::anyhow!("{}", verbose_pg_error(&e)))?;

        let mut out: Vec<TableColumns> = refs
            .iter()
            .map(|r| TableColumns {
                schema: r
                    .schema
                    .clone()
                    .unwrap_or_else(|| current_schema.to_string()),
                table: r.name.clone(),
                columns: Vec::new(),
            })
            .collect();
        for row in rows {
            let ord: i64 = row.try_get(0).unwrap_or(0);
            let idx = ord.saturating_sub(1) as usize;
            if idx >= out.len() {
                continue;
            }
            let nspname: String = row.try_get(1).unwrap_or_default();
            let relname: String = row.try_get(2).unwrap_or_default();
            let name: String = row.try_get(3).unwrap_or_default();
            let dtype: String = row.try_get(4).unwrap_or_default();
            let not_null: bool = row.try_get(5).unwrap_or(false);
            if name.trim().is_empty() {
                continue;
            }
            if out[idx].columns.is_empty() {
                out[idx].schema = nspname;
                out[idx].table = relname;
            }
            out[idx].columns.push(ColumnInfo {
                name,
                data_type: if dtype.is_empty() { "?".into() } else { dtype },
                nullable: !not_null,
            });
        }
        Ok(out)
    }
}

/// Argument for `to_regclass`: bare names fold like SQL (lowercase), quoted
/// names keep their case. Schema is optional.
fn pg_regclass_arg(r: &TableRef) -> String {
    let table = if r.quoted {
        pg_quote_ident(&r.name)
    } else {
        r.name.clone()
    };
    match &r.schema {
        Some(schema) => {
            let schema = if r.schema_quoted {
                pg_quote_ident(schema)
            } else {
                schema.clone()
            };
            format!("{}.{}", schema, table)
        }
        None => table,
    }
}

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

/// Unbounded `SELECT *` for a relation, used as the base for preview paging.
pub(crate) fn pg_relation_select(schema: &str, table: &str) -> String {
    format!(
        "SELECT * FROM {}.{}",
        pg_quote_ident(schema),
        pg_quote_ident(table)
    )
}

/// Build the `EXPLAIN <sql>` statement (pure, testable).
pub(crate) fn pg_explain_sql(inner_sql: &str) -> String {
    format!("EXPLAIN {}", inner_sql.trim().trim_end_matches(';').trim())
}

impl DbConnection for PgConnection {
    fn execute_query(&self, sql: &str, max_rows: usize) -> QueryRow {
        self.run_query(sql, max_rows)
    }
    fn execute_query_paged(&self, sql: &str, page_offset: usize, page_size: usize) -> QueryRow {
        self.run_query_paged(sql, page_offset, page_size)
    }
    fn cancel_query(&self) -> anyhow::Result<()> {
        if self.use_tls {
            let inner = native_tls::TlsConnector::new()
                .map_err(|e| anyhow::anyhow!("TLS connector setup failed: {}", e))?;
            let connector = postgres_native_tls::MakeTlsConnector::new(inner);
            self.cancel_token
                .cancel_query(connector)
                .map_err(|e| anyhow::anyhow!("{}", e))
        } else {
            self.cancel_token
                .cancel_query(postgres::NoTls)
                .map_err(|e| anyhow::anyhow!("{}", e))
        }
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
        let stmt = client.prepare(sql).map_err(|e| anyhow::anyhow!("{}", e))?;
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
                machine: client_addr,
                program: app,
                sql_id: short_query,
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
    fn list_columns(
        &self,
        schema: &str,
        name: &str,
        kind: &str,
    ) -> anyhow::Result<Vec<ColumnInfo>> {
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
    fn describe_object(&self, target: &DdlTarget) -> anyhow::Result<DescribedObject> {
        self.describe_pg_object(target)
    }
    fn collect_perf_snapshot(&self) -> anyhow::Result<PerfSnapshot> {
        self.collect_pg_perf_snapshot()
    }
    fn current_schema(&self) -> anyhow::Result<String> {
        PgConnection::current_schema(self)
    }
    fn list_schema_objects(&self, schema: &str) -> anyhow::Result<Vec<MetaObject>> {
        PgConnection::list_schema_objects(self, schema)
    }
    fn table_columns_batch(
        &self,
        refs: &[TableRef],
        current_schema: &str,
    ) -> anyhow::Result<Vec<TableColumns>> {
        PgConnection::table_columns_batch(self, refs, current_schema)
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
        assert!(is_query_sql_for(
            DbType::Postgres,
            "with x as (select 1) select * from x"
        ));
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
            pg_relation_select("public", "emp"),
            "SELECT * FROM \"public\".\"emp\""
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

    #[test]
    fn pg_sslmode_classification() {
        assert!(!sslmode_wants_tls("disable"));
        assert!(sslmode_wants_tls("require"));
        assert!(sslmode_wants_tls("verify-full"));
        assert!(sslmode_wants_tls("prefer"));
        assert!(sslmode_fallback_allowed("prefer"));
        assert!(sslmode_fallback_allowed("allow"));
        assert!(!sslmode_fallback_allowed("require"));
    }

    #[test]
    fn pg_tx_classification_for_no_autocommit() {
        assert_eq!(classify_pg_tx_action("SELECT * FROM emp"), PgTxAction::None);
        assert_eq!(
            classify_pg_tx_action("SELECT * FROM emp FOR UPDATE"),
            PgTxAction::ImplicitBegin
        );
        assert_eq!(
            classify_pg_tx_action("WITH d AS (DELETE FROM emp RETURNING *) SELECT * FROM d"),
            PgTxAction::ImplicitBegin
        );
        assert_eq!(
            classify_pg_tx_action("UPDATE emp SET sal = sal + 100"),
            PgTxAction::ImplicitBegin
        );
        assert_eq!(
            classify_pg_tx_action("CREATE TABLE t (id int)"),
            PgTxAction::ImplicitBegin
        );
        assert_eq!(
            classify_pg_tx_action("CREATE INDEX CONCURRENTLY idx_t ON t (id)"),
            PgTxAction::None
        );
        assert_eq!(classify_pg_tx_action("VACUUM ANALYZE t"), PgTxAction::None);
        assert_eq!(
            classify_pg_tx_action("CREATE DATABASE other"),
            PgTxAction::None
        );
        assert_eq!(classify_pg_tx_action("BEGIN"), PgTxAction::ExplicitBegin);
        assert_eq!(
            classify_pg_tx_action("START TRANSACTION"),
            PgTxAction::ExplicitBegin
        );
        assert_eq!(
            classify_pg_tx_action("COMMIT WORK"),
            PgTxAction::ExplicitCommit { chain: false }
        );
        assert_eq!(
            classify_pg_tx_action("COMMIT AND CHAIN"),
            PgTxAction::ExplicitCommit { chain: true }
        );
        assert_eq!(
            classify_pg_tx_action("ROLLBACK TO sp1"),
            PgTxAction::None
        );
        assert_eq!(
            classify_pg_tx_action("ROLLBACK"),
            PgTxAction::ExplicitRollback { chain: false }
        );
    }
}
