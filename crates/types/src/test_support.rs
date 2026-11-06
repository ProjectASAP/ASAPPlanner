//! Helpers that only tests call (this crate's integration tests and other
//! crates' tests), exposed under the `test-support` feature so the normal
//! public API stays as narrow as its production users need. Each forwards
//! to the crate-private function of the same name, except the fixture
//! [`population_over_selector`], which the executor's tests build.

use crate::ir::materialized_view::RawDataWindow;
use crate::ir::operator::maintained_population::CurrentSeriesInput;
use crate::ir::operator::node::Operator;
use crate::ir::properties::summary_coverage::SummaryCoverage;

use crate::ir::schema::Schema;
use crate::ir::{ASAPOp, NonASAPOp};
use crate::ir::{OperatorNode, SchemaDerivationError};
use std::rc::Rc;

/// `SummaryCoverage::definition_id`.
pub fn definition_id(coverage: &SummaryCoverage) -> String {
    coverage.definition_id()
}

/// `Operator::output_schema`.
pub fn output_schema(operator: &Operator) -> Result<Schema, SchemaDerivationError> {
    operator.output_schema()
}

/// `NonASAPOp::validate_inputs`.
pub fn validate_non_asap_inputs(op: &NonASAPOp) -> Result<(), SchemaDerivationError> {
    op.validate_inputs()
}

/// `ASAPOp::validate_inputs`.
pub fn validate_asap_inputs(op: &ASAPOp) -> Result<(), SchemaDerivationError> {
    op.validate_inputs()
}

/// `CurrentSeriesInput::matches_node`.
pub fn matches_node(input: &CurrentSeriesInput, node: &Rc<OperatorNode>) -> bool {
    input.matches_node(node)
}

/// `RawDataWindow::lookback_ms`.
pub fn lookback_ms(window: &RawDataWindow) -> u64 {
    window.lookback_ms()
}

/// `physical_export::physical_asap_dag_at_query_time_with_node_ids`: `roots`
/// exported with every node at query time, not materialized and without θ,
/// with each exported node's operator node by node id.
pub fn physical_asap_dag_at_query_time_with_node_ids(
    roots: &[crate::ir::QueryRoot],
) -> Result<
    (
        crate::ir::physical_export::PhysicalASAPDAG,
        Vec<Rc<OperatorNode>>,
    ),
    crate::ir::physical_export::PhysicalASAPDAGValidationError,
> {
    crate::ir::physical_export::physical_asap_dag_at_query_time_with_node_ids(roots)
}

/// [`physical_asap_dag_at_query_time_with_node_ids`] of the single root `root`, without
/// the node ids.
pub fn physical_asap_dag_at_query_time(
    root: &Rc<OperatorNode>,
) -> Result<
    crate::ir::physical_export::PhysicalASAPDAG,
    crate::ir::physical_export::PhysicalASAPDAGValidationError,
> {
    Ok(
        physical_asap_dag_at_query_time_with_node_ids(&[crate::ir::QueryRoot::Operator(
            Rc::clone(root),
        )])?
        .0,
    )
}

/// The maintained-population sub-DAG for executor tests: `root` (below its
/// projections, which are kept) replaced by `EvaluatePopulation(statistic)`
/// over a current-series population of the single selector under it,
/// grouped by `grouping`, with no matchers. The caller states the statistic
/// and grouping; nothing is recognized. No planner pass offers this sub-DAG
/// yet (#681).
pub fn population_over_selector(
    root: &Rc<OperatorNode>,
    statistic: crate::ir::operator::maintained_population::PopulationStatistic,
    grouping: &[&str],
) -> Rc<OperatorNode> {
    use crate::ir::operator::maintained_population::*;
    use crate::ir::operator::operator_properties::Source;
    if let Some(NonASAPOp::Project {
        cols,
        qualifier,
        child,
    }) = root.non_asap()
    {
        let child = population_over_selector(child, statistic.clone(), grouping);
        let project = Operator::NonASAP(NonASAPOp::Project {
            cols: cols.clone(),
            qualifier: qualifier.clone(),
            child,
        });
        return Rc::new(OperatorNode::with_schema(project, root.schema.clone()));
    }
    let selectors: Vec<_> = OperatorNode::reachable(root)
        .into_iter()
        .filter(|node| matches!(node.non_asap(), Some(NonASAPOp::TimeRange { .. })))
        .collect();
    let [selector] = selectors.as_slice() else {
        panic!("one selector under {root:?}");
    };
    let Some(NonASAPOp::TimeRange { range, child, .. }) = selector.non_asap() else {
        unreachable!()
    };
    let Some(NonASAPOp::Scan {
        source: Source::TimeSeries { metric },
        ..
    }) = child.non_asap()
    else {
        panic!("a time-series selector, got {child:?}");
    };
    let population = MaintainedPopulation {
        input: PopulationInput::CurrentSeries(CurrentSeriesInput {
            metric: metric.clone(),
            matchers: Vec::new(),
            grouping: grouping.iter().map(|label| label.to_string()).collect(),
            without: false,
            lookback_ms: range.as_millis() as u64,
        }),
        max_k: match statistic {
            PopulationStatistic::TopK { k } => k,
            _ => 0,
        },
        quantiles: matches!(statistic, PopulationStatistic::Quantile { .. }),
    };
    let maintained = OperatorNode::with_schema(
        Operator::ASAP(ASAPOp::MaintainPopulation {
            child: selector.clone(),
            population,
        }),
        selector.schema.clone(),
    );
    let evaluation = Operator::ASAP(ASAPOp::EvaluatePopulation {
        child: Rc::new(maintained),
        evaluation: statistic,
    });
    Rc::new(OperatorNode::with_schema(evaluation, root.schema.clone()))
}
