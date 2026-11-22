//! Reads along each path of a query's plan: the plan unfolded into a
//! tree of occurrences, each read once in a plan, whose flows bound the
//! query's reads from below (section 2.5, "Reads along each path").

use super::*;

/// The most occurrences [`unfold_paths`] builds per query. An occurrence
/// is expanded (all its children built at once) only while the tree stays
/// within this many; one left unexpanded keeps its own rows, and the
/// per-query rows still bound what lies below it (exact either way).
pub(super) const MOST_OCCURRENCES_PER_QUERY: usize = 4_096;

/// One edge out of an expanded occurrence: into consumer `consumer` on its
/// input `input`, from `producer`, edge `position`; `child` is the
/// occurrence reading `producer` through it, `None` when the producer's
/// memo group is already on the path (a cycle).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct OccurrenceEdge {
    pub(super) consumer: usize,
    pub(super) input: usize,
    pub(super) position: usize,
    pub(super) producer: usize,
    pub(super) child: Option<usize>,
}

/// One occurrence of a query's plan unfolded along its paths: the
/// executions it may read, and, when expanded, the edges out of them.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Occurrence<G> {
    pub(super) executions: BTreeSet<usize>,
    /// Whether it is an output reference's (its reads are the $z$).
    pub(super) root: bool,
    /// The memo groups from the root down to it, its own included.
    pub(super) path: Vec<G>,
    /// `None` when left unexpanded (a leaf, or past the cap).
    pub(super) edges: Option<Vec<OccurrenceEdge>>,
}

/// A query's plan unfolded into a tree of occurrences: one per output
/// reference (`roots`), and below an occurrence, one per (input, producer
/// memo group) of its executions (`inputs_of(c)`: per input, the
/// (position, producer) pairs). Executions `expands` rejects (ingestion
/// time) are leaves; a producer group already on the path gets no child
/// (a cycle); an occurrence is expanded only if all its children fit
/// within `cap` occurrences.
pub(super) fn unfold_paths<G: Ord + Copy>(
    roots: Vec<BTreeSet<usize>>,
    inputs_of: impl Fn(usize) -> Vec<Vec<(usize, usize)>>,
    group_of: impl Fn(usize) -> G,
    expands: impl Fn(usize) -> bool,
    cap: usize,
) -> Vec<Occurrence<G>> {
    let mut occurrences: Vec<Occurrence<G>> = roots
        .into_iter()
        .take(cap)
        .map(|executions| {
            let mut path: Vec<G> = executions.iter().map(|&c| group_of(c)).collect();
            path.sort_unstable();
            path.dedup();
            Occurrence {
                executions,
                root: true,
                path,
                edges: None,
            }
        })
        .collect();
    let mut next = 0;
    while next < occurrences.len() {
        let occurrence = next;
        next += 1;
        let mut edges = Vec::new();
        let mut children: BTreeMap<(usize, G), BTreeSet<usize>> = BTreeMap::new();
        for &consumer in occurrences[occurrence]
            .executions
            .iter()
            .filter(|&&c| expands(c))
        {
            for (input, producers) in inputs_of(consumer).into_iter().enumerate() {
                for (position, producer) in producers {
                    let group = group_of(producer);
                    let on_path = occurrences[occurrence].path.contains(&group);
                    if !on_path {
                        children.entry((input, group)).or_default().insert(producer);
                    }
                    edges.push((
                        consumer,
                        input,
                        position,
                        producer,
                        (!on_path).then_some(group),
                    ));
                }
            }
        }
        if edges.is_empty() || occurrences.len() + children.len() > cap {
            continue;
        }
        let first_child = occurrences.len();
        let child_of: BTreeMap<(usize, G), usize> = children
            .keys()
            .enumerate()
            .map(|(offset, &key)| (key, first_child + offset))
            .collect();
        let path = occurrences[occurrence].path.clone();
        for ((_, group), executions) in children {
            let mut below = path.clone();
            below.push(group);
            occurrences.push(Occurrence {
                executions,
                root: false,
                path: below,
                edges: None,
            });
        }
        occurrences[occurrence].edges = Some(
            edges
                .into_iter()
                .map(
                    |(consumer, input, position, producer, group)| OccurrenceEdge {
                        consumer,
                        input,
                        position,
                        producer,
                        child: group.map(|group| child_of[&(input, group)]),
                    },
                )
                .collect(),
        );
    }
    occurrences
}

