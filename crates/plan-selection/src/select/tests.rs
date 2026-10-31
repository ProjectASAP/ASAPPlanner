use super::*;
use crate::milp::model::tests::{approximate, cost_inputs, shared_sum, through_stages, Fixture};
use asap_accuracy::{AccuracyModel, BuiltinAccuracyModel, NoAccuracyEvidence};
use asap_types::ir::properties::{AccuracyGuarantee, AccuracyTarget};
use asap_types::stages::RealizedPlan;

fn realizer(fixture: &Fixture) -> impl AssignmentRealizer + '_ {
    asap_physical_asap_optimization::catalog::Stage2AssignmentRealizer {
        original_roots: &fixture.roots,
        entries: &fixture.entries,
        data: &fixture.data,
        query_time_results_retained: fixture.query_time_results_retained,
    }
}

fn select(
    fixture: &Fixture,
    accuracy: &dyn AccuracyModel,
    realizer: &dyn AssignmentRealizer,
) -> Result<Selection, ErrorSelectingPlan> {
    select_plan_with_milp(
        &fixture.physical,
        &fixture.entries,
        &fixture.data,
        &AccuracyCheckInputs {
            model: accuracy,
            evidence: &NoAccuracyEvidence,
        },
        &cost_inputs(),
        realizer,
        None,
    )
}

// The new search's selection: an optimal, valid plan whose cost is
// `plan_cost`'s, the MILP's size and every query's accepted accuracy.
#[test]
fn the_milp_search_selects_a_checked_plan() {
    // A ratio of quantiles is not here: on this fixture the A1 rows admit
    // two sketches at parts = 2 for which Stage 1 composes no guarantee,
    // so the search returns `SelectedPlanFailedCheck` (an open question
    // on #1095 C5).
    for fixture in [
        shared_sum(),
        through_stages(
            &[approximate("quantile_over_time(0.9, m[5m])", 60_000, 0.01)],
            true,
        ),
    ] {
        let stage2 = realizer(&fixture);
        let selection = select(&fixture, &BuiltinAccuracyModel, &stage2).unwrap();
        selection.selected_dag.validate(&fixture.physical).unwrap();
        let report = &selection.search_report;
        assert_eq!(report.status, SearchStatus::Optimal);
        assert_eq!(report.inexact_objective, None);
        assert!(report.milp_variables > 0 && report.milp_constraints > 0);
        let cost = selection.cost.cost_per_second.unwrap();
        assert!(report.relative_gap(cost).unwrap() <= 1e-6, "{report:?}");
        assert_eq!(
            selection.accepted_accuracy_per_query.len(),
            fixture.entries.len()
        );
        for pruned in &selection.pruned_candidates {
            assert!(matches!(
                pruned.reason,
                PruneReason::DominatedBy(_) | PruneReason::UsageNotCostable
            ));
            assert_eq!(
                pruned.group,
                fixture.physical.memo_group_of(pruned.candidate)
            );
        }
    }
}

/// A realizer that rejects every selection.
struct Rejecting;

impl AssignmentRealizer for Rejecting {
    fn realize(
        &self,
        _physical: &PhysicalASAPMemoCandidates,
        _selected: &DagOfSelectedCandidates,
    ) -> Result<RealizedPlan, PhysicalCandidateError> {
        Err(PhysicalCandidateError::RejectedAssignment(
            "a combination the catalog allows".into(),
        ))
    }
}

// A realizer rejection is a Stage 2 catalog bug: an error, no re-solve.
#[test]
fn a_realizer_rejection_is_realizer_rejected() {
    let fixture = shared_sum();
    match select(&fixture, &BuiltinAccuracyModel, &Rejecting) {
        Err(ErrorSelectingPlan::RealizerRejected(reason)) => {
            assert!(reason.contains("the catalog allows"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
}

/// An accuracy model that accepts no guarantee, exact included, and never
/// accepts an output otherwise: the A1 rows still run, so the MILP's plan
/// misses `satisfies`, as it would with a wrong weight table.
struct AcceptsNothing;

impl AccuracyModel for AcceptsNothing {
    fn satisfies(&self, _guarantee: &AccuracyGuarantee, _target: &AccuracyTarget) -> bool {
        false
    }

    fn may_accept_regardless_of_guarantee(&self) -> bool {
        false
    }
}

// A selected plan that misses `satisfies` is an error, never returned.
#[test]
fn an_accuracy_miss_is_an_error() {
    let fixture = shared_sum();
    let stage2 = realizer(&fixture);
    match select(&fixture, &AcceptsNothing, &stage2) {
        Err(ErrorSelectingPlan::SelectedPlanFailedCheck { cause, .. }) => {
            assert!(cause.failed_a_check(), "{cause:?}")
        }
        other => panic!("{other:?}"),
    }
}

// Cost models 2 and 3 are not supported by the new search.
#[test]
fn cost_model_2_is_unsupported() {
    let fixture = shared_sum();
    let costs = CostInputs {
        cost_model: asap_cost::CostModel::ProvisionedCapacity(
            asap_cost::ResourceUnitCost::illustrative(),
        ),
        ..cost_inputs()
    };
    let result = select_plan_with_milp(
        &fixture.physical,
        &fixture.entries,
        &fixture.data,
        &AccuracyCheckInputs {
            model: &BuiltinAccuracyModel,
            evidence: &NoAccuracyEvidence,
        },
        &costs,
        &Rejecting,
        None,
    );
    assert!(matches!(
        result,
        Err(ErrorSelectingPlan::UnsupportedCostModel(_))
    ));
}
