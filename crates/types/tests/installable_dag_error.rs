//! Which install rules Stage 3 can act on by selecting again: a rule over
//! one node's subtree names that node (`InstallableDagError::node_id`); a
//! rule over a node's readers or over the whole DAG names none.
use asap_types::plan::materialized_view::InstallableDagError;
use asap_types::plan::physical_asap_dag::PhysicalASAPDAGValidationError;

/// Each variant with the node `node_id` returns: the subtree-local rules
/// name their node (for `UnsupportedRawDataWindow`, its `TimeRange`); the
/// retention rules, which depend on the readers Stage 3 sets the retention
/// from, and an invalid DAG name none.
#[test]
fn node_id_names_the_node_of_a_subtree_local_rule_only() {
    let cases: Vec<(InstallableDagError, Option<usize>)> = vec![
        (
            InstallableDagError::UnsupportedRawDataWindow {
                time_range_node_id: 3,
                reason: "a subquery or a TimeShift above the range",
            },
            Some(3),
        ),
        (
            InstallableDagError::ViewReadsUnwindowedSource { node_id: 2 },
            Some(2),
        ),
        (
            InstallableDagError::ViewWithoutCoverage {
                node_id: 4,
                reason: "not a summary state".into(),
            },
            Some(4),
        ),
        (
            InstallableDagError::IngestionBuildReadsWindowsOfDifferentOffsets {
                node_id: 5,
                end_offsets_ms: vec![0, 90_000],
            },
            Some(5),
        ),
        (
            InstallableDagError::ViewWithoutRetention { node_id: 1 },
            None,
        ),
        (
            InstallableDagError::ViewRetentionTooShort { node_id: 1 },
            None,
        ),
        (
            InstallableDagError::ScanWithoutRetention { node_id: 0 },
            None,
        ),
        (
            InstallableDagError::ScanRetentionTooShort { node_id: 0 },
            None,
        ),
        (
            InstallableDagError::InvalidDag(PhysicalASAPDAGValidationError::Cycle),
            None,
        ),
    ];
    for (error, expected) in cases {
        assert_eq!(error.node_id(), expected, "{error:?}");
    }
}
