//! One solve of the selection MILP: its rows and objective, and when it
//! stops (a 1 % gap or 10,000 nodes by default, exact on request).

use super::*;

/// The largest objective coefficient once scaled, as cost model 1's.
pub(super) const LARGEST_SCALED_COEFFICIENT: f64 = 1e6;

/// The relative slack a lexicographic solve keeps on an earlier optimum.
pub(super) const LEXICOGRAPHIC_SLACK: f64 = 1e-9;

/// What one solve minimizes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Objective {
    MeanLatency,
    /// The mean latency's LP relaxation (a measurement).
    Relaxation,
    Memory,
    Disk,
    Limit(ClusterLimit),
}

/// Bounds on earlier optima (lexicographic solves): (weighted latency sum,
/// peak memory).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Fixed {
    pub(super) weighted_latency: Option<f64>,
    pub(super) memory: Option<f64>,
}

/// One solve: what it minimizes, the earlier optima it keeps, the limits
/// it enforces and the solution it starts from.
pub(super) struct Solve<'s> {
    pub(super) objective: Objective,
    pub(super) fixed: Fixed,
    pub(super) active: &'s [ClusterLimit],
    pub(super) start: Option<&'s [f64]>,
    /// A tie-break solve: only a plan whose objective is below this, within
    /// [`TIE_BREAK_NODES`] branch-and-bound nodes.
    pub(super) improve_below: Option<f64>,
    /// A plan to pin: its selection variables fixed to 1, every other
    /// selection variable to 0; HiGHS completes the continuous ones. The
    /// pinned solve's columns are the next solve's MIP start.
    pub(super) pinned: Option<&'s FeasiblePlan>,
    pub(super) stop: Stop,
}

/// When a solve stops.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Stop {
    /// At a proved optimum.
    Exact,
    /// At a relative gap of [`STOP_RELATIVE_GAP`] or after
    /// [`STOP_NODES`] branch-and-bound nodes, whichever comes first.
    GapOrNodes,
}

/// The default stop rule of the mean latency solve: a relative gap of 1 %
/// or 10,000 branch-and-bound nodes, whichever comes first
/// (`LatencySelection::exact_solve` turns it off). Limits in nodes, not
/// seconds, so the same inputs give the same plan on any host (one solver
/// thread); 10,000 nodes took about 30 s on the slowest workload measured
/// before the path reads.
pub(super) const STOP_RELATIVE_GAP: f64 = 0.01;

pub(super) const STOP_NODES: i32 = 10_000;

/// What a solve that reached its node limit without a plan returns.
pub(super) const NO_PLAN_IN_NODES: &str = "no plan within the node limit";

/// The most branch-and-bound nodes a tie-break solve explores for a
/// smaller plan (deterministic, as [`STOP_NODES`]).
pub(super) const TIE_BREAK_NODES: i32 = 500;

/// The values a solve returns.
pub(super) struct Solution {
    pub(super) selected: Vec<usize>,
    pub(super) edges: Vec<usize>,
    pub(super) answering: Vec<(QueryId, usize)>,
    pub(super) latency_ms: Vec<f64>,
    pub(super) objective: f64,
    /// The solver's lower bound on the objective, unscaled.
    pub(super) lower_bound: f64,
    /// Every column's value, the next solve's start.
    pub(super) columns: Vec<f64>,
    /// The plan's peak memory and disk, in bytes.
    pub(super) memory_bytes: f64,
    pub(super) disk_bytes: f64,
    pub(super) variable_count: usize,
    pub(super) constraint_count: usize,
    pub(super) solve_ms: u64,
}

/// Per refresh group, its big-M: the most $f_G$ can exceed the allowed
/// lateness ($F_q$'s own lower bound), from each group's largest refresh
/// time `refresh_bound`: its own, the groups it reads in full, and the
/// groups closing at the same instant over `threads`.
pub(super) fn freshness_big_m(
    refresh_bound: &[f64],
    windows: &[RefreshWindow],
    reads: &[BTreeSet<usize>],
    threads: f64,
) -> Vec<f64> {
    (0..windows.len())
        .map(|group| {
            let mut bound = refresh_bound[group];
            for (other, (&other_bound, &other_window)) in
                refresh_bound.iter().zip(windows).enumerate()
            {
                if other == group {
                    continue;
                }
                if reads[group].contains(&other) {
                    bound += other_bound;
                } else if close_together(windows[group], other_window) {
                    bound += other_bound / threads;
                }
            }
            bound
        })
        .collect()
}

