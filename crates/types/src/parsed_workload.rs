//! [`ParsedWorkload`] — a [`PlanningWorkload`] whose queries have been lowered
//! to pre-ASAP IR.
//!
//! This is the boundary between the frontend stage and the optimization stage
//! (issues #429, #430). Everything downstream of lowering consumes this type
//! and never sees query text, a catalog, or a query language; everything the
//! optimizer still needs about *demand* — recurrence, predictability,
//! execution time, accuracy requirement — is read off the retained
//! [`PlanningWorkload`], which the lowering never consumes.

use std::rc::Rc;

use crate::pre_asap::query_expr::QueryExpr;
use crate::workload::{
    DataWorkload, PlanningWorkload, QueryWorkload, QueryWorkloadEntry, WorkloadError,
};

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
/// Positional correspondence between `exprs` and the workload's entries is an
/// invariant, not a convention: the fields are private and [`Self::new`] is the
/// only constructor, so a caller cannot hand the optimizer a root bound to the
/// wrong entry's recurrence.
#[derive(Debug, Clone)]
pub struct ParsedWorkload {
    workload: PlanningWorkload,
    exprs: Vec<Rc<QueryExpr>>,
}

impl ParsedWorkload {
    /// `exprs[i]` must be the lowering of `workload.query_workload.entries()`'s
    /// `i`-th entry.
    pub fn new(
        workload: PlanningWorkload,
        exprs: Vec<Rc<QueryExpr>>,
    ) -> Result<Self, ParsedWorkloadError> {
        let entries = workload.query_workload.entries().count();
        if entries != exprs.len() {
            return Err(ParsedWorkloadError::LengthMismatch {
                entries,
                lowered: exprs.len(),
            });
        }
        Ok(Self { workload, exprs })
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

    pub fn exprs(&self) -> &[Rc<QueryExpr>] {
        &self.exprs
    }

    pub fn len(&self) -> usize {
        self.exprs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.exprs.is_empty()
    }

    /// Normalized entries paired with their lowered expression.
    pub fn entries(&self) -> impl Iterator<Item = (QueryWorkloadEntry, &Rc<QueryExpr>)> + '_ {
        self.workload
            .query_workload
            .entries()
            .zip(self.exprs.iter())
    }

    /// The retained workload's own validation — entry legality and data-workload
    /// consistency. The PromQL-specific checks it also runs were already a
    /// precondition of the lowering that produced `self`.
    pub fn validate(&self) -> Result<(), WorkloadError> {
        self.workload.validate()
    }
}
