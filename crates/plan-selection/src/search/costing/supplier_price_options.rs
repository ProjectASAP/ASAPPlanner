//! Local pricing interfaces are generated from certified supplier interfaces.
//! No workload plans are enumerated. A candidate's distinct local options are
//! connected jointly by the MILP; identical interfaces retain every binding.
use super::*;
use crate::search::milp::direct_price_options::{
    CandidatePriceOption, JointPriceProfile, SupplierPriceBinding,
};

const MAXIMUM_PRICE_OPTIONS: usize = 50_000;

#[derive(Clone)]
struct PriceClass<'a> {
    priced: PricedNode<'a>,
    lookback: Option<u64>,
    sketches: usize,
    raw_source: Option<String>,
    work: WorkBounds,
    workers: Option<u64>,
    compaction: f64,
    bindings: Vec<Vec<SupplierPriceBinding>>,
    ancestor_work: std::collections::BTreeMap<usize, u64>,
    sketch_ids: std::collections::BTreeSet<usize>,
    sketch_sources: std::collections::BTreeMap<usize, Option<String>>,
    range_span_ms: Option<u64>,
}

impl PriceClass<'_> {
    fn same_interface(&self, other: &Self) -> bool {
        self.ancestor_work == other.ancestor_work
            && self.sketch_ids == other.sketch_ids
            && self.sketch_sources == other.sketch_sources
            && self.range_span_ms == other.range_span_ms
            && same_price(&self.priced, &other.priced)
            && self.lookback == other.lookback
            && self.sketches == other.sketches
            && self.raw_source == other.raw_source
            && self.workers == other.workers
            && self.compaction.to_bits() == other.compaction.to_bits()
            && self.work.minimum.to_bits() == other.work.minimum.to_bits()
            && self.work.maximum.to_bits() == other.work.maximum.to_bits()
    }
}

/// Exact scan-lookback equality remains required: max-width sharing needs an
/// explicit selected-consumer witness. Cyclic catalogs use bounded local
/// interface propagation; selected physical graphs must still be acyclic.
struct InputTuples {
    choices: Vec<Vec<SupplierPriceBinding>>,
    indices: Vec<usize>,
    done: bool,
}
impl InputTuples {
    fn new(choices: Vec<Vec<SupplierPriceBinding>>) -> Self {
        Self {
            indices: vec![0; choices.len()],
            done: choices.iter().any(Vec::is_empty),
            choices,
        }
    }
}
impl Iterator for InputTuples {
    type Item = Vec<SupplierPriceBinding>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let tuple = self
            .choices
            .iter()
            .zip(&self.indices)
            .map(|(choices, index)| choices[*index])
            .collect();
        self.done = true;
        for position in (0..self.indices.len()).rev() {
            self.indices[position] += 1;
            if self.indices[position] < self.choices[position].len() {
                self.done = false;
                break;
            }
            self.indices[position] = 0;
        }
        Some(tuple)
    }
}

