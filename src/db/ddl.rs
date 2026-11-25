//! Toad-style `F4` Object Describe / DDL extraction and formatting.
//!
//! Resolves the database object under the SQL editor cursor (including
//! `schema.object`, `alias.column` -> base table, `table.column` -> table,
//! optimizer hints `/*+ INDEX(e idx_name) */`, and active text selections)
//! and formats clean, column-aligned DDL scripts.

use crate::db::completion;
use crate::db::connection::DbType;
use crate::db::sql_scan::{self, Token, TokenKind, WordPart};

/// What the user asked to describe with `F4`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdlTarget {
    pub schema: Option<String>,
    pub schema_quoted: bool,
    pub name: String,
    pub name_quoted: bool,
    /// Optional known object kind (set when `F4` is pressed inside the `F12`
    /// DB explorer, `None` when resolved from the SQL editor).
    pub kind_hint: Option<String>,
}

impl DdlTarget {
    pub fn display_label(&self) -> String {
        match &self.schema {
            Some(s) => format!("{}.{}", s, self.name),
            None => self.name.clone(),
        }
    }
}

/// Resolved object metadata + generated DDL returned by a backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescribedObject {
    pub schema: String,
    pub name: String,
    pub kind: String,
    pub ddl: String,
    pub summary: Option<String>,
    pub elapsed_ms: u64,
}

/// Rich column definition for `CREATE TABLE` generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RichColumn {
    pub name: String,
    pub full_type: String,
    pub default_expr: Option<String>,
    pub nullable: bool,
}

/// Constraint clause inside `CREATE TABLE` (`PRIMARY KEY`, `UNIQUE`, `FOREIGN KEY`, `CHECK`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableConstraintDdl {
    pub name: Option<String>,
    pub definition: String,
}

/// Extract the `[schema.]object` target from an active selection or from the
/// SQL token under/adjacent to `cursor_offset` (a byte offset into `text`).
pub fn extract_ddl_target(
    text: &str,
    cursor_offset: usize,
    selected_text: Option<&str>,
    dialect: DbType,
) -> Option<DdlTarget> {
    if let Some(sel) = selected_text {
        let trimmed = sel.trim();
        if !trimmed.is_empty() && !trimmed.contains('\n') {
            if let Some(target) = parse_standalone_target(trimmed) {
                return Some(target);
            }
        }
    }

    if text.trim().is_empty() {
        return None;
    }
    let cursor = cursor_offset.min(text.len());
    let ranges = sql_scan::statement_ranges(text);
    let (stmt_start, stmt_end) = range_containing(&ranges, cursor, text.len());
    let stmt = &text[stmt_start..stmt_end];
    let rel_cursor = cursor.saturating_sub(stmt_start).min(stmt.len());

    let tokens = sql_scan::tokenize(stmt);

    // Optimizer hints (`/*+ INDEX(e idx_emp) */` or `--+ ...`): re-scan the
    // comment body so pressing F4 on an index name inside a hint works too.
    for tok in &tokens {
        if tok.kind == TokenKind::Comment && rel_cursor >= tok.start && rel_cursor <= tok.end {
            if let Some(target) = extract_from_comment(stmt, tok, rel_cursor, cursor, text, dialect)
            {
                return Some(target);
            }
            return None;
        }
        if tok.kind == TokenKind::Str && rel_cursor > tok.start && rel_cursor < tok.end {
            return None;
        }
    }

    let parts = chain_at_cursor(&tokens, rel_cursor)?;
    let ctx = completion::analyze(text, cursor, dialect);
    Some(resolve_parts_with_context(&parts, &ctx))
}

fn parse_standalone_target(snippet: &str) -> Option<DdlTarget> {
    let toks = sql_scan::tokenize(snippet);
    if toks.is_empty() {
        return None;
    }
    let chains = collect_chains(&toks);
    if chains.len() != 1 {
        return None;
    }
    let chain = &chains[0];
    // Reject if there are stray tokens outside the single qualified identifier.
    if chain.first_tok != 0 || chain.last_tok + 1 != toks.len() {
        return None;
    }
    parts_to_direct_target(&chain.parts)
}