/// `costs` over the selection as one linear expression: each execution's
/// and edge's share times its variable, each retention share times $h_v$,
/// and per source the most raw data a selected scan keeps (a variable at
/// least each, added to `problem` with its rows).
pub(super) fn quantity_expression(
    inputs: &ClusterInputs<'_>,
    costs: &ExecutionCosts,
    variables: &SelectionVariables,
    kept: &[bool],
    problem: &mut ProblemVariables,
    rows: &mut Vec<Constraint>,
) -> Expression {
    let selectable = inputs.selection.selectable;
    let mut expression = Expression::default();
    for (index, &x) in variables.x.iter().enumerate() {
        if kept[index] {
            expression += share_or_zero(costs, index) * x;
        }
    }
    for (&position, &y) in &variables.y {
        expression += costs.per_edge[position].unwrap_or(0.0) * y;
    }
    for (v, &h) in &variables.h {
        if !costs.raw_data_source.contains_key(v) {
            let share = costs
                .retention_cost_per_retained_second
                .get(v)
                .copied()
                .unwrap_or(0.0);
            expression += share * h;
        }
    }
    let mut executions_of: HashMap<PhysicalCandidateId, Vec<usize>> = HashMap::new();
    for (index, execution) in selectable.executions().iter().enumerate() {
        if kept[index] {
            executions_of
                .entry(execution.candidate)
                .or_default()
                .push(index);
        }
    }
    let mut most_of_source: BTreeMap<String, Variable> = BTreeMap::new();
    let mut scans: Vec<_> = costs.raw_data_source.iter().collect();
    scans.sort_unstable_by_key(|(v, _)| **v);
    for (v, source) in scans {
        let r = *most_of_source
            .entry(format!("{source:?}"))
            .or_insert_with(|| problem.add(variable().min(0)));
        if let Some(&h) = variables.h.get(v) {
            let share = costs
                .retention_cost_per_retained_second
                .get(v)
                .copied()
                .unwrap_or(0.0);
            rows.push(constraint!(r >= share * h));
        } else if let Some(&kept_bytes) = costs.unbounded_raw_data_on_disk_cost_per_second.get(v) {
            for &index in executions_of.get(v).into_iter().flatten() {
                rows.push(constraint!(r >= kept_bytes * variables.x[index]));
            }
        }
    }
    for r in most_of_source.into_values() {
        expression += r;
    }
    expression
}

/// The largest absolute coefficient of `expression`'s variables.
pub(super) fn largest_coefficient(expression: &Expression) -> f64 {
    expression
        .linear_coefficients()
        .map(|(_, coefficient)| coefficient.abs())
        .fold(0.0, f64::max)
}

