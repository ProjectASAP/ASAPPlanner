use super::*;
use planner_types::summary_type::ExactParams;

// An exact updater charges the buffer it holds (its capacity), the
// same bytes its sealed column charges.
#[test]
fn exact_memory_is_the_buffer_capacity() {
    let family = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
    for groups in [1, 5, 17] {
        let mut updater = StateColumnUpdater::new(&family, None).unwrap();
        updater.ensure_groups(groups).unwrap();
        let charged = updater.memory_bytes();
        let GroupStates::Exact { states, .. } = &updater.states else {
            unreachable!()
        };
        assert_eq!(
            charged,
            states.capacity() * std::mem::size_of::<ScalarState>()
        );
        assert_eq!(
            charged,
            updater.seal().charged_state_bytes(),
            "{groups} groups"
        );
    }
}
