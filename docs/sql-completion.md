# Toad-style SQL completion for frog

Design notes / research summary. Status: **Phase 1 implemented** — manual
`Ctrl+Space` popup, statement-scope alias/CTE resolution, cached metadata worker
(Oracle + Postgres), no auto-popup yet. Remaining phases are listed at the end.

Goal: when you type `SELECT * FROM emp e WHERE e.` you get an instant list of `emp`
columns; `FROM sc.` lists tables/views in schema `sc`; `FROM ` lists tables,
views and schemas. Suggestions must feel local (sub-millisecond filtering) even
when the dictionary is on a remote Oracle box, and must never stall the editor
while a catalog query runs.

Non-goals for v1: full SQL validation, AI suggestions, completion inside PL/SQL
bodies, cross-database/DB-link resolution.

---

## 1. What existing tools do

| Tool | What it does | Lesson for frog |
|---|---|---|
| **Toad for Oracle** | "Code Insight" popup with a configurable auto-invoke delay; manual invoke on `Ctrl+.`; object types selectable; public synonyms disabled by default *for performance*; aliases resolve to columns. | The Toad feel is mostly: alias → columns, plus a cheap, preloaded object list. Auto-popup must be delay-based and disableable. Don't preload public synonyms. |
| **DBeaver** | Three engines: Semantic (parses statement into scopes, resolves aliases), Legacy (position-based), Combined. "Hippie" engine completes words found in the current script. Since 24.3.4, *metadata requests are no longer sent while typing* when auto-activate is on; a separate option controls whether table/columns are read for semantic analysis. | Semantic scope resolution is the feature, but it runs on **cached** metadata. Keep a word-based (Hippie) layer as a free fallback for aliases, CTE names and identifiers already in the script. |
| **DataGrip** | Introspection levels 1/2/3 (names → everything but source → everything). Defaults depend on size: current schema level 3 only if ≤1000 objects, level 2 if ≤3000, else level 1; non-current schemas never level 3; system schemas level 2 only if ≤100 objects. Automatic refresh on DDL, manual force refresh. | Tiered laziness is the scalable answer. **Don't** load all columns of a huge schema just to complete one query. |
| **pgAdmin** | Historically queried PG catalogs on every completion request. Issue #10461 documents the fix: 300 ms debounce, cancel in-flight requests, then a hierarchical lazy cache: static keywords → per-schema object names → columns only for tables referenced by the query. TTL 5–10 min, DDL invalidation, background refresh while serving stale data. Target: <5 ms cached completion. | This is essentially the blueprint below. The failure mode (network round-trip per keystroke) is exactly what we must avoid. |
| **DbVisualizer** | Metadata cache modes: Persistent (default), Shared, Transient, Disabled; keep-unused-for-N-days; refresh from the completion popup. | Persistence across runs is a cheap win, and users need an explicit refresh path. |
| **psql** | Readline tab-completion issues catalog queries at completion time (documented to interfere with e.g. `BEGIN; SET TRANSACTION ...`). | Fine for a 200 ms Tab press, not for auto-popup on every key. |
| **pgcli / mycli** | A background "completion refresher" loads schemas/tables/columns into in-memory dicts on connect and re-runs after DDL; prompt filtering is local. | Background preload + local filtering is the proven terminal-app pattern. |
| **Oracle dictionary** | `ALL_TAB_COLUMNS` is privilege-filtered and can be very slow (worse in multitenant/PDB setups); `USER_TAB_COLUMNS` / `USER_OBJECTS` avoid that filter and are much faster for your own schema. | Use `USER_*` for the current schema, `ALL_*` with `OWNER = :1` only when a statement explicitly references another schema. Batch by referenced tables, never scan everything. |

Key takeaway: no modern tool asks the database "what are all the columns?"
while you type. They all cache names and resolve columns lazily.

---

## 2. Design principles

1. **Completion never does I/O on the keystroke path.** Analysis + filtering run
   in memory. Catalog work happens on a worker thread, debounced, and is
   discarded if the user kept typing.