fn extract_from_comment(
    stmt: &str,
    comment_tok: &Token,
    rel_cursor: usize,
    abs_cursor: usize,
    full_text: &str,
    dialect: DbType,
) -> Option<DdlTarget> {
    let raw = &stmt[comment_tok.start..comment_tok.end];
    let (header_len, body_end) = if raw.starts_with("/*+") {
        (3, raw.len().saturating_sub(2))
    } else if raw.starts_with("/*") {
        (2, raw.len().saturating_sub(2))
    } else if raw.starts_with("--+") {
        (3, raw.len())
    } else if raw.starts_with("--") {
        (2, raw.len())
    } else {
        return None;
    };
    if body_end <= header_len {
        return None;
    }
    let inner = &raw[header_len..body_end];
    let inner_start = comment_tok.start + header_len;
    if rel_cursor < inner_start || rel_cursor > comment_tok.start + body_end {
        return None;
    }
    let inner_cursor = rel_cursor - inner_start;
    let inner_toks = sql_scan::tokenize(inner);
    let parts = chain_at_cursor(&inner_toks, inner_cursor)?;
    let ctx = completion::analyze(full_text, abs_cursor, dialect);
    // In `/*+ INDEX(e idx_emp) */`, `e` is the table alias; `idx_emp` is the index.
    Some(resolve_parts_with_context(&parts, &ctx))
}

struct IdentChain {
    parts: Vec<WordPart>,
    start: usize,
    end: usize,
    first_tok: usize,
    last_tok: usize,
}

fn token_as_part(tok: &Token, allow_keyword: bool) -> Option<WordPart> {
    match tok.kind {
        TokenKind::Ident { quoted } => Some(WordPart {
            text: tok.text.clone(),
            quoted,
        }),
        TokenKind::Keyword if allow_keyword => Some(WordPart {
            text: tok.text.clone(),
            quoted: false,
        }),
        _ => None,
    }
}

fn collect_chains(tokens: &[Token]) -> Vec<IdentChain> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < tokens.len() {
        // A chain can start with an identifier, or with a keyword only if
        // immediately followed by `.` (e.g. a schema named `user` or `comment`).
        let followed_by_dot = tokens.get(i + 1).is_some_and(|t| t.is_punct("."));
        let Some(first) = token_as_part(&tokens[i], followed_by_dot) else {
            i += 1;
            continue;
        };
        let first_tok = i;
        let start = tokens[i].start;
        let mut end = tokens[i].end;
        let mut last_tok = i;
        let mut parts = vec![first];

        while tokens.get(last_tok + 1).is_some_and(|t| t.is_punct(".")) {
            let dot_idx = last_tok + 1;
            if let Some(next_tok) = tokens.get(dot_idx + 1) {
                if let Some(part) = token_as_part(next_tok, true) {
                    parts.push(part);
                    end = next_tok.end;
                    last_tok = dot_idx + 1;
                    continue;
                }
            }
            break;
        }

        out.push(IdentChain {
            parts,
            start,
            end,
            first_tok,
            last_tok,
        });
        i = last_tok + 1;
    }
    out
}

fn chain_at_cursor(tokens: &[Token], rel_cursor: usize) -> Option<Vec<WordPart>> {
    let chains = collect_chains(tokens);
    // Prefer a chain where the cursor is strictly inside or at the end/start.
    for chain in &chains {
        if rel_cursor >= chain.start && rel_cursor <= chain.end {
            return Some(chain.parts.clone());
        }
    }
    None
}

fn parts_to_direct_target(parts: &[WordPart]) -> Option<DdlTarget> {
    match parts.len() {
        0 => None,
        1 => Some(DdlTarget {
            schema: None,
            schema_quoted: false,
            name: parts[0].text.clone(),
            name_quoted: parts[0].quoted,
            kind_hint: None,
        }),
        2 => Some(DdlTarget {
            schema: Some(parts[0].text.clone()),
            schema_quoted: parts[0].quoted,
            name: parts[1].text.clone(),
            name_quoted: parts[1].quoted,
            kind_hint: None,
        }),
        _ => Some(DdlTarget {
            schema: Some(parts[0].text.clone()),
            schema_quoted: parts[0].quoted,
            name: parts[1].text.clone(),
            name_quoted: parts[1].quoted,
            kind_hint: None,
        }),
    }
}

fn eq_ident(a: &str, b: &str, quoted: bool) -> bool {
    if quoted {
        a == b
    } else {
        a.eq_ignore_ascii_case(b)
    }
}

