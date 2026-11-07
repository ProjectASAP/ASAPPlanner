//! PromQL `limitk` and `limit_ratio`: a deterministic subset of each
//! group's series, their samples unchanged.
use super::*;
use planner_types::ir::operator::SampleKind;

impl Operator {
    /// The series of `input` (a PromQL vector) that `kind` keeps per group
    /// of the `groups` columns. `limitk(k)` keeps each group's first `k`
    /// series in label-set order, the order Prometheus reads series from
    /// its storage in. `limit_ratio(r)` keeps a series when its sampling
    /// offset ([`sampling_offset`]) is below `r` (for `r >= 0`) or at least
    /// `1 + r` (for `r < 0`), with `r` clamped to `[-1, 1]`, as Prometheus
    /// does; a NaN ratio is bad input.
    pub fn series_sample(
        input: SchemaRef,
        groups: Vec<usize>,
        kind: SampleKind,
    ) -> Result<Self, Error> {
        validate_groups(&input, &groups)?;
        super::series_labels::layout(&input)?;
        if matches!(kind, SampleKind::LimitRatio(ratio) if ratio.is_nan()) {
            return Err(invalid("limit_ratio ratio is NaN"));
        }
        Ok(Self {
            kind: Kind::SeriesSample { groups, kind },
            inputs: vec![input.clone()],
            output: input,
        })
    }
}

/// Prometheus' `limit_ratio` sampling offset of a series, in `[0, 1]`: the
/// xxHash64 of its label set in Prometheus' default (`stringlabels`)
/// encoding, over `u64::MAX`. That encoding writes each label, in name
/// order, as its name then its value, each prefixed by its length: one
/// byte below 255, else 255 and three little-endian bytes.
fn sampling_offset(labels: &super::series_labels::Labels) -> f64 {
    let mut encoded = Vec::new();
    for text in labels.iter().flat_map(|(name, value)| [name, value]) {
        let length = text.len();
        if length < 255 {
            encoded.push(length as u8);
        } else {
            encoded.push(255);
            encoded.extend_from_slice(&(length as u32).to_le_bytes()[..3]);
        }
        encoded.extend_from_slice(text.as_bytes());
    }
    xxhash_rust::xxh64::xxh64(&encoded, 0) as f64 / u64::MAX as f64
}

pub(super) fn execute<'a>(
    operator: &'a Operator,
    mut inputs: Vec<Input<'a, Batch>>,
    context: RunContext,
) -> Result<OutputStream<'a, Batch>, Error> {
    let Kind::SeriesSample { groups, kind } = &operator.kind else {
        unreachable!()
    };
    let input = inputs.pop().ok_or_else(|| invalid("input missing"))?;
    Ok(futures::stream::once(async move {
        let input = collect_batch(input, &operator.inputs[0], &context).await?;
        let batch = &input.batch;
        let mut workspace = Workspace::new(&context)?;
        let layout = super::series_labels::layout(&operator.inputs[0])?;
        let (source_of_row, sets) =
            super::series_labels::label_sources(batch, &layout, &operator.inputs[0])?;
        workspace.grow(sets.iter().map(super::series_labels::label_bytes).sum())?;
        let mut table = super::group_table::GroupTable::new(groups.clone());
        let group_of_row = table.group_ids(batch)?;
        workspace.grow(table.memory_bytes() + 8 * source_of_row.len())?;
        // Whether each label source (series) is kept.
        let kept: Vec<bool> = match *kind {
            SampleKind::LimitK(k) => {
                // Each group's series, in label-set order.
                let mut series = std::collections::BTreeSet::new();
                for (&group, &source) in group_of_row.iter().zip(&source_of_row) {
                    series.insert((group, &sets[source as usize], source));
                }
                let mut kept = vec![false; sets.len()];
                let mut taken = vec![0usize; table.len()];
                for (group, _, source) in series {
                    if taken[group as usize] < k {
                        taken[group as usize] += 1;
                        kept[source as usize] = true;
                    }
                }
                kept
            }
            SampleKind::LimitRatio(ratio) => {
                let ratio = ratio.clamp(-1.0, 1.0);
                sets.iter()
                    .map(|labels| {
                        let offset = sampling_offset(labels);
                        match ratio {
                            0.0 => false,
                            ratio if ratio > 0.0 => offset < ratio,
                            ratio => offset >= 1.0 + ratio,
                        }
                    })
                    .collect()
            }
        };
        let keep = source_of_row
            .iter()
            .map(|&source| Some(kept[source as usize]))
            .collect::<arrow::array::BooleanArray>();
        batch.filter(&keep)
    })
    .boxed_local())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sampling offset is Prometheus' (`stringlabels`) label hash over
    /// `u64::MAX`: `{__name__="m", instance="a"}` encodes as
    /// `\x08__name__\x01m\x08instance\x01a`.
    #[test]
    fn sampling_offset_hashes_the_prometheus_label_encoding() {
        let labels = super::super::series_labels::Labels::from([
            ("__name__".to_string(), "m".to_string()),
            ("instance".to_string(), "a".to_string()),
        ]);
        let encoded = b"\x08__name__\x01m\x08instance\x01a";
        assert_eq!(
            sampling_offset(&labels),
            xxhash_rust::xxh64::xxh64(encoded, 0) as f64 / u64::MAX as f64
        );
        // A 300-byte value takes the 4-byte length prefix.
        let long = "v".repeat(300);
        let labels = super::super::series_labels::Labels::from([("a".to_string(), long.clone())]);
        let mut encoded = b"\x01a\xff\x2c\x01\x00".to_vec();
        encoded.extend(long.as_bytes());
        assert_eq!(
            sampling_offset(&labels),
            xxhash_rust::xxh64::xxh64(&encoded, 0) as f64 / u64::MAX as f64
        );
    }
}
