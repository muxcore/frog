//! Performance & DBA querying catalog + telemetry snapshot helpers (`F6`).
//!
//! Provides curated DBA diagnostic queries for Oracle and Postgres (locking
//! sessions, top CPU sessions, top SQL, SGA/PGA & buffer cache memory, wait
//! events, long operations, storage/tablespaces, bloat/transactions) together
//! with lightweight terminal bar/sparkline formatting helpers.

use crate::db::connection::DbType;
use std::collections::VecDeque;

/// Maximum telemetry samples retained per session for `btop`-style sparklines.
pub const MAX_PERF_HISTORY: usize = 60;

/// One curated DBA diagnostic query shown in the `F6` Performance tab.
#[derive(Debug, Clone, Copy)]
pub struct PerfQuerySpec {
    pub badge: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub sql: &'static str,
    /// Optional fallback SQL when the primary view requires elevated dictionary
    /// privileges (`DBA_*` on Oracle) or an optional extension (`pg_stat_statements`).
    pub fallback_sql: Option<&'static str>,
}

/// Live health metrics displayed in the top graphical strip of `F6`.
#[derive(Debug, Clone, Default)]
pub struct PerfSnapshot {
    pub total_sessions: u64,
    pub active_sessions: u64,
    pub idle_in_tx_sessions: u64,
    pub blocked_sessions: u64,
    pub blocking_sessions: u64,
    pub max_active_secs: u64,
    pub cache_hit_pct: Option<f64>,
    pub mem_used_mb: Option<f64>,
    pub mem_total_mb: Option<f64>,
    pub mem_detail: String,
    /// Active sessions grouped by wait class (`CPU`, `User I/O`, `Lock`, ...).
    pub wait_breakdown: Vec<(String, u64)>,
    pub elapsed_ms: u64,
}

/// Rolling time-series history for `btop`-style sparklines in `F6`.
#[derive(Debug, Clone, Default)]
pub struct PerfHistory {
    pub active_series: VecDeque<u64>,
    pub blocked_series: VecDeque<u64>,
    pub cpu_wait_series: VecDeque<u64>,
}

impl PerfHistory {
    pub fn record(&mut self, snap: &PerfSnapshot) {
        Self::push_capped(&mut self.active_series, snap.active_sessions);
        Self::push_capped(&mut self.blocked_series, snap.blocked_sessions);
        let cpu_count = snap
            .wait_breakdown
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("CPU"))
            .map(|(_, v)| *v)
            .unwrap_or(snap.active_sessions);
        Self::push_capped(&mut self.cpu_wait_series, cpu_count);
    }

    fn push_capped(buf: &mut VecDeque<u64>, val: u64) {
        if buf.len() >= MAX_PERF_HISTORY {
            buf.pop_front();
        }
        buf.push_back(val);
    }

    pub fn active_slice(&self) -> Vec<u64> {
        self.active_series.iter().copied().collect()
    }

    pub fn blocked_slice(&self) -> Vec<u64> {
        self.blocked_series.iter().copied().collect()
    }
}

/// Render a high-resolution horizontal Unicode bar (`█████▋░░`) of `width` cells
/// for `ratio` in `0.0..=1.0` using 1/8th fractional block characters.
pub fn smooth_bar(ratio: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    const EIGHTHS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let clamped = if ratio.is_nan() {
        0.0
    } else {
        ratio.clamp(0.0, 1.0)
    };
    let total_eighths = (clamped * (width as f64) * 8.0).round() as usize;
    let full = (total_eighths / 8).min(width);
    let rem = total_eighths % 8;

    let mut out = String::with_capacity(width * 3);
    for _ in 0..full {
        out.push('█');
    }
    if full < width {
        if rem > 0 {
            out.push(EIGHTHS[rem]);
        } else {
            out.push('░');
        }
        for _ in (full + 1)..width {
            out.push('░');
        }
    }
    out
}