/// Build the MILP with the `active` limits and solve it for `objective`,
/// with the earlier optima `fixed`.
pub(super) fn build_and_solve(
    inputs: &ClusterInputs<'_>,
    shapes: &[QueryShape],
    groups: &Groups,
    kept: &[bool],
    solve: Solve<'_>,
) -> Result<Solution, SelectionMilpError> {
    let Solve {
        objective,
        fixed,
        active,
        start,
        improve_below,
        pinned,
        stop,
    } = solve;
    let selection = &inputs.selection;
    let selectable = selection.selectable;
    let execution = &inputs.cost_inputs.data_plane_execution;
    let rules = AccuracyRules::of(selection);
    let components = Components::of(selection);
    let shared = copies_in_equivalent_contexts(selection, kept, rules.as_ref());
    let mut problem = ProblemVariables::new();
    let variables = selection_variables(selection, kept, &shared, &components, &mut problem);
    // Variable counts by family, for the solver log (`log_solver`).
    let mut families: Vec<(&str, usize)> = vec![("selection (x, y, z, t, h, r, a)", problem.len())];
    let accuracy_rows = rules
        .as_ref()
        .map(|rules| {
            let error_model = ErrorModel::new(selection, rules, &variables.y, &shared);
            let accuracy = AccuracyVariables::new(&error_model, &mut problem);
            accuracy_constraints(
                selection,
                &error_model,
                &accuracy,
                &variables.y,
                &variables.z,
                &shared,
                &mut problem,
            )
        })
        .unwrap_or_default();
    families.push(("accuracy", problem.len()));
    let mut rows: Vec<Constraint> = Vec::new();
    // The quantities' expressions.
    let memory = quantity_expression(
        inputs,
        inputs.memory,
        &variables,
        kept,
        &mut problem,
        &mut rows,
    );
    let disk = quantity_expression(
        inputs,
        inputs.disk,
        &variables,
        kept,
        &mut problem,
        &mut rows,
    );
    let cpu = quantity_expression(
        inputs,
        inputs.cpu,
        &variables,
        kept,
        &mut problem,
        &mut rows,
    );
    families.push(("quantities (raw data per source)", problem.len()));
    let quantity_rows = rows.len();
    // Refresh groups: each one's refresh wall time per window, P_G.
    let wait_per_run = inputs
        .cost_inputs
        .measured_operation_costs
        .refresh_io_wait
        .ns_per_refresh
        * 1e-6;
    let mut refresh: Vec<Expression> = Vec::new();
    let mut refresh_bound: Vec<f64> = Vec::new();
    for members in &groups.executions {
        let mut expression = Expression::default();
        let mut bound = 0.0;
        for &index in members {
            if kept[index] {
                let p = share_at_largest_retention_or_zero(inputs.refresh_ms, index);
                expression += p * variables.x[index];
                bound += p.max(0.0);
            }
        }
        for (&position, &y) in &variables.y {
            if members.contains(&selectable.edges()[position].consumer) {
                let p = inputs.refresh_ms.per_edge[position].unwrap_or(0.0);
                expression += p * y;
                bound += p.max(0.0);
            }
        }
        if wait_per_run > 0.0 {
            let any = problem.add(variable().binary());
            for &index in members {
                rows.push(constraint!(any >= variables.x[index]));
            }
            expression += wait_per_run * any;
            bound += wait_per_run;
        }
        refresh.push(expression);
        refresh_bound.push(bound);
    }
    let threads = f64::from(execution.precompute_threads);
    let lateness = execution.allowed_lateness_ms as f64;
    // f_G = lateness + Q_G + P_G.
    let freshness: Vec<Expression> = (0..groups.windows.len())
        .map(|group| {
            let mut expression = Expression::from(lateness) + refresh[group].clone();
            for (other, (other_refresh, &other_window)) in
                refresh.iter().zip(&groups.windows).enumerate()
            {
                if other == group {
                    continue;
                }
                if groups.reads[group].contains(&other) {
                    expression += other_refresh.clone();
                } else if close_together(groups.windows[group], other_window) {
                    expression += other_refresh.clone() * (1.0 / threads);
                }
            }
            expression
        })
        .collect();
    families.push(("refresh groups", problem.len()));
    let big_m = freshness_big_m(&refresh_bound, &groups.windows, &groups.reads, threads);
    // Per query: reads a, edge reads w, freshness F, latency L.
    let mut latencies: Vec<Variable> = Vec::new();
    for shape in shapes {
        // Reads only of the executions kept: the others are fixed at 0.
        let reads: BTreeMap<usize, Variable> = shape
            .reach
            .iter()
            .filter(|&&index| kept[index])
            .map(|&index| (index, problem.add(variable().min(0).max(1))))
            .collect();
        for &index in &shape.answering {
            if let (Some(&z), Some(&read)) =
                (variables.z.get(&(shape.query, index)), reads.get(&index))
            {
                rows.push(constraint!(read >= z));
            }
        }
        // A query-time execution not kept across evaluations adds its work
        // once per round of the request's steps over the query threads.
        let runs = |index: usize| match once_per_request(inputs, index) {
            true => 1.0,
            false => shape.step_rounds as f64,
        };
        let fresh = problem.add(variable().min(lateness));
        let latency = problem.add(variable().min(0));
        let mut execution_ms = Expression::from(shape.request_ms);
        // Reads as a flow (T9): what the query reads of a consumer flows
        // into exactly one selected producer per input, `Σ read = a_c` over
        // the input's edges with `read ≤ y`, and a producer is read at
        // least as much as any edge into it, so a fractional read is
        // carried down the sub-DAG.
        let mut flows: BTreeMap<(usize, usize), Expression> = BTreeMap::new();
        for &position in &shape.edges {
            let Some(&y) = variables.y.get(&position) else {
                continue;
            };
            let edge = selectable.edges()[position];
            let (Some(_), Some(&producer)) = (reads.get(&edge.consumer), reads.get(&edge.producer))
            else {
                continue;
            };
            let read = problem.add(variable().min(0).max(1));
            rows.push(constraint!(read <= y));
            rows.push(constraint!(producer >= read));
            let input = inputs
                .selection
                .physical
                .edge(edge.edge)
                .input_index_of_consumer;
            *flows.entry((edge.consumer, input)).or_default() += read;
            let d = inputs.query_ms.per_edge[position].unwrap_or(0.0) * runs(edge.consumer);
            execution_ms += d * read;
        }
        for ((consumer, _), flow) in flows {
            rows.push(constraint!(flow == reads[&consumer]));
        }
        // Valid freshness cuts without the big-M (T10): a query waits at
        // least for the refresh of the executions of one group it reads.
        let mut read_refresh_of_group: BTreeMap<usize, Expression> = BTreeMap::new();
        for (&index, &read) in &reads {
            if let (Some(&group), true) =
                (groups.of_execution.get(&index), at_ingestion(inputs, index))
            {
                *read_refresh_of_group.entry(group).or_default() +=
                    share_at_largest_retention_or_zero(inputs.refresh_ms, index).max(0.0) * read;
            }
        }
        for (_, refresh) in read_refresh_of_group {
            rows.push(constraint!(fresh >= lateness + refresh));
        }
        // Reads along each path of the query's plan (exact): the plan's
        // sub-DAG of the query unfolded into a tree of occurrences, each
        // reading one execution in a plan; see `path_reads`.
        if inputs.cost_inputs.latency_selection_checks.path_reads {
            path_reads(
                inputs,
                shape,
                &reads,
                &variables,
                PathReadRows {
                    groups,
                    freshness: &freshness,
                    big_m: &big_m,
                    fresh,
                    lateness,
                    wait_per_run,
                },
                &mut problem,
                &mut rows,
            );
        }
        for (&index, &read) in &reads {
            if !kept[index] {
                continue;
            }
            match groups.of_execution.get(&index) {
                Some(&group) if at_ingestion(inputs, index) => {
                    rows.push(constraint!(
                        fresh >= freshness[group].clone() - big_m[group] * (1 - read)
                    ));
                    // Valid without the big-M: reading the execution waits
                    // at least for its own refresh ($f_G \ge a + p_e x_e$
                    // and a read execution is selected), which keeps the
                    // LP bound off zero.
                    let own = share_at_largest_retention_or_zero(inputs.refresh_ms, index).max(0.0)
                        + wait_per_run;
                    rows.push(constraint!(fresh >= lateness + own * read));
                }
                _ => {
                    execution_ms += share_at_largest_retention_or_zero(inputs.query_ms, index)
                        * runs(index)
                        * read
                }
            }
        }
        rows.push(constraint!(latency == fresh + execution_ms));
        if let Some(bound) = shape.max_latency_ms {
            if active.contains(&ClusterLimit::QueryLatencyMs {
                query: shape.query.0,
            }) {
                rows.push(constraint!(latency <= bound));
            }
        }
        latencies.push(latency);
    }
    families.push(("latency (reads, edge reads, F, L)", problem.len()));
    let latency_rows = rows.len();
    // The cluster's limits.
    let limits = inputs.limits;
    if active.contains(&ClusterLimit::MemoryBytes) {
        rows.push(constraint!(memory.clone() <= limits.memory_bytes as f64));
    }
    if active.contains(&ClusterLimit::DiskBytes) {
        rows.push(constraint!(disk.clone() <= limits.disk_bytes as f64));
    }
    // CPU: the plan's, the requests' and the decode of arriving samples.
    let fixed_cores: f64 = shapes
        .iter()
        .map(|shape| {
            let requests_per_second = selection
                .entries
                .get(shape.query.0)
                .and_then(|entry| {
                    evaluation_rate(
                        [(&entry.recurrence, None)],
                        inputs.cost_inputs.non_repeating_query_horizon_seconds,
                    )
                    .ok()
                })
                .unwrap_or(0.0);
            shape.request_cpu_ms * requests_per_second / 1_000.0
        })
        .sum::<f64>()
        + asap_cost::ingest_decode_cpu_core_seconds_per_second(inputs.data, inputs.cost_inputs);
    let cpu = cpu + fixed_cores;
    if let (Some(cores), true) = (limits.cpu_cores, active.contains(&ClusterLimit::CpuCores)) {
        rows.push(constraint!(cpu.clone() <= f64::from(cores)));
    }
    let mut load = Expression::default();
    for (group, window) in groups.windows.iter().enumerate() {
        load += refresh[group].clone() * (1.0 / (window.width_ms as f64 * threads));
    }
    if active.contains(&ClusterLimit::PrecomputeLoad) {
        rows.push(constraint!(
            load.clone() <= execution.max_thread_utilization
        ));
    }
    let weighted: Expression = shapes
        .iter()
        .zip(&latencies)
        .map(|(shape, &latency)| shape.weight * latency)
        .sum();
    if let Some(best) = fixed.weighted_latency {
        rows.push(constraint!(
            weighted.clone() <= best * (1.0 + LEXICOGRAPHIC_SLACK) + LEXICOGRAPHIC_SLACK
        ));
    }
    if let Some(best) = fixed.memory {
        rows.push(constraint!(
            memory.clone() <= best * (1.0 + LEXICOGRAPHIC_SLACK) + 1.0
        ));
    }
    if let Some(plan) = pinned {
        let pin = |variable: Variable, chosen: bool| {
            constraint!(variable == if chosen { 1.0 } else { 0.0 })
        };
        for (index, &x) in variables.x.iter().enumerate() {
            rows.push(pin(x, plan.executions.contains(&index)));
        }
        for (&position, &y) in &variables.y {
            rows.push(pin(y, plan.edges.contains(&position)));
        }
        for (&(query, index), &z) in &variables.z {
            rows.push(pin(z, plan.answering.contains(&(query, index))));
        }
    }
    let goal: Expression = match objective {
        Objective::MeanLatency | Objective::Relaxation => weighted,
        Objective::Memory | Objective::Limit(ClusterLimit::MemoryBytes) => memory.clone(),
        Objective::Disk | Objective::Limit(ClusterLimit::DiskBytes) => disk.clone(),
        Objective::Limit(ClusterLimit::CpuCores) => cpu,
        Objective::Limit(ClusterLimit::PrecomputeLoad) => load,
        Objective::Limit(ClusterLimit::QueryLatencyMs { query }) => shapes
            .iter()
            .position(|shape| shape.query.0 == query)
            .map_or_else(Expression::default, |at| Expression::from(latencies[at])),
    };
    let largest = largest_coefficient(&goal);
    let scale = match largest > 0.0 && largest.is_finite() {
        true => LARGEST_SCALED_COEFFICIENT / largest,
        false => 1.0,
    };
    let accuracy_count = accuracy_rows.len();
    let rows_total = rows.len();
    let constraints: Vec<Constraint> = [
        input_satisfaction_constraints(selection, &variables),
        producer_selection_constraints(selection, &variables),
        query_coverage_constraints(selection, &variables),
        acyclicity_constraints(selection, &variables, &components),
        use_constraints(selection, &variables),
        retention_constraints(
            selection,
            &variables.h,
            &variables.x,
            &variables.y,
            &variables.z,
        ),
        exact_only_constraints(
            selection,
            &variables.read_by_query,
            &variables.y,
            &variables.z,
        ),
        accuracy_rows,
        rows,
    ]
    .into_iter()
    .flatten()
    .collect();
    let variable_count = problem.len();
    let column_of: HashMap<Variable, usize> = problem
        .iter_variables_with_def()
        .enumerate()
        .map(|(column, (variable, _))| (variable, column))
        .collect();
    let constraint_count = constraints.len();
    let goal_scaled = goal.clone() * scale;
    let builder = problem
        .minimise(goal_scaled)
        .using(highs)
        .with_all(constraints);
    let mut model = builder
        .try_into_inner()
        .map_err(|error| SelectionMilpError::Solver(error.to_string()))?;
    model.set_option(
        "mip_rel_gap",
        match stop {
            Stop::Exact => 0.0,
            Stop::GapOrNodes => STOP_RELATIVE_GAP,
        },
    );
    model.set_option("mip_abs_gap", 0.0);
    if stop == Stop::GapOrNodes {
        model.set_option("mip_max_nodes", STOP_NODES);
    }
    let log = inputs.cost_inputs.latency_selection_checks.log_solver;
    model.set_option("output_flag", log);
    model.set_option("log_to_console", log);
    if log {
        let mut previous = 0;
        let counts: Vec<String> = families
            .iter()
            .map(|&(name, total)| {
                let count = total - previous;
                previous = total;
                format!("{name} {count}")
            })
            .collect();
        eprintln!(
            "cluster MILP {objective:?}: variables: {}; rows: structure {}, accuracy {}, quantities {}, latency {}, limits and fixed {}",
            counts.join(", "),
            constraint_count - rows_total - accuracy_count,
            accuracy_count,
            quantity_rows,
            latency_rows - quantity_rows,
            rows_total - latency_rows,
        );
    }
    model.set_option("threads", 1);
    if objective == Objective::Relaxation {
        model.set_option("solve_relaxation", true);
    }
    if let Some(below) = improve_below {
        model.set_option("mip_max_nodes", TIE_BREAK_NODES);
        model.set_option("objective_bound", below * scale);
    }
    if let Some(start) = start.filter(|start| start.len() == variable_count) {
        // The variables are built in the same order every solve, so a
        // column is the same variable in each; a start HiGHS refuses is
        // dropped, which only costs time.
        let _ = model.try_set_solution(Some(start), None, None, None);
    }
    let started = Instant::now();
    let solved = model
        .try_solve()
        .map_err(|status| SelectionMilpError::Solver(format!("{status:?}")))?;
    let solve_ms = started.elapsed().as_millis() as u64;
    use highs::{HighsModelStatus, HighsSolutionStatus};
    let columns: Vec<f64> = match solved.status() {
        HighsModelStatus::ModelEmpty => vec![0.0; variable_count],
        HighsModelStatus::Optimal | HighsModelStatus::ReachedSolutionLimit
            if solved.primal_solution_status() == HighsSolutionStatus::Feasible =>
        {
            solved.get_solution().columns().to_vec()
        }
        HighsModelStatus::Infeasible | HighsModelStatus::UnboundedOrInfeasible => {
            return Err(SelectionMilpError::Infeasible)
        }
        HighsModelStatus::ReachedSolutionLimit if improve_below.is_none() => {
            return Err(SelectionMilpError::Solver(NO_PLAN_IN_NODES.to_string()))
        }
        other => return Err(SelectionMilpError::Solver(format!("{other:?}"))),
    };
    // The lower bound HiGHS proved, unscaled; at a proved optimum (or an
    // empty model, or the LP relaxation) the objective itself.
    let proved_optimal = stop == Stop::Exact
        || objective == Objective::Relaxation
        || solved.status() == HighsModelStatus::ModelEmpty;
    let dual_bound = (!proved_optimal)
        .then(|| solved.double_info_value(c"mip_dual_bound").ok())
        .flatten()
        .filter(|bound| bound.is_finite())
        // HiGHS's objective has no constant term.
        .map(|bound| bound / scale + goal.constant());
    let value = |variable: Variable| columns.get(column_of[&variable]).copied().unwrap_or(0.0);
    let chosen = |variable: Variable| value(variable) > 0.5;
    let selected: Vec<usize> = (0..variables.x.len())
        .filter(|&index| chosen(variables.x[index]))
        .collect();
    let edges: Vec<usize> = variables
        .y
        .iter()
        .filter(|&(_, &y)| chosen(y))
        .map(|(&position, _)| position)
        .collect();
    // The LP relaxation answers fractionally: no plan to read off.
    let relaxed = objective == Objective::Relaxation;
    let answering: Vec<(QueryId, usize)> = selectable
        .answering()
        .iter()
        .filter(|_| !relaxed)
        .map(|(query, answering)| {
            let execution = answering
                .iter()
                .copied()
                .find(|&execution| chosen(variables.z[&(*query, execution)]))
                .expect("(C): one output per query output reference");
            (*query, execution)
        })
        .collect();
    let evaluate = |expression: &Expression| {
        expression
            .linear_coefficients()
            .map(|(variable, coefficient)| coefficient * value(variable))
            .sum::<f64>()
            + expression.constant()
    };
    let memory_bytes = evaluate(&memory);
    let disk_bytes = evaluate(&disk);
    let objective_value = (&goal)
        .linear_coefficients()
        .map(|(variable, coefficient)| coefficient * value(variable))
        .sum::<f64>()
        + goal.constant();
    Ok(Solution {
        selected,
        edges,
        answering,
        latency_ms: latencies.iter().map(|&latency| value(latency)).collect(),
        objective: objective_value,
        lower_bound: dual_bound.map_or(objective_value, |bound| bound.min(objective_value)),
        memory_bytes,
        disk_bytes,
        columns,
        variable_count,
        constraint_count,
        solve_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(width_ms: u64, phase_ms: u64) -> RefreshWindow {
        RefreshWindow { width_ms, phase_ms }
    }

    /// §6.4: windows of 60 s and 300 s at phase 0 close together (every
    /// 300 s); two 60 s windows at phases 0 and 30 s never do; 20 s at 5 s
    /// and 30 s at 15 s do (both close at 45 s).
    #[test]
    fn coinciding_refresh_groups() {
        assert!(close_together(window(60_000, 0), window(300_000, 0)));
        assert!(!close_together(window(60_000, 0), window(60_000, 30_000)));
        assert!(close_together(
            window(20_000, 5_000),
            window(30_000, 15_000)
        ));
    }

    /// §6.4: each group's big-M bounds what $f_G$ can exceed the lateness
    /// by, for every selection of the groups' members: three groups (two
    /// closing together, one read by the first), members of refresh times
    /// 1–4 ms, two precompute threads, every subset of members enumerated.
    #[test]
    fn big_m_bounds_the_freshness_of_every_selection() {
        let windows = [window(10_000, 0), window(20_000, 0), window(10_000, 5_000)];
        let members: [&[f64]; 3] = [&[1.0, 2.0], &[3.0], &[4.0, 1.5]];
        let reads = [BTreeSet::from([2]), BTreeSet::new(), BTreeSet::new()];
        let threads = 2.0;
        let bounds: Vec<f64> = members.iter().map(|m| m.iter().sum()).collect();
        let big_m = freshness_big_m(&bounds, &windows, &reads, threads);
        let all: Vec<(usize, f64)> = members
            .iter()
            .enumerate()
            .flat_map(|(group, m)| m.iter().map(move |&p| (group, p)))
            .collect();
        for selection in 0u32..(1 << all.len()) {
            let refresh: Vec<f64> = (0..3)
                .map(|group| {
                    all.iter()
                        .enumerate()
                        .filter(|&(bit, &(g, _))| g == group && selection & (1 << bit) != 0)
                        .map(|(_, &(_, p))| p)
                        .sum()
                })
                .collect();
            for group in 0..3 {
                let mut freshness = refresh[group];
                for other in 0..3 {
                    if other == group {
                        continue;
                    }
                    if reads[group].contains(&other) {
                        freshness += refresh[other];
                    } else if close_together(windows[group], windows[other]) {
                        freshness += refresh[other] / threads;
                    }
                }
                assert!(
                    freshness <= big_m[group] + 1e-12,
                    "group {group}, selection {selection:b}: {freshness} > {}",
                    big_m[group]
                );
            }
        }
    }
}
