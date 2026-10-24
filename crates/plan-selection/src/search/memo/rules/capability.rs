//! Compatibility imports during the shared-state rule migration.
//! Logical Pass 2 expansion and build resizing live with the logical memo;
//! selection only chooses configurations and prices their physical execution.

pub(crate) use asap_logical_asap_optimization::memo::shared_builds::{
    add_shared_builds, resized_at_threshold, share_equal_builds, shared_build_of, size_thresholds,
};
