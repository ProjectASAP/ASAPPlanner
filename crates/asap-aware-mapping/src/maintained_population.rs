//! Shared maintained-population candidates over canonical relational IR.
use crate::replacement::{
    Replacement, ReplacementProvenance, ReplacementStrategy, ReplacementSubDAG, TargetSubDAG,
};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, ScalarExpr};
use asap_types::post_asap::{maintained_population::*, ResultGuarantee, Schema};
use asap_types::pre_asap::{
    AggIntent, CompareOpKind, DataType, QueryExpr, Reduction, ScalarValue, Source,
};
use std::rc::Rc;

fn plain(schema: Schema) -> Schema {
    Schema::lifted(schema.fields, schema.time_index)
}

fn strip_projection(mut root: &OperatorNode) -> &OperatorNode {
    while let Some(NonASAPOp::Project { child, .. }) = root.non_asap() {
        root = child;
    }
    root
}

fn recognize(
    root: &OperatorNode,
) -> Option<(MaintainedPopulation, PopulationReadout, Rc<OperatorNode>)> {
    let root = strip_projection(root);
    let (source, grouping, readout, value_column) = match root.non_asap()? {
        NonASAPOp::Aggregate {
            child,
            reduction: Reduction::Reduce(grouping),
            measures,
            having: None,
            ..
        } => {
            let [intent] = measures.as_slice() else {
                return None;
            };
            let (col, readout) = match intent {
                AggIntent::Quantile { q, col, .. } if q.is_finite() => {
                    (*col, PopulationReadout::Quantile { q: *q })
                }
                AggIntent::TopK { k, .. } => (None, PopulationReadout::TopK { k: *k }),
                AggIntent::Sum { col } => (*col, PopulationReadout::Sum),
                AggIntent::Count { .. } => (None, PopulationReadout::Count),
                AggIntent::Avg { col } => (*col, PopulationReadout::Average),
                _ => return None,
            };
            let schema = &child.schema;
            if col.is_some_and(|c| schema.fields.get(c).is_none()) {
                return None;
            }
            (child, grouping, readout, col)
        }
        NonASAPOp::Limit {
            n: Some(n),
            offset: 0,
            child,
            ..
        } => {
            let Some(NonASAPOp::Sort {
                child,
                keys,
                partition_by,
            }) = child.non_asap()
            else {
                return None;
            };
            let [key] = keys.as_slice() else {
                return None;
            };
            let ScalarExpr::Column(col) = &key.expr else {
                return None;
            };
            if key.ascending {
                return None;
            }
            (
                child,
                partition_by,
                PopulationReadout::TopK { k: *n },
                Some(*col),
            )
        }
        _ => return None,
    };
    if let Some(NonASAPOp::Scan {
        source: table_source @ Source::Table { .. },
        predicates,
        schema,
    }) = source.non_asap()
    {
        // `PopulationInput::Rows` still names its input as a pre-ASAP scan
        // (source + schema). A scan with pushed-down predicates has no such
        // description, so it is not recognized rather than merged with an
        // unfiltered population of the same table.
        if !predicates.is_empty() {
            return None;
        }
        let value_column = value_column.or_else(|| {
            schema
                .fields
                .iter()
                .position(|c| c.dtype == DataType::Float64 && !c.nullable)
        })?;
        let population = MaintainedPopulation {
            input: PopulationInput::Rows {
                input: Rc::new(QueryExpr::Scan {
                    source: table_source.clone(),
                    predicates: Vec::new(),
                    schema: schema.clone(),
                }),
                value_column,
                grouping: grouping.clone(),
            },
            max_k: 0,
            quantiles: false,
        };
        if !schema.closed || !population.matches_node(source) {
            return None;
        }
        return Some((population, readout, Rc::clone(source)));
    }
    // A bare PromQL selector carries the declared ingestion interval as a
    // temporal input scope. Membership must expire at that horizon; retain
    // the wrapper as the maintained input so validation can check agreement.
    let (series_source, lookback_ms) = match source.non_asap() {
        Some(NonASAPOp::TimeRange { range, child, .. }) => {
            let ms = u64::try_from(range.as_millis()).ok()?;
            if ms == 0 || std::time::Duration::from_millis(ms) != *range {
                return None;
            }
            (child.as_ref(), ms)
        }
        _ => (source.as_ref(), 300_000),
    };
    let Some(NonASAPOp::Scan {
        source: Source::TimeSeries { metric },
        predicates,
        schema,
    }) = series_source.non_asap()
    else {
        return None;
    };
    if value_column.is_some_and(|c| schema.fields.get(c).is_none_or(|c| c.name != "value")) {
        return None;
    }
    // PromQL can retain open labels or resolve them into a complete identity column.
    if metric.is_empty()
        || (schema.closed && !schema.has_promql_series_identity())
        || schema.time_index.is_none()
    {
        return None;
    }
    let label = |col: usize| -> Option<String> {
        let c = schema.fields.get(col)?;
        (c.dtype == DataType::Utf8).then(|| c.name.clone())
    };
    let mut matchers = Vec::new();
    for predicate in predicates {
        let ScalarExpr::Compare {
            left, op, right, ..
        } = &predicate.0
        else {
            return None;
        };
        let (ScalarExpr::Column(col), ScalarExpr::Literal(ScalarValue::Utf8(value))) =
            (left.as_ref(), right.as_ref())
        else {
            return None;
        };
        let operation = match op {
            CompareOpKind::Eq => CurrentSeriesMatch::Equal,
            CompareOpKind::Ne => CurrentSeriesMatch::NotEqual,
            CompareOpKind::Regex => CurrentSeriesMatch::Regex,
            CompareOpKind::NotRegex => CurrentSeriesMatch::NotRegex,
            _ => return None,
        };
        matchers.push(CurrentSeriesMatcher {
            label: label(*col)?,
            value: value.clone(),
            operation,
        });
    }
    matchers.sort();
    matchers.dedup();
    let mut labels = grouping
        .keys()
        .iter()
        .map(|c| label(*c))
        .collect::<Option<Vec<_>>>()?;
    labels.sort();
    labels.dedup();
    Some((
        MaintainedPopulation {
            input: PopulationInput::CurrentSeries(CurrentSeriesInput {
                metric: metric.clone(),
                matchers,
                grouping: labels,
                without: grouping.is_without(),
                lookback_ms,
            }),
            max_k: 0,
            quantiles: false,
        },
        readout,
        Rc::clone(source),
    ))
}

