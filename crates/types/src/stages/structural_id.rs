//! Structural identities of operator nodes, which the memo hash-conses
//! groups on ([`super::Memo::intern`]).

use std::collections::HashMap;
use std::rc::Rc;

use crate::ir::OperatorNode;

/// Interned structural identities: the operator over its children's identities,
/// without the schema, guarantee, timing and materialization composition
/// derives. Memoized per node (the cache keeps each node alive, so its
/// address is not reused).
#[derive(Default, Clone)]
pub struct NodeToStructuralIdCache {
    id_of_node: HashMap<*const OperatorNode, (Rc<OperatorNode>, u64)>,
    id_of_encoded_operator: HashMap<Vec<u8>, u64>,
}

impl std::fmt::Debug for NodeToStructuralIdCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeToStructuralIdCache")
            .field("nodes", &self.id_of_node.len())
            .finish_non_exhaustive()
    }
}

impl NodeToStructuralIdCache {
    /// `node`'s structural id: equal for two nodes exactly when their
    /// operators, over their children's structural ids, are equal.
    pub fn structural_id(&mut self, node: &Rc<OperatorNode>) -> u64 {
        if let Some((_, key)) = self.id_of_node.get(&Rc::as_ptr(node)) {
            return *key;
        }
        let children: HashMap<*const OperatorNode, u64> = node
            .children()
            .into_iter()
            .map(|child| (Rc::as_ptr(child), self.structural_id(child)))
            .collect();
        let operator = node.operator.map_children(|c| children[&Rc::as_ptr(c)]);
        // MessagePack preserves non-finite floats and signed zero. HashMap
        // compares the complete byte key, so a hash collision cannot merge
        // different computations. Cloned/recomposed NaN literals retain identity.
        let encoded =
            rmp_serde::to_vec(&operator).expect("an IR operator over child IDs must serialize");
        let next = self.id_of_encoded_operator.len() as u64;
        let key = *self.id_of_encoded_operator.entry(encoded).or_insert(next);
        self.id_of_node
            .insert(Rc::as_ptr(node), (Rc::clone(node), key));
        key
    }
}
