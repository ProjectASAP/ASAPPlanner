//! Compile logical computation to native operators with typed external inputs.
//! Compilation needs no readers; deployment resolves inputs after selection.
use crate::{
    operators::{Expression, Operator, Reduction, SortKey},
    plan::{Boundedness, Emission, NodeId, PhysicalDag, PhysicalOperator, PlanProperties},
    values::{Batch, Schema},
    Error,
};
use planner_types::{
    post_asap::{
        ExactOperation, ExecutableDag, ExecutableDagNode, ExecutableOperatorPayload as Payload,
        SketchQuery, SummaryFamilyType, SummaryInputExpr, ValueOperation,
    },
    pre_asap::{
        AggIntent, ColumnRef, CompareOpKind, GroupKeys, QueryExpr, Reduction as PlannerReduction,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

/// Source nodes cut the DAG at an installed storage/ingestion frontier. The
/// binding must have exactly the declared schema and no upstream dependencies.
/// A deployment must authorize these frontiers before calling this function.
pub type Source<'a> = Box<dyn PhysicalOperator<Batch, Schema> + 'a>;

mod candidates;
pub use candidates::{
    compile_candidate, compile_candidates, select_candidate, CandidateCost, CandidateSelection,
    PhysicalCandidate,
};

mod compiled;
pub use compiled::{CompiledPhysicalDag, InputContract};

/// Compile computation without opening or retaining deployment readers.
/// Input contracts identify explicit boundaries selected by maintenance planning.
pub fn compile(
    dag: &ExecutableDag,
    inputs: BTreeMap<NodeId, InputContract>,
    roots: &[NodeId],
) -> Result<CompiledPhysicalDag, Error> {
    compile_internal(dag, inputs, roots)
}

/// Convenience for callers that already resolved inputs. Lowering still uses
/// only their contracts, and instantiation checks those contracts again.
pub fn bind<'a>(
    dag: &ExecutableDag,
    sources: BTreeMap<NodeId, Source<'a>>,
    roots: &[NodeId],
) -> Result<PhysicalDag<'a, Batch, Schema>, Error> {
    let inputs = sources
        .iter()
        .map(|(&id, source)| (id, InputContract::from_source(source.as_ref())))
        .collect();
    compile(dag, inputs, roots)?.instantiate(sources)
}

/// Resolve raw scan connectors before invoking the reader-independent compiler.
pub fn bind_with_data_sources<'a>(
    dag: &ExecutableDag,
    mut sources: BTreeMap<NodeId, Source<'a>>,
    roots: &[NodeId],
    data_sources: &crate::sources::DataSources,
) -> Result<PhysicalDag<'a, Batch, Schema>, Error> {
    // Only resolve scans reachable below the selected input boundaries.
    let mut pending = roots.to_vec();
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) || sources.contains_key(&id) {
            continue;
        }
        let node = dag
            .nodes
            .iter()
            .find(|n| u64::from(n.id.0) == id)
            .ok_or_else(|| invalid(format!("missing node {id}")))?;
        if let Payload::Fallback {
            expression: expression @ QueryExpr::Scan { .. },
        } = &node.payload
        {
            sources.insert(id, Box::new(data_sources.bind(expression)?));
        } else {
            pending.extend(
                dag.edges
                    .iter()
                    .filter(|e| u64::from(e.consumer.0) == id)
                    .map(|e| u64::from(e.producer.0)),
            );
        }
    }
    bind(dag, sources, roots)
}

