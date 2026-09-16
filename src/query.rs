//! Structured filter DSL: `service:api level:error user_id:42 "connection refused"`.
//!
//! Translates a small search-bar DSL into a safe, parameterized SQL WHERE
//! clause. Keys are validated against a whitelist (built-in columns + configured
//! hot attributes) so user input can never become raw SQL. Values are always
//! bound as parameters — never interpolated.
//!
//! Grammar:
//! ```text
//! filter     := or_expr
//! or_expr    := and_expr ( OR and_expr )*
//! and_expr   := unary ( (AND)? unary )*      (adjacent clauses = implicit AND)
//! unary      := '-' unary | atom
//! atom       := '(' or_expr ')' | clause
//! clause     := comparison | bare_text
//! comparison := key op value
//! op         := ':' | '!=' | '>=' | '<=' | '>' | '<' | '~'
//! key        := identifier  (must be a known column)
//! value      := quoted("...") | bare_token
//! bare_text  := a token with no key:value form → implicit `message ILIKE '%text%'`
//! ```
//!
//! - Connectors: `AND` / `OR` (case-insensitive). Adjacent clauses combine
//!   with implicit AND. AND binds tighter than OR.
//! - Parens: `(a OR b) AND c` groups explicitly.
//! - Exclusion: a leading `-` negates a clause or group (`-service:x`,
//!   `-(a OR b)`). Only position 0 counts — `user_id:-42` keeps `-42` as the
//!   value. Quote a leading dash to search for it literally.
//!
//! Examples:
//! - `service:api level:error` → `service = ? AND level = ?` with `["api","error"]`
//! - `user_id:42` → `user_id = ?` with `["42"]` (cast to BIGINT by DuckDB)
//! - `message:"connection refused"` → `message ILIKE ?` with `["%connection refused%"]`
//! - `duration_ms>1000` → `duration_ms > ?` with `["1000"]`
//! - `service:api timeout` → `service = ? AND message ILIKE ?` with `["api","%timeout%"]`
//! - `service:api OR service:web` → `service = ? OR service = ?`
//! - `-service:central-logs` → `service != ?` (exclude internal audit logs)
//! - `level:error -service:central-logs` → errors from everything but central-logs
//! - `(service:api OR service:web) -service:central-logs` →
//!   `(service = ? OR service = ?) AND service != ?`
//! - `a OR b c` → `a OR (b AND c)` (AND binds tighter)

use std::collections::HashMap;

use crate::hot::{HotAttribute, HotType};

/// SQL operator that a clause uses to combine key and value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,   // `:` or `=`
    Ne,   // `!=`
    Gt,   // `>`
    Ge,   // `>=`
    Lt,   // `<`
    Le,   // `<=`
    Like, // `~`  (ILIKE)
}

/// SQL text for `op`, flipped to its inverse when `negated` (the `-` prefix).
/// Like has no clean inverse operator, so it renders as `NOT ILIKE`
/// (supported by DuckDB).
fn sql_op(op: Op, negated: bool) -> &'static str {
    match (op, negated) {
        (Op::Eq, false) => "=",
        (Op::Eq, true) => "!=",
        (Op::Ne, false) => "!=",
        (Op::Ne, true) => "=",
        (Op::Gt, false) => ">",
        (Op::Gt, true) => "<=",
        (Op::Ge, false) => ">=",
        (Op::Ge, true) => "<",
        (Op::Lt, false) => "<",
        (Op::Lt, true) => ">=",
        (Op::Le, false) => "<=",
        (Op::Le, true) => ">",
        (Op::Like, false) => "ILIKE",
        (Op::Like, true) => "NOT ILIKE",
    }
}

/// One parsed leaf clause.
#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    /// `key op value` — emits `key <op> ?` (or the negated form).
    Comparison {
        key: String,
        op: Op,
        value: String,
        /// `-key:value` → negated (e.g. Eq renders as `!=`).
        negated: bool,
    },
    /// Bare text with no key — emits `message ILIKE ?` with `%text%`
    /// (or `NOT (message ILIKE ?)` when negated).
    TextSearch { text: String, negated: bool },
}

/// Boolean expression tree over clauses. N-ary And/Or keep the rendered SQL
/// free of redundant parens for lists like `a AND b AND c`.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Term(Clause),
    /// `-(group)` — renders `NOT (group)` (bare `-clause` is folded into the
    /// clause's own `negated` flag instead).
    Not(Box<Expr>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
}

