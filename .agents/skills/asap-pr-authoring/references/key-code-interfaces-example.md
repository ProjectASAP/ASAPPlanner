# Example: Key code interfaces section

An excerpt in the expected layout: links per file group, real declarations, and a usage snippet.

## Key code interfaces

[`crates/types/src/ir/node.rs`](https://github.com/ORG/REPO/blob/<commit>/crates/types/src/ir/node.rs), [`scalar.rs`](https://github.com/ORG/REPO/blob/<commit>/crates/types/src/ir/scalar.rs)

```rust
/// One relational or ASAP operator; `C` is how it refers to its inputs
/// (`Rc` nodes in a DAG, `NodeId` in a flat DAG).            // new
pub enum Operator<C = Rc<OperatorNode>> { NonASAP(NonASAPOp<C>), ASAP(ASAPOp<C>) }
/// A per-row expression; it may read other operators' results through
/// subquery variants.                                         // changed: generic over C
pub enum ScalarExpr<C = Rc<OperatorNode>> { /* ScalarSubquery(C), Exists { subquery: C, .. }, ... */ }
/// What one query returns: a relation, or a single scalar.   // new
pub enum QueryRoot<C = Rc<OperatorNode>> { Operator(C), Scalar(ScalarExpr<C>) }

impl<C> Operator<C> {
    /// Every node this operator reads: its inputs first, then the nodes its
    /// scalar expressions reference. Used for traversal and CSE.
    pub fn children(&self) -> Vec<&C>;                          // new
    /// Rebuilds the operator with each child mapped, e.g. `Rc` to `NodeId`.
    pub fn map_children<D>(&self, f: impl FnMut(&C) -> D) -> Operator<D>;   // new
}

/// Why plan selection could not finish (distinct from a candidate being
/// rejected, which is recorded in `Selection.rejected`).
pub enum SelectionError {
    // ...
    /// Rebuilding a candidate with the sketch sizes its δ needs failed. An
    /// internal bug: resizing should never break a schema.
    Sizing { candidate: String, error: SizingError },          // new
}
```

[`crates/types/src/ir/flat.rs`](https://github.com/ORG/REPO/blob/<commit>/crates/types/src/ir/flat.rs)

```rust
pub type NodeId = usize;

pub struct FlatDag {
    pub nodes: Vec<FlatNode>,           // node i is nodes[i]; children before parents
    pub roots: Vec<QueryRoot<NodeId>>,  // one per input root, in order
}

/// Also returns the original `Rc` for each id.
pub fn flatten(roots: &[QueryRoot]) -> (FlatDag, Vec<Rc<OperatorNode>>);
```

Usage:

```rust
let (flat, originals) = flatten(&[QueryRoot::Operator(Rc::clone(&root))]);
let json = serde_json::to_string(&flat)?;
```