fn compile_internal(
    dag: &ExecutableDag,
    mut sources: BTreeMap<NodeId, InputContract>,
    roots: &[NodeId],
) -> Result<CompiledPhysicalDag, Error> {
    preflight_depth(dag)?;
    dag.validate().map_err(|e| invalid(e.to_string()))?;
    let nodes = dag
        .nodes
        .iter()
        .map(|node| (u64::from(node.id.0), node))
        .collect::<BTreeMap<_, _>>();
    let mut dependencies = BTreeMap::<NodeId, Vec<NodeId>>::new();
    // Binary input order is semantic; serialized edge order is not.
    let mut edges = dag.edges.iter().collect::<Vec<_>>();
    edges.sort_by_key(|edge| {
        (
            edge.consumer.0,
            match edge.role {
                planner_types::post_asap::EdgeRole::Left => 0,
                planner_types::post_asap::EdgeRole::Input => 1,
                planner_types::post_asap::EdgeRole::Right => 2,
            },
        )
    });
    for edge in edges {
        dependencies
            .entry(u64::from(edge.consumer.0))
            .or_default()
            .push(u64::from(edge.producer.0));
    }
    if sources.keys().any(|id| !nodes.contains_key(id)) {
        return Err(invalid("source binding names an unknown node"));
    }
    let mut ordered = Vec::new();
    let mut seen = BTreeSet::new();
    let mut pending = roots.iter().map(|&id| (id, false)).collect::<Vec<_>>();
    while let Some((id, expanded)) = pending.pop() {
        if expanded {
            ordered.push(id);
            continue;
        }
        if !seen.insert(id) {
            continue;
        }
        if !nodes.contains_key(&id) {
            return Err(invalid(format!("missing root {id}")));
        }
        pending.push((id, true));
        if !sources.contains_key(&id) {
            for &input in dependencies.get(&id).into_iter().flatten() {
                pending.push((input, false));
            }
        }
    }
    let mut graph = CompiledPhysicalDag::new(roots.to_vec());
    let mut auxiliary = u64::MAX;
    for id in ordered {
        let node = nodes[&id];
        let output = Arc::new(node.output_schema.clone());
        crate::values::validate_schema(&output)?;
        if let Some(source) = sources.remove(&id) {
            if source.schema != output {
                return Err(invalid("frontier does not have the declared schema"));
            }
            graph.add_input(id, source)?;
        } else {
            let mut inputs = dependencies.get(&id).cloned().unwrap_or_default();
            let mut schemas = inputs
                .iter()
                .map(|id| Arc::new(nodes[id].output_schema.clone()))
                .collect::<Vec<_>>();
            if matches!(node.payload, Payload::SummaryMerge) && inputs.len() > 1 {
                if schemas.iter().any(|s| s != &schemas[0]) {
                    return Err(invalid("summary merge inputs have different schemas"));
                }
                graph.add(
                    auxiliary,
                    inputs,
                    Operator::union(schemas[0].clone(), schemas.len())?,
                )?;
                inputs = vec![auxiliary];
                auxiliary -= 1;
                schemas.truncate(1);
            }
            let operator = compile_node(node, &schemas)
                .map_err(|error| invalid(format!("node {id}: {error}")))?;
            graph.add(id, inputs, operator)?;
        }
    }
    graph.validate()?;
    Ok(graph)
}

/// Bind a Planner node against the schemas supplied by its deployment edges.
/// This is the same checked path used by complete DAG binding.
pub fn compile_node(node: &ExecutableDagNode, inputs: &[Schema]) -> Result<Operator, Error> {
    for schema in inputs {
        crate::values::validate_schema(schema)?;
    }
    bind_operation(node, inputs)?.with_output_schema(Arc::new(node.output_schema.clone()))
}

