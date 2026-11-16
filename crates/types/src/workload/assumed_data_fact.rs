//! A data fact the data workload does not declare, which the planner
//! replaced by a written default: what it assumed, for which sources, and
//! whether a plan's accuracy target depends on the assumption. The plan
//! output names each one as a warning, so a deployment knows which facts to
//! declare.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::ValueMagnitudeRange;

/// One assumption a plan rests on. E.g. a DDSketch over `latency`'s
/// `value`, whose source declares no `column_magnitudes`, is costed for
/// magnitudes in [1, 1e9]:
/// `AssumedDataFact { sources: ["latency"], assumption:
/// DDSketchValueMagnitudeRange { column: "value", assumed_range: [1, 1e9] } }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssumedDataFact {
    /// The sources (PromQL metrics or SQL tables) the assumption is about,
    /// those that do not declare the fact, in name order.
    pub sources: Vec<String>,
    /// What was assumed in place of the undeclared fact.
    pub assumption: DataFactAssumption,
}

/// The default value the planner used for one undeclared fact. JSON: the
/// snake_case variant name, e.g. `{"ddsketch_value_magnitude_range":
/// {"column": "value", "assumed_range": {"min_abs": 1.0, "max_abs": 1e9}}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DataFactAssumption {
    /// The sources declare no `column_magnitudes` for `column`, so a
    /// DDSketch state over it is costed for magnitudes in `assumed_range`.
    /// `column` is `None` when the state folds values computed from the
    /// sources (e.g. rates), whose magnitudes no source fact declares.
    /// Cost only: the DDSketch store grows to whatever range the values
    /// span, so its relative error α holds for any range.
    #[serde(rename = "ddsketch_value_magnitude_range")]
    DDSketchValueMagnitudeRange {
        column: Option<String>,
        assumed_range: ValueMagnitudeRange,
    },
    /// The data arrival declares no `max_out_of_order_ms`, so the PromQL
    /// samples an ingestion-time `rate`, `increase` or `irate` state folds
    /// are taken to arrive in timestamp order. A sample arriving late is
    /// read as a counter reset, so the exact answer may be wrong.
    InOrderArrival,
}

impl AssumedDataFact {
    /// Whether the selected plan may miss a query's accuracy target when
    /// the assumption does not hold; otherwise only its cost is assumed.
    pub fn accuracy_target_depends_on_it(&self) -> bool {
        match self.assumption {
            DataFactAssumption::DDSketchValueMagnitudeRange { .. } => false,
            DataFactAssumption::InOrderArrival => true,
        }
    }
}

/// The warning line, e.g. "assumed data fact: source latency declares no
/// column_magnitudes for value, so a DDSketch state over it is costed for
/// magnitudes in [1, 1000000000]; this affects cost only, the accuracy
/// guarantee does not depend on it".
impl fmt::Display for AssumedDataFact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sources = match self.sources.as_slice() {
            [one] => format!("source {one}"),
            many => format!("sources {}", many.join(", ")),
        };
        write!(f, "assumed data fact: ")?;
        match &self.assumption {
            DataFactAssumption::DDSketchValueMagnitudeRange {
                column,
                assumed_range,
            } => {
                match column {
                    Some(column) => write!(
                        f,
                        "{sources} declares no column_magnitudes for {column}, so a DDSketch \
                         state over it"
                    )?,
                    None => write!(
                        f,
                        "a DDSketch state folds values computed from {sources}, whose \
                         magnitudes no source declares, so it"
                    )?,
                }
                write!(
                    f,
                    " is costed for magnitudes in [{}, {}]",
                    assumed_range.min_abs, assumed_range.max_abs
                )?
            }
            DataFactAssumption::InOrderArrival => write!(
                f,
                "the data arrival declares no max_out_of_order_ms, so the samples of {sources} \
                 an ingestion-time rate, increase or irate state folds are taken to arrive in \
                 timestamp order"
            )?,
        }
        if self.accuracy_target_depends_on_it() {
            write!(
                f,
                "; the plan may not meet its accuracy target if this does not hold"
            )
        } else {
            write!(
                f,
                "; this affects cost only, the accuracy guarantee does not depend on it"
            )
        }
    }
}

/// Adds `fact` to `facts` unless an equal one is there: a plan names each
/// assumption once, however many of its nodes rest on it.
pub fn add_assumed_data_fact(facts: &mut Vec<AssumedDataFact>, fact: AssumedDataFact) {
    if !facts.contains(&fact) {
        facts.push(fact);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One source is named alone, several in a list; an accuracy-affecting
    /// assumption says the plan may miss its target.
    #[test]
    fn the_warning_names_the_sources_the_default_and_the_accuracy_effect() {
        let ddsketch = AssumedDataFact {
            sources: vec!["latency".into()],
            assumption: DataFactAssumption::DDSketchValueMagnitudeRange {
                column: Some("value".into()),
                assumed_range: ValueMagnitudeRange {
                    min_abs: 1.0,
                    max_abs: 1e9,
                },
            },
        };
        assert_eq!(
            ddsketch.to_string(),
            "assumed data fact: source latency declares no column_magnitudes for value, so a \
             DDSketch state over it is costed for magnitudes in [1, 1000000000]; this affects \
             cost only, the accuracy guarantee does not depend on it"
        );
        let computed = AssumedDataFact {
            sources: vec!["requests_total".into()],
            assumption: DataFactAssumption::DDSketchValueMagnitudeRange {
                column: None,
                assumed_range: ValueMagnitudeRange {
                    min_abs: 1.0,
                    max_abs: 1e9,
                },
            },
        };
        assert_eq!(
            computed.to_string(),
            "assumed data fact: a DDSketch state folds values computed from source \
             requests_total, whose magnitudes no source declares, so it is costed for \
             magnitudes in [1, 1000000000]; this affects cost only, the accuracy guarantee \
             does not depend on it"
        );
        let in_order = AssumedDataFact {
            sources: vec!["a_total".into(), "b_total".into()],
            assumption: DataFactAssumption::InOrderArrival,
        };
        assert_eq!(
            in_order.to_string(),
            "assumed data fact: the data arrival declares no max_out_of_order_ms, so the \
             samples of sources a_total, b_total an ingestion-time rate, increase or irate \
             state folds are taken to arrive in timestamp order; the plan may not meet its \
             accuracy target if this does not hold"
        );
        let mut facts = vec![];
        add_assumed_data_fact(&mut facts, in_order.clone());
        add_assumed_data_fact(&mut facts, in_order);
        assert_eq!(facts.len(), 1);
    }
}