2. **Three cache levels.** Static keywords → object names per schema → columns
   of referenced tables. Load each level only when it is needed.
3. **Scope from the current statement, not the whole buffer.** A 500-line
   script costs the same as a one-liner.
4. **Stale-while-revalidate.** Never blank the popup because a refresh started;
   serve cached data and swap in the new list when it lands.
5. **Everything is bounded.** Caps on rows, objects, columns and memory; huge
   schemas degrade to lazy mode instead of hanging.

---

## 3. Architecture

```
key event ──► completion context (pure, sync, <1 ms)
                 │  word prefix, qualifiers, clause, aliases, table refs
                 ▼
           candidate provider ──► static keywords/functions
                 │            ──► in-memory MetadataStore (per session)
                 │            ──► "unknown table" → enqueue MetaReq, show partial list
                 ▼
           rank + top-k (pure) ──► popup (ratatui overlay)
                 ▲
                 │ MetaMsg (background)
        metadata worker thread ──► DbConnection batch queries
```

Proposed modules:

```
src/db/sql_scan.rs       # shared SQL scanner (extracted from statements_with_ranges)
                         # + tokenize(text) -> tokens with byte ranges
src/db/completion.rs     # CompletionContext, alias/CTE analysis, ranking (pure)
src/db/metadata.rs       # MetadataStore, cache entries, worker, MetaReq/MetaMsg
src/tui/completion.rs    # popup state + rendering (or a section of widgets.rs)
```

`src/db/connection.rs` gets a small metadata API instead of forcing callers to
issue one query per object:

```rust
pub struct TableRef {
    pub schema: Option<String>,   // None = resolve via current schema/search_path
    pub name: String,
    pub alias: Option<String>,
    pub quoted: bool,
}

pub enum MetaObjectKind { Table, View, MView, Synonym, Sequence, Procedure, Function, Package, Type }

pub struct MetaObject {
    pub schema: String,
    pub name: String,
    pub kind: MetaObjectKind,
    /// For synonyms: the resolved target, when cheap to obtain.
    pub target: Option<(String, String)>,
}

pub trait DbConnection {
    // existing methods stay...
    fn current_schema(&self) -> anyhow::Result<String>;
    fn list_schemas(&self) -> anyhow::Result<Vec<String>>;                 // already exists
    fn list_schema_objects(&self, schema: &str) -> anyhow::Result<Vec<MetaObject>>; // 1 query, all kinds
    fn table_columns_batch(&self, refs: &[TableRef]) -> anyhow::Result<Vec<(TableRef, Vec<ColumnInfo>)>>;
}
```

`list_schema_objects` is deliberately one query per schema — calling the
existing `list_objects` once per type folder would mean ~10 round trips. The
existing F12 explorer can later be pointed at the same store and stop issuing
synchronous dictionary queries on the UI thread.

### 3.1 In-memory model

```rust
pub struct MetadataStore {
    pub dialect: DbType,
    pub current_schema: String,
    pub schemas: Vec<String>,                  // sorted, for prefix search
    pub objects: HashMap<String, SchemaObjects>, // key: normalized schema
    pub columns: HashMap<TableKey, CachedColumns>, // LRU, key: (schema, table)
    pub last_full_refresh: Instant,
    pub generation: u64,                       // bumped on DDL / manual refresh
    pub pending: HashSet<MetaReqKey>,          // in-flight de-dup
}

pub struct SchemaObjects {
    pub entries: Vec<MetaObject>,  // sorted by normalized name
    pub loaded_at: Instant,
    pub truncated: bool,           // hit the row cap
}

pub struct CachedColumns {
    pub cols: Vec<ColumnInfo>,
    pub loaded_at: Instant,
}
```

`TableKey` uses dialect-aware normalization: uppercase for Oracle unless the
identifier was quoted, lowercase for Postgres unless quoted.

---

## 4. Context analysis (the Toad feel)

