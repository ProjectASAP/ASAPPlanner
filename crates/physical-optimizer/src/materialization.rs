//! #509 Stage 2 materialization: for each summary state of one logical
//! candidate, whether it is computed at query time (not materialized, the
//! default), maintained at ingestion time, or, for tumbling panes, computed
//! at query time and kept across evaluations.
//!
//! **Eligibility (S4).** A `SummaryAgg` may run at ingestion time when
//!
//! 1. the data keeps arriving (`DataArrival::ContinuouslyIngesting` or
//!    `Mixed`; `Unknown` counts as not ingesting);
//! 2. every root reaching it repeats and is `Predictable`; and
//! 3. its window can be maintained as data arrives: it is a tumbling pane
//!    (a summary merged by a `SummaryMerge`, over its own time range), or it
//!    reads one fixed window per evaluation, that is, every root reaching it
//!    recurs with a known phase (`FixedIntervalAt`) and the window is no
//!    longer than the interval, so successive windows do not overlap.
//!
//! A summary over a merge of panes reads a window that slides with the
//! evaluation; it is not maintainable and stays at query time.
//!
//! **Query time, kept (Example 4, B3).** A tumbling pane whose roots all
//! repeat at a fixed interval equal to the pane width may instead be built
//! at query time and kept: each evaluation builds the newest pane from its
//! width of raw data, merges it with the panes kept from earlier
//! evaluations, then keeps it and drops the oldest. This needs neither
//! arriving data nor predictability; the deployment must be able to keep
//! query-time state (Stage 3 checks it).
//!
//! **Units.** The panes merged by one `SummaryMerge` are decided together:
//! a pane chain is maintained as one stream of panes, and a mixed chain is
//! never cheaper. Panes shared by two merges join both chains into one unit.
//! A merge of panes of different widths (uneven shared segments, Q67) is no
//! stream of panes, so its chain stays at query time.
//!
//! **Down-closed sets.** Everything upstream of an ingestion-time node also
//! runs at ingestion time (#509), so a unit is at ingestion time only if
//! every unit below it is. [`MaterializationSpace::down_closed_sets`]
//! enumerates exactly those choices, each unit not materialized, at
//! ingestion time or kept, the empty choice (all query time) first.
//!
//! **Not materialized for several consumers (Q44, Example 4 A3)** would
//! duplicate a shared sub-DAG per consuming query. It is not generated yet:
//! a shared query-time node is computed once per evaluation for all its
//! consumers.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::schema::{FieldDataType, GroupingStrategy};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode};
use asap_types::workload::{
    DataArrival, DataWorkload, Predictability, QueryRecurrence, RepeatedDemand, RootDemand,
};

/// Most physical candidates Stage 2 enumerates for one logical candidate.
/// Above it, Stage 2 searches greedily and the result is not guaranteed
/// optimal.
pub const MAX_PHYSICAL_PER_LOGICAL: usize = 16;

/// Summaries whose materialization is decided together.
#[derive(Debug, Clone)]
pub struct Unit {
    pub summaries: Vec<Rc<OperatorNode>>,
    /// E.g. "Kll ×5 panes" or "exact Sum".
    pub label: String,
    /// The unit may run at ingestion time.
    pub ingestion: bool,
    /// The unit may be kept at query time (tumbling panes only).
    pub kept: bool,
}

/// How a unit is materialized; a unit absent from a [`Choices`] is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Choice {
    /// Maintained at ingestion time.
    IngestionTime,
    /// Built at query time and kept across evaluations (B3).
    Kept,
}

/// One materialization choice per unit index.
pub type Choices = BTreeMap<usize, Choice>;

/// The materialization choices of one logical candidate.
#[derive(Debug, Clone)]
pub struct MaterializationSpace {
    pub units: Vec<Unit>,
    /// `below[u]`: the units with a summary strictly below one of `u`'s.
    below: Vec<BTreeSet<usize>>,
}

