//! Exact local profiles for a restricted query-time proposal domain.
//!
//! Each accepted input is priced from its actual recursively certified supplier.
//! Different priced interfaces are excluded from this proposal, not the full
//! catalog. No original-template child is used as workload statistics evidence.
use crate::search::candidates::configurations::CandidateConfigurationInventory;
use crate::search::candidates::graph::PhysicalCandidateId;
use crate::search::milp::direct_cost::{DirectCostProfile, NodeCostProfile};
use crate::search::refinement::planner::CandidatePlanningContext;
use crate::{CostModel, PricedNode, ProducerOutput};
use asap_types::ir::physical_export::PhysicalASAPDAGNode;
use asap_types::ir::properties::execution_data_state::{DataPrimitive, ExecutionDataState};
use asap_types::ir::properties::{AccuracyScope, ExecutionTiming};
use asap_types::ir::schema::{FieldDataType, SketchParams};
use asap_types::ir::{ASAPOp, NonASAPOp, QueryRoot};
use asap_types::physical::{Materialization, Medium};
use asap_types::workload::{QueryRecurrence, RepeatedDemand};
use std::collections::HashSet;

#[derive(Debug, thiserror::Error)]
pub(in crate::search) enum DirectProfileError {
    #[error(transparent)]
    DeadlineExceeded(#[from] asap_logical_asap_optimization::memo::CandidateGenerationTimeout),
    #[error("direct cost profile: {0}")]
    Invalid(String),
}

pub(in crate::search) struct DirectProfileExtraction {
    pub profile: DirectCostProfile,
    pub supported: Vec<bool>,
    /// The sole input's certified suppliers; empty for scans/unsupported nodes.
    pub suppliers: Vec<Vec<PhysicalCandidateId>>,
}

fn supported_node(node: &asap_types::ir::OperatorNode) -> bool {
    matches!(
        node.non_asap(),
        Some(
            NonASAPOp::Scan { .. }
                | NonASAPOp::TimeRange { .. }
                | NonASAPOp::Filter { .. }
                | NonASAPOp::Project { .. }
                | NonASAPOp::Aggregate { .. }
        )
    ) || matches!(
        node.asap(),
        Some(
            ASAPOp::SummaryAgg { .. }
                | ASAPOp::SummaryEstimate { .. }
                | ASAPOp::FinalizeExactAccumulator { .. }
                | ASAPOp::SummaryMerge { .. }
        )
    )
}

/// Only fixed, identical request schedules, no range requests and one operator
/// root per query are supported. Every returned profile is priced at one
/// execution; the MILP supplies demand, held lifetime, retention and overhead.
pub(in crate::search) fn extract(
    configured: &CandidateConfigurationInventory,
    context: &CandidatePlanningContext<'_>,
) -> Result<Option<DirectProfileExtraction>, DirectProfileError> {
    let settings = context.models.plan_selection;
    let CostModel::UsageOverTime(prices) = &settings.cost_model else {
        return Ok(None);
    };
    let Some(first) = context.entries.first() else {
        return Ok(None);
    };
    if !matches!(
        first.recurrence,
        QueryRecurrence::Repeated(
            RepeatedDemand::FixedInterval(_) | RepeatedDemand::FixedIntervalAt { .. }
        )
    ) || context
        .entries
        .iter()
        .any(|entry| entry.recurrence != first.recurrence || entry.range_evaluation.is_some())
        || context
            .original_roots
            .iter()
            .any(|root| !matches!(root, QueryRoot::Operator(_)))
        || context.original_roots.len() != configured.inventory.graph.query_roots.len()
    {
        return Ok(None);
    }
    let rate = crate::cost::execution_demand::evaluation_rate(
        [(&first.recurrence, None)],
        settings.non_repeating_query_horizon_seconds,
    )
    .map_err(|error| DirectProfileError::Invalid(error.to_string()))?;
    if !rate.is_finite() || rate <= 0.0 {
        return Ok(None);
    }
    if context.entries.iter().any(|entry| {
        !entry.requirements.accuracy.is_exact()
            && (entry.requirements.accuracy.accuracy_scope != AccuracyScope::PerValue
                || entry
                    .requirements
                    .accuracy
                    .failure_probability
                    .is_none_or(|delta| !delta.is_finite() || delta <= 0.0))
    }) {
        return Ok(None);
    }
    let delta = context
        .entries
        .iter()
        .filter(|entry| !entry.requirements.accuracy.is_exact())
        .filter_map(|entry| entry.requirements.accuracy.failure_probability)
        .reduce(f64::min);
    let count = configured.configurations.len();
    let mut nodes = Vec::with_capacity(count);
    for (id, configuration) in configured.configurations.iter().enumerate() {
        check_deadline(context.deadline)?;
        let node = configured.inventory.templates[id].as_ref();
        nodes.push(
            node.filter(|node| {
                supported_node(node)
                    && stable_size(node, delta)
                    && configuration.timing == ExecutionTiming::QueryTime
                    && matches!(
                        configuration.materialization,
                        Materialization::NotMaterialized
                            | Materialization::MatAtQuery(Medium::Memory)
                    )
                    && configured.inventory.graph.candidates[id]
                        .producer_requirements
                        .len()
                        <= 1
            })
            .map(|node| {
                let mut position = 0;
                PhysicalASAPDAGNode {
                    id,
                    payload: node.operator.map_children(|_| {
                        let current = position;
                        position += 1;
                        current
                    }),
                    output_state: ExecutionDataState {
                        timing: ExecutionTiming::QueryTime,
                        primitive: if node.asap().and_then(ASAPOp::produced_state).is_some() {
                            DataPrimitive::SummaryState
                        } else {
                            DataPrimitive::Raw
                        },
                    },
                    materialization: configuration.materialization,
                    output_schema: node.schema.clone(),
                    guarantee: node.guarantee.clone(),
                }
            }),
        );
    }
    // The evaluator inspects immediate scan consumers (and TimeShift, excluded
    // here). Different possible window/no-window contexts cannot share a price.
    let mut lookbacks = vec![None; count];
    for id in 0..count {
        if !nodes[id].as_ref().is_some_and(|node| {
            matches!(
                node.payload,
                asap_types::ir::Operator::NonASAP(NonASAPOp::Scan { .. })
            )
        }) {
            continue;
        }
        let mut windows = HashSet::new();
        for (consumer, candidate) in configured.inventory.graph.candidates.iter().enumerate() {
            check_deadline(context.deadline)?;
            let Some(node) = &nodes[consumer] else {
                continue;
            };
            if candidate.producer_requirements.iter().any(|input| {
                input
                    .compatible_producers
                    .contains(&PhysicalCandidateId(id))
            }) {
                windows.insert(match &node.payload {
                    asap_types::ir::Operator::NonASAP(NonASAPOp::TimeRange { range, .. }) => {
                        Some(range.as_millis() as u64)
                    }
                    _ => None,
                });
            }
        }
        if configured.inventory.graph.query_roots.iter().any(|root| {
            root.compatible_candidates
                .contains(&PhysicalCandidateId(id))
        }) {
            windows.insert(None);
        }
        if windows.len() == 1 {
            lookbacks[id] = *windows.iter().next().unwrap();
        } else {
            nodes[id] = None;
        }
    }
    let mut cache = vec![None; count];
    let mut visited = vec![false; count];
    let mut visiting = vec![false; count];
    let mut suppliers = vec![Vec::new(); count];
    let mut sketch_counts = vec![0; count];
    for id in 0..count {
        price(
            id,
            configured,
            context,
            &nodes,
            &lookbacks,
            &mut cache,
            &mut visited,
            &mut visiting,
            &mut suppliers,
            &mut sketch_counts,
        )?;
    }
    let supported: Vec<_> = cache.iter().map(Option::is_some).collect();

    let zero = || NodeCostProfile {
        execution_cost: 0.0,
        work_seconds: 0.0,
        held_memory_cost_per_second: 0.0,
        materialized: false,
        raw_retention: None,
    };
    let profiles = cache
        .iter()
        .enumerate()
        .map(|(id, priced)| {
            let Some(priced) = priced else {
                return Ok(zero());
            };
            let raw_retention = priced
                .raw_scan
                .map(|(source, bytes)| {
                    serde_json::to_string(source)
                        .map(|identity| (identity, bytes as f64 * prices.cost_per_disk_byte_second))
                        .map_err(|error| DirectProfileError::Invalid(error.to_string()))
                })
                .transpose()?;
            Ok(NodeCostProfile {
                execution_cost: priced.cost.cost_per_second.unwrap(),
                work_seconds: priced.work_ms / 1000.0,
                held_memory_cost_per_second: priced.held.map_or(0.0, |(bytes, _, _)| {
                    bytes as f64 * prices.cost_per_memory_byte_second
                }),
                materialized: nodes[id].as_ref().unwrap().materialization
                    != Materialization::NotMaterialized,
                raw_retention,
            })
        })
        .collect::<Result<Vec<_>, DirectProfileError>>()?;
    let request_ms = settings.measured_operation_costs.ns_per_evaluation * 1e-6;
    Ok(Some(DirectProfileExtraction {
        profile: DirectCostProfile {
            nodes: profiles,
            query_rate: rate,
            request_latency_seconds: request_ms / 1000.0,
            request_cost: request_ms / settings.resource_speed.ms_per_cpu_op
                * prices.cost_per_cpu_op
                * rate
                * context.entries.len() as f64,
        },
        supported,
        suppliers,
    }))
}

#[allow(clippy::too_many_arguments)]
fn price<'a>(
    id: usize,
    configured: &CandidateConfigurationInventory,
    context: &'a CandidatePlanningContext<'_>,
    nodes: &'a [Option<PhysicalASAPDAGNode>],
    lookbacks: &[Option<u64>],
    cache: &mut [Option<PricedNode<'a>>],
    visited: &mut [bool],
    visiting: &mut [bool],
    suppliers: &mut [Vec<PhysicalCandidateId>],
    sketch_counts: &mut [usize],
) -> Result<(), DirectProfileError> {
    check_deadline(context.deadline)?;
    if visited[id] || visiting[id] {
        return Ok(());
    }
    let Some(node) = &nodes[id] else {
        visited[id] = true;
        return Ok(());
    };
    visiting[id] = true;
    let requirements = &configured.inventory.graph.candidates[id].producer_requirements;
    let options: Vec<_> = requirements.first().map_or_else(
        || vec![None],
        |input| {
            input
                .compatible_producers
                .iter()
                .copied()
                .map(Some)
                .collect()
        },
    );
    for supplier in options {
        let mut views = Vec::new();
        if let Some(supplier) = supplier {
            if visiting[supplier.0] {
                continue;
            }
            price(
                supplier.0,
                configured,
                context,
                nodes,
                lookbacks,
                cache,
                visited,
                visiting,
                suppliers,
                sketch_counts,
            )?;
            let Some(priced) = &cache[supplier.0] else {
                continue;
            };
            let producer = nodes[supplier.0].as_ref().unwrap();
            views.push(ProducerOutput {
                payload: &producer.payload,
                output_schema: &producer.output_schema,
                output: priced.output,
                sources: priced.sources.clone(),
                family: priced.family,
                unchanged_columns: priced.unchanged_columns.clone(),
            });
        }
        let sketches = supplier.map_or(0, |supplier| sketch_counts[supplier.0])
            + usize::from(matches!(
                &node.payload,
                asap_types::ir::Operator::ASAP(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(..),
                    ..
                })
            ));
        // Failure-budget sizing splits delta across approximate inputs. This
        // slice admits at most one sketch build along each unary output path.
        if sketches > 1 {
            continue;
        }
        let Ok(priced) = crate::price_node(
            node,
            &views,
            (1, 1.0),
            lookbacks[id],
            1,
            None,
            context.data,
            context.models.plan_selection,
            context.calibration.factor(),
        ) else {
            continue;
        };
        // Compare the actual evaluator outputs, including every piece of the
        // downstream pricing interface. This is not a template-row equivalence.
        if cache[id]
            .as_ref()
            .is_some_and(|prior| !same_price(prior, &priced))
        {
            continue;
        }
        if priced.cost.cost_per_second.is_none()
            || priced.average_memory_bytes != 0.0
            || priced.disk_bytes != 0
            || priced.maintained.is_some()
        {
            continue;
        }
        if let Some(supplier) = supplier {
            suppliers[id].push(supplier);
        }
        sketch_counts[id] = sketch_counts[id].max(sketches);
        if cache[id].is_none() {
            cache[id] = Some(priced);
        }
    }
    visiting[id] = false;
    visited[id] = true;
    Ok(())
}

