//! Exact current-series population candidates over canonical PromQL IR.
use crate::replacement::{
    Replacement, ReplacementProvenance, ReplacementStrategy, ReplacementSubDAG, TargetSubDAG,
};
use asap_types::post_asap::{
    current_series::*, ExecutionTiming, ResultGuarantee, SummaryExpr, SummaryFamilyType,
    SummaryField, SummaryNode, SummarySchema, ValueOperation,
};
use asap_types::pre_asap::{
    AggIntent, CompareOpKind, DataType, QueryExpr, Reduction, ScalarValue, Schema, Source,
};
use std::rc::Rc;

fn plain(schema: Schema) -> SummarySchema {
    SummarySchema {
        time_index: schema.time_index,
        fields: schema
            .columns
            .into_iter()
            .map(|c| SummaryField {
                name: c.name,
                dtype: SummaryFamilyType::Plain(c.dtype),
                nullable: c.nullable,
            })
            .collect(),
    }
}

fn recognize(
    root: &QueryExpr,
) -> Option<(CurrentSeriesPopulation, CurrentSeriesReadout, Rc<QueryExpr>)> {
    let (source, grouping, readout) = match root {
        QueryExpr::Aggregate {
            child,
            reduction: Reduction::Reduce(grouping),
            measures,
            having: None,
            ..
        } => {
            let [AggIntent::Quantile { q, col, .. }] = measures.as_slice() else {
                return None;
            };
            if !q.is_finite() {
                return None;
            }
            let schema = child.output_schema().ok()?;
            if col.is_some_and(|c| schema.columns.get(c).is_none_or(|c| c.name != "value")) {
                return None;
            }
            (child, grouping, CurrentSeriesReadout::Quantile { q: *q })
        }
        QueryExpr::Limit {
            n,
            offset: 0,
            child,
        } => {
            let QueryExpr::Sort {
                child,
                keys,
                partition_by,
            } = child.as_ref()
            else {
                return None;
            };
            let [key] = keys.as_slice() else {
                return None;
            };
            let QueryExpr::Column(col) = &key.expr else {
                return None;
            };
            if key.ascending || child.output_schema().ok()?.columns.get(*col)?.name != "value" {
                return None;
            }
            (child, partition_by, CurrentSeriesReadout::TopK { k: *n })
        }
        _ => return None,
    };
    let QueryExpr::Scan {
        source: Source::TimeSeries { metric },
        predicates,
        schema,
    } = source.as_ref()
    else {
        return None;
    };
    // Open time-series schemas distinguish instant PromQL populations from table rows.
    if metric.is_empty() || schema.closed || schema.time_index.is_none() {
        return None;
    }
    let label = |col: usize| -> Option<String> {
        let c = schema.columns.get(col)?;
        (c.dtype == DataType::Utf8).then(|| c.name.clone())
    };
    let mut matchers = Vec::new();
    for predicate in predicates {
        let QueryExpr::Compare { left, op, right } = predicate.0.as_ref() else {
            return None;
        };
        let (QueryExpr::Column(col), QueryExpr::Literal(ScalarValue::Utf8(value))) =
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
        CurrentSeriesPopulation {
            metric: metric.clone(),
            matchers,
            grouping: labels,
            without: grouping.is_without(),
            lookback_ms: 300_000,
            max_k: 0,
            quantiles: false,
        },
        readout,
        Rc::clone(source),
    ))
}