/// Classification of a numeric cell for inline bar rendering in the Performance table.
#[derive(Debug, Clone, PartialEq)]
pub enum CellBarKind {
    /// Percentage where higher means more saturated/hot (`USED_PCT`, `SHARE_PCT`, `DEAD_PCT`).
    HotPct(f64),
    /// Percentage where higher means healthier/further along (`CACHE_HIT_PCT`, `PCT_DONE`).
    GoodPct(f64),
    /// Magnitude scaled relative to the column maximum (`CPU_SEC`, `PGA_MB`, `WAIT_SECS`).
    Relative { ratio: f64 },
}

/// Inspect a column header + cell text and return an inline Unicode bar prefix
/// if the column is a performance percentage or primary resource metric.
pub fn classify_perf_cell(
    col_name: &str,
    raw_val: &str,
    col_max: Option<f64>,
) -> Option<(String, CellBarKind)> {
    let trimmed = raw_val.trim().trim_end_matches('%').trim();
    let val: f64 = trimmed.parse().ok()?;
    if !val.is_finite() {
        return None;
    }

    let upper = col_name.trim().to_ascii_uppercase();
    let is_pct = upper == "%"
        || upper.ends_with("_PCT")
        || upper.ends_with("PCT")
        || upper.contains("PERCENT")
        || upper.contains("HIT_RATIO");

    if is_pct {
        let pct = val.clamp(0.0, 100.0);
        let bar = smooth_bar(pct / 100.0, 8);
        let formatted = format!("{} {:>5.1}%", bar, pct);
        let good = upper.contains("HIT") || upper.contains("DONE") || upper.contains("IDX_USE");
        let kind = if good {
            CellBarKind::GoodPct(pct)
        } else {
            CellBarKind::HotPct(pct)
        };
        return Some((formatted, kind));
    }

    let is_metric = matches!(
        upper.as_str(),
        "CPU_SEC"
            | "ELAPSED_SEC"
            | "TOTAL_SEC"
            | "WAIT_SECS"
            | "QUERY_SECS"
            | "XACT_SECS"
            | "TIME_WAITED_SEC"
            | "PGA_MB"
            | "PGA_MAX_MB"
            | "SIZE_MB"
            | "USED_MB"
            | "TOTAL_MB"
            | "DISK_READS"
            | "BUFFER_GETS"
    );
    if is_metric {
        let max = col_max.unwrap_or(0.0);
        if max > 0.0 && val >= 0.0 {
            let ratio = (val / max).clamp(0.0, 1.0);
            let bar = smooth_bar(ratio, 6);
            return Some((
                format!("{} {}", bar, raw_val.trim()),
                CellBarKind::Relative { ratio },
            ));
        }
    }

    None
}

/// Compute per-column maximum numeric values (used to scale relative bars).
pub fn column_maxima(columns: &[String], rows: &[Vec<String>]) -> Vec<Option<f64>> {
    columns
        .iter()
        .enumerate()
        .map(|(col_idx, _)| {
            let mut max_val: Option<f64> = None;
            for row in rows {
                if let Some(cell) = row.get(col_idx) {
                    if let Ok(v) = cell.trim().trim_end_matches('%').trim().parse::<f64>() {
                        if v.is_finite() {
                            max_val = Some(match max_val {
                                Some(cur) => cur.max(v),
                                None => v,
                            });
                        }
                    }
                }
            }
            max_val
        })
        .collect()
}

/// Curated DBA diagnostic queries for the given database backend.
pub fn perf_queries_for(db_type: DbType) -> &'static [PerfQuerySpec] {
    match db_type {
        DbType::Oracle => &ORACLE_PERF_QUERIES,
        DbType::Postgres => &POSTGRES_PERF_QUERIES,
    }
}

