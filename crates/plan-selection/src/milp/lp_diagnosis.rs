//! #1095 §6.1: where the selection MILP's LP relaxation loses its bound
//! (measurement only, behind the `lp-diagnosis` feature). With
//! `ASAP_STAGE3_LP_DIAGNOSIS=<path>`, each solve appends one JSON line:
//! - `lp`: the LP relaxation of the model as built, and `root`: HiGHS's
//!   bound after its root node (presolve, cuts), both from the warm start;
//! - `lp_without`: the LP with one row family dropped, per family: how much
//!   of the LP bound that family's rows hold up;
//! - `lp_tightened`: the LP with valid rows added: `Σ terms ≥ run` for each
//!   run charged only on its configuration or window terms (at integral
//!   binaries one of them is 1);
//! - `objective_by_family`: the objective split by the family of each
//!   charged variable, at the LP's solution, at the warm start and at the
//!   incumbent, so `incumbent − lp` splits into one share per family;
//! - `runs`: the charged `p` and `x` variables with the largest difference
//!   between the incumbent and the LP, with their values in both.
//!
//! The deadline is pushed back by the diagnosis's time, so the MILP keeps
//! its budget; a diagnosed run's wall time (and the facade's later rounds)
//! is still not comparable with an undiagnosed one.
use std::collections::BTreeMap;
use std::time::Instant;

use good_lp::solvers::highs::highs;
use good_lp::{constraint, variable, Constraint, Expression, ProblemVariables, SolverModel};

use super::model::{objective_scale, SelectionMilp, HIGHS_THREADS};

/// The environment variable naming the file the diagnosis appends to.
const DIAGNOSIS_ENV: &str = "ASAP_STAGE3_LP_DIAGNOSIS";

/// The runs listed per solve.
const RUNS_SHOWN: usize = 40;

/// The time limit of each LP and of the root node.
const LP_TIME_LIMIT_S: f64 = 300.0;

/// One LP (or root node) solved.
#[derive(Debug, Clone, serde::Serialize)]
struct Bound {
    /// The bound, cost per second (`None` when HiGHS gave none).
    per_second: Option<f64>,
    status: String,
    ms: u64,
}

/// The LP solves, before the MILP's.
pub(crate) struct Diagnosis {
    path: String,
    lp: Bound,
    root: Bound,
    lp_without: BTreeMap<&'static str, Bound>,
    lp_tightened: Bound,
    tightening_rows: usize,
    /// The LP's columns.
    lp_columns: Vec<f64>,
    /// The objective, by column.
    objective: Vec<(usize, f64)>,
    column_families: Vec<&'static str>,
    /// Per `p` and `x` column, what it stands for.
    labels: BTreeMap<usize, String>,
}

impl Diagnosis {
    /// The LP solves of `milp` from `start` (columns), when the diagnosis is
    /// asked for.
    pub(crate) fn run(milp: &SelectionMilp<'_>, start: Option<&[f64]>) -> Option<Self> {
        let path = std::env::var(DIAGNOSIS_ENV).ok()?;
        let definitions: Vec<_> = milp
            .builder
            .variables
            .iter_variables_with_def()
            .map(|(variable, definition)| (variable, definition.clone()))
            .collect();
        let column_of: std::collections::HashMap<_, usize> = definitions
            .iter()
            .enumerate()
            .map(|(column, (variable, _))| (*variable, column))
            .collect();
        let objective: Vec<(usize, f64)> = milp
            .objective_terms
            .iter()
            .map(|&(variable, coefficient)| (column_of[&variable], coefficient))
            .collect();
        let start_values: Option<Vec<_>> = start.map(|start| {
            definitions
                .iter()
                .map(|(variable, _)| *variable)
                .zip(start.iter().copied())
                .collect()
        });
        let scale = objective_scale(&milp.objective_terms, start_values.as_deref());
        // The relaxation: the same variables (same indices), none integral.
        let relaxed = || {
            let mut variables = ProblemVariables::new();
            for (_, definition) in &definitions {
                variables.add(
                    variable()
                        .min(definition.get_min())
                        .max(definition.get_max()),
                );
            }
            variables
        };
        let objective_expression: Expression = milp
            .objective_terms
            .iter()
            .map(|&(variable, coefficient)| coefficient * scale * variable)
            .sum();
        let solve = |variables: ProblemVariables, rows: Vec<Constraint>, root: bool| {
            let started = Instant::now();
            let problem = variables
                .minimise(objective_expression.clone())
                .using(highs)
                .with_all(rows);
            let mut model = problem.try_into_inner().expect("a HiGHS model");
            model.set_option("output_flag", false);
            model.set_option("threads", HIGHS_THREADS);
            model.set_option("time_limit", LP_TIME_LIMIT_S);
            if root {
                // The root node, then stop: its bound is the root bound.
                model.set_option("mip_max_nodes", 1);
                model.set_option("mip_rel_gap", 0.0);
                model.set_option("mip_abs_gap", 0.0);
                if let Some(start) = start {
                    let _ = model.try_set_solution(Some(start), None, None, None);
                }
            }
            let Ok(solved) = model.try_solve() else {
                return (
                    Bound {
                        per_second: None,
                        status: "solver error".into(),
                        ms: started.elapsed().as_millis() as u64,
                    },
                    Vec::new(),
                );
            };
            let value = if root {
                solved.double_info_value(c"mip_dual_bound").ok()
            } else if solved.status() == highs::HighsModelStatus::Optimal {
                Some(solved.objective_value())
            } else {
                None
            };
            let columns = if root {
                Vec::new()
            } else {
                solved.get_solution().columns().to_vec()
            };
            (
                Bound {
                    per_second: value.filter(|v| v.is_finite()).map(|v| v / scale),
                    status: format!("{:?}", solved.status()),
                    ms: started.elapsed().as_millis() as u64,
                },
                columns,
            )
        };
        let rows = &milp.builder.rows;
        let families = &milp.builder.row_families;
        let (lp, lp_columns) = solve(relaxed(), rows.clone(), false);
        let (root, _) = solve(milp.builder.variables.clone(), rows.clone(), true);
        let mut lp_without = BTreeMap::new();
        let present: std::collections::BTreeSet<&'static str> = families.iter().copied().collect();
        for family in present {
            let kept: Vec<Constraint> = rows
                .iter()
                .zip(families)
                .filter(|(_, &f)| f != family)
                .map(|(row, _)| row.clone())
                .collect();
            lp_without.insert(family, solve(relaxed(), kept, false).0);
        }
        let tightening = cover_rows(milp);
        let tightening_rows = tightening.len();
        let mut tightened = rows.clone();
        tightened.extend(tightening);
        let (lp_tightened, _) = solve(relaxed(), tightened, false);
        let candidate = |c: asap_types::stages::PhysicalCandidateId| {
            let data = &milp.physical.candidates[c.0];
            format!(
                "c{} {} {:?} {:?}",
                c.0,
                milp.physical.physical_node(c).operator.kind_name(),
                data.timing,
                data.materialization
            )
        };
        let mut labels = BTreeMap::new();
        for (context, choices) in milp.choice.iter().enumerate() {
            let place = match milp.inputs.contexts[context].kind {
                crate::groups::ContextKind::Query(query) => format!("q{query}"),
                crate::groups::ContextKind::Ingestion(pipeline) => format!("I{pipeline:?}"),
                crate::groups::ContextKind::Shared(m) => format!("m{}", m.0),
            };
            for (&c, variable) in choices {
                labels.insert(column_of[variable], format!("p[{place}] {}", candidate(c)));
            }
        }
        for (&m, variable) in &milp.selected {
            labels.insert(column_of[variable], format!("x {}", candidate(m)));
        }
        Some(Self {
            labels,
            path,
            lp,
            root,
            lp_without,
            lp_tightened,
            tightening_rows,
            lp_columns,
            objective,
            column_families: milp.builder.column_families.clone(),
        })
    }

