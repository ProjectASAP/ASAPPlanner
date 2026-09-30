//! Timed stage of the candidate collection; lifecycle machinery stays internal.
use std::rc::Rc;

use crate::{
    cost_model::CostModel,
    replacement::{CandidatePostASAPDAGs, RealizationError},
    summary_maintenance_lifecycle::{
        enumerate_summary_maintenance_lifecycles, SummaryMaintenanceLifecycleCandidates,
    },
    Horizon, LifecyclePostASAPDAG, LifecyclePostASAPDAGError, SummaryMaintenanceDeployment,
    SummaryMaintenanceLifecycleCapabilities, SummaryMaintenanceLifecycleChoiceError,
    SummaryMaintenanceTimingError, WorkloadDemand,
};
use asap_types::post_asap::{
    index_post_asap_dag, ExecutionDataStateError, PostASAPDAG, PostASAPDAGAssignment,
    PostASAPDAGIndex, PostASAPNodeId, SummaryMaintenanceLifecycle,
    SummaryMaintenanceLifecycleGuarantee,
};

/// Demand and evidence bound to the requested workload root, not unrelated queries.
pub struct CandidateTimingContext<'a> {
    pub demand: WorkloadDemand<'a>,
    pub now_ms: u64,
    pub horizon: Option<Horizon>,
    pub capabilities: SummaryMaintenanceLifecycleCapabilities,
    pub cost_model: &'a dyn CostModel,
}

#[derive(Debug, thiserror::Error)]
pub enum CandidateTimingError {
    #[error(transparent)]
    Logical(#[from] RealizationError),
    #[error(transparent)]
    Lifecycle(#[from] LifecyclePostASAPDAGError),
    #[error(transparent)]
    Choice(#[from] SummaryMaintenanceLifecycleChoiceError),
    #[error(transparent)]
    Timing(#[from] SummaryMaintenanceTimingError),
    #[error(transparent)]
    Graph(#[from] ExecutionDataStateError),
    #[error("timed candidate expansion exceeds limit {0}")]
    ExpansionLimit(usize),
    #[error("unknown logical candidate {0}")]
    UnknownLogicalCandidate(usize),
}

struct PreparedTiming<'a> {
    index: Rc<PostASAPDAGIndex>,
    lifecycles: SummaryMaintenanceLifecycleCandidates<'a>,
    count: usize,
}

/// Every lifecycle assignment of one workload root's logical candidates, each a
/// [`LifecyclePostASAPDAG`]. It retains shared graph indices and factored
/// choices, never a Cartesian-product vector of timed graphs, and it does not
/// select an assignment.
pub struct CandidateLifecyclePostASAPDAGs<'a, Id> {
    id: Id,
    logical: Vec<Result<PreparedTiming<'a>, Rc<CandidateTimingError>>>,
    rejected_assemblies: Vec<String>,
    count: usize,
}

/// Candidate identity and lifecycle evidence survive both timing and compilation
/// failures. `lifecycle` is `None` when lifecycle binding failed, not an
/// implicit default.
#[derive(Debug)]
pub struct PostASAPCandidateMetadata<Id> {
    pub id: Id,
    pub logical_candidate: usize,
    pub assignment_candidate: usize,
    pub choices: Vec<(PostASAPNodeId, SummaryMaintenanceLifecycle)>,
    pub lifecycle: Option<LifecyclePostASAPDAG>,
}

impl<Id: Clone + PartialEq> CandidatePostASAPDAGs<Id> {
    /// Attach every lifecycle alternative for this root's logical candidates.
    /// Both budgets are checked before a collection can be iterated. Whole-
    /// workload selection must still coordinate choices and shared state.
    pub fn with_timing_for_root<'a>(
        &self,
        id: &Id,
        context: CandidateTimingContext<'a>,
        logical_limit: usize,
        assignment_limit: usize,
    ) -> Result<CandidateLifecyclePostASAPDAGs<'a, Id>, CandidateTimingError> {
        let inventory = self
            .enumerate_candidate_dags_for_root(id, logical_limit)
            .map_err(|error| match error {
                RealizationError::ExpansionLimit(limit) => {
                    CandidateTimingError::ExpansionLimit(limit)
                }
                error => error.into(),
            })?;
        let roots = inventory.candidates.into_iter().map(|mut roots| {
            // The logical enumerator was explicitly scoped to this one root.
            debug_assert_eq!(roots.len(), 1);
            roots.remove(0).1
        });
        prepare(
            id.clone(),
            roots,
            inventory.rejected_assemblies,
            context,
            assignment_limit,
        )
    }
}

impl<'a, Id> CandidateLifecyclePostASAPDAGs<'a, Id> {
    /// Enter the same timed collection API when a caller already has one logical
    /// graph. Explicit selection helpers do not need another lifecycle API type.
    pub fn from_post_asap_dag(
        id: Id,
        root: PostASAPDAG,
        context: CandidateTimingContext<'a>,
        assignment_limit: usize,
    ) -> Result<Self, CandidateTimingError> {
        prepare(id, [root], Vec::new(), context, assignment_limit)
    }
}

fn prepare<'a, Id>(
    id: Id,
    roots: impl IntoIterator<Item = PostASAPDAG>,
    rejected_assemblies: Vec<String>,
    context: CandidateTimingContext<'a>,
    limit: usize,
) -> Result<CandidateLifecyclePostASAPDAGs<'a, Id>, CandidateTimingError> {
    let enumerated = roots.into_iter().map(|root| {
        let index = Rc::new(index_post_asap_dag(&root)?);
        let lifecycles = enumerate_summary_maintenance_lifecycles(
            root,
            context.demand,
            context.now_ms,
            context.horizon,
            context.capabilities,
            context.cost_model,
        )?;
        Ok((index, lifecycles))
    });
    collect_timing(id, enumerated, rejected_assemblies, limit)
}

