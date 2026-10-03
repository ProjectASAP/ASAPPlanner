//! Shared maintained-population candidates over canonical relational IR.
use crate::replacement::{
    Replacement, ReplacementProvenance, ReplacementStrategy, ReplacementSubDAG, TargetSubDAG,
};
use asap_types::ir::non_asap::any_measure_filtered;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, ScalarExpr};
use asap_types::post_asap::{maintained_population::*, ResultGuarantee};
use asap_types::pre_asap::{AggIntent, CompareOpKind, DataType, Reduction, ScalarValue, Source};
use std::rc::Rc;

fn strip_projection(mut root: &OperatorNode) -> &OperatorNode {
    while let Some(NonASAPOp::Project { child, .. }) = root.non_asap() {
        root = child;
    }
    root
}

/// Skips a projection that keeps every column in place, such as the one
/// `SELECT *` lowers to: it changes neither the rows nor the column positions
/// a Sort key refers to.
fn strip_identity_projection(expr: &Rc<OperatorNode>) -> &Rc<OperatorNode> {
    if let Some(NonASAPOp::Project {
        cols,
        qualifier: None,
        child,
    }) = expr.non_asap()
    {
        let width = child
            .operator
            .output_schema()
            .map(|schema| schema.fields.len());
        if width.ok() == Some(cols.len())
            && cols.iter().enumerate().all(|(i, item)| {
                item.alias.is_none() && matches!(item.expr, ScalarExpr::Column(c) if c == i)
            })
        {
            return child;
        }
    }
    expr
}

fn recognize(
    root: &OperatorNode,
) -> Option<(MaintainedPopulation, PopulationStatistic, Rc<OperatorNode>)> {
    let root = strip_projection(root);
    let (source, grouping, evaluation, value_column) = match root.non_asap()? {
        NonASAPOp::Aggregate {
            child,
            reduction: Reduction::Reduce(grouping),
            measures,
            filters,
            having: None,
            ..
        } => {
            let [intent] = measures.as_slice() else {
                return None;
            };
            if any_measure_filtered(filters) {
                return None;
            }
            let (col, evaluation) = match intent {
                AggIntent::Quantile { q, col, .. } if q.is_finite() => {
                    (*col, PopulationStatistic::Quantile { q: *q })
                }
                AggIntent::TopK { k, .. } => (None, PopulationStatistic::TopK { k: *k }),
                AggIntent::Sum { col } => (*col, PopulationStatistic::Sum),
                AggIntent::Count { .. } => (None, PopulationStatistic::Count),
                AggIntent::Avg { col } => (*col, PopulationStatistic::Average),
                _ => return None,
            };
            let schema = &child.schema;
            if col.is_some_and(|c| schema.fields.get(c).is_none()) {
                return None;
            }
            (child, grouping, evaluation, col)
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
                strip_identity_projection(child),
                partition_by,
                PopulationStatistic::TopK { k: *n },
                Some(*col),
            )
        }
        _ => return None,
    };
    if let Some(NonASAPOp::Scan {
        source: Source::Table { .. },
        schema,
        ..
    }) = source.non_asap()
    {
        let value_column = value_column.or_else(|| {
            schema
                .fields
                .iter()
                .position(|c| c.dtype == DataType::Float64 && !c.nullable)
        })?;
        let population = MaintainedPopulation {
            input: PopulationInput::Rows {
                input: Rc::clone(source),
                value_column,
                grouping: grouping.clone(),
            },
            max_k: 0,
            quantiles: false,
        };
        if !schema.closed || !population.matches_node(source) {
            return None;
        }
        return Some((population, evaluation, Rc::clone(source)));
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
        evaluation,
        Rc::clone(source),
    ))
}

