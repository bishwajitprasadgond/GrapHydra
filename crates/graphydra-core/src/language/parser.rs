//! Recursive-descent parser for GHQL.
//!
//! The grammar is documented in `docs/query-language.md`. The parser never
//! evaluates anything; it only builds an AST.

use crate::error::{GraphError, Result};
use crate::language::ast::*;
use crate::language::lexer::{Tok, Token, syntax_error, tokenize};
use crate::model::{IndexDef, IndexKind, IndexTarget};
use crate::store::Direction;
use crate::value::Value;

/// Words that cannot be used as unquoted variable names.
const RESERVED: &[&str] = &[
    "MATCH",
    "WHERE",
    "RETURN",
    "CREATE",
    "SET",
    "DELETE",
    "DETACH",
    "REMOVE",
    "ORDER",
    "BY",
    "SKIP",
    "LIMIT",
    "AND",
    "OR",
    "XOR",
    "NOT",
    "IN",
    "IS",
    "NULL",
    "TRUE",
    "FALSE",
    "AS",
    "DISTINCT",
    "ASC",
    "DESC",
    "ASCENDING",
    "DESCENDING",
    "CONTAINS",
    "STARTS",
    "ENDS",
    "WITH",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "EXPLAIN",
    "PROFILE",
    "OPTIONAL",
    "UNION",
    "MERGE",
];

/// Parse exactly one statement (an optional trailing `;` is allowed).
pub fn parse_statement(src: &str) -> Result<Statement> {
    let mut stmts = parse_script(src)?;
    match stmts.len() {
        1 => Ok(stmts.remove(0)),
        0 => Err(syntax_error(src, 0, "empty query")),
        _ => Err(GraphError::semantic("expected a single statement; use execute_script for several")),
    }
}

/// Parse a `;`-separated sequence of statements.
pub fn parse_script(src: &str) -> Result<Vec<Statement>> {
    let tokens = tokenize(src)?;
    let mut p = Parser { src, tokens, pos: 0, depth: 0 };
    let mut out = Vec::new();
    loop {
        while p.eat(&Tok::Semi) {}
        if p.at(&Tok::Eof) {
            break;
        }
        out.push(p.statement()?);
        if !p.eat(&Tok::Semi) && !p.at(&Tok::Eof) {
            return Err(p.error(format!("expected ';' or end of input, found {}", p.peek().describe())));
        }
    }
    Ok(out)
}

/// Maximum nesting depth of expressions, protecting the parser's stack.
const MAX_DEPTH: usize = 64;

struct Parser<'s> {
    src: &'s str,
    tokens: Vec<Token>,
    pos: usize,
    depth: usize,
}

impl<'s> Parser<'s> {
    // ---------------------------------------------------------------
    // Token helpers
    // ---------------------------------------------------------------

    fn peek(&self) -> &Tok {
        &self.tokens[self.pos].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        let i = (self.pos + n).min(self.tokens.len() - 1);
        &self.tokens[i].tok
    }

