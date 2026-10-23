//! Independent clusters of the memo: queries whose plans share no cost
//! under cost model 1, searched as separate workloads by [`crate::search::milp::model`]
//! and combined into one plan.

use asap_physical_asap_optimization::{optimize_physical_asap_assigned, PhysicalCandidate};
use asap_types::ir::QueryRoot;
use asap_types::physical::Materialization;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::search::memo::{CouplingSite, GroupId, Memo};
use crate::{CostModel, PlanSelector, PlanningModels, QuerySearchReport, Selection};

/// The workload's queries (`roots`, in workload order) split into
/// clusters no plan's cost links, each in workload order, ordered by their
/// first query. Two queries are in one cluster when:
///
/// - they reach a common group;
/// - a candidate of a group one reaches reads, requires or rolls up from a
///   group the other reaches (children, exact children, required sites,
///   a rollup's finer group);
/// - candidates of groups each reaches carry one coupling site (shared
///   build size, pane chain, scan labels, raw horizon);
/// - they read one source while raw data is kept and priced on disk: the
///   source's raw data is charged once, for its longest query-time scan.
///
/// Every query is in one cluster when the deployment calibrated operation
/// costs over the whole workload: its prices are not a sum over clusters.
pub(crate) fn split(
    memo: &Memo,
    roots: &[QueryRoot],
    models: &PlanningModels<'_>,
) -> Vec<Vec<usize>> {
    let count = roots.len();
    if !models
        .plan_selection
        .measured_operation_costs
        .operators
        .is_empty()
    {
        return vec![(0..count).collect()];
    }
    let mut parent: Vec<usize> = (0..count).collect();
    fn find(parent: &mut [usize], i: usize) -> usize {
        let mut root = i;
        while parent[root] != root {
            root = parent[root];
        }
        let mut at = i;
        while parent[at] != root {
            let next = parent[at];
            parent[at] = root;
            at = next;
        }
        root
    }
    fn union(parent: &mut [usize], a: usize, b: usize) {
        let (ra, rb) = (find(parent, a), find(parent, b));
        parent[ra.max(rb)] = ra.min(rb);
    }
    let first_query = |g: GroupId| memo.groups[g.0 as usize].queries.first().copied();
    let mut site_query: std::collections::HashMap<CouplingSite, usize> =
        std::collections::HashMap::new();
    for (g, group) in memo.groups.iter().enumerate() {
        for pair in group.queries.windows(2) {
            union(&mut parent, pair[0], pair[1]);
        }
        let Some(own) = group.queries.first().copied() else {
            continue;
        };
        for candidate in &memo.candidates[g] {
            let linked = candidate
                .implements
                .iter()
                .chain(&candidate.children)
                .chain(&candidate.requires_exact_children)
                .chain(candidate.requires_sites.iter().map(|(group, _)| group))
                .chain(candidate.rollup_of.iter().map(|r| &r.finer_group));
            for &other in linked {
                if let Some(q) = first_query(other) {
                    union(&mut parent, own, q);
                }
            }
            for site in candidate
                .coupling_sites
                .iter()
                .chain(candidate.requires_sites.iter().map(|(_, site)| site))
            {
                let q = *site_query.entry(*site).or_insert(own);
                union(&mut parent, own, q);
            }
        }
    }
    // A query no group reaches (a scalar) shares nothing: it joins the
    // first cluster rather than being a workload of its own.
    let mut reached = vec![false; count];
    for group in &memo.groups {
        for &q in &group.queries {
            reached[q] = true;
        }
    }
    for (q, _) in reached.iter().enumerate().skip(1).filter(|(_, r)| !**r) {
        union(&mut parent, 0, q);
    }
    if count > 0 && !reached[0] {
        for q in 1..count {
            union(&mut parent, 0, q);
        }
    }
    let disk_priced = match &models.plan_selection.cost_model {
        CostModel::UsageOverTime(prices) => prices.cost_per_disk_byte_second != 0.0,
        _ => true,
    };
    if disk_priced && models.deployment_capabilities.raw_data_retained {
        for queries in crate::search::legacy::components::source_components(roots) {
            for pair in queries.windows(2) {
                union(&mut parent, pair[0], pair[1]);
            }
        }
    }
    let mut clusters: Vec<Vec<usize>> = Vec::new();
    let mut cluster_of_root: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::new();
    for q in 0..count {
        let root = find(&mut parent, q);
        let c = *cluster_of_root.entry(root).or_insert_with(|| {
            clusters.push(Vec::new());
            clusters.len() - 1
        });
        clusters[c].push(q);
    }
    clusters
}

