//! What a runtime shares across threads: the compiled plan, its operators
//! and expressions, the raw-data-sample selectors, batches and state columns.
use asap_executor::expressions::CompiledExpression;
use asap_executor::operators::Operator;
use asap_executor::physical_planner::CompiledPhysicalDAG;
use asap_executor::values::{Batch, StateColumn};
use asap_executor::{RawDataSampleSelector, StateColumnUpdater};

fn shareable<T: Send + Sync>() {}
fn movable<T: Send>() {}

// A runtime compiles a plan once and runs it on any thread.
#[test]
fn the_compiled_plan_and_selectors_are_send_and_sync() {
    shareable::<CompiledPhysicalDAG>();
    shareable::<Operator>();
    shareable::<CompiledExpression>();
    shareable::<RawDataSampleSelector>();
    shareable::<Batch>();
    shareable::<StateColumn>();
    // An open pane's updater moves to the worker that ingests its shard.
    movable::<StateColumnUpdater>();
}
