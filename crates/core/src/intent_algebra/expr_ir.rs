use super::expr::ColumnRef;

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
///
/// `Regex` / `NotRegex` carry PromQL/RE2 regex-match semantics (`=~` / `!~`):
/// the right-hand side is a regular-expression pattern, not a literal value.
/// SQL `LIKE` / `ILIKE` are kept as separate operators because their
/// wildcard grammar (`%` / `_`) differs from regex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// RHS is a regular-expression pattern; matches PromQL `=~`.
    Regex,
    /// RHS is a regular-expression pattern; matches PromQL `!~`.
    NotRegex,
}

// ── Expression IR ─────────────────────────────────────────────────────────────

/// A scalar expression used in filter predicates and (eventually) projection
/// lists and sort keys. Language-independent: PromQL label matchers, SQL
/// `WHERE` conjuncts, and Elastic term filters all lower to the same shape.
///
/// Flat conjunctions (`BoolAnd`) / disjunctions (`BoolOr`) make per-conjunct
/// selectivity estimation and label-matcher lowering straightforward without
/// recursive descent.
#[derive(Debug, Clone, PartialEq)]
pub enum L3Expr {
    /// Reference to a named column. For time-series sources a label name
    /// (e.g. `Column("env")`) or the synthetic sample-value column.
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
}

impl L3Expr {
    /// If this expression is a `BoolAnd`, return its elements; otherwise a
    /// single-element slice containing `self`. Lets callers iterate all
    /// top-level conjuncts without cloning.
    pub fn conjuncts(&self) -> &[L3Expr] {
        match self {
            L3Expr::BoolAnd(v) => v.as_slice(),
            _ => std::slice::from_ref(self),
        }
    }

    /// Recursively collect every `ColumnRef` referenced anywhere in this
    /// expression. Used by L4 for column-lineage and selectivity estimation,
    /// and by schema derivation to discover label columns referenced by a
    /// time-series scan's predicates.
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
            L3Expr::Not(e) => e.columns_referenced(),
        }
    }
}
