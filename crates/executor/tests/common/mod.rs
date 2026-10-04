#![allow(dead_code)]
use planner_types::ir::physical_export::PhysicalASAPDAG;
use planner_types::ir::{
    apply_materialization_timings, MaterializationAssignment, OperatorNode, TimingMemo,
};
use std::rc::Rc;

pub fn compile_physical_asap_dag(
    root: &Rc<OperatorNode>,
) -> Result<PhysicalASAPDAG, Box<dyn std::error::Error>> {
    let root = apply_materialization_timings(
        root,
        &MaterializationAssignment::default(),
        &mut TimingMemo::default(),
    )?;
    Ok(planner_types::ir::physical_export::compile_physical_asap_dag(&root)?)
}