impl MaterializationSpace {
    /// The eligible units of `roots`; `demand[i]` is the demand of
    /// `roots[i]` (a root without one is not eligible).
    pub fn new(roots: &[Rc<OperatorNode>], demand: &[RootDemand], data: &DataWorkload) -> Self {
        let empty = Self {
            units: Vec::new(),
            below: Vec::new(),
        };
        let arriving = matches!(
            data.arrival,
            DataArrival::ContinuouslyIngesting | DataArrival::Mixed
        );
        // Every summary, with the roots reaching it, in discovery order.
        let mut summaries: Vec<Rc<OperatorNode>> = Vec::new();
        let mut reaching: HashMap<*const OperatorNode, BTreeSet<usize>> = HashMap::new();
        let mut merges: Vec<Rc<OperatorNode>> = Vec::new();
        let mut seen_merges = HashSet::new();
        for (index, root) in roots.iter().enumerate() {
            for node in OperatorNode::reachable(root) {
                match &node.operator {
                    Operator::ASAP(ASAPOp::SummaryAgg { .. }) => {
                        let entry = reaching.entry(Rc::as_ptr(&node)).or_default();
                        if entry.is_empty() {
                            summaries.push(Rc::clone(&node));
                        }
                        entry.insert(index);
                    }
                    Operator::ASAP(ASAPOp::SummaryMerge { .. })
                        if seen_merges.insert(Rc::as_ptr(&node)) =>
                    {
                        merges.push(Rc::clone(&node));
                    }
                    _ => {}
                }
            }
        }
        let panes: HashSet<*const OperatorNode> = merges
            .iter()
            .flat_map(|merge| merge.children())
            .map(Rc::as_ptr)
            .collect();
        // Per summary: (may run at ingestion time, may be kept).
        let options: Vec<(bool, bool)> = summaries
            .iter()
            .map(|summary| {
                let roots: Vec<&RootDemand> = reaching[&Rc::as_ptr(summary)]
                    .iter()
                    .filter_map(|&r| demand.get(r))
                    .collect();
                let all_roots = roots.len() == reaching[&Rc::as_ptr(summary)].len();
                let pane = panes.contains(&Rc::as_ptr(summary));
                let window = window_of(summary);
                let ingestion = arriving
                    && all_roots
                    && roots.iter().all(|d| repeats_predictably(d))
                    && !forces_query_time(summary)
                    && match window {
                        Window::Fixed { length_ms } => {
                            pane || roots.iter().all(|d| fixed_window_fits(d, length_ms))
                        }
                        Window::None | Window::Sliding => false,
                    };
                let kept = pane
                    && all_roots
                    && matches!(window, Window::Fixed { length_ms }
                        if roots.iter().all(|d| evaluates_every(d, length_ms)));
                (ingestion, kept)
            })
            .collect();
        let eligible: Vec<bool> = options.iter().map(|&(i, k)| i || k).collect();
        let index: HashMap<*const OperatorNode, usize> = summaries
            .iter()
            .enumerate()
            .map(|(i, s)| (Rc::as_ptr(s), i))
            .collect();
        // Union the panes of each merge whose panes are all eligible; a
        // merge with an ineligible pane makes its whole chain ineligible.
        let mut parent: Vec<usize> = (0..summaries.len()).collect();
        let mut usable = eligible.clone();
        fn find(parent: &mut [usize], i: usize) -> usize {
            if parent[i] != i {
                let root = find(parent, parent[i]);
                parent[i] = root;
            }
            parent[i]
        }
        for merge in &merges {
            let members: Vec<usize> = merge
                .children()
                .into_iter()
                .filter_map(|c| index.get(&Rc::as_ptr(c)).copied())
                .collect();
            // Panes of different widths (uneven shared segments, Q67) are
            // not one stream of panes: no pane is a later one shifted back.
            let uneven = members
                .windows(2)
                .any(|pair| window_of(&summaries[pair[0]]) != window_of(&summaries[pair[1]]));
            if uneven || members.iter().any(|&m| !eligible[m]) {
                for &m in &members {
                    usable[m] = false;
                }
            }
            for pair in members.windows(2) {
                let (a, b) = (find(&mut parent, pair[0]), find(&mut parent, pair[1]));
                parent[a] = b;
            }
        }
        // A chain with one unusable member is unusable as a whole.
        for i in 0..summaries.len() {
            if !usable[i] {
                let root = find(&mut parent, i);
                usable[root] = false;
            }
        }
        let mut unit_of: HashMap<usize, usize> = HashMap::new();
        let mut units: Vec<Unit> = Vec::new();
        let mut members: Vec<Vec<usize>> = Vec::new();
        for i in 0..summaries.len() {
            let root = find(&mut parent, i);
            if !usable[i] || !usable[root] {
                continue;
            }
            let u = *unit_of.entry(root).or_insert_with(|| {
                units.push(Unit {
                    summaries: Vec::new(),
                    label: String::new(),
                    ingestion: true,
                    kept: true,
                });
                members.push(Vec::new());
                units.len() - 1
            });
            units[u].summaries.push(Rc::clone(&summaries[i]));
            units[u].ingestion &= options[i].0;
            units[u].kept &= options[i].1;
            members[u].push(i);
        }
        // A chain whose members allow different options has none in common.
        let (mut units, members): (Vec<Unit>, Vec<Vec<usize>>) = units
            .into_iter()
            .zip(members)
            .filter(|(unit, _)| unit.ingestion || unit.kept)
            .unzip();
        for (unit, members) in units.iter_mut().zip(&members) {
            let name = family_name(&summaries[members[0]]);
            unit.label = if members
                .iter()
                .any(|m| panes.contains(&Rc::as_ptr(&summaries[*m])))
            {
                format!("{name} ×{} panes", members.len())
            } else {
                name
            };
        }
        let unit_index: HashMap<*const OperatorNode, usize> = units
            .iter()
            .enumerate()
            .flat_map(|(u, unit)| unit.summaries.iter().map(move |s| (Rc::as_ptr(s), u)))
            .collect();
        let below = units
            .iter()
            .enumerate()
            .map(|(u, unit)| {
                unit.summaries
                    .iter()
                    .flat_map(|s| s.children().into_iter().flat_map(OperatorNode::reachable))
                    .filter_map(|n| unit_index.get(&Rc::as_ptr(&n)).copied())
                    .filter(|&v| v != u)
                    .collect()
            })
            .collect();
        if units.is_empty() {
            return empty;
        }
        Self { units, below }
    }