pub static ORACLE_PERF_QUERIES: [PerfQuerySpec; 10] = [
    PerfQuerySpec {
        badge: "LOCK",
        title: "Blocking & Waiting Sessions",
        description: "Sessions blocked on locks, their blocking SID, wait event, locked object and wait time",
        sql: "\
SELECT
  w.sid AS waiter_sid,
  w.serial# AS waiter_serial,
  w.username AS waiter_user,
  w.blocking_session AS blocker_sid,
  b.username AS blocker_user,
  b.status AS blocker_status,
  w.seconds_in_wait AS wait_secs,
  w.event AS wait_event,
  CASE
    WHEN o.object_name IS NOT NULL THEN o.owner || '.' || o.object_name
    ELSE NULL
  END AS locked_object,
  w.sql_id AS waiter_sql_id,
  b.sql_id AS blocker_sql_id,
  w.machine,
  w.program
FROM v$session w
LEFT JOIN v$session b ON b.sid = w.blocking_session
LEFT JOIN all_objects o ON o.object_id = w.row_wait_obj#
WHERE w.blocking_session IS NOT NULL
   OR w.sid IN (SELECT blocking_session FROM v$session WHERE blocking_session IS NOT NULL)
ORDER BY w.blocking_session NULLS FIRST, w.seconds_in_wait DESC",
        fallback_sql: None,
    },
    PerfQuerySpec {
        badge: "CPU",
        title: "Top CPU Consuming Sessions",
        description: "Sessions ranked by cumulative CPU time used (v$sesstat + v$session)",
        sql: "\
SELECT
  s.sid,
  s.serial#,
  NVL(s.username, s.program) AS username,
  s.status,
  ROUND(st.value / 100, 2) AS cpu_sec,
  ROUND(RATIO_TO_REPORT(st.value) OVER () * 100, 1) AS share_pct,
  s.sql_id,
  CASE WHEN s.state != 'WAITING' THEN 'ON CPU' ELSE s.event END AS current_activity,
  s.last_call_et AS call_secs,
  s.machine,
  s.program
FROM v$session s
JOIN v$sesstat st ON st.sid = s.sid
JOIN v$statname sn ON sn.statistic# = st.statistic#
WHERE sn.name = 'CPU used by this session'
  AND s.type != 'BACKGROUND'
  AND st.value > 0
ORDER BY st.value DESC",
        fallback_sql: Some(
            "\
SELECT
  s.sid,
  s.serial#,
  s.username,
  s.status,
  s.last_call_et AS query_secs,
  CASE WHEN s.state != 'WAITING' THEN 'ON CPU' ELSE s.wait_class END AS activity,
  s.event,
  s.sql_id,
  s.machine,
  s.program
FROM v$session s
WHERE s.type != 'BACKGROUND'
  AND s.status = 'ACTIVE'
ORDER BY s.last_call_et DESC",
        ),
    },
    PerfQuerySpec {
        badge: "SQL",
        title: "Top SQL by CPU & Elapsed Time",
        description: "Heaviest cached statements in v$sql ranked by total CPU seconds",
        sql: "\
SELECT * FROM (
  SELECT
    sql_id,
    executions AS execs,
    ROUND(cpu_time / 1e6, 2) AS cpu_sec,
    ROUND(elapsed_time / 1e6, 2) AS elapsed_sec,
    ROUND(elapsed_time / NULLIF(executions, 0) / 1e3, 2) AS avg_ms,
    ROUND(RATIO_TO_REPORT(cpu_time) OVER () * 100, 1) AS share_pct,
    buffer_gets,
    disk_reads,
    rows_processed,
    parsing_schema_name AS schema_name,
    SUBSTR(REPLACE(sql_text, CHR(10), ' '), 1, 100) AS sql_preview
  FROM v$sql
  WHERE cpu_time > 0
  ORDER BY cpu_time DESC
) WHERE ROWNUM <= 30",
        fallback_sql: None,
    },
    PerfQuerySpec {
        badge: "MEM",
        title: "Memory Overview (SGA & PGA)",
        description: "Instance SGA pools and PGA allocation / in-use breakdown",
        sql: "\
SELECT
  'SGA: ' || name AS memory_area,
  ROUND(bytes / 1048576, 1) AS size_mb,
  ROUND(RATIO_TO_REPORT(bytes) OVER () * 100, 1) AS share_pct
FROM v$sgainfo
WHERE bytes > 0
UNION ALL
SELECT
  'PGA: ' || name AS memory_area,
  ROUND(value / 1048576, 1) AS size_mb,
  NULL AS share_pct
FROM v$pgastat
WHERE name IN (
  'aggregate PGA target parameter',
  'total PGA allocated',
  'total PGA inuse',
  'maximum PGA allocated',
  'over allocation count'
)",
        fallback_sql: Some(
            "\
SELECT
  name AS sga_component,
  ROUND(value / 1048576, 1) AS size_mb,
  ROUND(RATIO_TO_REPORT(value) OVER () * 100, 1) AS share_pct
FROM v$sga",
        ),
    },
    PerfQuerySpec {
        badge: "PGA",
        title: "Top Memory Consuming Sessions (PGA)",
        description: "User sessions holding the largest PGA / UGA process memory",
        sql: "\
SELECT
  s.sid,
  s.serial#,
  NVL(s.username, s.program) AS username,
  s.status,
  ROUND(p.pga_used_mem / 1048576, 2) AS pga_mb,
  ROUND(p.pga_alloc_mem / 1048576, 2) AS pga_alloc_mb,
  ROUND(p.pga_max_mem / 1048576, 2) AS pga_max_mb,
  ROUND(RATIO_TO_REPORT(p.pga_used_mem) OVER () * 100, 1) AS share_pct,
  s.sql_id,
  s.machine,
  s.program
FROM v$session s
JOIN v$process p ON p.addr = s.paddr
WHERE s.type != 'BACKGROUND'
ORDER BY p.pga_used_mem DESC",
        fallback_sql: None,
    },
    PerfQuerySpec {
        badge: "WAIT",
        title: "System Wait Events (Non-Idle)",
        description: "Cumulative non-idle wait classes & events ordered by time waited",
        sql: "\
SELECT * FROM (
  SELECT
    wait_class,
    event,
    total_waits,
    ROUND(time_waited_micro / 1e6, 2) AS time_waited_sec,
    ROUND(time_waited_micro / NULLIF(total_waits, 0) / 1e3, 2) AS avg_wait_ms,
    ROUND(RATIO_TO_REPORT(time_waited_micro) OVER () * 100, 1) AS wait_pct
  FROM v$system_event
  WHERE wait_class != 'Idle'
    AND time_waited_micro > 0
  ORDER BY time_waited_micro DESC
) WHERE ROWNUM <= 30",
        fallback_sql: None,
    },
    PerfQuerySpec {
        badge: "LONG",
        title: "Long Operations Progress (v$session_longops)",
        description: "Active and recent long-running table scans, sorts, backups and index builds",
        sql: "\
SELECT * FROM (
  SELECT
    sid,
    serial#,
    username,
    opname,
    target,
    ROUND(sofar * 100 / NULLIF(totalwork, 0), 1) AS pct_done,
    elapsed_seconds AS elapsed_sec,
    time_remaining AS rem_sec,
    sql_id,
    message
  FROM v$session_longops
  WHERE totalwork > 0
  ORDER BY CASE WHEN time_remaining > 0 THEN 0 ELSE 1 END, start_time DESC
) WHERE ROWNUM <= 30",
        fallback_sql: None,
    },
    PerfQuerySpec {
        badge: "TBS",
        title: "Tablespace & Storage Usage",
        description: "Tablespace capacity, used MB and utilization percentage",
        sql: "\
SELECT
  m.tablespace_name,
  t.contents,
  ROUND(m.used_space * t.block_size / 1048576, 1) AS used_mb,
  ROUND(m.tablespace_size * t.block_size / 1048576, 1) AS total_mb,
  ROUND(m.used_percent, 1) AS used_pct
FROM dba_tablespace_usage_metrics m
JOIN dba_tablespaces t ON t.tablespace_name = m.tablespace_name
ORDER BY m.used_percent DESC",
        fallback_sql: Some(
            "\
SELECT
  t.tablespace_name,
  t.contents,
  t.status,
  ROUND(NVL(SUM(s.bytes), 0) / 1048576, 1) AS used_mb
FROM user_tablespaces t
LEFT JOIN user_segments s ON s.tablespace_name = t.tablespace_name
GROUP BY t.tablespace_name, t.contents, t.status
ORDER BY used_mb DESC",
        ),
    },
    PerfQuerySpec {
        badge: "I/O",
        title: "Top Physical & Logical I/O SQL",
        description: "Statements driving the heaviest disk reads and buffer gets",
        sql: "\
SELECT * FROM (
  SELECT
    sql_id,
    disk_reads,
    buffer_gets,
    executions AS execs,
    ROUND(disk_reads / NULLIF(executions, 0), 1) AS disk_per_exec,
    ROUND((1 - disk_reads / NULLIF(buffer_gets, 0)) * 100, 1) AS cache_hit_pct,
    ROUND(elapsed_time / 1e6, 2) AS elapsed_sec,
    SUBSTR(REPLACE(sql_text, CHR(10), ' '), 1, 90) AS sql_preview
  FROM v$sql
  WHERE buffer_gets > 0
  ORDER BY disk_reads DESC, buffer_gets DESC
) WHERE ROWNUM <= 30",
        fallback_sql: None,
    },
    PerfQuerySpec {
        badge: "TXN",
        title: "Active Transactions & Undo Usage",
        description: "Open uncommitted transactions, undo blocks held and duration",
        sql: "\
SELECT
  s.sid,
  s.serial#,
  s.username,
  s.status,
  t.start_time,
  ROUND((SYSDATE - t.start_date) * 86400) AS xact_secs,
  t.used_ublk AS undo_blocks,
  t.used_urec AS undo_records,
  t.log_io,
  t.phy_io,
  s.sql_id,
  s.program
FROM v$transaction t
JOIN v$session s ON s.taddr = t.addr
ORDER BY t.used_ublk DESC, t.start_date ASC",
        fallback_sql: None,
    },
];

