//! Shared maintained-population candidates over canonical relational IR.
use crate::replacement::{
    Replacement, ReplacementProvenance, ReplacementStrategy, ReplacementSubDAG, TargetSubDAG,
};
use asap_types::post_asap::{
    maintained_population::*, ExecutionTiming, ResultGuarantee, SummaryExpr, SummaryFamilyType,
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

fn strip_projection(mut root: &QueryExpr) -> &QueryExpr {
    while let QueryExpr::Project { child, .. } = root {
        root = child;
    }
    root
}

fn recognize(root: &QueryExpr) -> Option<(MaintainedPopulation, PopulationReadout, Rc<QueryExpr>)> {
    let root = strip_projection(root);
    let (source, grouping, readout, value_column) = match root {
        QueryExpr::Aggregate {
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
                AggIntent::Sum { col } => (*col, PopulationReadout::Sum),
                AggIntent::Count { .. } => (None, PopulationReadout::Count),
                AggIntent::Avg { col } => (*col, PopulationReadout::Average),
                _ => return None,
            };
            let schema = child.output_schema().ok()?;
            if col.is_some_and(|c| schema.columns.get(c).is_none()) {
                return None;
            }
            (child, grouping, readout, col)
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
    if let QueryExpr::Scan {
        source: Source::Table { .. },
        schema,
        ..
    } = source.as_ref()
    {
        let value_column = value_column.or_else(|| {
            schema
                .columns
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
        if !schema.closed || !population.matches_input(source) {
            return None;
        }
        return Some((population, readout, Rc::clone(source)));
    }
    // A bare PromQL selector carries the declared ingestion interval as a
    // temporal input scope. Membership must expire at that horizon; retain
    // the wrapper as the maintained input so validation can check agreement.
    let (series_source, lookback_ms) = match source.as_ref() {
        QueryExpr::TimeRange { range, child } => {
            let ms = u64::try_from(range.as_millis()).ok()?;
            if ms == 0 || std::time::Duration::from_millis(ms) != *range {
                return None;
            }
            (child.as_ref(), ms)
        }
        other => (other, 300_000),
    };
    let QueryExpr::Scan {
        source: Source::TimeSeries { metric },
        predicates,
        schema,
    } = series_source
    else {
        return None;
    };
    if value_column.is_some_and(|c| schema.columns.get(c).is_none_or(|c| c.name != "value")) {
        return None;
    }
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
    roots: Vec<Rc<QueryExpr>>,
}
impl MaintainedPopulationStrategy {
    pub fn new(roots: &[Rc<QueryExpr>]) -> Self {
        Self {
            roots: roots.to_vec(),
        }
    }
    pub fn candidate(&self, root: &Rc<QueryExpr>) -> Option<Rc<SummaryNode>> {
        if let QueryExpr::Project {
            cols,
            qualifier,
            child,
        } = root.as_ref()
        {
            let child = self.candidate(child)?;
            return Some(Rc::new(SummaryNode {
                guarantee: child.guarantee.clone(),
                schema: plain(root.output_schema().ok()?),
                expr: SummaryExpr::ValueOperation {
                    child,
                    operation: ValueOperation::Project {
                        cols: cols.clone(),
                        qualifier: qualifier.clone(),
                    },
                    timing: ExecutionTiming::ReadTime,
                },
            }));
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
        let input_schema = plain(source.output_schema().ok()?);
        let scan = Rc::new(SummaryNode {
            expr: SummaryExpr::KeepPreAsap(source),
            schema: input_schema.clone(),
            guarantee: Some(ResultGuarantee::exact("source samples")),
        });
        let maintained = Rc::new(SummaryNode {
            expr: SummaryExpr::ValueOperation {
                child: scan,
                operation: ValueOperation::MaintainPopulation { population },
                timing: ExecutionTiming::MaintenanceTime,
            },
            schema: input_schema,
            guarantee: Some(ResultGuarantee::exact(
                "exact members under the declared population semantics",
            )),
        });
        Some(Rc::new(SummaryNode {
            expr: SummaryExpr::ValueOperation {
                child: maintained,
                operation: ValueOperation::ReadPopulation { readout },
                timing: ExecutionTiming::ReadTime,
            },
            schema: plain(root.output_schema().ok()?),
            guarantee: Some(ResultGuarantee::exact("exact current-population readout")),
        }))
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
                replacement: Replacement::Summary(node),
                provenance: ReplacementProvenance::SummaryImplementation,
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
    use asap_types::post_asap::{compile_executable_dag, share_common_summary_subtrees};

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
            compile_executable_dag(&candidate).expect("typed executable DAG");
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
            .groups()
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
            compile_executable_dag(plan).unwrap();
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
        assert!(compile_executable_dag(&Rc::new(bad.clone())).is_err());
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
        assert!(compile_executable_dag(&Rc::new(bad)).is_err());
    }
}