    /// Whether `unit`, not yet materialized in `choices`, may take
    /// `choice` with the ingestion-time units staying down-closed.
    pub fn can_choose(&self, choices: &Choices, unit: usize, choice: Choice) -> bool {
        !choices.contains_key(&unit)
            && match choice {
                Choice::IngestionTime => {
                    self.units[unit].ingestion
                        && self.below[unit]
                            .iter()
                            .all(|v| choices.get(v) == Some(&Choice::IngestionTime))
                }
                Choice::Kept => self.units[unit].kept,
            }
    }

    /// Every choice of materialization per unit whose ingestion-time units
    /// are down-closed, fewest materialized units first (the empty choice,
    /// all query time, is first); `None` when there are more than `max`.
    pub fn down_closed_sets(&self, max: usize) -> Option<Vec<Choices>> {
        // Units bottom-up, so a unit is decided after every unit below it.
        let mut order: Vec<usize> = Vec::new();
        while order.len() < self.units.len() {
            let before = order.len();
            for u in 0..self.units.len() {
                if !order.contains(&u) && self.below[u].iter().all(|v| order.contains(v)) {
                    order.push(u);
                }
            }
            assert!(
                order.len() > before,
                "units below one another form no cycle"
            );
        }
        let mut sets = vec![Choices::new()];
        for u in order {
            let additions: Vec<_> = sets
                .iter()
                .flat_map(|set| {
                    [Choice::IngestionTime, Choice::Kept]
                        .into_iter()
                        .filter(|&choice| self.can_choose(set, u, choice))
                        .map(move |choice| {
                            let mut next = set.clone();
                            next.insert(u, choice);
                            next
                        })
                })
                .collect();
            sets.extend(additions);
            if sets.len() > max {
                return None;
            }
        }
        sets.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
        Some(sets)
    }

    /// The assignment putting the summaries of `choices` at ingestion time,
    /// or keeping them at query time.
    pub fn assignment(&self, choices: &Choices) -> asap_types::ir::MaterializationAssignment {
        let mut assignment = asap_types::ir::MaterializationAssignment::all_query_time();
        for (&u, choice) in choices {
            for summary in &self.units[u].summaries {
                match choice {
                    Choice::IngestionTime => {
                        assignment.set(summary, ExecutionTiming::IngestionTime)
                    }
                    Choice::Kept => assignment.set_kept(summary),
                }
            }
        }
        assignment
    }

