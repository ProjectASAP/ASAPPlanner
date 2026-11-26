//! Executions left out before the MILP, each exactly: those whose least
//! possible latency is over every reader's bound, those no plan with them
//! beats a feasible plan with (T8), and those over a limit alone.

use super::*;

/// Per execution, whether its least possible latency is over the
/// `max_latency_ms` of every query that may read it (S2): its least work,
/// its own and the least of each input's choices, plus its least
/// freshness, the allowed lateness and, at ingestion time, its own refresh
/// with the wait per run. The work is per step (at least one per request)
/// and each share is taken at its least retention (`per_execution`, which
/// takes the most retention can take off), so this is a lower bound on
/// every plan's latency through the execution. Iterated from 0 (every
/// execution's time is at least 0, so each iterate stays a lower bound)
/// until stable, so cycles of selectable edges are bounded too.
pub(super) fn over_every_readers_bound(
    inputs: &ClusterInputs<'_>,
    shapes: &[QueryShape],
    kept: &[bool],
    active: &[ClusterLimit],
) -> Vec<bool> {
    let least = least_latency(inputs, kept);
    (0..least.len())
        .map(|index| {
            let readers: Vec<&QueryShape> = shapes
                .iter()
                .filter(|shape| shape.reach.contains(&index))
                .collect();
            !readers.is_empty()
                && readers.iter().all(|shape| {
                    let bounded = active.contains(&ClusterLimit::QueryLatencyMs {
                        query: shape.query.0,
                    });
                    bounded
                        && shape.max_latency_ms.is_some_and(|bound| {
                            least[index] + shape.request_ms > bound * (1.0 + 1e-9)
                        })
                })
        })
        .collect()
}

