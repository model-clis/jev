//! Assertion expressions turn answer sets into branchable automation outcomes.
//!
//! Grammar (precedence: `not` > `and` > `or`):
//!
//! ```text
//! expr       := or
//! or         := and { "or" and }
//! and        := unary { "and" unary }
//! unary      := "not" unary | "(" expr ")" | comparison
//! comparison := operand op operand | operand "in" list
//! operand    := path | number | true | false | "string"
//! path       := ident { "." ident | "[" "string" "]" }
//! op         := <= | >= | == | != | < | >
//! list       := "[" operand { "," operand } "]"
//! ```
//!
//! Paths resolve against the evaluated response envelope (`model`, `answers`,
//! `usage`). A path that cannot be resolved, or a comparison between mixed
//! scalar types, is a usage error — it never silently evaluates to `false`,
//! because a silent false would pass or block a gate without notice.

use anyhow::{Context, Result, bail};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Number(f64),
    Str(String),
    Dot,
    Le,
    Ge,
    Eq,
    Ne,
    Lt,
    Gt,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    And,
    Or,
    Not,
    In,
    True,
    False,
}

fn lex(src: &str) -> Result<Vec<Token>> {
    let chars: Vec<char> = src.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\r' | '\n' => i += 1,
            '.' => {
                tokens.push(Token::Dot);
                i += 1;
            }
            '<' | '>' | '=' | '!' => {
                let (token, width) = match (c, chars.get(i + 1)) {
                    ('=', Some('=')) => (Token::Eq, 2),
                    ('=', _) => {
                        bail!("single '=' is not an operator; use ==, !=, <=, >=, <, >")
                    }
                    ('!', Some('=')) => (Token::Ne, 2),
                    ('!', _) => bail!("single '!' is not an operator; use != for inequality"),
                    ('<', Some('=')) => (Token::Le, 2),
                    ('>', Some('=')) => (Token::Ge, 2),
                    ('<', _) => (Token::Lt, 1),
                    (_, _) => (Token::Gt, 1),
                };
                tokens.push(token);
                i += width;
            }
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            '[' => {
                tokens.push(Token::LBracket);
                i += 1;
            }
            ']' => {
                tokens.push(Token::RBracket);
                i += 1;
            }
            ',' => {
                tokens.push(Token::Comma);
                i += 1;
            }
            '"' => {
                let (s, next) = lex_string(&chars, i + 1)
                    .with_context(|| format!("at column {}: bad string literal", i + 1))?;
                tokens.push(Token::Str(s));
                i = next;
            }
            '0'..='9' => {
                let mut j = i;
                let mut dots = 0;
                while j < chars.len()
                    && (chars[j].is_ascii_digit()
                        || (chars[j] == '.'
                            && dots == 0
                            && chars.get(j + 1).is_some_and(|c| c.is_ascii_digit())))
                {
                    if chars[j] == '.' {
                        dots += 1;
                    }
                    j += 1;
                }
                let text: String = chars[i..j].iter().collect();
                let n: f64 = text
                    .parse()
                    .with_context(|| format!("invalid number '{text}'"))?;
                tokens.push(Token::Number(n));
                i = j;
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                // '-' is safe inside identifiers: the grammar has no minus operator.
                let mut j = i;
                while j < chars.len()
                    && (chars[j].is_ascii_alphanumeric() || chars[j] == '_' || chars[j] == '-')
                {
                    j += 1;
                }
                let word: String = chars[i..j].iter().collect();
                tokens.push(match word.as_str() {
                    "and" => Token::And,
                    "or" => Token::Or,
                    "not" => Token::Not,
                    "in" => Token::In,
                    "true" => Token::True,
                    "false" => Token::False,
                    _ => Token::Ident(word),
                });
                i = j;
            }
            other => bail!("unexpected character '{other}' at column {}", i + 1),
        }
    }
    Ok(tokens)
}

