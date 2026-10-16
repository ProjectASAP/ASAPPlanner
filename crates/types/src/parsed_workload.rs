//! [`ParsedWorkload`] — a [`PlanningWorkload`] whose queries have been lowered
//! to the operator IR.
//!
//! This is the boundary between the query-language frontends and Stage 1,
//! logical ASAP-aware optimization (issues #429, #430). Everything downstream of lowering consumes this type
//! and never sees query text, a catalog, or a query language; everything the
//! optimizer still needs about *demand* — recurrence, predictability,
//! execution time, accuracy requirement — is read off the retained
//! [`PlanningWorkload`], which the lowering never consumes.

use std::rc::Rc;

use crate::ir::{OperatorNode, QueryRoot, ScalarExpr};
use crate::workload::{DataWorkload, PlanningWorkload, QueryWorkload, WorkloadError};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParsedWorkloadError {
    #[error(
        "{lowered} lowered expression(s) for a workload with {entries} normalized entry/entries"
    )]
    LengthMismatch { entries: usize, lowered: usize },
}

/// One lowered expression per normalized workload entry, in
/// [`QueryWorkload::entries`] order (batch first, then repeating).
///
/// Positional correspondence between the lowered roots and the workload's
/// entries is an invariant, not a convention: the fields are private and
/// [`Self::from_roots`] is the only constructor, so a caller cannot hand the
/// optimizer a root bound to the wrong entry's recurrence.
#[derive(Debug, Clone)]
pub struct ParsedWorkload {
    workload: PlanningWorkload,
    exprs: Vec<Rc<OperatorNode>>,
    operator_indices: Vec<usize>,
    scalars: Vec<(usize, ScalarExpr)>,
}

impl ParsedWorkload {
    /// `roots[i]` must be the lowering of `workload.query_workload.entries()`'s
    /// `i`-th entry.
    pub fn from_roots(
        workload: PlanningWorkload,
        roots: Vec<QueryRoot>,
    ) -> Result<Self, ParsedWorkloadError> {
        let entries = workload.query_workload.entries().count();
        if entries != roots.len() {
            return Err(ParsedWorkloadError::LengthMismatch {
                entries,
                lowered: roots.len(),
            });
        }
        let mut exprs = Vec::new();
        let mut operator_indices = Vec::new();
        let mut scalars = Vec::new();
        for (index, root) in roots.into_iter().enumerate() {
            match root {
                QueryRoot::Operator(node) => {
                    operator_indices.push(index);
                    exprs.push(node);
                }
                QueryRoot::Scalar(expr) => scalars.push((index, expr)),
            }
        }
        Ok(Self {
            workload,
            exprs,
            operator_indices,
            scalars,
        })
    }

    pub fn planning_workload(&self) -> &PlanningWorkload {
        &self.workload
    }

    pub fn query_workload(&self) -> &QueryWorkload {
        &self.workload.query_workload
    }

    pub fn data_workload(&self) -> Option<&DataWorkload> {
        self.workload.data_workload.as_ref()
    }

    pub fn exprs(&self) -> &[Rc<OperatorNode>] {
        &self.exprs
    }

    pub fn len(&self) -> usize {
        self.exprs.len() + self.scalars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn operator_indices(&self) -> &[usize] {
        &self.operator_indices
    }

    pub fn scalar_roots(&self) -> &[(usize, ScalarExpr)] {
        &self.scalars
    }

    /// The retained workload's own validation — entry legality and data-workload
    /// consistency. The PromQL-specific checks it also runs were already a
    /// precondition of the lowering that produced `self`.
    pub fn validate(&self) -> Result<(), WorkloadError> {
        self.workload.validate()
    }
}
