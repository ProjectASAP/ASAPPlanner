//! What a runtime shares across threads: the compiled plan, its operators
//! and expressions, in-memory data stores, batches and state columns.
use asap_executor::expressions::CompiledExpression;
use asap_executor::operators::Operator;
use asap_executor::physical_planner::CompiledPhysicalDAG;
use asap_executor::sources::InMemoryDataStore;
use asap_executor::values::{Batch, StateColumn};
use asap_executor::StateColumnUpdater;

fn shareable<T: Send + Sync>() {}
fn movable<T: Send>() {}

// A runtime compiles a plan once and runs it on any thread, over raw rows
// it buffers once in a data store the `Scan` reads.
#[test]
fn the_compiled_plan_and_its_data_stores_are_send_and_sync() {
    shareable::<CompiledPhysicalDAG>();
    shareable::<Operator>();
    shareable::<CompiledExpression>();
    shareable::<InMemoryDataStore>();
    shareable::<Batch>();
    shareable::<StateColumn>();
    // An open pane's updater moves to the worker that ingests its shard.
    movable::<StateColumnUpdater>();
}
