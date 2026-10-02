//! Ordinary (non-ASAP) query operators: everything a front end emits and
//! everything that survives ASAP optimization unchanged.

use std::rc::Rc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::node::{OperatorNode, OperatorResultKind};
use super::scalar::{Predicate, ProjectItem, ScalarExpr, SortKey};
use crate::pre_asap::agg_intent::AggIntent;
use crate::pre_asap::schema::{ColumnId, DataType, Field, FieldDataType, Schema};
use crate::pre_asap::vocabulary::{
    aggregate_output_schema, BinaryOpKind, ConcatDiscriminatorKey, GroupKeys, InfoMatcher,
    JoinKind, QueryExprError, Reduction, RelationalSetOpKind, SampleKind, Source, TimeShift,
    VectorMatch, WindowFrame, WindowFuncKind,
};

/// All semantics owned by a binary operator.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BinaryOperator {
    /// Execute division only for finite operands, a nonzero divisor, and a
    /// normal finite result; otherwise use exact execution. Required by the
    /// relative-value division certificate, including floating-point range.
    #[serde(default)]
    pub checked_relative_division: bool,
    /// Conditional exact rewrites (such as temporal average from sum/count)
    /// require finite operands and quotient. Zero/subnormal results are valid;
    /// overflow must fall back to the original query rather than emit infinity.
    #[serde(default)]
    pub checked_finite_division: bool,
    pub kind: BinaryOpKind,
    /// `None` is the only currently supported vector/vector matching mode.
    /// The field is retained so execution never has to recover semantics by
    /// re-parsing PromQL.
    pub vector_match: Option<VectorMatch>,
}

/// Which samples a PromQL selector reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeRangeKind {
    /// An instant selector: `range` is the lookback horizon and the latest
    /// eligible sample per series is selected.
    Instant,
    /// A range selector (`m[5m]`): every sample in the window.
    Range,
}

/// The non-ASAP operator vocabulary. Children are [`Rc<OperatorNode>`], so an
/// ordinary operator can read a summary readout and a summary can read any
/// relational subtree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NonASAPOp {
    /// Leaf. `schema` is the binding schema every positional `ColumnId` in
    /// the tree indexes into; `predicates` are leaf-level row filters (PromQL
    /// label matchers, pushed-down `WHERE` conjuncts).
    Scan {
        source: Source,
        #[serde(default)]
        predicates: Vec<Predicate>,
        schema: Schema,
    },
    /// SQL `VALUES` rows, or the one empty row of a `SELECT` without `FROM`.
    /// Row expressions have no input-column scope.
    Values {
        rows: Vec<Vec<ScalarExpr>>,
        schema: Schema,
    },
    /// σ — row-level filter. Output schema = child schema.
    Filter {
        pred: Predicate,
        child: Rc<OperatorNode>,
    },
    /// π — projection.
    Project {
        cols: Vec<ProjectItem>,
        /// Re-qualifies every output column with this table alias (a derived
        /// table / inline view). `None` for an ordinary SELECT list.
        #[serde(default)]
        qualifier: Option<String>,
        child: Rc<OperatorNode>,
    },
    /// γ + α — grouping + aggregate intents.
    Aggregate {
        reduction: Reduction,
        measures: Vec<AggIntent>,
        /// Output column names parallel to `measures`; an empty entry falls
        /// back to the intent's synthetic name.
        #[serde(default)]
        output_names: Vec<String>,
        #[serde(default)]
        having: Option<Predicate>,
        child: Rc<OperatorNode>,
    },
    Join {
        kind: JoinKind,
        pred: Predicate,
        left: Rc<OperatorNode>,
        right: Rc<OperatorNode>,
    },
    SetOp {
        kind: RelationalSetOpKind,
        all: bool,
        left: Rc<OperatorNode>,
        right: Rc<OperatorNode>,
    },
    /// ⊕ — exact n-ary `UNION ALL` of union-compatible branches. The output
    /// schema is the first child's.
    Concat {
        children: Vec<Rc<OperatorNode>>,
        #[serde(default)]
        discriminator_unique_key: Option<ConcatDiscriminatorKey>,
    },
    /// δ — deduplication; empty `cols` = all columns.
    Dedup {
        cols: Vec<ColumnId>,
        child: Rc<OperatorNode>,
    },
    /// Order-by, per `partition_by` group when non-empty.
    Sort {
        keys: Vec<SortKey>,
        #[serde(default)]
        partition_by: GroupKeys,
        child: Rc<OperatorNode>,
    },
    /// Row selection; `n = None` is offset-only. `partition_by` applies the
    /// limit per group (PromQL `topk by (..)`).
    Limit {
        n: Option<usize>,
        offset: usize,
        #[serde(default)]
        partition_by: GroupKeys,
        child: Rc<OperatorNode>,
    },
    /// Arithmetic / comparison / set composition of two operands (PromQL
    /// binary operators). A scalar operand is a [`Self::ScalarBridge`] leaf.
    BinaryOp {
        operator: BinaryOperator,
        /// PromQL `bool` modifier: a comparison returns `0`/`1` instead of
        /// filtering. Valid only for comparison operators.
        #[serde(default)]
        return_bool: bool,
        lhs: Rc<OperatorNode>,
        rhs: Rc<OperatorNode>,
    },
    /// SQL analytic window function. Output schema = child schema + one
    /// column named `output_name`.
    SQLWindowFunc {
        func: WindowFuncKind,
        args: Vec<ScalarExpr>,
        partition_by: GroupKeys,
        order_by: Vec<SortKey>,
        #[serde(default)]
        frame: Option<WindowFrame>,
        output_name: String,
        child: Rc<OperatorNode>,
    },
    /// Temporal selection over a time-series input.
    TimeRange {
        range: Duration,
        kind: TimeRangeKind,
        child: Rc<OperatorNode>,
    },
    /// PromQL `offset` / `@`: moves when `child` is evaluated.
    TimeShift {
        shift: TimeShift,
        child: Rc<OperatorNode>,
    },
    /// PromQL `vector(s)`: a label-less instant vector carrying a scalar.
    PromqlVectorFromScalar(ScalarExpr),
    /// ρ — PromQL `label_replace` / `label_join`.
    PromqlRelabel {
        dst: String,
        value: ScalarExpr,
        child: Rc<OperatorNode>,
    },
    /// PromQL `info(v, selector)` label enrichment.
    PromqlInfoEnrich {
        #[serde(default)]
        selector: Vec<InfoMatcher>,
        child: Rc<OperatorNode>,
    },
    /// PromQL `limitk` / `limit_ratio`.
    PromqlSeriesSample {
        #[serde(default)]
        by: GroupKeys,
        kind: SampleKind,
        child: Rc<OperatorNode>,
    },
    /// PromQL subquery `<expr>[range:resolution]`.
    PromqlSubquery {
        range: Duration,
        #[serde(default)]
        resolution: Option<Duration>,
        child: Rc<OperatorNode>,
    },
    /// A scalar expression at an operator position: a bare PromQL scalar
    /// query (`5`, `time()`) or the scalar operand of `<vector> op <scalar>`.
    /// Its output is one `value` column with no series.
    ScalarBridge(ScalarExpr),
}

