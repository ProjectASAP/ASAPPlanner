//! The memo solved as a mixed-integer linear program: the model and its
//! coefficients.

#![allow(dead_code)]

pub(crate) mod clusters;
pub(crate) mod coefficients;
pub(crate) mod cuts;
pub(crate) mod decode;
pub(crate) mod model;
pub(crate) mod sizing;
