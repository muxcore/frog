//! Pure SQL completion engine: context extraction, alias/scope resolution and
//! ranking. No I/O happens here; metadata comes from `MetadataStore`.
//!
//! The flow is:
//!   analyze(text, cursor) -> CompletionContext  (prefix, qualifiers, refs)
//!   build_items(ctx, store) -> Vec<CompletionItem> (ranked, capped)
//!
//! Both are cheap enough to run on every keystroke while the popup is open.

use crate::db::connection::{DbType, MetaObject, MetaObjectKind, TableRef};
use crate::db::metadata::MetadataStore;
use crate::db::sql_scan::{self, Token, TokenKind, WordPart};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Clause {
    Select,
    From,
    Join,
    Where,
    GroupOrder,
    Set,
    Dml,
    #[default]
    Other,
}

#[derive(Debug, Clone, Default)]
pub struct CompletionContext {
    pub dialect: DbType,
    /// Byte column on the cursor line where the replaceable word starts.
    pub word_start: usize,
    pub prefix: String,
    pub prefix_quoted: bool,
    /// Qualifiers before the last dot, outermost first.
    pub parts: Vec<WordPart>,
    pub clause: Clause,
    /// Tables/views in the current statement's FROM/JOIN clauses.
    pub refs: Vec<TableRef>,
    /// CTE names (`WITH x AS ...`).
    pub ctes: Vec<String>,
    /// Derived-table aliases (`FROM (SELECT ...) x`): known to be relations,
    /// but their columns are not resolved in v1.
    pub derived: Vec<String>,
    /// All identifiers in the statement, used as a zero-cost "Hippie" layer.
    pub identifiers: Vec<String>,
    pub in_literal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Table,
    View,
    MView,
    Synonym,
    Sequence,
    Procedure,
    Function,
    Package,
    Type,
    Column,
    Schema,
    Keyword,
    Word,
    Other,
}

