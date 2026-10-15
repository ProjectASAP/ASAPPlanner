# Example: Key code interfaces section

An excerpt in the expected layout: links per file group, real declarations, and a usage snippet.

## Key code interfaces

[`crates/types/src/ir/node.rs`](https://github.com/ORG/REPO/blob/<commit>/crates/types/src/ir/node.rs), [`scalar.rs`](https://github.com/ORG/REPO/blob/<commit>/crates/types/src/ir/scalar.rs)

```rust
pub enum Operator<C = Rc<OperatorNode>> { NonASAP(NonASAPOp<C>), ASAP(ASAPOp<C>) }
pub enum ScalarExpr<C = Rc<OperatorNode>> { /* ScalarSubquery(C), Exists { subquery: C, .. }, ... */ }
pub enum QueryRoot<C = Rc<OperatorNode>> { Operator(C), Scalar(ScalarExpr<C>) }

impl<C> Operator<C> {
    pub fn children(&self) -> Vec<&C>;   // operator inputs, then scalar-referenced nodes
    pub fn map_children<D>(&self, f: impl FnMut(&C) -> D) -> Operator<D>;
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