/// Workload-aware rule: compatible readouts share one retractable population.
/// Deployments opt in by registering this strategy when they can maintain complete
/// population updates and price the maintenance/readout boundary.
/// The population is exact; max_k bounds the shared readout cache, not its members.
pub struct MaintainedPopulationStrategy {
    roots: Vec<Rc<OperatorNode>>,
}
impl MaintainedPopulationStrategy {
    pub fn new(roots: &[Rc<OperatorNode>]) -> Self {
        Self {
            roots: roots.to_vec(),
        }
    }
    pub fn candidate(&self, root: &Rc<OperatorNode>) -> Option<Rc<OperatorNode>> {
        if let Some(NonASAPOp::Project {
            cols,
            qualifier,
            child,
        }) = root.non_asap()
        {
            let child = self.candidate(child)?;
            let guarantee = child.guarantee.clone();
            return Some(Rc::new(
                OperatorNode::with_schema(
                    Operator::NonASAP(NonASAPOp::Project {
                        cols: cols.clone(),
                        qualifier: qualifier.clone(),
                        child,
                    }),
                    plain(root.schema.clone()),
                )
                .with_guarantee(guarantee),
            ));
        }
        let (mut population, readout, source) = recognize(root)?;
        let identity = population.clone();
        for other in self.roots.iter().chain(std::iter::once(root)) {
            if let Some((p, r, _)) = recognize(other) {
                if p == identity {
                    match r {
                        PopulationReadout::Quantile { .. } => population.quantiles = true,
                        PopulationReadout::TopK { k } => population.max_k = population.max_k.max(k),
                        PopulationReadout::Sum
                        | PopulationReadout::Count
                        | PopulationReadout::Average => {}
                    }
                }
            }
        }
        let input_schema = plain(source.schema.clone());
        // The source node itself is the maintained input (a non-ASAP node
        // keeps its derived schema), kept with its exact guarantee.
        let scan = Rc::new(
            source
                .as_ref()
                .clone()
                .with_guarantee(Some(ResultGuarantee::exact("source samples"))),
        );
        let maintained = OperatorNode::asap_node(
            ASAPOp::MaintainPopulation {
                child: scan,
                population,
            },
            input_schema,
            Some(ResultGuarantee::exact(
                "exact members under the declared population semantics",
            )),
        );
        Some(OperatorNode::asap_node(
            ASAPOp::ReadPopulation {
                child: maintained,
                readout,
            },
            plain(root.schema.clone()),
            Some(ResultGuarantee::exact("exact current-population readout")),
        ))
    }
}
impl ReplacementStrategy for MaintainedPopulationStrategy {
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool {
        recognize(target.root).is_some()
    }
    fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
        self.candidate(target.root)
            .map(|node| ReplacementSubDAG {
                strategy: "MaintainedPopulationStrategy",
                replacement: Replacement::Subtree(node),
                provenance: ReplacementProvenance::SummaryRealization,
                rationale:
                    "share an exact maintained population across compatible aggregate readouts"
                        .into(),
            })
            .into_iter()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::lower_promql;
    use asap_types::post_asap::{compile_post_asap_dag, share_common_summary_subtrees};