fn bind_operation(node: &ExecutableDagNode, inputs: &[Schema]) -> Result<Operator, Error> {
    if let Payload::RelationalJoin {
        join_kind,
        pred,
        pruning,
    } = &node.payload
    {
        use planner_types::{post_asap::CandidateCompleteness, pre_asap::JoinKind};
        if pruning.is_some() && *join_kind != JoinKind::Semi {
            return Err(invalid("pruning certificate requires a semi-join"));
        }
        if matches!(pruning,Some(CandidateCompleteness::Certified { guarantee }) if guarantee.has_unknown() || guarantee.metric != planner_types::post_asap::ErrorMetric::TopKMembership)
        {
            return Err(invalid("invalid pruning certificate"));
        }
        let [left, right] = inputs else {
            return Err(invalid("join requires two inputs"));
        };
        if *join_kind == JoinKind::Semi {
            if let Ok(keys) = equijoin_keys(pred, left, right) {
                return Operator::semi_join(left.clone(), right.clone(), keys);
            }
        }
        return Operator::relational_join(
            left.clone(),
            right.clone(),
            join_kind.clone(),
            pred,
            Arc::new(node.output_schema.clone()),
        );
    }
    let [input] = inputs else {
        return Err(invalid(
            "native Planner binding currently requires a unary operation or an explicit source",
        ));
    };
    match &node.payload {
        Payload::Value { operation, .. } => match operation {
            ValueOperation::Project { cols, .. } => Operator::project(
                input.clone(),
                cols.iter()
                    .enumerate()
                    .map(|(i, col)| {
                        Ok((
                            node.output_schema
                                .fields
                                .get(i)
                                .ok_or_else(|| invalid("projection width mismatch"))?
                                .name
                                .clone(),
                            expression(&col.expr, input)?,
                        ))
                    })
                    .collect::<Result<_, Error>>()?,
            ),
            ValueOperation::Filter { pred } => {
                Operator::filter(input.clone(), expression(&pred.0, input)?)
            }
            ValueOperation::Sort { keys, partition_by } => Operator::sort(
                input.clone(),
                keys.iter()
                    .map(|key| {
                        let QueryExpr::Column(column) = key.expr else {
                            return Err(invalid(
                                "sort expression must be projected before sorting",
                            ));
                        };
                        Ok(SortKey {
                            column,
                            descending: !key.ascending,
                            nulls_first: key.nulls_first,
                        })
                    })
                    .collect::<Result<_, Error>>()?,
                groups(input, partition_by)?,
            ),
            ValueOperation::Limit {
                n,
                offset,
                partition_by,
            } => Operator::limit(
                input.clone(),
                *n as u64,
                *offset as u64,
                groups(input, partition_by)?,
            ),
            ValueOperation::Exact(ExactOperation::Aggregate {
                reduction,
                measures,
                output_names,
                having: None,
            }) => {
                if measures.len() != output_names.len() {
                    return Err(invalid("aggregate output names differ from measures"));
                }
                let PlannerReduction::Reduce(keys) = reduction else {
                    return Err(invalid(
                        "per-entity aggregate requires an explicit entity binding",
                    ));
                };
                let measures = measures
                    .iter()
                    .zip(output_names)
                    .map(|(m, name)| {
                        let column = |col: Option<usize>| {
                            col.map(Ok)
                                .unwrap_or_else(|| named_column(input, &ColumnRef::SampleValue))
                        };
                        let m = match m {
                            AggIntent::Count { .. } => Reduction::Count,
                            AggIntent::Sum { col } => Reduction::Sum(column(*col)?),
                            AggIntent::Avg { col } => Reduction::Avg(column(*col)?),
                            AggIntent::Min { col } => Reduction::Min(column(*col)?),
                            AggIntent::Max { col } => Reduction::Max(column(*col)?),
                            _ => {
                                return Err(invalid(
                                    "aggregate intent has no native implementation",
                                ))
                            }
                        };
                        Ok((name.clone(), m))
                    })
                    .collect::<Result<_, Error>>()?;
                Operator::aggregate(input.clone(), groups(input, keys)?, measures)
            }
            ValueOperation::FinalizeExactAccumulator => {
                let state = summary_column(input)?;
                use crate::Statistic as S;
                use planner_types::post_asap::ExactKind as E;
                let statistic = match &input.fields[state].dtype {
                    SummaryFamilyType::ExactAggregate(kind, _) => match kind {
                        E::Sum => S::Sum,
                        E::Count => S::Count,
                        E::Min => S::Min,
                        E::Max => S::Max,
                        E::Rate => S::Rate,
                        E::Increase => S::Increase,
                        _ => return Err(invalid("exact family readout is unsupported")),
                    },
                    _ => return Err(invalid("exact finalization requires exact state")),
                };
                Operator::readout(input.clone(), state, statistic, Default::default())
            }
            _ => Err(invalid("value operation has no native implementation")),
        },
        Payload::SummaryAgg {
            family,
            input: update,
            reduction,
            grouping,
        } => {
            if let Some(item) = &update.item {
                let PlannerReduction::Reduce(keys) = reduction else {
                    return Err(invalid("keyed summary requires explicit partitions"));
                };
                let SummaryInputExpr::Column(weight) = &update.weight else {
                    return Err(invalid(
                        "keyed summary weight must be a finalized value column",
                    ));
                };
                if matches!(family, SummaryFamilyType::Sketch(kind, _) if kind.algorithm() == &planner_types::post_asap::SketchAlgorithm::CmsWithHeap)
                    && !matches!(
                        update.weight_domain,
                        planner_types::post_asap::WeightDomain::NonNegative { .. }
                    )
                {
                    return Err(invalid("CMS requires a nonnegative weight contract"));
                }
                fn columns(
                    expr: &SummaryInputExpr,
                    input: &Schema,
                    result: &mut Vec<usize>,
                ) -> Result<(), Error> {
                    match expr {
                        SummaryInputExpr::Column(column) => {
                            result.push(named_column(input, column)?)
                        }
                        SummaryInputExpr::Tuple(items) => {
                            for item in items {
                                columns(item, input, result)?;
                            }
                        }
                        _ => return Err(invalid("keyed summary needs explicit item columns")),
                    }
                    Ok(())
                }
                let mut items = Vec::new();
                columns(item, input, &mut items)?;
                return Operator::keyed_summary_build(
                    input.clone(),
                    family.clone(),
                    named_column(input, weight)?,
                    items,
                    groups(input, keys)?,
                );
            }
            crate::capability::validate_summary_kernel(family, update, grouping)
                .map_err(Error::Invalid)?;
            let SummaryInputExpr::Column(column) = &update.weight else {
                return Err(invalid(
                    "summary update expression must be projected to a column",
                ));
            };
            let PlannerReduction::Reduce(keys) = reduction else {
                return Err(invalid(
                    "summary construction requires explicit grouping columns",
                ));
            };
            Operator::summary_build(
                input.clone(),
                family.clone(),
                named_column(input, column)?,
                input.time_index,
                groups(input, keys)?,
            )
        }
        Payload::SummaryMerge => {
            let state = summary_column(input)?;
            Operator::summary_merge(
                input.clone(),
                state,
                (0..input.fields.len())
                    .filter(|&i| i != state && Some(i) != input.time_index)
                    .collect(),
            )
        }
        Payload::SummaryEstimate { query } => {
            if let SketchQuery::TopK { k } = query {
                return Operator::keyed_readout(
                    input.clone(),
                    summary_column(input)?,
                    *k,
                    Arc::new(node.output_schema.clone()),
                );
            }
            let mut params = std::collections::HashMap::new();
            let statistic = match query {
                SketchQuery::Quantile { q } => {
                    params.insert("quantile".into(), q.to_string());
                    crate::Statistic::Quantile
                }
                SketchQuery::Cardinality => crate::Statistic::Cardinality,
                SketchQuery::PointCount { value: None, .. } => crate::Statistic::Count,
                _ => return Err(invalid("summary readout is not implemented")),
            };
            Operator::readout(input.clone(), summary_column(input)?, statistic, params)
        }
        _ => Err(invalid(
            "physical operation has no native binding; no fallback is installed",
        )),
    }
}
fn summary_column(input: &Schema) -> Result<usize, Error> {
    let columns = input
        .fields
        .iter()
        .enumerate()
        .filter(|(_, f)| !matches!(f.dtype, SummaryFamilyType::Plain(_)))
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    match columns.as_slice() {
        [column] => Ok(*column),
        _ => Err(invalid("one summary state column required")),
    }
}
fn named_column(input: &Schema, column: &ColumnRef) -> Result<usize, Error> {
    let name = match column {
        ColumnRef::Named(name) => name.as_str(),
        ColumnRef::SampleValue => "value",
        _ => {
            return Err(invalid(
                "summary update requires an unambiguous bound column",
            ))
        }
    };
    let matches = input
        .fields
        .iter()
        .enumerate()
        .filter(|(_, field)| field.name == name)
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [column] => Ok(*column),
        _ => Err(invalid("summary update column missing or ambiguous")),
    }
}
fn groups(input: &Schema, groups: &GroupKeys) -> Result<Vec<usize>, Error> {
    if groups.is_without() {
        return Err(invalid("grouping without requires resolved label columns"));
    }
    if groups.keys().iter().any(|&i| i >= input.fields.len()) {
        return Err(invalid("grouping column out of range"));
    }
    Ok(groups.keys().to_vec())
}
fn expression(expr: &QueryExpr, input: &Schema) -> Result<Expression, Error> {
    Ok(Expression::planner(
        crate::expressions::CompiledExpression::compile(expr, input)?,
    ))
}

