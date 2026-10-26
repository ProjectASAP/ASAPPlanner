//! Exact finite demand atoms for the evaluator's recurrence accounting policy.
//! Different fixed intervals add; an unknown phase joins the first selected
//! known phase, estimated rates add, and nonrepeating demand takes a maximum.
use crate::cost::execution_demand::evaluation_rate;
use crate::search::candidates::graph::CandidateSelectionError;
use asap_types::workload::{QueryRecurrence, QueryWorkloadEntry, RangeEvaluation, RepeatedDemand};
use good_lp::{constraint, variable, Constraint, ProblemVariables, Variable};
use std::collections::BTreeMap;
use std::time::Instant;

const MAXIMUM_ENUMERATED_OFFSETS: usize = 100_000;
const MAXIMUM_OFFSET_VISITS: usize = 500_000;

#[derive(Debug, Clone)]
struct Atom {
    readers: Vec<usize>,
    count: u64,
}
#[derive(Debug, Clone)]
struct FixedPhase {
    readers: Vec<usize>,
    atoms: Vec<Atom>,
}
#[derive(Debug, Clone)]
struct FixedInterval {
    rate: f64,
    unknown: Vec<usize>,
    unknown_atoms: Vec<Atom>,
    // Ascending phase order is semantically significant.
    known: Vec<FixedPhase>,
}
#[derive(Debug, Clone, Default)]
struct DemandAtoms {
    fixed: Vec<FixedInterval>,
    estimated: Vec<(usize, f64)>,
    batch: Vec<(u64, Vec<usize>)>,
    horizon: f64,
}
#[derive(Debug, Clone)]
pub(crate) struct GeneralScheduleRates {
    queries: Vec<(QueryRecurrence, Option<RangeEvaluation>)>,
    rates: Vec<f64>,
    evaluations: DemandAtoms,
    requests: DemandAtoms,
}
fn unsupported(reason: &str) -> CandidateSelectionError {
    CandidateSelectionError::UnsupportedDirectCost(reason.into())
}
fn check(deadline: Option<Instant>) -> Result<(), CandidateSelectionError> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        Err(CandidateSelectionError::Solver(
            "time budget exhausted constructing schedule atoms".into(),
        ))
    } else {
        Ok(())
    }
}

// With a common step, k-ranges are nested and can be counted without expanding
// even a billion individual timestamps. Mixed steps retain exact coverage sets.
fn coverage(
    readers: &[(usize, u64, u64)],
    deadline: Option<Instant>,
) -> Result<Vec<Atom>, CandidateSelectionError> {
    if readers.is_empty() {
        return Ok(Vec::new());
    }
    if readers.iter().all(|(_, step, _)| *step == readers[0].1) {
        let mut endpoints: Vec<_> = readers
            .iter()
            .map(|(_, step, span)| span / step + 1)
            .collect();
        endpoints.sort_unstable();
        endpoints.dedup();
        let mut previous = 0;
        let mut atoms = Vec::new();
        for end in endpoints {
            check(deadline)?;
            let active = readers
                .iter()
                .filter(|(_, step, span)| span / step + 1 >= end)
                .map(|(id, _, _)| *id)
                .collect();
            atoms.push(Atom {
                readers: active,
                count: end - previous,
            });
            previous = end;
        }
        return Ok(atoms);
    }
    // A few distinct arithmetic progressions can be counted exactly without
    // enumerating their offsets. Duplicate geometries share all their readers.
    let mut geometries = BTreeMap::<(u64, u64), Vec<usize>>::new();
    for &(id, step, span) in readers {
        check(deadline)?;
        geometries.entry((step, span)).or_default().push(id);
    }
    if geometries.len() <= 12 {
        return arithmetic_coverage(&geometries.into_iter().collect::<Vec<_>>(), deadline);
    }
    let mut offsets: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    let mut visits = 0usize;
    for &(id, step, span) in readers {
        let count = span / step + 1;
        if count > MAXIMUM_OFFSET_VISITS as u64
            || visits.saturating_add(count as usize) > MAXIMUM_OFFSET_VISITS
        {
            return Err(unsupported(
                "mixed-step range demand exceeds the exact offset construction budget",
            ));
        }
        visits += count as usize;
        for k in 0..count {
            check(deadline)?;
            offsets.entry(k * step).or_default().push(id);
            if offsets.len() > MAXIMUM_ENUMERATED_OFFSETS {
                return Err(unsupported(
                    "mixed-step range demand exceeds the exact offset domain",
                ));
            }
        }
    }
    let mut classes = BTreeMap::<Vec<usize>, u64>::new();
    for (_, mut readers) in offsets {
        check(deadline)?;
        readers.sort_unstable();
        readers.dedup();
        *classes.entry(readers).or_default() += 1;
    }
    Ok(classes
        .into_iter()
        .map(|(readers, count)| Atom { readers, count })
        .collect())
}