    fn lower(q: &str) -> Rc<QueryExpr> {
        Rc::new(lower_promql(q, asap_types::types::AccuracyTarget::Exact))
    }

    // Instant scalar aggregations share the same retractable series population.
    #[test]
    fn instant_sum_count_average_are_typed_current_series_candidates() {
        let roots: Vec<_> = [
            "sum(a)",
            "count(a)",
            "avg(a)",
            "sum by(job)(a)",
            "count by(job)(a)",
            "avg by(job)(a)",
        ]
        .map(lower)
        .into();
        let rule = MaintainedPopulationStrategy::new(&roots);
        for root in roots {
            let candidate = rule
                .candidate(&root)
                .expect("current-series rule candidate");
            compile_post_asap_dag(&candidate).expect("typed post-ASAP DAG");
        }
    }

    // Different readout parameters retain one shared maintenance producer in the DAG.
    #[test]
    fn quantiles_and_topk_share_a_planner_population() {
        let roots: Vec<_> = [
            "quantile by(job)(0.5,a)",
            "quantile by(job)(0.99,a)",
            "topk by(job)(1,a)",
            "topk by(job)(5,a)",
        ]
        .map(lower)
        .into();
        let strategy = MaintainedPopulationStrategy::new(&roots);
        let space = crate::search_workload_with(
            roots
                .iter()
                .enumerate()
                .map(|(i, r)| (i, Rc::clone(r)))
                .collect(),
            &[Box::new(MaintainedPopulationStrategy::new(&roots))],
        );
        assert!(space
            .target_subdag_candidates()
            .flat_map(|g| &g.candidates)
            .any(|c| c.strategy == "MaintainedPopulationStrategy"));
        let plans = share_common_summary_subtrees(
            roots
                .iter()
                .enumerate()
                .map(|(i, r)| (i, strategy.candidate(r).unwrap()))
                .collect(),
        );
        let mut producers = Vec::new();
        for (_, plan) in &plans {
            compile_post_asap_dag(plan).unwrap();
            let SummaryExpr::ValueOperation {
                child,
                operation: ValueOperation::ReadPopulation { .. },
                ..
            } = &plan.expr
            else {
                panic!("missing typed readout")
            };
            let SummaryExpr::ValueOperation {
                operation: ValueOperation::MaintainPopulation { population },
                ..
            } = &child.expr
            else {
                panic!("missing maintained population")
            };
            assert_eq!(population.max_k, 5);
            assert!(population.quantiles);
            producers.push(Rc::as_ptr(child));
        }
        assert!(producers.iter().all(|p| *p == producers[0]));
    }