struct CheckedSource<'a> {
    source: Source<'a>,
    output: Schema,
}
impl PhysicalOperator<Batch, Schema> for CheckedSource<'_> {
    fn properties(&self, inputs: &[crate::plan::PlanProperties]) -> crate::plan::PlanProperties {
        self.source.properties(inputs)
    }

    fn name(&self) -> &str {
        self.source.name()
    }
    fn input_schemas(&self) -> Vec<Schema> {
        vec![]
    }
    fn output_schema(&self) -> Schema {
        self.output.clone()
    }
    fn output_bytes(&self, batch: &Batch) -> usize {
        self.source.output_bytes(batch)
    }
    fn start<'a>(
        &'a self,
        inputs: Vec<crate::runtime::Input<'a, Batch>>,
        context: crate::runtime::RunContext,
    ) -> Result<crate::runtime::OutputStream<'a, Batch>, Error> {
        use futures::StreamExt;
        Ok(self
            .source
            .start(inputs, context)?
            .map(|batch| {
                let batch = batch?;
                if batch.schema() != &self.output {
                    return Err(invalid("source batch differs from its bound schema"));
                }
                Ok(batch)
            })
            .boxed_local())
    }
}

// Bound recursion before invoking the upstream recursive provenance validator.
fn preflight_depth(dag: &ExecutableDag) -> Result<(), Error> {
    let mut remaining = dag
        .nodes
        .iter()
        .map(|node| (node.id, 0usize))
        .collect::<BTreeMap<_, _>>();
    if remaining.len() != dag.nodes.len() {
        return Err(invalid("duplicate Planner node"));
    }
    let mut consumers = BTreeMap::<_, Vec<_>>::new();
    for edge in &dag.edges {
        if !remaining.contains_key(&edge.producer) {
            return Err(invalid("missing Planner edge producer"));
        }
        *remaining
            .get_mut(&edge.consumer)
            .ok_or_else(|| invalid("missing Planner edge consumer"))? += 1;
        consumers
            .entry(edge.producer)
            .or_default()
            .push(edge.consumer);
    }
    let mut ready = remaining
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(id, _)| *id)
        .collect::<std::collections::VecDeque<_>>();
    let mut depths = BTreeMap::new();
    let mut visited = 0;
    while let Some(id) = ready.pop_front() {
        visited += 1;
        let depth = *depths.get(&id).unwrap_or(&1usize);
        if depth > 128 {
            return Err(invalid("DAG exceeds the supported execution depth of 128"));
        }
        for &consumer in consumers.get(&id).into_iter().flatten() {
            let next = depths.entry(consumer).or_insert(1);
            *next = (*next).max(depth + 1);
            let count = remaining.get_mut(&consumer).expect("validated endpoint");
            *count -= 1;
            if *count == 0 {
                ready.push_back(consumer);
            }
        }
    }
    if visited != dag.nodes.len() {
        return Err(invalid("Planner DAG contains a cycle"));
    }
    Ok(())
}