/// Collect each logical candidate's enumerated lifecycles into the timed collection.
pub(crate) fn collect_timing<'a, Id>(
    id: Id,
    enumerated: impl IntoIterator<
        Item = Result<
            (
                Rc<PostASAPDAGIndex>,
                SummaryMaintenanceLifecycleCandidates<'a>,
            ),
            CandidateTimingError,
        >,
    >,
    rejected_assemblies: Vec<String>,
    limit: usize,
) -> Result<CandidateLifecyclePostASAPDAGs<'a, Id>, CandidateTimingError> {
    let mut logical = Vec::new();
    let mut count = 0usize;
    for prepared in enumerated {
        let prepared = match prepared
            .and_then(|(index, lifecycles)| prepared_timing(index, lifecycles, limit))
        {
            // Budget failures are collection failures, never rejected choices
            // inside a deceptively complete partial collection.
            Err(error @ CandidateTimingError::ExpansionLimit(_)) => return Err(error),
            prepared => prepared.map_err(Rc::new),
        };
        count = count
            .checked_add(prepared.as_ref().map_or(1, |p| p.count))
            .filter(|count| *count <= limit)
            .ok_or(CandidateTimingError::ExpansionLimit(limit))?;
        logical.push(prepared);
    }
    Ok(CandidateLifecyclePostASAPDAGs {
        id,
        logical,
        rejected_assemblies,
        count,
    })
}

/// A logical candidate yields its assignments, or one diagnostic entry when it
/// has none: a state with no lifecycle alternative must not vanish silently.
fn prepared_timing(
    index: Rc<PostASAPDAGIndex>,
    lifecycles: SummaryMaintenanceLifecycleCandidates<'_>,
    limit: usize,
) -> Result<PreparedTiming<'_>, CandidateTimingError> {
    match lifecycles.assignment_count(limit) {
        Ok(count) => Ok(PreparedTiming {
            index,
            lifecycles,
            count,
        }),
        Err(SummaryMaintenanceLifecycleChoiceError::ExpansionLimit(limit)) => {
            Err(CandidateTimingError::ExpansionLimit(limit))
        }
        Err(error) => Err(error.into()),
    }
}