    /// E.g. "ingestion time: Kll ×5 panes" or "query time, kept: Kll ×5
    /// panes"; empty for the empty choice.
    pub fn label(&self, choices: &Choices) -> String {
        [
            (Choice::IngestionTime, "ingestion time"),
            (Choice::Kept, "query time, kept"),
        ]
        .into_iter()
        .filter_map(|(want, name)| {
            let units: Vec<_> = choices
                .iter()
                .filter(|(_, choice)| **choice == want)
                .map(|(&u, _)| self.units[u].label.as_str())
                .collect();
            (!units.is_empty()).then(|| format!("{name}: {}", units.join(", ")))
        })
        .collect::<Vec<_>>()
        .join("; ")
    }
}

fn repeats_predictably(demand: &RootDemand) -> bool {
    matches!(demand.recurrence, QueryRecurrence::Repeated(_))
        && matches!(demand.predictability, Predictability::Predictable { .. })
}

/// Evaluations come every `width_ms`, so each builds one new pane of that
/// width.
fn evaluates_every(demand: &RootDemand, width_ms: u64) -> bool {
    matches!(
        demand.recurrence,
        QueryRecurrence::Repeated(
            RepeatedDemand::FixedInterval(interval)
                | RepeatedDemand::FixedIntervalAt { interval, .. }
        ) if u64::from(interval.0) == width_ms
    )
}

/// One window per evaluation, phase known, and no overlap between
/// successive evaluations' windows.
fn fixed_window_fits(demand: &RootDemand, length_ms: u64) -> bool {
    matches!(
        demand.recurrence,
        QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt { interval, .. })
            if length_ms <= u64::from(interval.0)
    )
}

/// Whether something below `summary` can only run at query time.
fn forces_query_time(summary: &Rc<OperatorNode>) -> bool {
    summary
        .children()
        .into_iter()
        .flat_map(OperatorNode::reachable)
        .any(|n| {
            n.timing == Some(ExecutionTiming::QueryTime)
                || matches!(
                    n.operator,
                    Operator::ASAP(
                        ASAPOp::SummaryEstimate { .. } | ASAPOp::EvaluatePopulation { .. }
                    )
                )
        })
}

/// The window a summary's input reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    /// No time range: not a window over arriving data.
    None,
    /// One range of `length_ms` per evaluation.
    Fixed { length_ms: u64 },
    /// A merge of panes: the window slides with the evaluation.
    Sliding,
}

fn window_of(summary: &Rc<OperatorNode>) -> Window {
    fn below(node: &Rc<OperatorNode>) -> Window {
        match &node.operator {
            Operator::NonASAP(NonASAPOp::TimeRange { range, .. }) => Window::Fixed {
                length_ms: u64::try_from(range.as_millis()).unwrap_or(u64::MAX),
            },
            Operator::ASAP(ASAPOp::SummaryMerge { .. }) => Window::Sliding,
            _ => node
                .children()
                .into_iter()
                .map(below)
                .fold(Window::None, |a, b| match (a, b) {
                    (Window::Sliding, _) | (_, Window::Sliding) => Window::Sliding,
                    (Window::Fixed { length_ms: x }, Window::Fixed { length_ms: y }) => {
                        Window::Fixed {
                            length_ms: x.max(y),
                        }
                    }
                    (Window::Fixed { length_ms }, Window::None)
                    | (Window::None, Window::Fixed { length_ms }) => Window::Fixed { length_ms },
                    (Window::None, Window::None) => Window::None,
                }),
        }
    }
    summary
        .children()
        .first()
        .map_or(Window::None, |child| below(child))
}