/// Binding strength for minimal-parens rendering. Higher binds tighter.
const PREC_OR: u8 = 1;
const PREC_AND: u8 = 2;
const PREC_ATOM: u8 = 3;

/// Result of compiling a DSL string.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompiledFilter {
    pub root: Option<Expr>,
}

impl CompiledFilter {
    /// True if nothing was parsed — caller should skip emitting a WHERE
    /// clause entirely.
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// Render to a SQL WHERE-clause body (without the leading `WHERE`) and a
    /// parallel parameter vector. `columns` is the key whitelist (column name
    /// → DuckDB type); unknown keys cause an error.
    pub fn to_sql(
        &self,
        columns: &ColumnWhitelist,
    ) -> Result<(String, Vec<String>), FilterError> {
        let Some(root) = &self.root else {
            return Ok((String::new(), Vec::new()));
        };
        let mut params = Vec::new();
        let sql = render_expr(root, Ctx::Top, columns, &mut params)?;
        Ok((sql, params))
    }

    /// All leaf clauses in the tree, in order. Used by consumers that inspect
    /// individual terms (e.g. per-service query caps). Note: terms inside a
    /// `NOT (group)` are still yielded — treat them conservatively.
    pub fn terms(&self) -> Vec<&Clause> {
        fn walk<'a>(e: &'a Expr, out: &mut Vec<&'a Clause>) {
            match e {
                Expr::Term(c) => out.push(c),
                Expr::Not(inner) => walk(inner, out),
                Expr::And(items) | Expr::Or(items) => {
                    for i in items {
                        walk(i, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            walk(root, &mut out);
        }
        out
    }
}

/// Render `expr` to SQL, adding parens where a sub-group's binding differs
/// from its context: an `And` group inside an `Or` gets wrapped for
/// readability; an `Or` group inside an `And` MUST be wrapped (required).
/// Terms and `NOT (…)` render bare — `Not` adds its own parens.
fn render_expr(
    expr: &Expr,
    parent: Ctx,
    columns: &ColumnWhitelist,
    params: &mut Vec<String>,
) -> Result<String, FilterError> {
    match expr {
        Expr::Term(c) => render_clause(c, columns, params),
        Expr::Not(inner) => {
            let body = render_expr(inner, Ctx::InNot, columns, params)?;
            Ok(format!("NOT ({body})"))
        }
        Expr::And(items) => {
            let parts: Vec<String> = items
                .iter()
                .map(|i| render_expr(i, Ctx::InAnd, columns, params))
                .collect::<Result<_, _>>()?;
            let body = parts.join(" AND ");
            Ok(if matches!(parent, Ctx::InOr) {
                format!("({body})")
            } else {
                body
            })
        }
        Expr::Or(items) => {
            let parts: Vec<String> = items
                .iter()
                .map(|i| render_expr(i, Ctx::InOr, columns, params))
                .collect::<Result<_, _>>()?;
            let body = parts.join(" OR ");
            Ok(if matches!(parent, Ctx::InAnd) {
                format!("({body})")
            } else {
                body
            })
        }
    }
}

/// Rendering context: which connector (if any) directly encloses the node.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Ctx {
    Top,
    InAnd,
    InOr,
    InNot,
}

/// Render one leaf clause and push its bound parameter.
fn render_clause(
    c: &Clause,
    columns: &ColumnWhitelist,
    params: &mut Vec<String>,
) -> Result<String, FilterError> {
    match c {
        Clause::TextSearch { text, negated } => {
            params.push(format!("%{text}%"));
            Ok(if *negated {
                "NOT (message ILIKE ?)".to_string()
            } else {
                "message ILIKE ?".to_string()
            })
        }
        Clause::Comparison {
            key,
            op,
            value,
            negated,
        } => {
            let ty =
                columns.get(key).ok_or_else(|| FilterError::UnknownColumn(key.clone()))?;
            // Numeric operators (`>`, `<=`, etc.) require a numeric column;
            // the value must parse. We allow them on Bigint / Double hot
            // columns plus the raw_len column. The check uses the *original*
            // op — negation doesn't change which operators are numeric.
            let numeric_op = matches!(op, Op::Gt | Op::Ge | Op::Lt | Op::Le);
            if numeric_op && !ty.is_numeric() {
                return Err(FilterError::NumericOpOnText {
                    key: key.clone(),
                    op: *op,
                });
            }
            // The `~` (ILIKE) operator is only valid on text columns.
            if *op == Op::Like && !ty.is_text() {
                return Err(FilterError::LikeOnNonText(key.clone()));
            }

            let sql_value = if *op == Op::Like {
                format!("%{value}%")
            } else {
                value.clone()
            };
            // Numeric columns: validate that the value parses, so we never
            // bind "abc" to a BIGINT column. DuckDB would error at runtime,
            // but a friendly parse-time message is better than a SQL failure
            // deep in the engine.
            if ty.is_numeric() && *op != Op::Like {
                let v = sql_value.trim();
                let ok = match ty {
                    ColumnType::Bigint => v.parse::<i64>().is_ok(),
                    ColumnType::Double => v.parse::<f64>().is_ok(),
                    _ => true,
                };
                if !ok {
                    return Err(FilterError::ValueParseError {
                        key: key.clone(),
                        value: value.clone(),
                        expected: format!("{ty:?}"),
                    });
                }
            }
            params.push(sql_value);
            Ok(format!("{key} {} ?", sql_op(*op, *negated)))
        }
    }
}

/// Built-in column types. Hot attributes are added on top of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Text,
    Bigint,
    Double,
    Boolean,
    Timestamp,
}

impl ColumnType {
    pub fn is_text(&self) -> bool {
        matches!(self, ColumnType::Text)
    }
    pub fn is_numeric(&self) -> bool {
        matches!(self, ColumnType::Bigint | ColumnType::Double)
    }
}

impl From<HotType> for ColumnType {
    fn from(t: HotType) -> Self {
        match t {
            HotType::Varchar => ColumnType::Text,
            HotType::Bigint => ColumnType::Bigint,
            HotType::Double => ColumnType::Double,
            HotType::Boolean => ColumnType::Boolean,
        }
    }
}

/// Whitelist of columns the DSL is allowed to reference. Prevents SQL injection
/// via key names (e.g. `1=1 OR foo:bar` where `1=1 OR foo` would be the key).
#[derive(Debug, Clone)]
pub struct ColumnWhitelist {
    map: HashMap<String, ColumnType>,
}

impl ColumnWhitelist {
    /// Build the standard whitelist: built-in filterable columns + configured
    /// hot attributes. Excludes JSON blob columns and internal-only fields.
    pub fn standard(hot: &[HotAttribute]) -> Self {
        let mut map = HashMap::new();
        // Built-in filterable columns.
        map.insert("service".into(), ColumnType::Text);
        map.insert("level".into(), ColumnType::Text);
        map.insert("source_host".into(), ColumnType::Text);
        map.insert("host".into(), ColumnType::Text);
        map.insert("protocol".into(), ColumnType::Text);
        map.insert("trace_id".into(), ColumnType::Text);
        map.insert("span_id".into(), ColumnType::Text);
        map.insert("geo_country".into(), ColumnType::Text);
        map.insert("message".into(), ColumnType::Text);
        map.insert("fingerprint".into(), ColumnType::Text);
        map.insert("raw_len".into(), ColumnType::Bigint);
        // Hot attributes (promoted columns).
        for attr in hot {
            map.insert(attr.name.clone(), attr.duckdb_type.into());
        }
        Self { map }
    }