impl NonASAPOp {
    /// The direct operator inputs, in field order, followed by the operator
    /// nodes referenced from this operator's scalar expressions.
    pub fn children(&self) -> Vec<&Rc<OperatorNode>> {
        use NonASAPOp::*;
        let mut out: Vec<&Rc<OperatorNode>> = match self {
            Scan { .. } | Values { .. } | PromqlVectorFromScalar(_) | ScalarBridge(_) => vec![],
            Filter { child, .. }
            | Project { child, .. }
            | Aggregate { child, .. }
            | Dedup { child, .. }
            | Sort { child, .. }
            | Limit { child, .. }
            | SQLWindowFunc { child, .. }
            | TimeRange { child, .. }
            | TimeShift { child, .. }
            | PromqlRelabel { child, .. }
            | PromqlInfoEnrich { child, .. }
            | PromqlSeriesSample { child, .. }
            | PromqlSubquery { child, .. } => vec![child],
            Join { left, right, .. } | SetOp { left, right, .. } => vec![left, right],
            BinaryOp { lhs, rhs, .. } => vec![lhs, rhs],
            Concat { children, .. } => children.iter().collect(),
        };
        for expr in self.scalar_exprs() {
            out.extend(expr.operator_refs());
        }
        out
    }

    /// Every scalar expression this operator owns.
    pub fn scalar_exprs(&self) -> Vec<&ScalarExpr> {
        use NonASAPOp::*;
        match self {
            Scan { predicates, .. } => predicates.iter().map(|p| &p.0).collect(),
            Values { rows, .. } => rows.iter().flatten().collect(),
            Filter { pred, .. } | Join { pred, .. } => vec![&pred.0],
            Project { cols, .. } => cols.iter().map(|c| &c.expr).collect(),
            Aggregate { having, .. } => having.iter().map(|p| &p.0).collect(),
            Sort { keys, .. } => keys.iter().map(|k| &k.expr).collect(),
            SQLWindowFunc { args, order_by, .. } => args
                .iter()
                .chain(order_by.iter().map(|k| &k.expr))
                .collect(),
            PromqlVectorFromScalar(e) | ScalarBridge(e) => vec![e],
            PromqlRelabel { value, .. } => vec![value],
            SetOp { .. }
            | Concat { .. }
            | Dedup { .. }
            | Limit { .. }
            | BinaryOp { .. }
            | TimeRange { .. }
            | TimeShift { .. }
            | PromqlInfoEnrich { .. }
            | PromqlSeriesSample { .. }
            | PromqlSubquery { .. } => vec![],
        }
    }

