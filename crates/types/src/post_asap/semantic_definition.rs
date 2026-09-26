//! Persistable dependency closure using Planner's typed operation vocabulary.
//! Node hashes are local semantic references, not executable or deployed IDs.
use crate::post_asap::{
    EdgeRole, ExecutableDag, ExecutableOperatorPayload, PostAsapNodeId, SummarySchema,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummarySemanticFragment {
    pub format_version: u32,
    pub output: String,
    pub nodes: BTreeMap<String, SemanticOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticOperation {
    // Wire ownership must be Send + Sync. These values are checked against the
    // Planner types on export and on recovery; arbitrary JSON is not accepted.
    pub operation: serde_json::Value,
    pub output_schema: serde_json::Value,
    pub inputs: Vec<SemanticInput>,
    /// The direct input range is supplied by the stored record, not by a query lookback.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub record_range: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticInput {
    pub role: EdgeRole,
    pub node: String,
}

pub(crate) fn canonical_bytes(value: &impl Serialize) -> Result<Vec<u8>, String> {
    fn canonical(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(values) => serde_json::Value::Object(
                values
                    .into_iter()
                    .map(|(k, v)| (k, canonical(v)))
                    .collect::<BTreeMap<_, _>>()
                    .into_iter()
                    .collect(),
            ),
            serde_json::Value::Array(values) => {
                serde_json::Value::Array(values.into_iter().map(canonical).collect())
            }
            value => value,
        }
    }
    serde_json::to_vec(&canonical(
        serde_json::to_value(value).map_err(|e| e.to_string())?,
    ))
    .map_err(|e| e.to_string())
}
fn hash(value: &impl Serialize) -> Result<String, String> {
    Ok(format!("{:x}", Sha256::digest(canonical_bytes(value)?)))
}
fn role(role: EdgeRole) -> u8 {
    match role {
        EdgeRole::Input => 0,
        EdgeRole::Left => 1,
        EdgeRole::Right => 2,
    }
}

impl SummarySemanticFragment {
    pub fn from_stored_output(dag: &ExecutableDag, output: PostAsapNodeId) -> Result<Self, String> {
        Self::export(dag, output, true)
    }

    pub fn from_dag(dag: &ExecutableDag, output: PostAsapNodeId) -> Result<Self, String> {
        Self::export(dag, output, false)
    }

    fn export(
        dag: &ExecutableDag,
        output: PostAsapNodeId,
        parameterize_range: bool,
    ) -> Result<Self, String> {
        let mut included = BTreeSet::new();
        let mut pending = vec![output];
        while let Some(id) = pending.pop() {
            if included.insert(id) {
                pending.extend(
                    dag.edges
                        .iter()
                        .filter(|e| e.consumer == id)
                        .map(|e| e.producer),
                );
            }
        }
        let dag = ExecutableDag {
            nodes: dag
                .nodes
                .iter()
                .filter(|n| included.contains(&n.id))
                .cloned()
                .collect(),
            edges: dag
                .edges
                .iter()
                .filter(|e| included.contains(&e.consumer))
                .cloned()
                .collect(),
            root: output,
        };
        dag.validate().map_err(|e| e.to_string())?;
        if dag.nodes.len() > 4096 {
            return Err("semantic fragment exceeds node budget".into());
        }
        // Open PromQL entities carry all labels. Nullable label columns demanded
        // only by a downstream consumer do not change a per-entity scalar state.
        let mut dag = dag;
        let sample_only = matches!(
            &dag.nodes
                .iter()
                .find(|n| n.id == output)
                .ok_or("missing output")?
                .payload,
            ExecutableOperatorPayload::SummaryAgg {
                reduction: crate::pre_asap::Reduction::PerEntity,
                grouping: crate::post_asap::GroupingStrategy::PerSubpopulationInstance,
                input: crate::post_asap::SummaryUpdate {
                    item: None,
                    weight: crate::post_asap::SummaryInputExpr::Column(
                        crate::pre_asap::ColumnRef::SampleValue
                    ),
                    ..
                },
                ..
            }
        );
        if parameterize_range && sample_only {
            let direct: BTreeSet<_> = dag
                .edges
                .iter()
                .filter(|e| e.consumer == output)
                .map(|e| e.producer)
                .collect();
            let mut normalized = false;
            for node in &mut dag.nodes {
                if !direct.contains(&node.id) {
                    continue;
                }
                if let ExecutableOperatorPayload::Fallback { expression } = &mut node.payload {
                    let source = match expression {
                        crate::pre_asap::QueryExpr::TimeRange { child, .. } => {
                            std::rc::Rc::make_mut(child)
                        }
                        other => other,
                    };
                    if let crate::pre_asap::QueryExpr::Scan {
                        source: crate::pre_asap::Source::TimeSeries { .. },
                        predicates,
                        schema,
                    } = source
                    {
                        if !schema.closed
                            && predicates.is_empty()
                            && schema.unique_keys.is_empty()
                            && schema
                                .columns
                                .iter()
                                .take_while(|c| {
                                    !(c.nullable && c.dtype == crate::pre_asap::DataType::Utf8)
                                })
                                .count()
                                + schema
                                    .columns
                                    .iter()
                                    .rev()
                                    .take_while(|c| {
                                        c.nullable && c.dtype == crate::pre_asap::DataType::Utf8
                                    })
                                    .count()
                                == schema.columns.len()
                        {
                            schema.columns.retain(|c| {
                                !(c.nullable && c.dtype == crate::pre_asap::DataType::Utf8)
                            });
                            node.output_schema.fields.retain(|c| {
                                !(c.nullable
                                    && c.dtype
                                        == crate::post_asap::SummaryFamilyType::Plain(
                                            crate::pre_asap::DataType::Utf8,
                                        ))
                            });
                            normalized = true;
                        }
                    }
                }
            }
            if normalized {
                dag.nodes
                    .iter_mut()
                    .find(|n| n.id == output)
                    .unwrap()
                    .output_schema
                    .fields
                    .retain(|c| {
                        !(c.nullable
                            && c.dtype
                                == crate::post_asap::SummaryFamilyType::Plain(
                                    crate::pre_asap::DataType::Utf8,
                                ))
                    });
            }
        }
        let nodes: BTreeMap<_, _> = dag.nodes.iter().map(|n| (n.id, n)).collect();
        let mut hashes: BTreeMap<PostAsapNodeId, String> = BTreeMap::new();
        let mut result = Self {
            format_version: 1,
            output: String::new(),
            nodes: BTreeMap::new(),
        };
        let mut stack = vec![(output, false)];
        while let Some((id, finish)) = stack.pop() {
            if hashes.contains_key(&id) {
                continue;
            }
            let node = nodes.get(&id).ok_or("missing semantic output")?;
            let edges: Vec<_> = dag.edges.iter().filter(|e| e.consumer == id).collect();
            if !finish {
                stack.push((id, true));
                for edge in &edges {
                    stack.push((edge.producer, false));
                }
                continue;
            }
            let mut inputs = edges
                .iter()
                .map(|e| SemanticInput {
                    role: e.role,
                    node: hashes[&e.producer].clone(),
                })
                .collect::<Vec<_>>();
            inputs.sort_by(|a, b| (role(a.role), &a.node).cmp(&(role(b.role), &b.node)));
            let mut payload = node.payload.clone();
            let mut record_range = false;
            if parameterize_range
                && matches!(
                    nodes[&output].payload,
                    ExecutableOperatorPayload::SummaryAgg { .. }
                )
                && dag
                    .edges
                    .iter()
                    .any(|e| e.consumer == output && e.producer == id)
            {
                if let ExecutableOperatorPayload::Fallback {
                    expression: crate::pre_asap::QueryExpr::TimeRange { child, .. },
                } = &payload
                {
                    payload = ExecutableOperatorPayload::Fallback {
                        expression: child.as_ref().clone(),
                    };
                    record_range = true;
                }
            }
            if let ExecutableOperatorPayload::RelationalJoin { pruning, .. } = &mut payload {
                *pruning = None;
            }
            let operation = SemanticOperation {
                record_range,
                operation: serde_json::to_value(&payload).map_err(|e| e.to_string())?,
                output_schema: serde_json::to_value(&node.output_schema)
                    .map_err(|e| e.to_string())?,
                inputs,
            };
            let key = hash(&operation)?;
            result.nodes.insert(key.clone(), operation);
            hashes.insert(id, key);
        }
        result.output = hashes
            .remove(&output)
            .ok_or("missing semantic output hash")?;
        result.validate()?;
        Ok(result)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.format_version != 1
            || self.nodes.is_empty()
            || self.nodes.len() > 4096
            || canonical_bytes(self)?.len() > 4 * 1024 * 1024
        {
            return Err("unsupported semantic fragment version or size".into());
        }
        for (key, node) in &self.nodes {
            let payload: ExecutableOperatorPayload =
                serde_json::from_value(node.operation.clone()).map_err(|e| e.to_string())?;
            if node.record_range {
                let root = self
                    .nodes
                    .get(&self.output)
                    .ok_or("missing semantic root")?;
                let root_payload: ExecutableOperatorPayload =
                    serde_json::from_value(root.operation.clone()).map_err(|e| e.to_string())?;
                if !matches!(payload, ExecutableOperatorPayload::Fallback { .. })
                    || !matches!(root_payload, ExecutableOperatorPayload::SummaryAgg { .. })
                    || !root.inputs.iter().any(|input| &input.node == key)
                {
                    return Err("record range must belong to a direct summary input".into());
                }
            }
            let _: SummarySchema =
                serde_json::from_value(node.output_schema.clone()).map_err(|e| e.to_string())?;
            if hash(node)? != *key
                || node
                    .inputs
                    .iter()
                    .any(|i| !self.nodes.contains_key(&i.node))
            {
                return Err("semantic fragment hash or dependency mismatch".into());
            }
            if node
                .inputs
                .windows(2)
                .any(|p| (role(p[0].role), &p[0].node) > (role(p[1].role), &p[1].node))
            {
                return Err("noncanonical semantic input order".into());
            }
        }
        let mut seen = BTreeSet::new();
        let mut stack = vec![self.output.as_str()];
        while let Some(id) = stack.pop() {
            let node = self.nodes.get(id).ok_or("missing semantic fragment root")?;
            if seen.insert(id) {
                stack.extend(node.inputs.iter().map(|i| i.node.as_str()));
            }
        }
        if seen.len() != self.nodes.len() {
            return Err("unrelated semantic fragment nodes".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::post_asap::{compile_executable_dag, SummaryExpr, SummaryNode};
    use crate::pre_asap::{Column, DataType, QueryExpr, Schema, Source};
    use std::rc::Rc;

    fn fixture(metric: &str) -> ExecutableDag {
        let scan = QueryExpr::Scan {
            source: Source::TimeSeries {
                metric: metric.into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![Column::new("value", DataType::Float64, false)]),
        };
        let schema = SummarySchema {
            fields: vec![crate::post_asap::SummaryField {
                name: "value".into(),
                dtype: crate::post_asap::SummaryFamilyType::Plain(DataType::Float64),
                nullable: false,
            }],
            time_index: None,
        };
        compile_executable_dag(&Rc::new(SummaryNode {
            expr: SummaryExpr::KeepPreAsap(Rc::new(scan)),
            schema,
            guarantee: None,
        }))
        .unwrap()
    }

    // Storage identity must ignore temporary identifiers and execution placement.
    #[test]
    fn identity_ignores_node_ids_and_phase() {
        let dag = fixture("latency");
        let expected = SummarySemanticFragment::from_dag(&dag, dag.root).unwrap();
        let mut other = dag.clone();
        other.root = PostAsapNodeId(71);
        other.nodes[0].id = other.root;
        other.nodes[0].output_state.timing = crate::post_asap::ExecutionTiming::IngestionTime;
        assert_eq!(
            canonical_bytes(&expected).unwrap(),
            canonical_bytes(&SummarySemanticFragment::from_dag(&other, other.root).unwrap())
                .unwrap()
        );
    }

    // Source identity and supported semantic format survive restart independently.
    #[test]
    fn semantics_roundtrip_and_reject_unknown_version() {
        let a = fixture("latency");
        let b = fixture("bytes");
        let a = SummarySemanticFragment::from_dag(&a, a.root).unwrap();
        let b = SummarySemanticFragment::from_dag(&b, b.root).unwrap();
        assert_ne!(canonical_bytes(&a).unwrap(), canonical_bytes(&b).unwrap());
        let mut restored: SummarySemanticFragment =
            serde_json::from_slice(&canonical_bytes(&a).unwrap()).unwrap();
        restored.validate().unwrap();
        restored.format_version += 1;
        assert!(restored.validate().is_err());
    }
    // A transformed value cannot share state identity with its source column.
    #[test]
    fn value_expression_is_semantic_and_nonfinite_constants_are_rejected() {
        use crate::pre_asap::{ProjectItem, ScalarValue};
        let original = fixture("latency");
        let expected = SummarySemanticFragment::from_dag(&original, original.root).unwrap();
        let mut transformed = original.clone();
        let ExecutableOperatorPayload::Fallback { expression } = &mut transformed.nodes[0].payload
        else {
            unreachable!()
        };
        *expression = QueryExpr::Project {
            cols: vec![ProjectItem {
                alias: Some("value".into()),
                expr: QueryExpr::FunctionCall {
                    name: "ln".into(),
                    args: vec![QueryExpr::Column(0)],
                },
            }],
            qualifier: None,
            child: Rc::new(expression.clone()),
        };
        let logged = SummarySemanticFragment::from_dag(&transformed, transformed.root).unwrap();
        assert_ne!(
            canonical_bytes(&expected).unwrap(),
            canonical_bytes(&logged).unwrap()
        );
        let ExecutableOperatorPayload::Fallback {
            expression: QueryExpr::Project { cols, .. },
        } = &mut transformed.nodes[0].payload
        else {
            unreachable!()
        };
        cols[0].expr = QueryExpr::Literal(ScalarValue::Float64(f64::NAN));
        assert!(SummarySemanticFragment::from_dag(&transformed, transformed.root).is_err());
    }

    // Changing a downstream consumer cannot change the persisted input definition.
    #[test]
    fn only_output_dependency_closure_is_exported() {
        let mut dag = fixture("latency");
        let stored = dag.root;
        let mut consumer = dag.nodes[0].clone();
        consumer.id = PostAsapNodeId(9);
        consumer.payload = ExecutableOperatorPayload::Value {
            operation: crate::post_asap::ValueOperation::Project {
                cols: vec![],
                qualifier: None,
            },
        };
        dag.edges.push(crate::post_asap::ExecutableDagEdge {
            producer: stored,
            consumer: consumer.id,
            role: EdgeRole::Input,
            intermediate_schema: dag.nodes[0].output_schema.clone(),
            data_state: dag.nodes[0].output_state,
            grouping: crate::post_asap::GroupingEdgeCompatibility::NotApplicable,
            window: crate::post_asap::WindowEdgeCompatibility::NotApplicable,
        });
        dag.root = consumer.id;
        dag.nodes.push(consumer);
        let before = SummarySemanticFragment::from_dag(&dag, stored).unwrap();
        dag.nodes.reverse();
        let after = SummarySemanticFragment::from_dag(&dag, stored).unwrap();
        assert_eq!(before, after);
        assert_eq!(before.nodes.len(), 1);
    }
    // Query lookback does not become the identity of each stored input pane.
    #[test]
    fn stored_input_range_is_parameterized_but_logical_range_is_preserved() {
        use crate::post_asap::*;
        use crate::pre_asap::{ColumnRef, Reduction};
        let make = |seconds| {
            let mut dag = fixture("latency");
            let ExecutableOperatorPayload::Fallback { expression } = &mut dag.nodes[0].payload
            else {
                unreachable!()
            };
            *expression = QueryExpr::TimeRange {
                range: std::time::Duration::from_secs(seconds),
                child: Rc::new(expression.clone()),
            };
            let mut output = dag.nodes[0].clone();
            output.id = PostAsapNodeId(1);
            let family = SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
            output.payload = ExecutableOperatorPayload::SummaryAgg {
                family: family.clone(),
                input: SummaryUpdate {
                    item: None,
                    weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
                    weight_domain: Default::default(),
                },
                reduction: Reduction::PerEntity,
                grouping: Default::default(),
            };
            output.output_schema.fields[0].dtype = family;
            output.output_state.primitive = DataPrimitive::SummaryState;
            dag.edges.push(ExecutableDagEdge {
                producer: dag.root,
                consumer: output.id,
                role: EdgeRole::Input,
                intermediate_schema: dag.nodes[0].output_schema.clone(),
                data_state: dag.nodes[0].output_state,
                grouping: GroupingEdgeCompatibility::NotApplicable,
                window: WindowEdgeCompatibility::NotApplicable,
            });
            dag.root = output.id;
            dag.nodes.push(output);
            dag
        };
        let one = make(60);
        let five = make(300);
        assert_ne!(
            SummarySemanticFragment::from_dag(&one, one.root).unwrap(),
            SummarySemanticFragment::from_dag(&five, five.root).unwrap()
        );
        assert_eq!(
            SummarySemanticFragment::from_stored_output(&one, one.root).unwrap(),
            SummarySemanticFragment::from_stored_output(&five, five.root).unwrap()
        );
        // Open entities retain their full label identity; consumer-demanded
        // optional labels do not change the per-entity stored computation.
        let mut open = one.clone();
        let ExecutableOperatorPayload::Fallback {
            expression: QueryExpr::TimeRange { child, .. },
        } = &mut open.nodes[0].payload
        else {
            unreachable!()
        };
        let QueryExpr::Scan { schema, .. } = Rc::make_mut(child) else {
            unreachable!()
        };
        schema.closed = false;
        let expected = SummarySemanticFragment::from_stored_output(&open, open.root).unwrap();
        let ExecutableOperatorPayload::Fallback {
            expression: QueryExpr::TimeRange { child, .. },
        } = &mut open.nodes[0].payload
        else {
            unreachable!()
        };
        let QueryExpr::Scan { schema, .. } = Rc::make_mut(child) else {
            unreachable!()
        };
        schema
            .columns
            .push(Column::new("job", DataType::Utf8, true));
        let label = SummaryField {
            name: "job".into(),
            dtype: SummaryFamilyType::Plain(DataType::Utf8),
            nullable: true,
        };
        for node in &mut open.nodes {
            node.output_schema.fields.push(label.clone());
        }
        open.edges[0].intermediate_schema.fields.push(label);
        assert_eq!(
            expected,
            SummarySemanticFragment::from_stored_output(&open, open.root).unwrap()
        );
        let mut forged = SummarySemanticFragment::from_stored_output(&one, one.root).unwrap();
        forged.nodes.values_mut().next().unwrap().operation =
            serde_json::json!({"kind": "unknown"});
        assert!(forged.validate().is_err());
    }
    // Persisted semantic format changes require an explicit migration/version review.
    #[test]
    fn semantic_format_v1_has_stable_wire_identity() {
        let dag = fixture("latency");
        let exported = SummarySemanticFragment::from_dag(&dag, dag.root).unwrap();
        assert_eq!(
            exported.output,
            "488a0550f37763997397403ae5ed3588dee5d2a09fa7840f4110b775095fe594"
        );
    }
}
