//! Compressed samples: every sample of one series in one tumbling window,
//! as one sealed block of the run's [`SampleBlockCodec`] (design doc
//! `compressed-sample-windows.md`). Exact: a `DecodeSamplesInRange` decodes
//! the blocks back into the raw sample rows, and the exact operators answer
//! from them.
use crate::summary_kernels::sample_blocks::SampleBlockCodec;
use crate::summary_kernels::traits::{aggregate_core_object_methods, downcast_peer};
use crate::{AggregateCore, KernelError};
use std::sync::Arc;

/// Bytes a sealed state column holds per state besides its block: the
/// block's offset in the column's byte buffer.
pub(crate) const BLOCK_OFFSET_BYTES: usize = 8;

/// One series' samples in one window: a sealed block and the codec that
/// wrote it.
#[derive(Debug, Clone)]
pub struct CompressedSamplesAccumulator {
    block: Arc<[u8]>,
    codec: Arc<dyn SampleBlockCodec>,
}

impl CompressedSamplesAccumulator {
    /// The state of the sealed `block`, which `codec` wrote.
    pub(crate) fn new(block: Arc<[u8]>, codec: Arc<dyn SampleBlockCodec>) -> Self {
        Self { block, codec }
    }

    /// The sealed block's bytes, which a deployment persists as they are.
    pub fn block(&self) -> &[u8] {
        &self.block
    }

    /// The codec that wrote the block.
    pub fn codec(&self) -> &Arc<dyn SampleBlockCodec> {
        &self.codec
    }

    /// The block's samples with `start_exclusive_ms < t <=
    /// end_inclusive_ms`, in increasing time.
    pub fn samples_in(
        &self,
        start_exclusive_ms: i64,
        end_inclusive_ms: i64,
    ) -> Result<Vec<(i64, f64)>, KernelError> {
        self.codec
            .samples_in(&self.block, start_exclusive_ms, end_inclusive_ms)
    }

    /// The state restored from a block read with [`Self::block`] and
    /// persisted, with the codec that wrote it. An error when `codec` does
    /// not read the bytes.
    pub fn restore_from_disk(
        block: Vec<u8>,
        codec: Arc<dyn SampleBlockCodec>,
    ) -> Result<Self, KernelError> {
        codec.decode(&block)?;
        Ok(Self {
            block: block.into(),
            codec,
        })
    }
}

impl AggregateCore for CompressedSamplesAccumulator {
    aggregate_core_object_methods!();

    /// The sealed block of both blocks' samples, the other's value winning
    /// on an equal timestamp: exact whether the blocks' spans overlap or not.
    fn merge_with(&self, other: &dyn AggregateCore) -> Result<Box<dyn AggregateCore>, KernelError> {
        let other = downcast_peer::<Self>(
            other,
            "compressed samples merge only with compressed samples",
        )?;
        Ok(Box::new(Self {
            block: self.codec.merge(&self.block, &other.block)?.into(),
            codec: self.codec.clone(),
        }))
    }

    fn is_empty(&self) -> bool {
        self.codec
            .decode(&self.block)
            .is_ok_and(|samples| samples.is_empty())
    }

    /// The block's bytes and its offset in a sealed column: what the cost
    /// model prices a sealed state at.
    fn approx_memory_bytes(&self) -> usize {
        self.block.len() + BLOCK_OFFSET_BYTES
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::summary_kernels::sample_blocks::PlainSampleBlockCodec;

    // A merge holds both blocks' samples in time order.
    #[test]
    fn merge_holds_both_blocks_samples() {
        let codec: Arc<dyn SampleBlockCodec> = Arc::new(PlainSampleBlockCodec);
        let state = |samples: &[(i64, f64)]| {
            let mut block = codec.new_block();
            for &(t, v) in samples {
                block.append(t, v);
            }
            CompressedSamplesAccumulator::new(block.seal().into(), codec.clone())
        };
        let merged = state(&[(1_000, 1.5), (3_000, 3.5)])
            .merge_with(&state(&[(2_000, 2.5)]))
            .unwrap();
        let merged = merged
            .as_any()
            .downcast_ref::<CompressedSamplesAccumulator>()
            .unwrap();
        assert_eq!(
            merged.samples_in(i64::MIN, i64::MAX).unwrap(),
            [(1_000, 1.5), (2_000, 2.5), (3_000, 3.5)]
        );
        assert_eq!(merged.approx_memory_bytes(), merged.block().len() + 8);
    }
}
