//! Timed stage of the candidate collection; lifecycle machinery stays internal.
use std::{marker::PhantomData, rc::Rc};

use crate::{
    cost_model::CostModel,
    replacement::{CandidatePostASAPDAGs, RealizationError},
    summary_maintenance_lifecycle::{
        enumerate_summary_maintenance_lifecycles, SummaryMaintenanceLifecycleCandidates,
    },
    Horizon, SummaryMaintenanceDeployment, SummaryMaintenanceLifecycleCapabilities,
    SummaryMaintenanceLifecycleChoiceError, SummaryMaintenanceLifecyclePlan,
    SummaryMaintenanceLifecyclePlanError, SummaryMaintenanceTimingError, WorkloadDemand,
};
use asap_types::post_asap::{
    index_post_asap_dag, ExecutionDataStateError, PostASAPDAG, PostASAPDAGAssignment,
    PostASAPDAGIndex, PostAsapNodeId, SummaryMaintenanceLifecycle,
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
    Lifecycle(#[from] SummaryMaintenanceLifecyclePlanError),
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

/// Storage for the timed stage. It retains shared graph indices and factored
/// choices, never a Cartesian-product vector of timed graphs.
pub struct WithTiming<'a, Id> {
    id: Id,
    logical: Vec<Result<PreparedTiming<'a>, Rc<CandidateTimingError>>>,
    rejected_assemblies: Vec<String>,
    count: usize,
}

/// Candidate identity and lifecycle evidence survive both timing and compilation
/// failures. `None` means lifecycle binding failed, not an implicit default.
#[derive(Debug)]
pub struct PostASAPCandidateMetadata<Id> {
    pub id: Id,
    pub logical_candidate: usize,
    pub assignment_candidate: usize,
    pub choices: Vec<(PostAsapNodeId, SummaryMaintenanceLifecycle)>,
    pub lifecycle: Option<SummaryMaintenanceLifecyclePlan>,
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
    ) -> Result<CandidatePostASAPDAGs<Id, WithTiming<'a, Id>>, CandidateTimingError> {
        let inventory = self.enumerate_candidate_dags_for_root(id, logical_limit)?;
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

    /// Enter the same timed collection API when a caller already has one logical
    /// graph. Explicit selection helpers do not need another lifecycle API type.
    pub fn from_post_asap_dag<'a>(
        id: Id,
        root: PostASAPDAG,
        context: CandidateTimingContext<'a>,
        assignment_limit: usize,
    ) -> Result<CandidatePostASAPDAGs<Id, WithTiming<'a, Id>>, CandidateTimingError> {
        prepare(id, [root], Vec::new(), context, assignment_limit)
    }
}

fn prepare<'a, Id>(
    id: Id,
    roots: impl IntoIterator<Item = PostASAPDAG>,
    rejected_assemblies: Vec<String>,
    context: CandidateTimingContext<'a>,
    limit: usize,
) -> Result<CandidatePostASAPDAGs<Id, WithTiming<'a, Id>>, CandidateTimingError> {
    let mut logical = Vec::new();
    let mut count = 0usize;
    for root in roots {
        let prepared = (|| {
            let index = Rc::new(index_post_asap_dag(&root)?);
            let lifecycles = enumerate_summary_maintenance_lifecycles(
                root,
                context.demand,
                context.now_ms,
                context.horizon,
                context.capabilities,
                context.cost_model,
            )?;
            Ok::<_, CandidateTimingError>((index, lifecycles))
        })();
        let prepared = match prepared {
            Ok((index, lifecycles)) => {
                // Budget failures are collection failures, never rejected choices
                // inside a deceptively complete partial collection.
                let n = lifecycles.assignment_count(limit)?;
                Ok(PreparedTiming {
                    index,
                    lifecycles,
                    count: n,
                })
            }
            Err(error) => Err(Rc::new(error)),
        };
        count = count
            .checked_add(prepared.as_ref().map_or(1, |p| p.count))
            .filter(|count| *count <= limit)
            .ok_or(CandidateTimingError::ExpansionLimit(limit))?;
        logical.push(prepared);
    }
    Ok(CandidatePostASAPDAGs {
        stage: WithTiming {
            id,
            logical,
            rejected_assemblies,
            count,
        },
        identity: PhantomData,
    })
}

impl<Id: Clone> CandidatePostASAPDAGs<Id, WithTiming<'_, Id>> {
    /// Includes rejected assignments; failures retain their candidate identity.
    pub fn len(&self) -> usize {
        self.stage.count
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn logical_len(&self) -> usize {
        self.stage.logical.len()
    }
    pub fn rejected_assemblies(&self) -> &[String] {
        &self.stage.rejected_assemblies
    }

    pub fn iter(
        &self,
    ) -> impl Iterator<
        Item = (
            PostASAPCandidateMetadata<Id>,
            Result<PostASAPDAGAssignment, Rc<CandidateTimingError>>,
        ),
    > + '_ {
        self.stage
            .logical
            .iter()
            .enumerate()
            .flat_map(move |(logical_candidate, prepared)| {
                (0..prepared.as_ref().map_or(1, |p| p.count)).map(move |assignment_candidate| {
                    let mut metadata = PostASAPCandidateMetadata {
                        id: self.stage.id.clone(),
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
    pub fn select_lifecycles(
        &self,
        logical_candidate: usize,
        choices: &[(PostAsapNodeId, SummaryMaintenanceLifecycle)],
    ) -> Result<SummaryMaintenanceLifecyclePlan, Rc<CandidateTimingError>> {
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
        self.stage
            .logical
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
