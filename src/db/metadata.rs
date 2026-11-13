//! Per-session metadata cache for SQL completion.
//!
//! Completion must never touch the database on the keystroke path. This module
//! owns the in-memory store (schemas -> object names -> columns) and a small
//! worker that fills it in the background. Queries are cheap and bounded:
//! one for a schema's object names, one batched query for the columns of the
//! tables actually referenced by the statement.
//!
//! Design notes and the research behind the layering live in
//! `docs/sql-completion.md`.

use crate::db::connection::{ColumnInfo, DbConnection, DbType, MetaObject, TableColumns, TableRef};
use std::collections::{HashMap, HashSet};
use std::sync::{mpsc::Sender, Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a cache level stays fresh. Stale entries are still served while a
/// refresh runs in the background (stale-while-revalidate).
pub const METADATA_TTL: Duration = Duration::from_secs(600);

/// Hard caps: a huge schema must degrade to lazy mode, never hang or exhaust
/// memory. Object lists are capped per schema in the query itself too.
pub const MAX_OBJECTS_PER_SCHEMA: usize = 20_000;
pub const MAX_CACHED_COLUMNS: usize = 100_000;

/// Normalize an identifier for cache keys and matching.
/// Oracle folds unquoted identifiers to uppercase, Postgres to lowercase.
/// Quoted identifiers keep their exact spelling.
pub fn norm_ident(dialect: DbType, name: &str, quoted: bool) -> String {
    if quoted {
        name.to_string()
    } else {
        match dialect {
            DbType::Oracle => name.to_uppercase(),
            DbType::Postgres => name.to_lowercase(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SchemaObjects {
    pub entries: Vec<MetaObject>,
    pub loaded_at: Instant,
}

#[derive(Debug, Clone)]
pub struct CachedColumns {
    pub columns: Vec<ColumnInfo>,
    pub loaded_at: Instant,
}

/// All completion metadata for one session. Not `Sync`: it lives inside the
/// UI-thread-owned `Session` and is only mutated by `poll_metadata`.
#[derive(Debug)]
pub struct MetadataStore {
    pub dialect: DbType,
    pub current_schema: String,
    pub schemas: Vec<String>,
    pub schemas_at: Option<Instant>,
    /// Key: normalized schema name.
    pub objects: HashMap<String, SchemaObjects>,
    /// Key: (normalized schema, normalized table).
    pub columns: HashMap<(String, String), CachedColumns>,
    /// Bumped on DDL / manual refresh; in-flight results from older
    /// generations are discarded.
    pub generation: u64,
    /// In-flight request keys (`"*"`, `"o:SCHEMA"`, `"c:SCHEMA.TABLE"`).
    pub pending: HashSet<String>,
    pub last_error: Option<String>,
    /// When the last worker error happened; requests are paused briefly so a
    /// missing privilege does not turn every keystroke into a catalog query.
    pub last_error_at: Option<Instant>,
}

impl Default for MetadataStore {
    fn default() -> Self {
        Self {
            dialect: DbType::Oracle,
            current_schema: String::new(),
            schemas: Vec::new(),
            schemas_at: None,
            objects: HashMap::new(),
            columns: HashMap::new(),
            generation: 0,
            pending: HashSet::new(),
            last_error: None,
            last_error_at: None,
        }
    }
}

impl MetadataStore {
    pub fn set_dialect(&mut self, dialect: DbType) {
        if self.dialect != dialect {
            self.dialect = dialect;
            self.clear_caches();
        }
    }

    pub fn clear_caches(&mut self) {
        self.objects.clear();
        self.columns.clear();
        self.schemas.clear();
        self.schemas_at = None;
        self.pending.clear();
    }

    pub fn bootstrapped(&self) -> bool {
        !self.current_schema.is_empty() && self.schemas_at.is_some()
    }

    /// False briefly after a worker error, to avoid a query per keystroke when
    /// the account lacks dictionary privileges.
    pub fn can_request(&self) -> bool {
        match self.last_error_at {
            None => true,
            Some(at) => at.elapsed() > Duration::from_secs(30),
        }
    }

    pub fn mark_error(&mut self, message: String) {
        self.last_error = Some(message);
        self.last_error_at = Some(Instant::now());
    }

    pub fn mark_ok(&mut self) {
        self.last_error = None;
        self.last_error_at = None;
    }

    /// True when there are in-flight catalog requests.
    pub fn loading(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn objects_need_load(&self, schema: &str) -> bool {
        let key = self.schema_key(schema);
        match self.objects.get(&key) {
            None => true,
            Some(o) => o.loaded_at.elapsed() > METADATA_TTL,
        }
    }

    pub fn columns_need_load(&self, schema: &str, table: &str) -> bool {
        let key = (self.schema_key(schema), self.table_key(table, false));
        match self.columns.get(&key) {
            None => true,
            Some(c) => c.loaded_at.elapsed() > METADATA_TTL,
        }
    }

    pub fn schema_key(&self, name: &str) -> String {
        norm_ident(self.dialect, name, false)
    }

    pub fn table_key(&self, name: &str, quoted: bool) -> String {
        norm_ident(self.dialect, name, quoted)
    }

    pub fn ident_eq(&self, actual: &str, typed: &str, quoted: bool) -> bool {
        if quoted {
            actual == typed
        } else {
            actual.eq_ignore_ascii_case(typed)
        }
    }

    /// Actual schema name matching what the user typed, if known.
    pub fn find_schema(&self, typed: &str, quoted: bool) -> Option<&str> {
        self.schemas
            .iter()
            .find(|s| self.ident_eq(s, typed, quoted))
            .map(|s| s.as_str())
    }

    pub fn objects_for(&self, schema: &str, _quoted: bool) -> Option<&[MetaObject]> {
        let key = self.schema_key(schema);
        self.objects.get(&key).map(|o| o.entries.as_slice())
    }

    pub fn columns_for(&self, schema: &str, table: &str) -> Option<&[ColumnInfo]> {
        let key = (self.schema_key(schema), self.table_key(table, false));
        self.columns.get(&key).map(|c| c.columns.as_slice())
    }

    /// Columns for a parsed reference. Falls back to any loaded schema with a
    /// matching table name, which covers Postgres `search_path` relations and
    /// Oracle synonyms already resolved into a schema. A cached empty entry
    /// (a miss) never hides a real one found under another schema.
    pub fn columns_for_ref(&self, r: &TableRef) -> Option<&[ColumnInfo]> {
        let mut empty: Option<&[ColumnInfo]> = None;
        if let Some(schema) = r.schema.as_deref().or(Some(self.current_schema.as_str())) {
            if let Some(cols) = self.columns_for(schema, &r.name) {
                if !cols.is_empty() {
                    return Some(cols);
                }
                empty = Some(cols);
            }
        }
        let wanted = norm_ident(self.dialect, &r.name, false);
        if let Some(cols) = self
            .columns
            .iter()
            .find(|((_, table), _)| table == &wanted)
            .map(|(_, c)| c.columns.as_slice())
        {
            if !cols.is_empty() {
                return Some(cols);
            }
            empty = empty.or(Some(cols));
        }
        empty
    }

    pub fn put_schemas(&mut self, mut names: Vec<String>) {
        names.sort();
        names.dedup();
        self.schemas = names;
        self.schemas_at = Some(Instant::now());
    }

    pub fn put_objects(&mut self, schema: &str, mut entries: Vec<MetaObject>) {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries.truncate(MAX_OBJECTS_PER_SCHEMA);
        let key = self.schema_key(schema);
        self.objects.insert(
            key,
            SchemaObjects {
                entries,
                loaded_at: Instant::now(),
            },
        );
    }

    pub fn put_columns(&mut self, schema: &str, table: &str, columns: Vec<ColumnInfo>) {
        let key = (self.schema_key(schema), self.table_key(table, false));
        self.columns.insert(
            key,
            CachedColumns {
                columns,
                loaded_at: Instant::now(),
            },
        );
        self.enforce_column_cap();
    }

    /// Record a requested (schema, table) that the catalog did not return, so
    /// it is not queried again on every keystroke.
    pub fn mark_columns_loaded(&mut self, key: (String, String)) {
        self.columns.entry(key).or_insert_with(|| CachedColumns {
            columns: Vec::new(),
            loaded_at: Instant::now(),
        });
    }

    fn enforce_column_cap(&mut self) {
        let total: usize = self.columns.values().map(|c| c.columns.len()).sum();
        if total <= MAX_CACHED_COLUMNS {
            return;
        }
        // Evict whole tables, oldest first.
        let mut keys: Vec<_> = self
            .columns
            .iter()
            .map(|(k, v)| (k.clone(), v.loaded_at))
            .collect();
        keys.sort_by_key(|(_, t)| *t);
        let mut total = total;
        for (key, _) in keys {
            if total <= MAX_CACHED_COLUMNS {
                break;
            }
            if let Some(removed) = self.columns.remove(&key) {
                total = total.saturating_sub(removed.columns.len());
            }
        }
    }

    pub fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.objects.clear();
        self.columns.clear();
        self.schemas.clear();
        self.schemas_at = None;
        self.pending.clear();
        self.last_error = None;
        self.last_error_at = None;
    }
}

// ---------------------------------------------------------------------------
// Background worker
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum MetaRequest {
    /// Current schema + schema list + objects of the current schema.
    Bootstrap,
    Objects {
        schema: String,
    },
    Columns {
        refs: Vec<TableRef>,
        current_schema: String,
    },
}

#[derive(Debug)]
pub enum MetaKind {
    Bootstrap {
        current_schema: Result<String, String>,
        schemas: Result<Vec<String>, String>,
        objects: Result<(String, Vec<MetaObject>), String>,
    },
    Objects {
        schema: String,
        objects: Result<Vec<MetaObject>, String>,
    },
    Columns {
        /// Normalized keys that were requested, so missing tables can be
        /// cached as "no columns" instead of being re-queried every time.
        requested: Vec<(String, String)>,
        found: Vec<TableColumns>,
        error: Option<String>,
    },
}

#[derive(Debug)]
pub struct MetaMsg {
    pub session_id: usize,
    pub generation: u64,
    pub kind: MetaKind,
}

/// Spawn one metadata job. The lock serializes catalog queries with user
/// queries on the shared connection (ODPI-C must not be used concurrently);
/// the UI thread never blocks on it.
pub fn spawn_meta_job(
    conn: Arc<dyn DbConnection>,
    exec_lock: Arc<Mutex<()>>,
    tx: Sender<MetaMsg>,
    session_id: usize,
    generation: u64,
    request: MetaRequest,
) {
    std::thread::spawn(move || {
        let dialect = conn.db_type();
        let kind = {
            let _guard = exec_lock.lock().unwrap_or_else(|e| e.into_inner());
            match request {
                MetaRequest::Bootstrap => {
                    let current_schema = conn.current_schema().map_err(|e| e.to_string());
                    let schemas = conn.list_schemas().map_err(|e| e.to_string());
                    let objects = match &current_schema {
                        Ok(cs) => conn
                            .list_schema_objects(cs)
                            .map(|objs| (cs.clone(), objs))
                            .map_err(|e| e.to_string()),
                        Err(e) => Err(e.clone()),
                    };
                    MetaKind::Bootstrap {
                        current_schema,
                        schemas,
                        objects,
                    }
                }
                MetaRequest::Objects { schema } => MetaKind::Objects {
                    objects: conn.list_schema_objects(&schema).map_err(|e| e.to_string()),
                    schema,
                },
                MetaRequest::Columns {
                    refs,
                    current_schema,
                } => {
                    let requested: Vec<(String, String)> = refs
                        .iter()
                        .map(|r| {
                            let schema = r.schema.as_deref().unwrap_or(&current_schema);
                            (
                                norm_ident(dialect, schema, false),
                                norm_ident(dialect, &r.name, false),
                            )
                        })
                        .collect();
                    match conn.table_columns_batch(&refs, &current_schema) {
                        Ok(found) => MetaKind::Columns {
                            requested,
                            found,
                            error: None,
                        },
                        Err(e) => MetaKind::Columns {
                            requested,
                            found: Vec::new(),
                            error: Some(e.to_string()),
                        },
                    }
                }
            }
        };
        let _ = tx.send(MetaMsg {
            session_id,
            generation,
            kind,
        });
    });
}

/// True when a statement changes the schema and should invalidate metadata.
pub fn is_ddl(sql: &str) -> bool {
    let mut head = sql.trim_start();
    loop {
        if let Some(rest) = head.strip_prefix("--") {
            head = rest
                .split_once('\n')
                .map(|(_, r)| r)
                .unwrap_or("")
                .trim_start();
            continue;
        }
        if let Some(rest) = head.strip_prefix("/*") {
            head = rest
                .split_once("*/")
                .map(|(_, r)| r)
                .unwrap_or("")
                .trim_start();
            continue;
        }
        break;
    }
    let word = head
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    matches!(
        word.as_str(),
        "CREATE" | "ALTER" | "DROP" | "RENAME" | "TRUNCATE" | "COMMENT" | "GRANT" | "REVOKE"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddl_detection() {
        assert!(is_ddl("create table t (a number)"));
        assert!(is_ddl("  -- hi\n  DROP TABLE t"));
        assert!(is_ddl("/* x */ alter table t add b number"));
        assert!(!is_ddl("select * from t"));
        assert!(!is_ddl("insert into t values (1)"));
    }

    #[test]
    fn normalization_per_dialect() {
        assert_eq!(norm_ident(DbType::Oracle, "emp", false), "EMP");
        assert_eq!(norm_ident(DbType::Oracle, "Emp", true), "Emp");
        assert_eq!(norm_ident(DbType::Postgres, "EMP", false), "emp");
        assert_eq!(norm_ident(DbType::Postgres, "Emp", true), "Emp");
    }
}
