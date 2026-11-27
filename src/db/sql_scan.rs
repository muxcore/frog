//! SQL scanning helpers shared by statement splitting and completion.
//!
//! The completion engine needs two things: the byte range of the statement
//! under the cursor, and a token stream with positions for that statement.
//! Both live here so quoting/comment rules can never diverge from the
//! statement splitter used by execution.

/// What a token is. `text` always holds the semantic value: identifiers are
/// unquoted (`"Emp"` -> `Emp`), strings/comments keep their content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    Ident {
        quoted: bool,
    },
    /// An identifier that appears in [`KEYWORDS`].
    Keyword,
    Number,
    Str,
    Comment,
    Punct,
    Op,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub text: String,
    pub start: usize,
    pub end: usize,
}

impl Token {
    pub fn is_ident(&self) -> bool {
        matches!(self.kind, TokenKind::Ident { .. })
    }

    pub fn is_quoted(&self) -> bool {
        matches!(self.kind, TokenKind::Ident { quoted: true })
    }

    pub fn is_punct(&self, s: &str) -> bool {
        self.kind == TokenKind::Punct && self.text == s
    }

    pub fn is_keyword(&self, word: &str) -> bool {
        self.kind == TokenKind::Keyword && self.text.eq_ignore_ascii_case(word)
    }
}

/// Clause keywords that matter for completion. Kept sorted for binary_search.
pub const KEYWORDS: &[&str] = &[
    "ADD",
    "ALL",
    "ALTER",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASE",
    "CHECK",
    "COLUMN",
    "COMMENT",
    "COMMIT",
    "CONNECT",
    "CREATE",
    "CROSS",
    "CURRENT",
    "DECLARE",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "FETCH",
    "FOR",
    "FOREIGN",
    "FROM",
    "FULL",
    "GRANT",
    "GROUP",
    "HAVING",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LEFT",
    "LIKE",
    "MERGE",
    "MINUS",
    "NATURAL",
    "NOT",
    "NULL",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "PRIMARY",
    "PROCEDURE",
    "REFERENCES",
    "RETURNING",
    "REVOKE",
    "RIGHT",
    "ROLLBACK",
    "ROWNUM",
    "SELECT",
    "SET",
    "START",
    "SYNONYM",
    "TABLE",
    "THEN",
    "TO",
    "TRIGGER",
    "TRUNCATE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WITH",
];

fn is_keyword_word(s: &str) -> bool {
    KEYWORDS.binary_search_by(|k| k.cmp(&s)).is_ok()
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || matches!(c, '_' | '$' | '#')
}

fn is_ident_cont(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '$' | '#')
}

fn char_at(text: &str, i: usize) -> Option<(char, usize)> {
    text.get(i..)?.chars().next().map(|c| (c, c.len_utf8()))
}

/// Oracle alternative quoting: `q'[text]'`, `q'{...}'`, `q'!...!'`, ...
/// Returns the index just past the closing quote when `text[i..]` starts one.
fn scan_q_quote(bytes: &[u8], i: usize) -> Option<usize> {
    if i + 2 >= bytes.len() {
        return None;
    }
    if !(bytes[i] == b'q' || bytes[i] == b'Q') || bytes[i + 1] != b'\'' {
        return None;
    }
    let delimiter = bytes[i + 2];
    let close = match delimiter {
        b'[' => b']',
        b'{' => b'}',
        b'(' => b')',
        b'<' => b'>',
        other => other,
    };
    let mut j = i + 3;
    while j < bytes.len() {
        if bytes[j] == close && j + 1 < bytes.len() && bytes[j + 1] == b'\'' {
            return Some(j + 2);
        }
        j += 1;
    }
    Some(bytes.len())
}

