//! The cost of a plan given as values of the MILP's binaries: the cheapest
//! values of the continuous variables, found without a solver.
//!
//! Every continuous variable of the model is one of:
//! - a lower bound that minimization makes tight: `v ≥ f(others)` over one or
//!   more rows (reach `a`, latency `L`, held time `G`/`H`/`W`, AND variables,
//!   rate components, raw-retention levels, …). Its cheapest value is the
//!   largest right-hand side, or 0;
//! - an equality `N = Σ a` (approximate values read);
//! - free below its upper rows only (`t`, reach through): its value is the
//!   smallest upper bound, which leaves the most room to the rows it
//!   relaxes.
//!
//! A row defines the continuous variable it bounds from below: the one with a
//! negative coefficient in `expression ≤ 0` (or any nonzero one in an
//! equality) that was created last, since the model creates each variable
//! after the variables its defining rows read. Rows of the families in
//! [`CHECK_ONLY_FAMILIES`] (upper bounds of reach, reader constraints, and
//! valid inequalities a plan's values meet anyway) define nothing. The
//! values are computed in creation order, repeated until no value changes,
//! then every row and bound is checked. A plan that fails a check is
//! infeasible as far as this completion goes (HiGHS's LP completion could
//! find another one; the warm start then just skips that plan).
//!
//! Each value is the smallest its defining rows allow, so at a plan's
//! binaries the values are the plan's own reach, latencies, held times and
//! rates, and the objective is what the model charges the plan: with the
//! exact cost terms of [`super::cost_terms`], what `asap_cost::plan_cost`
//! says (the warm start's test checks it).
use std::collections::HashMap;

use good_lp::{IntoAffineExpression, Variable};

use super::model::SelectionMilp;

/// Families whose rows only check values (upper bounds of reach, reader
/// constraints, rows tests add) and define no variable.
const CHECK_ONLY_FAMILIES: [&str; 4] = [
    "6 reach from above",
    "7 reader constraints",
    "V (slowest rate)",
    "test",
];

/// The most passes over the variables before giving up on a fixpoint.
const MOST_PASSES: usize = 64;

/// One row in `Σ coefficient · column + constant ≤ 0` (or `= 0`).
struct Row {
    /// Range into `columns` and `coefficients`.
    start: usize,
    end: usize,
    constant: f64,
    equality: bool,
}

/// The model's rows, compiled for repeated completions.
pub(crate) struct Completion {
    column_of: HashMap<Variable, usize>,
    binary: Vec<bool>,
    max: Vec<f64>,
    rows: Vec<Row>,
    columns: Vec<u32>,
    coefficients: Vec<f64>,
    /// Per column, the rows defining it.
    defining_rows: Vec<Vec<u32>>,
    /// Per column with no defining row, the rows bounding it from above.
    upper_rows: Vec<Vec<u32>>,
    /// The columns with defining rows, in creation order.
    defined: Vec<usize>,
    /// The columns with neither (free continuous variables), in creation
    /// order.
    free: Vec<usize>,
    objective: Vec<(usize, f64)>,
}

/// A completed plan.
pub(crate) struct Completed {
    /// Every variable's value, in creation order.
    pub(crate) columns: Vec<f64>,
    /// The objective (cost per second without the request fees).
    pub(crate) cost_per_second: f64,
}