    /// Rebuild this operator with `f` applied to every child, including the
    /// operator nodes referenced from scalar expressions. Every other field
    /// is cloned.
    pub fn map_children(&self, mut f: impl FnMut(&Rc<OperatorNode>) -> Rc<OperatorNode>) -> Self {
        use NonASAPOp::*;
        let mut map_scalar = |e: &ScalarExpr| e.map_operator_refs(&mut f);
        fn map_pred(p: &Predicate, f: &mut impl FnMut(&ScalarExpr) -> ScalarExpr) -> Predicate {
            Predicate(f(&p.0))
        }
        fn map_keys(
            keys: &[SortKey],
            f: &mut impl FnMut(&ScalarExpr) -> ScalarExpr,
        ) -> Vec<SortKey> {
            keys.iter()
                .map(|k| SortKey {
                    expr: f(&k.expr),
                    ascending: k.ascending,
                    nulls_first: k.nulls_first,
                })
                .collect()
        }
        match self {
            Scan {
                source,
                predicates,
                schema,
            } => Scan {
                source: source.clone(),
                predicates: predicates
                    .iter()
                    .map(|p| map_pred(p, &mut map_scalar))
                    .collect(),
                schema: schema.clone(),
            },
            Values { rows, schema } => Values {
                rows: rows
                    .iter()
                    .map(|row| row.iter().map(&mut map_scalar).collect())
                    .collect(),
                schema: schema.clone(),
            },
            PromqlVectorFromScalar(e) => PromqlVectorFromScalar(map_scalar(e)),
            ScalarBridge(e) => ScalarBridge(map_scalar(e)),
            Filter { pred, child } => {
                let pred = map_pred(pred, &mut map_scalar);
                Filter {
                    pred,
                    child: f(child),
                }
            }
            Project {
                cols,
                qualifier,
                child,
            } => {
                let cols = cols
                    .iter()
                    .map(|c| ProjectItem {
                        alias: c.alias.clone(),
                        expr: map_scalar(&c.expr),
                    })
                    .collect();
                Project {
                    cols,
                    qualifier: qualifier.clone(),
                    child: f(child),
                }
            }
            Aggregate {
                reduction,
                measures,
                output_names,
                having,
                child,
            } => {
                let having = having.as_ref().map(|p| map_pred(p, &mut map_scalar));
                Aggregate {
                    reduction: reduction.clone(),
                    measures: measures.clone(),
                    output_names: output_names.clone(),
                    having,
                    child: f(child),
                }
            }
            Join {
                kind,
                pred,
                left,
                right,
            } => {
                let pred = map_pred(pred, &mut map_scalar);
                Join {
                    kind: kind.clone(),
                    pred,
                    left: f(left),
                    right: f(right),
                }
            }
            SetOp {
                kind,
                all,
                left,
                right,
            } => SetOp {
                kind: kind.clone(),
                all: *all,
                left: f(left),
                right: f(right),
            },
            Concat {
                children,
                discriminator_unique_key,
            } => Concat {
                children: children.iter().map(&mut f).collect(),
                discriminator_unique_key: discriminator_unique_key.clone(),
            },
            Dedup { cols, child } => Dedup {
                cols: cols.clone(),
                child: f(child),
            },
            Sort {
                keys,
                partition_by,
                child,
            } => {
                let keys = map_keys(keys, &mut map_scalar);
                Sort {
                    keys,
                    partition_by: partition_by.clone(),
                    child: f(child),
                }
            }
            Limit {
                n,
                offset,
                partition_by,
                child,
            } => Limit {
                n: *n,
                offset: *offset,
                partition_by: partition_by.clone(),
                child: f(child),
            },
            BinaryOp {
                operator,
                return_bool,
                lhs,
                rhs,
            } => BinaryOp {
                operator: operator.clone(),
                return_bool: *return_bool,
                lhs: f(lhs),
                rhs: f(rhs),
            },
            SQLWindowFunc {
                func,
                args,
                partition_by,
                order_by,
                frame,
                output_name,
                child,
            } => {
                let args = args.iter().map(&mut map_scalar).collect();
                let order_by = map_keys(order_by, &mut map_scalar);
                SQLWindowFunc {
                    func: func.clone(),
                    args,
                    partition_by: partition_by.clone(),
                    order_by,
                    frame: frame.clone(),
                    output_name: output_name.clone(),
                    child: f(child),
                }
            }
            TimeRange { range, kind, child } => TimeRange {
                range: *range,
                kind: *kind,
                child: f(child),
            },
            TimeShift { shift, child } => TimeShift {
                shift: *shift,
                child: f(child),
            },
            PromqlRelabel { dst, value, child } => {
                let value = map_scalar(value);
                PromqlRelabel {
                    dst: dst.clone(),
                    value,
                    child: f(child),
                }
            }
            PromqlInfoEnrich { selector, child } => PromqlInfoEnrich {
                selector: selector.clone(),
                child: f(child),
            },
            PromqlSeriesSample { by, kind, child } => PromqlSeriesSample {
                by: by.clone(),
                kind: *kind,
                child: f(child),
            },
            PromqlSubquery {
                range,
                resolution,
                child,
            } => PromqlSubquery {
                range: *range,
                resolution: *resolution,
                child: f(child),
            },
        }
    }

