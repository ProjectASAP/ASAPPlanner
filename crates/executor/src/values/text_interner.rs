//! Distinct texts, each held once, by position: the values of a text
//! dictionary built a text at a time ([`TextInterner`]).

use ahash::{AHashMap as HashMap, RandomState};
use arrow::array::{Array, StringArray};
use arrow::buffer::{Buffer, OffsetBuffer, ScalarBuffer};

/// Distinct texts, each held once, at the position it was first interned:
/// `intern("a")`, `intern("b")`, `intern("a")` give 0, 1, 0 and the values
/// `["a", "b"]`. A text is found through a table of positions by hash,
/// never through a second copy of it.
///
/// The texts live in a [`StringArray`] when frozen
/// ([`Self::frozen_values`], for a dictionary to read them) and in growable
/// buffers while interning; switching takes the buffers back without a
/// copy when no frozen array is still read, so interning `n` new texts
/// costs `O(n)` amortized, and the buffers hold at most a quarter more
/// than their texts.
pub(crate) struct TextInterner {
    texts: Texts,
    /// The first position of each hash; later ones with the same hash
    /// follow through `next_with_same_hash`.
    first_with_hash: HashMap<u64, u32>,
    /// Per position, the next position with the same hash (`u32::MAX`:
    /// none).
    next_with_same_hash: Vec<u32>,
    hasher: RandomState,
}

enum Texts {
    Growing { data: Vec<u8>, offsets: Vec<i32> },
    Frozen(StringArray),
}

impl Default for TextInterner {
    fn default() -> Self {
        Self {
            texts: Texts::Growing {
                data: Vec::new(),
                offsets: vec![0],
            },
            first_with_hash: HashMap::new(),
            next_with_same_hash: Vec::new(),
            hasher: RandomState::new(),
        }
    }
}

impl TextInterner {
    /// The number of texts.
    pub(crate) fn len(&self) -> usize {
        self.next_with_same_hash.len()
    }

    /// The text at `position`.
    pub(crate) fn text(&self, position: u32) -> &str {
        let position = position as usize;
        match &self.texts {
            Texts::Growing { data, offsets } => {
                let (start, end) = (offsets[position] as usize, offsets[position + 1] as usize);
                std::str::from_utf8(&data[start..end]).expect("interned texts are UTF-8")
            }
            Texts::Frozen(values) => values.value(position),
        }
    }

    /// The position of `text`, interning it when new.
    pub(crate) fn intern(&mut self, text: &str) -> u32 {
        let hash = self.hasher.hash_one(text);
        if let Some(&first) = self.first_with_hash.get(&hash) {
            let mut position = first;
            loop {
                if self.text(position) == text {
                    return position;
                }
                match self.next_with_same_hash[position as usize] {
                    u32::MAX => break,
                    next => position = next,
                }
            }
        }
        let position =
            u32::try_from(self.len()).expect("a text dictionary has at most u32::MAX texts");
        let (data, offsets) = self.growing();
        // Grown by a quarter when full, not doubled: the texts are kept for
        // a view's life, so at most a quarter of them is spare, and each
        // text is still copied O(1) times on average.
        if data.capacity() - data.len() < text.len() {
            data.reserve_exact(text.len().max(data.len() / 4));
        }
        if offsets.capacity() == offsets.len() {
            offsets.reserve_exact(1.max(offsets.len() / 4));
        }
        data.extend_from_slice(text.as_bytes());
        offsets.push(i32::try_from(data.len()).expect("a text dictionary holds under 2 GiB"));
        // The new text heads its hash's chain.
        let previous_first = self.first_with_hash.insert(hash, position);
        self.next_with_same_hash
            .push(previous_first.unwrap_or(u32::MAX));
        position
    }

