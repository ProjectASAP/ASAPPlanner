//! Couplings through a source's rows: scan widening and the raw-data
//! horizon (joint-planning-search design, "Memo-group search", step 4).
//!
//! - **Scan widening** ([`add_scan_widening`]): the time-series scans of a
//!   metric that project different labels are separate groups (an unkeyed
//!   scan is never one group with another). The scans of the candidates
//!   that read them can be widened to one label set, as
//!   `with_union_scan_labels` does for a whole candidate, so that they are
//!   one scan. Each reader candidate gets a [`CandidateOrigin::WidenedScans`]
//!   twin with the metric's [`CouplingKind::ScanLabels`] site; the label
//!   set is the site's threshold ([`label_thresholds`]), a reader may read
//!   it iff its own labels are a subset ([`may_read_labels`]), and its node
//!   at a threshold is [`widened_at_threshold`]: the labels passed on by the
//!   nodes that pass them (a range or shift, a per-series build and its
//!   reads) and narrowed back by a `Project` above them.
//! - **Raw horizon** ([`add_raw_horizons`]): the raw data a source keeps on
//!   disk is the longest window a query-time scan reads from it. Every
//!   candidate that reads a raw window of a source read by two or more
//!   groups gets the source's [`CouplingKind::RawHorizon`] site; the
//!   horizon is the site's threshold ([`horizon_thresholds`]), and a reader
//!   may read at most it ([`may_read_horizon`]). Whether a read is at query
//!   time is the reader's materialization, which the search knows: a read
//!   built at ingestion keeps no raw data.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use asap_logical_asap_optimization::asap_aware_cse::with_union_scan_labels;
use asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidateError;
use asap_types::ir::operator::Source;
use asap_types::ir::{NonASAPOp, OperatorNode, QueryRoot};

use super::groups::NodeKeys;
use super::{CandidateOrigin, CouplingKind, CouplingSite, GroupId, ImplementationCandidate, Memo};

/// One label set a metric's scans may be widened to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct LabelThreshold {
    /// The column names the widened scan projects.
    pub labels: BTreeSet<String>,
}

/// A candidate's read of a metric's scan: the columns its scan projects.
#[derive(Debug, Clone)]
pub(crate) struct ScanReader {
    /// The metric's [`CouplingSite::key`].
    pub key: u64,
    /// The column names the reader's scan projects.
    pub labels: BTreeSet<String>,
    /// The scan.
    pub scan: Rc<OperatorNode>,
}

/// One horizon a source's raw data may be kept for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct HorizonThreshold {
    /// The longest window read from the raw data, in ms.
    pub horizon_ms: u64,
}

/// A candidate's read of a raw window of a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RawReader {
    /// The source's [`CouplingSite::key`].
    pub key: u64,
    /// The window it reads, in ms.
    pub horizon_ms: u64,
}

/// Scan widening: for each metric whose scans read by candidates of two or
/// more groups project different labels, a
/// [`CandidateOrigin::WidenedScans`] twin of each reader candidate with
/// the metric's [`CouplingKind::ScanLabels`] site. The twin keeps the
/// reader's node and children; [`widened_at_threshold`] gives both at a
/// threshold.
pub(crate) fn add_scan_widening(memo: &mut Memo) {
    // Per metric key, the readers: (group, candidate index, labels).
    let mut readers: HashMap<u64, Vec<(usize, usize, BTreeSet<String>)>> = HashMap::new();
    let mut order = Vec::new();
    for (g, candidates) in memo.candidates.iter().enumerate() {
        if passes_rows_on(&memo.groups[g].node) {
            // Its readers widen it.
            continue;
        }
        for (index, candidate) in candidates.iter().enumerate() {
            if candidate.origin == CandidateOrigin::WidenedScans {
                continue;
            }
            for reader in reads(memo, candidate, scan_reader) {
                let entry = readers.entry(reader.key).or_default();
                if entry.is_empty() {
                    order.push(reader.key);
                }
                if !entry.iter().any(|(eg, ei, _)| (*eg, *ei) == (g, index)) {
                    entry.push((g, index, reader.labels));
                }
            }
        }
    }
    for key in order {
        let members = &readers[&key];
        let groups: HashSet<usize> = members.iter().map(|(g, _, _)| *g).collect();
        let label_sets: HashSet<&BTreeSet<String>> = members.iter().map(|(_, _, l)| l).collect();
        if groups.len() < 2 || label_sets.len() < 2 {
            continue;
        }
        let site = CouplingSite {
            key,
            kind: CouplingKind::ScanLabels,
        };
        for (g, index, _) in members.clone() {
            let mut twin = memo.candidates[g][index].clone();
            twin.origin = CandidateOrigin::WidenedScans;
            twin.coupling_sites.push(site);
            memo.candidates[g].push(twin);
        }
    }
}