    pub fn get(&self, key: &str) -> Option<ColumnType> {
        self.map.get(key).copied()
    }

    pub fn keys(&self) -> Vec<&str> {
        self.map.keys().map(|s| s.as_str()).collect()
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum FilterError {
    #[error("syntax error at position {pos}: {msg}")]
    Syntax { pos: usize, msg: String },
    #[error("unknown column '{0}' (not in whitelist)")]
    UnknownColumn(String),
    #[error("numeric operator {op:?} not valid for text column '{key}'")]
    NumericOpOnText { key: String, op: Op },
    #[error("ILIKE (~) operator not valid for non-text column '{0}'")]
    LikeOnNonText(String),
    #[error("value '{value}' doesn't parse as {expected} for column '{key}'")]
    ValueParseError {
        key: String,
        value: String,
        expected: String,
    },
}

/// Parse a DSL string into a [`CompiledFilter`]. Empty / whitespace-only input
/// returns an empty filter.
pub fn parse_filter(input: &str) -> Result<CompiledFilter, FilterError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(CompiledFilter::default());
    }
    let tokens = tokenize(trimmed)?;
    // Merge `Word("key:")` + `Quoted("value")` into a single comparison.
    // Without this, `message:"foo bar"` would parse as two separate clauses.
    let merged = merge_key_quoted(tokens);
    let mut parser = Parser::new(&merged);
    let root = parser.parse_or()?;
    if parser.pos != parser.tokens.len() {
        return Err(FilterError::Syntax {
            pos: 0,
            msg: "unexpected trailing input".into(),
        });
    }
    Ok(CompiledFilter { root: Some(root) })
}

/// Build owned DuckDB Values for a heterogeneous param list. Tries to parse
/// each value as i64, then f64, then falls back to Text. This makes numeric
/// comparisons (`raw_len>=100`) actually compare as numbers instead of strings.
pub(crate) fn params_as_duck(values: &[String]) -> Vec<duckdb::types::Value> {
    values
        .iter()
        .map(|s| {
            // Timestamps (ISO-8601) start with a digit and contain `-`/`:` —
            // NOT numbers. Leave them as Text so DuckDB casts to TIMESTAMP.
            let is_iso_ts = s.len() >= 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[10] == b'T';
            if is_iso_ts {
                return duckdb::types::Value::Text(s.clone());
            }
            if let Ok(i) = s.parse::<i64>() {
                return duckdb::types::Value::BigInt(i);
            }
            if let Ok(f) = s.parse::<f64>() {
                return duckdb::types::Value::Double(f);
            }
            duckdb::types::Value::Text(s.clone())
        })
        .collect()
}

/// Recursive-descent parser over the token list.
///
/// ```text
/// parse_or  := parse_and ( OR parse_and )*
/// parse_and := parse_unary ( (AND)? parse_unary )*   (implicit AND)
/// parse_unary := '-' parse_unary | parse_atom
/// parse_atom := '(' parse_or ')' | clause
/// ```
struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(tokens: &[Token]) -> Self {
        Self {
            tokens: tokens.to_vec(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn at_keyword(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Token::Word(w)) if w.eq_ignore_ascii_case(kw))
    }

    /// True if the next token can begin an atom (clause, quoted text, group,
    /// or negation) — used to detect implicit AND.
    fn at_atom_start(&self) -> bool {
        match self.peek() {
            Some(Token::Word(w)) => {
                !w.eq_ignore_ascii_case("and") && !w.eq_ignore_ascii_case("or")
            }
            Some(Token::Quoted(_) | Token::LParen) => true,
            _ => false,
        }
    }

    fn parse_or(&mut self) -> Result<Expr, FilterError> {
        let mut items = vec![self.parse_and()?];
        while self.at_keyword("or") {
            self.pos += 1;
            items.push(self.parse_and()?);
        }
        Ok(if items.len() == 1 {
            items.pop().expect("len checked")
        } else {
            Expr::Or(items)
        })
    }

    fn parse_and(&mut self) -> Result<Expr, FilterError> {
        let mut items = vec![self.parse_unary()?];
        loop {
            if self.at_keyword("and") {
                self.pos += 1;
                items.push(self.parse_unary()?);
            } else if self.at_atom_start() {
                // Implicit AND between adjacent clauses.
                items.push(self.parse_unary()?);
            } else {
                break;
            }
        }
        Ok(if items.len() == 1 {
            items.pop().expect("len checked")
        } else {
            Expr::And(items)
        })
    }

    fn parse_unary(&mut self) -> Result<Expr, FilterError> {
        if matches!(self.peek(), Some(Token::Word(w)) if w == "-") {
            self.pos += 1;
            let inner = self.parse_unary()?;
            // A negated leaf folds into the clause's own flag so it renders
            // as a clean op flip (`service != ?`); only groups get NOT (…).
            return Ok(match inner {
                Expr::Term(Clause::Comparison { key, op, value, .. }) => {
                    Expr::Term(Clause::Comparison { key, op, value, negated: true })
                }
                Expr::Term(Clause::TextSearch { text, .. }) => {
                    Expr::Term(Clause::TextSearch { text, negated: true })
                }
                other => Expr::Not(Box::new(other)),
            });
        }
        self.parse_atom()
    }

    fn parse_atom(&mut self) -> Result<Expr, FilterError> {
        match self.peek() {
            Some(Token::LParen) => {
                self.pos += 1;
                let inner = self.parse_or()?;
                match self.peek() {
                    Some(Token::RParen) => self.pos += 1,
                    _ => {
                        return Err(FilterError::Syntax {
                            pos: 0,
                            msg: "unbalanced parentheses: missing ')'".into(),
                        })
                    }
                }
                Ok(inner)
            }
            Some(Token::Word(w)) if w.eq_ignore_ascii_case("and") => {
                Err(FilterError::Syntax {
                    pos: 0,
                    msg: "unexpected AND".into(),
                })
            }
            Some(Token::Word(w)) if w.eq_ignore_ascii_case("or") => {
                Err(FilterError::Syntax {
                    pos: 0,
                    msg: "unexpected OR".into(),
                })
            }
            Some(Token::Word(w)) => {
                let c = parse_word(w)?;
                self.pos += 1;
                Ok(Expr::Term(c))
            }
            Some(Token::Quoted(s)) => {
                let c = Clause::TextSearch {
                    text: s.clone(),
                    negated: false,
                };
                self.pos += 1;
                Ok(Expr::Term(c))
            }
            Some(Token::RParen) => Err(FilterError::Syntax {
                pos: 0,
                msg: "unexpected ')'".into(),
            }),
            None => Err(FilterError::Syntax {
                pos: 0,
                msg: "unexpected end of filter".into(),
            }),
        }
    }
}

/// Walk the token list; when a Word ends in an operator char (`:`, `=`, `!`,
/// `>`, `<`, `~`) AND the next token is a Quoted, glue them together.
fn merge_key_quoted(tokens: Vec<Token>) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut iter = tokens.into_iter().peekable();
    while let Some(tok) = iter.next() {
        match tok {
            Token::Word(ref w) if ends_with_operator(w) => {
                if let Some(Token::Quoted(q)) = iter.peek() {
                    let q = q.clone();
                    iter.next(); // consume the quoted
                    out.push(Token::Word(format!("{w}{q}")));
                    continue;
                }
                out.push(tok);
            }
            other => out.push(other),
        }
    }
    out
}

