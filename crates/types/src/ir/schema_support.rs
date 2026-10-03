//! Series-identity realization for the unified dag.
use crate::pre_asap::schema::*;
/// Resolve a PromQL root to rows carrying [`PROMQL_SERIES_IDENTITY`] before
/// candidate search. `closed` describes physical columns here: the final
/// column contains every dynamic source label. It does not assert that the
/// query's projected labels are the full label set.
///
/// This realization supports explicit `by` grouping and per-series computation.
/// Operators that rewrite or implicitly match dynamic label sets require their
/// own realization; they must not accidentally treat the opaque identity as a
/// user label or silently discard it.
pub fn with_promql_series_identity(
    root: &std::rc::Rc<crate::ir::OperatorNode>,
) -> Result<std::rc::Rc<crate::ir::OperatorNode>, String> {
    use crate::ir::{NonASAPOp, Operator, OperatorNode};
    use crate::pre_asap::Source;
    use std::{collections::HashMap, rc::Rc};
    fn visit(
        node: &Rc<OperatorNode>,
        memo: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
    ) -> Result<Rc<OperatorNode>, String> {
        if let Some(found) = memo.get(&Rc::as_ptr(node)) {
            return Ok(Rc::clone(found));
        }
        let mut error = None;
        let mut operator = node
            .operator
            .map_children(|child| match visit(child, memo) {
                Ok(child) => child,
                Err(e) => {
                    error = Some(e);
                    Rc::clone(child)
                }
            });
        if let Some(error) = error {
            return Err(error);
        }
        match &mut operator {
            Operator::NonASAP(NonASAPOp::Scan {
                source: Source::TimeSeries { .. },
                schema,
                ..
            }) => {
                if schema
                    .fields
                    .iter()
                    .any(|field| field.name == PROMQL_SERIES_IDENTITY)
                {
                    if !schema.has_promql_series_identity() {
                        return Err("invalid physical series identity".into());
                    }
                    memo.insert(Rc::as_ptr(node), Rc::clone(node));
                    return Ok(Rc::clone(node));
                }
                if schema.closed {
                    return Err("dynamic series identity requires an open PromQL source".into());
                }
                schema.fields.push(Field::new(
                    PROMQL_SERIES_IDENTITY,
                    FieldDataType::Plain(DataType::Utf8),
                    false,
                ));
                schema.closed = true;
            }
            Operator::NonASAP(NonASAPOp::Sort { partition_by, .. })
                if partition_by.is_without() =>
            {
                return Err("dynamic without ranking requires label-set projection".into());
            }
            Operator::NonASAP(
                NonASAPOp::TimeRange { .. }
                | NonASAPOp::Limit { .. }
                | NonASAPOp::Project { .. }
                | NonASAPOp::Filter { .. }
                | NonASAPOp::TimeShift { .. }
                | NonASAPOp::PromqlSubquery { .. }
                | NonASAPOp::PromqlRelabel { .. }
                | NonASAPOp::PromqlVectorFromScalar(_)
                | NonASAPOp::BinaryOp { .. }
                | NonASAPOp::Concat { .. }
                | NonASAPOp::Aggregate { .. }
                | NonASAPOp::Sort { .. },
            ) => {}
            _ => return Err("operator has no dynamic series-identity realization".into()),
        }
        let mut rebuilt = OperatorNode::new(operator).map_err(|e| e.to_string())?;
        rebuilt.guarantee = node.guarantee.clone();
        rebuilt.timing = node.timing;
        let rebuilt = Rc::new(rebuilt);
        memo.insert(Rc::as_ptr(node), Rc::clone(&rebuilt));
        Ok(rebuilt)
    }
    visit(root, &mut HashMap::new())
}