/// The reads of a metric's scans by `candidate`'s private nodes (directly,
/// or through ranges and shifts), each once.
pub(crate) fn scan_readers(memo: &Memo, candidate: &ImplementationCandidate) -> Vec<ScanReader> {
    reads(memo, candidate, scan_reader)
}

/// The label sets the scans of `site` may be widened to: each reader's
/// own, and the union of every reader's, each once, smallest first.
pub(crate) fn label_thresholds(memo: &Memo, site: &CouplingSite) -> Vec<LabelThreshold> {
    let mut thresholds: BTreeSet<Vec<String>> = BTreeSet::new();
    let mut union = BTreeSet::new();
    for reader in site_scan_readers(memo, site) {
        union.extend(reader.labels.iter().cloned());
        thresholds.insert(reader.labels.into_iter().collect());
    }
    thresholds.insert(union.into_iter().collect());
    let mut thresholds: Vec<LabelThreshold> = thresholds
        .into_iter()
        .map(|labels| LabelThreshold {
            labels: labels.into_iter().collect(),
        })
        .collect();
    thresholds.sort_by_key(|t| t.labels.len());
    thresholds
}

/// Whether `reader` may read a scan widened to `threshold`: its labels are
/// a subset.
pub(crate) fn may_read_labels(threshold: &LabelThreshold, reader: &ScanReader) -> bool {
    reader.labels.is_subset(&threshold.labels)
}

/// `candidate` (a twin with `site`) with its scans of the site's metric
/// widened to `threshold`, which each of its reads may read
/// ([`may_read_labels`]): its node rewritten as `with_union_scan_labels`
/// rewrites a candidate, its children the groups the new node reaches.
/// The candidate itself when its scans already project `threshold`;
/// `None` when the widening is not possible (a node that would change
/// arity, a summary state to narrow, a grouped or filtered pane chain over
/// the scan, or another metric's scans that would change too).
pub(crate) fn widened_at_threshold(
    memo: &Memo,
    candidate: &ImplementationCandidate,
    site: &CouplingSite,
    threshold: &LabelThreshold,
) -> Result<Option<ImplementationCandidate>, PerComputationCandidateError> {
    let own: Vec<ScanReader> = scan_readers(memo, candidate)
        .into_iter()
        .filter(|r| r.key == site.key)
        .collect();
    if own.iter().all(|r| r.labels == threshold.labels) {
        return Ok(Some(candidate.clone()));
    }
    if !own.iter().all(|r| may_read_labels(threshold, r)) {
        return Ok(None);
    }
    // The site's scans whose labels the threshold covers: their union is
    // the threshold when it is a union of readers' label sets.
    let mut roots = vec![QueryRoot::Operator(Rc::clone(&candidate.node))];
    let mut covered = BTreeSet::new();
    for reader in site_scan_readers(memo, site) {
        if may_read_labels(threshold, &reader) {
            covered.extend(reader.labels.iter().cloned());
            roots.push(QueryRoot::Operator(Rc::clone(&reader.scan)));
        }
    }
    if covered != threshold.labels {
        return Ok(None);
    }
    let Some(widened) = with_union_scan_labels(&roots)? else {
        return Ok(None);
    };
    let QueryRoot::Operator(node) = &widened[0] else {
        unreachable!("an operator root");
    };
    // Only the site's metric changes.
    let before: HashSet<*const OperatorNode> = OperatorNode::reachable(&candidate.node)
        .iter()
        .map(Rc::as_ptr)
        .collect();
    let metric = metric_of_site(memo, site);
    let other_changed = OperatorNode::reachable(node).iter().any(|n| {
        matches!(n.non_asap(), Some(NonASAPOp::Scan { source, .. })
            if Some(source) != metric.as_ref())
            && !before.contains(&Rc::as_ptr(n))
    });
    if other_changed {
        return Ok(None);
    }
    Ok(Some(ImplementationCandidate {
        implements: candidate.implements.clone(),
        node: Rc::clone(node),
        children: child_groups(&mut Boundary::of(memo, candidate), node),
        origin: CandidateOrigin::WidenedScans,
        coupling_sites: candidate.coupling_sites.clone(),
    }))
}