fn family_name(summary: &OperatorNode) -> String {
    match &summary.operator {
        Operator::ASAP(ASAPOp::SummaryAgg { family, .. }) => match family {
            // A Hydra's family is the per-group sketch it emulates.
            FieldDataType::Sketch(_, GroupingStrategy::SharedMultiSubpopulation { kind, .. }) => {
                format!("{kind:?}")
            }
            FieldDataType::Sketch(kind, _) => format!("{:?}", kind.algorithm()),
            FieldDataType::ExactAggregate(kind, _) => format!("exact {kind:?}"),
            other => format!("{other:?}"),
        },
        _ => "summary".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::implementation::physical_candidates::{stage2_physical, Stage2Candidates};
    use crate::test_support::lower_promql_all;
    use asap_logical_optimizer::pass1::logical_candidates::{
        compose_logical_candidate, LocalLogicalTarget,
    };
    use asap_logical_optimizer::pass2::identical_expressions::stage1_logical_candidates;
    use asap_logical_optimizer::pass2::window_composition::WindowForm;
    use asap_types::ir::export::PhysicalASAPOperatorPayload as Payload;
    use asap_types::ir::QueryRoot;
    use asap_types::types::AccuracyTarget;
    use asap_types::workload::{RepetitionInterval, TimestampMs};

    const P99: AccuracyTarget = AccuracyTarget::Epsilon(0.01);

    fn every(ms: u32) -> RootDemand {
        RootDemand {
            accuracy: None,
            recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                RepetitionInterval(ms),
            )),
            predictability: Predictability::Predictable { known_at: None },
            latency_ms: None,
        }
    }

    fn every_at(ms: u32) -> RootDemand {
        RootDemand {
            recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
                interval: RepetitionInterval(ms),
                evaluation_phase: TimestampMs(0),
            }),
            ..every(ms)
        }
    }

    fn ingesting() -> DataWorkload {
        DataWorkload {
            arrival: DataArrival::ContinuouslyIngesting,
            ..Default::default()
        }
    }

    /// The workload's roots with each target's last alternative of `form`
    /// (a summary, when it has one), in Pass 2's independent variant.
    fn roots(
        queries: &[(&str, AccuracyTarget)],
        demand: &[RootDemand],
        form: WindowForm,
    ) -> Vec<Rc<OperatorNode>> {
        roots_with(queries, demand, |t, c| t.windows[c] == form)
    }

    /// The workload's roots with each target's last alternative that `keep`
    /// accepts (else the pass-through).
    fn roots_with(
        queries: &[(&str, AccuracyTarget)],
        demand: &[RootDemand],
        keep: impl Fn(&LocalLogicalTarget, usize) -> bool,
    ) -> Vec<Rc<OperatorNode>> {
        let roots = lower_promql_all(queries)
            .into_iter()
            .enumerate()
            .map(|(i, root)| (i, QueryRoot::Operator(root)))
            .collect();
        let variants = stage1_logical_candidates(roots, &Default::default(), demand).unwrap();
        let inventory = &variants[0].inventory;
        let choice: Vec<_> = inventory
            .targets
            .iter()
            .map(|t| {
                (0..t.alternatives.len())
                    .rev()
                    .find(|&c| keep(t, c))
                    .unwrap_or(0)
            })
            .collect();
        compose_logical_candidate(inventory, &choice)
            .unwrap()
            .into_iter()
            .map(|(_, root)| match root {
                QueryRoot::Operator(node) => node,
                QueryRoot::Scalar(_) => panic!("operator root"),
            })
            .collect()
    }

    fn stage2(
        roots: &[Rc<OperatorNode>],
        demand: &[RootDemand],
        data: &DataWorkload,
    ) -> Stage2Candidates {
        stage2_physical("L1", roots, demand, data, &|_| None).unwrap()
    }

    fn timings(
        candidate: &crate::implementation::physical_candidates::PhysicalCandidate,
        is: fn(&Payload) -> bool,
    ) -> Vec<ExecutionTiming> {
        candidate
            .dag
            .nodes
            .iter()
            .filter(|n| is(&n.payload))
            .map(|n| n.output_state.timing)
            .collect()
    }

    fn is_build(p: &Payload) -> bool {
        matches!(p, Payload::SummaryAgg { .. })
    }

    fn is_merge(p: &Payload) -> bool {
        matches!(p, Payload::SummaryMerge)
    }

    const P99_5M: &str = "quantile_over_time(0.99, m[5m])";

    /// A Hydra summary's unit is labeled by its Hydra kind ("HydraCms"),
    /// not by the per-group Count-Min it emulates.
    #[test]
    fn hydra_unit_is_labeled_by_its_hydra_kind() {
        let demand = [every_at(300_000)];
        let approximate = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.01,
        };
        let root = crate::test_support::lower_promql("count by (job) (m)", approximate);
        let root = asap_types::ir::schema_support::with_promql_series_identity(&root).unwrap();
        let variants = stage1_logical_candidates(
            vec![(0, QueryRoot::Operator(root))],
            &Default::default(),
            &demand,
        )
        .unwrap();
        let inventory = &variants[0].inventory;
        let choice: Vec<_> = inventory
            .targets
            .iter()
            .map(|t| {
                t.groupings
                    .iter()
                    .position(|g| *g != GroupingStrategy::default())
                    .unwrap_or(0)
            })
            .collect();
        let roots: Vec<_> = compose_logical_candidate(inventory, &choice)
            .unwrap()
            .into_iter()
            .map(|(_, root)| match root {
                QueryRoot::Operator(node) => node,
                QueryRoot::Scalar(_) => panic!("operator root"),
            })
            .collect();
        let space = MaterializationSpace::new(&roots, &demand, &ingesting());
        let labels: Vec<_> = space.units.iter().map(|u| u.label.as_str()).collect();
        assert_eq!(labels, ["HydraCms"]);
    }

    /// Panes of a query repeating every minute over arriving data give a
    /// second candidate that builds all five panes at ingestion time and
    /// merges them at query time (Example 4, B1), and a third that builds
    /// them at query time and keeps them (B3).
    #[test]
    fn tumbling_panes_can_run_at_ingestion_time_or_be_kept() {
        let demand = [every(60_000)];
        let roots = roots(
            &[(P99_5M, P99)],
            &demand,
            WindowForm::Tumbling { pane_ms: 60_000 },
        );
        let out = stage2(&roots, &demand, &ingesting());
        assert!(out.exhaustive);
        let [all_query, maintained, kept] = &out.candidates[..] else {
            panic!("three candidates: {:?}", out.candidates.len())
        };
        assert!(all_query.dag.nodes.iter().all(|n| !n.kept));
        assert!(maintained.dag.nodes.iter().all(|n| !n.kept));
        assert_eq!(kept.materialization, "query time, kept: DDSketch ×5 panes");
        assert!(timings(kept, |_| true)
            .iter()
            .all(|t| *t == ExecutionTiming::QueryTime));
        let kept_nodes: Vec<_> = kept.dag.nodes.iter().filter(|n| n.kept).collect();
        assert_eq!(kept_nodes.len(), 5);
        assert!(kept_nodes.iter().all(|n| is_build(&n.payload)));
        assert!(timings(all_query, |_| true)
            .iter()
            .all(|t| *t == ExecutionTiming::QueryTime));
        assert!(
            maintained.materialization.ends_with(" ×5 panes"),
            "{}",
            maintained.materialization
        );
        assert_eq!(
            timings(maintained, is_build),
            vec![ExecutionTiming::IngestionTime; 5]
        );
        assert_eq!(
            timings(maintained, is_merge),
            vec![ExecutionTiming::QueryTime]
        );
    }

    /// Ingestion time needs arriving data (`Unknown` counts as not
    /// ingesting) and repeated, predictable roots; keeping panes at query
    /// time needs only repetition.
    #[test]
    fn ingestion_time_needs_arrival_and_predictable_repetition() {
        let demand = [every(60_000)];
        let roots = roots(
            &[(P99_5M, P99)],
            &demand,
            WindowForm::Tumbling { pane_ms: 60_000 },
        );
        let labels = |demand: &[RootDemand], data: &DataWorkload| -> Vec<String> {
            stage2(&roots, demand, data)
                .candidates
                .into_iter()
                .map(|c| c.materialization)
                .collect()
        };
        let kept_only = ["", "query time, kept: DDSketch ×5 panes"];
        for arrival in [DataArrival::AtRest, DataArrival::Unknown] {
            let data = DataWorkload {
                arrival,
                ..Default::default()
            };
            assert_eq!(labels(&demand, &data), kept_only, "{arrival:?}");
        }
        let mixed = DataWorkload {
            arrival: DataArrival::Mixed,
            ..Default::default()
        };
        assert_eq!(labels(&demand, &mixed).len(), 3);
        let ad_hoc = RootDemand {
            predictability: Predictability::AdHoc,
            ..every(60_000)
        };
        let once = RootDemand {
            recurrence: QueryRecurrence::OneTime {
                invocations: 1,
                execute_at: None,
            },
            ..every(60_000)
        };
        assert_eq!(labels(&[ad_hoc], &ingesting()), kept_only);
        assert_eq!(labels(&[once], &ingesting()), [""]);
    }

    /// Keeping panes needs one new pane per evaluation: a 5-min window
    /// every 2 min has 1-min panes, so it gets no kept option.
    #[test]
    fn kept_panes_need_one_new_pane_per_evaluation() {
        let demand = [every(120_000)];
        let roots = roots(
            &[(P99_5M, P99)],
            &demand,
            WindowForm::Tumbling { pane_ms: 60_000 },
        );
        let labels: Vec<_> = stage2(&roots, &demand, &ingesting())
            .candidates
            .into_iter()
            .map(|c| c.materialization)
            .collect();
        assert_eq!(labels, ["", "ingestion time: DDSketch ×5 panes"]);
    }

    /// A whole window is maintainable only when each evaluation reads one
    /// fixed window: a known phase, and a window no longer than the interval.
    #[test]
    fn whole_window_needs_a_phase_and_no_overlap() {
        let count = |demand: RootDemand| {
            let demand = [demand];
            let roots = roots(
                &[("quantile_over_time(0.99, m[1m])", P99)],
                &demand,
                WindowForm::Whole,
            );
            stage2(&roots, &demand, &ingesting()).candidates.len()
        };
        assert_eq!(count(every(60_000)), 1, "no phase");
        assert_eq!(count(every_at(10_000)), 1, "windows overlap");
        assert_eq!(count(every_at(60_000)), 2);
    }

    /// A summary over a merge of panes slides with the evaluation, so it is
    /// not maintainable, and a summary is at ingestion time only if every
    /// summary below it is: one exact per-series sum (fixed window) under a
    /// Count-Min top-k gives three sets, not four.
    #[test]
    fn sets_are_down_closed() {
        let demand = [every_at(60_000)];
        // The exact sum, and a Count-Min top-k reading it (not absorbing it).
        let roots = roots_with(
            &[(
                "topk(5, sum_over_time(m[1m]))",
                AccuracyTarget::Epsilon(0.01),
            )],
            &demand,
            |t, c| t.windows[c] == WindowForm::Whole && t.absorbs[c].is_none(),
        );
        let space = MaterializationSpace::new(&roots, &demand, &ingesting());
        let labels: Vec<_> = space.units.iter().map(|u| u.label.as_str()).collect();
        assert_eq!(labels.len(), 2, "{labels:?}");
        let sets = space.down_closed_sets(MAX_PHYSICAL_PER_LOGICAL).unwrap();
        assert_eq!(sets.len(), 3, "{labels:?}");
        for set in &sets {
            for (&u, choice) in set {
                assert_eq!(*choice, Choice::IngestionTime);
                assert!(space.below[u]
                    .iter()
                    .all(|v| set.get(v) == Some(&Choice::IngestionTime)));
            }
        }
        // Over panes, the top-k sketch reads a sliding window: only the
        // panes are maintainable.
        let demand = [every(10_000)];
        let roots = roots_with(
            &[(
                "topk(5, sum_over_time(m[1m]))",
                AccuracyTarget::Epsilon(0.01),
            )],
            &demand,
            |t, c| t.absorbs[c].is_none() && c > 0,
        );
        let space = MaterializationSpace::new(&roots, &demand, &ingesting());
        let labels: Vec<_> = space.units.iter().map(|u| u.label.as_str()).collect();
        assert_eq!(labels, ["exact Sum ×6 panes"]);
    }

    /// Above the cap, Stage 2 moves one unit at a time while the score
    /// improves, and flags the result as not exhaustive.
    #[test]
    fn above_the_cap_the_search_is_greedy() {
        let queries: Vec<_> = (0..5)
            .map(|i| (format!("quantile_over_time(0.99, m{i}[5m])"), P99))
            .collect();
        let queries: Vec<_> = queries
            .iter()
            .map(|(q, a)| (q.as_str(), a.clone()))
            .collect();
        let demand = vec![every(60_000); 5];
        let roots = roots(&queries, &demand, WindowForm::Tumbling { pane_ms: 60_000 });
        // 2^5 = 32 down-closed sets.
        let ingestion_builds =
            |c: &crate::implementation::physical_candidates::PhysicalCandidate| {
                timings(c, is_build)
                    .iter()
                    .filter(|t| !t.is_query_time())
                    .count()
            };
        // Lower is better: one more maintained chain is always better.
        let score = |c: &crate::implementation::physical_candidates::PhysicalCandidate| {
            Some(-(ingestion_builds(c) as f64))
        };
        let out = stage2_physical("L1", &roots, &demand, &ingesting(), &score).unwrap();
        assert!(!out.exhaustive);
        let path: Vec<_> = out.candidates.iter().map(ingestion_builds).collect();
        assert_eq!(path, vec![0, 5, 10, 15, 20, 25]);
    }
}