fn arithmetic_coverage(
    geometries: &[((u64, u64), Vec<usize>)],
    deadline: Option<Instant>,
) -> Result<Vec<Atom>, CandidateSelectionError> {
    let size = 1usize << geometries.len();
    let mut counts = vec![0i128; size];
    for (mask, count) in counts.iter_mut().enumerate().skip(1) {
        check(deadline)?;
        let intersection: Vec<_> = geometries
            .iter()
            .enumerate()
            .filter(|(bit, _)| mask & (1 << bit) != 0)
            .map(|(_, (geometry, _))| *geometry)
            .collect();
        *count = crate::cost::execution_demand::offset_intersection_count(&intersection);
    }
    // Superset Möbius inversion changes intersection counts into counts of
    // offsets covered by exactly each geometry subset.
    for bit in 0..geometries.len() {
        for mask in 1..size {
            check(deadline)?;
            if mask & (1 << bit) == 0 {
                counts[mask] -= counts[mask | (1 << bit)];
            }
        }
    }
    let mut atoms = Vec::new();
    for (mask, count) in counts.into_iter().enumerate().skip(1) {
        check(deadline)?;
        if count == 0 {
            continue;
        }
        let count = u64::try_from(count)
            .map_err(|_| unsupported("arithmetic range coverage count overflow"))?;
        let mut readers: Vec<_> = geometries
            .iter()
            .enumerate()
            .filter(|(bit, _)| mask & (1 << bit) != 0)
            .flat_map(|(_, (_, readers))| readers.iter().copied())
            .collect();
        readers.sort_unstable();
        readers.dedup();
        atoms.push(Atom { readers, count });
    }
    Ok(atoms)
}

impl DemandAtoms {
    fn new(
        queries: &[(QueryRecurrence, Option<RangeEvaluation>)],
        horizon: f64,
        per_request: bool,
        deadline: Option<Instant>,
    ) -> Result<Self, CandidateSelectionError> {
        let mut fixed = BTreeMap::<u32, BTreeMap<Option<u64>, Vec<(usize, u64, u64)>>>::new();
        let mut estimated = Vec::new();
        let mut batch = BTreeMap::<u64, Vec<usize>>::new();
        for (id, (recurrence, range)) in queries.iter().enumerate() {
            check(deadline)?;
            let range = if per_request { None } else { *range };
            let steps = range.map_or(1, |range| range.evaluations_per_request());
            if steps > (1u64 << 53) {
                return Err(unsupported(
                    "range evaluation count exceeds exact floating-point integer precision",
                ));
            }
            match recurrence {
                QueryRecurrence::Repeated(
                    RepeatedDemand::FixedInterval(interval)
                    | RepeatedDemand::FixedIntervalAt { interval, .. },
                ) => {
                    let phase = match recurrence {
                        QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
                            evaluation_phase,
                            ..
                        }) => Some(evaluation_phase.0 % u64::from(interval.0)),
                        _ => None,
                    };
                    let (step, span) = range
                        .filter(|_| steps > 1)
                        .map_or((u64::from(interval.0), 0), |r| (r.step_ms, r.span_ms));
                    fixed
                        .entry(interval.0)
                        .or_default()
                        .entry(phase)
                        .or_default()
                        .push((id, step, span));
                }
                QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(estimate)) => {
                    estimated.push((id, estimate.expected_rate.0 * steps as f64))
                }
                _ => {
                    let requests = match recurrence {
                        QueryRecurrence::Repeated(RepeatedDemand::Scheduled(times)) => {
                            times.len() as u64
                        }
                        QueryRecurrence::OneTime { invocations, .. } => *invocations,
                        _ => 1,
                    };
                    let count = requests
                        .checked_mul(steps)
                        .ok_or_else(|| unsupported("batch invocation count overflow"))?;
                    if count > (1u64 << 53) {
                        return Err(unsupported(
                            "batch invocation count exceeds exact floating-point integer precision",
                        ));
                    }
                    if count > 0 {
                        batch.entry(count).or_default().push(id);
                    }
                }
            }
        }
        let mut intervals = Vec::new();
        for (interval, mut phases) in fixed {
            let unknown = phases.remove(&None).unwrap_or_default();
            let unknown_atoms = coverage(&unknown, deadline)?;
            let mut known = Vec::new();
            for (_, readers) in phases {
                let members = readers.iter().map(|(id, _, _)| *id).collect();
                let mut together = readers;
                together.extend_from_slice(&unknown);
                known.push(FixedPhase {
                    readers: members,
                    atoms: coverage(&together, deadline)?,
                });
            }
            intervals.push(FixedInterval {
                rate: 1000.0 / f64::from(interval),
                unknown: unknown.iter().map(|(id, _, _)| *id).collect(),
                unknown_atoms,
                known,
            });
        }
        Ok(Self {
            fixed: intervals,
            estimated,
            batch: batch.into_iter().collect(),
            horizon,
        })
    }
}