    // Source/group/matcher identity separates populations; temporal/nested operations are not instant populations.
    #[test]
    fn rule_respects_population_semantics() {
        let roots: Vec<_> = [
            "topk(5,a)",
            "topk(10,b)",
            "quantile by(job)(0.5,a)",
            "quantile(0.9,a{job=\"api\"})",
        ]
        .map(lower)
        .into();
        let strategy = MaintainedPopulationStrategy::new(&roots);
        let (p, _, _) = recognize(&roots[0]).unwrap();
        assert!(matches!(p.input, PopulationInput::CurrentSeries(ref s) if s.grouping.is_empty()));
        let candidate = strategy.candidate(&roots[0]).unwrap();
        let SummaryExpr::ValueOperation { child, .. } = &candidate.expr else {
            unreachable!()
        };
        let SummaryExpr::ValueOperation {
            operation: ValueOperation::MaintainPopulation { population },
            ..
        } = &child.expr
        else {
            unreachable!()
        };
        assert_eq!(population.max_k, 5);
        assert!(!population.quantiles);
        for q in [
            "quantile_over_time(0.5,a[1m])",
            "quantile(0.5,sum by(job)(a))",
            "topk(5,a offset 1m)",
            "topk(5,a @ 100)",
            "bottomk(5,a)",
        ] {
            assert!(
                strategy.candidate(&lower(q)).is_none(),
                "unexpected current population for {q}"
            );
        }
        let q = lower("quantile without(instance)(0.5,a{job=~\"api.*\"})");
        let (p, _, _) = recognize(&q).unwrap();
        let PopulationInput::CurrentSeries(p) = p.input else {
            panic!("series input")
        };
        assert!(p.without);
        assert_eq!(p.grouping, ["instance"]);
        assert_eq!(p.matchers[0].operation, CurrentSeriesMatch::Regex);
    }
    // A readout cannot reinterpret arbitrary rows as maintained state or exceed its producer's contract.
    #[test]
    fn malformed_population_dags_fail_closed() {
        let root = lower("topk(5,a)");
        let strategy = MaintainedPopulationStrategy::new(std::slice::from_ref(&root));
        let candidate = strategy.candidate(&root).unwrap();
        let mut bad = (*candidate).clone();
        let SummaryExpr::ValueOperation { operation, .. } = &mut bad.expr else {
            unreachable!()
        };
        *operation = ValueOperation::ReadPopulation {
            readout: PopulationReadout::TopK { k: 6 },
        };
        assert!(compile_post_asap_dag(&Rc::new(bad.clone())).is_err());
        let SummaryExpr::ValueOperation {
            child, operation, ..
        } = &mut bad.expr
        else {
            unreachable!()
        };
        *operation = ValueOperation::ReadPopulation {
            readout: PopulationReadout::TopK { k: 5 },
        };
        let producer = Rc::make_mut(child);
        let SummaryExpr::ValueOperation {
            operation: ValueOperation::MaintainPopulation { population },
            ..
        } = &mut producer.expr
        else {
            unreachable!()
        };
        let PopulationInput::CurrentSeries(spec) = &mut population.input else {
            unreachable!()
        };
        spec.metric = "b".into();
        assert!(compile_post_asap_dag(&Rc::new(bad)).is_err());
    }
}
