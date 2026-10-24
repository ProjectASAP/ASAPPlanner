//! The memo solved as a mixed-integer linear program: the model and its
//! coefficients.

#![allow(dead_code)]

pub(crate) mod clusters;
pub(crate) mod coefficients;
pub(crate) mod cuts;
pub(crate) mod decode;
pub(crate) mod model;
pub(crate) mod report;

// Candidate-connection core being integrated into the staged memo pipeline.
pub(crate) mod candidate_connections;

pub(crate) mod candidate_reconstruction;
