//! #509 Stage 2 decision data: the physical properties of each candidate:
//! when it runs and how long its output lives ([`PhysicalProperties`],
//! [`Lifetime`], [`StorageMedium`]).
pub mod properties;

pub use properties::{
    ExecutionTiming, Lifetime, PhysicalProperties, PhysicalPropertiesError, StorageMedium,
};