fn resolve_parts_with_context(
    parts: &[WordPart],
    ctx: &completion::CompletionContext,
) -> DdlTarget {
    match parts.len() {
        1 => {
            let p0 = &parts[0];
            // 1. Table alias in current statement (`WHERE e.id = 1` with cursor on `e`).
            if let Some(r) = ctx.refs.iter().find(|r| {
                r.alias
                    .as_deref()
                    .is_some_and(|a| eq_ident(a, &p0.text, p0.quoted))
            }) {
                return DdlTarget {
                    schema: r.schema.clone(),
                    schema_quoted: r.schema_quoted,
                    name: r.name.clone(),
                    name_quoted: r.quoted,
                    kind_hint: None,
                };
            }
            // 2. Unqualified reference to a schema-qualified table in FROM.
            if let Some(r) = ctx
                .refs
                .iter()
                .find(|r| r.schema.is_some() && eq_ident(&r.name, &p0.text, p0.quoted))
            {
                return DdlTarget {
                    schema: r.schema.clone(),
                    schema_quoted: r.schema_quoted,
                    name: r.name.clone(),
                    name_quoted: r.quoted,
                    kind_hint: None,
                };
            }
            DdlTarget {
                schema: None,
                schema_quoted: false,
                name: p0.text.clone(),
                name_quoted: p0.quoted,
                kind_hint: None,
            }
        }
        2 => {
            let (p0, p1) = (&parts[0], &parts[1]);
            // 1. Exact `schema.table` match in FROM/JOIN.
            if let Some(r) = ctx.refs.iter().find(|r| {
                r.schema
                    .as_deref()
                    .is_some_and(|s| eq_ident(s, &p0.text, p0.quoted))
                    && eq_ident(&r.name, &p1.text, p1.quoted)
            }) {
                return DdlTarget {
                    schema: r.schema.clone(),
                    schema_quoted: r.schema_quoted,
                    name: r.name.clone(),
                    name_quoted: r.quoted,
                    kind_hint: None,
                };
            }
            // 2. `alias.column` -> resolve alias to its base relation!
            if let Some(r) = ctx.refs.iter().find(|r| {
                r.alias
                    .as_deref()
                    .is_some_and(|a| eq_ident(a, &p0.text, p0.quoted))
            }) {
                return DdlTarget {
                    schema: r.schema.clone(),
                    schema_quoted: r.schema_quoted,
                    name: r.name.clone(),
                    name_quoted: r.quoted,
                    kind_hint: None,
                };
            }
            // 3. `table.column` (unaliased table used as column qualifier).
            if let Some(r) = ctx.refs.iter().find(|r| {
                eq_ident(&r.name, &p0.text, p0.quoted)
                    && !r
                        .schema
                        .as_deref()
                        .is_some_and(|s| eq_ident(s, &p0.text, p0.quoted))
            }) {
                return DdlTarget {
                    schema: r.schema.clone(),
                    schema_quoted: r.schema_quoted,
                    name: r.name.clone(),
                    name_quoted: r.quoted,
                    kind_hint: None,
                };
            }
            // 4. Otherwise `schema.object` (or `package.member`, handled by backend).
            DdlTarget {
                schema: Some(p0.text.clone()),
                schema_quoted: p0.quoted,
                name: p1.text.clone(),
                name_quoted: p1.quoted,
                kind_hint: None,
            }
        }
        _ => {
            // `schema.table.column` or `schema.package.procedure` -> `schema.table/package`.
            DdlTarget {
                schema: Some(parts[0].text.clone()),
                schema_quoted: parts[0].quoted,
                name: parts[1].text.clone(),
                name_quoted: parts[1].quoted,
                kind_hint: None,
            }
        }
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

/// Format an Oracle column data type with length / precision / scale.
pub fn format_oracle_col_type(
    data_type: &str,
    data_length: Option<i64>,
    char_length: Option<i64>,
    char_used: Option<&str>,
    precision: Option<i64>,
    scale: Option<i64>,
) -> String {
    let upper = data_type.trim().to_ascii_uppercase();
    match upper.as_str() {
        "VARCHAR2" | "CHAR" => {
            let len = char_length.filter(|&n| n > 0).or(data_length.filter(|&n| n > 0));
            match len {
                Some(n) if char_used.is_some_and(|u| u.eq_ignore_ascii_case("C")) => {
                    format!("{}({} CHAR)", upper, n)
                }
                Some(n) => format!("{}({})", upper, n),
                None => upper,
            }
        }
        "NVARCHAR2" | "NCHAR" => {
            let len = char_length.filter(|&n| n > 0).or(data_length.filter(|&n| n > 0));
            match len {
                Some(n) => format!("{}({})", upper, n),
                None => upper,
            }
        }
        "RAW" => match data_length.filter(|&n| n > 0) {
            Some(n) => format!("RAW({})", n),
            None => "RAW".to_string(),
        },
        "NUMBER" => match (precision, scale) {
            (Some(p), Some(s)) if s != 0 => format!("NUMBER({}, {})", p, s),
            (Some(p), _) => format!("NUMBER({})", p),
            (None, Some(0)) => "INTEGER".to_string(),
            (None, Some(s)) => format!("NUMBER(*, {})", s),
            (None, None) => "NUMBER".to_string(),
        },
        "FLOAT" => match precision {
            Some(p) if p > 0 && p != 126 => format!("FLOAT({})", p),
            _ => "FLOAT".to_string(),
        },
        "" => "?".to_string(),
        _ => data_type.trim().to_string(),
    }
}

/// Assemble a clean, Toad-style `CREATE TABLE` script with aligned columns,
/// inline constraints, optional partition key, and trailing index/comment DDL.
pub fn format_rich_create_table(
    table_keyword: &str,
    qualified: &str,
    cols: &[RichColumn],
    constraints: &[TableConstraintDdl],
    partition_clause: Option<&str>,
    extra_statements: &[String],
    quote: impl Fn(&str) -> String,
) -> String {
    let quoted_names: Vec<String> = cols.iter().map(|c| quote(&c.name)).collect();
    let name_width = quoted_names
        .iter()
        .map(|n| n.len())
        .max()
        .unwrap_or(0)
        .min(36);

    let total_items = cols.len() + constraints.len();
    let mut out = format!("CREATE {} {} (\n", table_keyword, qualified);

    for (i, (c, qname)) in cols.iter().zip(quoted_names.iter()).enumerate() {
        let pad = if qname.len() < name_width {
            " ".repeat(name_width - qname.len())
        } else {
            String::new()
        };
        let dtype = if c.full_type.is_empty() {
            "?"
        } else {
            c.full_type.as_str()
        };
        let def_part = match c.default_expr.as_deref().map(str::trim) {
            Some(d) if !d.is_empty() => format!(" DEFAULT {}", d),
            _ => String::new(),
        };
        let null_part = if c.nullable { "" } else { " NOT NULL" };
        let comma = if i + 1 < total_items { "," } else { "" };
        out.push_str(&format!(
            "  {}{}  {}{}{}{}\n",
            qname, pad, dtype, def_part, null_part, comma
        ));
    }

    for (j, con) in constraints.iter().enumerate() {
        let idx = cols.len() + j;
        let comma = if idx + 1 < total_items { "," } else { "" };
        match con.name.as_deref().filter(|n| !n.trim().is_empty()) {
            Some(name) => out.push_str(&format!(
                "  CONSTRAINT {} {}{}\n",
                quote(name),
                con.definition,
                comma
            )),
            None => out.push_str(&format!("  {}{}\n", con.definition, comma)),
        }
    }

    out.push(')');
    if let Some(part) = partition_clause.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str("\nPARTITION BY ");
        out.push_str(part);
    }
    out.push(';');

    if !extra_statements.is_empty() {
        out.push_str("\n\n");
        for (i, stmt) in extra_statements.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            let trimmed = stmt.trim();
            out.push_str(trimmed);
            if !trimmed.ends_with(';') && !trimmed.starts_with("--") {
                out.push(';');
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_bare_and_schema_qualified_objects() {
        let sql = "SELECT * FROM emp WHERE deptno = 10;";
        let at_emp = sql.find("emp").unwrap() + 1;
        let t = extract_ddl_target(sql, at_emp, None, DbType::Oracle).unwrap();
        assert_eq!(t.schema, None);
        assert_eq!(t.name, "emp");

        // Cursor right at the end of `emp` (`emp|`) also resolves `emp`.
        let at_emp_end = sql.find("emp").unwrap() + 3;
        let t_end = extract_ddl_target(sql, at_emp_end, None, DbType::Oracle).unwrap();
        assert_eq!(t_end.name, "emp");

        let sql2 = "SELECT * FROM hr.employees;";
        for off in sql2.find("hr").unwrap()..=sql2.find("employees").unwrap() + 9 {
            let t2 = extract_ddl_target(sql2, off, None, DbType::Oracle).unwrap();
            assert_eq!(t2.schema.as_deref(), Some("hr"));
            assert_eq!(t2.name, "employees");
        }
    }

    #[test]
    fn resolves_alias_and_column_references_to_base_table() {
        let sql = "SELECT e.salary, e.first_name FROM hr.employees e WHERE e.department_id = 10";
        // Cursor on `e.salary` -> resolves to `hr.employees`.
        let on_col = sql.find("salary").unwrap() + 2;
        let t = extract_ddl_target(sql, on_col, None, DbType::Oracle).unwrap();
        assert_eq!(t.schema.as_deref(), Some("hr"));
        assert_eq!(t.name, "employees");

        // Cursor on `e` in `WHERE e.department_id` -> resolves to `hr.employees`.
        let on_alias = sql.rfind("e.department_id").unwrap();
        let t2 = extract_ddl_target(sql, on_alias, None, DbType::Oracle).unwrap();
        assert_eq!(t2.schema.as_deref(), Some("hr"));
        assert_eq!(t2.name, "employees");
    }

    #[test]
    fn extracts_index_name_from_ddl_and_optimizer_hints() {
        let ddl = "ALTER INDEX scott.idx_emp_dept REBUILD;";
        let off = ddl.find("idx_emp_dept").unwrap() + 4;
        let t = extract_ddl_target(ddl, off, None, DbType::Oracle).unwrap();
        assert_eq!(t.schema.as_deref(), Some("scott"));
        assert_eq!(t.name, "idx_emp_dept");

        // Inside an Oracle optimizer hint `/*+ INDEX(e idx_emp_dept) */`.
        let hinted = "SELECT /*+ INDEX(e idx_emp_dept) */ * FROM emp e";
        let hint_off = hinted.find("idx_emp_dept").unwrap() + 3;
        let th = extract_ddl_target(hinted, hint_off, None, DbType::Oracle).unwrap();
        assert_eq!(th.schema, None);
        assert_eq!(th.name, "idx_emp_dept");
    }

    #[test]
    fn ignores_standalone_keywords_and_string_literals() {
        let sql = "SELECT 'not_an_object' FROM emp";
        assert!(extract_ddl_target(sql, 2, None, DbType::Oracle).is_none());
        let in_str = sql.find("not_an_object").unwrap() + 3;
        assert!(extract_ddl_target(sql, in_str, None, DbType::Oracle).is_none());
    }

    #[test]
    fn prefers_active_single_identifier_selection() {
        let sql = "SELECT * FROM emp";
        let t = extract_ddl_target(sql, 0, Some("hr.idx_emp_name"), DbType::Oracle).unwrap();
        assert_eq!(t.schema.as_deref(), Some("hr"));
        assert_eq!(t.name, "idx_emp_name");
    }

    #[test]
    fn formats_oracle_types_and_rich_table_ddl() {
        assert_eq!(
            format_oracle_col_type("VARCHAR2", Some(100), Some(50), Some("C"), None, None),
            "VARCHAR2(50 CHAR)"
        );
        assert_eq!(
            format_oracle_col_type("NUMBER", Some(22), None, None, Some(10), Some(2)),
            "NUMBER(10, 2)"
        );
        assert_eq!(
            format_oracle_col_type("NUMBER", Some(22), None, None, Some(9), Some(0)),
            "NUMBER(9)"
        );
        assert_eq!(
            format_oracle_col_type("NUMBER", Some(22), None, None, None, None),
            "NUMBER"
        );

        let cols = vec![
            RichColumn {
                name: "EMPNO".into(),
                full_type: "NUMBER(4)".into(),
                default_expr: None,
                nullable: false,
            },
            RichColumn {
                name: "SAL".into(),
                full_type: "NUMBER(7, 2)".into(),
                default_expr: Some("0".into()),
                nullable: true,
            },
        ];
        let cons = vec![TableConstraintDdl {
            name: Some("PK_EMP".into()),
            definition: "PRIMARY KEY (EMPNO)".into(),
        }];
        let extra = vec![
            "-- Indexes".to_string(),
            "CREATE INDEX SCOTT.IDX_EMP_SAL ON SCOTT.EMP (SAL);".to_string(),
        ];
        let ddl = format_rich_create_table("TABLE", "SCOTT.EMP", &cols, &cons, None, &extra, |s| {
            s.to_string()
        });
        assert!(ddl.contains("EMPNO  NUMBER(4) NOT NULL,"));
        assert!(ddl.contains("SAL    NUMBER(7, 2) DEFAULT 0,"));
        assert!(ddl.contains("CONSTRAINT PK_EMP PRIMARY KEY (EMPNO)"));
        assert!(ddl.contains("CREATE INDEX SCOTT.IDX_EMP_SAL"));
    }
}
