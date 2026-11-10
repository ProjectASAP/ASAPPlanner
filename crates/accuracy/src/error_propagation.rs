//! Each operator's local error propagation: the linear bound $g_c$ on a
//! consumer's output error from its inputs' errors (concepts doc, section 4).

use asap_types::ir::properties::AccuracyMetric;
use asap_types::ir::Operator;
use asap_types::stages::MemoGroupId;

/// $g_c$'s linear bound for errors in `metric`, each input's and the
/// output's: $\beta_{c,i}$ and $b_{c,i}$ per input, read from the
/// operator and its arguments. `None` when the consumer has no error
/// contract for `metric`: an input whose error is in `metric` then gives
/// no edge (the metric rule). No factor is guessed.
pub fn error_propagation(
    consumer: &Operator<MemoGroupId>,
    metric: AccuracyMetric,
) -> Option<ErrorPropagation> {
    let _ = (consumer, metric);
    todo!("#1207 PR 4: the error contract of each operator, per metric")
}

/// The local error propagation of one consumer: its output error is at
/// most $\sum_i \beta_{c,i}\varepsilon_{c,i}$ while each input's error stays
/// within its validity cap (concepts doc, section 4; approved 2026-11-05).
#[derive(Debug, Clone, PartialEq)]
pub struct ErrorPropagation {
    /// $\beta_{c,i}$ per input, for that input's metric.
    pub factor_per_input: Vec<f64>,
    /// $b_{c,i}$ per input: the cap under which the linear bound holds;
    /// `None` when the bound holds everywhere (e.g. a ratio caps only its
    /// denominator).
    pub validity_cap_per_input: Vec<Option<f64>>,
}
