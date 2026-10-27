//! A compiled physical plan checks its producer/reader boundary.
use asap_executor::{
    operators::{Operator, SortKey},
    physical_planner::CompiledPhysicalDAG,
};
use planner_types::ir::schema::{DataType, Field, FieldDataType};
use std::{collections::BTreeMap, sync::Arc};

fn sorted() -> CompiledPhysicalDAG {
    let schema = Arc::new(planner_types::ir::schema::Schema {
        unique_keys: vec![],
        closed: false,
        fields: vec![Field {
            table: None,
            name: "value".into(),
            dtype: FieldDataType::Plain(DataType::Float64),
            nullable: false,
        }],
        time_index: None,
    });
    CompiledPhysicalDAG::from_operators(
        BTreeMap::from([(0, schema.clone())]),
        BTreeMap::from([(
            1,
            (
                vec![0],
                Operator::sort(
                    schema,
                    vec![SortKey {
                        column: 0,
                        descending: true,
                        nulls_first: false,
                    }],
                    vec![],
                )
                .unwrap(),
            ),
        )]),
        vec![1],
    )
    .unwrap()
}

/// A plan whose stored output schema differs from what its precompute DAG
/// produces is rejected.
#[test]
fn candidate_validation_checks_materialization_boundary() {
    use asap_executor::physical_planner::CompiledPhysicalPlan;
    let precompute = sorted();
    let output = precompute.output_schema(1).unwrap();
    let query = CompiledPhysicalDAG::from_operators(
        BTreeMap::from([(1, output.clone())]),
        BTreeMap::from([(
            2,
            (
                vec![1],
                Operator::limit(output.clone(), 3, 0, vec![]).unwrap(),
            ),
        )]),
        vec![2],
    )
    .unwrap();
    let mut candidate = CompiledPhysicalPlan {
        precompute: Some(precompute),
        query,
        materialized_outputs: BTreeMap::from([(1, output.clone())]),
    };
    candidate.validate().unwrap();
    let mut utf8 = (*output).clone();
    utf8.fields[0].dtype = FieldDataType::Plain(DataType::Utf8);
    candidate.materialized_outputs.insert(1, Arc::new(utf8));
    assert!(candidate.validate().is_err());
}
