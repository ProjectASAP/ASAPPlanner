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
    bindings: Vec<SupplierPriceBinding>,
}

impl PriceClass<'_> {
    fn same_interface(&self, other: &Self) -> bool {
        same_price(&self.priced, &other.priced)
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
            let mut supplier_counts = Vec::new();
            if let Some(input) = configured.inventory.graph.candidates[id]
                .producer_requirements
                .first()
            {
                for producer in &input.compatible_producers {
                    check_deadline(context.deadline)?;
                    if producer.0 != id {
                        supplier_counts.push((Some(*producer), classes[producer.0].len()));
                    }
                }
            } else {
                supplier_counts.push((None, 1));
            }
            // Stream local bindings instead of allocating a potentially enormous
            // supplier-by-option product before the deadline can be checked.
            let inputs = supplier_counts.into_iter().flat_map(|(producer, count)| {
                (0..count).map(move |price_option| {
                    producer.map(|producer| SupplierPriceBinding {
                        producer,
                        price_option,
                    })
                })
            });
            for binding in inputs {
                for &lookback in &all_lookbacks[id] {
                    check_deadline(context.deadline)?;
                    if let Some(binding) = binding {
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
                        binding.map(|binding| binding.producer),
                    )?;
                    let priced = cache[id].take();
                    if let Some(binding) = binding {
                        cache[binding.producer.0] = None;
                    }
                    let Some(priced) = priced else { continue };
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
                        bindings: binding.into_iter().collect(),
                    };
                    if let Some(existing) = classes[id]
                        .iter_mut()
                        .find(|known| known.same_interface(&candidate))
                    {
                        for binding in candidate.bindings {
                            if !existing.bindings.contains(&binding) {
                                existing.bindings.push(binding);
                            }
                        }
                    } else {
                        total += 1;
                        if total > MAXIMUM_PRICE_OPTIONS {
                            return Err(DirectProfileError::Unsupported(format!("local pricing domain exceeds {MAXIMUM_PRICE_OPTIONS} interface options")));
                        }
                        classes[id].push(candidate);
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
                        compatible_suppliers: class.bindings.clone(),
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
        schedules,
        query_rate: rate,
        request_latency_seconds: request_ms / 1000.0,
        request_cost: request_ms / settings.resource_speed.ms_per_cpu_op
            * prices.cost_per_cpu_op
            * request_rate,
        candidates,
    }))
}