    /// The variant name, for diagnostics and export.
    pub fn kind_name(&self) -> &'static str {
        use NonASAPOp::*;
        match self {
            Scan { .. } => "Scan",
            Values { .. } => "Values",
            Filter { .. } => "Filter",
            Project { .. } => "Project",
            Aggregate { .. } => "Aggregate",
            Join { .. } => "Join",
            SetOp { .. } => "SetOp",
            Concat { .. } => "Concat",
            Dedup { .. } => "Dedup",
            Sort { .. } => "Sort",
            Limit { .. } => "Limit",
            BinaryOp { .. } => "BinaryOp",
            SQLWindowFunc { .. } => "SQLWindowFunc",
            TimeRange { .. } => "TimeRange",
            TimeShift { .. } => "TimeShift",
            PromqlVectorFromScalar(_) => "PromqlVectorFromScalar",
            PromqlRelabel { .. } => "PromqlRelabel",
            PromqlInfoEnrich { .. } => "PromqlInfoEnrich",
            PromqlSeriesSample { .. } => "PromqlSeriesSample",
            PromqlSubquery { .. } => "PromqlSubquery",
            ScalarBridge(_) => "ScalarBridge",
        }
    }

    /// Whether this operator is a scalar-valued leaf (`ScalarBridge`): the
    /// scalar operand of a PromQL `<vector> op <scalar>`.
    pub fn is_scalar_leaf(&self) -> bool {
        matches!(self, NonASAPOp::ScalarBridge(_))
    }

    /// The one-column schema of a scalar leaf.
    fn scalar_schema(dtype: DataType) -> Schema {
        Schema {
            fields: vec![Field::plain("value", dtype, false)],
            time_index: None,
            unique_keys: Vec::new(),
            closed: true,
        }
    }

    /// Output schema derived from this operator's parameters and its
    /// children's (already derived) schemas.
    pub fn output_schema(&self) -> Result<Schema, QueryExprError> {
        use NonASAPOp::*;
        Ok(match self {
            Scan { schema, .. } | Values { schema, .. } => schema.clone(),

            Aggregate {
                reduction,
                measures,
                output_names,
                child,
                ..
            } => aggregate_output_schema(&child.schema, reduction, measures, output_names)?,

            Filter { child, .. }
            | Sort { child, .. }
            | Limit { child, .. }
            | PromqlSubquery { child, .. }
            | PromqlSeriesSample { child, .. }
            | PromqlInfoEnrich { child, .. }
            | TimeRange { child, .. }
            | TimeShift { child, .. } => child.schema.clone(),

            // ρ — relabel preserves every input column and writes one label
            // `dst` (Utf8): overwritten in place if it already exists, else
            // appended (nullable). Row-uniqueness is no longer provable.
            PromqlRelabel { dst, child, .. } => {
                let mut out = child.schema.clone();
                if let Some(existing) = out.fields.iter_mut().find(|c| c.name == *dst) {
                    existing.dtype = FieldDataType::Plain(DataType::Utf8);
                    existing.nullable = true;
                } else {
                    out.fields
                        .push(Field::plain(dst.clone(), DataType::Utf8, true));
                }
                out.unique_keys.clear();
                out
            }

            // π — one output column per projection item. A bare column item
            // keeps its field verbatim, so an `ExactAggregate` state column
            // can pass through a projection unchanged; any other expression
            // is typed against the input and must read plain values.
            Project {
                cols,
                qualifier,
                child,
            } => {
                let in_schema = &child.schema;
                let fields: Vec<Field> = cols
                    .iter()
                    .enumerate()
                    .map(|(i, item)| {
                        let mut field = match &item.expr {
                            ScalarExpr::Column(id) if in_schema.fields.get(*id).is_some() => {
                                let mut f = in_schema.fields[*id].clone();
                                f.table = None;
                                f
                            }
                            expr => {
                                let (dtype, nullable) = expr.scalar_type(in_schema)?;
                                Field::plain(String::new(), dtype, nullable)
                            }
                        };
                        field.name = item
                            .alias
                            .clone()
                            .unwrap_or_else(|| default_proj_name(&item.expr, i, in_schema));
                        Ok(match qualifier {
                            Some(q) => field.with_table(q),
                            None => field,
                        })
                    })
                    .collect::<Result<Vec<_>, QueryExprError>>()?;
                let time_index = fields.iter().position(|c| c.name == "ts");
                let unique_keys = in_schema
                    .unique_keys
                    .iter()
                    .filter_map(|key| {
                        key.iter()
                            .map(|input_col| {
                                cols.iter().position(|item| {
                                    matches!(&item.expr, ScalarExpr::Column(col) if col == input_col)
                                })
                            })
                            .collect::<Option<Vec<_>>>()
                    })
                    .collect();
                Schema {
                    fields,
                    time_index,
                    unique_keys,
                    closed: true,
                }
            }

            Dedup { cols, child } => {
                let mut out = child.schema.clone();
                if !cols.is_empty() {
                    out.add_unique_key(cols.clone());
                }
                out
            }

            Concat {
                children,
                discriminator_unique_key,
            } => {
                let mut s = children
                    .first()
                    .ok_or(QueryExprError::EmptyConcat)?
                    .schema
                    .clone();
                s.unique_keys.clear();
                if let Some(key) = discriminator_unique_key {
                    let mut compound = vec![*key.discriminator()];
                    compound.extend(key.inner_key().iter().copied());
                    s.add_unique_key(compound);
                }
                s
            }
            SetOp { left, .. } => {
                let mut s = left.schema.clone();
                s.unique_keys.clear();
                s
            }
            Join {
                kind, left, right, ..
            } => {
                let l = &left.schema;
                let r = &right.schema;
                if matches!(kind, JoinKind::Semi | JoinKind::Anti) {
                    return Ok(Schema {
                        unique_keys: Vec::new(),
                        ..l.clone()
                    });
                }
                let (left_null, right_null) = match kind {
                    JoinKind::Left => (false, true),
                    JoinKind::Right => (true, false),
                    JoinKind::Full => (true, true),
                    JoinKind::Inner | JoinKind::Cross => (false, false),
                    JoinKind::Semi | JoinKind::Anti => unreachable!("handled above"),
                };
                let l_len = l.fields.len();
                let mut fields = Vec::with_capacity(l_len + r.fields.len());
                fields.extend(l.fields.iter().cloned().map(|mut c| {
                    c.nullable |= left_null;
                    c
                }));
                fields.extend(r.fields.iter().cloned().map(|mut c| {
                    c.nullable |= right_null;
                    c
                }));
                let time_index = l.time_index.or(r.time_index.map(|i| i + l_len));
                Schema {
                    fields,
                    time_index,
                    unique_keys: Vec::new(),
                    closed: l.closed && r.closed,
                }
            }
            SQLWindowFunc {
                func,
                args,
                output_name,
                child,
                ..
            } => {
                let mut out = child.schema.clone();
                let arg = args.first().and_then(|a| match a {
                    ScalarExpr::Column(id) => out.fields.get(*id),
                    _ => None,
                });
                let arg_dtype = || {
                    arg.and_then(|c| c.plain_dtype().cloned())
                        .unwrap_or(DataType::Float64)
                };
                let (dtype, nullable) = match func {
                    WindowFuncKind::RowNumber
                    | WindowFuncKind::Rank
                    | WindowFuncKind::DenseRank
                    | WindowFuncKind::Count => (DataType::Int64, false),
                    WindowFuncKind::Sum | WindowFuncKind::Avg => (DataType::Float64, true),
                    WindowFuncKind::Lag
                    | WindowFuncKind::Lead
                    | WindowFuncKind::LagInFrame
                    | WindowFuncKind::LeadInFrame
                    | WindowFuncKind::FirstValue
                    | WindowFuncKind::LastValue
                    | WindowFuncKind::NthValue(_) => (arg_dtype(), true),
                    WindowFuncKind::Min | WindowFuncKind::Max => {
                        (arg_dtype(), arg.is_none_or(|c| c.nullable))
                    }
                };
                out.fields
                    .push(Field::plain(output_name.clone(), dtype, nullable));
                out
            }

            ScalarBridge(expr) => {
                let dtype = match expr {
                    ScalarExpr::CurrentTimestamp => DataType::Timestamp,
                    _ => DataType::Float64,
                };
                Self::scalar_schema(dtype)
            }

            // `vector(s)` yields a label-less instant vector: the (ts, value)
            // floor and nothing else; its full label set (empty) is known.
            PromqlVectorFromScalar(_) => Schema {
                fields: vec![
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("value", DataType::Float64, false),
                ],
                time_index: Some(0),
                unique_keys: Vec::new(),
                closed: true,
            },

            // The output shape of `<vector> op <scalar>` is the vector side's:
            // a scalar operand contributes only its value, no labels. A `bool`
            // comparison still produces the vector's shape (values 0/1).
            BinaryOp { lhs, rhs, .. } => {
                if lhs.is_scalar_leaf() && !rhs.is_scalar_leaf() {
                    rhs.schema.clone()
                } else {
                    lhs.schema.clone()
                }
            }
        })
    }

    /// The output category derived from this operator and its children.
    pub fn output_kind(&self) -> OperatorResultKind {
        use NonASAPOp::*;
        match self {
            Scan { source, .. } => match source {
                Source::TimeSeries { .. } => OperatorResultKind::InstantVector,
                Source::Table { .. } => OperatorResultKind::Relation,
            },
            Values { .. } | SQLWindowFunc { .. } => OperatorResultKind::Relation,
            TimeRange { kind, .. } => match kind {
                TimeRangeKind::Instant => OperatorResultKind::InstantVector,
                TimeRangeKind::Range => OperatorResultKind::RangeVector,
            },
            PromqlSubquery { .. } => OperatorResultKind::RangeVector,
            PromqlVectorFromScalar(_) => OperatorResultKind::InstantVector,
            ScalarBridge(_) => OperatorResultKind::Scalar,
            // A per-entity range reduction turns a range vector into an
            // instant vector; a cross-series reduction keeps its input's
            // category (a SQL GROUP BY stays a relation).
            Aggregate {
                reduction, child, ..
            } => match (reduction, child.result_kind) {
                (Reduction::PerEntity, OperatorResultKind::RangeVector)
                | (_, OperatorResultKind::State) => OperatorResultKind::InstantVector,
                (_, kind) => kind,
            },
            Filter { child, .. }
            | Project { child, .. }
            | Dedup { child, .. }
            | Sort { child, .. }
            | Limit { child, .. }
            | TimeShift { child, .. }
            | PromqlRelabel { child, .. }
            | PromqlInfoEnrich { child, .. }
            | PromqlSeriesSample { child, .. } => readable(child.result_kind),
            Join { left, .. } | SetOp { left, .. } => readable(left.result_kind),
            Concat { children, .. } => children
                .first()
                .map_or(OperatorResultKind::Relation, |c| readable(c.result_kind)),
            BinaryOp { lhs, rhs, .. } => {
                if lhs.is_scalar_leaf() && !rhs.is_scalar_leaf() {
                    readable(rhs.result_kind)
                } else if lhs.is_scalar_leaf() && rhs.is_scalar_leaf() {
                    OperatorResultKind::Scalar
                } else {
                    readable(lhs.result_kind)
                }
            }
        }
    }

    /// Local producer/consumer contract checks that need only this operator
    /// and its children's output categories.
    pub fn validate_inputs(&self) -> Result<(), QueryExprError> {
        use NonASAPOp::*;
        let no_state = |node: &OperatorNode, what: &str| {
            if node.result_kind == OperatorResultKind::State {
                Err(QueryExprError::InvalidScalarSignature(format!(
                    "{what} consumes summary state; read it out first"
                )))
            } else {
                Ok(())
            }
        };
        match self {
            BinaryOp { lhs, rhs, .. } => {
                no_state(lhs, "BinaryOp")?;
                no_state(rhs, "BinaryOp")
            }
            Join { left, right, .. } | SetOp { left, right, .. } => {
                no_state(left, self.kind_name())?;
                no_state(right, self.kind_name())
            }
            Concat { children, .. } => children.iter().try_for_each(|c| no_state(c, "Concat")),
            Aggregate { child, .. } | Dedup { child, .. } => no_state(child, self.kind_name()),
            _ => Ok(()),
        }
    }
}