fn lex_string(chars: &[char], start: usize) -> Result<(String, usize)> {
    let mut out = String::new();
    let mut i = start;
    loop {
        let Some(&c) = chars.get(i) else {
            bail!("unterminated string");
        };
        match c {
            '"' => return Ok((out, i + 1)),
            '\\' => match chars.get(i + 1) {
                Some('"') => {
                    out.push('"');
                    i += 2;
                }
                Some('\\') => {
                    out.push('\\');
                    i += 2;
                }
                Some(other) => {
                    bail!("unsupported escape '\\{other}' (only \\\" and \\\\ are allowed)")
                }
                None => bail!("unterminated string"),
            },
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
}

/// A parsed assertion expression.
#[derive(Debug, Clone)]
pub struct Expr {
    root: Node,
}

#[derive(Debug, Clone)]
enum Node {
    Or(Vec<Node>),
    And(Vec<Node>),
    Not(Box<Node>),
    Cmp {
        left: Operand,
        op: CmpOp,
        right: Operand,
    },
    In {
        item: Operand,
        list: Vec<Operand>,
    },
}

#[derive(Debug, Clone)]
enum Operand {
    Path(Path),
    Lit(Lit),
}

#[derive(Debug, Clone)]
struct Path {
    segments: Vec<Seg>,
}

impl Path {
    fn render(&self) -> String {
        let mut out = String::new();
        for seg in &self.segments {
            match seg {
                Seg::Key(k) => {
                    if !out.is_empty() {
                        out.push('.');
                    }
                    out.push_str(k);
                }
                Seg::Index(k) => out.push_str(&format!("[\"{}\"]", escape(k))),
            }
        }
        out
    }

    fn lookup(&self, root: &Value) -> Result<Value> {
        let mut current = root;
        for (i, seg) in self.segments.iter().enumerate() {
            let key = match seg {
                Seg::Key(k) => k.as_str(),
                Seg::Index(k) => k.as_str(),
            };
            let Value::Object(map) = current else {
                bail!(
                    "path '{}' cannot be resolved: the value before segment {i} is a {}, not an object",
                    self.render(),
                    json_kind(current)
                );
            };
            let Some(next) = map.get(key) else {
                bail!(
                    "path '{}' cannot be resolved: no key '{}' at segment {i}",
                    self.render(),
                    key
                );
            };
            current = next;
        }
        Ok(current.clone())
    }
}

#[derive(Debug, Clone)]
enum Seg {
    Key(String),
    Index(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CmpOp {
    Le,
    Ge,
    Eq,
    Ne,
    Lt,
    Gt,
}

impl CmpOp {
    fn render(self) -> &'static str {
        match self {
            Self::Le => "<=",
            Self::Ge => ">=",
            Self::Eq => "==",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Gt => ">",
        }
    }
}

#[derive(Debug, Clone)]
enum Lit {
    Num(f64),
    Str(String),
    Bool(bool),
}

impl Lit {
    fn render(&self) -> String {
        match self {
            Self::Num(n) => serde_json::Value::from(*n).to_string(),
            Self::Str(s) => format!("\"{}\"", escape(s)),
            Self::Bool(b) => b.to_string(),
        }
    }

    fn value(&self) -> Value {
        match self {
            Self::Num(n) => Value::from(*n),
            Self::Str(s) => Value::String(s.clone()),
            Self::Bool(b) => Value::Bool(*b),
        }
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn parse(&mut self) -> Result<Node> {
        if self.tokens.is_empty() {
            bail!("empty expression");
        }
        let node = self.parse_or()?;
        if self.pos != self.tokens.len() {
            bail!("unexpected trailing tokens after the expression");
        }
        Ok(node)
    }

    fn parse_or(&mut self) -> Result<Node> {
        let mut parts = vec![self.parse_and()?];
        while self.eat(&Token::Or) {
            parts.push(self.parse_and()?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            Node::Or(parts)
        })
    }

    fn parse_and(&mut self) -> Result<Node> {
        let mut parts = vec![self.parse_unary()?];
        while self.eat(&Token::And) {
            parts.push(self.parse_unary()?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            Node::And(parts)
        })
    }

    fn parse_unary(&mut self) -> Result<Node> {
        if self.eat(&Token::Not) {
            return Ok(Node::Not(Box::new(self.parse_unary()?)));
        }
        if self.eat(&Token::LParen) {
            let inner = self.parse_or()?;
            if !self.eat(&Token::RParen) {
                bail!("expected ')' to close '('");
            }
            return Ok(inner);
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Node> {
        let left = self.parse_operand()?;
        if self.eat(&Token::In) {
            let list = self.parse_list()?;
            return Ok(Node::In { item: left, list });
        }
        let op = match self.peek() {
            Some(Token::Le) => CmpOp::Le,
            Some(Token::Ge) => CmpOp::Ge,
            Some(Token::Eq) => CmpOp::Eq,
            Some(Token::Ne) => CmpOp::Ne,
            Some(Token::Lt) => CmpOp::Lt,
            Some(Token::Gt) => CmpOp::Gt,
            _ => bail!(
                "expected a comparison operator (==, !=, <=, >=, <, >) or 'in' after the operand; only comparisons produce booleans"
            ),
        };
        self.advance();
        let right = self.parse_operand()?;
        Ok(Node::Cmp { left, op, right })
    }

    fn parse_list(&mut self) -> Result<Vec<Operand>> {
        if !self.eat(&Token::LBracket) {
            bail!("expected '[' to open an 'in' list");
        }
        let mut items = vec![self.parse_operand()?];
        while self.eat(&Token::Comma) {
            items.push(self.parse_operand()?);
        }
        if !self.eat(&Token::RBracket) {
            bail!("expected ']' to close the 'in' list");
        }
        Ok(items)
    }

    fn parse_operand(&mut self) -> Result<Operand> {
        match self.peek().cloned() {
            Some(Token::Number(n)) => {
                self.advance();
                Ok(Operand::Lit(Lit::Num(n)))
            }
            Some(Token::Str(s)) => {
                self.advance();
                Ok(Operand::Lit(Lit::Str(s)))
            }
            Some(Token::True) => {
                self.advance();
                Ok(Operand::Lit(Lit::Bool(true)))
            }
            Some(Token::False) => {
                self.advance();
                Ok(Operand::Lit(Lit::Bool(false)))
            }
            Some(Token::Ident(head)) => {
                self.advance();
                let mut segments = vec![Seg::Key(head)];
                loop {
                    if self.eat(&Token::Dot) {
                        let Some(Token::Ident(next)) = self.peek().cloned() else {
                            bail!("expected a path segment name after '.'");
                        };
                        self.advance();
                        segments.push(Seg::Key(next));
                    } else if self.peek() == Some(&Token::LBracket) {
                        self.advance();
                        let Some(Token::Str(key)) = self.peek().cloned() else {
                            bail!(
                                "expected a quoted string inside [ ] to name a key with spaces or punctuation"
                            );
                        };
                        self.advance();
                        if !self.eat(&Token::RBracket) {
                            bail!("expected ']' after the bracketed key");
                        }
                        segments.push(Seg::Index(key));
                    } else {
                        break;
                    }
                }
                Ok(Operand::Path(Path { segments }))
            }
            Some(other) => bail!("expected a path or literal, found {other:?}"),
            None => bail!("expression ended where an operand was expected"),
        }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) {
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
    }

    fn eat(&mut self, t: &Token) -> bool {
        if self.peek() == Some(t) {
            self.advance();
            true
        } else {
            false
        }
    }
}

pub fn parse(src: &str) -> Result<Expr> {
    let tokens = lex(src).with_context(|| format!("invalid assertion expression: {src}"))?;
    let mut parser = Parser { tokens, pos: 0 };
    let root = parser
        .parse()
        .with_context(|| format!("invalid assertion expression: {src}"))?;
    Ok(Expr { root })
}

/// Result of evaluating an expression against a response envelope.
#[derive(Debug)]
pub struct Outcome {
    pub passed: bool,
    pub failed: Vec<FailedClause>,
}

#[derive(Debug)]
pub struct FailedClause {
    pub clause: String,
    pub expected: String,
    pub actual: String,
}

struct Leaf {
    clause: String,
    passed: bool,
    expected: String,
    actual: String,
}

enum Val<'a> {
    Num(f64),
    Str(&'a str),
    Bool(bool),
}

impl Val<'_> {
    fn kind(&self) -> &'static str {
        match self {
            Self::Num(_) => "number",
            Self::Str(_) => "string",
            Self::Bool(_) => "boolean",
        }
    }
}

fn classify<'a>(value: &'a Value, where_: &str) -> Result<Val<'a>> {
    match value {
        Value::Number(n) => n
            .as_f64()
            .map(Val::Num)
            .ok_or_else(|| anyhow::anyhow!("{where_} is a non-finite number")),
        Value::String(s) => Ok(Val::Str(s.as_str())),
        Value::Bool(b) => Ok(Val::Bool(*b)),
        Value::Null => bail!("{where_} is null; nulls cannot be compared"),
        Value::Object(_) | Value::Array(_) => {
            bail!("{where_} is an object or array; assertions compare scalars")
        }
    }
}
impl Expr {
    pub fn evaluate(&self, envelope: &Value) -> Result<Outcome> {
        let mut leaves = Vec::new();
        let passed = eval_node(&self.root, envelope, &mut leaves)?;
        Ok(Outcome {
            passed,
            failed: leaves
                .into_iter()
                .filter(|leaf| !leaf.passed)
                .map(|leaf| FailedClause {
                    clause: leaf.clause,
                    expected: leaf.expected,
                    actual: leaf.actual,
                })
                .collect(),
        })
    }
}

fn eval_node(node: &Node, envelope: &Value, leaves: &mut Vec<Leaf>) -> Result<bool> {
    match node {
        Node::Or(parts) => {
            let mut any = false;
            for part in parts {
                any |= eval_node(part, envelope, leaves)?;
            }
            Ok(any)
        }
        Node::And(parts) => {
            let mut all = true;
            for part in parts {
                all &= eval_node(part, envelope, leaves)?;
            }
            Ok(all)
        }
        Node::Not(inner) => Ok(!eval_node(inner, envelope, leaves)?),
        Node::Cmp { left, op, right } => {
            let lv = resolve(left, envelope)?;
            let rv = resolve(right, envelope)?;
            let passed = compare(&lv, &rv, *op, &render_operand(left), &render_operand(right))?;
            leaves.push(Leaf {
                clause: format!(
                    "{} {} {}",
                    render_operand(left),
                    op.render(),
                    render_operand(right)
                ),
                passed,
                expected: rv.to_string(),
                actual: lv.to_string(),
            });
            Ok(passed)
        }
        Node::In { item, list } => {
            let clause = render_in(item, list);
            let iv = resolve(item, envelope)?;
            let item_val = classify(&iv, &render_operand(item))
                .with_context(|| format!("in clause '{clause}'"))?;
            let mut matched = false;
            for element in list {
                let ev = resolve(element, envelope)?;
                let ev = classify(&ev, &render_operand(element))
                    .with_context(|| format!("in clause '{clause}'"))?;
                if std::mem::discriminant(&item_val) != std::mem::discriminant(&ev) {
                    bail!(
                        "in clause '{clause}': cannot compare {} with {}",
                        item_val.kind(),
                        ev.kind()
                    );
                }
                matched |= values_equal(&item_val, &ev);
            }
            leaves.push(Leaf {
                clause,
                passed: matched,
                expected: list
                    .iter()
                    .map(render_operand)
                    .collect::<Vec<_>>()
                    .join(", "),
                actual: iv.to_string(),
            });
            Ok(matched)
        }
    }
}

fn render_in(item: &Operand, list: &[Operand]) -> String {
    let items = list
        .iter()
        .map(render_operand)
        .collect::<Vec<_>>()
        .join(", ");
    format!("{} in [{}]", render_operand(item), items)
}

fn values_equal(a: &Val<'_>, b: &Val<'_>) -> bool {
    match (a, b) {
        (Val::Num(x), Val::Num(y)) => x == y,
        (Val::Str(x), Val::Str(y)) => x == y,
        (Val::Bool(x), Val::Bool(y)) => x == y,
        _ => false,
    }
}

fn compare(lv: &Value, rv: &Value, op: CmpOp, left_text: &str, right_text: &str) -> Result<bool> {
    let l = classify(lv, left_text)?;
    let r = classify(rv, right_text)?;
    match op {
        CmpOp::Eq | CmpOp::Ne => {
            let equal = match (&l, &r) {
                (Val::Num(x), Val::Num(y)) => x == y,
                (Val::Str(x), Val::Str(y)) => x == y,
                (Val::Bool(x), Val::Bool(y)) => x == y,
                _ => {
                    bail!(
                        "cannot compare {l_kind} with {r_kind} in '{left_text} {op_text} {right_text}'",
                        l_kind = l.kind(),
                        r_kind = r.kind(),
                        op_text = op.render()
                    )
                }
            };
            Ok(if op == CmpOp::Eq { equal } else { !equal })
        }
        CmpOp::Le | CmpOp::Ge | CmpOp::Lt | CmpOp::Gt => {
            let (Val::Num(x), Val::Num(y)) = (&l, &r) else {
                bail!(
                    "ordering comparison needs numbers in '{left_text} {op_text} {right_text}'",
                    op_text = op.render()
                );
            };
            Ok(match op {
                CmpOp::Le => x <= y,
                CmpOp::Ge => x >= y,
                CmpOp::Lt => x < y,
                CmpOp::Gt => x > y,
                CmpOp::Eq | CmpOp::Ne => unreachable!(),
            })
        }
    }
}

fn resolve(operand: &Operand, envelope: &Value) -> Result<Value> {
    match operand {
        Operand::Lit(l) => Ok(l.value()),
        Operand::Path(p) => p
            .lookup(envelope)
            .with_context(|| format!("while resolving path '{}'", p.render())),
    }
}

fn render_operand(operand: &Operand) -> String {
    match operand {
        Operand::Lit(l) => l.render(),
        Operand::Path(p) => p.render(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env() -> Value {
        json!({
            "model": "jev-latest",
            "answers": {
                "q": {"type": "noul", "noul": 0.93},
                "area": {"type": "choice", "choice": "technical", "probabilities": {"billing": 0.08, "technical": 0.85, "sales": 0.07}, "confidence": 0.82},
                "risk": {"type": "score", "score": 1.6, "probabilities": {"0": 0.05, "1": 0.3, "2": 0.65}, "confidence": 0.78},
                "label": {"type": "choice", "choice": "needs human review"}
            },
            "usage": {"input_tokens": 312, "output_tokens": 48}
        })
    }

    #[test]
    fn parses_and_evaluates_boolean_combinations() {
        let e = parse("answers.q.noul >= 0.8 and answers.area.confidence >= 0.6").unwrap();
        let out = e.evaluate(&env()).unwrap();
        assert!(out.passed);
        assert!(out.failed.is_empty());
    }

    #[test]
    fn precedence_not_over_and_over_or() {
        // A = q.noul >= 0.8 is true, C = confidence >= 0.9 is true here, so
        // "A or B and not C" (A wins) and "(A or B) and not C" (not C wins) differ.
        let mut envelope = env();
        envelope["answers"]["area"]["confidence"] = json!(0.95);
        assert!(parse("answers.q.noul >= 0.8 or answers.q.noul <= 0.1 and not answers.area.confidence >= 0.9")
            .unwrap()
            .evaluate(&envelope)
            .unwrap()
            .passed);
        assert!(!parse("(answers.q.noul >= 0.8 or answers.q.noul <= 0.1) and not answers.area.confidence >= 0.9")
            .unwrap()
            .evaluate(&envelope)
            .unwrap()
            .passed);
    }

    #[test]
    fn choice_equality_and_in_list() {
        let envelope = env();
        assert!(
            parse("answers.area.choice == \"technical\"")
                .unwrap()
                .evaluate(&envelope)
                .unwrap()
                .passed
        );
        assert!(
            parse("answers.area.choice in [\"billing\", \"sales\", \"technical\"]")
                .unwrap()
                .evaluate(&envelope)
                .unwrap()
                .passed
        );
        assert!(
            !parse("answers.area.choice in [\"billing\", \"sales\"]")
                .unwrap()
                .evaluate(&envelope)
                .unwrap()
                .passed
        );
    }

    #[test]
    fn bracketed_keys_reach_spaces_and_hyphens() {
        let envelope = env();
        assert!(
            parse("answers.label.choice == \"needs human review\"")
                .unwrap()
                .evaluate(&envelope)
                .unwrap()
                .passed
        );
        let mut v = env();
        v["answers"]["is-spam"] = json!({"noul": 0.7});
        assert!(
            parse("answers[\"is-spam\"].noul >= 0.5")
                .unwrap()
                .evaluate(&v)
                .unwrap()
                .passed
        );
    }

    #[test]
    fn both_sides_may_be_paths() {
        let e = parse("answers.risk.score >= answers.q.noul").unwrap();
        assert!(e.evaluate(&env()).unwrap().passed);
    }

    #[test]
    fn failed_clauses_report_expected_and_actual() {
        let e = parse("answers.q.noul <= 0.5 and answers.risk.score < 1.0").unwrap();
        let out = e.evaluate(&env()).unwrap();
        assert!(!out.passed);
        assert_eq!(out.failed.len(), 2);
        assert_eq!(out.failed[0].clause, "answers.q.noul <= 0.5");
        assert_eq!(out.failed[0].expected, "0.5");
        assert_eq!(out.failed[0].actual, "0.93");
        assert_eq!(out.failed[1].actual, "1.6");
    }

    #[test]
    fn failed_in_clause_reports_list() {
        let e = parse("answers.area.choice in [\"billing\", \"sales\"]").unwrap();
        let out = e.evaluate(&env()).unwrap();
        assert!(!out.passed);
        assert_eq!(out.failed.len(), 1);
        assert_eq!(out.failed[0].actual, "\"technical\"");
    }

    #[test]
    fn unresolvable_path_is_an_error_not_false() {
        let e = parse("answers.missing.noul >= 0.5").unwrap();
        let err = e.evaluate(&env()).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("answers.missing.noul"));
        assert!(text.contains("no key 'missing'"));
    }

    #[test]
    fn path_through_scalar_is_an_error() {
        let e = parse("answers.q.noul.x >= 0.5").unwrap();
        assert!(e.evaluate(&env()).is_err());
    }

    #[test]
    fn mixed_type_comparison_is_an_error() {
        let e = parse("answers.q.noul == \"technical\"").unwrap();
        let err = e.evaluate(&env()).unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot compare number with string")
        );
    }

    #[test]
    fn ordering_requires_numbers() {
        let e = parse("answers.area.choice >= \"a\"").unwrap();
        assert!(e.evaluate(&env()).is_err());
    }

    #[test]
    fn in_list_mixed_types_are_an_error() {
        let e = parse("answers.area.choice in [\"a\", 3]").unwrap();
        assert!(e.evaluate(&env()).is_err());
    }

    #[test]
    fn rejects_bare_operands_and_bad_operators() {
        assert!(parse("answers.q.noul").is_err());
        assert!(parse("answers.q.noul = 0.5").is_err());
        assert!(parse("").is_err());
        assert!(parse("answers.q.noul >= 0.5 trailing").is_err());
        assert!(parse("answers.q.noul >= 0.5 and").is_err());
        assert!(parse("answers..q").is_err());
    }

    #[test]
    fn usage_fields_are_addressable() {
        let e = parse("usage.input_tokens <= 400").unwrap();
        assert!(e.evaluate(&env()).unwrap().passed);
    }
}
