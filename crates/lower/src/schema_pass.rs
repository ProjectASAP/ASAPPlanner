use std::rc::Rc;

use asap_control_core::intent_algebra::expr::QueryExpr;
use asap_control_core::intent_algebra::schema::{HasSchema, L3Schema, SchemaCatalog};
use asap_control_core::intent_algebra::L3Node;

/// Recursively populate the `schema` field on every node in a `QueryExpr` tree.
///
/// The lowerer creates every node with an empty schema (`make_node`). This
/// pass walks the tree bottom-up, computing each node's output schema from
/// its children's schemas and the catalog, and returns a fully typed
/// `Rc<L3Node>` tree.
pub fn populate_schemas(expr: QueryExpr, catalog: &SchemaCatalog) -> Rc<L3Node> {
    let (rebuilt, child_schemas) = rebuild(expr, catalog);
    let refs: Vec<&L3Schema> = child_schemas.iter().collect();
    let schema = rebuilt.output_schema(&refs, catalog);
    Rc::new(L3Node { expr: rebuilt, schema })
}

/// Recursively rebuild the expression tree with populated child nodes.
/// Returns `(rebuilt_expr, child_output_schemas)` so the caller can pass
/// those schemas to `output_schema`.
fn rebuild(expr: QueryExpr, catalog: &SchemaCatalog) -> (QueryExpr, Vec<L3Schema>) {
    use QueryExpr::*;

    // Helper: process one child Rc<L3Node> → fresh Rc<L3Node> with schema set.
    let proc = |node: Rc<L3Node>| populate_schemas(node.expr.clone(), catalog);

    match expr {
        // Leaf: schema comes from the catalog, no child schemas needed.
        Scan { .. } => (expr, vec![]),

        Filter { child, pred } => {
            let c = proc(child);
            let cs = c.schema.clone();
            (Filter { child: c, pred }, vec![cs])
        }
        Project { child, cols } => {
            let c = proc(child);
            let cs = c.schema.clone();
            (Project { child: c, cols }, vec![cs])
        }
        Aggregate { child, by, aggs, having } => {
            let c = proc(child);
            let cs = c.schema.clone();
            (Aggregate { child: c, by, aggs, having }, vec![cs])
        }
        Sort { child, keys } => {
            let c = proc(child);
            let cs = c.schema.clone();
            (Sort { child: c, keys }, vec![cs])
        }
        Limit { child, n, offset } => {
            let c = proc(child);
            let cs = c.schema.clone();
            (Limit { child: c, n, offset }, vec![cs])
        }
        Distinct { child, cols } => {
            let c = proc(child);
            let cs = c.schema.clone();
            (Distinct { child: c, cols }, vec![cs])
        }
        Partition { child, keys } => {
            let c = proc(child);
            let cs = c.schema.clone();
            (Partition { child: c, keys }, vec![cs])
        }
        TimeWindow { child, kind, size, slide } => {
            let c = proc(child);
            let cs = c.schema.clone();
            (TimeWindow { child: c, kind, size, slide }, vec![cs])
        }
        WindowFunc { child, func, args, partition_by, order_by, frame } => {
            let c = proc(child);
            let cs = c.schema.clone();
            (WindowFunc { child: c, func, args, partition_by, order_by, frame }, vec![cs])
        }
        SetOp { kind, all, left, right } => {
            let l = proc(left);
            let r = proc(right);
            let ls = l.schema.clone();
            let rs = r.schema.clone();
            (SetOp { kind, all, left: l, right: r }, vec![ls, rs])
        }
        Merge { children } => {
            let new_children: Vec<Rc<L3Node>> =
                children.into_iter().map(proc).collect();
            let schemas: Vec<L3Schema> = new_children.iter().map(|c| c.schema.clone()).collect();
            (Merge { children: new_children }, schemas)
        }
        // Unimplemented variants: return as-is; output_schema will todo!() if called.
        other => (other, vec![]),
    }
}