/// A readout-shaped category for a value-level operator over `kind`: state
/// never flows through an ordinary operator unchanged in category.
fn readable(kind: OperatorResultKind) -> OperatorResultKind {
    match kind {
        OperatorResultKind::State => OperatorResultKind::Relation,
        other => other,
    }
}

/// Default output-column name for a projection item with no explicit alias:
/// a bare column keeps its (schema) name; anything else gets `col_{i}`.
fn default_proj_name(expr: &ScalarExpr, idx: usize, schema: &Schema) -> String {
    match expr {
        ScalarExpr::Column(id) => schema
            .fields
            .get(*id)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| format!("col_{idx}")),
        _ => format!("col_{idx}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::scalar::ExprSemantics;
    use crate::pre_asap::expr_ir::{ArithmeticOpKind, CompareOpKind, ScalarValue};
    use crate::pre_asap::vocabulary::{
        AtModifier, VectorMatchKind, WindowFrameBound, WindowFrameOffset, WindowFrameUnits,
    };

    fn col(name: &str, dtype: DataType, nullable: bool) -> Field {
        Field::plain(name, dtype, nullable)
    }

    fn node(op: NonASAPOp) -> Rc<OperatorNode> {
        OperatorNode::non_asap_node(op).unwrap()
    }

    fn scan(
        columns: Vec<Field>,
        time_index: Option<ColumnId>,
        uk: Vec<Vec<ColumnId>>,
    ) -> NonASAPOp {
        NonASAPOp::Scan {
            source: Source::Table {
                table_ref: "t".into(),
            },
            predicates: vec![],
            schema: Schema {
                fields: columns,
                time_index,
                unique_keys: uk,
                closed: true,
            },
        }
    }

    /// `[ts, value, job]` time-series leaf; open, as a PromQL leaf is.
    fn series_scan() -> NonASAPOp {
        NonASAPOp::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    col("ts", DataType::Timestamp, false),
                    col("value", DataType::Float64, false),
                    col("job", DataType::Utf8, true),
                ],
                0,
                vec![],
            ),
        }
    }

    fn item(alias: Option<&str>, expr: ScalarExpr) -> ProjectItem {
        ProjectItem {
            alias: alias.map(Into::into),
            expr,
        }
    }

    fn add(left: ScalarExpr, right: ScalarExpr) -> ScalarExpr {
        ScalarExpr::Arithmetic {
            op: ArithmeticOpKind::Add,
            left: Box::new(left),
            right: Box::new(right),
            semantics: ExprSemantics::Sql,
        }
    }

    fn project(cols: Vec<ProjectItem>, child: NonASAPOp) -> NonASAPOp {
        NonASAPOp::Project {
            cols,
            qualifier: None,
            child: node(child),
        }
    }

    fn dedup_branch(columns: Vec<Field>) -> Rc<OperatorNode> {
        node(NonASAPOp::Dedup {
            cols: vec![0],
            child: node(scan(columns, None, vec![])),
        })
    }

    fn concat(
        children: Vec<Rc<OperatorNode>>,
        discriminator_unique_key: Option<ConcatDiscriminatorKey>,
    ) -> NonASAPOp {
        NonASAPOp::Concat {
            children,
            discriminator_unique_key,
        }
    }

    fn rate_over(child: NonASAPOp) -> NonASAPOp {
        NonASAPOp::Aggregate {
            reduction: Reduction::PerEntity,
            measures: vec![AggIntent::Rate],
            output_names: vec![],
            having: None,
            child: node(child),
        }
    }

    #[test]
    fn project_preserves_unique_keys_that_are_passed_through() {
        let input = scan(
            vec![
                col("tenant", DataType::Utf8, false),
                col("region", DataType::Utf8, false),
                col("value", DataType::Int64, false),
            ],
            None,
            vec![vec![0, 1]],
        );
        let projected = project(
            vec![
                item(Some("r"), ScalarExpr::Column(1)),
                item(Some("t"), ScalarExpr::Column(0)),
                item(
                    None,
                    add(
                        ScalarExpr::Column(2),
                        ScalarExpr::Literal(ScalarValue::Int64(1)),
                    ),
                ),
            ],
            input,
        );
        assert_eq!(
            projected.output_schema().unwrap().unique_keys,
            vec![vec![1, 0]]
        );
    }

    #[test]
    fn project_drops_a_unique_key_when_a_key_column_is_omitted() {
        let input = scan(
            vec![
                col("tenant", DataType::Utf8, false),
                col("region", DataType::Utf8, false),
            ],
            None,
            vec![vec![0, 1]],
        );
        let projected = project(vec![item(None, ScalarExpr::Column(0))], input);
        assert!(projected.output_schema().unwrap().unique_keys.is_empty());
    }

    #[test]
    fn project_retypes_and_renames_per_item() {
        let child = scan(
            vec![
                col("ts", DataType::Timestamp, false),
                col("host", DataType::Utf8, false),
                col("value", DataType::Float64, false),
            ],
            Some(0),
            vec![vec![0, 1]],
        );
        let q = project(
            vec![
                // A bare column keeps its name and type.
                item(None, ScalarExpr::Column(1)),
                item(
                    Some("dbl"),
                    add(ScalarExpr::Column(2), ScalarExpr::Column(2)),
                ),
                // A comparison is a nullable Bool under 3-valued logic.
                item(
                    Some("flag"),
                    ScalarExpr::Compare {
                        left: Box::new(ScalarExpr::Column(2)),
                        op: CompareOpKind::Gt,
                        right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(0.0))),
                        semantics: ExprSemantics::Sql,
                    },
                ),
            ],
            child,
        );
        let s = q.output_schema().unwrap();
        assert_eq!(s.fields.len(), 3);
        assert_eq!(s.fields[0], col("host", DataType::Utf8, false));
        assert_eq!(s.fields[1], col("dbl", DataType::Float64, false));
        assert_eq!(s.fields[2], col("flag", DataType::Bool, true));
        // `ts` is not retained: no time axis, and the key is lost.
        assert!(s.time_index.is_none());
        assert!(s.unique_keys.is_empty());
    }

    #[test]
    fn project_keeps_time_index_when_ts_passed_through() {
        let child = scan(
            vec![
                col("ts", DataType::Timestamp, false),
                col("value", DataType::Float64, false),
            ],
            Some(0),
            vec![],
        );
        let q = project(
            vec![
                item(None, ScalarExpr::Column(1)),
                item(None, ScalarExpr::Column(0)),
            ],
            child,
        );
        let s = q.output_schema().unwrap();
        assert_eq!(s.fields[0].name, "value");
        assert_eq!(s.fields[1].name, "ts");
        assert_eq!(s.time_index, Some(1));
    }

    #[test]
    fn legacy_window_json_without_frame_deserializes_as_unspecified() {
        let window = NonASAPOp::SQLWindowFunc {
            func: WindowFuncKind::RowNumber,
            args: vec![],
            partition_by: GroupKeys::by(vec![]),
            order_by: vec![],
            frame: Some(WindowFrame {
                units: WindowFrameUnits::Range,
                start_bound: WindowFrameBound::Preceding(WindowFrameOffset::Scalar(
                    ScalarValue::Null,
                )),
                end_bound: WindowFrameBound::CurrentRow,
            }),
            output_name: "row_number".into(),
            child: node(scan(vec![col("v", DataType::Int64, false)], None, vec![])),
        };
        let mut json = serde_json::to_value(window).unwrap();
        json.get_mut("SQLWindowFunc")
            .and_then(serde_json::Value::as_object_mut)
            .unwrap()
            .remove("frame");
        let decoded: NonASAPOp = serde_json::from_value(json).unwrap();
        assert!(matches!(
            decoded,
            NonASAPOp::SQLWindowFunc { frame: None, .. }
        ));
    }

    /// A row can appear in more than one branch, so no branch's unique key is
    /// a key of the union — `unique_keys` feeds CSE's sharing legality check.
    #[test]
    fn merge_drops_the_branches_unique_keys() {
        let branch = || {
            dedup_branch(vec![
                col("k", DataType::Utf8, false),
                col("v", DataType::Int64, false),
            ])
        };
        assert_eq!(
            branch().schema.unique_keys,
            vec![vec![0]],
            "a Dedup branch does have a unique key on its own"
        );
        let schema = concat(vec![branch(), branch()], None)
            .output_schema()
            .unwrap();
        assert!(
            schema.unique_keys.is_empty(),
            "the union of two deduplicated branches is not deduplicated"
        );
        assert_eq!(schema.fields.len(), 2, "column shape is the first branch's");
    }

    #[test]
    fn merge_and_setop_agree_on_unique_keys() {
        let branch = || dedup_branch(vec![col("k", DataType::Utf8, false)]);
        let merged = concat(vec![branch(), branch()], None);
        let setop = NonASAPOp::SetOp {
            kind: RelationalSetOpKind::Union,
            all: true,
            left: branch(),
            right: branch(),
        };
        assert_eq!(
            merged.output_schema().unwrap().unique_keys,
            setop.output_schema().unwrap().unique_keys,
        );
    }

    #[test]
    fn an_empty_merge_has_no_schema() {
        assert!(matches!(
            concat(vec![], None).output_schema(),
            Err(QueryExprError::EmptyConcat)
        ));
    }

    /// Issue #228: an asserted discriminator yields the compound
    /// `(discriminator, inner_key)` unique key, although each branch's own
    /// `inner_key` repeats across branches.
    #[test]
    fn discriminator_override_produces_a_compound_unique_key() {
        let branch = || {
            dedup_branch(vec![
                col("k", DataType::Utf8, false),
                col("branch_id", DataType::Int64, false),
            ])
        };
        let schema = concat(
            vec![branch(), branch()],
            Some(ConcatDiscriminatorKey::new(1, vec![0])),
        )
        .output_schema()
        .unwrap();
        assert_eq!(schema.unique_keys, vec![vec![1, 0]]);
        assert_eq!(schema.fields.len(), 2, "column shape is the first branch's");
    }

    /// Without a named discriminator a `Concat` never claims a unique key.
    #[test]
    fn no_way_to_fabricate_a_unique_key_without_naming_a_discriminator() {
        let branch = || dedup_branch(vec![col("k", DataType::Utf8, false)]);
        assert!(concat(vec![branch(), branch()], None)
            .output_schema()
            .unwrap()
            .unique_keys
            .is_empty());
    }

    #[test]
    fn without_aggregate_keeps_open_schema_minus_excluded() {
        // `sum without (instance) (m)` over `[ts, value, instance, job]`.
        let leaf = NonASAPOp::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    col("ts", DataType::Timestamp, false),
                    col("value", DataType::Float64, false),
                    col("instance", DataType::Utf8, true),
                    col("job", DataType::Utf8, true),
                ],
                0,
                vec![],
            ),
        };
        let agg = NonASAPOp::Aggregate {
            reduction: Reduction::Reduce(GroupKeys::without(vec![2])),
            measures: vec![AggIntent::Sum { col: None }],
            output_names: vec![],
            having: None,
            child: node(leaf),
        };
        let s = agg.output_schema().unwrap();
        let names: Vec<_> = s.fields.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["job", "sum"], "kept `job`, dropped `instance`");
        assert!(!s.closed, "a `without` result stays open");
        assert!(s.time_index.is_none());
        assert!(s.unique_keys.is_empty(), "kept set unknown → no unique key");
    }

    #[test]
    fn time_shift_is_schema_pass_through() {
        let leaf = node(scan(
            vec![
                col("ts", DataType::Timestamp, false),
                col("value", DataType::Float64, false),
                col("job", DataType::Utf8, true),
            ],
            Some(0),
            vec![],
        ));
        let shifted = NonASAPOp::TimeShift {
            shift: TimeShift {
                offset_ms: 3_600_000,
                at: Some(AtModifier::Timestamp(1_609_746_000_000)),
            },
            child: Rc::clone(&leaf),
        };
        assert_eq!(shifted.output_schema().unwrap(), leaf.schema);
    }

    /// `rate` and `*_over_time` are per-series: every label survives and only
    /// the sample value is replaced (kept named `value`).
    #[test]
    fn per_series_reductions_preserve_labels() {
        for measure in [AggIntent::Rate, AggIntent::Avg { col: None }] {
            let reduced = NonASAPOp::Aggregate {
                reduction: Reduction::PerEntity,
                measures: vec![measure],
                output_names: vec![],
                having: None,
                child: node(NonASAPOp::TimeRange {
                    range: Duration::from_secs(300),
                    kind: TimeRangeKind::Range,
                    child: node(series_scan()),
                }),
            };
            let s = reduced.output_schema().unwrap();
            let names: Vec<_> = s.fields.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(names, vec!["ts", "value", "job"]);
            assert_eq!(s.time_index, Some(0));
        }
    }

    /// An open leaf stays open through a per-series `rate` and is frozen to
    /// closed by a cross-series aggregate.
    #[test]
    fn completeness_open_leaf_freezes_to_closed_at_cross_series_aggregate() {
        let leaf = series_scan();
        assert!(
            !leaf.output_schema().unwrap().closed,
            "schemaless leaf is open"
        );
        let rate = rate_over(leaf);
        assert!(!rate.output_schema().unwrap().closed, "rate stays open");
        let sum_by_job = NonASAPOp::Aggregate {
            reduction: Reduction::by(vec![2]),
            measures: vec![AggIntent::Sum { col: None }],
            output_names: vec![],
            having: None,
            child: node(rate),
        };
        assert!(sum_by_job.output_schema().unwrap().closed);
    }

    fn join(kind: JoinKind) -> NonASAPOp {
        NonASAPOp::Join {
            kind,
            pred: Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
            left: node(scan(
                vec![col("a", DataType::Int64, false)],
                None,
                vec![vec![0]],
            )),
            right: node(scan(vec![col("b", DataType::Utf8, false)], None, vec![])),
        }
    }

    #[test]
    fn inner_join_concatenates_both_sides() {
        let s = join(JoinKind::Inner).output_schema().unwrap();
        assert_eq!(
            s.fields,
            vec![
                col("a", DataType::Int64, false),
                col("b", DataType::Utf8, false)
            ]
        );
        assert!(
            s.unique_keys.is_empty(),
            "post-join row identity not provable"
        );
    }

    #[test]
    fn left_join_makes_right_side_nullable() {
        let s = join(JoinKind::Left).output_schema().unwrap();
        assert!(!s.fields[0].nullable);
        assert!(s.fields[1].nullable);
    }

    #[test]
    fn full_join_makes_both_sides_nullable() {
        let s = join(JoinKind::Full).output_schema().unwrap();
        assert!(s.fields[0].nullable);
        assert!(s.fields[1].nullable);
    }

    #[test]
    fn setop_takes_left_shape_and_drops_unique_keys() {
        let side = || {
            node(scan(
                vec![
                    col("k", DataType::Utf8, false),
                    col("v", DataType::Int64, false),
                ],
                None,
                vec![vec![0]],
            ))
        };
        let s = NonASAPOp::SetOp {
            kind: RelationalSetOpKind::Union,
            all: false,
            left: side(),
            right: side(),
        }
        .output_schema()
        .unwrap();
        assert_eq!(s.fields.len(), 2);
        assert_eq!(s.fields[0].name, "k");
        assert!(
            s.unique_keys.is_empty(),
            "UNION does not preserve row identity"
        );
    }

    /// A scalar at an operator position is one `value` column with no series.
    #[test]
    fn scalar_bridge_has_a_single_value_row_schema() {
        let s = NonASAPOp::ScalarBridge(ScalarExpr::literal_f64(42.0))
            .output_schema()
            .unwrap();
        assert_eq!(s.fields.len(), 1);
        assert_eq!(s.fields[0].name, "value");
        assert_eq!(s.fields[0].dtype, DataType::Float64);
        assert!(s.time_index.is_none());
    }

    /// `<vector> op <scalar>` takes the vector side's schema; the vector
    /// match modifier is kept on the operator.
    #[test]
    fn binary_op_schema_follows_the_vector_side_over_a_scalar_bridge() {
        let vector = node(scan(
            vec![
                col("host", DataType::Utf8, false),
                col("value", DataType::Float64, false),
            ],
            None,
            vec![],
        ));
        let vm = VectorMatch {
            kind: VectorMatchKind::On,
            labels: vec!["host".into()],
            grouping: None,
        };
        let op = NonASAPOp::BinaryOp {
            operator: BinaryOperator {
                checked_relative_division: false,
                checked_finite_division: false,
                kind: BinaryOpKind::Compare(CompareOpKind::Gt),
                vector_match: Some(vm.clone()),
            },
            return_bool: false,
            lhs: Rc::clone(&vector),
            rhs: node(NonASAPOp::ScalarBridge(ScalarExpr::literal_f64(1.0))),
        };
        assert_eq!(op.output_schema().unwrap(), vector.schema);
        let NonASAPOp::BinaryOp { operator, .. } = &op else {
            unreachable!()
        };
        assert_eq!(operator.vector_match.as_ref(), Some(&vm));
    }
}