pub static POSTGRES_PERF_QUERIES: [PerfQuerySpec; 10] = [
    PerfQuerySpec {
        badge: "LOCK",
        title: "Blocking & Waiting Locks",
        description: "Blocked backends paired with their blocking PIDs, lock duration and queries",
        sql: "\
SELECT
  w.pid AS blocked_pid,
  w.usename AS blocked_user,
  EXTRACT(EPOCH FROM (clock_timestamp() - w.query_start))::bigint AS wait_secs,
  w.wait_event_type || ':' || COALESCE(w.wait_event, '?') AS waiting_on,
  b.pid AS blocker_pid,
  b.usename AS blocker_user,
  b.state AS blocker_state,
  LEFT(REPLACE(w.query, E'\\n', ' '), 80) AS blocked_query,
  LEFT(REPLACE(b.query, E'\\n', ' '), 80) AS blocker_query
FROM pg_stat_activity w
JOIN LATERAL unnest(pg_blocking_pids(w.pid)) AS bp(pid) ON TRUE
JOIN pg_stat_activity b ON b.pid = bp.pid
ORDER BY wait_secs DESC NULLS LAST",
        fallback_sql: None,
    },
    PerfQuerySpec {
        badge: "CPU",
        title: "Active & CPU Consuming Sessions",
        description: "Running backends ordered by active query runtime and wait state",
        sql: "\
SELECT
  pid,
  usename AS username,
  state,
  CASE
    WHEN wait_event_type IS NULL AND state = 'active' THEN 'ON CPU'
    ELSE COALESCE(wait_event_type || ':' || wait_event, state)
  END AS activity,
  GREATEST(EXTRACT(EPOCH FROM (clock_timestamp() - query_start))::numeric(12,1), 0) AS query_secs,
  GREATEST(EXTRACT(EPOCH FROM (clock_timestamp() - xact_start))::numeric(12,1), 0) AS xact_secs,
  application_name AS app,
  COALESCE(client_addr::text, 'local') AS client,
  LEFT(REPLACE(query, E'\\n', ' '), 100) AS sql_preview
FROM pg_stat_activity
WHERE datname = current_database()
  AND pid <> pg_backend_pid()
  AND state IS NOT NULL
  AND state <> 'idle'
ORDER BY query_secs DESC NULLS LAST",
        fallback_sql: None,
    },
    PerfQuerySpec {
        badge: "SQL",
        title: "Top SQL by Execution Time (pg_stat_statements)",
        description: "Heaviest statements by total execution time, average latency and cache hit ratio",
        sql: "\
SELECT
  calls,
  ROUND((total_exec_time / 1000.0)::numeric, 2) AS total_sec,
  ROUND(mean_exec_time::numeric, 2) AS avg_ms,
  ROUND((total_exec_time * 100.0 / NULLIF(SUM(total_exec_time) OVER (), 0))::numeric, 1) AS share_pct,
  rows,
  ROUND((shared_blks_hit * 100.0 / NULLIF(shared_blks_hit + shared_blks_read, 0))::numeric, 1) AS cache_hit_pct,
  LEFT(REPLACE(query, E'\\n', ' '), 100) AS sql_preview
FROM pg_stat_statements
ORDER BY total_exec_time DESC
LIMIT 30",
        fallback_sql: Some(
            "\
SELECT
  pid,
  usename AS username,
  state,
  GREATEST(EXTRACT(EPOCH FROM (clock_timestamp() - query_start))::numeric(12,2), 0) AS elapsed_sec,
  'enable pg_stat_statements for historical top SQL' AS note,
  LEFT(REPLACE(query, E'\\n', ' '), 100) AS sql_preview
FROM pg_stat_activity
WHERE datname = current_database()
  AND pid <> pg_backend_pid()
ORDER BY query_start ASC NULLS LAST
LIMIT 30",
        ),
    },
    PerfQuerySpec {
        badge: "MEM",
        title: "Memory Settings & Buffer Cache Health",
        description: "Configured Postgres memory parameters alongside live cache hit & temp spill stats",
        sql: "\
SELECT
  name AS metric,
  CASE
    WHEN unit = '8kB' THEN pg_size_pretty(setting::bigint * 8192)
    WHEN unit = 'kB'  THEN pg_size_pretty(setting::bigint * 1024)
    WHEN unit = 'MB'  THEN pg_size_pretty(setting::bigint * 1048576)
    WHEN unit = 'B'   THEN pg_size_pretty(setting::bigint)
    ELSE setting || COALESCE(' ' || unit, '')
  END AS current_value,
  short_desc AS description
FROM pg_settings
WHERE name IN (
  'shared_buffers',
  'effective_cache_size',
  'work_mem',
  'maintenance_work_mem',
  'temp_buffers',
  'wal_buffers',
  'max_connections'
)
UNION ALL
SELECT
  'buffer_cache_hit_pct',
  ROUND(COALESCE(blks_hit * 100.0 / NULLIF(blks_hit + blks_read, 0), 100.0), 2)::text || '%',
  'Current database shared buffer cache hit ratio'
FROM pg_stat_database WHERE datname = current_database()
UNION ALL
SELECT
  'temp_bytes_spilled',
  pg_size_pretty(temp_bytes) || ' (' || temp_files || ' files)',
  'Disk spill when sorts/hashes exceed work_mem'
FROM pg_stat_database WHERE datname = current_database()
UNION ALL
SELECT
  'deadlocks_total',
  deadlocks::text,
  'Cumulative deadlocks detected in current database'
FROM pg_stat_database WHERE datname = current_database()",
        fallback_sql: None,
    },
    PerfQuerySpec {
        badge: "TXN",
        title: "Long & Idle-in-Transaction Sessions",
        description: "Open transactions holding XIDs/snapshots that block VACUUM and retain locks",
        sql: "\
SELECT
  pid,
  usename AS username,
  state,
  GREATEST(EXTRACT(EPOCH FROM (clock_timestamp() - xact_start))::numeric(12,1), 0) AS xact_secs,
  GREATEST(EXTRACT(EPOCH FROM (clock_timestamp() - state_change))::numeric(12,1), 0) AS state_secs,
  backend_xid::text AS xid,
  backend_xmin::text AS xmin_horizon,
  application_name AS app,
  LEFT(REPLACE(query, E'\\n', ' '), 90) AS last_query
FROM pg_stat_activity
WHERE datname = current_database()
  AND pid <> pg_backend_pid()
  AND xact_start IS NOT NULL
ORDER BY xact_secs DESC",
        fallback_sql: None,
    },
    PerfQuerySpec {
        badge: "WAIT",
        title: "Active Wait Events Breakdown",
        description: "Current sessions grouped by wait_event_type and wait_event",
        sql: "\
SELECT
  COALESCE(wait_event_type, 'CPU') AS wait_type,
  COALESCE(wait_event, 'Running on CPU') AS wait_event,
  state,
  COUNT(*)::bigint AS sessions,
  ROUND((COUNT(*) * 100.0 / NULLIF(SUM(COUNT(*)) OVER (), 0))::numeric, 1) AS share_pct,
  COALESCE(MAX(EXTRACT(EPOCH FROM (clock_timestamp() - state_change)))::bigint, 0) AS max_state_secs
FROM pg_stat_activity
WHERE datname = current_database()
  AND pid <> pg_backend_pid()
  AND state IS NOT NULL
  AND state <> 'idle'
GROUP BY 1, 2, 3
ORDER BY sessions DESC, max_state_secs DESC",
        fallback_sql: None,
    },
    PerfQuerySpec {
        badge: "SIZE",
        title: "Largest Tables & Indexes (Storage)",
        description: "Top 30 relations by total disk footprint (heap + TOAST + indexes)",
        sql: "\
SELECT
  n.nspname || '.' || c.relname AS relation,
  ROUND(pg_total_relation_size(c.oid) / 1048576.0, 1) AS total_mb,
  ROUND(pg_table_size(c.oid) / 1048576.0, 1) AS table_mb,
  ROUND(pg_indexes_size(c.oid) / 1048576.0, 1) AS index_mb,
  ROUND((pg_total_relation_size(c.oid) * 100.0 / NULLIF(pg_database_size(current_database()), 0))::numeric, 1) AS used_pct,
  GREATEST(c.reltuples::bigint, 0) AS est_rows
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE c.relkind IN ('r', 'p', 'm')
  AND n.nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast')
ORDER BY pg_total_relation_size(c.oid) DESC
LIMIT 30",
        fallback_sql: None,
    },
    PerfQuerySpec {
        badge: "VAC",
        title: "Dead Tuples & Autovacuum Health",
        description: "Tables with dead-tuple bloat needing VACUUM / ANALYZE attention",
        sql: "\
SELECT
  schemaname || '.' || relname AS table_name,
  n_live_tup AS live_rows,
  n_dead_tup AS dead_rows,
  ROUND((n_dead_tup * 100.0 / NULLIF(n_live_tup + n_dead_tup, 0))::numeric, 1) AS dead_pct,
  COALESCE(TO_CHAR(GREATEST(last_vacuum, last_autovacuum), 'YYYY-MM-DD HH24:MI'), 'never') AS last_vacuum,
  COALESCE(TO_CHAR(GREATEST(last_analyze, last_autoanalyze), 'YYYY-MM-DD HH24:MI'), 'never') AS last_analyze,
  autovacuum_count AS autovacs
FROM pg_stat_user_tables
ORDER BY n_dead_tup DESC, dead_pct DESC NULLS LAST
LIMIT 30",
        fallback_sql: None,
    },
    PerfQuerySpec {
        badge: "I/O",
        title: "Table Sequential Scans & Buffer Cache I/O",
        description: "Tables with heavy sequential scans (missing indexes?) or disk block reads",
        sql: "\
SELECT
  s.schemaname || '.' || s.relname AS table_name,
  s.seq_scan,
  COALESCE(s.idx_scan, 0) AS idx_scan,
  ROUND((COALESCE(s.idx_scan, 0) * 100.0 / NULLIF(s.seq_scan + COALESCE(s.idx_scan, 0), 0))::numeric, 1) AS idx_use_pct,
  io.heap_blks_read AS disk_blks,
  io.heap_blks_hit AS cache_blks,
  ROUND((io.heap_blks_hit * 100.0 / NULLIF(io.heap_blks_hit + io.heap_blks_read, 0))::numeric, 1) AS cache_hit_pct,
  s.n_live_tup AS live_rows
FROM pg_stat_user_tables s
JOIN pg_statio_user_tables io ON io.relid = s.relid
WHERE (s.seq_scan + COALESCE(s.idx_scan, 0) + io.heap_blks_read + io.heap_blks_hit) > 0
ORDER BY io.heap_blks_read DESC, s.seq_scan DESC
LIMIT 30",
        fallback_sql: None,
    },
    PerfQuerySpec {
        badge: "CONN",
        title: "Connection Pool Utilization by User & App",
        description: "Backend count grouped by user, application and state vs max_connections",
        sql: "\
WITH maxc AS (
  SELECT setting::numeric AS max_conn FROM pg_settings WHERE name = 'max_connections'
)
SELECT
  COALESCE(usename, '(bg)') AS username,
  COALESCE(NULLIF(application_name, ''), '(none)') AS app,
  COALESCE(state, 'background') AS state,
  COUNT(*)::bigint AS connections,
  ROUND((COUNT(*) * 100.0 / NULLIF((SELECT max_conn FROM maxc), 0))::numeric, 1) AS conn_pct
FROM pg_stat_activity
WHERE datname = current_database()
GROUP BY 1, 2, 3
ORDER BY connections DESC",
        fallback_sql: None,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn smooth_bar_renders_exact_display_width() {
        for pct in [0.0, 0.125, 0.33, 0.5, 0.78, 1.0, -0.5, 1.5, f64::NAN] {
            let b = smooth_bar(pct, 10);
            assert_eq!(UnicodeWidthStr::width(b.as_str()), 10);
        }
        assert_eq!(smooth_bar(0.0, 4), "░░░░");
        assert_eq!(smooth_bar(1.0, 4), "████");
    }

    #[test]
    fn classifies_percentage_and_relative_metric_cells() {
        let (txt, kind) = classify_perf_cell("USED_PCT", "87.5", None).unwrap();
        assert_eq!(kind, CellBarKind::HotPct(87.5));
        assert!(txt.contains("87.5%"));

        let (_, hit_kind) = classify_perf_cell("CACHE_HIT_PCT", "99.2", None).unwrap();
        assert_eq!(hit_kind, CellBarKind::GoodPct(99.2));

        let (rel_txt, rel_kind) = classify_perf_cell("CPU_SEC", "50.0", Some(100.0)).unwrap();
        assert!(matches!(rel_kind, CellBarKind::Relative { ratio } if (ratio - 0.5).abs() < 1e-6));
        assert!(rel_txt.contains("50.0"));

        // Non-metric text columns are left untouched.
        assert!(classify_perf_cell("USERNAME", "SCOTT", Some(100.0)).is_none());
    }

    #[test]
    fn perf_history_caps_at_max_samples() {
        let mut h = PerfHistory::default();
        for i in 0..80 {
            h.record(&PerfSnapshot {
                active_sessions: i,
                blocked_sessions: i % 3,
                ..PerfSnapshot::default()
            });
        }
        assert_eq!(h.active_series.len(), MAX_PERF_HISTORY);
        assert_eq!(h.active_series.front().copied(), Some(20));
        assert_eq!(h.active_series.back().copied(), Some(79));
    }

    #[test]
    fn both_backends_expose_ten_curated_queries() {
        let ora = perf_queries_for(DbType::Oracle);
        let pg = perf_queries_for(DbType::Postgres);
        assert_eq!(ora.len(), 10);
        assert_eq!(pg.len(), 10);
        for q in ora.iter().chain(pg.iter()) {
            assert!(!q.badge.is_empty());
            assert!(!q.title.is_empty());
            assert!(q.sql.to_ascii_uppercase().contains("SELECT"));
        }
    }
}