fn ends_with_operator(s: &str) -> bool {
    let last = match s.bytes().last() {
        Some(b) => b,
        None => return false,
    };
    matches!(last, b':' | b'=' | b'!' | b'>' | b'<' | b'~')
}

/// A single token: a bare word, a quoted string, or a paren.
#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    Quoted(String),
    LParen,
    RParen,
}

fn tokenize(input: &str) -> Result<Vec<Token>, FilterError> {
    let mut out = Vec::new();
    let mut chars = input.char_indices().peekable();
    let mut current = String::new();
    let mut current_start = 0usize;

    let flush_word = |out: &mut Vec<Token>, current: &mut String, _start: &mut usize| {
        if !current.is_empty() {
            out.push(Token::Word(std::mem::take(current)));
        }
    };

    while let Some((i, c)) = chars.next() {
        if c.is_whitespace() {
            flush_word(&mut out, &mut current, &mut current_start);
            continue;
        }
        if c == '(' || c == ')' {
            // Parens are structural — always their own tokens, even when
            // glued to a word (`(service:api OR service:web)`).
            flush_word(&mut out, &mut current, &mut current_start);
            out.push(if c == '(' { Token::LParen } else { Token::RParen });
            continue;
        }
        if c == '"' {
            // Start of quoted string. Flush any in-progress word first
            // (handles `message:"foo bar"` — the `message:` part is a word).
            flush_word(&mut out, &mut current, &mut current_start);
            let mut content = String::new();
            let mut closed = false;
            for (_, c2) in chars.by_ref() {
                if c2 == '"' {
                    closed = true;
                    break;
                }
                content.push(c2);
            }
            if !closed {
                return Err(FilterError::Syntax {
                    pos: i,
                    msg: "unterminated quoted string".into(),
                });
            }
            out.push(Token::Quoted(content));
            continue;
        }
        if current.is_empty() {
            current_start = i;
        }
        current.push(c);
    }
    flush_word(&mut out, &mut current, &mut current_start);
    Ok(out)
}