impl<Id: Clone> CandidateLifecyclePostASAPDAGs<'_, Id> {
    /// Includes rejected assignments; failures retain their candidate identity.
    pub fn len(&self) -> usize {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn logical_len(&self) -> usize {
        self.logical.len()
    }
    pub fn rejected_assemblies(&self) -> &[String] {
        &self.rejected_assemblies
    }

    pub fn iter(
        &self,
    ) -> impl Iterator<
        Item = (
            PostASAPCandidateMetadata<Id>,
            Result<PostASAPDAGAssignment, Rc<CandidateTimingError>>,
        ),
    > + '_ {
        self.logical
            .iter()
            .enumerate()
            .flat_map(move |(logical_candidate, prepared)| {
                (0..prepared.as_ref().map_or(1, |p| p.count)).map(move |assignment_candidate| {
                    let mut metadata = PostASAPCandidateMetadata {
                        id: self.id.clone(),
                        logical_candidate,
                        assignment_candidate,
                        choices: Vec::new(),
                        lifecycle: None,
                    };
                    let timing = match prepared {
                        Err(error) => Err(error.clone()),
                        Ok(prepared) => {
                            let candidate = prepared.lifecycles.assignment_at(assignment_candidate);
                            metadata.choices = candidate.choices;
                            match candidate.plan {
                                Err(error) => Err(Rc::new(CandidateTimingError::Choice(error))),
                                Ok(plan) => {
                                    let timing =
                                        plan.execution_assignment(prepared.index.clone()).map_err(
                                            |error| Rc::new(CandidateTimingError::Timing(error)),
                                        );
                                    metadata.lifecycle = Some(plan);
                                    timing
                                }
                            }
                        }
                    };
                    (metadata, timing)
                })
            })
    }

    /// Inspect alternatives or explicitly bind a choice without exposing the
    /// internal enumerator as another stage output.
    pub fn lifecycle_alternatives(
        &self,
        logical_candidate: usize,
    ) -> Result<&[SummaryMaintenanceDeployment], Rc<CandidateTimingError>> {
        self.prepared(logical_candidate)
            .map(|p| p.lifecycles.deployments())
    }
    /// Guarantee that choosing `lifecycle` for `state` would attach under this
    /// workload's data arrival, so a deployment can supply a price for that
    /// alternative before selection. `lifecycle` must be one of the state's alternatives.
    pub fn lifecycle_guarantee(
        &self,
        logical_candidate: usize,
        state: PostASAPNodeId,
        lifecycle: &SummaryMaintenanceLifecycle,
    ) -> Result<SummaryMaintenanceLifecycleGuarantee, Rc<CandidateTimingError>> {
        use SummaryMaintenanceLifecycleChoiceError as E;
        let prepared = self.prepared(logical_candidate)?;
        let deployment = prepared
            .lifecycles
            .deployments()
            .iter()
            .find(|d| d.post_asap_node_id == state)
            .ok_or_else(|| Rc::new(CandidateTimingError::Choice(E::UnknownSummary(state))))?;
        if !deployment
            .alternatives
            .iter()
            .any(|a| &a.summary_maintenance_lifecycle == lifecycle)
        {
            return Err(Rc::new(CandidateTimingError::Choice(E::NotAnAlternative(
                state,
            ))));
        }
        Ok(prepared.lifecycles.guarantee(lifecycle))
    }
    pub fn select_lifecycles(
        &self,
        logical_candidate: usize,
        choices: &[(PostASAPNodeId, SummaryMaintenanceLifecycle)],
    ) -> Result<LifecyclePostASAPDAG, Rc<CandidateTimingError>> {
        self.prepared(logical_candidate)?
            .lifecycles
            .clone()
            .select(choices)
            .map_err(|error| Rc::new(CandidateTimingError::Choice(error)))
    }
    fn prepared(
        &self,
        logical_candidate: usize,
    ) -> Result<&PreparedTiming<'_>, Rc<CandidateTimingError>> {
        self.logical
            .get(logical_candidate)
            .ok_or_else(|| {
                Rc::new(CandidateTimingError::UnknownLogicalCandidate(
                    logical_candidate,
                ))
            })?
            .as_ref()
            .map_err(Rc::clone)
    }
}