/// Postgres escaped string literal: `E'...'` or `e'...'` where `\` escapes the
/// next character (e.g. `\'` or `\\`) in addition to `''`. Caller checks that
/// `i` is not preceded by an identifier character.
fn scan_e_quote(bytes: &[u8], i: usize) -> Option<usize> {
    if i + 1 >= bytes.len() {
        return None;
    }
    if !(bytes[i] == b'e' || bytes[i] == b'E') || bytes[i + 1] != b'\'' {
        return None;
    }
    let mut j = i + 2;
    while j < bytes.len() {
        if bytes[j] == b'\\' {
            j = (j + 2).min(bytes.len());
            continue;
        }
        if bytes[j] == b'\'' {
            if j + 1 < bytes.len() && bytes[j + 1] == b'\'' {
                j += 2;
                continue;
            }
            return Some(j + 1);
        }
        j += 1;
    }
    Some(bytes.len())
}

/// Postgres dollar-quoted string literal: `$$...$$` or `$tag$...$tag$`.
/// Tag must be empty or start with an ASCII letter / `_` (never a digit, so
/// `$1` parameters are untouched). Caller checks that `i` is not preceded by
/// an identifier character (so Oracle names like `v$session` or `BIN$x$0`
/// never match).
fn scan_dollar_quote(bytes: &[u8], i: usize) -> Option<usize> {
    if i + 1 >= bytes.len() || bytes[i] != b'$' {
        return None;
    }
    let mut tag_end = i + 1;
    if tag_end < bytes.len() && bytes[tag_end] != b'$' {
        let first = bytes[tag_end];
        if !(first.is_ascii_alphabetic() || first == b'_') {
            return None;
        }
        tag_end += 1;
        while tag_end < bytes.len()
            && (bytes[tag_end].is_ascii_alphanumeric() || bytes[tag_end] == b'_')
        {
            tag_end += 1;
        }
    }
    if tag_end >= bytes.len() || bytes[tag_end] != b'$' {
        return None;
    }
    let delim = &bytes[i..=tag_end];
    let mut j = tag_end + 1;
    while j + delim.len() <= bytes.len() {
        if &bytes[j..j + delim.len()] == delim {
            return Some(j + delim.len());
        }
        j += 1;
    }
    Some(bytes.len())
}

