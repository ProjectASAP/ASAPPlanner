//! Deserialized physical plans recover selected operators without logical lowering.
//! Deployments choose the encoding; JSON is used here only as a test format.
use asap_physical_operators::{
    operators::{Operator, SortKey},
    physical_planner::{InputContract, PhysicalPostASAPDAG},
};
use planner_types::{
    post_asap::{SummaryFamilyType, SummaryField, SummarySchema},
    pre_asap::DataType,
};
use std::{collections::BTreeMap, sync::Arc};

fn sorted() -> PhysicalPostASAPDAG {
    let schema = Arc::new(SummarySchema {
        fields: vec![SummaryField {
            name: "value".into(),
            dtype: SummaryFamilyType::Plain(DataType::Float64),
            nullable: false,
        }],
        time_index: None,
    });
    PhysicalPostASAPDAG::from_operators(
        BTreeMap::from([(0, InputContract::bounded(schema.clone()))]),
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

#[test]
fn recovery_retains_selected_operator_and_rejects_invalid_contracts() {
    let bytes = serde_json::to_vec(&sorted()).unwrap();
    let recovered = serde_json::from_slice::<PhysicalPostASAPDAG>(&bytes).unwrap();
    assert_eq!(serde_json::to_vec(&recovered).unwrap(), bytes);
    for mutation in ["column", "edge", "output"] {
        let mut wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        match mutation {
            "column" => {
                wire["nodes"]["1"]["Operator"]["operator"]["kind"]["Sort"]["keys"][0]["column"] =
                    7.into()
            }
            "edge" => wire["nodes"]["1"]["Operator"]["inputs"][0] = 999.into(),
            "output" => {
                wire["nodes"]["1"]["Operator"]["operator"]["output"]["fields"][0]["dtype"] =
                    serde_json::json!({"Plain":"utf8"})
            }
            _ => unreachable!(),
        }
        assert!(
            serde_json::from_slice::<PhysicalPostASAPDAG>(&serde_json::to_vec(&wire).unwrap())
                .is_err(),
            "accepted {mutation}"
        );
    }
}

#[test]
fn candidate_recovery_preserves_materialization_boundary() {
    use asap_physical_operators::physical_planner::PhysicalCandidate;
    let precompute = sorted();
    let output = InputContract::bounded(precompute.output_contract(1).unwrap().schema);
    let query = PhysicalPostASAPDAG::from_operators(
        BTreeMap::from([(1, output.clone())]),
        BTreeMap::from([(
            2,
            (
                vec![1],
                Operator::limit(output.schema.clone(), 3, 0, vec![]).unwrap(),
            ),
        )]),
        vec![2],
    )
    .unwrap();
    let candidate = PhysicalCandidate {
        precompute: Some(precompute),
        query,
        materialized_outputs: BTreeMap::from([(1, output)]),
    };
    let bytes = serde_json::to_vec(&candidate).unwrap();
    let restored = serde_json::from_slice::<PhysicalCandidate>(&bytes).unwrap();
    assert_eq!(restored.precompute.as_ref().unwrap().roots(), &[1]);
    assert_eq!(restored.query.roots(), &[2]);
    assert_eq!(serde_json::to_vec(&restored).unwrap(), bytes);
    let mut wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    wire["materialized_outputs"]["1"]["schema"]["fields"][0]["dtype"] =
        serde_json::json!({"Plain":"utf8"});
    assert!(
        serde_json::from_slice::<PhysicalCandidate>(&serde_json::to_vec(&wire).unwrap()).is_err()
    );
}