/// Join predicates address the concatenated left/right schema.
fn semi_join_keys(
    expr: &QueryExpr,
    left: usize,
    right: usize,
    keys: &mut Vec<(usize, usize)>,
) -> Result<(), Error> {
    match expr {
        QueryExpr::BoolAnd(parts) => {
            for part in parts {
                semi_join_keys(part, left, right, keys)?;
            }
        }
        QueryExpr::Compare {
            left: a,
            op: CompareOpKind::Eq,
            right: b,
        } => {
            let (QueryExpr::Column(a), QueryExpr::Column(b)) = (a.as_ref(), b.as_ref()) else {
                return Err(invalid("semi-join requires column equality keys"));
            };
            let (a, b) = if a < b { (*a, *b) } else { (*b, *a) };
            if a >= left || b < left || b >= left + right {
                return Err(invalid("semi-join key must match left to right"));
            }
            keys.push((a, b - left));
        }
        _ => return Err(invalid("unsupported semi-join predicate")),
    }
    Ok(())
}

/// Resolve equality keys against the Planner join's concatenated input schema.
/// Deployments may use these positions to bind their source columns.
pub fn equijoin_keys(
    pred: &planner_types::pre_asap::Predicate,
    left: &planner_types::post_asap::SummarySchema,
    right: &planner_types::post_asap::SummarySchema,
) -> Result<Vec<(usize, usize)>, Error> {
    let mut keys = Vec::new();
    semi_join_keys(&pred.0, left.fields.len(), right.fields.len(), &mut keys)?;
    if keys.is_empty() {
        return Err(invalid("semi-join requires explicit matching keys"));
    }
    Ok(keys)
}