    /// The texts, by position, as an array a dictionary can read; built
    /// from the growing buffers without copying them.
    pub(crate) fn frozen_values(&mut self) -> &StringArray {
        if let Texts::Growing { data, offsets } = &mut self.texts {
            let data = Buffer::from_vec(std::mem::take(data));
            let offsets = ScalarBuffer::from(Buffer::from_vec(std::mem::take(offsets)));
            // SAFETY: the offsets start at 0, never decrease and end at the
            // data's length, and each text appended is valid UTF-8.
            let values = unsafe {
                StringArray::new_unchecked(OffsetBuffer::new_unchecked(offsets), data, None)
            };
            self.texts = Texts::Frozen(values);
        }
        match &self.texts {
            Texts::Frozen(values) => values,
            Texts::Growing { .. } => unreachable!("frozen above"),
        }
    }

    /// The frozen values; `None` while growing.
    pub(crate) fn values(&self) -> Option<&StringArray> {
        match &self.texts {
            Texts::Frozen(values) => Some(values),
            Texts::Growing { .. } => None,
        }
    }

    /// Bytes held: the texts and their offsets as allocated, and the
    /// table of positions by hash.
    pub(crate) fn memory_bytes(&self) -> usize {
        let texts = match &self.texts {
            Texts::Growing { data, offsets } => {
                data.capacity() + offsets.capacity() * std::mem::size_of::<i32>()
            }
            Texts::Frozen(values) => values.get_array_memory_size(),
        };
        // A hash table holds a 1-byte control and an entry per bucket, at
        // most 7/8 of its buckets full.
        let buckets = self.first_with_hash.capacity() * 8 / 7;
        let table = buckets * (std::mem::size_of::<(u64, u32)>() + 1)
            + self.next_with_same_hash.capacity() * std::mem::size_of::<u32>();
        texts + table
    }

    /// The growing buffers, taken back from a frozen array without a copy
    /// when nothing else reads it, else copied.
    fn growing(&mut self) -> (&mut Vec<u8>, &mut Vec<i32>) {
        if let Texts::Frozen(values) = &mut self.texts {
            let values = std::mem::replace(values, StringArray::from(Vec::<&str>::new()));
            let (offsets, data, _) = values.into_parts();
            let offsets: Buffer = offsets.into_inner().into_inner();
            let offsets = offsets
                .into_vec::<i32>()
                .unwrap_or_else(|shared| shared.typed_data::<i32>().to_vec());
            let data = data
                .into_vec::<u8>()
                .unwrap_or_else(|shared| shared.as_slice().to_vec());
            self.texts = Texts::Growing { data, offsets };
        }
        match &mut self.texts {
            Texts::Growing { data, offsets } => (data, offsets),
            Texts::Frozen(_) => unreachable!("thawed above"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Texts are interned once each, found by text, and frozen into an
    // array of them in order; interning after freezing keeps them all.
    #[test]
    fn texts_are_interned_once_and_frozen_in_order() {
        let mut interner = TextInterner::default();
        assert_eq!(
            ["a", "b", "a", "c"].map(|text| interner.intern(text)),
            [0, 1, 0, 2]
        );
        let frozen: Vec<_> = interner.frozen_values().iter().flatten().collect();
        assert_eq!(frozen, ["a", "b", "c"]);
        assert_eq!(interner.intern("d"), 3);
        assert_eq!(interner.intern("a"), 0);
        let frozen: Vec<_> = interner.frozen_values().iter().flatten().collect();
        assert_eq!(frozen, ["a", "b", "c", "d"]);
        assert!(interner.memory_bytes() >= 4 + 5 * 4);
    }

    // Thawing a frozen array nothing else reads takes its buffers back:
    // the data buffer keeps its address.
    #[test]
    fn interning_after_freezing_reuses_the_buffers() {
        let mut interner = TextInterner::default();
        for i in 0..100 {
            interner.intern(&format!("series-{i}"));
        }
        let before = interner.frozen_values().value_data().as_ptr();
        interner.intern("series-0");
        let (data, _) = interner.growing();
        assert_eq!(data.as_ptr(), before);
    }
}