    fn advance(&mut self) -> Tok {
        let t = self.tokens[self.pos].tok.clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn at(&self, t: &Tok) -> bool {
        self.peek() == t
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.at(t) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: &Tok, what: &str) -> Result<()> {
        if self.eat(t) { Ok(()) } else { Err(self.error(format!("expected {what}, found {}", self.peek().describe()))) }
    }

    fn error(&self, msg: impl Into<String>) -> GraphError {
        syntax_error(self.src, self.tokens[self.pos].start, msg)
    }

    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Word(w) if w.eq_ignore_ascii_case(kw))
    }

    fn is_kw_at(&self, n: usize, kw: &str) -> bool {
        matches!(self.peek_at(n), Tok::Word(w) if w.eq_ignore_ascii_case(kw))
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> Result<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(self.error(format!("expected {kw}, found {}", self.peek().describe())))
        }
    }

    /// Variable name: unreserved word or quoted identifier.
    fn ident(&mut self, what: &str) -> Result<String> {
        match self.peek().clone() {
            Tok::Word(w) if !RESERVED.iter().any(|r| r.eq_ignore_ascii_case(&w)) => {
                self.advance();
                Ok(w)
            }
            Tok::Quoted(w) => {
                self.advance();
                Ok(w)
            }
            other => Err(self.error(format!("expected {what}, found {}", other.describe()))),
        }
    }

    fn at_ident(&self) -> bool {
        match self.peek() {
            Tok::Word(w) => !RESERVED.iter().any(|r| r.eq_ignore_ascii_case(w)),
            Tok::Quoted(_) => true,
            _ => false,
        }
    }

    /// Label, type or property name: any word (keywords allowed) or quoted.
    fn name(&mut self, what: &str) -> Result<String> {
        match self.peek().clone() {
            Tok::Word(w) | Tok::Quoted(w) => {
                self.advance();
                Ok(w)
            }
            other => Err(self.error(format!("expected {what}, found {}", other.describe()))),
        }
    }

    fn prev_end(&self) -> usize {
        if self.pos == 0 { 0 } else { self.tokens[self.pos - 1].end }
    }

    // ---------------------------------------------------------------
    // Statements
    // ---------------------------------------------------------------

    fn statement(&mut self) -> Result<Statement> {
        let explain = if self.eat_kw("EXPLAIN") {
            ExplainMode::Explain
        } else if self.eat_kw("PROFILE") {
            ExplainMode::Profile
        } else {
            ExplainMode::None
        };
        let kind = if self.is_index_ddl("CREATE") {
            self.advance();
            StatementKind::CreateIndex(self.index_def()?)
        } else if self.is_index_ddl("DROP") {
            self.advance();
            StatementKind::DropIndex(self.index_def()?)
        } else if self.is_kw("SHOW") && self.is_kw_at(1, "INDEXES") {
            self.advance();
            self.advance();
            StatementKind::ShowIndexes
        } else {
            StatementKind::Query(Box::new(self.query()?))
        };
        Ok(Statement { explain, kind })
    }

    fn is_index_ddl(&self, verb: &str) -> bool {
        self.is_kw(verb)
            && (self.is_kw_at(1, "INDEX")
                || (["HASH", "SORTED", "TEXT", "FULLTEXT"].iter().any(|k| self.is_kw_at(1, k))
                    && self.is_kw_at(2, "INDEX")))
    }

    fn index_def(&mut self) -> Result<IndexDef> {
        let kind = if self.eat_kw("HASH") {
            IndexKind::Hash
        } else if self.eat_kw("SORTED") {
            IndexKind::Sorted
        } else if self.eat_kw("TEXT") || self.eat_kw("FULLTEXT") {
            IndexKind::Text
        } else {
            IndexKind::Hash
        };
        self.expect_kw("INDEX")?;
        self.expect_kw("ON")?;
        let target = if self.eat_kw("NODES") || self.eat_kw("NODE") {
            IndexTarget::Node
        } else if self.eat_kw("EDGES") || self.eat_kw("EDGE") || self.eat_kw("RELATIONSHIPS") {
            IndexTarget::Edge
        } else {
            return Err(self.error("expected NODES or EDGES"));
        };
        self.expect(&Tok::LParen, "'('")?;
        let property = self.name("property name")?;
        self.expect(&Tok::RParen, "')'")?;
        Ok(IndexDef { kind, target, property })
    }

    fn query(&mut self) -> Result<Query> {
        let mut patterns = Vec::new();
        let mut wheres: Vec<Expr> = Vec::new();
        let mut updates = Vec::new();
        let mut ret = None;
        let start = self.pos;
        loop {
            if self.eat_kw("MATCH") {
                if !updates.is_empty() {
                    return Err(self.error("MATCH must come before update clauses"));
                }
                patterns.extend(self.pattern_list()?);
                if self.eat_kw("WHERE") {
                    wheres.push(self.expr()?);
                }
            } else if self.is_kw("OPTIONAL") {
                return Err(self.error("OPTIONAL MATCH is not supported in this version"));
            } else if self.is_kw("CREATE") {
                self.advance();
                // `CREATE NODE (...)` / `CREATE EDGE (...)` spellings.
                if (self.is_kw("NODE") || self.is_kw("EDGE")) && self.peek_at(1) == &Tok::LParen {
                    self.advance();
                }
                updates.push(UpdateClause::Create(self.pattern_list()?));
            } else if self.eat_kw("SET") {
                updates.push(UpdateClause::Set(self.set_items()?));
            } else if self.eat_kw("REMOVE") {
                updates.push(UpdateClause::Remove(self.remove_items()?));
            } else if self.is_kw("DETACH") || self.is_kw("DELETE") {
                let detach = self.eat_kw("DETACH");
                self.expect_kw("DELETE")?;
                let mut exprs = vec![self.expr()?];
                while self.eat(&Tok::Comma) {
                    exprs.push(self.expr()?);
                }
                updates.push(UpdateClause::Delete { detach, exprs });
            } else if self.eat_kw("RETURN") {
                ret = Some(self.return_clause()?);
                break;
            } else if self.is_kw("WHERE") {
                return Err(self.error("WHERE must follow a MATCH clause"));
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(self
                .error(format!("expected MATCH, CREATE, RETURN or another clause, found {}", self.peek().describe())));
        }
        if ret.is_none() && updates.is_empty() {
            return Err(self.error("query must end with RETURN or contain an update clause"));
        }
        let where_ = wheres.into_iter().reduce(|a, b| Expr::Binary(BinOp::And, Box::new(a), Box::new(b)));
        Ok(Query { patterns, where_, updates, ret })
    }

    fn set_items(&mut self) -> Result<Vec<SetItem>> {
        let mut items = Vec::new();
        loop {
            let var = self.ident("variable")?;
            if self.eat(&Tok::Dot) {
                let key = self.name("property name")?;
                self.expect(&Tok::Eq, "'='")?;
                items.push(SetItem::Property { var, key, value: self.expr()? });
            } else if self.at(&Tok::Colon) {
                while self.eat(&Tok::Colon) {
                    items.push(SetItem::Label { var: var.clone(), label: self.name("label")? });
                }
            } else if self.eat(&Tok::PlusEq) {
                items.push(SetItem::Merge { var, value: self.expr()?, replace: false });
            } else if self.eat(&Tok::Eq) {
                items.push(SetItem::Merge { var, value: self.expr()?, replace: true });
            } else {
                return Err(self.error("expected '.property =', ':Label', '+=' or '=' in SET"));
            }
            if !self.eat(&Tok::Comma) {
                return Ok(items);
            }
        }
    }

    fn remove_items(&mut self) -> Result<Vec<RemoveItem>> {
        let mut items = Vec::new();
        loop {
            let var = self.ident("variable")?;
            if self.eat(&Tok::Dot) {
                items.push(RemoveItem::Property { var, key: self.name("property name")? });
            } else if self.at(&Tok::Colon) {
                while self.eat(&Tok::Colon) {
                    items.push(RemoveItem::Label { var: var.clone(), label: self.name("label")? });
                }
            } else {
                return Err(self.error("expected '.property' or ':Label' in REMOVE"));
            }
            if !self.eat(&Tok::Comma) {
                return Ok(items);
            }
        }
    }

    fn return_clause(&mut self) -> Result<ReturnClause> {
        let distinct = self.eat_kw("DISTINCT");
        let mut star = false;
        let mut items = Vec::new();
        if self.eat(&Tok::Star) {
            star = true;
        } else {
            loop {
                let begin = self.tokens[self.pos].start;
                let expr = self.expr()?;
                let text = self.src[begin..self.prev_end()].trim().to_owned();
                let name = if self.eat_kw("AS") { self.name("column alias")? } else { text };
                items.push(ReturnItem { expr, name });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let mut order_by = Vec::new();
        if self.eat_kw("ORDER") {
            self.expect_kw("BY")?;
            loop {
                let expr = self.expr()?;
                let descending = if self.eat_kw("DESC") || self.eat_kw("DESCENDING") {
                    true
                } else {
                    let _ = self.eat_kw("ASC") || self.eat_kw("ASCENDING");
                    false
                };
                order_by.push(OrderItem { expr, descending });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let skip = if self.eat_kw("SKIP") || self.eat_kw("OFFSET") { Some(self.expr()?) } else { None };
        let limit = if self.eat_kw("LIMIT") { Some(self.expr()?) } else { None };
        Ok(ReturnClause { distinct, star, items, order_by, skip, limit })
    }

    // ---------------------------------------------------------------
    // Patterns
    // ---------------------------------------------------------------

    fn pattern_list(&mut self) -> Result<Vec<Pattern>> {
        let mut v = vec![self.pattern()?];
        while self.eat(&Tok::Comma) {
            v.push(self.pattern()?);
        }
        Ok(v)
    }

    fn pattern(&mut self) -> Result<Pattern> {
        let path_var = if self.at_ident() && self.peek_at(1) == &Tok::Eq {
            let v = self.ident("path variable")?;
            self.advance();
            Some(v)
        } else {
            None
        };
        let shortest = matches!(self.peek(), Tok::Word(w) if w.eq_ignore_ascii_case("shortestPath"))
            && self.peek_at(1) == &Tok::LParen;
        if shortest {
            self.advance();
            self.advance();
        }
        let start = self.node_pattern()?;
        let mut steps = Vec::new();
        while matches!(self.peek(), Tok::Minus | Tok::LArrow) {
            let rel = self.rel_pattern()?;
            let node = self.node_pattern()?;
            steps.push((rel, node));
        }
        if shortest {
            self.expect(&Tok::RParen, "')' closing shortestPath")?;
        }
        Ok(Pattern { path_var, shortest, start, steps })
    }

    fn node_pattern(&mut self) -> Result<NodePattern> {
        self.expect(&Tok::LParen, "'(' starting a node pattern")?;
        let var = if self.at_ident() { Some(self.ident("variable")?) } else { None };
        let mut labels = Vec::new();
        while self.eat(&Tok::Colon) {
            labels.push(self.name("label")?);
        }
        let props = if self.at(&Tok::LBrace) { self.map_entries()? } else { Vec::new() };
        self.expect(&Tok::RParen, "')' closing the node pattern")?;
        Ok(NodePattern { var, labels, props })
    }

    fn rel_pattern(&mut self) -> Result<RelPattern> {
        let left_arrow = match self.advance() {
            Tok::LArrow => true,
            Tok::Minus => false,
            _ => unreachable!("caller checked"),
        };
        let mut rel =
            RelPattern { var: None, types: Vec::new(), direction: Direction::Both, props: Vec::new(), length: None };
        if self.eat(&Tok::LBracket) {
            if self.at_ident() {
                rel.var = Some(self.ident("variable")?);
            }
            if self.eat(&Tok::Colon) {
                rel.types.push(self.name("relationship type")?);
                while self.eat(&Tok::Pipe) {
                    self.eat(&Tok::Colon);
                    rel.types.push(self.name("relationship type")?);
                }
            }
            if self.eat(&Tok::Star) {
                let min = if let Tok::Int(_) = self.peek() { Some(self.small_int()?) } else { None };
                rel.length = Some(if self.eat(&Tok::DotDot) {
                    let max = if let Tok::Int(_) = self.peek() { Some(self.small_int()?) } else { None };
                    (min.unwrap_or(1), max)
                } else {
                    match min {
                        Some(n) => (n, Some(n)),
                        None => (1, None),
                    }
                });
            }
            if self.at(&Tok::LBrace) {
                rel.props = self.map_entries()?;
            }
            self.expect(&Tok::RBracket, "']' closing the relationship pattern")?;
        }
        let right_arrow = match self.peek() {
            Tok::Arrow => true,
            Tok::Minus => false,
            other => {
                return Err(self.error(format!("expected '-' or '->' after relationship, found {}", other.describe())));
            }
        };
        self.advance();
        rel.direction = match (left_arrow, right_arrow) {
            (false, true) => Direction::Outgoing,
            (true, false) => Direction::Incoming,
            (false, false) => Direction::Both,
            (true, true) => return Err(self.error("a relationship cannot point both ways; use '-[...]-'")),
        };
        Ok(rel)
    }

    fn small_int(&mut self) -> Result<u32> {
        match self.advance() {
            Tok::Int(i) if (0..=i64::from(u32::MAX)).contains(&i) => Ok(i as u32),
            _ => Err(self.error("expected a non-negative integer path length")),
        }
    }

    fn map_entries(&mut self) -> Result<Vec<(String, Expr)>> {
        self.expect(&Tok::LBrace, "'{'")?;
        let mut entries: Vec<(String, Expr)> = Vec::new();
        if !self.at(&Tok::RBrace) {
            loop {
                let key = match self.peek().clone() {
                    Tok::Str(s) => {
                        self.advance();
                        s
                    }
                    _ => self.name("map key")?,
                };
                if entries.iter().any(|(k, _)| *k == key) {
                    return Err(self.error(format!("duplicate map key {key:?}")));
                }
                self.expect(&Tok::Colon, "':' after map key")?;
                entries.push((key, self.expr()?));
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(&Tok::RBrace, "'}'")?;
        Ok(entries)
    }

    // ---------------------------------------------------------------
    // Expressions (lowest to highest precedence)
    // ---------------------------------------------------------------

    fn expr(&mut self) -> Result<Expr> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error(format!("expression is nested more than {MAX_DEPTH} levels deep")));
        }
        let e = self.or_expr();
        self.depth -= 1;
        e
    }

    fn or_expr(&mut self) -> Result<Expr> {
        let mut e = self.xor_expr()?;
        while self.eat_kw("OR") {
            e = Expr::Binary(BinOp::Or, Box::new(e), Box::new(self.xor_expr()?));
        }
        Ok(e)
    }

    fn xor_expr(&mut self) -> Result<Expr> {
        let mut e = self.and_expr()?;
        while self.eat_kw("XOR") {
            e = Expr::Binary(BinOp::Xor, Box::new(e), Box::new(self.and_expr()?));
        }
        Ok(e)
    }

    fn and_expr(&mut self) -> Result<Expr> {
        let mut e = self.not_expr()?;
        while self.eat_kw("AND") {
            e = Expr::Binary(BinOp::And, Box::new(e), Box::new(self.not_expr()?));
        }
        Ok(e)
    }

    fn not_expr(&mut self) -> Result<Expr> {
        if self.eat_kw("NOT") {
            return self.nested(|p| p.not_expr()).map(|e| Expr::Unary(UnOp::Not, Box::new(e)));
        }
        self.comparison()
    }

    /// Run `f` one nesting level deeper.
    fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error(format!("expression is nested more than {MAX_DEPTH} levels deep")));
        }
        let r = f(self);
        self.depth -= 1;
        r
    }

    fn comparison(&mut self) -> Result<Expr> {
        let left = self.additive()?;
        let op = match self.peek() {
            Tok::Eq => Some(BinOp::Eq),
            Tok::Neq => Some(BinOp::Neq),
            Tok::Lt => Some(BinOp::Lt),
            Tok::Le => Some(BinOp::Le),
            Tok::Gt => Some(BinOp::Gt),
            Tok::Ge => Some(BinOp::Ge),
            Tok::LArrow => {
                // `a<-1` lexes as `<-`; it means `a < -1`.
                self.advance();
                let first = Expr::Unary(UnOp::Neg, Box::new(self.unary()?));
                let first = fold_neg(first);
                let right = self.additive_from(first)?;
                return Ok(Expr::Binary(BinOp::Lt, Box::new(left), Box::new(right)));
            }
            _ => None,
        };
        if let Some(op) = op {
            self.advance();
            let right = self.additive()?;
            return Ok(Expr::Binary(op, Box::new(left), Box::new(right)));
        }
        if self.eat_kw("IN") {
            return Ok(Expr::Binary(BinOp::In, Box::new(left), Box::new(self.additive()?)));
        }
        if self.eat_kw("CONTAINS") {
            return Ok(Expr::Binary(BinOp::Contains, Box::new(left), Box::new(self.additive()?)));
        }
        if self.is_kw("STARTS") || self.is_kw("ENDS") {
            let op = if self.eat_kw("STARTS") {
                BinOp::StartsWith
            } else {
                self.advance();
                BinOp::EndsWith
            };
            self.expect_kw("WITH")?;
            return Ok(Expr::Binary(op, Box::new(left), Box::new(self.additive()?)));
        }
        if self.eat_kw("IS") {
            let negated = self.eat_kw("NOT");
            self.expect_kw("NULL")?;
            return Ok(Expr::IsNull { expr: Box::new(left), negated });
        }
        Ok(left)
    }

    fn additive(&mut self) -> Result<Expr> {
        let first = self.multiplicative()?;
        self.additive_rest(first)
    }

    fn additive_from(&mut self, first_unary: Expr) -> Result<Expr> {
        let first = self.multiplicative_rest(first_unary)?;
        self.additive_rest(first)
    }

    fn additive_rest(&mut self, mut e: Expr) -> Result<Expr> {
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => return Ok(e),
            };
            self.advance();
            e = Expr::Binary(op, Box::new(e), Box::new(self.multiplicative()?));
        }
    }

    fn multiplicative(&mut self) -> Result<Expr> {
        let first = self.power()?;
        self.multiplicative_rest(first)
    }

    fn multiplicative_rest(&mut self, mut e: Expr) -> Result<Expr> {
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Mod,
                _ => return Ok(e),
            };
            self.advance();
            e = Expr::Binary(op, Box::new(e), Box::new(self.power()?));
        }
    }

    fn power(&mut self) -> Result<Expr> {
        let base = self.unary()?;
        if self.eat(&Tok::Caret) {
            // Right-associative.
            let exp = self.nested(|p| p.power())?;
            return Ok(Expr::Binary(BinOp::Pow, Box::new(base), Box::new(exp)));
        }
        Ok(base)
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.eat(&Tok::Minus) {
            return self.nested(|p| p.unary()).map(|e| fold_neg(Expr::Unary(UnOp::Neg, Box::new(e))));
        }
        if self.eat(&Tok::Plus) {
            return self.nested(|p| p.unary());
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expr> {
        let mut e = self.atom()?;
        loop {
            if self.eat(&Tok::Dot) {
                let key = self.name("property name")?;
                e = Expr::Prop(Box::new(e), key);
            } else if self.eat(&Tok::LBracket) {
                let idx = self.expr()?;
                self.expect(&Tok::RBracket, "']'")?;
                e = Expr::Index(Box::new(e), Box::new(idx));
            } else {
                return Ok(e);
            }
        }
    }

    fn atom(&mut self) -> Result<Expr> {
        match self.peek().clone() {
            Tok::Int(i) => {
                self.advance();
                Ok(Expr::Literal(Value::Int(i)))
            }
            Tok::Float(f) => {
                self.advance();
                Ok(Expr::Literal(Value::Float(f)))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::Literal(Value::String(s)))
            }
            Tok::Param(p) => {
                self.advance();
                Ok(Expr::Param(p))
            }
            Tok::LParen => {
                self.advance();
                let e = self.expr()?;
                self.expect(&Tok::RParen, "')'")?;
                Ok(e)
            }
            Tok::LBracket => {
                self.advance();
                let mut items = Vec::new();
                if !self.at(&Tok::RBracket) {
                    loop {
                        items.push(self.expr()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                }
                self.expect(&Tok::RBracket, "']' closing the list")?;
                Ok(Expr::List(items))
            }
            Tok::LBrace => Ok(Expr::Map(self.map_entries()?)),
            Tok::Word(w) => {
                let upper = w.to_ascii_uppercase();
                match upper.as_str() {
                    "TRUE" => {
                        self.advance();
                        return Ok(Expr::Literal(Value::Bool(true)));
                    }
                    "FALSE" => {
                        self.advance();
                        return Ok(Expr::Literal(Value::Bool(false)));
                    }
                    "NULL" => {
                        self.advance();
                        return Ok(Expr::Literal(Value::Null));
                    }
                    "CASE" => return self.case_expr(),
                    _ => {}
                }
                if self.peek_at(1) == &Tok::LParen {
                    return self.call(w);
                }
                Ok(Expr::Var(self.ident("expression")?))
            }
            Tok::Quoted(_) => Ok(Expr::Var(self.ident("variable")?)),
            other => Err(self.error(format!("expected an expression, found {}", other.describe()))),
        }
    }

    fn call(&mut self, name: String) -> Result<Expr> {
        self.advance(); // name
        self.advance(); // (
        let lname = name.to_lowercase();
        if lname == "count" && self.at(&Tok::Star) {
            self.advance();
            self.expect(&Tok::RParen, "')'")?;
            return Ok(Expr::CountStar);
        }
        let distinct = self.eat_kw("DISTINCT");
        let mut args = Vec::new();
        if !self.at(&Tok::RParen) {
            loop {
                args.push(self.expr()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(&Tok::RParen, "')' closing the function call")?;
        Ok(Expr::Call { name: lname, distinct, args })
    }

    fn case_expr(&mut self) -> Result<Expr> {
        self.expect_kw("CASE")?;
        let operand = if self.is_kw("WHEN") { None } else { Some(Box::new(self.expr()?)) };
        let mut branches = Vec::new();
        while self.eat_kw("WHEN") {
            let w = self.expr()?;
            self.expect_kw("THEN")?;
            branches.push((w, self.expr()?));
        }
        if branches.is_empty() {
            return Err(self.error("CASE needs at least one WHEN branch"));
        }
        let default = if self.eat_kw("ELSE") { Some(Box::new(self.expr()?)) } else { None };
        self.expect_kw("END")?;
        Ok(Expr::Case { operand, branches, default })
    }
}

/// Fold `-literal` into a literal so that `{x: -1}` is a constant.
fn fold_neg(e: Expr) -> Expr {
    match e {
        Expr::Unary(UnOp::Neg, inner) => match *inner {
            Expr::Literal(Value::Int(i)) if i != i64::MIN => Expr::Literal(Value::Int(-i)),
            Expr::Literal(Value::Float(f)) => Expr::Literal(Value::Float(-f)),
            other => Expr::Unary(UnOp::Neg, Box::new(other)),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(src: &str) -> Query {
        match parse_statement(src).expect("parse").kind {
            StatementKind::Query(q) => *q,
            other => panic!("not a query: {other:?}"),
        }
    }

    #[test]
    fn parses_spec_examples() {
        let examples = [
            "MATCH (m:Malware) RETURN m.id, m.family, m.severity LIMIT 100;",
            "MATCH (m:Malware) WHERE m.severity = 'high' RETURN m.id, m.family ORDER BY m.family ASC LIMIT 50;",
            "MATCH (h:Host) WHERE h.risk_score >= 0.80 AND h.hostname CONTAINS 'workstation' RETURN h.id, h.hostname, h.risk_score;",
            "MATCH (h:Host)-[e:DETECTED]->(m:Malware) RETURN h.hostname, e.confidence, m.family;",
            "MATCH (h:Host)-[:DETECTED]->(m:Malware)\n      -[:USES_INDICATOR]->(i:Indicator)\nWHERE h.id = 'host-001'\nRETURN h.hostname, m.family, i.value;",
            "MATCH p = (h:Host)-[:DETECTED|USES_INDICATOR*1..3]->(x) WHERE h.id = 'host-001' RETURN p LIMIT 100;",
            "CREATE NODE (:Host {id: 'host-002', hostname: 'workstation-02', risk_score: 0.35});",
            "MATCH (h:Host), (m:Malware) WHERE h.id = 'host-002' AND m.id = 'malware-001' CREATE EDGE (h)-[:DETECTED {confidence: 0.88}]->(m);",
            "MATCH (h:Host) WHERE h.id = 'host-002' SET h.risk_score = 0.72;",
            "MATCH (h:Host)-[e:DETECTED]->(m:Malware) WHERE h.id = 'host-002' DELETE e;",
            "MATCH (h:Host) WHERE h.id = $host_id RETURN h.hostname, h.risk_score LIMIT 10",
        ];
        for ex in examples {
            parse_statement(ex).unwrap_or_else(|e| panic!("{ex}\n{e}"));
        }
    }

    #[test]
    fn relationship_shapes() {
        let query = q("MATCH (a)<-[r:X*2..]-(b)--(c)-->(d) RETURN a");
        let p = &query.patterns[0];
        assert_eq!(p.steps[0].0.direction, Direction::Incoming);
        assert_eq!(p.steps[0].0.length, Some((2, None)));
        assert_eq!(p.steps[1].0.direction, Direction::Both);
        assert_eq!(p.steps[2].0.direction, Direction::Outgoing);
        let query = q("MATCH (a)-[*3]->(b), (c)-[*..4]-(d) RETURN a");
        assert_eq!(query.patterns[0].steps[0].0.length, Some((3, Some(3))));
        assert_eq!(query.patterns[1].steps[0].0.length, Some((1, Some(4))));
    }

    #[test]
    fn precedence_and_columns() {
        let query = q("MATCH (n) WHERE NOT n.a = 1 OR n.b < -2 AND n.c<-3 RETURN n.a + 2 * 3 AS x, count(*)");
        let Some(Expr::Binary(BinOp::Or, l, r)) = &query.where_ else { panic!("expected OR") };
        assert!(matches!(**l, Expr::Unary(UnOp::Not, _)));
        assert!(matches!(**r, Expr::Binary(BinOp::And, _, _)));
        let ret = query.ret.expect("return");
        assert_eq!(ret.items[0].name, "x");
        assert_eq!(ret.items[1].name, "count(*)");
        assert!(matches!(ret.items[0].expr, Expr::Binary(BinOp::Add, _, _)));
    }

    #[test]
    fn rejects_bad_syntax_with_position() {
        for bad in [
            "MATCH (n RETURN n",
            "MATCH (n) RETURN",
            "MATCH (n)<-[:T]->(m) RETURN n",
            "MATCH (n) WHERE n.x = = 1 RETURN n",
            "RETURN {a: 1, a: 2}",
            "MATCH (n)",
            "MATCH (match) RETURN 1",
            "",
        ] {
            assert!(parse_statement(bad).is_err(), "{bad:?} should fail");
        }
        match parse_statement("MATCH (n)\nWHERE n.x >\nRETURN n") {
            Err(GraphError::Syntax(e)) => assert_eq!(e.line, 3),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn deep_nesting_is_rejected_not_a_stack_overflow() {
        for src in [
            format!("RETURN {}1{}", "(".repeat(100_000), ")".repeat(100_000)),
            format!("RETURN {}1", "-".repeat(100_000)),
            format!("RETURN {}true", "NOT ".repeat(100_000)),
            format!("RETURN 2{}", "^2".repeat(100_000)),
            format!("RETURN {}1{}", "[".repeat(100_000), "]".repeat(100_000)),
        ] {
            assert!(parse_statement(&src).is_err());
        }
        assert!(parse_statement(&format!("RETURN {}1{}", "(".repeat(60), ")".repeat(60))).is_ok());
    }

    #[test]
    fn ddl_statements() {
        let s = parse_statement("CREATE SORTED INDEX ON NODES(risk_score)").expect("ddl");
        assert_eq!(
            s.kind,
            StatementKind::CreateIndex(IndexDef::new(IndexKind::Sorted, IndexTarget::Node, "risk_score"))
        );
        let s = parse_statement("drop text index on edges(note)").expect("ddl");
        assert_eq!(s.kind, StatementKind::DropIndex(IndexDef::new(IndexKind::Text, IndexTarget::Edge, "note")));
        assert_eq!(parse_statement("SHOW INDEXES").expect("show").kind, StatementKind::ShowIndexes);
        assert_eq!(parse_script("RETURN 1; RETURN 2;").expect("script").len(), 2);
    }
}