Given the full editor text and cursor offset:

1. Find the current statement with `SqlEditor::statements_with_ranges` (already
   SQL-aware: strings/comments/PL-SQL blocks).
2. Walk back from the cursor on the current line to the start of the identifier
   word. Allowed chars: `[A-Za-z0-9_$#]`; a leading `"` switches to
   case-sensitive quoted mode. Split on `.`:
   `sc.emp.e|` → qualifiers `["sc","emp"]`, prefix `"e"`; `e.|` → prefix `""`.
3. Determine clause context by scanning backwards for the nearest top-level
   keyword, skipping strings/comments and parenthesized subqueries:
   `FROM`/`JOIN` → relations/schemas; `SELECT`/`WHERE`/`ON`/`GROUP BY`/
   `ORDER BY`/`HAVING`/`SET` → columns/functions; after `INSERT INTO`/`UPDATE`
   → relations; after `CALL`/`EXEC` → procedures/packages.
4. Extract relation references from the statement's `FROM`/`JOIN`/`,` list:

   ```
   FROM [schema.]table [alias]
   JOIN [schema.]table [alias] ON ...
   FROM a, b
   FROM (SELECT ...) x          -- v1: marks x as opaque, no columns
   WITH x AS (SELECT ...) ...   -- v1: x is a relation name, columns in v2
   ```

   Map `alias → TableRef`; a schema-qualified table without alias is addressable
   by its table name; a bare table is addressable by its own name too.
5. If the qualifier matches an alias/table → ask for that table's columns.
   If it matches a schema → ask for that schema's objects.
   If it matches both (schema and alias share a name) → prefer the alias.
6. No prefix, no qualifier, clause = FROM → relations in current schema first,
   then other schemas, then schemas themselves.

The scanner should be shared with `statements_with_ranges` so quoting and
comment rules can never diverge. This is a refactor, not new lexical logic.

Deliberately hand-rolled rather than pulling in `sqlparser`: Oracle syntax
(`CONNECT BY`, `(+)`, package calls, `MERGE`) is only partially covered by
generic parsers, and even DBeaver/DataGrip carry per-dialect parsers. A
scope extractor needs the FROM list and the cursor token, not a full AST.
Revisit `sqlparser` only if we later want `SELECT *` expansion or CTE column
inference.

---

## 5. Catalog strategy (Oracle first, Postgres equivalent)

### Level 0 — static (zero I/O)

- SQL keywords and clause phrases (`SELECT`, `FROM`, `WHERE`, `GROUP BY`, …).
- Built-in functions per dialect (`NVL`, `DECODE`, `TO_CHAR`, `COUNT`, `COALESCE`,
  …). Curated lists in the binary; no `ALL_OBJECTS` query for `SYS` packages.
- A "Hippie" layer: identifiers already present in the current script
  (aliases, CTE names, words the user just typed). Zero cost, surprisingly
  useful for aliases that failed to resolve.

### Level 1 — schemas + object names

**Oracle, current schema** (fast; no privilege filter):

```sql
SELECT OBJECT_NAME, OBJECT_TYPE
FROM   USER_OBJECTS
WHERE  OBJECT_TYPE IN ('TABLE','VIEW','MATERIALIZED VIEW','SEQUENCE',
                       'PROCEDURE','FUNCTION','PACKAGE','TYPE')
ORDER  BY OBJECT_NAME
```

**Oracle, another schema** (only when referenced or when completing `sc.`):

```sql
SELECT OBJECT_NAME, OBJECT_TYPE
FROM   ALL_OBJECTS
WHERE  OWNER = :1
AND    OBJECT_TYPE IN ('TABLE','VIEW','MATERIALIZED VIEW','SEQUENCE',
                       'PROCEDURE','FUNCTION','PACKAGE','TYPE')
ORDER  BY OBJECT_NAME
```

**Oracle current schema** is *not* `USER`:

```sql
SELECT SYS_CONTEXT('USERENV','CURRENT_SCHEMA') FROM dual
```