/// Parse a word that may contain a key:value boundary. The first occurrence of
/// an operator char (`:`, `=`, `!`, `>`, `<`, `~`) splits the word into
/// key + value. Everything after the operator is the value (no quoting needed
/// for `service:api`, since spaces would have split the token already).
///
/// A leading `-` negates the clause (`-service:central-logs`, `-timeout`).
/// Only position 0 counts — `user_id:-42` keeps `-42` as the value.
///
/// Special case: if the word contains no operator, it's bare text → TextSearch.
fn parse_word(w: &str) -> Result<Clause, FilterError> {
    let (negated, rest) = match w.strip_prefix('-') {
        Some(r) if !r.is_empty() => (true, r),
        _ => (false, w),
    };

    // Find the first operator character. The operators recognized:
    //   `>=` `<=` `!=` `:` `=` `>` `<` `~`
    let op_idx = rest
        .char_indices()
        .find(|(_, c)| matches!(c, ':' | '=' | '!' | '>' | '<' | '~'))
        .map(|(i, _)| i);
    let Some(op_idx) = op_idx else {
        // No operator → bare-text search.
        return Ok(Clause::TextSearch {
            text: rest.to_string(),
            negated,
        });
    };

    // Identify which operator (look at one or two chars).
    let bytes = rest.as_bytes();
    let (op, value_start) = match bytes[op_idx] {
        b'>' => {
            if bytes.get(op_idx + 1) == Some(&b'=') {
                (Op::Ge, op_idx + 2)
            } else {
                (Op::Gt, op_idx + 1)
            }
        }
        b'<' => {
            if bytes.get(op_idx + 1) == Some(&b'=') {
                (Op::Le, op_idx + 2)
            } else {
                (Op::Lt, op_idx + 1)
            }
        }
        b'!' => {
            if bytes.get(op_idx + 1) == Some(&b'=') {
                (Op::Ne, op_idx + 2)
            } else {
                return Err(FilterError::Syntax {
                    pos: op_idx,
                    msg: "expected `!=` after `!`".into(),
                });
            }
        }
        b'~' => (Op::Like, op_idx + 1),
        b':' | b'=' => (Op::Eq, op_idx + 1),
        _ => unreachable!(),
    };

    let key = rest[..op_idx].to_string();
    // Validate the key is a sane identifier (defensive — even though the
    // whitelist check in to_sql catches unknown keys, we want to reject
    // pathological input early).
    if key.is_empty()
        || key
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || c == '_'))
    {
        return Err(FilterError::Syntax {
            pos: 0,
            msg: format!("invalid column name '{key}'"),
        });
    }
    let value = rest[value_start..].to_string();
    if value.is_empty() {
        return Err(FilterError::Syntax {
            pos: value_start,
            msg: "missing value after operator".into(),
        });
    }
    Ok(Clause::Comparison {
        key,
        op,
        value,
        negated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn whitelist() -> ColumnWhitelist {
        ColumnWhitelist::standard(&[
            HotAttribute::parse_shorthand("user_id:bigint").unwrap(),
            HotAttribute::parse_shorthand("env:varchar").unwrap(),
            HotAttribute::parse_shorthand("is_canary:boolean").unwrap(),
            HotAttribute::parse_shorthand("duration_ms:double").unwrap(),
        ])
    }

    #[test]
    fn empty_input() {
        let f = parse_filter("").unwrap();
        assert!(f.is_empty());
        let f = parse_filter("   ").unwrap();
        assert!(f.is_empty());
    }

    #[test]
    fn simple_key_value_pairs() {
        let f = parse_filter("service:api level:error").unwrap();
        let (sql, params) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "service = ? AND level = ?");
        assert_eq!(params, vec!["api".to_string(), "error".to_string()]);
    }

    #[test]
    fn bare_text_implicit_message_ilike() {
        let f = parse_filter("timeout").unwrap();
        let (sql, params) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "message ILIKE ?");
        assert_eq!(params, vec!["%timeout%".to_string()]);
    }

    #[test]
    fn mixed_filters_and_text() {
        let f = parse_filter("service:api timeout").unwrap();
        let (sql, params) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "service = ? AND message ILIKE ?");
        assert_eq!(params, vec!["api".to_string(), "%timeout%".to_string()]);
    }

    #[test]
    fn numeric_operators() {
        let f = parse_filter("duration_ms>1000").unwrap();
        let (sql, params) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "duration_ms > ?");
        assert_eq!(params, vec!["1000".to_string()]);

        let f = parse_filter("raw_len>=1024").unwrap();
        let (sql, _) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "raw_len >= ?");
    }

    #[test]
    fn numeric_value_cast_inference() {
        // Bigint column → param still a string, DuckDB casts at bind time.
        let f = parse_filter("user_id:42").unwrap();
        let (sql, params) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "user_id = ?");
        assert_eq!(params, vec!["42".to_string()]);
    }

    #[test]
    fn unknown_column_rejected() {
        let f = parse_filter("evil_col:1").unwrap();
        let err = f.to_sql(&whitelist()).unwrap_err();
        assert!(matches!(err, FilterError::UnknownColumn(_)));
    }

    #[test]
    fn numeric_op_on_text_rejected() {
        let f = parse_filter("service>foo").unwrap();
        let err = f.to_sql(&whitelist()).unwrap_err();
        assert!(matches!(err, FilterError::NumericOpOnText { .. }));
    }

    #[test]
    fn invalid_numeric_value_rejected_at_parse_time() {
        let f = parse_filter("user_id:abc").unwrap();
        let err = f.to_sql(&whitelist()).unwrap_err();
        assert!(matches!(err, FilterError::ValueParseError { .. }));
    }

    #[test]
    fn sql_injection_via_key_rejected() {
        // Even if the user tries `1=1 OR foo:bar`, the key `1=1 OR foo`
        // contains spaces and won't tokenize as a single key. And even if they
        // wrote `evil:bar`, the whitelist check rejects unknown keys.
        let f = parse_filter("evil:bar").unwrap();
        assert!(matches!(f.to_sql(&whitelist()), Err(FilterError::UnknownColumn(_))));
    }

    #[test]
    fn sql_injection_via_value_is_safe() {
        // Even with malicious value content, the output is a bound parameter —
        // the value never reaches the SQL string as raw text. The value
        // `api';--` (with no spaces) stays as one token and becomes the param.
        let f = parse_filter("service:api';--").unwrap();
        let (sql, params) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "service = ?");
        assert_eq!(params, vec!["api';--".to_string()]);
        // The critical safety property: no DROP TABLE / OR 1=1 / semicolons in
        // the SQL string itself — everything is `?` placeholders.
        assert!(!sql.contains(';'), "no semicolons in generated SQL");
        assert!(!sql.contains("DROP"), "no DROP in generated SQL");
        assert!(!sql.contains(" OR "), "no OR in generated SQL");
    }

    #[test]
    fn quoted_value_with_spaces() {
        // Bare quoted string → message ILIKE.
        let f = parse_filter(r#""connection refused""#).unwrap();
        let (sql, params) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "message ILIKE ?");
        assert_eq!(params, vec!["%connection refused%".to_string()]);
    }

    #[test]
    fn key_with_quoted_value() {
        // `message:"connection refused"` should parse as ONE clause
        // (the tokenizer merges `message:` + the quoted string).
        let f = parse_filter(r#"message:"connection refused""#).unwrap();
        let (sql, params) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "message = ?");
        assert_eq!(params, vec!["connection refused".to_string()]);
    }

    #[test]
    fn key_with_quoted_value_and_other_clauses() {
        let f = parse_filter(r#"service:api message:"connection refused" user_id:42"#).unwrap();
        let (sql, params) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "service = ? AND message = ? AND user_id = ?");
        assert_eq!(
            params,
            vec![
                "api".to_string(),
                "connection refused".to_string(),
                "42".to_string(),
            ]
        );
    }

    #[test]
    fn like_operator_explicit() {
        let f = parse_filter("message~timeout").unwrap();
        let (sql, params) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "message ILIKE ?");
        assert_eq!(params, vec!["%timeout%".to_string()]);
    }

    #[test]
    fn like_on_numeric_rejected() {
        let f = parse_filter("user_id~42").unwrap();
        let err = f.to_sql(&whitelist()).unwrap_err();
        assert!(matches!(err, FilterError::LikeOnNonText(_)));
    }

    // ------------------------------------------------------------------
    // AND / OR / parens / exclusion
    // ------------------------------------------------------------------

    #[test]
    fn explicit_and_is_same_as_implicit() {
        let f = parse_filter("service:api AND level:error").unwrap();
        let (sql, params) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "service = ? AND level = ?");
        assert_eq!(params, vec!["api".to_string(), "error".to_string()]);
    }

    #[test]
    fn lowercase_connectors_also_work() {
        let f = parse_filter("service:api or service:web").unwrap();
        let (sql, _) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "service = ? OR service = ?");
    }

    #[test]
    fn and_binds_tighter_than_or() {
        // `a b OR c` → (a AND b) OR c
        let f = parse_filter("service:api level:error OR service:web").unwrap();
        let (sql, params) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "(service = ? AND level = ?) OR service = ?");
        assert_eq!(
            params,
            vec!["api".to_string(), "error".to_string(), "web".to_string()]
        );

        // `a OR b c` → a OR (b AND c)
        let f = parse_filter("service:api OR service:web level:error").unwrap();
        let (sql, _) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "service = ? OR (service = ? AND level = ?)");
    }

    #[test]
    fn parens_group_expressions() {
        let f = parse_filter("(service:api OR service:web) level:error").unwrap();
        let (sql, params) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "(service = ? OR service = ?) AND level = ?");
        assert_eq!(
            params,
            vec!["api".to_string(), "web".to_string(), "error".to_string()]
        );
    }

    #[test]
    fn nested_parens() {
        let f =
            parse_filter("((service:api OR service:web) OR service:db) -level:debug").unwrap();
        let (sql, _) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "(service = ? OR service = ? OR service = ?) AND level != ?");
    }

    #[test]
    fn quoted_and_stays_literal_text() {
        let f = parse_filter(r#"service:api "and""#).unwrap();
        let (sql, params) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "service = ? AND message ILIKE ?");
        assert_eq!(params, vec!["api".to_string(), "%and%".to_string()]);
    }

    #[test]
    fn dangling_connectors_rejected() {
        assert!(parse_filter("OR service:api").is_err());
        assert!(parse_filter("service:api OR").is_err());
        assert!(parse_filter("service:api OR OR service:web").is_err());
        assert!(parse_filter("AND").is_err());
    }

    #[test]
    fn unbalanced_parens_rejected() {
        assert!(parse_filter("(service:api OR service:web").is_err());
        assert!(parse_filter("service:api)").is_err());
        assert!(parse_filter(")(").is_err());
    }

    #[test]
    fn exclude_comparison() {
        let f = parse_filter("-service:central-logs").unwrap();
        let (sql, params) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "service != ?");
        assert_eq!(params, vec!["central-logs".to_string()]);
    }

    #[test]
    fn exclude_combined_with_clauses() {
        let f = parse_filter("level:error -service:central-logs").unwrap();
        let (sql, params) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "level = ? AND service != ?");
        assert_eq!(params, vec!["error".to_string(), "central-logs".to_string()]);
    }

    #[test]
    fn exclude_bare_text_uses_not_ilike() {
        let f = parse_filter("-timeout").unwrap();
        let (sql, params) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "NOT (message ILIKE ?)");
        assert_eq!(params, vec!["%timeout%".to_string()]);
    }

    #[test]
    fn exclude_like_operator() {
        let f = parse_filter("-message~health").unwrap();
        let (sql, params) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "message NOT ILIKE ?");
        assert_eq!(params, vec!["%health%".to_string()]);
    }

    #[test]
    fn exclude_flips_numeric_ops() {
        let f = parse_filter("-duration_ms>1000").unwrap();
        let (sql, params) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "duration_ms <= ?");
        assert_eq!(params, vec!["1000".to_string()]);
    }

    #[test]
    fn exclude_negated_group() {
        let f = parse_filter("level:error -(service:api OR service:web)").unwrap();
        let (sql, _) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "level = ? AND NOT (service = ? OR service = ?)");
    }

    #[test]
    fn negative_numeric_value_not_treated_as_exclusion() {
        let f = parse_filter("user_id:-42").unwrap();
        let (sql, params) = f.to_sql(&whitelist()).unwrap();
        assert_eq!(sql, "user_id = ?");
        assert_eq!(params, vec!["-42".to_string()]);
    }

    #[test]
    fn terms_walks_the_tree() {
        let f = parse_filter("(service:api OR service:web) -service:central-logs").unwrap();
        let terms = f.terms();
        assert_eq!(terms.len(), 3);
        let pinned: Vec<&str> = terms
            .iter()
            .filter_map(|c| match c {
                Clause::Comparison { key, op: Op::Eq, value, negated: false }
                    if key == "service" =>
                {
                    Some(value.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(pinned, vec!["api", "web"]);
    }
}