/// Raw horizon: for each source whose raw windows candidates of two or
/// more groups read, the source's [`CouplingKind::RawHorizon`] site added
/// to each of those candidates.
pub(crate) fn add_raw_horizons(memo: &mut Memo) {
    let mut readers: HashMap<u64, Vec<(usize, usize)>> = HashMap::new();
    let mut order = Vec::new();
    for (g, candidates) in memo.candidates.iter().enumerate() {
        for (index, candidate) in candidates.iter().enumerate() {
            for reader in reads(memo, candidate, raw_reader) {
                let entry = readers.entry(reader.key).or_default();
                if entry.is_empty() {
                    order.push(reader.key);
                }
                if !entry.contains(&(g, index)) {
                    entry.push((g, index));
                }
            }
        }
    }
    for key in order {
        let members = &readers[&key];
        let groups: HashSet<usize> = members.iter().map(|(g, _)| *g).collect();
        if groups.len() < 2 {
            continue;
        }
        let site = CouplingSite {
            key,
            kind: CouplingKind::RawHorizon,
        };
        for &(g, index) in members {
            memo.candidates[g][index].coupling_sites.push(site);
        }
    }
}

/// The raw windows `candidate`'s private nodes read: each range window
/// directly over a scan, read by one of them.
pub(crate) fn raw_readers(memo: &Memo, candidate: &ImplementationCandidate) -> Vec<RawReader> {
    reads(memo, candidate, raw_reader)
}

/// The horizons the raw data of `site` may be kept for: its readers'
/// windows, each once, shortest first.
pub(crate) fn horizon_thresholds(memo: &Memo, site: &CouplingSite) -> Vec<HorizonThreshold> {
    let horizons: BTreeSet<u64> = memo
        .candidates
        .iter()
        .flatten()
        .filter(|c| c.coupling_sites.contains(site))
        .flat_map(|c| raw_readers(memo, c))
        .filter(|r| r.key == site.key)
        .map(|r| r.horizon_ms)
        .collect();
    horizons
        .into_iter()
        .map(|horizon_ms| HorizonThreshold { horizon_ms })
        .collect()
}

/// Whether `reader` may read raw data kept for `threshold`: its window is
/// no longer.
pub(crate) fn may_read_horizon(threshold: &HorizonThreshold, reader: &RawReader) -> bool {
    reader.horizon_ms <= threshold.horizon_ms
}

/// The scan readers of the candidates that carry `site`, one per distinct
/// label set.
fn site_scan_readers(memo: &Memo, site: &CouplingSite) -> Vec<ScanReader> {
    let mut out: Vec<ScanReader> = Vec::new();
    for candidate in memo.candidates.iter().flatten() {
        if !candidate.coupling_sites.contains(site) {
            continue;
        }
        for reader in reads(memo, candidate, scan_reader) {
            if reader.key == site.key && !out.iter().any(|r| r.labels == reader.labels) {
                out.push(reader);
            }
        }
    }
    out
}

/// The metric of a scan-labels site.
fn metric_of_site(memo: &Memo, site: &CouplingSite) -> Option<Source> {
    site_scan_readers(memo, site)
        .first()
        .and_then(|r| match r.scan.non_asap() {
            Some(NonASAPOp::Scan { source, .. }) => Some(source.clone()),
            _ => None,
        })
}

/// What `read` finds in each node a private node of `candidate` reads
/// (its children, private or a child group's node), each node once.
fn reads<T>(
    memo: &Memo,
    candidate: &ImplementationCandidate,
    read: impl Fn(&Rc<OperatorNode>) -> Option<T>,
) -> Vec<T> {
    let mut boundary = Boundary::of(memo, candidate);
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![Rc::clone(&candidate.node)];
    while let Some(node) = stack.pop() {
        if !seen.insert(Rc::as_ptr(&node)) {
            continue;
        }
        for child in node.children() {
            if let Some(found) = read(child) {
                out.push(found);
            }
            if boundary.group(child).is_none() {
                stack.push(Rc::clone(child));
            }
        }
    }
    out
}