(`ALTER SESSION SET CURRENT_SCHEMA` changes it while `USER` stays the login.)

**Postgres** (search_path first; `current_schemas(false)` excludes `pg_catalog`):

```sql
SELECT n.nspname, c.relname, c.relkind
FROM   pg_catalog.pg_class c
JOIN   pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE  c.relkind IN ('r','p','v','m','f')
AND    n.nspname = ANY (current_schemas(false))
ORDER  BY 1, 2
```

Unqualified completion suggests search_path relations first; other schemas are
only listed when the user typed `schema.` (matches how names actually resolve).

**Synonyms** (Oracle): `USER_SYNONYMS` for own + `ALL_SYNONYMS` where
`OWNER='PUBLIC'` only when `completion.include_public_synonyms = true`
(off by default — Toad does the same, public synonym lists are huge). Resolve
`syn.` columns from the synonym target, cached like a table.

### Level 2 — columns of tables referenced by the statement

This is the important trick: after a short debounce, extract the table refs
from the current statement and fetch **only those** tables' columns in one
batched query.

**Oracle:**

```sql
SELECT OWNER, TABLE_NAME, COLUMN_NAME, DATA_TYPE, NULLABLE, COLUMN_ID
FROM   ALL_TAB_COLUMNS
WHERE  (OWNER, TABLE_NAME) IN ((:1,:2), (:3,:4) /* ... */)
ORDER  BY OWNER, TABLE_NAME, COLUMN_ID
```

Unqualified tables resolve to the current schema; for the current schema prefer
`USER_TAB_COLUMNS` (no owner filter, much faster), e.g.:

```sql
SELECT TABLE_NAME, COLUMN_NAME, DATA_TYPE, NULLABLE, COLUMN_ID
FROM   USER_TAB_COLUMNS
WHERE  TABLE_NAME IN (:1, :2, ...)
ORDER  BY TABLE_NAME, COLUMN_ID
```

**Postgres** (one round trip, `to_regclass` handles quoting and search_path):

```sql
SELECT n.nspname, c.relname, a.attname,
       pg_catalog.format_type(a.atttypid, a.atttypmod),
       a.attnotnull, a.attnum
FROM   pg_catalog.pg_attribute a
JOIN   pg_catalog.pg_class c     ON c.oid = a.attrelid
JOIN   pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE  a.attnum > 0
AND    NOT a.attisdropped
AND    c.oid = ANY (SELECT to_regclass(x) FROM unnest($1::text[]) x)
ORDER  BY 1, 2, 6
```

Pass `["public.emp", "\"Weird Table\""]` as `$1`.

Typical statement: 1–4 tables, 20–200 column rows. Cost is dominated by the
round trip, not the dictionary.

### Level 2.5 — optional full current-schema columns