impl ItemKind {
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
            Self::Column => "C",
            Self::Schema => "D",
            Self::Keyword => "K",
            Self::Word => "W",
            Self::Other => "?",
        }
    }

    fn from_meta(kind: MetaObjectKind) -> Self {
        match kind {
            MetaObjectKind::Table => Self::Table,
            MetaObjectKind::View => Self::View,
            MetaObjectKind::MView => Self::MView,
            MetaObjectKind::Synonym => Self::Synonym,
            MetaObjectKind::Sequence => Self::Sequence,
            MetaObjectKind::Procedure => Self::Procedure,
            MetaObjectKind::Function => Self::Function,
            MetaObjectKind::Package => Self::Package,
            MetaObjectKind::Type => Self::Type,
            MetaObjectKind::Other => Self::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    pub insert_text: String,
    pub kind: ItemKind,
    pub detail: String,
}

/// Popup state. Kept UI-only: the engine remains pure, this is just what the
/// renderer and key handler need to show and accept a suggestion.
#[derive(Debug, Default)]
pub struct CompletionPopup {
    pub items: Vec<CompletionItem>,
    pub selected: usize,
    /// Byte column on the cursor line where the replaceable word starts.
    pub word_start: usize,
    /// Prefix the list was filtered by; a change resets the highlight.
    pub prefix: String,
    /// Cursor the popup was computed for. Acceptance only edits the line when
    /// it still matches, so a stale popup can never corrupt the text.
    pub cursor_row: usize,
    pub cursor_col: usize,
    /// Metadata was still loading when the list was built.
    pub loading: bool,
    pub note: Option<String>,
}

impl CompletionPopup {
    pub fn new(
        items: Vec<CompletionItem>,
        word_start: usize,
        cursor_row: usize,
        cursor_col: usize,
        loading: bool,
    ) -> Self {
        Self {
            items,
            selected: 0,
            word_start,
            prefix: String::new(),
            cursor_row,
            cursor_col,
            loading,
            note: None,
        }
    }

    pub fn selected_item(&self) -> Option<&CompletionItem> {
        self.items.get(self.selected)
    }

    /// Move the highlight by `delta` rows, clamped to the item list.
    pub fn move_selection(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let last = self.items.len() - 1;
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, last as isize) as usize;
    }

    /// Number of rows the list is currently scrolled down by, so the selected
    /// item stays inside a `visible`-row window.
    pub fn scroll_offset(&self, visible: usize) -> usize {
        if visible == 0 || self.selected < visible {
            0
        } else {
            self.selected + 1 - visible
        }
    }
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

pub fn analyze(text: &str, cursor: usize, dialect: DbType) -> CompletionContext {
    let cursor = cursor.min(text.len());
    let line_start = text[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = text[line_start..]
        .find('\n')
        .map(|i| line_start + i)
        .unwrap_or(text.len());
    let line = &text[line_start..line_end];
    let col = cursor.saturating_sub(line_start).min(line.len());
    let word = sql_scan::word_before(line, col);

    if word.in_literal {
        return CompletionContext {
            dialect,
            word_start: word.start,
            in_literal: true,
            ..CompletionContext::default()
        };
    }

    let ranges = sql_scan::statement_ranges(text);
    let (stmt_start, stmt_end) = range_containing(&ranges, cursor, text.len());
    let stmt = &text[stmt_start..stmt_end];
    let rel_cursor = cursor.saturating_sub(stmt_start).min(stmt.len());
    let tokens = sql_scan::tokenize(stmt);

    let clause = clause_at(&tokens, rel_cursor);
    let Extracted {
        refs,
        ctes,
        derived,
    } = extract_refs(&tokens);
    let identifiers = collect_identifiers(&tokens);

    CompletionContext {
        dialect,
        word_start: word.start,
        prefix: word.prefix,
        prefix_quoted: word.prefix_quoted,
        parts: word.parts,
        clause,
        refs,
        ctes,
        derived,
        identifiers,
        in_literal: false,
    }
}

fn range_containing(
    ranges: &[(usize, usize, String)],
    cursor: usize,
    text_len: usize,
) -> (usize, usize) {
    let mut best = (0usize, text_len);
    for (a, b, _) in ranges {
        if cursor >= *a && cursor <= *b {
            return (*a, *b);
        }
        if *a <= cursor {
            best = (*a, (*b).min(text_len));
        }
    }
    best
}

fn clause_at(tokens: &[Token], rel: usize) -> Clause {
    let mut depth = 0i32;
    for t in tokens {
        if t.start >= rel {
            break;
        }
        if t.is_punct(")") {
            depth = (depth - 1).max(0);
        }
        if t.is_punct("(") {
            depth += 1;
        }
    }
    let cursor_depth = depth;

    depth = 0;
    let mut clause = Clause::Other;
    for t in tokens {
        if t.start >= rel {
            break;
        }
        if t.is_punct(")") {
            depth = (depth - 1).max(0);
        }
        if depth == cursor_depth && t.kind == TokenKind::Keyword {
            let word = t.text.to_ascii_uppercase();
            clause = match word.as_str() {
                "FROM" => Clause::From,
                "JOIN" => Clause::Join,
                "SELECT" => Clause::Select,
                "WHERE" | "HAVING" | "ON" | "AND" | "OR" | "WHEN" | "THEN" | "ELSE" | "CONNECT"
                | "START" | "BETWEEN" | "LIKE" | "IS" | "IN" | "USING" => Clause::Where,
                "GROUP" | "ORDER" | "BY" => Clause::GroupOrder,
                "SET" => Clause::Set,
                "INSERT" | "INTO" | "UPDATE" | "DELETE" | "MERGE" | "VALUES" | "RETURNING" => {
                    Clause::Dml
                }
                _ => clause,
            };
        }
        if t.is_punct("(") {
            depth += 1;
        }
    }
    clause
}

struct Extracted {
    refs: Vec<TableRef>,
    ctes: Vec<String>,
    derived: Vec<String>,
}

fn extract_refs(tokens: &[Token]) -> Extracted {
    let mut refs: Vec<TableRef> = Vec::new();
    let mut derived: Vec<String> = Vec::new();
    let mut seen: HashSet<(Option<String>, String, Option<String>)> = HashSet::new();
    let mut i = 0usize;

    while i < tokens.len() {
        if tokens[i].is_keyword("FROM") {
            i += 1;
            loop {
                let (next, parsed) = parse_ref(tokens, i, &mut refs, &mut derived, &mut seen);
                if !parsed {
                    break;
                }
                i = next;
                if tokens.get(i).map(|t| t.is_punct(",")).unwrap_or(false) {
                    i += 1;
                    continue;
                }
                break;
            }
            continue;
        }
        if tokens[i].is_keyword("JOIN") {
            let (next, _) = parse_ref(tokens, i + 1, &mut refs, &mut derived, &mut seen);
            i = next.max(i + 1);
            continue;
        }
        if tokens[i].is_keyword("UPDATE") || tokens[i].is_keyword("MERGE") {
            let (next, _) = parse_ref(tokens, i + 1, &mut refs, &mut derived, &mut seen);
            i = next.max(i + 1);
            continue;
        }
        if tokens[i].is_keyword("INTO")
            && i > 0
            && (tokens[i - 1].is_keyword("INSERT") || tokens[i - 1].is_keyword("MERGE"))
        {
            let (next, _) = parse_ref(tokens, i + 1, &mut refs, &mut derived, &mut seen);
            i = next.max(i + 1);
            continue;
        }
        i += 1;
    }

    let ctes = collect_ctes(tokens);
    Extracted {
        refs,
        ctes,
        derived,
    }
}

/// Parse `[schema.]name [AS] [alias]`. Returns (next index, parsed something).
fn parse_ref(
    tokens: &[Token],
    mut i: usize,
    refs: &mut Vec<TableRef>,
    derived: &mut Vec<String>,
    seen: &mut HashSet<(Option<String>, String, Option<String>)>,
) -> (usize, bool) {
    // Derived table / subquery: skip the parenthesized block and its alias.
    if tokens.get(i).map(|t| t.is_punct("(")).unwrap_or(false) {
        i = skip_paren_group(tokens, i);
        if tokens.get(i).map(|t| t.is_keyword("AS")).unwrap_or(false) {
            i += 1;
        }
        if let Some(t) = tokens.get(i) {
            if t.is_ident() {
                derived.push(t.text.clone());
                return (i + 1, true);
            }
        }
        return (i, true);
    }

    let Some(first) = tokens.get(i).filter(|t| t.is_ident()) else {
        return (i, false);
    };
    let (schema, schema_quoted, name, quoted) =
        if tokens.get(i + 1).map(|t| t.is_punct(".")).unwrap_or(false) {
            if let Some(second) = tokens.get(i + 2).filter(|t| t.is_ident()) {
                (
                    Some(first.text.clone()),
                    first.is_quoted(),
                    second.text.clone(),
                    second.is_quoted(),
                )
            } else {
                (None, false, first.text.clone(), first.is_quoted())
            }
        } else {
            (None, false, first.text.clone(), first.is_quoted())
        };
    i += if schema.is_some() { 3 } else { 1 };

    let mut alias = None;
    if tokens.get(i).map(|t| t.is_keyword("AS")).unwrap_or(false) {
        i += 1;
    }
    if let Some(t) = tokens.get(i) {
        if t.is_ident() {
            alias = Some(t.text.clone());
            i += 1;
        }
    }

    let key = (schema.clone(), name.clone(), alias.clone());
    if seen.insert(key) {
        refs.push(TableRef {
            schema,
            schema_quoted,
            name,
            quoted,
            alias,
        });
    }
    (i, true)
}

fn skip_paren_group(tokens: &[Token], mut i: usize) -> usize {
    if !tokens.get(i).map(|t| t.is_punct("(")).unwrap_or(false) {
        return i;
    }
    let mut depth = 0i32;
    while i < tokens.len() {
        if tokens[i].is_punct("(") {
            depth += 1;
        } else if tokens[i].is_punct(")") {
            depth -= 1;
            if depth <= 0 {
                return i + 1;
            }
        }
        i += 1;
    }
    i
}

fn collect_ctes(tokens: &[Token]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < tokens.len() {
        if tokens[i].is_keyword("WITH") {
            let mut j = i + 1;
            if tokens
                .get(j)
                .map(|t| t.is_keyword("RECURSIVE"))
                .unwrap_or(false)
            {
                j += 1;
            }
            while let Some(name) = tokens.get(j).filter(|t| t.is_ident()) {
                let name_text = name.text.clone();
                let mut k = j + 1;
                if tokens.get(k).map(|t| t.is_keyword("AS")).unwrap_or(false) {
                    out.push(name_text);
                    k = skip_paren_group(tokens, k + 1);
                } else {
                    break;
                }
                if tokens.get(k).map(|t| t.is_punct(",")).unwrap_or(false) {
                    j = k + 1;
                    continue;
                }
                j = k;
                break;
            }
            i = j;
        }
        i += 1;
    }
    out
}

fn collect_identifiers(tokens: &[Token]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for t in tokens {
        if t.is_ident() && seen.insert(t.text.to_ascii_uppercase()) {
            out.push(t.text.clone());
            if out.len() >= 300 {
                break;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Candidate generation + ranking
// ---------------------------------------------------------------------------

struct TopK {
    k: usize,
    items: Vec<(i32, CompletionItem)>,
}

impl TopK {
    fn new(k: usize) -> Self {
        Self {
            k,
            items: Vec::with_capacity(k + 1),
        }
    }

    fn push(&mut self, score: i32, item: CompletionItem) {
        if self.items.len() < self.k {
            self.items.push((score, item));
            return;
        }
        // Replace the worst candidate if the new one is better.
        let mut worst = 0usize;
        for idx in 1..self.items.len() {
            if key(&self.items[idx]) > key(&self.items[worst]) {
                worst = idx;
            }
        }
        let new_key = (score, item.label.len());
        if new_key < key(&self.items[worst]) {
            self.items[worst] = (score, item);
        }
    }

    fn into_sorted(mut self) -> Vec<CompletionItem> {
        self.items
            .sort_by(|a, b| key(a).cmp(&key(b)).then_with(|| a.1.label.cmp(&b.1.label)));
        self.items.into_iter().map(|(_, item)| item).collect()
    }
}

fn key(c: &(i32, CompletionItem)) -> (i32, usize) {
    (c.0, c.1.label.len())
}

/// Build ranked suggestions for a context from the in-memory store.
pub fn build_items(ctx: &CompletionContext, store: &MetadataStore) -> Vec<CompletionItem> {
    let mut top = TopK::new(24);
    let prefix = ctx.prefix.as_str();
    let case_sensitive = ctx.prefix_quoted;
    let mut seen_labels: HashSet<String> = HashSet::new();
    let qualified = !ctx.parts.is_empty();

    let mut column_refs: Vec<TableRef> = Vec::new();
    let mut object_schema: Option<String> = None;
    let mut want_objects = false;
    let mut want_schemas = false;

    match ctx.parts.len() {
        0 => match ctx.clause {
            Clause::From | Clause::Join | Clause::Dml => {
                want_objects = true;
                want_schemas = true;
            }
            _ => column_refs.extend(ctx.refs.iter().cloned()),
        },
        1 => {
            let q = &ctx.parts[0];
            if let Some(r) = find_ref(ctx, &q.text, q.quoted) {
                column_refs.push(r.clone());
            } else if let Some(schema) = store.find_schema(&q.text, q.quoted) {
                object_schema = Some(schema.to_string());
                want_objects = true;
            }
        }
        _ => {
            let schema_part = &ctx.parts[ctx.parts.len() - 2];
            let table_part = &ctx.parts[ctx.parts.len() - 1];
            if let Some(r) = find_ref(ctx, &schema_part.text, schema_part.quoted) {
                column_refs.push(r.clone());
            } else if let Some(schema) = store.find_schema(&schema_part.text, schema_part.quoted) {
                if let Some(cols) = store.columns_for(schema, &table_part.text) {
                    for c in cols {
                        add_column(&mut top, &mut seen_labels, ctx, c);
                    }
                }
            }
        }
    }

    // Columns of the tables in scope (aliases and table names are resolved
    // above; unqualified completion uses every reference).
    for r in &column_refs {
        if let Some(cols) = store.columns_for_ref(r) {
            for c in cols {
                add_column(&mut top, &mut seen_labels, ctx, c);
            }
        }
    }

    if want_objects {
        let current = store.current_schema.clone();
        if let Some(schema) = object_schema.as_deref() {
            if let Some(objs) = store.objects_for(schema, false) {
                for obj in objs {
                    add_object(&mut top, ctx, obj, false, &current);
                }
            }
        } else {
            // Current schema first.
            if let Some(objs) = store.objects_for(&current, false) {
                for obj in objs {
                    add_object(&mut top, ctx, obj, false, &current);
                }
            }
            // When the user is typing, also search other loaded schemas.
            if !prefix.is_empty() {
                let current_key = store.schema_key(&current);
                for (key, schema_objs) in &store.objects {
                    if key == &current_key {
                        continue;
                    }
                    for obj in &schema_objs.entries {
                        add_object(&mut top, ctx, obj, true, &current);
                    }
                }
            }
        }
    }

    if want_schemas {
        for schema in &store.schemas {
            add_schema(&mut top, ctx, schema);
        }
        for cte in &ctx.ctes {
            add_word(&mut top, ctx, cte, "CTE");
        }
        for alias in ctx.refs.iter().filter_map(|r| r.alias.as_ref()) {
            add_word(&mut top, ctx, alias, "alias");
        }
    }

    // Qualified completion (`e.`) is a member list: no keywords/functions.
    if !qualified {
        add_static(&mut top, ctx, case_sensitive);
        for word in &ctx.identifiers {
            if !prefix.is_empty() && !word.eq_ignore_ascii_case(prefix) {
                add_word(&mut top, ctx, word, "in statement");
            }
        }
    }

    top.into_sorted()
}

fn add_column(
    top: &mut TopK,
    seen: &mut HashSet<String>,
    ctx: &CompletionContext,
    col: &crate::db::connection::ColumnInfo,
) {
    if !seen.insert(col.name.to_ascii_uppercase()) {
        return;
    }
    let Some(score) = match_score(&col.name, &ctx.prefix, ctx.prefix_quoted) else {
        return;
    };
    top.push(
        score - 2,
        CompletionItem {
            label: col.name.clone(),
            insert_text: insert_name(ctx.dialect, &col.name, ctx.prefix_quoted, &ctx.prefix),
            kind: ItemKind::Column,
            detail: format!(
                "{} {}",
                if col.data_type.is_empty() {
                    "?"
                } else {
                    col.data_type.as_str()
                },
                if col.nullable { "NULL" } else { "NOT NULL" }
            ),
        },
    );
}

fn add_object(
    top: &mut TopK,
    ctx: &CompletionContext,
    obj: &MetaObject,
    annotate_schema: bool,
    current_schema: &str,
) {
    let Some(score) = match_score(&obj.name, &ctx.prefix, ctx.prefix_quoted) else {
        return;
    };
    let is_current = !current_schema.is_empty() && obj.schema.eq_ignore_ascii_case(current_schema);
    let kind_boost = if obj.kind.is_relation() { 0 } else { 3 };
    let detail = if annotate_schema || !is_current {
        obj.schema.clone()
    } else {
        String::new()
    };
    top.push(
        score + kind_boost + if is_current { -1 } else { 0 },
        CompletionItem {
            label: obj.name.clone(),
            insert_text: insert_name(ctx.dialect, &obj.name, ctx.prefix_quoted, &ctx.prefix),
            kind: ItemKind::from_meta(obj.kind),
            detail,
        },
    );
}

fn add_schema(top: &mut TopK, ctx: &CompletionContext, schema: &str) {
    let Some(score) = match_score(schema, &ctx.prefix, ctx.prefix_quoted) else {
        return;
    };
    top.push(
        score + 3,
        CompletionItem {
            label: schema.to_string(),
            insert_text: insert_name(ctx.dialect, schema, ctx.prefix_quoted, &ctx.prefix),
            kind: ItemKind::Schema,
            detail: "schema".into(),
        },
    );
}

fn add_word(top: &mut TopK, ctx: &CompletionContext, word: &str, detail: &str) {
    if word.is_empty() {
        return;
    }
    let Some(score) = match_score(word, &ctx.prefix, ctx.prefix_quoted) else {
        return;
    };
    top.push(
        score + 4,
        CompletionItem {
            label: word.to_string(),
            insert_text: insert_name(ctx.dialect, word, ctx.prefix_quoted, &ctx.prefix),
            kind: ItemKind::Word,
            detail: detail.to_string(),
        },
    );
}

fn add_static(top: &mut TopK, ctx: &CompletionContext, case_sensitive: bool) {
    let prefix = &ctx.prefix;
    if prefix.is_empty() {
        return;
    }
    for kw in sql_scan::KEYWORDS {
        if let Some(score) = match_score(kw, prefix, case_sensitive) {
            top.push(
                score + 5,
                CompletionItem {
                    label: (*kw).to_string(),
                    insert_text: (*kw).to_string(),
                    kind: ItemKind::Keyword,
                    detail: "keyword".into(),
                },
            );
        }
    }
    for func in functions_for(ctx.dialect) {
        if let Some(score) = match_score(func, prefix, case_sensitive) {
            top.push(
                score + 4,
                CompletionItem {
                    label: (*func).to_string(),
                    insert_text: (*func).to_string(),
                    kind: ItemKind::Function,
                    detail: "function".into(),
                },
            );
        }
    }
}

fn functions_for(dialect: DbType) -> &'static [&'static str] {
    match dialect {
        DbType::Oracle => &[
            "ABS",
            "ADD_MONTHS",
            "AVG",
            "CAST",
            "CEIL",
            "COALESCE",
            "COUNT",
            "CURRENT_DATE",
            "CURRENT_TIMESTAMP",
            "DECODE",
            "DENSE_RANK",
            "EXTRACT",
            "FLOOR",
            "GREATEST",
            "INITCAP",
            "INSTR",
            "LAG",
            "LAST_DAY",
            "LEAD",
            "LEAST",
            "LENGTH",
            "LISTAGG",
            "LOWER",
            "LPAD",
            "LTRIM",
            "MAX",
            "MIN",
            "MOD",
            "MONTHS_BETWEEN",
            "NEXT_DAY",
            "NULLIF",
            "NVL",
            "NVL2",
            "RANK",
            "REGEXP_LIKE",
            "REGEXP_REPLACE",
            "REPLACE",
            "ROUND",
            "ROW_NUMBER",
            "RPAD",
            "RTRIM",
            "SIGN",
            "SUBSTR",
            "SUM",
            "SYSDATE",
            "SYSTIMESTAMP",
            "TO_CHAR",
            "TO_DATE",
            "TO_NUMBER",
            "TO_TIMESTAMP",
            "TRIM",
            "TRUNC",
            "UPPER",
        ],
        DbType::Postgres => &[
            "ARRAY_AGG",
            "AVG",
            "CAST",
            "CEIL",
            "COALESCE",
            "COUNT",
            "CURRENT_DATE",
            "CURRENT_TIMESTAMP",
            "DATE_TRUNC",
            "DENSE_RANK",
            "EXTRACT",
            "FLOOR",
            "GENERATE_SERIES",
            "GREATEST",
            "JSONB_BUILD_OBJECT",
            "LAG",
            "LEAD",
            "LEAST",
            "LENGTH",
            "LOWER",
            "LPAD",
            "LTRIM",
            "MAX",
            "MIN",
            "NOW",
            "NULLIF",
            "RANK",
            "REGEXP_REPLACE",
            "REPLACE",
            "ROUND",
            "ROW_NUMBER",
            "RPAD",
            "RTRIM",
            "STRING_AGG",
            "SUBSTRING",
            "SUM",
            "TO_CHAR",
            "TO_DATE",
            "TO_NUMBER",
            "TRIM",
            "UPPER",
        ],
    }
}

pub fn find_ref<'a>(ctx: &'a CompletionContext, name: &str, quoted: bool) -> Option<&'a TableRef> {
    let eq = |a: &str, b: &str| {
        if quoted {
            a == b
        } else {
            a.eq_ignore_ascii_case(b)
        }
    };
    ctx.refs
        .iter()
        .find(|r| r.alias.as_deref().map(|a| eq(a, name)).unwrap_or(false))
        .or_else(|| {
            ctx.refs
                .iter()
                .find(|r| r.schema.is_none() && eq(&r.name, name))
        })
        .or_else(|| ctx.refs.iter().find(|r| eq(&r.name, name)))
}

// ---------------------------------------------------------------------------
// Matching / quoting
// ---------------------------------------------------------------------------

/// Match quality: 0 exact, 1 prefix, 2 word-segment prefix, 3 substring,
/// 4 subsequence. `None` = no match.
pub fn match_score(name: &str, prefix: &str, case_sensitive: bool) -> Option<i32> {
    if prefix.is_empty() {
        return Some(0);
    }
    if case_sensitive {
        if name == prefix {
            return Some(0);
        }
        if name.starts_with(prefix) {
            return Some(1);
        }
        if segment_prefix(name, prefix, true) {
            return Some(2);
        }
        if name.contains(prefix) {
            return Some(3);
        }
        if prefix.chars().count() >= 2 && subsequence(name, prefix, true) {
            return Some(4);
        }
        return None;
    }

    if name.eq_ignore_ascii_case(prefix) {
        return Some(0);
    }
    if starts_with_ci(name, prefix) {
        return Some(1);
    }
    if segment_prefix(name, prefix, false) {
        return Some(2);
    }
    if contains_ci(name, prefix) {
        return Some(3);
    }
    if prefix.chars().count() >= 2 && subsequence(name, prefix, false) {
        return Some(4);
    }
    None
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len())
        .map(|head| head.eq_ignore_ascii_case(prefix))
        .unwrap_or(false)
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() {
        return true;
    }
    if n.len() > h.len() {
        return false;
    }
    (0..=h.len() - n.len()).any(|i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

fn segment_prefix(name: &str, prefix: &str, case_sensitive: bool) -> bool {
    let mut prev: Option<char> = None;
    for (i, c) in name.char_indices() {
        let boundary = i > 0
            && (prev == Some('_')
                || (prev.map(|p| p.is_lowercase()).unwrap_or(false) && c.is_uppercase()));
        if boundary {
            let rest = &name[i..];
            let matched = if case_sensitive {
                rest.starts_with(prefix)
            } else {
                starts_with_ci(rest, prefix)
            };
            if matched {
                return true;
            }
        }
        prev = Some(c);
    }
    false
}

fn subsequence(name: &str, prefix: &str, case_sensitive: bool) -> bool {
    let mut want = prefix.chars();
    let mut target = want.next();
    for c in name.chars() {
        let Some(t) = target else {
            return true;
        };
        let hit = if case_sensitive {
            c == t
        } else {
            c.eq_ignore_ascii_case(&t)
        };
        if hit {
            target = want.next();
        }
    }
    target.is_none()
}

/// Insert text for a catalog name: quote when required, keep the user's case
/// style for unquoted Oracle names, never double-quote a plain name in PG.
pub fn insert_name(dialect: DbType, name: &str, force_quote: bool, prefix: &str) -> String {
    let needs_quote = force_quote
        || match dialect {
            DbType::Oracle => {
                let first_ok = name
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_alphabetic())
                    .unwrap_or(false);
                !first_ok
                    || !name.chars().all(|c| {
                        c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, '_' | '$' | '#')
                    })
            }
            DbType::Postgres => {
                let first_ok = name
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_lowercase() || c == '_')
                    .unwrap_or(false);
                !first_ok
                    || !name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            }
        };

    if needs_quote {
        return format!("\"{}\"", name.replace('"', "\"\""));
    }

    let typed_lower = !prefix.is_empty() && prefix.chars().all(|c| c.is_lowercase());
    let name_upper = name.chars().all(|c| c.is_uppercase() || !c.is_alphabetic());
    if dialect == DbType::Oracle && typed_lower && name_upper {
        name.to_lowercase()
    } else {
        name.to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connection::ColumnInfo;

    fn store_with_emp() -> MetadataStore {
        let mut store = MetadataStore::default();
        store.set_dialect(DbType::Oracle);
        store.current_schema = "SCOTT".into();
        store.put_schemas(vec!["SCOTT".into(), "HR".into()]);
        store.put_objects(
            "SCOTT",
            vec![
                MetaObject {
                    schema: "SCOTT".into(),
                    name: "EMP".into(),
                    kind: MetaObjectKind::Table,
                },
                MetaObject {
                    schema: "SCOTT".into(),
                    name: "DEPT".into(),
                    kind: MetaObjectKind::Table,
                },
                MetaObject {
                    schema: "SCOTT".into(),
                    name: "EMP_V".into(),
                    kind: MetaObjectKind::View,
                },
            ],
        );
        store.put_columns(
            "SCOTT",
            "EMP",
            vec![
                ColumnInfo {
                    name: "EMPNO".into(),
                    data_type: "NUMBER".into(),
                    nullable: false,
                },
                ColumnInfo {
                    name: "ENAME".into(),
                    data_type: "VARCHAR2(10)".into(),
                    nullable: true,
                },
            ],
        );
        store
    }

    fn analyze_at(text: &str) -> CompletionContext {
        analyze(text, text.len(), DbType::Oracle)
    }

    #[test]
    fn alias_dot_lists_columns() {
        let text = "SELECT * FROM emp e WHERE e.";
        let store = store_with_emp();
        // Current schema in the context is empty; emulate the real store.
        let ctx = analyze_at(text);
        assert_eq!(ctx.parts.len(), 1);
        assert_eq!(ctx.parts[0].text, "e");
        assert_eq!(ctx.refs.len(), 1);
        assert_eq!(ctx.refs[0].name, "emp");
        assert_eq!(ctx.refs[0].alias.as_deref(), Some("e"));

        let items = build_items(&ctx, &store);
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"EMPNO"));
        assert!(labels.contains(&"ENAME"));
        assert!(!labels.contains(&"DEPT"));
    }

    #[test]
    fn qualified_partial_filters_columns() {
        let text = "SELECT e.em FROM emp e";
        let store = store_with_emp();
        let ctx = analyze_at(&text[..text.find(" FROM").unwrap_or(text.len())]);
        let _ = ctx;
        // Cursor after "e.em".
        let text2 = "SELECT * FROM emp e WHERE e.em";
        let ctx2 = analyze(text2, text2.len(), DbType::Oracle);
        let items = build_items(&ctx2, &store);
        assert_eq!(items[0].label, "EMPNO");
    }

    #[test]
    fn from_lists_tables_and_schemas() {
        let store = store_with_emp();
        let ctx = analyze_at("SELECT * FROM ");
        let items = build_items(&ctx, &store);
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"EMP"));
        assert!(labels.contains(&"SCOTT"));
    }

    #[test]
    fn schema_dot_lists_objects() {
        let store = store_with_emp();
        let ctx = analyze_at("SELECT * FROM hr.");
        let items = build_items(&ctx, &store);
        // HR objects are not loaded yet: no tables, but HR itself is known.
        assert!(items.is_empty() || items.iter().all(|i| i.kind != ItemKind::Table));
    }

    #[test]
    fn keywords_are_suggested_unqualified() {
        let store = store_with_emp();
        let ctx = analyze_at("SEL");
        let items = build_items(&ctx, &store);
        assert!(items.iter().any(|i| i.label == "SELECT"));
    }

    #[test]
    fn strings_disable_completion() {
        let ctx = analyze_at("SELECT 'abc");
        assert!(ctx.in_literal);
    }

    #[test]
    fn oracle_case_style_preserved() {
        assert_eq!(insert_name(DbType::Oracle, "EMP", false, "em"), "emp");
        assert_eq!(insert_name(DbType::Oracle, "EMP", false, "EM"), "EMP");
        assert_eq!(
            insert_name(DbType::Oracle, "Weird Name", false, ""),
            "\"Weird Name\""
        );
    }

    #[test]
    fn match_scores() {
        assert_eq!(match_score("EMP", "EM", false), Some(1));
        assert_eq!(match_score("EMP_MASTER", "MA", false), Some(2));
        assert_eq!(match_score("MY_EMP", "EMP", false), Some(2));
        assert_eq!(match_score("MYXEMP", "EMP", false), Some(3));
        assert_eq!(match_score("EMPLOYEE_ID", "EMPID", false), Some(4));
        assert_eq!(match_score("EMPLOYEE_ID", "ZZ", false), None);
    }

    #[test]
    fn cte_names_are_parsed() {
        let ctx = analyze_at("WITH recent AS (SELECT * FROM emp) SELECT * FROM ");
        assert!(ctx.ctes.iter().any(|c| c == "recent"));
    }

    fn item(label: &str) -> CompletionItem {
        CompletionItem {
            label: label.into(),
            insert_text: label.into(),
            kind: ItemKind::Column,
            detail: String::new(),
        }
    }

    #[test]
    fn popup_selection_clamps_and_scrolls() {
        let mut popup = CompletionPopup::new(
            vec![item("A"), item("B"), item("C"), item("D")],
            0,
            0,
            0,
            false,
        );
        assert_eq!(popup.selected_item().map(|i| i.label.as_str()), Some("A"));
        popup.move_selection(-1);
        assert_eq!(popup.selected, 0);
        popup.move_selection(10);
        assert_eq!(popup.selected, 3);
        assert_eq!(popup.scroll_offset(2), 2);
        assert_eq!(popup.selected_item().map(|i| i.label.as_str()), Some("D"));
    }
}
