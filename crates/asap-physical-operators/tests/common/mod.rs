#![allow(dead_code)]
use planner_types::ir::export::PostAsapDag;
use planner_types::ir::{apply_lifecycle_timings, LifecycleAssignment, OperatorNode, TimingMemo};
use std::rc::Rc;

pub fn compile_post_asap_dag(
    root: &Rc<OperatorNode>,
) -> Result<PostAsapDag, Box<dyn std::error::Error>> {
    let root = apply_lifecycle_timings(
        root,
        &LifecycleAssignment::default(),
        &mut TimingMemo::default(),
    )?;
    Ok(planner_types::ir::export::compile_post_asap_dag(&root)?)
}