pub(in crate::search) fn extract_price_options(
    configured: &CandidateConfigurationInventory,
    context: &CandidatePlanningContext<'_>,
) -> Result<Option<JointPriceProfile>, DirectProfileError> {
    let Some(prepared) = prepare_profile(configured, context, false)? else {
        return Ok(None);
    };
    let PreparedProfile {
        nodes,
        mut lookbacks,
        all_lookbacks,
        delta,
        schedules,
        rate,
        request_rate,
    } = prepared;
    let count = nodes.len();
    let mut pending = vec![0usize; count];
    let mut parents = vec![Vec::new(); count];
    for (id, node) in nodes.iter().enumerate() {
        if node.is_none() {
            continue;
        }
        for supplier in configured.inventory.graph.candidates[id]
            .producer_requirements
            .iter()
            .flat_map(|input| input.compatible_producers.iter())
        {
            if supplier.0 != id && nodes[supplier.0].is_some() {
                pending[id] += 1;
                parents[supplier.0].push(id);
            }
        }
    }
    let mut ready: Vec<_> = (0..count).filter(|id| pending[*id] == 0).collect();
    let mut order = Vec::new();
    while let Some(id) = ready.pop() {
        check_deadline(context.deadline)?;
        order.push(id);
        for parent in &parents[id] {
            pending[*parent] -= 1;
            if pending[*parent] == 0 {
                ready.push(*parent);
            }
        }
    }
    let acyclic = order.len() == count;
    if !acyclic {
        let mut ordered = vec![false; count];
        for id in &order {
            ordered[*id] = true;
        }
        order.extend((0..count).filter(|id| !ordered[*id]));
    }
    // Reuse the existing one-node certification/pricing path. Only one already
    // priced supplier interface is present in this reusable scratch cache, so
    // price() performs one local operation without recursive plan generation.
    let mut cache = vec![None; count];
    let mut visited = vec![true; count];
    let mut visiting = vec![false; count];
    let mut suppliers = vec![Vec::new(); count];
    let mut sketch_counts = vec![0; count];
    let mut raw_sources = vec![None; count];
    let mut closure_work = vec![WorkBounds::default(); count];
    let mut worker_counts = vec![None; count];
    let mut compactions = vec![0.0; count];
    let mut classes: Vec<Vec<PriceClass<'_>>> = vec![Vec::new(); count];
    let mut total = 0;
    let mut total_bindings = 0usize;
    // Every legal unary DAG path has at most N candidates. N monotone passes
    // therefore cover every supported simple path even in a cyclic catalog.
    // Walk-derived bindings are harmless: the solver enforces original-node
    // uniqueness and acyclicity. Class IDs remain stable as options append.
    for _round in 0..if acyclic { 1 } else { count } {
        let before = total;
        for &id in &order {
            check_deadline(context.deadline)?;
            if nodes[id].is_none() {
                continue;
            }
            let mut inputs = Vec::new();
            for input in &configured.inventory.graph.candidates[id].producer_requirements {
                let mut choices = Vec::new();
                for producer in &input.compatible_producers {
                    check_deadline(context.deadline)?;
                    if producer.0 == id {
                        continue;
                    }
                    for price_option in 0..classes[producer.0].len() {
                        choices.push(SupplierPriceBinding {
                            producer: *producer,
                            price_option,
                        });
                    }
                }
                inputs.push(choices);
            }
            let node = nodes[id].as_ref().unwrap();
            let spans: Vec<Option<u64>> = if matches!(
                node.payload,
                asap_types::ir::Operator::ASAP(ASAPOp::PaneChain { .. })
            ) {
                let mut spans: Vec<_> = context
                    .entries
                    .iter()
                    .map(|entry| Some(entry.range_evaluation.map_or(0, |range| range.span_ms)))
                    .collect();
                spans.sort();
                spans.dedup();
                spans
            } else {
                vec![None]
            };
            for binding in InputTuples::new(inputs) {
                for range_span_ms in &spans {
                    for &lookback in &all_lookbacks[id] {
                        check_deadline(context.deadline)?;
                        let mut selected_sources = std::collections::BTreeMap::new();
                        let mut ancestor_work = std::collections::BTreeMap::new();
                        let mut sketch_ids = std::collections::BTreeSet::new();
                        let mut sketch_sources = std::collections::BTreeMap::new();
                        let mut compatible = true;
                        for binding in &binding {
                            if selected_sources
                                .insert(binding.producer.0, binding.price_option)
                                .is_some_and(|prior| prior != binding.price_option)
                            {
                                compatible = false;
                                break;
                            }
                            let source = &classes[binding.producer.0][binding.price_option];
                            for (ancestor, work) in &source.ancestor_work {
                                if ancestor_work
                                    .insert(*ancestor, *work)
                                    .is_some_and(|prior| prior != *work)
                                {
                                    compatible = false;
                                }
                            }
                            sketch_ids.extend(source.sketch_ids.iter().copied());
                            for (sketch, proof) in &source.sketch_sources {
                                if sketch_sources
                                    .insert(*sketch, proof.clone())
                                    .is_some_and(|prior| prior != *proof)
                                {
                                    compatible = false;
                                }
                            }
                        }
                        if !compatible || ancestor_work.contains_key(&id) {
                            continue;
                        }
                        let upstream_sketches = sketch_ids.len();
                        if matches!(
                            &node.payload,
                            asap_types::ir::Operator::ASAP(
                                ASAPOp::SummaryAgg {
                                    family: FieldDataType::Sketch(..),
                                    ..
                                } | ASAPOp::PaneChain {
                                    family: FieldDataType::Sketch(..),
                                    ..
                                }
                            )
                        ) {
                            sketch_ids.insert(id);
                            sketch_sources.insert(
                                id,
                                binding.first().and_then(|input| {
                                    classes[input.producer.0][input.price_option]
                                        .raw_source
                                        .clone()
                                }),
                            );
                        }
                        if !sketch_ids.is_empty() {
                            let Some(delta) = delta else { continue };
                            let divided = delta / sketch_ids.len() as f64;
                            if !sketch_ids.iter().all(|sketch| {
                                let template =
                                    configured.inventory.templates[*sketch].as_ref().unwrap();
                                if requires_source_sizing(template) {
                                    let Some(source) =
                                        sketch_sources.get(sketch).and_then(|s| s.as_ref())
                                    else {
                                        return false;
                                    };
                                    let Some(bound) = context
                                        .data
                                        .sources
                                        .get(source)
                                        .and_then(|facts| facts.max_distinct_per_evaluation)
                                    else {
                                        return false;
                                    };
                                    let Some(
                                        ASAPOp::SummaryAgg {
                                            family: FieldDataType::Sketch(kind, _),
                                            ..
                                        }
                                        | ASAPOp::PaneChain {
                                            family: FieldDataType::Sketch(kind, _),
                                            ..
                                        },
                                    ) = template.asap()
                                    else {
                                        return false;
                                    };
                                    let SketchParams::Hll {
                                        precision,
                                        relative_error,
                                    } = kind.params()
                                    else {
                                        return false;
                                    };
                                    hll_size_certified(*precision, *relative_error, divided, bound)
                                } else {
                                    stable_size(template, Some(divided))
                                }
                            }) {
                                continue;
                            }
                        }
                        let mut upstream_work = WorkBounds::default();
                        for work in ancestor_work.values() {
                            upstream_work.minimum = (upstream_work.minimum + f64::from_bits(*work))
                                .next_down()
                                .max(0.0);
                            upstream_work.maximum =
                                (upstream_work.maximum + f64::from_bits(*work)).next_up();
                        }
                        for binding in &binding {
                            let supplier = binding.producer.0;
                            let source = &classes[supplier][binding.price_option];
                            cache[supplier] = Some(source.priced.clone());
                            lookbacks[supplier] = source.lookback;
                            sketch_counts[supplier] = source.sketches;
                            raw_sources[supplier] = source.raw_source.clone();
                            closure_work[supplier] = source.work;
                            worker_counts[supplier] = source.workers;
                            compactions[supplier] = source.compaction;
                        }
                        lookbacks[id] = lookback;
                        cache[id] = None;
                        visited[id] = false;
                        suppliers[id].clear();
                        sketch_counts[id] = 0;
                        raw_sources[id] = None;
                        closure_work[id] = WorkBounds::default();
                        worker_counts[id] = None;
                        compactions[id] = 0.0;
                        let input_context = PriceInputs {
                            suppliers: binding.iter().map(|binding| binding.producer).collect(),
                            upstream_work,
                            upstream_sketches,
                            range_span_ms: *range_span_ms,
                        };
                        price(
                            id,
                            configured,
                            context,
                            &nodes,
                            &lookbacks,
                            &mut cache,
                            &mut visited,
                            &mut visiting,
                            &mut suppliers,
                            &mut sketch_counts,
                            &mut raw_sources,
                            &mut closure_work,
                            &mut worker_counts,
                            &mut compactions,
                            delta,
                            Some(&input_context),
                        )?;
                        let priced = cache[id].take();
                        for binding in &binding {
                            cache[binding.producer.0] = None;
                        }
                        let Some(priced) = priced else { continue };
                        if matches!(
                            &node.payload,
                            asap_types::ir::Operator::ASAP(
                                ASAPOp::SummaryAgg {
                                    family: FieldDataType::Sketch(..),
                                    ..
                                } | ASAPOp::PaneChain {
                                    family: FieldDataType::Sketch(..),
                                    ..
                                }
                            )
                        ) {
                            sketch_ids.insert(id);
                        }
                        if node.output_state.timing == ExecutionTiming::IngestionTime {
                            ancestor_work.insert(id, priced.work_ms.to_bits());
                        } else {
                            ancestor_work.clear();
                        }
                        let candidate = PriceClass {
                            priced,
                            lookback,
                            sketches: sketch_counts[id],
                            raw_source: raw_sources[id].clone(),
                            // Query-time closure work never determines an ingestion
                            // worker count. Keeping it would distinguish every traversal
                            // of an identity cycle despite identical local interfaces.
                            work: if nodes[id].as_ref().unwrap().output_state.timing
                                == ExecutionTiming::IngestionTime
                            {
                                closure_work[id]
                            } else {
                                WorkBounds::default()
                            },
                            workers: worker_counts[id],
                            compaction: compactions[id],
                            bindings: vec![binding.clone()],
                            ancestor_work,
                            sketch_ids,
                            sketch_sources,
                            range_span_ms: *range_span_ms,
                        };
                        if let Some(existing) = classes[id]
                            .iter_mut()
                            .find(|known| known.same_interface(&candidate))
                        {
                            for binding in candidate.bindings {
                                if !existing.bindings.contains(&binding) {
                                    total_bindings += 1;
                                    if total_bindings > 200_000 {
                                        return Err(DirectProfileError::Unsupported("local pricing domain exceeds 200000 correlated input tuples".into()));
                                    }
                                    existing.bindings.push(binding);
                                }
                            }
                        } else {
                            total_bindings += candidate.bindings.len();
                            if total_bindings > 200_000 {
                                return Err(DirectProfileError::Unsupported(
                                    "local pricing domain exceeds 200000 correlated input tuples"
                                        .into(),
                                ));
                            }
                            total += 1;
                            if total > MAXIMUM_PRICE_OPTIONS {
                                return Err(DirectProfileError::Unsupported(format!("local pricing domain exceeds {MAXIMUM_PRICE_OPTIONS} interface options")));
                            }
                            classes[id].push(candidate);
                        }
                    }
                }
            }
        }
        if total == before {
            break;
        }
    }
    let candidates = classes
        .iter()
        .enumerate()
        .map(|(id, classes)| {
            classes
                .iter()
                .map(|class| {
                    Ok(CandidatePriceOption {
                        profile: profile_for_priced(
                            &class.priced,
                            nodes[id].as_ref().unwrap(),
                            class.compaction,
                            context,
                        )?,
                        compatible_input_tuples: class.bindings.clone(),
                        required_range_span_ms: class.range_span_ms,
                    })
                })
                .collect::<Result<Vec<_>, DirectProfileError>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    let settings = context.models.plan_selection;
    let CostModel::UsageOverTime(prices) = &settings.cost_model else {
        return Ok(None);
    };
    let request_ms = settings.measured_operation_costs.ns_per_evaluation * 1e-6;
    Ok(Some(JointPriceProfile {
        query_range_spans_ms: context
            .entries
            .iter()
            .map(|entry| entry.range_evaluation.map_or(0, |range| range.span_ms))
            .collect(),
        schedules,
        query_rate: rate,
        request_latency_seconds: request_ms / 1000.0,
        request_cost: request_ms / settings.resource_speed.ms_per_cpu_op
            * prices.cost_per_cpu_op
            * request_rate,
        candidates,
    }))
}
