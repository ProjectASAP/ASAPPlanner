//! The per-value rules of SUM, AVG, MIN and MAX that every exact reduction
//! shares: the `Aggregate` ([`super::Reduction`]) over a group's rows and a
//! SQL window function over a frame's rows reduce the same rows to the same
//! value. An Int64 sum is exact (`i128`) and fails only when the result
//! leaves Int64; a Float64 sum is compensated; MIN and MAX of Float64 skip
//! NaN while a number exists: MAX over {1, NaN} is 1, over {NaN} NaN.
use super::kahan_inc;
use crate::values::Value;
use crate::Error;

/// Prometheus `avg` as a running state: a compensated sum until it would
/// overflow, then an incremental mean (see `super::promql_avg`).
#[derive(Clone, Copy, Default)]
pub(in crate::operators) struct RunningAvg {
    sum: f64,
    compensation: f64,
    mean: f64,
    incremental: bool,
    pub(in crate::operators) count: u64,
}

impl RunningAvg {
    pub(in crate::operators) fn add(&mut self, value: f64) {
        if self.count == 0 {
            *self = Self {
                sum: value,
                count: 1,
                ..Self::default()
            };
            return;
        }
        self.count += 1;
        let count = self.count as f64;
        if !self.incremental {
            let (next, next_c) = kahan_inc(value, self.sum, self.compensation);
            if !next.is_infinite() {
                (self.sum, self.compensation) = (next, next_c);
                return;
            }
            self.incremental = true;
            self.mean = self.sum / (count - 1.);
            self.compensation /= count - 1.;
        }
        let q = (count - 1.) / count;
        (self.mean, self.compensation) =
            kahan_inc(value / count, q * self.mean, q * self.compensation);
    }

    pub(in crate::operators) fn value(&self) -> f64 {
        if self.count == 0 {
            return f64::NAN;
        }
        if self.incremental {
            self.mean + self.compensation
        } else {
            let count = self.count as f64;
            self.sum / count + self.compensation / count
        }
    }
}

/// A compensated Float64 sum's value: the sum plus its compensation, or
/// the sum alone once infinite.
pub(in crate::operators) fn compensated_sum_value(sum: f64, compensation: f64) -> f64 {
    if sum.is_infinite() {
        sum
    } else {
        sum + compensation
    }
}

/// An exact Int64 sum's value: an error when it leaves Int64, though its
/// partial sums may: `i64::MAX + 1 - 1` is `i64::MAX`.
pub(in crate::operators) fn integer_sum_value(sum: i128) -> Result<Value, Error> {
    i64::try_from(sum)
        .map(Value::Int64)
        .map_err(|_| Error::Invalid("integer sum overflow".into()))
}

/// Whether Float64 `new` replaces `old` as the running MIN (`min`) or MAX:
/// a number replaces NaN, NaN replaces nothing, else the smaller (larger)
/// number replaces.
pub(in crate::operators) fn float_extremum_replaces(new: f64, old: f64, min: bool) -> bool {
    match (new.is_nan(), old.is_nan()) {
        (true, _) => false,
        (false, true) => true,
        _ if min => new < old,
        _ => new > old,
    }
}

/// Whether non-NULL `new` replaces `old` as the running MIN (`min`) or
/// MAX: [`float_extremum_replaces`] for floats, else their order.
pub(in crate::operators) fn extremum_replaces(
    new: &Value,
    old: &Value,
    min: bool,
) -> Result<bool, Error> {
    if let (Value::Float64(new), Value::Float64(old)) = (new, old) {
        return Ok(float_extremum_replaces(*new, *old, min));
    }
    let order = new.compare(old)?;
    Ok(if min { order.is_lt() } else { order.is_gt() })
}