/// What [`path_reads`]'s freshness rows read.
pub(super) struct PathReadRows<'a> {
    pub(super) groups: &'a Groups,
    pub(super) freshness: &'a [Expression],
    pub(super) big_m: &'a [f64],
    pub(super) fresh: Variable,
    pub(super) lateness: f64,
    pub(super) wait_per_run: f64,
}

/// Reads along each path of query `shape`'s plan. The per-query reads
/// $a_{q,e}$ are bounded below by the largest edge read into $e$ only, so
/// that a producer two branches of the query share (a diamond: `avg` as a
/// sum over a count off one state) counts once; in the LP relaxation a
/// read then splits over alternative consumers and halves at every layer
/// where they meet a shared producer. Here the query's plan is unfolded
/// ([`unfold_paths`]); along one path a memo group is read at most once,
/// so in a plan an occurrence reads one execution and one edge per input
/// of it: per expanded occurrence $o$, consumer $c$ and input $i$, the
/// edge reads are a flow, $\sum_u s_{o,c\to u} = r_{o,c}$ with $s \le y$;
/// a child's reads are $r_{o',u} = \sum s_{o,c\to u}$, each bounding
/// $a_{q,u}$ from below; and the freshness big-M row is carried on an
/// occurrence's sums per refresh group. Every row holds in the plan's own
/// reads, so the optimum is kept.
pub(super) fn path_reads(
    inputs: &ClusterInputs<'_>,
    shape: &QueryShape,
    reads: &BTreeMap<usize, Variable>,
    variables: &SelectionVariables,
    freshness_rows: PathReadRows<'_>,
    problem: &mut ProblemVariables,
    rows: &mut Vec<Constraint>,
) {
    let selectable = inputs.selection.selectable;
    let PathReadRows {
        groups,
        freshness,
        big_m,
        fresh,
        lateness,
        wait_per_run,
    } = freshness_rows;
    let roots: Vec<BTreeSet<usize>> = selectable
        .answering()
        .iter()
        .filter(|(query, _)| *query == shape.query)
        .map(|(_, answering)| {
            answering
                .iter()
                .copied()
                .filter(|index| reads.contains_key(index))
                .collect()
        })
        .collect();
    let inputs_of = |consumer: usize| -> Vec<Vec<(usize, usize)>> {
        (0..selectable.input_count(consumer))
            .map(|input| {
                selectable
                    .edges_into(consumer, input)
                    .iter()
                    .map(|&position| (position, selectable.edges()[position].producer))
                    .filter(|&(position, producer)| {
                        variables.y.contains_key(&position) && reads.contains_key(&producer)
                    })
                    .collect()
            })
            .collect()
    };
    let occurrences = unfold_paths(
        roots,
        inputs_of,
        |index| memo_group_of(inputs, index),
        |index| !at_ingestion(inputs, index),
        MOST_OCCURRENCES_PER_QUERY,
    );
    // Each occurrence's reads: a root's are its answering z, a child's the
    // edge reads into it.
    let mut read_of: Vec<BTreeMap<usize, Expression>> = vec![BTreeMap::new(); occurrences.len()];
    for (occurrence, root) in occurrences.iter().enumerate().filter(|(_, o)| o.root) {
        for &index in &root.executions {
            if let Some(&z) = variables.z.get(&(shape.query, index)) {
                read_of[occurrence].insert(index, Expression::from(z));
            }
        }
    }
    // In order: a child comes after its parent, whose reads are complete.
    for (own, occurrence) in occurrences.iter().enumerate() {
        let Some(edges) = &occurrence.edges else {
            continue;
        };
        let mut flows: BTreeMap<(usize, usize), Expression> = BTreeMap::new();
        for edge in edges {
            let edge_read = problem.add(variable().min(0).max(1));
            rows.push(constraint!(edge_read <= variables.y[&edge.position]));
            *flows.entry((edge.consumer, edge.input)).or_default() += edge_read;
            if let Some(child) = edge.child {
                *read_of[child].entry(edge.producer).or_default() += edge_read;
            }
        }
        for ((consumer, _), flow) in flows {
            let read = read_of[own].get(&consumer).cloned().unwrap_or_default();
            rows.push(constraint!(flow == read));
        }
    }
    for (occurrence, read) in occurrences.iter().zip(&read_of) {
        // Below the root, each read bounds the query's read from below.
        if !occurrence.root {
            for (index, read) in read {
                rows.push(constraint!(reads[index] >= read.clone()));
            }
        }
        // Freshness over the occurrence's executions in one refresh group.
        let mut by_refresh_group: BTreeMap<usize, (Expression, Expression, usize)> =
            BTreeMap::new();
        for (&index, read) in read {
            if let (Some(&group), true) =
                (groups.of_execution.get(&index), at_ingestion(inputs, index))
            {
                let own = share_at_largest_retention_or_zero(inputs.refresh_ms, index).max(0.0)
                    + wait_per_run;
                let entry = by_refresh_group.entry(group).or_default();
                entry.0 += read.clone();
                entry.1 += read.clone() * own;
                entry.2 += 1;
            }
        }
        for (group, (read, own, executions)) in by_refresh_group {
            if executions > 1 {
                rows.push(constraint!(
                    fresh >= freshness[group].clone() - big_m[group] + big_m[group] * read
                ));
                rows.push(constraint!(fresh >= lateness + own));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A toy plan for [`unfold_paths`]: per execution, per input, the
    /// (edge position, producer) pairs, and each execution's memo group.
    fn unfold(
        inputs: &[Vec<Vec<(usize, usize)>>],
        groups: &[u32],
        roots: Vec<BTreeSet<usize>>,
        cap: usize,
    ) -> Vec<Occurrence<u32>> {
        unfold_paths(roots, |c| inputs[c].clone(), |c| groups[c], |_| true, cap)
    }

    /// A cycle of memo groups: execution 0 (group 0) reads 1 (group 1),
    /// which reads 2 (group 0 again). Group 0 is on the path, so the edge
    /// into 2 has no child occurrence: the tree stops instead of looping,
    /// and the edge is still listed (its read stays in the flow).
    #[test]
    fn a_cycle_is_not_unfolded_again() {
        let inputs = vec![
            vec![vec![(10, 1)]],
            vec![vec![(11, 2)]],
            vec![vec![(12, 1)]],
        ];
        let occurrences = unfold(&inputs, &[0, 1, 0], vec![BTreeSet::from([0])], 100);
        assert_eq!(occurrences.len(), 2, "{occurrences:?}");
        let below = occurrences[1].edges.as_ref().expect("expanded");
        assert_eq!(below.len(), 1);
        assert_eq!((below[0].position, below[0].child), (11, None));
        assert_eq!(occurrences[1].path, vec![0, 1]);
    }

    /// A diamond: 0 reads groups 1 and 2 on its two inputs, both reading
    /// the shared group 3 (execution 3 or its alternative 4): 3 is unfolded
    /// once per path (two occurrences), each reading 3 or 4.
    #[test]
    fn a_diamond_unfolds_once_per_path() {
        let inputs = vec![
            vec![vec![(10, 1)], vec![(11, 2)]],
            vec![vec![(12, 3), (13, 4)]],
            vec![vec![(14, 3), (15, 4)]],
            vec![],
            vec![],
        ];
        let occurrences = unfold(&inputs, &[0, 1, 2, 3, 3], vec![BTreeSet::from([0])], 100);
        let shared: Vec<_> = occurrences
            .iter()
            .filter(|o| o.executions == BTreeSet::from([3, 4]))
            .collect();
        assert_eq!(shared.len(), 2, "{occurrences:?}");
    }

    /// The cap: a chain of diamonds doubles the occurrences per level; with
    /// a cap of 10 the tree stays within 10, and every expanded occurrence
    /// has all its children (so its flow rows are complete).
    #[test]
    fn the_cap_bounds_the_tree_and_expands_whole_occurrences() {
        // Levels of two executions each, both reading both of the next.
        let levels = 6;
        let mut inputs = Vec::new();
        let mut groups = Vec::new();
        for level in 0..levels {
            for _ in 0..2 {
                let next = 2 * (level + 1);
                inputs.push(match level + 1 < levels {
                    true => vec![vec![(next, next)], vec![(next + 1, next + 1)]],
                    false => vec![],
                });
                groups.push(level as u32 * 2 + (inputs.len() as u32 - 1) % 2);
            }
        }
        let occurrences = unfold(&inputs, &groups, vec![BTreeSet::from([0])], 10);
        assert!(occurrences.len() <= 10, "{}", occurrences.len());
        let unbounded = unfold(&inputs, &groups, vec![BTreeSet::from([0])], usize::MAX);
        assert!(unbounded.len() > 10);
        for occurrence in &occurrences {
            if let Some(edges) = &occurrence.edges {
                for &consumer in &occurrence.executions {
                    let expected: usize = inputs[consumer].iter().map(Vec::len).sum();
                    let listed = edges.iter().filter(|e| e.consumer == consumer).count();
                    assert_eq!(listed, expected);
                }
                assert!(edges
                    .iter()
                    .all(|e| e.child.is_some_and(|c| c < occurrences.len())));
            }
        }
    }
}
