#![allow(dead_code)]
use planner_types::ir::export::PhysicalASAPDAG;
use planner_types::ir::{apply_lifecycle_timings, LifecycleAssignment, OperatorNode, TimingMemo};
use std::rc::Rc;

pub fn compile_physical_asap_dag(
    root: &Rc<OperatorNode>,
) -> Result<PhysicalASAPDAG, Box<dyn std::error::Error>> {
    let root = apply_lifecycle_timings(
        root,
        &LifecycleAssignment::default(),
        &mut TimingMemo::default(),
    )?;
    Ok(planner_types::ir::export::compile_physical_asap_dag(&root)?)
}