Only when the schema is small (DataGrip's heuristic): if object count and
estimated columns are under a threshold (say ≤200 tables / ≤5000 columns), load
all current-schema columns in the background once, so unqualified column
completion is instant for the whole schema. Above the threshold, stay lazy.
This one toggle prevents the "big schema becomes unusable" failure mode.

### Level 3 — not loaded

Source/DDL, comments, constraints, indexes: no. The F12 explorer already has
those on demand.

### Caps

- `max_objects_per_schema` (default 20 000) — stop and mark truncated.
- `max_columns` (default 100 000) — LRU-evict whole tables.
- Hard row cap per query (e.g. 20 000) using `ROWNUM`/`FETCH FIRST`.
- Never load `PUBLIC` synonyms or `SYS` objects by default.

---

## 6. When to fetch

| Trigger | Action |
|---|---|
| Connection succeeds (`poll_conn_result`) | Background: `current_schema`, `list_schemas`, current-schema objects. |
| User opens completion (`Ctrl+Space`) | Serve cache immediately; if the qualifier's schema/table is unknown, enqueue a request and update the popup when it lands. |
| Editor text changes (auto mode) | Debounce ~250 ms; re-run context analysis; enqueue missing table refs for the current statement only. No request for keywords/aliases. |
| `.` typed after an alias/table | Immediate popup if columns are cached; otherwise enqueue and show a "loading" row. |
| DDL executed by frog | Detect `CREATE`/`ALTER`/`DROP`/`RENAME`/`TRUNCATE`/`COMMENT`/`GRANT`/`REVOKE` as the first keyword of a successfully executed statement → bump generation, drop that schema's objects and affected columns; next completion reloads in background. |
| TTL expires (default 10 min) | Stale-while-revalidate: keep serving, refresh in background. |
| `Ctrl+Space` on an already-open popup, or `F5` in the popup | Force refresh of the relevant level. |
| `:refresh-meta` command | Clear the store for this session. |

---

## 7. Concurrency & cancellation

- One metadata worker per session, fed by an `mpsc` channel; it owns the
  `Arc<dyn DbConnection>` and performs all catalog queries.
- Coalesce: keep a `HashSet<MetaReqKey>` of in-flight keys so holding a key
  down doesn't queue 30 identical queries.
- Tag every request/response with `(session_id, generation, statement_seq)`.
  Drop responses whose statement/generation is older than what the UI is
  showing; never overwrite newer data with older data.
- **Connection choice:** v1 uses the session's connection under a metadata
  mutex, so ODPI-C is never called concurrently. If a user query is running,
  completion serves cache and marks column lists as pending (no blocking).
  Add `completion.dedicated_metadata_connection: true` as an opt-in: a lazily
  opened second Oracle/Postgres session for metadata only (what pgAdmin and
  DataGrip do). It costs one lightweight DB session and removes all contention.
- Timeouts: catalog queries are small, but add a soft deadline (e.g. 2 s) after
  which the worker abandons the result and reports "metadata timeout" in the
  popup footer. Abandoning is safe because it's just a SELECT.
- Never call `break_execution` from the worker: that would cancel the user's
  query on a shared connection. If a metadata query must be aborted, use a
  dedicated connection instead.

Today's F12 explorer issues several queries synchronously on the UI thread
(`refresh_explorer`, `load_schema_groups`). Completion must not reuse that
path; a follow-up can move the explorer onto the same worker/store.

---

## 8. Ranking

Score tiers (lower is better), compared case-insensitively on normalized names:

1. exact match
2. prefix match (`EMP` → `EMP`, `EMP_MASTER`)
3. word-segment prefix (`EM` → `EMP_MASTER` via `_`/camel-case boundaries)
4. substring (`EMP` → `MY_EMP`)
5. subsequence (`EMPID` → `EMPLOYEE_ID`)

Boosts / tie-breaks, in order:

- alias or table already used in the statement (columns)
- object in the current schema
- object kind relevance for the clause (tables in `FROM`, columns in `SELECT`)
- recently used object in this session / history
- shorter name, then alphabetical

Implementation notes:

- Keep each schema's entries sorted by normalized name so prefix candidates are
  a binary-search range, not a full scan. For 100k names a prefix scan is
  microseconds; substring fallback is limited to short prefixes (<2 chars are
  literal-only anyway).
- Maintain a fixed-size top-k (k ≈ 15) with a binary heap; never sort the whole
  candidate set on each keystroke.
- Cache the last `(statement_hash, cursor, prefix)` result so cursor movement
  and PgUp/PgDn don't re-filter.

---

## 9. UX

**Keys**

| Key | Action |
|---|---|
| `Ctrl+Space` | Open completion / refresh if already open |
| `Ctrl+.` | Alias for terminals that eat `Ctrl+Space` (Toad's manual invoke) |
| typing | Filter locally; auto-popup optional |
| `Up`/`Down`, `PgUp`/`PgDn` | Move selection |
| `Enter` / `Tab` | Accept selected item (only while popup is open — Tab still cycles panels otherwise) |
| `Esc` | Close popup |
| `Ctrl+Shift+Space` | Accept only the identifier part up to the last dot (DBeaver-style) |

`Ctrl+Space` delivery is terminal-dependent: handle both
`KeyEvent { code: Char(' '), modifiers: CONTROL }` and `KeyCode::Null` /
`Char('\0')`.

**Auto mode** (config, default off initially, like Toad's auto-invoke):

- Trigger after an identifier char, `.`, or `,` once the identifier is ≥2 chars,
  with a ~250 ms idle delay. `.` can open immediately (members are usually
  already cached due to statement preloading).
- Never trigger inside strings/comments or in the middle of a number.
- Any keypress resets the timer; a pending metadata request is not cancelled,
  but its result is discarded if the statement changed.

**Insertion rules**

- Replace the current word (from word start to cursor) with the chosen name.
- Oracle: unquoted names are case-insensitive; insert the stored spelling
  (usually uppercase) unless the typed prefix is all-lowercase, in which case
  insert lowercase (keeps user style, still valid). Quoted/mixed-case objects
  are inserted quoted, with `"` doubled.
- Postgres: insert lowercase for unquoted, quoted for mixed case/reserved
  words. A name that needs quoting is quoted automatically.
- After accepting a schema or table, do not append `.` automatically (the user
  may want a space/alias); if the next char is already `.`, don't duplicate it.
- Show the detail column: kind + schema for objects, `TYPE NULL/NOT NULL` for
  columns, `alias e for EMP` for aliases. This is what makes the list readable.

**Popup rendering**

- Compute the screen cell of the editor cursor: editor inner origin + gutter
  width + display width of `line[..cursor_col]` (`unicode-width`, already a
  dependency), row = `cursor_row - scroll_offset`.
- Prefer below the cursor; flip above when out of room; clamp width
  (`30..60` cells) to the editor area.
- `Clear` then draw a bordered list; mark the selected row with the focused
  color. Keep the terminal cursor visible in the editor.

---

## 10. Performance budget (targets)

| Path | Target | Notes |
|---|---|---|
| Cached filter + rank (10k names) | < 0.5 ms | prefix binary search + top-k |
| Cached filter + rank (200k names) | < 3 ms | still one frame at 60 fps |
| Popup open with cached data | same frame | zero DB I/O |
| L1 names, local Oracle | 5–50 ms | one dictionary query |
| L1 names, remote Oracle | 100–800 ms | hidden behind cache; prefetched on connect |
| L2 columns batch, local | 2–20 ms | 1–4 tables |
| L2 columns batch, remote | 50–300 ms | only on statement change |
| Memory | < 32 MB default cap | names ~50 B each; columns LRU |

Measure with `Instant` around worker queries, surfaced via
`:completion-stats` (a command-bar command) or `RUST_LOG`-style debug output.
Add a `#[ignore]` bench test with a synthetic 100k-name catalog to catch
accidental O(n log n) sorts on the keystroke path.

---

## 11. Phased delivery

**Phase 1 — manual popup (implemented)**

- [x] Extract `sql_scan` from `statements_with_ranges` (`src/db/sql_scan.rs`).
- [x] `completion.rs`: context/word extraction, FROM/JOIN alias mapping, ranking.
- [x] `metadata.rs`: L1 current-schema objects + L2 columns for referenced tables,
  worker thread, caching, TTL, DDL invalidation.
- [x] `Ctrl+Space` popup in the editor; accept with Enter/Tab (`Ctrl+.` fallback).
- [x] Oracle + Postgres queries above.
- [x] Static keyword/function lists + statement-identifier ("Hippie") fallback.
- [x] Tests: `e.` → emp columns; `sc.` → objects; unqualified `FROM ` → tables;
  quoted identifiers; strings/comments ignored; popup selection/accept.

**Phase 2 — auto-popup + polish**

- Debounced auto-invoke, config toggle and delay.
- Better ranking/boosts, detail column, `SELECT *` expansion.
- Dedicated metadata connection option.
- Move F12 explorer onto the shared store (removes UI-thread queries).

**Phase 3 — scale & persistence**

- CTE columns, subquery aliases, synonyms target resolution.
- Persistent on-disk cache (`~/.cache/frog/meta/<hash>.json`, versioned,
  atomic writes, 7-day stale window; opt-in) for instant first completion.
- Incremental refresh on DDL and a manual "refresh metadata" action.
- Optional `completion.max_columns` / `max_objects` tuning in `config.yml`.

**Config sketch**

```yaml
completion:
  enabled: true
  auto: false              # auto-popup after typing
  delay_ms: 250
  ttl_secs: 600
  include_public_synonyms: false
  dedicated_metadata_connection: false
  max_objects_per_schema: 20000
  max_columns: 100000
  preserve_case: true      # lowercase prefix -> lowercase insert
```

---

## 12. Risks / edge cases

- **Oracle dictionary latency and privileges.** Missing `SELECT_CATALOG_ROLE`
  makes `ALL_*` fail; `ALL_TAB_COLUMNS` can be slow in PDBs. Prefer `USER_*`,
  cap rows, and surface a one-line hint instead of an empty popup (reuse
  `explorer_error_hint`).
- **Case and quoting.** Oracle folds unquoted identifiers to uppercase;
  Postgres to lowercase. A quoted `"Emp"` must match exactly and be inserted
  quoted. Normalize per dialect, never globally.
- **Huge schemas.** Without caps, a 500k-column schema will exhaust memory and
  make filtering slow. The level-2.5 size threshold and LRU caps are mandatory.
- **Stale metadata.** Expected; TTL + DDL invalidation + manual refresh, and
  label the popup footer with "cached 3m ago" when it matters.
- **Alias ambiguity.** `FROM emp e, dept e` is invalid SQL; don't over-engineer.
  Schema vs alias collisions prefer the alias.
- **PL/SQL.** Cursor inside a `BEGIN ... END;` block: fall back to static +
  Hippie + global names; don't try to resolve local variables in v1.
- **Terminal key delivery.** `Ctrl+Space` may arrive as NUL; provide `Ctrl+.`
  as a fallback and document it.
- **Extra DB session.** Some DBAs dislike a second session; keep the default
  shared and make the dedicated connection opt-in.
- **Editor is line/cell based.** Popup positioning must use display width, not
  byte length, for tabs/wide chars; frog already tracks cursor as a byte offset.

---

## 13. References

- DBeaver — SQL Assist and Auto Complete: <https://dbeaver.com/docs/dbeaver/SQL-Assist-and-Auto-Complete/>
- DBeaver release notes (24.3.x/25.x completion + metadata changes): <https://dbeaver.io/category/releases/>
- DataGrip — Introspection levels: <https://www.jetbrains.com/help/datagrip/introspection-levels.html>
- DataGrip — Metadata and introspection: <https://www.jetbrains.com/help/datagrip/introspection.html>
- pgAdmin issue #10461 — debounce + hierarchical lazy schema cache: <https://github.com/pgadmin-org/pgadmin4/issues/10461>
- DbVisualizer — Metadata cache: <https://www.dbvis.com/docs/ug/database-connection-options/metadata-cache>
- PostgreSQL — psql tab completion (server queries at completion time): <https://www.postgresql.org/docs/current/app-psql.html>
- Toad for Oracle — Code Insight tips (delay, object types, public synonyms): <https://i.dell.com/sites/doccontent/corporate/secure/en/Documents/Toad_Tips_and_Tricks.pdf>
- Oracle `ALL_TAB_COLUMNS` / `USER_TAB_COLUMNS` docs: <https://docs.oracle.com/en/database/oracle/oracle-database/19/refrn/ALL_TAB_COLUMNS.html>
- Dictionary query slowness in multitenant (context, not a fix to rely on): <https://mikedietrichde.com/2025/02/10/queries-on-dictionary-tables-are-slow-in-oracle-multitenant/>
- pgcli (background completion refresher): <https://www.pgcli.com/docs>