/// The workload's plan from its `clusters`' plans (`found[i]` the
/// selection, physical candidate, logical roots and report of
/// `clusters[i]`): the clusters' roots in workload order, each node
/// materialized as its cluster's plan materializes it, checked and costed
/// by Stage 3 over the whole workload. `None` when that cost is not the
/// sum of the clusters' costs.
pub(crate) fn combine(
    found: Vec<(
        Selection,
        PhysicalCandidate,
        Vec<QueryRoot>,
        crate::MilpClusterReport,
    )>,
    clusters: &[Vec<usize>],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Option<(Selection, PhysicalCandidate, Vec<QueryRoot>)> {
    let count = entries.len();
    let mut cluster_of = vec![0; count];
    let mut combined: Vec<Option<QueryRoot>> = vec![None; count];
    for (c, (queries, (_, _, roots, _))) in clusters.iter().zip(&found).enumerate() {
        for (&q, root) in queries.iter().zip(roots) {
            combined[q] = Some(root.clone());
            cluster_of[q] = c;
        }
    }
    let combined: Vec<QueryRoot> = combined.into_iter().collect::<Option<_>>()?;
    let mut assignments: Vec<std::vec::IntoIter<Materialization>> = found
        .iter()
        .map(|(_, selected, _, _)| selected.assignment.clone().into_iter())
        .collect();
    let mut choose = |readers: &[usize]| assignments[cluster_of[*readers.first()?]].next();
    let mut candidate = optimize_physical_asap_assigned(
        "L1",
        &combined,
        entries,
        data,
        models.deployment_capabilities,
        &mut choose,
    )
    .ok()??;
    if assignments.iter_mut().any(|rest| rest.next().is_some()) {
        return None;
    }
    candidate.id = found
        .iter()
        .map(|(s, _, _, _)| s.selected.as_str())
        .collect::<Vec<_>>()
        .join("+");
    // Cost models 2 and 3 do not run the MILP yet: their peak cores and
    // cluster capacity are not sums over clusters, and would be checked
    // here on the combined plan.
    let mut selector = PlanSelector::new(entries, data, models, false).ok()?;
    let candidate = selector.sized(&candidate)?;
    selector.offer(&candidate).ok()?;
    let (mut selection, selected) = selector.finish().ok()?;
    let price = selection.costs[&selection.selected].cost_per_second?;
    let costs: Vec<f64> = found
        .iter()
        .map(|(_, _, _, report)| report.incumbent_cost)
        .collect::<Option<_>>()?;
    let total: f64 = costs.iter().sum();
    if (price - total).abs() > 1e-9 * total.abs() {
        return None;
    }
    let mut queries = vec![QuerySearchReport::default(); count];
    let mut rejected = Vec::new();
    for (cluster, (s, _, _, _)) in clusters.iter().zip(&found) {
        for (&q, report) in cluster.iter().zip(&s.search.queries) {
            queries[q] = report.clone();
        }
        rejected.extend(
            s.rejected
                .iter()
                .cloned()
                .map(|r| crate::search::legacy::components::renumber(r, cluster)),
        );
    }
    selection.rejected = rejected;
    selection.search.queries = queries;
    selection.search.candidates_checked += found
        .iter()
        .map(|(s, _, _, _)| s.search.candidates_checked)
        .sum::<u64>();
    selection.search.raw_scan_assignments_skipped = found
        .iter()
        .map(|(s, _, _, _)| s.search.raw_scan_assignments_skipped)
        .sum();
    selection.search.incumbent_cost = Some(price);
    selection.search.milp = Some(milp_report(&found));
    let roots = combined;
    Some((selection, selected, roots))
}

/// The workload's MILP report from its clusters': bounds, rounds, model
/// sizes and solver time summed, the gap from the summed bound and cost.
fn milp_report(
    found: &[(
        Selection,
        PhysicalCandidate,
        Vec<QueryRoot>,
        crate::MilpClusterReport,
    )],
) -> crate::MilpReport {
    let reports: Vec<&crate::MilpReport> = found
        .iter()
        .filter_map(|(s, _, _, _)| s.search.milp.as_ref())
        .collect();
    let sum = |value: fn(&crate::MilpReport) -> Option<f64>| -> Option<f64> {
        reports.iter().map(|r| value(r)).sum()
    };
    let lower_bound = sum(|r| r.lower_bound);
    let incumbent_cost: Option<f64> = found.iter().map(|(_, _, _, c)| c.incumbent_cost).sum();
    crate::MilpReport {
        lower_bound,
        first_lower_bound: sum(|r| r.first_lower_bound),
        gap: crate::search::milp::model::gap(incumbent_cost, lower_bound),
        rounds: reports.iter().map(|r| r.rounds).sum(),
        variable_count: reports.iter().map(|r| r.variable_count).sum(),
        constraint_count: reports.iter().map(|r| r.constraint_count).sum(),
        solver_time_ms: reports.iter().map(|r| r.solver_time_ms).sum(),
        time_limit_reached: reports.iter().any(|r| r.time_limit_reached),
        decode_failures: reports.iter().map(|r| r.decode_failures).sum(),
        clusters: found.iter().map(|(_, _, _, c)| c.clone()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::memo::{groups, CouplingKind, ImplementationCandidate};
    use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::test_support::lower_promql;
    use asap_types::ir::properties::AccuracyTarget;

    /// The memo of `queries` (Pass 1 groups only) and their roots.
    fn memo(queries: &[&str]) -> (Memo, Vec<QueryRoot>) {
        let roots: Vec<QueryRoot> = queries
            .iter()
            .map(|q| QueryRoot::Operator(lower_promql(q, AccuracyTarget::max_error(0.05))))
            .collect();
        let per_computation =
            generate_per_computation_candidates(roots.iter().cloned().enumerate().collect())
                .unwrap();
        (groups::memo_of(&per_computation), roots)
    }

    /// The first candidate of the group whose root is query `q`.
    fn root_candidate(memo: &mut Memo, q: usize) -> &mut ImplementationCandidate {
        let g = memo
            .groups
            .iter()
            .position(|g| g.roots_of.contains(&q))
            .unwrap();
        &mut memo.candidates[g][0]
    }

    /// Queries over two gauges are two clusters, those over one gauge
    /// reaching its scan group; one coupling site (a shared build's size,
    /// a source's raw horizon) carried by candidates of both joins them.
    #[test]
    fn a_shared_group_or_coupling_site_joins_clusters() {
        let models = PlanningModels::builtin();
        let (memo, roots) = memo(&[
            "sum_over_time(m[5m])",
            "max_over_time(n[5m])",
            "quantile_over_time(0.9, m[1m])",
        ]);
        assert_eq!(split(&memo, &roots, &models), [vec![0, 2], vec![1]]);
        for kind in [CouplingKind::SharedBuildSize, CouplingKind::RawHorizon] {
            let mut linked = memo.clone();
            let site = CouplingSite { key: 7, kind };
            root_candidate(&mut linked, 1).coupling_sites.push(site);
            assert_eq!(split(&linked, &roots, &models), [vec![0, 2], vec![1]]);
            root_candidate(&mut linked, 2).coupling_sites.push(site);
            assert_eq!(split(&linked, &roots, &models), [vec![0, 1, 2]], "{kind:?}");
        }
    }

    /// Queries over one source through different scans share no group;
    /// with raw data kept and disk priced, the source's raw data is charged
    /// once for its longest scan, which joins them. Without a disk price,
    /// or without raw data, they stay apart.
    #[test]
    fn a_source_s_charged_raw_data_joins_its_queries() {
        let (memo, roots) = memo(&[
            r#"sum_over_time(m{a="1"}[5m])"#,
            r#"max_over_time(m{a="2"}[10m])"#,
        ]);
        let unpriced = PlanningModels::builtin();
        assert_eq!(split(&memo, &roots, &unpriced), [vec![0], vec![1]]);
        let mut settings = crate::PlanSelectionSettings::default();
        let CostModel::UsageOverTime(prices) = &mut settings.cost_model else {
            unreachable!("cost model 1 by default")
        };
        prices.cost_per_disk_byte_second = 1e-12;
        let priced = PlanningModels::builtin().with_plan_selection(&settings);
        assert_eq!(split(&memo, &roots, &priced), [vec![0, 1]]);
        let no_raw = asap_logical_asap_optimization::DeploymentCapabilities {
            raw_data_retained: false,
            ..Default::default()
        };
        let priced_no_raw = priced.with_deployment_capabilities(&no_raw);
        assert_eq!(split(&memo, &roots, &priced_no_raw), [vec![0], vec![1]]);
    }

    /// A candidate reading another query's group (a rollup or a top-k
    /// reuse) joins the two queries.
    #[test]
    fn a_candidate_reading_another_query_s_group_joins_them() {
        let models = PlanningModels::builtin();
        let (mut memo, roots) = memo(&["sum_over_time(m[5m])", "max_over_time(n[5m])"]);
        let other = memo
            .groups
            .iter()
            .position(|g| g.roots_of.contains(&1))
            .unwrap();
        root_candidate(&mut memo, 0)
            .children
            .push(GroupId(other as u32));
        assert_eq!(split(&memo, &roots, &models), [vec![0, 1]]);
    }
}