impl GeneralScheduleRates {
    pub(crate) fn new(
        entries: &[QueryWorkloadEntry],
        horizon: f64,
        deadline: Option<Instant>,
    ) -> Result<Self, CandidateSelectionError> {
        check(deadline)?;
        if entries.is_empty() || !horizon.is_finite() || horizon <= 0.0 {
            return Err(CandidateSelectionError::InvalidGraph(
                "invalid general schedule horizon or empty workload".into(),
            ));
        }
        let queries: Vec<_> = entries
            .iter()
            .map(|entry| (entry.recurrence.clone(), entry.range_evaluation))
            .collect();
        for (recurrence, range) in &queries {
            check(deadline)?;
            if range.is_some_and(|r| r.step_ms == 0 || r.span_ms == u64::MAX) {
                return Err(unsupported("invalid or overflowing range schedule"));
            }
            if matches!(recurrence,QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(interval) | RepeatedDemand::FixedIntervalAt{interval,..}) if interval.0==0)
            {
                return Err(CandidateSelectionError::InvalidGraph(
                    "zero fixed interval".into(),
                ));
            }
        }
        // Build checked integer products before calling the evaluator oracle.
        let evaluations = DemandAtoms::new(&queries, horizon, false, deadline)?;
        let requests = DemandAtoms::new(&queries, horizon, true, deadline)?;
        let rates = queries
            .iter()
            .map(|(r, range)| {
                evaluation_rate([(r, *range)], horizon)
                    .map_err(|e| CandidateSelectionError::InvalidGraph(e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if rates.iter().any(|rate| !rate.is_finite() || *rate < 0.0) {
            return Err(unsupported("nonfinite general schedule rate"));
        }
        Ok(Self {
            queries,
            rates,
            evaluations,
            requests,
        })
    }
    pub(crate) fn validate(
        &self,
        rates: &[f64],
        queries: usize,
    ) -> Result<(), CandidateSelectionError> {
        if queries != self.queries.len() || rates != self.rates {
            return Err(CandidateSelectionError::InvalidGraph(
                "inconsistent general schedule rates".into(),
            ));
        }
        Ok(())
    }
    pub(crate) fn coefficient_rates(&self) -> Vec<f64> {
        let mut result = Vec::new();
        for atoms in [&self.evaluations, &self.requests] {
            for interval in &atoms.fixed {
                result.extend(
                    interval
                        .known
                        .iter()
                        .flat_map(|phase| &phase.atoms)
                        .chain(&interval.unknown_atoms)
                        .map(|atom| interval.rate * atom.count as f64),
                );
            }
            result.extend(atoms.estimated.iter().map(|(_, rate)| *rate));
            let mut previous = 0;
            for (count, _) in &atoms.batch {
                result.push((*count - previous) as f64 / atoms.horizon);
                previous = *count;
            }
        }
        result.retain(|rate| *rate > 0.0);
        result
    }
    pub(crate) fn per_query_rates(&self) -> &[f64] {
        &self.rates
    }
    pub(crate) fn union_rate(&self, selected: impl Iterator<Item = bool>) -> f64 {
        self.rate(selected, false)
    }
    pub(crate) fn request_union_rate(&self, selected: impl Iterator<Item = bool>) -> f64 {
        self.rate(selected, true)
    }
    fn rate(&self, selected: impl Iterator<Item = bool>, per_request: bool) -> f64 {
        let selected: Vec<_> = selected.collect();
        let atoms = if per_request {
            &self.requests
        } else {
            &self.evaluations
        };
        let any = |readers: &[usize]| {
            readers
                .iter()
                .any(|q| selected.get(*q).copied().unwrap_or(false))
        };
        let mut total = 0.0;
        for interval in &atoms.fixed {
            let first = interval.known.iter().position(|phase| any(&phase.readers));
            for (index, phase) in interval.known.iter().enumerate() {
                for atom in &phase.atoms {
                    if atom.readers.iter().any(|q| {
                        selected.get(*q).copied().unwrap_or(false)
                            && (!interval.unknown.contains(q) || first == Some(index))
                    }) {
                        total += interval.rate * atom.count as f64;
                    }
                }
            }
            if first.is_none() {
                for atom in &interval.unknown_atoms {
                    if any(&atom.readers) {
                        total += interval.rate * atom.count as f64;
                    }
                }
            }
        }
        total += atoms
            .estimated
            .iter()
            .filter(|(q, _)| selected.get(*q).copied().unwrap_or(false))
            .map(|(_, rate)| *rate)
            .sum::<f64>();
        total += atoms
            .batch
            .iter()
            .filter(|(_, readers)| any(readers))
            .map(|(count, _)| *count)
            .max()
            .unwrap_or(0) as f64
            / atoms.horizon;
        total
    }
    pub(crate) fn encode_terms(
        &self,
        node: usize,
        reach: &[Vec<Option<Variable>>],
        variables: &mut ProblemVariables,
        constraints: &mut Vec<Constraint>,
        check_deadline: impl Fn() -> Result<(), CandidateSelectionError>,
        per_request: bool,
    ) -> Result<Vec<(Variable, f64)>, CandidateSelectionError> {
        let atoms = if per_request {
            &self.requests
        } else {
            &self.evaluations
        };
        let mut terms = Vec::new();
        let readers = |ids: &[usize]| {
            ids.iter()
                .filter_map(|q| reach[*q][node])
                .collect::<Vec<_>>()
        };
        for interval in &atoms.fixed {
            check_deadline()?;
            let mut prior = None;
            for phase in &interval.known {
                let present = or(variables, constraints, &readers(&phase.readers));
                let first = present.map(|present| without(variables, constraints, present, prior));
                for atom in &phase.atoms {
                    check_deadline()?;
                    let (unknown, known): (Vec<&usize>, Vec<&usize>) = atom
                        .readers
                        .iter()
                        .partition(|q| interval.unknown.contains(q));
                    let known: Vec<_> = known.into_iter().filter_map(|q| reach[*q][node]).collect();
                    let unknown: Vec<_> = unknown
                        .into_iter()
                        .filter_map(|q| reach[*q][node])
                        .collect();
                    let mut occupied = or(variables, constraints, &known)
                        .into_iter()
                        .collect::<Vec<_>>();
                    if let (Some(first), Some(unknown)) =
                        (first, or(variables, constraints, &unknown))
                    {
                        occupied.push(and(variables, constraints, first, unknown));
                    }
                    if let Some(value) = or(variables, constraints, &occupied) {
                        terms.push((value, interval.rate * atom.count as f64));
                    }
                }
                prior = or(
                    variables,
                    constraints,
                    &prior.into_iter().chain(present).collect::<Vec<_>>(),
                );
            }
            for atom in &interval.unknown_atoms {
                check_deadline()?;
                if let Some(unknown) = or(variables, constraints, &readers(&atom.readers)) {
                    terms.push((
                        without(variables, constraints, unknown, prior),
                        interval.rate * atom.count as f64,
                    ));
                }
            }
        }
        for &(q, rate) in &atoms.estimated {
            check_deadline()?;
            if rate > 0.0 {
                if let Some(reach) = reach[q][node] {
                    terms.push((reach, rate));
                }
            }
        }
        let mut previous = 0;
        for (count, _) in &atoms.batch {
            check_deadline()?;
            let members: Vec<_> = atoms
                .batch
                .iter()
                .filter(|(other, _)| other >= count)
                .flat_map(|(_, ids)| readers(ids))
                .collect();
            if let Some(value) = or(variables, constraints, &members) {
                terms.push((value, (*count - previous) as f64 / atoms.horizon));
            }
            previous = *count;
        }
        Ok(terms)
    }
}
fn or(
    variables: &mut ProblemVariables,
    constraints: &mut Vec<Constraint>,
    inputs: &[Variable],
) -> Option<Variable> {
    if inputs.is_empty() {
        return None;
    }
    if inputs.len() == 1 {
        return Some(inputs[0]);
    }
    let value = variables.add(variable().min(0).max(1));
    for input in inputs {
        constraints.push(constraint!(value >= *input));
    }
    constraints.push(constraint!(
        value <= inputs.iter().copied().sum::<good_lp::Expression>()
    ));
    Some(value)
}
fn and(
    variables: &mut ProblemVariables,
    constraints: &mut Vec<Constraint>,
    left: Variable,
    right: Variable,
) -> Variable {
    let value = variables.add(variable().min(0).max(1));
    constraints.push(constraint!(value <= left));
    constraints.push(constraint!(value <= right));
    constraints.push(constraint!(value >= left + right - 1));
    value
}
fn without(
    variables: &mut ProblemVariables,
    constraints: &mut Vec<Constraint>,
    value: Variable,
    excluded: Option<Variable>,
) -> Variable {
    let Some(excluded) = excluded else {
        return value;
    };
    let result = variables.add(variable().min(0).max(1));
    constraints.push(constraint!(result <= value));
    constraints.push(constraint!(result <= 1.0 - excluded));
    constraints.push(constraint!(result >= value - excluded));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::workload::{
        DemandEstimate, Predictability, Query, QueryRequirements, Rate, RepetitionInterval,
        TimestampMs,
    };
    use good_lp::{highs, Expression, Solution, SolverModel};

    fn entry(recurrence: QueryRecurrence, range: Option<(u64, u64)>) -> QueryWorkloadEntry {
        QueryWorkloadEntry {
            query: Query("sum(m)".into()),
            requirements: QueryRequirements {
                accuracy: AccuracyTarget::exact(),
                max_latency_ms: None,
            },
            predictability: Predictability::Predictable { known_at: None },
            recurrence,
            range_evaluation: range.map(|(span_ms, step_ms)| RangeEvaluation { span_ms, step_ms }),
        }
    }
    fn fixed(interval: u32, phase: Option<u64>, range: Option<(u64, u64)>) -> QueryWorkloadEntry {
        entry(
            QueryRecurrence::Repeated(match phase {
                None => RepeatedDemand::FixedInterval(RepetitionInterval(interval)),
                Some(phase) => RepeatedDemand::FixedIntervalAt {
                    interval: RepetitionInterval(interval),
                    evaluation_phase: TimestampMs(phase),
                },
            }),
            range,
        )
    }
    fn compare_all_subsets(entries: &[QueryWorkloadEntry]) {
        let schedule = GeneralScheduleRates::new(entries, 60.0, None).unwrap();
        for per_request in [false, true] {
            for mask in 0..(1usize << entries.len()) {
                let mut variables = ProblemVariables::new();
                let mut constraints = Vec::new();
                let reach: Vec<_> = (0..entries.len())
                    .map(|q| {
                        let v = variables.add(variable().binary());
                        constraints.push(constraint!(v == f64::from(mask & (1 << q) != 0)));
                        vec![Some(v)]
                    })
                    .collect();
                let terms = schedule
                    .encode_terms(
                        0,
                        &reach,
                        &mut variables,
                        &mut constraints,
                        || Ok(()),
                        per_request,
                    )
                    .unwrap();
                let expression: Expression = terms.iter().map(|(v, rate)| *rate * *v).sum();
                // Maximise to prove that an auxiliary rate cannot be inflated;
                // all OR/first-phase/max relations are exact equalities in use.
                let solution = variables
                    .minimise(-expression.clone())
                    .using(highs)
                    .with_all(constraints)
                    .solve()
                    .unwrap();
                let expected = evaluation_rate(
                    entries
                        .iter()
                        .enumerate()
                        .filter(|(q, _)| mask & (1 << q) != 0)
                        .map(|(_, entry)| {
                            (
                                &entry.recurrence,
                                if per_request {
                                    None
                                } else {
                                    entry.range_evaluation
                                },
                            )
                        }),
                    60.0,
                )
                .unwrap();
                let actual = solution.eval(&expression);
                let decoded = schedule.rate(
                    (0..entries.len()).map(|q| mask & (1 << q) != 0),
                    per_request,
                );
                assert!(
                    (decoded - expected).abs() <= 1e-10 * expected.max(1.0),
                    "decoded {decoded} != oracle {expected}"
                );
                assert!(
                    (actual - expected).abs() <= 1e-10 * expected.max(1.0),
                    "mask={mask}, requests={per_request}: {actual} != {expected}"
                );
            }
        }
    }
    #[test]
    fn fixed_ranges_join_unknown_to_first_selected_known_phase() {
        compare_all_subsets(&[
            fixed(1000, None, Some((6, 2))),
            fixed(1000, Some(0), Some((6, 3))),
            fixed(1000, Some(500), Some((6, 2))),
            fixed(2000, Some(0), Some((6, 2))),
        ]);
    }
    #[test]
    fn mixed_estimates_scheduled_batches_and_unknown_match_evaluator() {
        let estimate = |rate| {
            QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(DemandEstimate {
                expected_rate: Rate(rate),
                observed_at: None,
                valid_for: None,
            }))
        };
        compare_all_subsets(&[
            fixed(997, None, Some((10, 3))),
            entry(estimate(0.137), Some((8, 2))),
            entry(estimate(0.193), None),
            entry(
                QueryRecurrence::Repeated(RepeatedDemand::Scheduled(vec![
                    TimestampMs(10),
                    TimestampMs(20),
                ])),
                Some((6, 3)),
            ),
            entry(
                QueryRecurrence::OneTime {
                    invocations: 3,
                    execute_at: None,
                },
                Some((8, 2)),
            ),
            entry(QueryRecurrence::Unknown, Some((10, 1))),
        ]);
    }
    #[test]
    fn same_step_large_ranges_are_counted_without_timestamp_enumeration() {
        let entries = [
            fixed(1000, None, Some((1_000_000_000, 1))),
            fixed(1000, None, Some((500_000_000, 1))),
        ];
        let schedule = GeneralScheduleRates::new(&entries, 60.0, None).unwrap();
        assert_eq!(schedule.evaluations.fixed[0].unknown_atoms.len(), 2);
        assert_eq!(
            schedule.union_rate([true, true].into_iter()),
            1_000_000_001.0
        );
        assert_eq!(schedule.request_union_rate([true, true].into_iter()), 1.0);
    }
    #[test]
    fn large_coprime_ranges_use_exact_arithmetic_coverage() {
        let entries = [
            fixed(1000, None, Some((1_000_000_000, 2))),
            fixed(1000, None, Some((1_000_000_000, 3))),
        ];
        let schedule = GeneralScheduleRates::new(&entries, 60.0, None).unwrap();
        assert_eq!(schedule.evaluations.fixed[0].unknown_atoms.len(), 3);
        assert_eq!(schedule.union_rate([true, true].into_iter()), 666_666_668.0);
        assert_eq!(
            schedule.union_rate([true, false].into_iter()),
            500_000_001.0
        );
        assert_eq!(
            schedule.union_rate([false, true].into_iter()),
            333_333_334.0
        );
        compare_all_subsets(&[
            fixed(1000, None, Some((60, 2))),
            fixed(1000, None, Some((60, 3))),
        ]);
    }
    #[test]
    fn arithmetic_atoms_match_offset_coverage_with_duplicates_and_lcm_overflow() {
        let readers = [(0, 2, 60), (1, 3, 60), (2, 2, 60), (3, 5, 37), (4, 7, 0)];
        let atoms = coverage(&readers, None).unwrap();
        let actual: BTreeMap<_, _> = atoms
            .into_iter()
            .map(|atom| (atom.readers, atom.count))
            .collect();
        let mut expected = BTreeMap::<Vec<usize>, u64>::new();
        for offset in 0..=60 {
            let members: Vec<_> = readers
                .iter()
                .filter(|(_, step, span)| offset <= *span && offset % step == 0)
                .map(|(id, _, _)| *id)
                .collect();
            if !members.is_empty() {
                *expected.entry(members).or_default() += 1;
            }
        }
        assert_eq!(actual, expected);
        let large = 1u64 << 63;
        let atoms = coverage(&[(0, large - 1, large), (1, large - 3, large)], None).unwrap();
        assert_eq!(atoms.iter().map(|atom| atom.count).sum::<u64>(), 3);
        assert!(atoms
            .iter()
            .any(|atom| atom.readers == vec![0, 1] && atom.count == 1));
    }
    #[test]
    fn excessive_mixed_offset_domains_and_deadlines_are_explicit() {
        let entries: Vec<_> = (1..=13)
            .map(|step| fixed(1000, None, Some((10_000_000, step))))
            .collect();
        assert!(matches!(
            GeneralScheduleRates::new(&entries, 60.0, None),
            Err(CandidateSelectionError::UnsupportedDirectCost(_))
        ));
        assert!(matches!(
            GeneralScheduleRates::new(&entries, 60.0, Some(Instant::now())),
            Err(CandidateSelectionError::Solver(_))
        ));
    }
}