/// The scan of a metric `node` reads, directly or through ranges and
/// shifts, without predicates.
fn scan_reader(node: &Rc<OperatorNode>) -> Option<ScanReader> {
    match node.non_asap() {
        Some(NonASAPOp::Scan {
            source: source @ Source::TimeSeries { .. },
            predicates,
            schema,
        }) if predicates.is_empty() => Some(ScanReader {
            key: hash_of(&("ScanLabels", format!("{source:?}"))),
            labels: schema.fields.iter().map(|f| f.name.clone()).collect(),
            scan: Rc::clone(node),
        }),
        Some(NonASAPOp::TimeRange { child, .. } | NonASAPOp::TimeShift { child, .. }) => {
            scan_reader(child)
        }
        _ => None,
    }
}

/// The raw window `node` is, when it is a range window directly over a
/// scan.
fn raw_reader(node: &Rc<OperatorNode>) -> Option<RawReader> {
    let Some(NonASAPOp::TimeRange { range, child, .. }) = node.non_asap() else {
        return None;
    };
    let Some(NonASAPOp::Scan { source, .. }) = child.non_asap() else {
        return None;
    };
    Some(RawReader {
        key: hash_of(&("RawHorizon", format!("{source:?}"))),
        horizon_ms: u64::try_from(range.as_millis()).unwrap_or(u64::MAX),
    })
}

/// Whether `node` passes its input's rows on as a scan's reader sees them:
/// a scan, range or shift, whose readers widen it.
fn passes_rows_on(node: &OperatorNode) -> bool {
    matches!(
        node.non_asap(),
        Some(NonASAPOp::Scan { .. } | NonASAPOp::TimeRange { .. } | NonASAPOp::TimeShift { .. })
    )
}

/// A candidate's child groups by node key ([`NodeKeys`]): its private
/// nodes end at the first node equal to one of them up to what composition
/// derives (schema, guarantee, timing, materialization).
struct Boundary {
    keys: NodeKeys,
    children: HashMap<u64, GroupId>,
}

impl Boundary {
    fn of(memo: &Memo, candidate: &ImplementationCandidate) -> Self {
        let mut keys = NodeKeys::default();
        let children = candidate
            .children
            .iter()
            .map(|&child| (keys.key(&memo.groups[child.0 as usize].node), child))
            .collect();
        Self { keys, children }
    }

    /// The child group `node` is, if any.
    fn group(&mut self, node: &Rc<OperatorNode>) -> Option<GroupId> {
        let key = self.keys.key(node);
        self.children.get(&key).copied()
    }
}

/// The child groups `node` reaches below its root without passing through
/// another child group's node, in discovery order.
fn child_groups(boundary: &mut Boundary, node: &Rc<OperatorNode>) -> Vec<GroupId> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![Rc::clone(node)];
    while let Some(node) = stack.pop() {
        for child in node.children() {
            if !seen.insert(Rc::as_ptr(child)) {
                continue;
            }
            match boundary.group(child) {
                Some(g) => {
                    if !out.contains(&g) {
                        out.push(g);
                    }
                }
                None => stack.push(Rc::clone(child)),
            }
        }
    }
    out
}