/// Workload-aware rule: compatible readouts share one retractable population.
/// Deployments opt in by registering this strategy when they can maintain complete
/// current-series inputs and price the maintenance/readout boundary.
/// The population is exact; max_k bounds the shared readout cache, not its members.
pub struct CurrentSeriesStrategy {
    roots: Vec<Rc<QueryExpr>>,
}
impl CurrentSeriesStrategy {
    pub fn new(roots: &[Rc<QueryExpr>]) -> Self {
        Self {
            roots: roots.to_vec(),
        }
    }
    pub fn candidate(&self, root: &Rc<QueryExpr>) -> Option<Rc<SummaryNode>> {
        let (mut population, readout, source) = recognize(root)?;
        let identity = population.clone();
        for other in self.roots.iter().chain(std::iter::once(root)) {
            if let Some((p, r, _)) = recognize(other) {
                if p == identity {
                    match r {
                        CurrentSeriesReadout::Quantile { .. } => population.quantiles = true,
                        CurrentSeriesReadout::TopK { k } => {
                            population.max_k = population.max_k.max(k)
                        }
                    }
                }
            }
        }
        let input_schema = plain(source.output_schema().ok()?);
        let scan = Rc::new(SummaryNode {
            expr: SummaryExpr::KeepPreAsap(source),
            schema: input_schema.clone(),
            guarantee: Some(ResultGuarantee::exact("source samples")),
        });
        let maintained = Rc::new(SummaryNode {
            expr: SummaryExpr::ValueOperation {
                child: scan,
                operation: ValueOperation::MaintainCurrentSeries { population },
                timing: ExecutionTiming::MaintenanceTime,
            },
            schema: input_schema,
            guarantee: Some(ResultGuarantee::exact(
                "latest value per series with stale retraction and lookback expiry",
            )),
        });
        Some(Rc::new(SummaryNode {
            expr: SummaryExpr::ValueOperation {
                child: maintained,
                operation: ValueOperation::ReadCurrentSeries { readout },
                timing: ExecutionTiming::ReadTime,
            },
            schema: plain(root.output_schema().ok()?),
            guarantee: Some(ResultGuarantee::exact("exact current-population readout")),
        }))
    }
}
impl ReplacementStrategy for CurrentSeriesStrategy {
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool {
        recognize(target.root).is_some()
    }
    fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
        self.candidate(target.root).map(|node| ReplacementSubDAG { strategy: "CurrentSeriesStrategy", replacement: Replacement::Summary(node), provenance: ReplacementProvenance::SummaryImplementation, rationale: "share an exact retractable current-series population across quantiles and TopK limits".into() }).into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::post_asap::{compile_executable_dag, share_common_summary_subtrees};
    fn lower(q: &str) -> Rc<QueryExpr> {
        Rc::new(
            asap_frontend_promql::lower_promql(q, asap_types::types::AccuracyTarget::Exact)
                .unwrap(),
        )
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
        let strategy = CurrentSeriesStrategy::new(&roots);
        let space = crate::search_workload_with(
            roots
                .iter()
                .enumerate()
                .map(|(i, r)| (i, Rc::clone(r)))
                .collect(),
            &[Box::new(CurrentSeriesStrategy::new(&roots))],
        );
        assert!(space
            .groups()
            .flat_map(|g| &g.candidates)
            .any(|c| c.strategy == "CurrentSeriesStrategy"));
        let plans = share_common_summary_subtrees(
            roots
                .iter()
                .enumerate()
                .map(|(i, r)| (i, strategy.candidate(r).unwrap()))
                .collect(),
        );
        let mut producers = Vec::new();
        for (_, plan) in &plans {
            compile_executable_dag(plan).unwrap();
            let SummaryExpr::ValueOperation {
                child,
                operation: ValueOperation::ReadCurrentSeries { .. },
                ..
            } = &plan.expr
            else {
                panic!("missing typed readout")
            };
            let SummaryExpr::ValueOperation {
                operation: ValueOperation::MaintainCurrentSeries { population },
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
        let strategy = CurrentSeriesStrategy::new(&roots);
        let (p, _, _) = recognize(&roots[0]).unwrap();
        assert!(p.grouping.is_empty());
        let candidate = strategy.candidate(&roots[0]).unwrap();
        let SummaryExpr::ValueOperation { child, .. } = &candidate.expr else {
            unreachable!()
        };
        let SummaryExpr::ValueOperation {
            operation: ValueOperation::MaintainCurrentSeries { population },
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
        assert!(p.without);
        assert_eq!(p.grouping, ["instance"]);
        assert_eq!(p.matchers[0].operation, CurrentSeriesMatch::Regex);
    }
    // A readout cannot reinterpret arbitrary rows as maintained state or exceed its producer's contract.
    #[test]
    fn malformed_population_dags_fail_closed() {
        let root = lower("topk(5,a)");
        let strategy = CurrentSeriesStrategy::new(std::slice::from_ref(&root));
        let candidate = strategy.candidate(&root).unwrap();
        let mut bad = (*candidate).clone();
        let SummaryExpr::ValueOperation { operation, .. } = &mut bad.expr else {
            unreachable!()
        };
        *operation = ValueOperation::ReadCurrentSeries {
            readout: CurrentSeriesReadout::TopK { k: 6 },
        };
        assert!(compile_executable_dag(&Rc::new(bad.clone())).is_err());
        let SummaryExpr::ValueOperation {
            child, operation, ..
        } = &mut bad.expr
        else {
            unreachable!()
        };
        *operation = ValueOperation::ReadCurrentSeries {
            readout: CurrentSeriesReadout::TopK { k: 5 },
        };
        let producer = Rc::make_mut(child);
        let SummaryExpr::ValueOperation {
            operation: ValueOperation::MaintainCurrentSeries { population },
            ..
        } = &mut producer.expr
        else {
            unreachable!()
        };
        population.metric = "b".into();
        assert!(compile_executable_dag(&Rc::new(bad)).is_err());
    }
}
