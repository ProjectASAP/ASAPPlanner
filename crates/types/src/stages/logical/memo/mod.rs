//! Memo scope: the memo, Stage 0's and Stage 1's one type, and each query
//! over its groups; the operations over the whole memo
//! (concepts doc `memo-groups-properties-requirements.md`, section 3).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

mod hash_consing;
mod invariants;

pub use invariants::{InvariantCheck, InvariantOrigin, InvariantViolation, MemoContents};

use super::candidate::parameters_fit;
use super::group::{
    derive_group_properties, validate_expression_over_input_groups, MemoGroupAsInput,
    MemoGroupError,
};
use crate::accuracy::AccuracyTarget;
use crate::ir::operator::arguments::{JoinKind, Reduction, Source, SourceColumn};
use crate::ir::operator::input::InputOrTemporaryBuild;
use crate::ir::operator::AggIntent;
use crate::ir::schema::{DataType, FieldDataType, Schema};
use crate::ir::QueryRoot;
use crate::ir::{ASAPOp, NonASAPOp, Operator, TimeRangeKind};
use crate::stages::{
    CandidateError, CandidateForMemoGroup, CandidateForMemoGroupId, ColumnLineage,
    LogicalRequirements, MemoGroup, MemoGroupId,
};
use crate::summary_type::SketchStatistic;
use crate::workload::bounds::{declared_max_distinct_per_evaluation, smaller_known, source_name};
use crate::workload::{series_facts, DataArrival, DataWorkload, SourceFacts};
use hash_consing::{
    expression_hash, same_candidate, same_serialized_expression, serialized_expression,
};

/// The memo, Stage 0's and Stage 1's one type: the memo groups, each with
/// its candidates ([`MemoGroup::candidates`]: empty after Stage 0), and each
/// query's root over them.
#[derive(Debug, Clone)]
pub struct Memo {
    /// The groups, by id ([`Self::groups`]); added only through
    /// [`Self::find_or_add_group`].
    groups: Vec<MemoGroup>,
    /// The queries, by index in the workload ([`Self::queries`]); added only
    /// through [`Self::add_query`].
    queries: Vec<MemoQuery>,
    /// The groups [`Self::find_or_add_group`] may return, by
    /// [`expression_hash`] of their expression: every group whose sharing
    /// is legal ([`GroupProperties::shareable_when_equal`](crate::stages::GroupProperties::shareable_when_equal)).
    groups_by_expression_hash: HashMap<u64, Vec<(MemoGroupId, String)>>,
    /// The data workload's facts per source (`DataWorkload::sources`), fixed
    /// at construction: a `Scan` group's column signs read them.
    pub(super) source_facts: BTreeMap<String, SourceFacts>,
    /// The data workload's arrival (`DataWorkload::arrival`), fixed at
    /// construction ([`Self::data_arrival`]).
    data_arrival: DataArrival,
}

/// One query of the memo: its root over groups, its accuracy target and
/// $n_q$.
#[derive(Debug, Clone)]
pub struct MemoQuery {
    pub root: QueryRoot<MemoGroupId>,
    /// $A_q$; `None` for a query without one (every query when accuracy is
    /// not enforced).
    pub accuracy_target: Option<AccuracyTarget>,
    /// $n_q$, the number of values one evaluation returns; `None` when the
    /// data workload does not bound it. Pass 2 turns a whole-result failure
    /// probability into a per-value one with it.
    pub number_of_values_per_query_evaluation: Option<u64>,
}

impl Memo {
    /// A memo without groups over the sources `data` describes: a `Scan`
    /// group's column signs are the ones `data` declares for its source
    /// (`GroupProperties::column_signs`), and its arrival is `data`'s
    /// ([`Self::data_arrival`]).
    pub fn new(data: &DataWorkload) -> Self {
        Self {
            groups: Vec::new(),
            queries: Vec::new(),
            groups_by_expression_hash: HashMap::new(),
            source_facts: data.sources.clone(),
            data_arrival: data.arrival,
        }
    }

    /// How the data the memo's sources hold arrives
    /// (`DataWorkload::arrival`): whether a stored result over them can
    /// change after it was computed. `Unknown` counts as arriving.
    pub fn data_arrival(&self) -> DataArrival {
        self.data_arrival
    }

    /// The groups, by id.
    pub fn groups(&self) -> &[MemoGroup] {
        &self.groups
    }