fn stable_size(node: &asap_types::ir::OperatorNode, delta: Option<f64>) -> bool {
    let Some(ASAPOp::SummaryAgg {
        family: FieldDataType::Sketch(kind, _),
        ..
    }) = node.asap()
    else {
        return true;
    };
    if !matches!(
        kind.params(),
        SketchParams::Kmv {
            relative_error: Some(_),
            ..
        }
    ) {
        return false;
    }
    let Some(delta) = delta else { return false };
    let sized = asap_logical_asap_optimization::accuracy::estimators::with_failure_probability(
        kind.params(),
        delta,
    );
    match (rmp_serde::to_vec(kind.params()), rmp_serde::to_vec(&sized)) {
        (Ok(before), Ok(after)) => before == after,
        _ => false,
    }
}

fn check_deadline(deadline: Option<std::time::Instant>) -> Result<(), DirectProfileError> {
    if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
        Err(asap_logical_asap_optimization::memo::CandidateGenerationTimeout.into())
    } else {
        Ok(())
    }
}

fn same_price(a: &PricedNode<'_>, b: &PricedNode<'_>) -> bool {
    let family = match (a.family, b.family) {
        (crate::StateFamily::None, crate::StateFamily::None) => true,
        (crate::StateFamily::Exact(a), crate::StateFamily::Exact(b)) => a == b,
        (crate::StateFamily::Sketch(a), crate::StateFamily::Sketch(b)) => {
            // SketchParams PartialEq intentionally omits some confidence fields.
            match (rmp_serde::to_vec(a), rmp_serde::to_vec(b)) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            }
        }
        _ => false,
    };
    let bits = |p: &PricedNode<'_>| {
        [
            p.work_ms.to_bits(),
            p.cpu_ops_per_second.to_bits(),
            p.scanned_bytes_per_second.to_bits(),
            p.average_memory_bytes.to_bits(),
        ]
    };
    let held = |p: &PricedNode<'_>| {
        p.held
            .map(|(bytes, rate, per_reader)| (bytes, rate.to_bits(), per_reader))
    };
    family
        && a.output == b.output
        && a.sources == b.sources
        && a.unchanged_columns == b.unchanged_columns
        && bits(a) == bits(b)
        && a.peak_memory_bytes == b.peak_memory_bytes
        && a.disk_bytes == b.disk_bytes
        && held(a) == held(b)
        && a.maintained == b.maintained
        && a.raw_scan == b.raw_scan
        && a.cost.cost_per_second.map(f64::to_bits) == b.cost.cost_per_second.map(f64::to_bits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::candidates::configurations::CandidateConfiguration;
    use crate::search::candidates::graph::{
        CandidateDependencyGraph, PhysicalCandidate, ProducerRequirement, QueryRootCandidates,
    };
    use crate::search::candidates::inventory::CandidateInventory;
    use asap_logical_asap_optimization::memo::MemoGroupId;
    use asap_logical_asap_optimization::test_support::lower_promql;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::{Operator, OperatorNode, ProjectItem, ScalarExpr};
    use asap_types::workload::{
        DataWorkload, Predictability, Query, QueryRequirements, QueryWorkloadEntry,
        RepetitionInterval,
    };

    // Direct pricing must not certify a state that Stage 3 would enlarge.
    #[test]
    fn kmv_profiles_require_already_sized_failure_probability() {
        use asap_types::ir::schema::{SketchAlgorithm, SketchKind};
        let mut node = asap_logical_asap_optimization::test_support::cms_summary("count(m)");
        let small = SketchParams::Kmv {
            k: 2,
            relative_error: Some(0.05),
        };
        let sized = asap_logical_asap_optimization::accuracy::estimators::with_failure_probability(
            &small, 0.05,
        );
        let set = |node: &mut OperatorNode, params| {
            let Operator::ASAP(ASAPOp::SummaryAgg { family, .. }) = &mut node.operator else {
                panic!("summary fixture")
            };
            let FieldDataType::Sketch(_, grouping) = family else {
                panic!("sketch fixture")
            };
            *family = FieldDataType::Sketch(
                SketchKind::new(SketchAlgorithm::Kmv, params),
                grouping.clone(),
            );
        };
        set(&mut node, small);
        assert!(!stable_size(&node, Some(0.05)));
        set(&mut node, sized);
        assert!(stable_size(&node, Some(0.05)));
        assert!(!stable_size(&node, Some(0.00001)));
        assert!(!stable_size(&node, None));
    }

    // Equal-priced plain-output consumers retain both exact and sketch paths.
    // The sketch-count bound controls later resizing, not the price interface.
    #[test]
    fn plain_reader_keeps_exact_and_kmv_supplier_interfaces() {
        use asap_types::ir::schema::{SketchAlgorithm, SketchKind, SketchStatistic};
        use std::rc::Rc;
        let target = AccuracyTarget::max_error(0.05)
            .failure_probability(0.05)
            .accuracy_scope(AccuracyScope::PerValue);
        let exact = OperatorNode::reachable(&lower_promql("count(m)", target.clone()))
            .into_iter()
            .find(|node| matches!(node.non_asap(), Some(NonASAPOp::Aggregate { .. })))
            .unwrap();
        let mut summary = asap_logical_asap_optimization::test_support::cms_summary("count(m)");
        let params = asap_logical_asap_optimization::accuracy::estimators::with_failure_probability(
            &SketchParams::Kmv {
                k: 2,
                relative_error: Some(0.05),
            },
            0.05,
        );
        let Operator::ASAP(ASAPOp::SummaryAgg { family, .. }) = &mut summary.operator else {
            panic!("summary")
        };
        let FieldDataType::Sketch(_, grouping) = family else {
            panic!("sketch")
        };
        *family = FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::Kmv, params),
            grouping.clone(),
        );
        let summary = OperatorNode::new_shared(summary.operator).unwrap();
        let estimate = Rc::new(OperatorNode::with_schema(
            Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input: summary,
                query: SketchStatistic::Cardinality,
            }),
            exact.schema.clone(),
        ));
        let project = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
            cols: exact
                .schema
                .fields
                .iter()
                .enumerate()
                .map(|(index, field)| ProjectItem {
                    alias: Some(field.name.clone()),
                    expr: ScalarExpr::Column(index),
                })
                .collect(),
            qualifier: None,
            child: exact.clone(),
        }))
        .unwrap();
        let mut configured = CandidateConfigurationInventory {
            inventory: CandidateInventory {
                graph: CandidateDependencyGraph::default(),
                templates: vec![],
                group_candidates: vec![],
            },
            configurations: vec![],
        };
        fn append(
            node: Rc<OperatorNode>,
            configured: &mut CandidateConfigurationInventory,
        ) -> PhysicalCandidateId {
            let mut inputs = Vec::new();
            node.operator
                .map_children(|child| inputs.push(append(child.clone(), configured)));
            let id = PhysicalCandidateId(configured.configurations.len());
            configured.configurations.push(CandidateConfiguration {
                source_candidate: id,
                materialization: Materialization::NotMaterialized,
                timing: ExecutionTiming::QueryTime,
            });
            configured.inventory.templates.push(Some(node));
            configured
                .inventory
                .graph
                .candidates
                .push(PhysicalCandidate {
                    group: MemoGroupId(id.0 as u32),
                    local_cost: 0.0,
                    producer_requirements: inputs
                        .into_iter()
                        .map(|supplier| ProducerRequirement {
                            compatible_producers: vec![supplier].into(),
                        })
                        .collect(),
                });
            id
        }
        let exact_id = append(exact, &mut configured);
        let estimate_id = append(estimate, &mut configured);
        let project_id = append(project.clone(), &mut configured);
        configured.inventory.graph.candidates[project_id.0].producer_requirements[0]
            .compatible_producers = vec![exact_id, estimate_id].into();
        configured
            .inventory
            .graph
            .query_roots
            .push(QueryRootCandidates {
                compatible_candidates: vec![project_id],
            });
        let roots = vec![QueryRoot::Operator(project)];
        let entries = vec![QueryWorkloadEntry {
            query: Query("count(m)".into()),
            requirements: QueryRequirements {
                accuracy: target,
                max_latency_ms: None,
            },
            predictability: Predictability::Predictable { known_at: None },
            recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                RepetitionInterval(60_000),
            )),
            range_evaluation: None,
        }];
        let data: DataWorkload = serde_json::from_value(serde_json::json!({ "arrival": {"ingesting": {"rows_per_second":100.0}}, "sources":{"m":{"rows_held":1000,"series_count":1}} })).unwrap();
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            original_roots: &roots,
            entries: &entries,
            data: &data,
            models: crate::PlanningModels::builtin(),
            calibration: crate::CalibrationFit::default(),
        };
        let extracted = extract(&configured, &context).unwrap().unwrap();
        assert!(extracted.supported[exact_id.0] && extracted.supported[estimate_id.0]);
        assert_eq!(
            extracted.suppliers[project_id.0],
            vec![exact_id, estimate_id]
        );
    }

    // A template child is not evidence that every supplier has the same rows.
    // Two deliberately different source alternatives exercise cost-interface
    // filtering independently of the logical compatibility check upstream.
    #[test]
    fn different_supplier_statistics_are_not_priced_as_the_template_child() {
        let scan = |metric| {
            OperatorNode::reachable(&lower_promql(metric, AccuracyTarget::exact()))
                .into_iter()
                .find(|node| matches!(node.non_asap(), Some(NonASAPOp::Scan { .. })))
                .unwrap()
        };
        let m = scan("m");
        let n = scan("n");
        let project = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
            cols: m
                .schema
                .fields
                .iter()
                .enumerate()
                .map(|(index, field)| ProjectItem {
                    alias: Some(field.name.clone()),
                    expr: ScalarExpr::Column(index),
                })
                .collect(),
            qualifier: None,
            child: m.clone(),
        }))
        .unwrap();
        let configured = CandidateConfigurationInventory {
            inventory: CandidateInventory {
                graph: CandidateDependencyGraph {
                    candidates: (0..3)
                        .map(|id| PhysicalCandidate {
                            group: MemoGroupId(id as u32),
                            local_cost: 0.0,
                            producer_requirements: if id == 2 {
                                vec![ProducerRequirement {
                                    compatible_producers: vec![
                                        PhysicalCandidateId(0),
                                        PhysicalCandidateId(1),
                                    ]
                                    .into(),
                                }]
                            } else {
                                vec![]
                            },
                        })
                        .collect(),
                    query_roots: vec![QueryRootCandidates {
                        compatible_candidates: vec![PhysicalCandidateId(2)],
                    }],
                    ..Default::default()
                },
                templates: vec![Some(m), Some(n), Some(project.clone())],
                group_candidates: vec![],
            },
            configurations: (0..3)
                .map(|id| CandidateConfiguration {
                    source_candidate: PhysicalCandidateId(id),
                    materialization: Materialization::NotMaterialized,
                    timing: ExecutionTiming::QueryTime,
                })
                .collect(),
        };
        let roots = vec![QueryRoot::Operator(project)];
        let entries = vec![QueryWorkloadEntry {
            query: Query("m".into()),
            requirements: QueryRequirements {
                accuracy: AccuracyTarget::exact(),
                max_latency_ms: None,
            },
            predictability: Predictability::Predictable { known_at: None },
            recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                RepetitionInterval(60_000),
            )),
            range_evaluation: None,
        }];
        let data: DataWorkload = serde_json::from_value(serde_json::json!({
            "arrival": {"ingesting": {"rows_per_second": 100.0}},
            "sources": {"m": {"rows_held":10,"series_count":1},"n":{"rows_held":100,"series_count":1}}
        })).unwrap();
        let context = CandidatePlanningContext {
            deadline: None,
            refine_materialization: false,
            original_roots: &roots,
            entries: &entries,
            data: &data,
            models: crate::PlanningModels::builtin(),
            calibration: crate::CalibrationFit::default(),
        };
        let extracted = extract(&configured, &context).unwrap().unwrap();
        assert_eq!(extracted.supported, vec![true, true, true]);
        assert_eq!(extracted.suppliers[2], vec![PhysicalCandidateId(0)]);
        assert!(
            extracted.profile.nodes[1].execution_cost > extracted.profile.nodes[0].execution_cost
        );
        assert_eq!(
            configured.inventory.graph.candidates[2].producer_requirements[0]
                .compatible_producers
                .len(),
            2,
            "full catalog remains unchanged"
        );
        let timed = CandidatePlanningContext {
            deadline: Some(std::time::Instant::now()),
            ..context
        };
        assert!(matches!(
            extract(&configured, &timed),
            Err(DirectProfileError::DeadlineExceeded(_))
        ));
    }
}
