//! Measurement values and uncertainty metadata shared across resource dimensions.

use serde::{Deserialize, Serialize};

/// A measured quantity whose unit and operation scope are set by its field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measurement {
    pub value: f64,
    pub stddev: Option<f64>,
    pub samples: u32,
    /// Measurement procedure and scope, such as allocator heap versus payload.
    #[serde(default)]
    pub method: Option<String>,
}