fn hash_of<T: Hash>(value: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pass::subexpr::groups::memo_of;
    use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::test_support::lower_promql;
    use asap_types::ir::properties::AccuracyTarget;

    /// The memo of exact PromQL `queries`.
    fn memo(queries: &[&str]) -> Memo {
        let roots = queries
            .iter()
            .map(|q| QueryRoot::Operator(lower_promql(q, AccuracyTarget::exact())))
            .enumerate()
            .collect();
        memo_of(&generate_per_computation_candidates(roots).unwrap())
    }

    /// The sites of `kind` the candidates of `memo` carry.
    fn sites(memo: &Memo, kind: CouplingKind) -> Vec<CouplingSite> {
        let sites: BTreeSet<CouplingSite> = memo
            .candidates
            .iter()
            .flatten()
            .flat_map(|c| c.coupling_sites.iter().copied())
            .filter(|s| s.kind == kind)
            .collect();
        sites.into_iter().collect()
    }

    /// The candidates of `memo` that carry `site`.
    fn carrying<'a>(memo: &'a Memo, site: &CouplingSite) -> Vec<&'a ImplementationCandidate> {
        memo.candidates
            .iter()
            .flatten()
            .filter(|c| c.coupling_sites.contains(site))
            .collect()
    }

    fn labels(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    /// The column names of the metric scans `node` reaches.
    fn scan_labels(node: &Rc<OperatorNode>) -> Vec<BTreeSet<String>> {
        OperatorNode::reachable(node)
            .iter()
            .filter_map(|n| match n.non_asap() {
                Some(NonASAPOp::Scan { schema, .. }) => {
                    Some(schema.fields.iter().map(|f| f.name.clone()).collect())
                }
                _ => None,
            })
            .collect()
    }

    /// Per-job and per-instance rates of one metric: its two scans project
    /// different labels. One scan-labels site, read by each rate's twin;
    /// its thresholds are each reader's labels and their union, which
    /// admits both. Widened to the union, each rate reads a scan of every
    /// label, through a `Project` back to its own, and the scan and window
    /// groups it read become its own nodes.
    #[test]
    fn scans_of_one_metric_with_different_labels_share_a_widened_scan() {
        let mut memo = memo(&[
            "sum by (job) (rate(m[5m]))",
            "sum by (instance) (rate(m[5m]))",
        ]);
        add_scan_widening(&mut memo);
        let sites = sites(&memo, CouplingKind::ScanLabels);
        assert_eq!(sites.len(), 1);
        let twins = carrying(&memo, &sites[0]);
        assert_eq!(twins.len(), 2);
        assert!(twins
            .iter()
            .all(|c| c.origin == CandidateOrigin::WidenedScans));
        let thresholds = label_thresholds(&memo, &sites[0]);
        let union = labels(&["ts", "value", "job", "instance"]);
        assert_eq!(
            thresholds
                .iter()
                .map(|t| t.labels.clone())
                .collect::<Vec<_>>(),
            vec![
                labels(&["ts", "value", "instance"]),
                labels(&["ts", "value", "job"]),
                union.clone(),
            ]
        );
        let widest = &thresholds[2];
        for twin in &twins {
            let readers = scan_readers(&memo, twin);
            assert!(readers.iter().all(|r| may_read_labels(widest, r)));
            // Its own labels admit only itself.
            let own = LabelThreshold {
                labels: readers[0].labels.clone(),
            };
            let other = twins
                .iter()
                .find(|c| !Rc::ptr_eq(&c.node, &twin.node))
                .unwrap();
            assert!(!scan_readers(&memo, other)
                .iter()
                .all(|r| may_read_labels(&own, r)));
            let widened = widened_at_threshold(&memo, twin, &sites[0], widest)
                .unwrap()
                .expect("a rate over a range widens");
            assert_eq!(scan_labels(&widened.node), vec![union.clone()]);
            assert_eq!(widened.node.schema.fields, twin.node.schema.fields);
            assert!(widened.children.is_empty());
        }
    }

    /// Scans of two metrics with different labels: no site.
    #[test]
    fn scans_of_different_metrics_are_not_widened() {
        let mut memo = memo(&[
            "sum by (job) (rate(m[5m]))",
            "sum by (instance) (rate(x[5m]))",
        ]);
        add_scan_widening(&mut memo);
        assert!(sites(&memo, CouplingKind::ScanLabels).is_empty());
    }

    /// One-minute and five-minute windows of one metric read raw data: one
    /// raw-horizon site with both windows as thresholds; the one-minute
    /// reader may read either, the five-minute one only the longer.
    #[test]
    fn raw_windows_of_one_source_give_one_horizon_site() {
        let mut memo = memo(&["sum_over_time(m[1m])", "sum_over_time(m[5m])"]);
        add_raw_horizons(&mut memo);
        let sites = sites(&memo, CouplingKind::RawHorizon);
        assert_eq!(sites.len(), 1);
        let thresholds = horizon_thresholds(&memo, &sites[0]);
        assert_eq!(
            thresholds.iter().map(|t| t.horizon_ms).collect::<Vec<_>>(),
            vec![60_000, 300_000]
        );
        let readers: Vec<RawReader> = carrying(&memo, &sites[0])
            .into_iter()
            .flat_map(|c| raw_readers(&memo, c))
            .collect();
        let short = readers.iter().find(|r| r.horizon_ms == 60_000).unwrap();
        let long = readers.iter().find(|r| r.horizon_ms == 300_000).unwrap();
        assert!(may_read_horizon(&thresholds[0], short));
        assert!(may_read_horizon(&thresholds[1], short));
        assert!(!may_read_horizon(&thresholds[0], long));
        assert!(may_read_horizon(&thresholds[1], long));
    }

    /// A source read by one group only couples nothing.
    #[test]
    fn a_source_read_by_one_group_has_no_horizon_site() {
        let mut memo = memo(&["sum_over_time(m[1m])", "sum_over_time(x[5m])"]);
        add_raw_horizons(&mut memo);
        assert!(sites(&memo, CouplingKind::RawHorizon).is_empty());
    }
}
