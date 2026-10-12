//! Abstract syntax tree for GHQL.

use crate::model::IndexDef;
use crate::store::Direction;
use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplainMode {
    None,
    /// Show the plan without executing.
    Explain,
    /// Execute and attach the plan and statistics.
    Profile,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Statement {
    pub explain: ExplainMode,
    pub kind: StatementKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StatementKind {
    Query(Box<Query>),
    CreateIndex(IndexDef),
    DropIndex(IndexDef),
    ShowIndexes,
}

impl Statement {
    /// True if executing the statement can change the graph.
    pub fn is_mutation(&self) -> bool {
        match &self.kind {
            StatementKind::Query(q) => !q.updates.is_empty(),
            StatementKind::CreateIndex(_) | StatementKind::DropIndex(_) => true,
            StatementKind::ShowIndexes => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    /// All `MATCH` patterns (multiple `MATCH` clauses are merged).
    pub patterns: Vec<Pattern>,
    /// Conjunction of all `WHERE` clauses.
    pub where_: Option<Expr>,
    pub updates: Vec<UpdateClause>,
    pub ret: Option<ReturnClause>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    /// `p = (...)` path variable.
    pub path_var: Option<String>,
    pub shortest: bool,
    pub start: NodePattern,
    pub steps: Vec<(RelPattern, NodePattern)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodePattern {
    pub var: Option<String>,
    pub labels: Vec<String>,
    pub props: Vec<(String, Expr)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelPattern {
    pub var: Option<String>,
    pub types: Vec<String>,
    /// Direction from the left node to the right node.
    pub direction: Direction,
    pub props: Vec<(String, Expr)>,
    /// `*min..max`; `None` for a single-hop relationship.
    pub length: Option<(u32, Option<u32>)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UpdateClause {
    Create(Vec<Pattern>),
    Set(Vec<SetItem>),
    Remove(Vec<RemoveItem>),
    Delete { detach: bool, exprs: Vec<Expr> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetItem {
    /// `n.key = expr`
    Property { var: String, key: String, value: Expr },
    /// `n += {map}` merges properties; `n = {map}` replaces them.
    Merge { var: String, value: Expr, replace: bool },
    /// `n:Label`
    Label { var: String, label: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum RemoveItem {
    Property { var: String, key: String },
    Label { var: String, label: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReturnClause {
    pub distinct: bool,
    /// `RETURN *`
    pub star: bool,
    pub items: Vec<ReturnItem>,
    pub order_by: Vec<OrderItem>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReturnItem {
    pub expr: Expr,
    /// Column name: the alias, or the expression's source text.
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    pub expr: Expr,
    pub descending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Or,
    Xor,
    And,
    Eq,
    Neq,
    Lt,
    Le,
    Gt,
    Ge,
    In,
    Contains,
    StartsWith,
    EndsWith,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Not,
    Neg,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Value),
    Param(String),
    Var(String),
    Prop(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    List(Vec<Expr>),
    Map(Vec<(String, Expr)>),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    /// Function call; `name` is lower-cased.
    Call {
        name: String,
        distinct: bool,
        args: Vec<Expr>,
    },
    CountStar,
    Case {
        operand: Option<Box<Expr>>,
        branches: Vec<(Expr, Expr)>,
        default: Option<Box<Expr>>,
    },
}

impl Expr {
    pub(crate) fn walk<'a>(&'a self, f: &mut impl FnMut(&'a Expr)) {
        f(self);
        match self {
            Expr::Prop(e, _) | Expr::Unary(_, e) | Expr::IsNull { expr: e, .. } => e.walk(f),
            Expr::Index(a, b) | Expr::Binary(_, a, b) => {
                a.walk(f);
                b.walk(f);
            }
            Expr::List(items) | Expr::Call { args: items, .. } => items.iter().for_each(|e| e.walk(f)),
            Expr::Map(entries) => entries.iter().for_each(|(_, e)| e.walk(f)),
            Expr::Case { operand, branches, default } => {
                if let Some(o) = operand {
                    o.walk(f);
                }
                for (w, t) in branches {
                    w.walk(f);
                    t.walk(f);
                }
                if let Some(d) = default {
                    d.walk(f);
                }
            }
            Expr::Literal(_) | Expr::Param(_) | Expr::Var(_) | Expr::CountStar => {}
        }
    }

    pub(crate) fn contains_aggregate(&self) -> bool {
        let mut found = false;
        self.walk(&mut |e| {
            if e.is_aggregate_call() {
                found = true;
            }
        });
        found
    }

    pub(crate) fn is_aggregate_call(&self) -> bool {
        match self {
            Expr::CountStar => true,
            Expr::Call { name, .. } => is_aggregate_name(name),
            _ => false,
        }
    }

    /// True for literals, parameters and lists/maps of them.
    pub(crate) fn is_constant(&self) -> bool {
        match self {
            Expr::Literal(_) | Expr::Param(_) => true,
            Expr::List(items) => items.iter().all(Expr::is_constant),
            Expr::Map(entries) => entries.iter().all(|(_, e)| e.is_constant()),
            Expr::Unary(UnOp::Neg, e) => e.is_constant(),
            _ => false,
        }
    }
}

pub(crate) fn is_aggregate_name(name: &str) -> bool {
    matches!(name, "count" | "sum" | "avg" | "min" | "max" | "collect")
}
