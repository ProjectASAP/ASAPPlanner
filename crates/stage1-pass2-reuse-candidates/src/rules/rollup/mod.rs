//! Step 1, rollup from the finest common group (concepts doc, section 3):
//! a coarse aggregate group $G_c$ computed from a finer group $G_f$ of the
//! same definition, and per cluster of equal definitions one finest common
//! group $G^*$ every member can roll up from.
//!
//! - `member`: one aggregate group as the rule reads it, and its cluster.
//! - `finest_common_group`: a cluster's finest common group $G^*$.
//! - `rewrite`: the candidate computing a coarse group from a finer one.

mod finest_common_group;
mod member;
mod rewrite;

use crate::Pass2RuleError;
use asap_types::stages::{Memo, MemoGroupId};
use finest_common_group::finest_common_group;
use member::Member;
use rewrite::rollup_from;

/// Adds, to each aggregate group $G_c$, a candidate computing it from each
/// finer group $G_f$ of its cluster ([`rollup_from`]), after synthesizing
/// each cluster's finest common group ([`finest_common_group`]). A cluster
/// is the summary state groups (`SummaryAgg` of a summary type whose
/// states merge across groups) or exact `Aggregate` groups (of sums,
/// counts, minima and maxima) with equal definitions apart from their keys
/// ([`Member::definition`]) and equal interval constraints (the time
/// window).
pub(crate) fn add_rollup_candidates(memo: &mut Memo) -> Result<(), Pass2RuleError> {
    let mut clusters: Vec<Vec<Member>> = Vec::new();
    for index in 0..memo.groups().len() {
        let Some(member) = Member::of(memo, MemoGroupId(index as u32)) else {
            continue;
        };
        match clusters
            .iter_mut()
            .find(|cluster| cluster[0].same_cluster(&member))
        {
            Some(cluster) => cluster.push(member),
            None => clusters.push(vec![member]),
        }
    }
    for mut cluster in clusters.into_iter().filter(|cluster| cluster.len() > 1) {
        if let Some(finest) = finest_common_group(memo, &cluster)? {
            if cluster.iter().all(|member| member.group != finest) {
                cluster.extend(Member::of(memo, finest));
            }
        }
        for coarse in &cluster {
            for fine in &cluster {
                if coarse.group != fine.group {
                    rollup_from(memo, coarse, fine)?;
                }
            }
        }
    }
    Ok(())
}