/// Per execution, the least latency of its sub-DAG (work plus freshness,
/// [`over_every_readers_bound`]), without a query's fixed cost per
/// request: a lower bound on any plan's latency through it.
pub(super) fn least_latency(inputs: &ClusterInputs<'_>, kept: &[bool]) -> Vec<f64> {
    let selectable = inputs.selection.selectable;
    let count = selectable.executions().len();
    let lateness = inputs.cost_inputs.data_plane_execution.allowed_lateness_ms as f64;
    let wait_per_run = inputs
        .cost_inputs
        .measured_operation_costs
        .refresh_io_wait
        .ns_per_refresh
        * 1e-6;
    // Whether an execution's inputs may share executions below it (an
    // in-query diamond): their memo groups' descendants (over the
    // selectable edges) intersect. Without one, their least works add up.
    // Only query-time producers count: an ingestion-time execution's work
    // is in the freshness, which takes the largest, not a sum.
    let mut group_inputs: HashMap<_, BTreeSet<_>> = HashMap::new();
    for edge in selectable
        .edges()
        .iter()
        .filter(|edge| !at_ingestion(inputs, edge.producer))
    {
        group_inputs
            .entry(memo_group_of(inputs, edge.consumer))
            .or_default()
            .insert(memo_group_of(inputs, edge.producer));
    }
    let mut below: HashMap<_, BTreeSet<_>> = HashMap::new();
    let descendants = |group, below: &mut HashMap<_, BTreeSet<_>>| -> BTreeSet<_> {
        if let Some(known) = below.get(&group) {
            return known.clone();
        }
        let mut seen = BTreeSet::from([group]);
        let mut stack = vec![group];
        while let Some(next) = stack.pop() {
            for &input in group_inputs.get(&next).into_iter().flatten() {
                if seen.insert(input) {
                    stack.push(input);
                }
            }
        }
        below.insert(group, seen.clone());
        seen
    };
    // The most one execution of each memo group can add to a least work:
    // its own share and every positive edge share into it, at the largest
    // retention.
    let mut most_of_group: HashMap<_, f64> = HashMap::new();
    for index in (0..count).filter(|&index| kept[index] && !at_ingestion(inputs, index)) {
        let positive_shares: f64 = (0..selectable.input_count(index))
            .map(|input| {
                selectable
                    .edges_into(index, input)
                    .iter()
                    .filter_map(|&position| inputs.query_ms.per_edge[position])
                    .fold(0.0, f64::max)
            })
            .sum();
        let most = share_at_largest_retention_or_zero(inputs.query_ms, index)
            .max(share_or_zero(inputs.query_ms, index))
            .max(0.0)
            + positive_shares;
        let entry = most_of_group
            .entry(memo_group_of(inputs, index))
            .or_insert(0.0);
        *entry = entry.max(most);
    }
    // Per execution, the most its inputs' least works may count twice: each
    // memo group below more than one input counts once per extra input, at
    // most what one of its executions adds. Their sub-DAGs' union has at
    // least the sum of their least works less this.
    let counted_twice: Vec<f64> = (0..count)
        .map(|index| {
            let mut inputs_reaching: HashMap<_, usize> = HashMap::new();
            for input in 0..selectable.input_count(index) {
                let mut reached = BTreeSet::new();
                for &position in selectable.edges_into(index, input) {
                    if at_ingestion(inputs, selectable.edges()[position].producer) {
                        continue;
                    }
                    reached.extend(descendants(
                        memo_group_of(inputs, selectable.edges()[position].producer),
                        &mut below,
                    ));
                }
                for group in reached {
                    *inputs_reaching.entry(group).or_default() += 1;
                }
            }
            inputs_reaching
                .iter()
                .filter(|(_, &reaching)| reaching > 1)
                .map(|(group, &reaching)| {
                    (reaching - 1) as f64 * most_of_group.get(group).copied().unwrap_or(0.0)
                })
                .sum()
        })
        .collect();
    let mut work = vec![0.0_f64; count];
    let mut fresh = vec![lateness; count];
    for _ in 0..count.max(1) {
        let mut changed = false;
        for index in 0..count {
            if !kept[index] {
                continue;
            }
            let (new_work, new_fresh) = if at_ingestion(inputs, index) {
                // Its refresh with each producer's least share (a share
                // may take some of its baseline back).
                let shares: f64 = (0..selectable.input_count(index))
                    .map(|input| {
                        selectable
                            .edges_into(index, input)
                            .iter()
                            .filter_map(|&position| inputs.refresh_ms.per_edge[position])
                            .fold(0.0, f64::min)
                    })
                    .sum();
                let refresh = (share_or_zero(inputs.refresh_ms, index) + shares).max(0.0);
                (0.0, lateness + refresh + wait_per_run)
            } else {
                // Its own work: its baseline plus, per input, the least
                // share any producer leaves it (a negative share included:
                // a consumer's baseline is costed over its dearest
                // producer, and the share takes that back). Its inputs'
                // least works add up when their sub-DAGs cannot share an
                // execution; when they can (an in-query diamond, e.g. two
                // quantiles read off one state), only the largest counts:
                // a sum would count a shared execution twice and overstate
                // the bound.
                let mut least_work = share_or_zero(inputs.query_ms, index);
                let mut sum_of_inputs = 0.0_f64;
                let mut largest_input = 0.0_f64;
                let mut least_fresh = lateness;
                for input in 0..selectable.input_count(index) {
                    let choices: Vec<(f64, f64, f64)> = selectable
                        .edges_into(index, input)
                        .iter()
                        .filter(|&&position| kept[selectable.edges()[position].producer])
                        .map(|&position| {
                            let producer = selectable.edges()[position].producer;
                            let edge = inputs.query_ms.per_edge[position].unwrap_or(0.0);
                            (edge, work[producer], fresh[producer])
                        })
                        .collect();
                    if choices.is_empty() {
                        continue;
                    }
                    let least = |pick: fn(&(f64, f64, f64)) -> f64| {
                        choices.iter().map(pick).fold(f64::INFINITY, f64::min)
                    };
                    least_work += least(|choice| choice.0);
                    sum_of_inputs += least(|choice| choice.1);
                    largest_input = largest_input.max(least(|choice| choice.1));
                    least_fresh = least_fresh.max(least(|choice| choice.2));
                }
                // The union of the inputs' sub-DAGs: at least their least
                // works' sum less what may be counted twice, and at least
                // the largest.
                least_work += largest_input.max(sum_of_inputs - counted_twice[index]);
                (least_work, least_fresh)
            };
            let new_work = new_work.max(0.0);
            if new_work > work[index] + 1e-12 || new_fresh > fresh[index] + 1e-12 {
                work[index] = work[index].max(new_work);
                fresh[index] = fresh[index].max(new_fresh);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    work.iter()
        .zip(&fresh)
        .map(|(work, fresh)| work + fresh)
        .collect()
}

/// Per execution, whether it is in no optimal plan given a feasible plan
/// of weighted latency `upper_bound` (T8): any plan in which a query $q$
/// reads it has $\sum_{q'} w_{q'} L_{q'} \ge w_q (\ell^{min}_e + r_q) +
/// \sum_{q' \ne q} w_{q'} L^{min}_{q'}$ ($\ell^{min}_e$ its least latency,
/// $r_q$ the fixed cost per request, $L^{min}_{q'}$ the least latency of
/// any answer of $q'$), and an execution is selected only when some query
/// reads it ((F): every selected execution is used, down from an output).
/// When that bound is over `upper_bound` for every query that may read it,
/// every plan selecting it is worse than the feasible one: it is in no
/// optimal plan, and leaving it out keeps every optimum.
pub(super) fn dearer_than_a_feasible_plan(
    inputs: &ClusterInputs<'_>,
    shapes: &[QueryShape],
    kept: &[bool],
    upper_bound: f64,
) -> Vec<bool> {
    let least = least_latency(inputs, kept);
    let least_of_query: Vec<f64> = shapes
        .iter()
        .map(|shape| {
            shape
                .answering
                .iter()
                .filter(|&&index| kept[index])
                .map(|&index| least[index])
                .fold(f64::INFINITY, f64::min)
                .min(f64::MAX)
                + shape.request_ms
        })
        .collect();
    let everyone: f64 = shapes
        .iter()
        .zip(&least_of_query)
        .map(|(shape, least)| shape.weight * least)
        .sum();
    let limit = upper_bound * (1.0 + 1e-9) + 1e-9;
    (0..least.len())
        .map(|index| {
            let readers: Vec<usize> = (0..shapes.len())
                .filter(|&q| shapes[q].reach.contains(&index))
                .collect();
            let over_alone = over_a_limit_alone(inputs, index);
            kept[index]
                && (over_alone
                    || !readers.is_empty()
                        && readers.iter().all(|&q| {
                            let shape = &shapes[q];
                            everyone - shape.weight * least_of_query[q]
                                + shape.weight * (least[index] + shape.request_ms)
                                > limit
                        }))
        })
        .collect()
}

/// Whether execution `index` alone is over the cluster's memory or disk,
/// with every producer share at its most favourable: in no plan.
pub(super) fn over_a_limit_alone(inputs: &ClusterInputs<'_>, index: usize) -> bool {
    let selectable = inputs.selection.selectable;
    let least = |costs: &ExecutionCosts| {
        share_or_zero(costs, index)
            + (0..selectable.input_count(index))
                .map(|input| {
                    selectable
                        .edges_into(index, input)
                        .iter()
                        .filter_map(|&position| costs.per_edge[position])
                        .fold(0.0, f64::min)
                })
                .sum::<f64>()
    };
    least(inputs.memory) > inputs.limits.memory_bytes as f64 * (1.0 + 1e-9)
        || least(inputs.disk) > inputs.limits.disk_bytes as f64 * (1.0 + 1e-9)
}