/// Tokenize a piece of SQL. Whitespace is skipped. Never fails: unterminated
/// strings/comments simply run to the end of the input, which is exactly what
/// we need while the user is still typing.
pub fn tokenize(text: &str) -> Vec<Token> {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut out: Vec<Token> = Vec::new();
    let mut i = 0usize;

    while i < len {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;

        // Comments.
        if c == b'-' && i + 1 < len && bytes[i + 1] == b'-' {
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            out.push(tok(TokenKind::Comment, &text[start..i], start, i));
            continue;
        }
        if c == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(len);
            out.push(tok(TokenKind::Comment, &text[start..i], start, i));
            continue;
        }

        // Strings (with '' escaping, Oracle q'...', Postgres E'...' and $$...$$).
        let prev_is_ident = text[..start]
            .chars()
            .next_back()
            .map(is_ident_cont)
            .unwrap_or(false);
        if let Some(end) = if !prev_is_ident {
            scan_q_quote(bytes, i)
                .or_else(|| scan_e_quote(bytes, i))
                .or_else(|| scan_dollar_quote(bytes, i))
        } else {
            None
        } {
            i = end;
            out.push(tok(TokenKind::Str, &text[start..i], start, i));
            continue;
        }
        if c == b'\'' {
            i += 1;
            while i < len {
                if bytes[i] == b'\'' {
                    if i + 1 < len && bytes[i + 1] == b'\'' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push(tok(TokenKind::Str, &text[start..i], start, i));
            continue;
        }

        // Quoted identifiers: "" escapes a quote.
        if c == b'"' {
            i += 1;
            let mut value = String::new();
            while i < len {
                if bytes[i] == b'"' {
                    if i + 1 < len && bytes[i + 1] == b'"' {
                        value.push('"');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                if let Some((ch, l)) = char_at(text, i) {
                    value.push(ch);
                    i += l;
                } else {
                    break;
                }
            }
            out.push(Token {
                kind: TokenKind::Ident { quoted: true },
                text: value,
                start,
                end: i,
            });
            continue;
        }

        // Numbers.
        if c.is_ascii_digit() {
            i += 1;
            while i < len && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i + 1 < len && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit() {
                i += 1;
                while i < len && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            out.push(tok(TokenKind::Number, &text[start..i], start, i));
            continue;
        }

        // Identifiers / keywords.
        if let Some((ch, l)) = char_at(text, i) {
            if is_ident_start(ch) {
                i += l;
                while i < len {
                    if let Some((ch, l)) = char_at(text, i) {
                        if is_ident_cont(ch) {
                            i += l;
                            continue;
                        }
                    }
                    break;
                }
                let raw = &text[start..i];
                let upper = raw.to_uppercase();
                let kind = if is_keyword_word(&upper) {
                    TokenKind::Keyword
                } else {
                    TokenKind::Ident { quoted: false }
                };
                out.push(Token {
                    kind,
                    text: raw.to_string(),
                    start,
                    end: i,
                });
                continue;
            }
        }

        // Punctuation we care about.
        if matches!(c, b'.' | b',' | b';' | b'(' | b')' | b'[' | b']') {
            i += 1;
            out.push(tok(TokenKind::Punct, &text[start..i], start, i));
            continue;
        }

        // Anything else (operators, binds, ...). One char is enough.
        if let Some((_, l)) = char_at(text, i) {
            i += l;
        } else {
            i += 1;
        }
        out.push(tok(TokenKind::Op, &text[start..i], start, i));
    }

    out
}

fn tok(kind: TokenKind, text: &str, start: usize, end: usize) -> Token {
    Token {
        kind,
        text: text.to_string(),
        start,
        end,
    }
}

/// The word (possibly dot-qualified) immediately before the cursor on one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordPart {
    pub text: String,
    pub quoted: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WordContext {
    /// Byte offset where the replaceable word starts (on the line).
    pub start: usize,
    pub prefix: String,
    pub prefix_quoted: bool,
    /// Qualifiers before the last dot, outermost first.
    pub parts: Vec<WordPart>,
    /// True when the cursor sits in a string or comment: no completion.
    pub in_literal: bool,
}

/// Extract the identifier being typed at `cursor_col` (a byte column) of
/// `line`. Handles `e.`, `e.emp`, `sc.emp.e`, quoted identifiers and the
/// "cursor inside a string/comment" case.
pub fn word_before(line: &str, cursor_col: usize) -> WordContext {
    let cursor_col = cursor_col.min(line.len());
    let text = &line[..cursor_col];
    let tokens = tokenize(text);

    if let Some(last) = tokens.last() {
        if matches!(last.kind, TokenKind::Str | TokenKind::Comment) && last.end == text.len() {
            return WordContext {
                start: cursor_col,
                in_literal: true,
                ..WordContext::default()
            };
        }
    }

    // The token ending exactly at the cursor is the (possibly partial) word.
    let mut prefix = String::new();
    let mut prefix_quoted = false;
    let mut start = cursor_col;
    let mut pos = tokens.len(); // index one past the prefix token

    if let Some(last) = tokens.last() {
        if last.is_ident() && last.end == text.len() {
            prefix = last.text.clone();
            prefix_quoted = last.is_quoted();
            start = last.start;
            pos = tokens.len() - 1;
        }
    }

    let mut parts: Vec<WordPart> = Vec::new();
    loop {
        if pos >= 2 && tokens[pos - 1].is_punct(".") {
            if let Some(part) = tokens.get(pos - 2) {
                if part.is_ident() {
                    parts.push(WordPart {
                        text: part.text.clone(),
                        quoted: part.is_quoted(),
                    });
                    pos -= 2;
                    continue;
                }
            }
        }
        break;
    }
    parts.reverse();

    WordContext {
        start,
        prefix,
        prefix_quoted,
        parts,
        in_literal: false,
    }
}

// ---------------------------------------------------------------------------
// Statement ranges (moved here from session_manager so completion can share
// them without depending on the editor type).
// ---------------------------------------------------------------------------

fn is_word_tok(t: &Token, word: &str) -> bool {
    !t.is_quoted()
        && matches!(t.kind, TokenKind::Keyword | TokenKind::Ident { .. })
        && t.text.eq_ignore_ascii_case(word)
}

fn is_plsql_block_tokens(toks: &[&Token]) -> bool {
    let Some(first) = toks.first() else {
        return false;
    };
    if is_word_tok(first, "DECLARE") || is_word_tok(first, "BEGIN") {
        return true;
    }
    if !is_word_tok(first, "CREATE") {
        return false;
    }
    let mut idx = 1usize;
    while let Some(t) = toks.get(idx) {
        if is_word_tok(t, "OR")
            || is_word_tok(t, "REPLACE")
            || is_word_tok(t, "EDITIONABLE")
            || is_word_tok(t, "NONEDITIONABLE")
        {
            idx += 1;
            continue;
        }
        if is_word_tok(t, "PROCEDURE")
            || is_word_tok(t, "FUNCTION")
            || is_word_tok(t, "PACKAGE")
            || is_word_tok(t, "TRIGGER")
        {
            return true;
        }
        // `CREATE TYPE ... AS ENUM/OBJECT` terminates at `);`, whereas
        // `CREATE TYPE BODY ...` is a PL/SQL unit terminating at `END [name];`.
        if is_word_tok(t, "TYPE") {
            return toks.get(idx + 1).is_some_and(|n| is_word_tok(n, "BODY"));
        }
        return false;
    }
    false
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlsqlScope {
    Block { in_body: bool },
    Case,
}

/// True when `toks` (non-comment tokens before a `;`) forms a complete PL/SQL
/// block whose outermost scope just closed at `END [label]`. Tracks nested
/// `BEGIN ... END`, subprogram bodies inside `DECLARE` / `PACKAGE BODY`, and
/// `CASE ... END` expressions so neither `CASE ... END;` nor `END sub_proc;`
/// splits the outer block prematurely, while `END pkg_name;` terminates cleanly.
fn plsql_block_complete(toks: &[&Token]) -> bool {
    let len = toks.len();
    if len == 0 {
        return false;
    }
    let ends_with_end = is_word_tok(toks[len - 1], "END")
        || (len >= 2
            && is_word_tok(toks[len - 2], "END")
            && matches!(
                toks[len - 1].kind,
                TokenKind::Ident { .. } | TokenKind::Keyword
            )
            && !is_word_tok(toks[len - 1], "IF")
            && !is_word_tok(toks[len - 1], "LOOP")
            && !is_word_tok(toks[len - 1], "CASE"));
    if !ends_with_end {
        return false;
    }

    let mut stack: Vec<PlsqlScope> = Vec::new();
    let mut awaiting_unit_is_as = false;
    let mut saw_open = false;
    let mut paren_depth = 0usize;
    let mut k = 0usize;

    while k < len {
        let t = toks[k];
        if t.is_punct("(") {
            paren_depth += 1;
            k += 1;
            continue;
        }
        if t.is_punct(")") {
            paren_depth = paren_depth.saturating_sub(1);
            k += 1;
            continue;
        }
        if t.is_punct(";") {
            if paren_depth == 0 {
                // Forward declaration in a package spec (`PROCEDURE p(x NUMBER);`).
                awaiting_unit_is_as = false;
            }
            k += 1;
            continue;
        }

        // Qualified member names (`pkg.end`, `rec.begin`) are never PL/SQL keywords.
        if k > 0 && toks[k - 1].is_punct(".") {
            k += 1;
            continue;
        }

        if !t.is_quoted() && matches!(t.kind, TokenKind::Keyword | TokenKind::Ident { .. }) {
            let up = t.text.to_ascii_uppercase();
            match up.as_str() {
                "PACKAGE" | "PROCEDURE" | "FUNCTION" if paren_depth == 0 => {
                    awaiting_unit_is_as = true;
                }
                "TYPE"
                    if paren_depth == 0
                        && toks.get(k + 1).is_some_and(|n| is_word_tok(n, "BODY")) =>
                {
                    awaiting_unit_is_as = true;
                }
                "COMPOUND"
                    if paren_depth == 0
                        && toks.get(k + 1).is_some_and(|n| is_word_tok(n, "TRIGGER")) =>
                {
                    stack.push(PlsqlScope::Block { in_body: true });
                    saw_open = true;
                    k += 2;
                    continue;
                }
                "IS" | "AS" if awaiting_unit_is_as && paren_depth == 0 => {
                    awaiting_unit_is_as = false;
                    let is_ext = toks
                        .get(k + 1)
                        .is_some_and(|n| is_word_tok(n, "LANGUAGE"));
                    if !is_ext {
                        stack.push(PlsqlScope::Block { in_body: false });
                        saw_open = true;
                    }
                }
                "DECLARE" if paren_depth == 0 => {
                    awaiting_unit_is_as = false;
                    stack.push(PlsqlScope::Block { in_body: false });
                    saw_open = true;
                }
                "BEGIN" if paren_depth == 0 => {
                    awaiting_unit_is_as = false;
                    match stack.last_mut() {
                        Some(PlsqlScope::Block { in_body }) if !*in_body => {
                            *in_body = true;
                        }
                        _ => {
                            stack.push(PlsqlScope::Block { in_body: true });
                            saw_open = true;
                        }
                    }
                }
                "CASE" => {
                    stack.push(PlsqlScope::Case);
                }
                "END" => {
                    let next_word = toks
                        .get(k + 1)
                        .filter(|n| {
                            !n.is_quoted()
                                && matches!(n.kind, TokenKind::Keyword | TokenKind::Ident { .. })
                        })
                        .map(|n| n.text.to_ascii_uppercase());
                    match next_word.as_deref() {
                        Some("IF") | Some("LOOP") => {
                            k += 2;
                            continue;
                        }
                        Some("CASE") => {
                            if let Some(pos) = stack.iter().rposition(|s| *s == PlsqlScope::Case) {
                                stack.truncate(pos);
                            }
                            k += 2;
                            continue;
                        }
                        Some("BEFORE") | Some("AFTER") | Some("INSTEAD") => {
                            if matches!(stack.last(), Some(PlsqlScope::Block { .. })) {
                                stack.pop();
                            }
                            k += 2;
                            continue;
                        }
                        _ => {
                            stack.pop();
                            // Skip optional `END <label>` right before `;` or end of input.
                            if k + 1 < len
                                && matches!(
                                    toks[k + 1].kind,
                                    TokenKind::Ident { .. } | TokenKind::Keyword
                                )
                                && (k + 2 == len || toks[k + 2].is_punct(";"))
                            {
                                k += 2;
                                continue;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        k += 1;
    }

    saw_open && stack.is_empty() && !awaiting_unit_is_as
}

/// Split SQL into `(start, end, trimmed_statement)` byte ranges. `end` points
/// just past the terminator; `start` is the raw (untrimmed) start.
pub fn statement_ranges(text: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut line_start = 0usize;

    let mut in_single = false;
    let mut in_double = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut saw_dollar_quote = false;

    while i < len {
        let c = bytes[i];

        if in_line_comment {
            if c == b'\n' {
                in_line_comment = false;
                line_start = i + 1;
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if c == b'*' && i + 1 < len && bytes[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
            } else {
                if c == b'\n' {
                    line_start = i + 1;
                }
                i += 1;
            }
            continue;
        }
        if in_single {
            if c == b'\'' {
                in_single = false;
            } else if c == b'\n' {
                line_start = i + 1;
            }
            i += 1;
            continue;
        }
        if in_double {
            if c == b'"' {
                in_double = false;
            } else if c == b'\n' {
                line_start = i + 1;
            }
            i += 1;
            continue;
        }

        // Oracle `q'[...]'`, Postgres `E'...'` and `$$...$$` / `$tag$...$tag$` literals.
        if matches!(c, b'q' | b'Q' | b'e' | b'E' | b'$') {
            let prev_is_ident = text[..i]
                .chars()
                .next_back()
                .map(is_ident_cont)
                .unwrap_or(false);
            if !prev_is_ident {
                if let Some((end, is_dollar)) = scan_q_quote(bytes, i)
                    .map(|e| (e, false))
                    .or_else(|| scan_e_quote(bytes, i).map(|e| (e, false)))
                    .or_else(|| scan_dollar_quote(bytes, i).map(|e| (e, true)))
                {
                    if is_dollar {
                        saw_dollar_quote = true;
                    }
                    if let Some(rel_nl) = bytes[i..end].iter().rposition(|&b| b == b'\n') {
                        line_start = i + rel_nl + 1;
                    }
                    i = end;
                    continue;
                }
            }
        }

        match c {
            b'\n' => {
                let line = text[line_start..i].trim();
                if line == "/" {
                    let stmt = text[start..line_start]
                        .trim()
                        .trim_end_matches(';')
                        .trim()
                        .to_string();
                    if !stmt.is_empty() {
                        out.push((start, i + 1, stmt));
                    }
                    start = i + 1;
                    saw_dollar_quote = false;
                }
                line_start = i + 1;
            }
            b'-' if i + 1 < len && bytes[i + 1] == b'-' => {
                in_line_comment = true;
                i += 1;
            }
            b'/' if i + 1 < len && bytes[i + 1] == b'*' => {
                in_block_comment = true;
                i += 1;
            }
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b';' => {
                let acc = text[start..i].trim();
                // Postgres routines (`CREATE FUNCTION ... AS $$ ... $$ LANGUAGE plpgsql;`)
                // wrap internal semicolons in dollar quotes and terminate at the outer `;`.
                let keep_collecting = if !saw_dollar_quote && !acc.is_empty() {
                    let all_toks = tokenize(acc);
                    let non_comment: Vec<&Token> = all_toks
                        .iter()
                        .filter(|t| t.kind != TokenKind::Comment)
                        .collect();
                    is_plsql_block_tokens(&non_comment) && !plsql_block_complete(&non_comment)
                } else {
                    false
                };
                if keep_collecting {
                    // Internal `;` inside a PL/SQL block — keep collecting.
                } else {
                    let stmt = acc.trim_end_matches(';').trim().to_string();
                    if !stmt.is_empty() {
                        out.push((start, i + 1, stmt));
                    }
                    start = i + 1;
                    line_start = i + 1;
                    saw_dollar_quote = false;
                }
            }
            _ => {}
        }
        i += 1;
    }

    let tail = text[start..]
        .trim()
        .trim_end_matches(';')
        .trim()
        .to_string();
    if !tail.is_empty() && tail != "/" {
        out.push((start, text.len(), tail));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_skips_strings_and_comments() {
        let toks = tokenize("SELECT 'a;b' -- x;\nFROM emp");
        assert!(toks.iter().any(|t| t.is_keyword("SELECT")));
        assert!(toks.iter().any(|t| t.is_ident() && t.text == "emp"));
        assert!(!toks.iter().any(|t| t.is_punct(";")));
    }

    #[test]
    fn word_before_alias_dot() {
        let w = word_before("SELECT * FROM emp e WHERE e.", 28);
        assert_eq!(w.prefix, "");
        assert_eq!(w.parts.len(), 1);
        assert_eq!(w.parts[0].text, "e");
    }

    #[test]
    fn word_before_qualified_partial() {
        let line = "SELECT e.emp";
        let w = word_before(line, line.len());
        assert_eq!(w.prefix, "emp");
        assert_eq!(w.parts[0].text, "e");
        assert_eq!(w.start, 9);
    }

    #[test]
    fn word_before_schema_table() {
        let line = "FROM sc.emp";
        let w = word_before(line, line.len());
        assert_eq!(w.prefix, "emp");
        assert_eq!(w.parts.len(), 1);
        assert_eq!(w.parts[0].text, "sc");
    }

    #[test]
    fn word_before_quoted() {
        let line = "FROM \"Emp";
        let w = word_before(line, line.len());
        assert_eq!(w.prefix, "Emp");
        assert!(w.prefix_quoted);
        assert_eq!(w.start, 5);
    }

    #[test]
    fn word_before_string_is_literal() {
        let line = "SELECT 'abc";
        let w = word_before(line, line.len());
        assert!(w.in_literal);
    }

    #[test]
    fn statement_ranges_still_splits() {
        let ranges = statement_ranges("select 1; select 2;");
        assert_eq!(ranges.len(), 2);
    }

    #[test]
    fn statement_ranges_respects_q_quotes_and_dollar_quotes() {
        // Oracle q'[...]' containing single quotes and semicolons stays intact.
        let q = statement_ranges("SELECT q'[it's; still; one]' FROM dual; SELECT 2;");
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].2, "SELECT q'[it's; still; one]' FROM dual");

        // Postgres dollar-quoted function + subsequent query splits cleanly at outer `;`.
        let pg = statement_ranges(
            "CREATE OR REPLACE FUNCTION f() RETURNS int AS $fn$\nBEGIN\n  RETURN 1;\nEND;\n$fn$ LANGUAGE plpgsql;\nSELECT f();",
        );
        assert_eq!(pg.len(), 2);
        assert!(pg[0].2.ends_with("LANGUAGE plpgsql"));
        assert_eq!(pg[1].2, "SELECT f()");

        // Oracle identifiers with `$` (`v$session`, `BIN$abc$0`) are never misread as dollar quotes.
        let ora = statement_ranges("SELECT * FROM v$session WHERE name = 'BIN$1$0'; SELECT 1;");
        assert_eq!(ora.len(), 2);

        // Postgres E'...' backslash-escaped quotes do not split on internal `;`.
        let pg_e = statement_ranges("SELECT E'it\\'s; still; one', E'back\\\\'; SELECT 2;");
        assert_eq!(pg_e.len(), 2);
        assert_eq!(pg_e[0].2, "SELECT E'it\\'s; still; one', E'back\\\\'");
        assert_eq!(pg_e[1].2, "SELECT 2");
    }

    #[test]
    fn statement_ranges_handles_plsql_end_labels_case_and_packages() {
        // Named PL/SQL ending (`END my_proc;`) terminates cleanly without swallowing next query.
        let named = statement_ranges(
            "CREATE OR REPLACE PROCEDURE my_proc AS\nBEGIN\n  NULL;\nEND my_proc;\nSELECT 1 FROM dual;",
        );
        assert_eq!(named.len(), 2);
        assert!(named[0].2.ends_with("END my_proc"));
        assert_eq!(named[1].2, "SELECT 1 FROM dual");

        // Internal `CASE ... END;` and nested `BEGIN ... END;` do not split the outer block.
        let nested = statement_ranges(
            "DECLARE\n  x NUMBER;\nBEGIN\n  x := CASE WHEN 1 = 1 THEN 2 ELSE 3 END;\n  BEGIN\n    NULL;\n  END;\nEND;\nSELECT 2 FROM dual;",
        );
        assert_eq!(nested.len(), 2);
        assert!(nested[0].2.starts_with("DECLARE"));
        assert_eq!(nested[1].2, "SELECT 2 FROM dual");

        // Package spec + package body with internal subprograms.
        let pkg = statement_ranges(
            "CREATE OR REPLACE PACKAGE pkg AS\n  PROCEDURE p(x NUMBER);\nEND pkg;\n\
             CREATE OR REPLACE PACKAGE BODY pkg AS\n  PROCEDURE p(x NUMBER) IS\n  BEGIN\n    NULL;\n  END p;\nEND pkg;\n\
             SELECT 3 FROM dual;",
        );
        assert_eq!(pkg.len(), 3);
        assert_eq!(pkg[2].2, "SELECT 3 FROM dual");

        // `CREATE TYPE ... AS ENUM` is not a PL/SQL block and terminates at `);`.
        let pg_enum = statement_ranges("CREATE TYPE mood AS ENUM ('sad', 'happy');\nSELECT 1;");
        assert_eq!(pg_enum.len(), 2);
        assert_eq!(pg_enum[1].2, "SELECT 1");
    }
}
