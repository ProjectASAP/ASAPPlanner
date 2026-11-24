//! The byte form of a compressed-samples state: one series' samples in one
//! window, as an opaque block a deployment's codec writes and reads
//! (design doc `compressed-sample-windows.md`). The executor builds, seals,
//! merges and decodes blocks only through [`SampleBlockCodec`]; a run takes
//! the deployment's codec from its context
//! ([`RunContext::with_sample_block_codec`](crate::runtime::RunContext::with_sample_block_codec)),
//! and [`PlainSampleBlockCodec`], the uncompressed reference, otherwise.
use crate::KernelError;

/// A deployment's encoding of sample blocks. A sealed block is the bytes
/// [`OpenSampleBlock::seal`] returns: what a sealed state column holds, what
/// a deployment persists, and what [`Self::decode`] reads back. Every
/// method is exact: a block decodes to exactly the samples appended to it.
pub trait SampleBlockCodec: Send + Sync + std::fmt::Debug {
    /// An empty open block, which the build appends a window's samples to.
    fn new_block(&self) -> Box<dyn OpenSampleBlock>;

    /// Every sample of the sealed `block`, `(timestamp_ms, value)` in
    /// increasing time. An error for bytes this codec did not write.
    fn decode(&self, block: &[u8]) -> Result<Vec<(i64, f64)>, KernelError>;

    /// The samples of `block` with `start_exclusive_ms < t <=
    /// end_inclusive_ms`, in increasing time. The default decodes the whole
    /// block; a codec may skip what it can.
    fn samples_in(
        &self,
        block: &[u8],
        start_exclusive_ms: i64,
        end_inclusive_ms: i64,
    ) -> Result<Vec<(i64, f64)>, KernelError> {
        let mut samples = self.decode(block)?;
        samples.retain(|&(t, _)| start_exclusive_ms < t && t <= end_inclusive_ms);
        Ok(samples)
    }

    /// The sealed block of both blocks' samples, in increasing time; on an
    /// equal timestamp `newer`'s value wins. Used for shards of one window.
    fn merge(&self, older: &[u8], newer: &[u8]) -> Result<Vec<u8>, KernelError> {
        let mut block = self.new_block();
        for (t, value) in self.decode(older)?.into_iter().chain(self.decode(newer)?) {
            block.append(t, value);
        }
        Ok(block.seal())
    }
}

/// A block being built: the samples of one series' window so far.
pub trait OpenSampleBlock: Send {
    /// Adds a sample. One older than the newest is inserted in time order;
    /// one at a timestamp already present replaces its value (the last
    /// write wins), so arrival order never changes the sealed block.
    fn append(&mut self, timestamp_ms: i64, value: f64);

    /// The number of samples.
    fn len(&self) -> usize;

    /// Whether the block holds no sample.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes the open block holds now, charged to the run building it.
    fn memory_bytes(&self) -> usize;

    /// The sealed block of the samples so far. Not consuming: it is also
    /// the open window's snapshot an unsealed-window read receives.
    fn seal(&self) -> Vec<u8>;
}

/// The reference codec: uncompressed. A block is the sample count (`u32`,
/// little-endian), then each sample's timestamp (`i64`) and value bits
/// (`u64`), little-endian, in increasing time: `4 + 16 n` bytes. The
/// Planner's own tests and runtimes use it; a deployment supplies its own
/// compressed codec.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlainSampleBlockCodec;

/// An open block of [`PlainSampleBlockCodec`]: the samples, in time order.
#[derive(Debug, Clone, Default)]
struct PlainOpenBlock {
    samples: Vec<(i64, f64)>,
}

impl SampleBlockCodec for PlainSampleBlockCodec {
    fn new_block(&self) -> Box<dyn OpenSampleBlock> {
        Box::new(PlainOpenBlock::default())
    }

    fn decode(&self, block: &[u8]) -> Result<Vec<(i64, f64)>, KernelError> {
        let (count, body) = block
            .split_first_chunk::<4>()
            .ok_or("a plain sample block starts with its sample count")?;
        let count = u32::from_le_bytes(*count) as usize;
        if body.len() != count * 16 {
            return Err("a plain sample block's length differs from its sample count".into());
        }
        Ok(body
            .as_chunks::<16>()
            .0
            .iter()
            .map(|sample| {
                let (t, value) = sample.split_at(8);
                (
                    i64::from_le_bytes(t.try_into().expect("8 bytes")),
                    f64::from_bits(u64::from_le_bytes(value.try_into().expect("8 bytes"))),
                )
            })
            .collect())
    }
}

impl OpenSampleBlock for PlainOpenBlock {
    fn append(&mut self, timestamp_ms: i64, value: f64) {
        match self.samples.last() {
            Some(&(last, _)) if timestamp_ms <= last => {
                match self
                    .samples
                    .binary_search_by_key(&timestamp_ms, |&(t, _)| t)
                {
                    Ok(index) => self.samples[index].1 = value,
                    Err(index) => self.samples.insert(index, (timestamp_ms, value)),
                }
            }
            _ => self.samples.push((timestamp_ms, value)),
        }
    }

    fn len(&self) -> usize {
        self.samples.len()
    }

    fn memory_bytes(&self) -> usize {
        std::mem::size_of_val(self.samples.as_slice())
    }

    fn seal(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(4 + 16 * self.samples.len());
        bytes.extend((self.samples.len() as u32).to_le_bytes());
        for &(t, value) in &self.samples {
            bytes.extend(t.to_le_bytes());
            bytes.extend(value.to_bits().to_le_bytes());
        }
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Out-of-order and repeated timestamps seal as the samples in time
    // order, the last write winning; the block decodes back bit for bit.
    #[test]
    fn plain_blocks_round_trip_in_time_order() {
        let codec = PlainSampleBlockCodec;
        let mut block = codec.new_block();
        for (t, value) in [(3, 3.0), (1, f64::NAN), (2, 2.0), (2, -0.0)] {
            block.append(t, value);
        }
        let sealed = block.seal();
        assert_eq!(sealed.len(), 4 + 3 * 16);
        let bits: Vec<_> = codec
            .decode(&sealed)
            .unwrap()
            .into_iter()
            .map(|(t, v)| (t, v.to_bits()))
            .collect();
        assert_eq!(
            bits,
            [
                (1, f64::NAN.to_bits()),
                (2, (-0.0f64).to_bits()),
                (3, 3.0f64.to_bits())
            ]
        );
        assert_eq!(codec.samples_in(&sealed, 1, 2).unwrap().len(), 1);
        assert!(codec.decode(&sealed[..10]).is_err());
    }

    // A merge holds both blocks' samples, the newer winning on a tie.
    #[test]
    fn plain_merge_keeps_both_blocks() {
        let codec = PlainSampleBlockCodec;
        let seal = |samples: &[(i64, f64)]| {
            let mut block = codec.new_block();
            for &(t, v) in samples {
                block.append(t, v);
            }
            block.seal()
        };
        let merged = codec
            .merge(&seal(&[(1, 1.0), (3, 3.0)]), &seal(&[(2, 2.0), (3, 30.0)]))
            .unwrap();
        assert_eq!(
            codec.decode(&merged).unwrap(),
            [(1, 1.0), (2, 2.0), (3, 30.0)]
        );
    }
}
