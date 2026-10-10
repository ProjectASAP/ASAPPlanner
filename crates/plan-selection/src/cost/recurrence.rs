//! Query recurrence as an evaluation rate (issue #287): Stage 3 charges a
//! query-time node per evaluation, at the summed rate of the repeating roots
//! that reach it.

use asap_types::workload::RepetitionInterval;

/// How often a target is *evaluated* by its consumers — evaluations per
/// second (Hz). For a summary shared by repeating consumers with intervals
/// `t1..tn`, `sum(1 / t_i)` ([`evaluation_rate_of`]).
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct EvaluationRate(pub f64);

/// Errors from deriving an evaluation rate.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum RecurrenceError {
    /// A [`RepetitionInterval`] of zero was supplied (`RepetitionInterval` is
    /// `u32`-backed, so zero is the only non-positive value). A zero interval
    /// has no finite rate (`1 / 0`), so it cannot contribute to an
    /// [`EvaluationRate`].
    #[error(
        "invalid RepetitionInterval({0:?}ms): a repeating query's interval must be > 0 to \
         contribute a finite evaluation rate"
    )]
    InvalidInterval(RepetitionInterval),
}

/// `sum(1 / interval_i)`, converted from milliseconds
/// ([`RepetitionInterval`]'s own unit) to Hz, over every repeating
/// consumer's interval. `Ok(None)` when `intervals` is empty — "no
/// repeating consumers observed", distinct from "observed consumers whose
/// rate happens to be zero" (impossible: every valid interval contributes a
/// strictly positive rate). `Err` on the first zero interval encountered.
pub fn evaluation_rate_of<I>(intervals: I) -> Result<Option<EvaluationRate>, RecurrenceError>
where
    I: IntoIterator<Item = RepetitionInterval>,
{
    let mut total_hz = 0.0;
    let mut any = false;
    for interval in intervals {
        if interval.0 == 0 {
            return Err(RecurrenceError::InvalidInterval(interval));
        }
        any = true;
        // RepetitionInterval is in milliseconds; Hz = 1000 / ms.
        total_hz += 1000.0 / f64::from(interval.0);
    }
    Ok(any.then_some(EvaluationRate(total_hz)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interval(ms: u32) -> RepetitionInterval {
        RepetitionInterval(ms)
    }

    // ── evaluation_rate_of ──────────────────────────────────────────────

    #[test]
    fn evaluation_rate_of_empty_is_none() {
        assert_eq!(evaluation_rate_of(vec![]).unwrap(), None);
    }

    #[test]
    fn evaluation_rate_of_single_interval() {
        // 1000ms interval => 1 Hz.
        let rate = evaluation_rate_of(vec![interval(1000)]).unwrap().unwrap();
        assert!((rate.0 - 1.0).abs() < 1e-9);
    }

    #[test]
    fn evaluation_rate_of_mixed_intervals_sums_reciprocals() {
        // 1s, 10s, 100s intervals => 1 + 0.1 + 0.01 Hz.
        let rate = evaluation_rate_of(vec![interval(1_000), interval(10_000), interval(100_000)])
            .unwrap()
            .unwrap();
        assert!((rate.0 - 1.11).abs() < 1e-9, "rate={}", rate.0);
    }

    #[test]
    fn evaluation_rate_of_rejects_zero_interval() {
        let err = evaluation_rate_of(vec![interval(1000), interval(0)]).unwrap_err();
        assert_eq!(err, RecurrenceError::InvalidInterval(interval(0)));
    }
}