/// Workload-aware rule: compatible evaluations share one retractable population.
/// Deployments opt in by registering this strategy when they can maintain complete
/// population updates and price the maintenance/evaluation boundary.
/// The population is exact; max_k bounds the shared evaluation cache, not its members.
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
                    root.schema.clone(),
                )
                .with_guarantee(guarantee),
            ));
        }
        let (mut population, evaluation, source) = recognize(root)?;
        let identity = population.clone();
        for other in self.roots.iter().chain(std::iter::once(root)) {
            if let Some((p, r, _)) = recognize(other) {
                if p == identity {
                    match r {
                        PopulationStatistic::Quantile { .. } => population.quantiles = true,
                        PopulationStatistic::TopK { k } => {
                            population.max_k = population.max_k.max(k)
                        }
                        PopulationStatistic::Sum
                        | PopulationStatistic::Count
                        | PopulationStatistic::Average => {}
                    }
                }
            }
        }
        let input_schema = source.schema.clone();
        // The source node itself is the maintained input (a non-ASAP node
        // keeps its derived schema), kept with its exact guarantee.
        let scan = Rc::new(
            source
                .as_ref()
                .clone()
                .with_guarantee(Some(ResultGuarantee::exact("source samples"))),
        );
        let maintained = std::rc::Rc::new(
            OperatorNode::with_schema(
                asap_types::ir::Operator::ASAP(ASAPOp::MaintainPopulation {
                    child: scan,
                    population,
                }),
                input_schema,
            )
            .with_guarantee(Some(ResultGuarantee::exact(
                "exact members under the declared population semantics",
            ))),
        );
        Some(std::rc::Rc::new(
            OperatorNode::with_schema(
                asap_types::ir::Operator::ASAP(ASAPOp::EvaluatePopulation {
                    child: maintained,
                    evaluation,
                }),
                root.schema.clone(),
            )
            .with_guarantee(Some(ResultGuarantee::exact(
                "exact current-population evaluation",
            ))),
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
                replacement: Replacement::SubDAG(node),
                provenance: ReplacementProvenance::SummaryRealization,
                rationale:
                    "share an exact maintained population across compatible aggregate evaluations"
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
    use asap_types::ir::cse::share_common_sub_dags;
    use asap_types::ir::export::compile_physical_asap_dag as export_timed;
    use asap_types::ir::timing::{apply_lifecycle_timings, LifecycleAssignment, TimingMemo};

    /// Time `root` under the default lifecycle assignment (which runs the
    /// data-state / population-contract validation) and export it.
    fn compile_physical_asap_dag(root: &Rc<OperatorNode>) -> Result<(), String> {
        root.validate_structure().map_err(|e| e.to_string())?;
        let timed = apply_lifecycle_timings(
            root,
            &LifecycleAssignment::default_maintained(),
            &mut TimingMemo::new(),
        )
        .map_err(|e| format!("{e:?}"))?;
        export_timed(&timed).map_err(|e| format!("{e:?}"))?;
        Ok(())
    }

    fn lower(q: &str) -> Rc<OperatorNode> {
        lower_promql(q, asap_types::types::AccuracyTarget::Exact)
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
            compile_physical_asap_dag(&candidate).expect("typed post-ASAP DAG");
        }
    }

    // Different evaluation parameters retain one shared maintenance producer in the DAG.
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
        let plans = share_common_sub_dags(
            roots
                .iter()
                .enumerate()
                .map(|(i, r)| (i, strategy.candidate(r).unwrap()))
                .collect(),
        );
        let mut producers = Vec::new();
        for (_, plan) in &plans {
            compile_physical_asap_dag(plan).unwrap();
            let Operator::ASAP(ASAPOp::EvaluatePopulation { child, .. }) = &plan.operator else {
                panic!("missing typed evaluation")
            };
            let Operator::ASAP(ASAPOp::MaintainPopulation { population, .. }) = &child.operator
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
        let Operator::ASAP(ASAPOp::EvaluatePopulation { child, .. }) = &candidate.operator else {
            unreachable!()
        };
        let Operator::ASAP(ASAPOp::MaintainPopulation { population, .. }) = &child.operator else {
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
    // Population timing is a lifecycle choice: a retained or rebuilt
    // population both validate, while its evaluation must stay at query time.
    #[test]
    fn population_timing_is_not_structural() {
        let root = lower("topk(5,a)");
        let candidate = MaintainedPopulationStrategy::new(std::slice::from_ref(&root))
            .candidate(&root)
            .unwrap();
        use asap_types::post_asap::ExecutionTiming;
        let with_timings = |population: ExecutionTiming, evaluation: ExecutionTiming| {
            let mut node = (*candidate).clone();
            node.timing = Some(evaluation);
            let Operator::ASAP(ASAPOp::EvaluatePopulation { child, .. }) = &mut node.operator
            else {
                unreachable!()
            };
            Rc::make_mut(child).timing = Some(population);
            compile_physical_asap_dag(&Rc::new(node))
        };
        use ExecutionTiming::{IngestionTime, QueryTime};
        assert!(with_timings(IngestionTime, QueryTime).is_ok());
        assert!(with_timings(QueryTime, QueryTime).is_ok());
        assert!(with_timings(IngestionTime, IngestionTime).is_err());
    }
    // A evaluation cannot reinterpret arbitrary rows as maintained state or exceed its producer's contract.
    #[test]
    fn malformed_population_dags_fail_closed() {
        let root = lower("topk(5,a)");
        let strategy = MaintainedPopulationStrategy::new(std::slice::from_ref(&root));
        let candidate = strategy.candidate(&root).unwrap();
        compile_physical_asap_dag(&candidate).expect("the unmodified candidate is legal");
        let mut bad = (*candidate).clone();
        let Operator::ASAP(ASAPOp::EvaluatePopulation { evaluation, .. }) = &mut bad.operator
        else {
            unreachable!()
        };
        *evaluation = PopulationStatistic::TopK { k: 6 };
        assert!(compile_physical_asap_dag(&Rc::new(bad.clone())).is_err());
        let Operator::ASAP(ASAPOp::EvaluatePopulation { child, evaluation }) = &mut bad.operator
        else {
            unreachable!()
        };
        *evaluation = PopulationStatistic::TopK { k: 5 };
        let producer = Rc::make_mut(child);
        let Operator::ASAP(ASAPOp::MaintainPopulation { population, .. }) = &mut producer.operator
        else {
            unreachable!()
        };
        let PopulationInput::CurrentSeries(spec) = &mut population.input else {
            unreachable!()
        };
        spec.metric = "b".into();
        assert!(compile_physical_asap_dag(&Rc::new(bad)).is_err());
    }
}