impl Completion {
    pub(crate) fn new(milp: &SelectionMilp<'_>) -> Self {
        let builder = &milp.builder;
        let mut column_of = HashMap::new();
        let mut binary = Vec::new();
        let mut max = Vec::new();
        for (column, (variable, definition)) in
            builder.variables.iter_variables_with_def().enumerate()
        {
            column_of.insert(variable, column);
            binary.push(definition.is_integer());
            max.push(definition.get_max());
        }
        let count = binary.len();
        let mut rows = Vec::with_capacity(builder.rows.len());
        let mut columns = Vec::new();
        let mut coefficients = Vec::new();
        let mut defining_rows: Vec<Vec<u32>> = vec![Vec::new(); count];
        let mut positive_rows: Vec<Vec<u32>> = vec![Vec::new(); count];
        for (index, (row, family)) in builder.rows.iter().zip(&builder.row_families).enumerate() {
            let expression = row.expression();
            let start = columns.len();
            let mut defines: Option<usize> = None;
            for (variable, coefficient) in expression.linear_coefficients() {
                if coefficient == 0.0 {
                    continue;
                }
                let column = column_of[&variable];
                columns.push(column as u32);
                coefficients.push(coefficient);
                if binary[column] {
                    continue;
                }
                if coefficient > 0.0 {
                    positive_rows[column].push(index as u32);
                }
                if (coefficient < 0.0 || row.is_equality())
                    && defines.is_none_or(|known| column > known)
                {
                    defines = Some(column);
                }
            }
            if !CHECK_ONLY_FAMILIES.contains(family) {
                if let Some(column) = defines {
                    defining_rows[column].push(index as u32);
                }
            }
            rows.push(Row {
                start,
                end: columns.len(),
                constant: expression.constant(),
                equality: row.is_equality(),
            });
        }
        let mut defined = Vec::new();
        let mut free = Vec::new();
        let mut upper_rows: Vec<Vec<u32>> = vec![Vec::new(); count];
        for column in 0..count {
            if binary[column] {
                continue;
            }
            if defining_rows[column].is_empty() {
                upper_rows[column] = std::mem::take(&mut positive_rows[column]);
                free.push(column);
            } else {
                defined.push(column);
            }
        }
        let objective = milp
            .objective_terms
            .iter()
            .map(|&(variable, coefficient)| (column_of[&variable], coefficient))
            .collect();
        Self {
            column_of,
            binary,
            max,
            rows,
            columns,
            coefficients,
            defining_rows,
            upper_rows,
            defined,
            free,
            objective,
        }
    }

    /// `Σ coefficient · value + constant` of row `row`, without column
    /// `skip`, and the coefficient of `skip`.
    fn row_value(&self, row: &Row, values: &[f64], skip: usize) -> (f64, f64) {
        let mut sum = row.constant;
        let mut own = 0.0;
        for at in row.start..row.end {
            let column = self.columns[at] as usize;
            if column == skip {
                own += self.coefficients[at];
            } else {
                sum += self.coefficients[at] * values[column];
            }
        }
        (sum, own)
    }

    /// The cheapest completion of `binaries` (every binary not listed is 0),
    /// or why it has none here: the row or bound it breaks.
    pub(crate) fn complete(&self, binaries: &[(Variable, f64)]) -> Result<Completed, String> {
        let mut values = vec![0.0; self.binary.len()];
        for &(variable, value) in binaries {
            let column = self.column_of[&variable];
            if self.binary[column] {
                values[column] = value;
            }
        }
        // Defined columns: the largest lower bound, to a fixpoint.
        let mut converged = false;
        for _ in 0..MOST_PASSES {
            let mut changed = false;
            for &column in &self.defined {
                let mut value: f64 = 0.0;
                for &row in &self.defining_rows[column] {
                    let row = &self.rows[row as usize];
                    let (rest, own) = self.row_value(row, &values, column);
                    // own · v + rest ≤ 0 (or = 0) with own < 0 for an inequality.
                    let bound = -rest / own;
                    if row.equality {
                        value = bound;
                        break;
                    }
                    value = value.max(bound);
                }
                if (value - values[column]).abs() > 1e-12 * value.abs().max(1.0) {
                    changed = true;
                }
                values[column] = value;
            }
            if !changed {
                converged = true;
                break;
            }
        }
        if !converged {
            return Err("no fixpoint of the lower bounds".into());
        }
        // Free columns: the smallest upper bound.
        for &column in &self.free {
            let mut value = self.max[column];
            for &row in &self.upper_rows[column] {
                let row = &self.rows[row as usize];
                let (rest, own) = self.row_value(row, &values, column);
                value = value.min(-rest / own);
            }
            values[column] = if value.is_finite() {
                value.max(0.0)
            } else {
                0.0
            };
        }
        // Every row and bound.
        for (index, row) in self.rows.iter().enumerate() {
            let mut sum = row.constant;
            let mut magnitude = row.constant.abs();
            for at in row.start..row.end {
                let term = self.coefficients[at] * values[self.columns[at] as usize];
                sum += term;
                magnitude += term.abs();
            }
            let tolerance = 1e-7 * magnitude.max(1.0);
            if sum > tolerance || (row.equality && sum < -tolerance) {
                return Err(format!("row {index} off by {sum:e}"));
            }
        }
        for (column, &value) in values.iter().enumerate() {
            if value > self.max[column] + 1e-7 * self.max[column].abs().max(1.0) {
                return Err(format!("column {column} at {value:e} above its bound"));
            }
        }
        let cost_per_second = self
            .objective
            .iter()
            .map(|&(column, coefficient)| coefficient * values[column])
            .sum();
        Ok(Completed {
            columns: values,
            cost_per_second,
        })
    }
}