    /// Appends the line, with the warm start's and the incumbent's columns
    /// (completed) and the MILP's results.
    pub(crate) fn write(
        self,
        start: Option<&[f64]>,
        incumbent: Option<&[f64]>,
        objective_per_second: f64,
        dual_bound_per_second: f64,
        status: &str,
    ) {
        let by_family = |columns: Option<&[f64]>| -> Option<BTreeMap<&'static str, f64>> {
            let columns = columns.filter(|columns| !columns.is_empty())?;
            let mut split: BTreeMap<&'static str, f64> = BTreeMap::new();
            for &(column, coefficient) in &self.objective {
                *split.entry(self.column_families[column]).or_default() +=
                    coefficient * columns[column];
            }
            Some(split)
        };
        // Per labeled column: (LP charge, incumbent charge, LP value,
        // incumbent value).
        let mut runs: Vec<(String, f64, f64, f64, f64)> = Vec::new();
        if let Some(incumbent) = incumbent.filter(|columns| !columns.is_empty()) {
            let mut charge: BTreeMap<usize, f64> = BTreeMap::new();
            for &(column, coefficient) in &self.objective {
                if self.labels.contains_key(&column) {
                    *charge.entry(column).or_default() += coefficient;
                }
            }
            for (column, coefficient) in charge {
                let (lp, at) = (self.lp_columns[column], incumbent[column]);
                runs.push((
                    self.labels[&column].clone(),
                    coefficient * lp,
                    coefficient * at,
                    lp,
                    at,
                ));
            }
            runs.sort_by(|a, b| (b.2 - b.1).abs().total_cmp(&(a.2 - a.1).abs()));
            runs.truncate(RUNS_SHOWN);
        }
        let line = serde_json::json!({
            "runs": runs,
            "variables": self.column_families.len(),
            "status": status,
            "objective_per_second": objective_per_second,
            "dual_bound_per_second": dual_bound_per_second,
            "lp": self.lp,
            "root": self.root,
            "lp_without": self.lp_without,
            "lp_tightened": self.lp_tightened,
            "tightening_rows": self.tightening_rows,
            "objective_by_family": {
                "lp": by_family(Some(&self.lp_columns)),
                "warm_start": by_family(start),
                "incumbent": by_family(incumbent),
            },
        });
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(file, "{line}");
        }
    }
}

/// `Σ terms ≥ run` for each run whose terms are all configuration or
/// window terms (none on the run's own indicator): at integral binaries a
/// used run has one term at 1.
fn cover_rows(milp: &SelectionMilp<'_>) -> Vec<Constraint> {
    let mut rows = Vec::new();
    for (&(context, candidate), terms) in &milp.run_terms_cache {
        if terms.is_empty() || terms.iter().any(|term| term.plain) {
            continue;
        }
        let Some(run) = milp.run_indicator(context, candidate) else {
            continue;
        };
        let sum: Expression = terms.iter().map(|term| term.indicator.clone()).sum();
        rows.push(constraint!(sum >= run));
    }
    rows
}
