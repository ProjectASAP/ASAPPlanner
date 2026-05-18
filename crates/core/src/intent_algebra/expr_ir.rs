use super::expr::ColumnRef;
use super::schema::L3DataType;

// ── Scalar literals ───────────────────────────────────────────────────────────

/// A typed scalar constant. Used in `L3Expr::Literal`.
#[derive(Debug, Clone, PartialEq)]
pub enum L3Scalar {
    Int64(i64),
    Float64(f64),
    Utf8(String),
    Boolean(bool),
    Null,
}

// ── Comparison operators ──────────────────────────────────────────────────────

/// Binary comparison operators for `L3Expr::Compare`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Like,
    NotLike,
    ILike,
    NotILike,
}

// ── Arithmetic operators ──────────────────────────────────────────────────────

/// Binary arithmetic operators for `L3Expr::Arith`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

// ── Expression IR ─────────────────────────────────────────────────────────────

/// A scalar expression used in filter predicates, projection lists, and sort
/// keys. Flat conjunctions (`BoolAnd`) and disjunctions (`BoolOr`) make
/// per-conjunct selectivity estimation and PromQL label-matcher lowering
/// straightforward without recursive descent.
#[derive(Debug, Clone, PartialEq)]
pub enum L3Expr {
    /// Reference to a named column.
    Column(ColumnRef),
    /// A constant literal value.
    Literal(L3Scalar),
    /// `left op right` — binary comparison.
    Compare {
        left: Box<L3Expr>,
        op: CompareOp,
        right: Box<L3Expr>,
    },
    /// Flat conjunction (logical AND). An empty list is vacuously true.
    BoolAnd(Vec<L3Expr>),
    /// Flat disjunction (logical OR). An empty list is vacuously false.
    BoolOr(Vec<L3Expr>),
    /// Logical NOT.
    Not(Box<L3Expr>),
    /// `expr IS NULL`.
    IsNull(Box<L3Expr>),
    /// `expr IS NOT NULL`.
    IsNotNull(Box<L3Expr>),
    /// `CAST(expr AS to)`.
    Cast { expr: Box<L3Expr>, to: L3DataType },
    /// `expr [NOT] IN (v1, v2, …)`.
    InList {
        expr: Box<L3Expr>,
        list: Vec<L3Expr>,
        negated: bool,
    },
    /// Scalar function call, e.g. `LOWER(col)`, `ABS(x)`.
    FunctionCall { name: String, args: Vec<L3Expr> },
    /// Binary arithmetic: `left op right`.
    Arith {
        op: ArithOp,
        left: Box<L3Expr>,
        right: Box<L3Expr>,
    },
    /// SQL `CASE` expression (both searched and simple forms).
    /// `operand` is present for simple CASE (`CASE expr WHEN ...`),
    /// absent for searched CASE (`CASE WHEN condition THEN ...`).
    Case {
        operand: Option<Box<L3Expr>>,
        branches: Vec<(L3Expr, L3Expr)>,
        else_expr: Option<Box<L3Expr>>,
    },
}

impl L3Expr {
    /// If this expression is a `BoolAnd`, return its elements.
    /// Otherwise return a single-element slice containing `self`.
    /// Lets callers iterate all top-level conjuncts without cloning.
    pub fn conjuncts(&self) -> &[L3Expr] {
        match self {
            L3Expr::BoolAnd(v) => v.as_slice(),
            _ => std::slice::from_ref(self),
        }
    }

    /// If this expression is a `BoolOr`, return its elements.
    /// Otherwise return a single-element slice containing `self`.
    pub fn disjuncts(&self) -> &[L3Expr] {
        match self {
            L3Expr::BoolOr(v) => v.as_slice(),
            _ => std::slice::from_ref(self),
        }
    }

    /// Recursively collect all `ColumnRef`s referenced anywhere in this
    /// expression. Used by L4 for column-lineage and selectivity estimation.
    pub fn columns_referenced(&self) -> Vec<&ColumnRef> {
        match self {
            L3Expr::Column(c) => vec![c],
            L3Expr::Literal(_) => vec![],
            L3Expr::Compare { left, right, .. } => {
                let mut v = left.columns_referenced();
                v.extend(right.columns_referenced());
                v
            }
            L3Expr::BoolAnd(parts) | L3Expr::BoolOr(parts) => {
                parts.iter().flat_map(|e| e.columns_referenced()).collect()
            }
            L3Expr::Not(e) | L3Expr::IsNull(e) | L3Expr::IsNotNull(e) => e.columns_referenced(),
            L3Expr::Cast { expr, .. } => expr.columns_referenced(),
            L3Expr::InList { expr, list, .. } => {
                let mut v = expr.columns_referenced();
                v.extend(list.iter().flat_map(|e| e.columns_referenced()));
                v
            }
            L3Expr::FunctionCall { args, .. } => {
                args.iter().flat_map(|e| e.columns_referenced()).collect()
            }
            L3Expr::Arith { left, right, .. } => {
                let mut v = left.columns_referenced();
                v.extend(right.columns_referenced());
                v
            }
            L3Expr::Case {
                operand,
                branches,
                else_expr,
            } => {
                let mut v = vec![];
                if let Some(op) = operand {
                    v.extend(op.columns_referenced());
                }
                for (when, then) in branches {
                    v.extend(when.columns_referenced());
                    v.extend(then.columns_referenced());
                }
                if let Some(e) = else_expr {
                    v.extend(e.columns_referenced());
                }
                v
            }
        }
    }
}