    /// The most values one evaluation of `group`, a readout, a state, a
    /// grouped aggregate or a query result over one, returns, from the
    /// facts the data workload declares; `None` when they bound it in no
    /// way. As SQL optimizers estimate the groups of a
    /// `GROUP BY` (the product of the key columns' distinct counts, clamped
    /// to the input rows), but as an upper bound, it is the smaller of:
    /// - the values its keys can take ([`Self::keys_of_values`]): for keys
    ///   copied unchanged from source columns, the product, over the scans
    ///   they come from, of the combinations each scan's source declares
    ///   for its keys (`SourceFacts::distinct_values_per_label` and
    ///   `distinct_values_per_label_set`), at most its series for a PromQL
    ///   source, each series having one label set; 1 for no key; for a
    ///   per-series result (or several rows per group, as a top-k), the
    ///   series of the PromQL sources scanned below it, summed
    ///   (`SourceFacts::series_count`). A SQL key with no declared count,
    ///   such as a measure column, or a computed key or a time, bounds
    ///   nothing: the groups of rows a SQL source declares do not say which
    ///   columns form them;
    /// - the rows its input reads per evaluation
    ///   ([`Self::max_rows_per_evaluation`]): there are never more groups
    ///   than input rows;
    /// - the rows the group itself returns: `ORDER BY q DESC LIMIT 5` above
    ///   `GROUP BY dst` returns at most 5 values, whatever `dst` declares.
    ///
    /// Keys from different scans multiply, for a join pairs their values:
    /// `flows JOIN hosts ... GROUP BY f.src, h.region`, with 6 `src` and 6
    /// `region` values declared, returns up to 36 values, not 6 + 6. The
    /// "one" side of a many-to-one vector match is another scan in the same
    /// way (`m{side="l"} * on(x) group_left(instance) m{side="r"}` by
    /// `job, instance` pairs labels of two series), and a key from an outer
    /// join's nullable side, which may be NULL, is computed
    /// (`ColumnLineage`).
    ///
    /// Examples: a readout of `quantile_over_time(0.9, m[5m])` over `m`
    /// with 1000 series returns at most 1000 values, which `max(...)` over
    /// it reads at once; `SELECT count(DISTINCT dst) FROM flows` reads one
    /// distinct count per evaluation; `GROUP BY bytes` over the last 15
    /// minutes of `flows`, 6 groups of rows sampled every minute, returns
    /// at most the 6 × 16 = 96 rows it reads. It is the readout values `R`
    /// a query's failure probability splits over, and Stage 0's bound on a
    /// query's values `n_q` (`workload::bounds::values_per_result`): `GROUP
    /// BY dst` with 300 `dst` values declared returns up to 300.
    pub fn max_values_per_evaluation(&self, group: MemoGroupId) -> Option<u64> {
        let (keys, input) = self.keys_of_values(group)?;
        let by_keys = match keys {
            // PromQL has no join: a vector binary operator returns at most
            // one series per series of one side, `or` both sides', so the
            // series of the sources scanned add up.
            KeysOfValues::PerSeries => self.groups[input.0 as usize]
                .group_properties
                .scanned_sources
                .iter()
                .try_fold(0u64, |total, source| {
                    if !source.is_time_series() {
                        return None;
                    }
                    let (facts, _) = series_facts(&self.source_facts, source_name(source))?;
                    total.checked_add(facts.series_count?)
                }),
            KeysOfValues::SourceColumns(columns) => {
                let mut columns_per_scan: BTreeMap<(&Source, u32), Vec<&str>> = BTreeMap::new();
                for column in &columns {
                    columns_per_scan
                        .entry((&column.source, column.scan_ordinal))
                        .or_default()
                        .push(column.column.as_str());
                }
                columns_per_scan
                    .into_iter()
                    .try_fold(1u64, |product, ((source, _), columns)| {
                        let (facts, _) = series_facts(&self.source_facts, source_name(source))?;
                        let declared = facts.declared_distinct_combinations(&columns);
                        let series = facts.series_count.filter(|_| source.is_time_series());
                        product.checked_mul(smaller_known(declared, series)?)
                    })
            }
            KeysOfValues::ComputedOrTime => None,
        };
        let by_rows = smaller_known(
            self.max_rows_per_evaluation(input),
            self.max_rows_per_evaluation(group),
        );
        smaller_known(by_keys, by_rows)
    }

