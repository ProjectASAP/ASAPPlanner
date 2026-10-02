//! Ordinary (non-ASAP) query operators: everything a front end emits and
//! everything that survives ASAP optimization unchanged.

use std::rc::Rc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::node::{OperatorNode, OperatorResultKind};
use super::scalar::{Predicate, ProjectItem, ScalarExpr, SortKey};
use crate::ir::aggregate_schema::aggregate_output_schema;
use crate::ir::operator_properties::{
    BinaryOpKind, ConcatDiscriminatorKey, GroupKeys, InfoMatcher, JoinKind, Reduction,
    RelationalSetOpKind, SampleKind, Source, TimeShift, VectorMatch, WindowFrame, WindowFuncKind,
};
use crate::ir::SchemaDerivationError;
use crate::pre_asap::agg_intent::AggIntent;
use crate::pre_asap::schema::{ColumnId, DataType, Field, FieldDataType, Schema};

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
/// ordinary operator can read a summary evaluation and a summary can read any
/// relational sub-DAG.
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
        filters: Vec<Option<Predicate>>,
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
    /// binary operators). Mixed scalar/vector operations use Project or Filter.
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
}

impl NonASAPOp {
    /// The direct operator inputs, in field order, followed by the operator
    /// nodes referenced from this operator's scalar expressions.
    pub fn children(&self) -> Vec<&Rc<OperatorNode>> {
        use NonASAPOp::*;
        let mut out: Vec<&Rc<OperatorNode>> = match self {
            Scan { .. } | Values { .. } | PromqlVectorFromScalar(_) => vec![],
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
            Aggregate {
                filters, having, ..
            } => filters
                .iter()
                .flatten()
                .chain(having.iter())
                .map(|p| &p.0)
                .collect(),
            Sort { keys, .. } => keys.iter().map(|k| &k.expr).collect(),
            SQLWindowFunc { args, order_by, .. } => args
                .iter()
                .chain(order_by.iter().map(|k| &k.expr))
                .collect(),
            PromqlVectorFromScalar(e) => vec![e],
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
                filters,
                having,
                child,
            } => {
                let filters = filters
                    .iter()
                    .map(|p| p.as_ref().map(|p| map_pred(p, &mut map_scalar)))
                    .collect();
                let having = having.as_ref().map(|p| map_pred(p, &mut map_scalar));
                Aggregate {
                    reduction: reduction.clone(),
                    measures: measures.clone(),
                    output_names: output_names.clone(),
                    filters,
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
        }
    }

    /// Output schema derived from this operator's parameters and its
    /// children's (already derived) schemas.
    pub fn output_schema(&self) -> Result<Schema, SchemaDerivationError> {
        use NonASAPOp::*;
        Ok(match self {
            Scan { schema, .. } | Values { schema, .. } => schema.clone(),

            Aggregate {
                reduction,
                measures,
                output_names,
                child,
                filters,
                ..
            } => {
                let mut output =
                    aggregate_output_schema(&child.schema, reduction, measures, output_names)?;
                if child.result_kind == OperatorResultKind::Relation {
                    let offset = reduction.group_keys().map_or(0, |keys| keys.len());
                    for (index, measure) in measures.iter().enumerate() {
                        if matches!(
                            measure,
                            AggIntent::Sum { .. }
                                | AggIntent::Avg { .. }
                                | AggIntent::Min { .. }
                                | AggIntent::Max { .. }
                        ) {
                            let nullable = offset == 0
                                || filters.get(index).is_some_and(Option::is_some)
                                || measure
                                    .input_cols()
                                    .iter()
                                    .any(|i| child.schema.fields[*i].nullable);
                            if let Some(field) = output.fields.get_mut(offset + index) {
                                field.nullable = nullable;
                            }
                        }
                    }
                }
                output
            }

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
                    .collect::<Result<Vec<_>, SchemaDerivationError>>()?;
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
                    .ok_or(SchemaDerivationError::EmptyConcat)?
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
                let arg = args.first().map(|a| a.scalar_type(&out)).transpose()?;
                let arg_dtype = || {
                    arg.as_ref()
                        .map(|(ty, _)| ty.clone())
                        .unwrap_or(DataType::Null)
                };
                let (dtype, nullable) = match func {
                    WindowFuncKind::RowNumber
                    | WindowFuncKind::Rank
                    | WindowFuncKind::DenseRank
                    | WindowFuncKind::Count => (DataType::Int64, false),
                    WindowFuncKind::Sum => (arg_dtype(), true),
                    WindowFuncKind::Avg => (DataType::Float64, true),
                    WindowFuncKind::Lag
                    | WindowFuncKind::Lead
                    | WindowFuncKind::LagInFrame
                    | WindowFuncKind::LeadInFrame
                    | WindowFuncKind::FirstValue
                    | WindowFuncKind::LastValue
                    | WindowFuncKind::NthValue(_) => (arg_dtype(), true),
                    WindowFuncKind::Min | WindowFuncKind::Max => (arg_dtype(), true),
                };
                out.fields
                    .push(Field::plain(output_name.clone(), dtype, nullable));
                out
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
            BinaryOp {
                lhs, rhs, operator, ..
            } => {
                let mut output = lhs.schema.clone();
                let grouping = operator
                    .vector_match
                    .as_ref()
                    .and_then(|m| m.grouping.as_ref());
                let right_rows = matches!(
                    operator.kind,
                    BinaryOpKind::Set(crate::pre_asap::PromQLVectorSetOpKind::Or)
                ) || matches!(grouping, Some(g) if g.side == crate::pre_asap::GroupSide::Right);
                let mut additions = Vec::new();
                if right_rows {
                    additions.extend(
                        rhs.schema
                            .fields
                            .iter()
                            .filter(|c| c.plain_dtype() == Some(&DataType::Utf8))
                            .cloned(),
                    );
                }
                if let Some(grouping) = grouping {
                    additions.extend(
                        grouping
                            .labels
                            .iter()
                            .map(|name| Field::plain(name.clone(), DataType::Utf8, true)),
                    );
                }
                for column in additions {
                    if !output.fields.iter().any(|c| c.name == column.name) {
                        output.fields.push(column);
                    }
                }
                output
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
            TimeRange { child, .. } if child.result_kind == OperatorResultKind::Relation => {
                OperatorResultKind::Relation
            }
            TimeRange { kind, .. } => match kind {
                TimeRangeKind::Instant => OperatorResultKind::InstantVector,
                TimeRangeKind::Range => OperatorResultKind::RangeVector,
            },
            PromqlSubquery { .. } => OperatorResultKind::RangeVector,
            PromqlVectorFromScalar(_) => OperatorResultKind::InstantVector,
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
            Project { child, cols, .. } if cols.iter().any(|item| matches!(item.expr, ScalarExpr::Column(i) if child.schema.fields.get(i).is_some_and(|f| !f.is_plain()))) => OperatorResultKind::State,
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
            BinaryOp { lhs, .. } => readable(lhs.result_kind),
        }
    }

    /// Local producer/consumer contract checks that need only this operator
    /// and its children's output categories.
    pub fn validate_inputs(&self) -> Result<(), SchemaDerivationError> {
        use NonASAPOp::*;
        let no_state = |node: &OperatorNode, what: &str| {
            if node.result_kind == OperatorResultKind::State {
                Err(SchemaDerivationError::InvalidScalarSignature(format!(
                    "{what} consumes summary state; read it out first"
                )))
            } else {
                Ok(())
            }
        };
        let invalid = |message: &str| SchemaDerivationError::InvalidScalarSignature(message.into());
        let predicate = |pred: &Predicate, scope: &Schema| -> Result<(), SchemaDerivationError> {
            if matches!(
                pred.0.scalar_type(scope)?.0,
                DataType::Bool | DataType::Null
            ) {
                Ok(())
            } else {
                Err(invalid("predicate must be boolean"))
            }
        };
        let instant = |child: &OperatorNode| -> Result<(), SchemaDerivationError> {
            if child.result_kind == OperatorResultKind::InstantVector {
                Ok(())
            } else {
                Err(invalid("operation requires an instant vector"))
            }
        };
        let columns = |cols: &[usize], scope: &Schema| -> Result<(), SchemaDerivationError> {
            if cols.iter().any(|i| *i >= scope.fields.len()) {
                Err(invalid("column outside operator input scope"))
            } else {
                Ok(())
            }
        };
        match self {
            Scan {
                predicates, schema, ..
            } => {
                for pred in predicates {
                    predicate(pred, schema)?;
                }
            }
            Values { rows, schema } => {
                for row in rows {
                    if row.len() != schema.fields.len() {
                        return Err(invalid("Values row width differs from its schema"));
                    }
                    for (expr, field) in row.iter().zip(&schema.fields) {
                        let (ty, nullable) = expr.scalar_type(&Schema::default())?;
                        if field
                            .plain_dtype()
                            .is_none_or(|declared| *declared != ty && ty != DataType::Null)
                            || nullable && !field.nullable
                        {
                            return Err(invalid(
                                "Values expression differs from declared type/nullability",
                            ));
                        }
                    }
                }
            }
            Filter { child, pred } => {
                no_state(child, "Filter")?;
                predicate(pred, &child.schema)?;
            }
            Project { child, cols, .. } => {
                for col in cols {
                    if let ScalarExpr::Column(index) = col.expr {
                        columns(&[index], &child.schema)?;
                    } else {
                        col.expr.scalar_type(&child.schema)?;
                    }
                }
            }
            BinaryOp {
                lhs,
                rhs,
                operator,
                return_bool,
            } => {
                no_state(lhs, "BinaryOp")?;
                no_state(rhs, "BinaryOp")?;
                if lhs.result_kind != rhs.result_kind
                    || lhs.result_kind == OperatorResultKind::RangeVector
                {
                    return Err(invalid("binary operands have incompatible result kinds"));
                }
                if *return_bool && !matches!(operator.kind, BinaryOpKind::Compare(_)) {
                    return Err(invalid("bool mode requires a comparison"));
                }
            }
            Join {
                left, right, pred, ..
            } => {
                no_state(left, "Join")?;
                no_state(right, "Join")?;
                if left.result_kind != OperatorResultKind::Relation
                    || right.result_kind != OperatorResultKind::Relation
                {
                    return Err(invalid("SQL join requires relations"));
                }
                let scope = Schema::new(
                    left.schema
                        .fields
                        .iter()
                        .chain(&right.schema.fields)
                        .cloned()
                        .collect(),
                );
                predicate(pred, &scope)?;
            }
            SetOp { left, right, .. } => {
                no_state(left, "SetOp")?;
                no_state(right, "SetOp")?;
                if left.result_kind != OperatorResultKind::Relation
                    || right.result_kind != OperatorResultKind::Relation
                {
                    return Err(invalid("SQL set operation requires relations"));
                }
            }
            Concat { children, .. } => {
                for child in children {
                    no_state(child, "Concat")?;
                }
            }
            Aggregate {
                child,
                filters,
                measures,
                having,
                reduction,
                ..
            } => {
                no_state(child, "Aggregate")?;
                if let Reduction::Reduce(keys) = reduction {
                    columns(keys.keys(), &child.schema)?;
                }
                for measure in measures {
                    columns(&measure.input_cols(), &child.schema)?;
                }
                if !filters.is_empty() && filters.len() != measures.len() {
                    return Err(invalid("aggregate filter count differs from measure count"));
                }
                for pred in filters.iter().flatten() {
                    predicate(pred, &child.schema)?;
                }
                if let Some(pred) = having {
                    predicate(pred, &self.output_schema()?)?;
                }
            }
            Dedup { child, cols } => {
                no_state(child, "Dedup")?;
                columns(cols, &child.schema)?;
            }
            Sort {
                child,
                keys,
                partition_by,
            } => {
                columns(partition_by.keys(), &child.schema)?;
                for key in keys {
                    key.expr.scalar_type(&child.schema)?;
                }
            }
            Limit {
                child,
                partition_by,
                ..
            } => columns(partition_by.keys(), &child.schema)?,
            SQLWindowFunc {
                child,
                args,
                order_by,
                partition_by,
                ..
            } => {
                if child.result_kind != OperatorResultKind::Relation {
                    return Err(invalid("SQL window requires a relation"));
                }
                columns(partition_by.keys(), &child.schema)?;
                for expr in args.iter().chain(order_by.iter().map(|k| &k.expr)) {
                    expr.scalar_type(&child.schema)?;
                }
            }
            PromqlVectorFromScalar(expr) => {
                if expr.scalar_type(&Schema::default())? != (DataType::Float64, false) {
                    return Err(invalid("vector() requires a non-null float scalar"));
                }
            }
            PromqlSubquery { child, .. }
            | PromqlInfoEnrich { child, .. }
            | PromqlSeriesSample { child, .. } => instant(child)?,
            PromqlRelabel { child, value, .. } => {
                instant(child)?;
                if value.scalar_type(&child.schema)?.0 != DataType::Utf8 {
                    return Err(invalid("label expression requires a string"));
                }
            }
            TimeRange { child, .. } => {
                if child.result_kind != OperatorResultKind::Relation {
                    instant(child)?;
                }
            }
            TimeShift { child, .. } => {
                if !matches!(
                    child.result_kind,
                    OperatorResultKind::InstantVector | OperatorResultKind::RangeVector
                ) {
                    return Err(invalid("time shift requires a vector"));
                }
            }
        }
        Ok(())
    }
}

/// A evaluation-shaped category for a value-level operator over `kind`: state
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

/// Whether any aggregate measure has its own input predicate.
pub fn any_measure_filtered(filters: &[Option<Predicate>]) -> bool {
    filters.iter().any(Option::is_some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::operator_properties::{
        AtModifier, VectorMatchKind, WindowFrameBound, WindowFrameOffset, WindowFrameUnits,
    };
    use crate::ir::scalar::ExprSemantics;
    use crate::pre_asap::expr_ir::{ArithmeticOpKind, CompareOpKind, ScalarValue};

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
            filters: vec![],
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
        assert_eq!(s.fields[2], col("flag", DataType::Bool, false));
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
            Err(SchemaDerivationError::EmptyConcat)
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
            filters: vec![],
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
                filters: vec![],
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
            filters: vec![],
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

    /// Standalone constants are scalar roots and carry no operator schema.
    #[test]
    fn constant_is_a_scalar_root() {
        let root = crate::ir::QueryRoot::Scalar(ScalarExpr::literal_f64(42.0));
        assert!(root.as_operator().is_none());
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
            rhs: Rc::clone(&vector),
        };
        assert_eq!(op.output_schema().unwrap(), vector.schema);
        let NonASAPOp::BinaryOp { operator, .. } = &op else {
            unreachable!()
        };
        assert_eq!(operator.vector_match.as_ref(), Some(&vm));
    }
}
