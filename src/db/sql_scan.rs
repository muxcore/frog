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

        // Strings (with '' escaping and Oracle q'...').
        let prev_is_ident = text[..start]
            .chars()
            .next_back()
            .map(is_ident_cont)
            .unwrap_or(false);
        if let Some(end) = if !prev_is_ident {
            scan_q_quote(bytes, i)
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

fn is_plsql_block_start(upper: &str) -> bool {
    if upper.starts_with("DECLARE") || upper.starts_with("BEGIN") {
        return true;
    }
    let mut words = upper.split_whitespace();
    if words.next() != Some("CREATE") {
        return false;
    }
    loop {
        match words.next() {
            None => return false,
            Some("OR") => continue,
            Some("REPLACE") | Some("EDITIONABLE") | Some("NONEDITIONABLE") => continue,
            Some(
                "PROCEDURE" | "FUNCTION" | "PACKAGE" | "TRIGGER" | "TYPE" | "JAVA" | "LIBRARY",
            ) => return true,
            _ => return false,
        }
    }
}

fn ends_with_end_block(upper: &str) -> bool {
    upper.split_whitespace().next_back() == Some("END")
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
                let upper = acc.to_uppercase();
                let is_block = is_plsql_block_start(&upper);
                if is_block && !ends_with_end_block(&upper) {
                    // Internal `;` inside a PL/SQL block — keep collecting.
                } else {
                    let stmt = acc.trim_end_matches(';').trim().to_string();
                    if !stmt.is_empty() {
                        out.push((start, i + 1, stmt));
                    }
                    start = i + 1;
                    line_start = i + 1;
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
}