    /// The most estimates (sketch readout values) one evaluation of the
    /// readout group `group` computes: its values
    /// ([`Self::max_values_per_evaluation`]), except for a top-k, which
    /// estimates every distinct item of each state it ranks. That is at
    /// most its states' count times the distinct items one state counts
    /// ([`Self::max_distinct_per_evaluation`]), and at most the rows (or
    /// series) its values are bounded by, as each item comes from a row;
    /// the smaller known one. This is the `R` a readout's failure
    /// probability is charged for when an output reads all its values.
    /// The distinct-item bound is the deployment's contract, as HLL's
    /// error is: a source declaring fewer distinct items than a state
    /// counts gives too small an `R`, and a δ split over too few
    /// estimates.
    ///
    /// Example: `topk(10, sum by (dst) (count_over_time(flows[5m])))` as
    /// one heap per `job`, with 3 jobs and at most 500 distinct `dst` per
    /// evaluation declared, estimates at most 3 × 500 = 1500 items, while
    /// it returns 30 values.
    pub fn max_estimates_per_evaluation(&self, group: MemoGroupId) -> Option<u64> {
        let Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input,
            query: SketchStatistic::TopK { .. },
        }) = &self.groups[group.0 as usize].expression
        else {
            return self.max_values_per_evaluation(group);
        };
        let by_distinct_items = self
            .max_values_per_evaluation(*summary_input)
            .zip(self.max_distinct_per_evaluation(group))
            .and_then(|(states, items)| states.checked_mul(items));
        smaller_known(by_distinct_items, self.max_values_per_evaluation(group))
    }

    /// The most rows one evaluation of `group` returns, from the facts the
    /// data workload declares; `None` when they bound it in no way. A `Scan`
    /// returns the rows its source holds (`SourceFacts::rows_held`), and a
    /// `TimeRange` over it the rows of its window
    /// (`SourceFacts::max_rows_in_window`; for an instant selector, one per
    /// series at most); an operator that keeps, drops or groups its input's
    /// rows at most its input's, and `LIMIT n` over the whole input at most
    /// n; a join at most the product of its inputs' (plus each side's own
    /// for an outer join), a union their sum.
    /// Example: `flows WHERE ts >= now() - INTERVAL '15 minutes'`, 6 groups
    /// of rows sampled every minute, returns at most 6 × 16 = 96 rows.
    fn max_rows_per_evaluation(&self, group: MemoGroupId) -> Option<u64> {
        let rows = |input: &MemoGroupId| self.max_rows_per_evaluation(*input);
        let facts = |source: &Source| {
            series_facts(&self.source_facts, source_name(source)).map(|(facts, _)| facts)
        };
        let Operator::NonASAP(op) = &self.groups[group.0 as usize].expression else {
            // A state, readout or merge: its rows are not its input's.
            return None;
        };
        match op {
            NonASAPOp::Scan { source, .. } => facts(source)?.rows_held,
            NonASAPOp::Values { rows, .. } => u64::try_from(rows.len()).ok(),
            NonASAPOp::TimeRange { range, kind, child } => {
                let window_rows = match &self.groups[child.0 as usize].expression {
                    Operator::NonASAP(NonASAPOp::Scan { source, .. }) => {
                        facts(source).and_then(|facts| {
                            let window_ms = u64::try_from(range.as_millis()).ok()?;
                            let in_window = facts.max_rows_in_window(window_ms);
                            match kind {
                                TimeRangeKind::Range => in_window,
                                TimeRangeKind::Instant => {
                                    smaller_known(in_window, facts.series_count)
                                }
                            }
                        })
                    }
                    Operator::NonASAP(_) | Operator::ASAP(_) => None,
                };
                smaller_known(window_rows, rows(child))
            }
            NonASAPOp::Filter { child, .. }
            | NonASAPOp::Project { child, .. }
            | NonASAPOp::Dedup { child, .. }
            | NonASAPOp::Sort { child, .. }
            | NonASAPOp::SQLWindowFunc { child, .. }
            | NonASAPOp::TimeShift { child, .. }
            | NonASAPOp::PromqlRelabel { child, .. }
            | NonASAPOp::PromqlInfoEnrich { child, .. }
            | NonASAPOp::PromqlSeriesSample { child, .. } => rows(child),
            // `LIMIT n` over the whole input keeps at most n rows; per
            // partition (`topk by (job)`), n per partition.
            NonASAPOp::Limit {
                n,
                partition_by,
                child,
                ..
            } => {
                let whole_input = !partition_by.is_without() && partition_by.keys().is_empty();
                let kept = n
                    .filter(|_| whole_input)
                    .and_then(|n| u64::try_from(n).ok());
                smaller_known(kept, rows(child))
            }
            // A global aggregate returns one row even over no rows.
            NonASAPOp::Aggregate { child, .. } => rows(child).map(|rows| rows.max(1)),
            NonASAPOp::Join {
                kind, left, right, ..
            } => {
                let (left, right) = (rows(left)?, rows(right)?);
                let pairs = left.checked_mul(right)?;
                match kind {
                    JoinKind::Inner | JoinKind::Cross => Some(pairs),
                    JoinKind::Semi | JoinKind::Anti => Some(left),
                    JoinKind::Left | JoinKind::Right | JoinKind::Full => {
                        pairs.checked_add(left)?.checked_add(right)
                    }
                }
            }
            // A vector binary operator returns at most one row per row of
            // one side (`or` keeps both sides' rows); a set operator at most
            // both sides' rows.
            NonASAPOp::BinaryOp { lhs, rhs, .. }
            | NonASAPOp::SetOp {
                left: lhs,
                right: rhs,
                ..
            } => rows(lhs)?.checked_add(rows(rhs)?),
            NonASAPOp::Concat { children } => children
                .iter()
                .try_fold(0u64, |total, child| total.checked_add(rows(child)?)),
            NonASAPOp::PromqlVectorFromScalar(_) => Some(1),
            // Its child's rows at each step of its range, which no declared
            // fact bounds here.
            NonASAPOp::PromqlSubquery { .. } => None,
            // States, not rows.
            NonASAPOp::TimeRangeCover { .. } => None,
        }
    }

    /// The most distinct items one distinct-count summary over `group`'s
    /// rows counts in one evaluation: the sum of the bounds the data
    /// workload declares for the sources scanned below it
    /// (`GroupProperties::scanned_sources`,
    /// [`declared_max_distinct_per_evaluation`]); `None` when one of them
    /// declares none.
    ///
    /// Example: `count(distinct user_id)` over `Scan events` with `events`
    /// bounded at 9,636 gives 9,636 for every group above the scan.
    pub fn max_distinct_per_evaluation(&self, group: MemoGroupId) -> Option<u64> {
        declared_max_distinct_per_evaluation(
            &self.source_facts,
            self.groups[group.0 as usize]
                .group_properties
                .scanned_sources
                .iter()
                .map(source_name),
        )
    }

    /// What the values of `group` are keyed by, and the group of rows they
    /// are computed from ([`Self::max_values_per_evaluation`]): the keys of
    /// the state it is, or reads, or of the aggregate it is, and that
    /// state's or aggregate's input. A top-k readout or aggregate returns
    /// several rows per group, so it counts as per series. Above them, an
    /// operator that keeps, drops or reorders their rows (the projection,
    /// `HAVING`, `ORDER BY` and `LIMIT` of `SELECT src, count(*) ... GROUP
    /// BY src`), a finalization, or a merge keeping its states' groups has
    /// their keys. `None` for any other group.
    fn keys_of_values(&self, group: MemoGroupId) -> Option<(KeysOfValues, MemoGroupId)> {
        let (reduction, child, one_row_per_group) = match &self.groups[group.0 as usize].expression
        {
            Operator::NonASAP(
                NonASAPOp::Project { child, .. }
                | NonASAPOp::Filter { child, .. }
                | NonASAPOp::Sort { child, .. }
                | NonASAPOp::Limit { child, .. }
                | NonASAPOp::Dedup { child, .. },
            )
            | Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child, .. }) => {
                return self.keys_of_values(*child);
            }
            // A merge's children are states of one definition: its first
            // build's keys stand for all of them.
            Operator::ASAP(ASAPOp::SummaryMerge {
                children,
                group_by: None,
            }) => return self.keys_of_values(*children.first()?),
            Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input,
                query,
            }) => {
                let (keys, input) = self.keys_of_values(*summary_input)?;
                return Some(if matches!(query, SketchStatistic::TopK { .. }) {
                    (keys.several_rows_per_group(), input)
                } else {
                    (keys, input)
                });
            }
            Operator::ASAP(
                ASAPOp::SummaryAgg {
                    reduction, child, ..
                }
                | ASAPOp::SummaryTumblingWindowChain {
                    reduction, child, ..
                },
            ) => (reduction, *child, true),
            Operator::NonASAP(NonASAPOp::Aggregate {
                reduction,
                measures,
                child,
                ..
            }) => (
                reduction,
                *child,
                !measures
                    .iter()
                    .any(|m| matches!(m, AggIntent::TopK { .. } | AggIntent::CountValues { .. })),
            ),
            _ => return None,
        };
        let keys = match reduction {
            Reduction::Reduce(by) if !by.is_without() => by.keys(),
            // One value per series, or per group of every label but some.
            _ => return Some((KeysOfValues::PerSeries, child)),
        };
        let input = &self.groups[child.0 as usize].group_properties;
        let mut columns = Vec::new();
        for key in keys {
            let Some(ColumnLineage::SourceColumn(column)) = input.column_lineage.get(*key) else {
                return Some((KeysOfValues::ComputedOrTime, child));
            };
            let time_valued = input.schema.fields.get(*key).is_some_and(|field| {
                matches!(
                    field.dtype,
                    FieldDataType::Plain(DataType::Timestamp | DataType::Date)
                )
            });
            if time_valued {
                return Some((KeysOfValues::ComputedOrTime, child));
            }
            columns.push(column.clone());
        }
        let keys = KeysOfValues::SourceColumns(columns);
        Some(if one_row_per_group {
            (keys, child)
        } else {
            (keys.several_rows_per_group(), child)
        })
    }

    /// The queries, by index in the workload.
    pub fn queries(&self) -> &[MemoQuery] {
        &self.queries
    }

    /// Each query output reference (the query's index, and the group its
    /// operator root, or one operator of its scalar root, reads),
    /// deduplicated, ascending.
    pub fn output_groups(&self) -> Vec<(usize, MemoGroupId)> {
        self.queries
            .iter()
            .enumerate()
            .flat_map(|(index, query)| {
                query
                    .root
                    .operator_refs()
                    .into_iter()
                    .map(move |&group| (index, group))
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Adds `query` and returns its index: its root's groups and every
    /// group below them list it ([`MemoGroup::reached_by_queries`]). Stage 0
    /// adds each query once.
    pub fn add_query(&mut self, query: MemoQuery) -> usize {
        let index = self.queries.len();
        let groups = query.root.operator_refs().into_iter().copied().collect();
        self.queries.push(query);
        self.add_queries_below(groups, &[index]);
        index
    }

    /// Hash-consing: the group whose expression is `expression` (operator,
    /// arguments and input groups by typed equality, confirmed by equal
    /// serializations: `same_serialized_expression`), else a
    /// new group for it, with the `GroupProperties` `derive_group_properties`
    /// derives from its input groups, listed as a parent of each input
    /// group. A group whose copies may return different rows (an unordered
    /// `LIMIT`, [`GroupProperties::shareable_when_equal`](crate::stages::GroupProperties::shareable_when_equal)) is never returned
    /// for another expression, so such an expression always gets a new
    /// group. Fails, leaving the memo unchanged, when the expression is not
    /// valid over its input groups (its output schema does not derive, an
    /// input fails validation, or a merge reads states it may not merge),
    /// or with an [`InvariantViolation`] (checks 1–2).
    pub fn find_or_add_group(
        &mut self,
        expression: Operator<MemoGroupId>,
    ) -> Result<MemoGroupId, MemoGroupError> {
        let violation = |check, detail| {
            MemoGroupError::Invariant(InvariantViolation {
                check,
                group: None,
                origin: InvariantOrigin::GroupInsertion,
                detail,
            })
        };
        if let Some(missing) = self.first_missing_group(expression.children()) {
            return Err(violation(
                InvariantCheck::ReadsMissingGroup,
                format!("reads {missing:?} of {} groups", self.groups.len()),
            ));
        }
        let serialized = serialized_expression(&expression);
        let hash = expression_hash(&serialized);
        // Only shareable groups are listed by hash, and an equal expression
        // is as shareable as the group it equals, which was validated.
        if let Some((group, _)) = self
            .groups_by_expression_hash
            .get(&hash)
            .and_then(|groups| {
                groups.iter().find(|(group, known)| {
                    same_serialized_expression(
                        (&self.groups[group.0 as usize].expression, known),
                        (&expression, &serialized),
                    )
                })
            })
        {
            return Ok(*group);
        }
        let id = MemoGroupId(
            u32::try_from(self.groups.len()).expect("a memo has at most u32::MAX groups"),
        );
        let group_properties = derive_group_properties(self, id, &expression)?;
        invariants::check_column_facts(&group_properties, &expression)?;
        let shareable = group_properties.shareable_when_equal;
        for &input in expression.children() {
            add_parent(&mut self.groups[input.0 as usize], id);
        }
        self.groups.push(MemoGroup {
            expression,
            group_properties,
            parents: Vec::new(),
            queries: Vec::new(),
            candidates: Vec::new(),
        });
        if shareable {
            self.groups_by_expression_hash
                .entry(hash)
                .or_default()
                .push((id, serialized));
        }
        Ok(id)
    }

    /// The output schema `expression` would have as a new group, without
    /// adding anything: the dry run of [`Self::find_or_add_group`], which a
    /// rule calls to decide whether a way to compute a group can be built
    /// before it adds any of the way's groups. `expression` reads memo
    /// groups or pending expressions, not yet in the memo: the input
    /// `MemoGroupId(n + k)`, `n` the memo's group count, is
    /// `pending[k]`, the id it would get as a new group, with its output
    /// schema (from earlier calls). The expression's output schema must
    /// derive and its inputs pass validation.
    ///
    /// It mirrors `validate_expression_over_input_groups`, minus the merge
    /// coverage check, which runs when a merge is added.
    ///
    /// # Panics
    ///
    /// When a pending expression reads one at or after it.
    pub fn output_schema_of_pending_expression(
        &self,
        pending: &[(Operator<MemoGroupId>, Schema)],
        expression: &Operator<MemoGroupId>,
    ) -> Result<Schema, MemoGroupError> {
        fn input<'m>(
            memo: &'m Memo,
            built: &[InputOrTemporaryBuild<MemoGroupAsInput<'m>>],
            group_id: MemoGroupId,
        ) -> InputOrTemporaryBuild<MemoGroupAsInput<'m>> {
            match (group_id.0 as usize).checked_sub(memo.groups.len()) {
                None => InputOrTemporaryBuild::Input(MemoGroupAsInput { memo, group_id }),
                Some(index) => built
                    .get(index)
                    .expect("a pending expression reads only earlier ones")
                    .clone(),
            }
        }
        let mut built = Vec::with_capacity(pending.len());
        for (pending_expression, schema) in pending {
            let operator =
                pending_expression.map_children(|&group_id| input(self, &built, group_id));
            built.push(InputOrTemporaryBuild::with_schema(operator, schema.clone()));
        }
        let operator = expression.map_children(|&group_id| input(self, &built, group_id));
        let schema = operator.output_schema()?;
        operator.validate_inputs()?;
        Ok(schema)
    }

    /// Adds `candidate` to `group`'s candidates and returns its id, unless
    /// the group already has a candidate with the same expression
    /// (`same_expression`), logical properties and input requirements: then
    /// that candidate's id, and `candidate` is dropped. A new candidate makes
    /// `group` a parent of each of its input groups, and `group`'s queries
    /// reach each input group and every group below it.
    ///
    /// Fails, leaving the memo unchanged, when the candidate's expression
    /// is not valid over its input groups
    /// (`validate_expression_over_input_groups`),
    /// when its output schema or kind differs from the group's (then it
    /// does not compute the group's result), or when its θ does not fit the
    /// group's output: a candidate of a sketch state group (a build, or a
    /// merge, `Filter`, `TimeRange` or `TimeShift` passing states on) has a
    /// θ from that algorithm's list, any other candidate none. Only the
    /// output mismatch is an ordinary rejection (Pass 2's rules rely on
    /// it); the rest are [`InvariantViolation`]s (checks 2–4) naming the
    /// candidate's rule.
    pub fn add_candidate(
        &mut self,
        group: MemoGroupId,
        candidate: CandidateForMemoGroup,
    ) -> Result<CandidateForMemoGroupId, CandidateError> {
        let violation = |check, detail| {
            CandidateError::Invariant(InvariantViolation {
                check,
                group: Some(group),
                origin: InvariantOrigin::Rule(candidate.origin.clone()),
                detail,
            })
        };
        let inputs = candidate.expression.operator.children();
        if let Some(missing) = self.first_missing_group(
            std::iter::once(&group)
                .chain(inputs.iter().copied())
                .collect(),
        ) {
            return Err(violation(
                InvariantCheck::ReadsMissingGroup,
                format!("reads {missing:?} of {} groups", self.groups.len()),
            ));
        }
        if candidate.input_requirements.len() != inputs.len() {
            return Err(violation(
                InvariantCheck::InputRequirementsPerInput,
                format!(
                    "{} input requirements for {} inputs",
                    candidate.input_requirements.len(),
                    inputs.len()
                ),
            ));
        }
        if let Some(index_in_group) = self.groups[group.0 as usize]
            .candidates
            .iter()
            .position(|known| same_candidate(known, &candidate))
        {
            return Ok(CandidateForMemoGroupId {
                group,
                index_in_group,
            });
        }
        let operator = candidate
            .expression
            .operator
            .map_children(|&group_id| MemoGroupAsInput {
                memo: self,
                group_id,
            });
        let (schema, _) = validate_expression_over_input_groups(&operator).map_err(|error| {
            violation(
                InvariantCheck::CandidateNotValidOverInputs,
                error.to_string(),
            )
        })?;
        let result_kind = operator.output_kind();
        let group_properties = &self.groups[group.0 as usize].group_properties;
        if schema != group_properties.schema || result_kind != group_properties.result_kind {
            return Err(CandidateError::OutputDiffersFromGroup { group });
        }
        parameters_fit(
            group_properties.sketch_algorithm_of_state(),
            candidate.logical_properties.parameters(),
        )
        .map_err(|error| {
            violation(
                InvariantCheck::CandidateParametersDoNotFit,
                error.to_string(),
            )
        })?;
        let candidates = &mut self.groups[group.0 as usize].candidates;
        let input_groups: Vec<MemoGroupId> = candidate
            .expression
            .operator
            .children()
            .into_iter()
            .copied()
            .collect();
        candidates.push(candidate);
        let index_in_group = candidates.len() - 1;
        for &input in &input_groups {
            add_parent(&mut self.groups[input.0 as usize], group);
        }
        let queries = self.groups[group.0 as usize].queries.clone();
        self.add_queries_below(input_groups, &queries);
        Ok(CandidateForMemoGroupId {
            group,
            index_in_group,
        })
    }

    /// The first of `groups` not in the memo.
    fn first_missing_group(&self, groups: Vec<&MemoGroupId>) -> Option<MemoGroupId> {
        groups
            .into_iter()
            .find(|group| group.0 as usize >= self.groups.len())
            .copied()
    }

    /// Adds `queries` to each of `groups` and every group below them, through
    /// the groups' expressions and their candidates' input groups. A group
    /// that has every query already is not walked: every group below it has
    /// them too.
    fn add_queries_below(&mut self, groups: Vec<MemoGroupId>, queries: &[usize]) {
        let mut to_visit = groups;
        while let Some(group) = to_visit.pop() {
            let group = &mut self.groups[group.0 as usize];
            let mut added = false;
            for &query in queries {
                if let Err(position) = group.queries.binary_search(&query) {
                    group.queries.insert(position, query);
                    added = true;
                }
            }
            if added {
                to_visit.extend(
                    group
                        .expression
                        .children()
                        .into_iter()
                        .chain(
                            group
                                .candidates
                                .iter()
                                .flat_map(|candidate| candidate.expression.operator.children()),
                        )
                        .copied(),
                );
            }
        }
    }

    /// The candidate `id`.
    pub fn candidate(&self, id: CandidateForMemoGroupId) -> &CandidateForMemoGroup {
        &self.groups[id.group.0 as usize].candidates[id.index_in_group]
    }

    /// Replaces candidate `id`'s input requirements $r^{\log}_{c,i}$, one
    /// per input group. The only way to change a candidate's $r^{\log}$:
    /// Pass 2 sets a readout's required accuracy after the rules have run.
    /// Panics when the vector does not have one entry per input group.
    pub fn set_candidate_input_requirements(
        &mut self,
        id: CandidateForMemoGroupId,
        input_requirements: Vec<LogicalRequirements>,
    ) {
        let candidate = &mut self.groups[id.group.0 as usize].candidates[id.index_in_group];
        assert_eq!(
            input_requirements.len(),
            candidate.expression.operator.children().len(),
            "one input requirement per input group of candidate {id:?}"
        );
        candidate.input_requirements = input_requirements;
    }

    /// The candidates `consumer` may read on input `input`: those of the
    /// input group, except that a consumer with a θ (a build, or a candidate
    /// passing states on) reads only producers with the same θ or none.
    pub fn producers(
        &self,
        consumer: CandidateForMemoGroupId,
        input: usize,
    ) -> Vec<CandidateForMemoGroupId> {
        let candidate = self.candidate(consumer);
        let group = *candidate.expression.operator.children()[input];
        let theta = candidate.logical_properties.parameters();
        self.groups[group.0 as usize]
            .candidates
            .iter()
            .enumerate()
            .filter(
                |(_, producer)| match (theta, producer.logical_properties.parameters()) {
                    (Some(theta), Some(produced)) => theta == produced,
                    _ => true,
                },
            )
            .map(|(index_in_group, _)| CandidateForMemoGroupId {
                group,
                index_in_group,
            })
            .collect()
    }

    /// The groups, each after every group it or one of its candidates
    /// reads, the lower index first among those ready: a dependency order for
    /// candidate traversal, whatever order rules appended groups in. `None`
    /// when those reads form a cycle, a group reading itself included.
    pub fn topological_order(&self) -> Option<Vec<MemoGroupId>> {
        let count = self.groups.len();
        let mut reads: Vec<HashSet<usize>> = vec![HashSet::new(); count];
        for (g, group) in self.groups.iter().enumerate() {
            for child in group.expression.children().into_iter().chain(
                group
                    .candidates
                    .iter()
                    .flat_map(|c| c.expression.operator.children()),
            ) {
                // A group reading itself never becomes ready: a cycle.
                reads[g].insert(child.0 as usize);
            }
        }
        let mut consumers: Vec<Vec<usize>> = vec![Vec::new(); count];
        let mut waiting: Vec<usize> = reads.iter().map(HashSet::len).collect();
        for (g, read) in reads.iter().enumerate() {
            for &child in read {
                consumers[child].push(g);
            }
        }
        let mut ready: std::collections::BinaryHeap<std::cmp::Reverse<usize>> = (0..count)
            .filter(|&g| waiting[g] == 0)
            .map(std::cmp::Reverse)
            .collect();
        let mut order = Vec::with_capacity(count);
        while let Some(std::cmp::Reverse(g)) = ready.pop() {
            order.push(MemoGroupId(g as u32));
            for &consumer in &consumers[g] {
                waiting[consumer] -= 1;
                if waiting[consumer] == 0 {
                    ready.push(std::cmp::Reverse(consumer));
                }
            }
        }
        (order.len() == count).then_some(order)
    }

    /// The groups some query's root reads, ascending: the groups of the
    /// queries' roots that are a query's result or a scalar query's operand.
    pub fn root_groups(&self) -> BTreeSet<MemoGroupId> {
        self.queries
            .iter()
            .flat_map(|query| query.root.operator_refs())
            .copied()
            .collect()
    }

    /// The queries whose root reads `group`, ascending.
    pub fn queries_rooted_at(&self, group: MemoGroupId) -> Vec<usize> {
        (0..self.queries.len())
            .filter(|&query| self.queries[query].root.operator_refs().contains(&&group))
            .collect()
    }
}

/// Lists `parent` among `group`'s parents, once.
fn add_parent(group: &mut MemoGroup, parent: MemoGroupId) {
    if !group.parents.contains(&parent) {
        group.parents.push(parent);
    }
}

/// What the values of a readout, state or aggregate are keyed by
/// ([`Memo::keys_of_values`]).
enum KeysOfValues {
    /// One value per series (or per group of every label but some), or
    /// several per group (a top-k): at most the series of the PromQL
    /// sources scanned.
    PerSeries,
    /// One value per combination of these columns, each copied unchanged
    /// from a column of one scan of a source (`by (job)`, `GROUP BY src`);
    /// none for one value.
    SourceColumns(Vec<SourceColumn>),
    /// A key is computed (`date_trunc('minute', ts)`, an expression) or a
    /// time: one value per bucket or time, which no declared count bounds.
    ComputedOrTime,
}

impl KeysOfValues {
    /// These keys for a result with several rows per group (a top-k): its
    /// values are not bounded by its groups.
    fn several_rows_per_group(self) -> Self {
        match self {
            Self::PerSeries | Self::SourceColumns(_) => Self::PerSeries,
            Self::ComputedOrTime => Self::ComputedOrTime,
        }
    }
}

#[cfg(test)]
mod tests;
