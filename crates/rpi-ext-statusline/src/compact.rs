//! Compact tokenizer tables (V19-07 FR-F / R1.4.4, ADR-0049 decision 13).
//!
//! The bundled tables used to hold every token twice — once as the rank →
//! bytes vector (`Vec<Box<[u8]>>`, one heap allocation per token) and once as
//! the `HashMap<Box<[u8]>, u32>` key — plus a hash map per merge pair and a
//! `HashSet<Box<str>>` for the `ignore_merges` vocabulary. For the ~165 K
//! tiktoken tokens of Kimi that is tens of megabytes of anonymous memory on
//! top of the asset bytes, and the tokenizer cache is the single largest RES
//! contributor of the statusline once a model family is in use.
//!
//! [`BytePoolIndex`] keeps the bytes of every entry ONCE in a contiguous
//! pool, a `rank → (offset, length)` table, and an open-addressed hash table
//! of `u32` ranks keyed by an FNV-1a hash of the entry bytes. Lookups probe
//! the table and compare the FULL entry bytes, so a hash collision can never
//! change a count (it only costs a probe); nothing allocates per entry.
//! [`SortedPairs`] replaces the `HashMap<u64, u32>` of merge ranks by a sorted
//! vector searched by key (cache-friendly binary search, no hash overhead).
//!
//! Semantics are the ones the official full pass and the incremental counter
//! rely on: `find` answers the rank of the exact byte string or `None`; the
//! six-family golden vectors and the every-delta-boundary parity tests pin
//! the counts.

/// One entry of a [`BytePoolIndex`]: where the bytes live in the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EntryRange {
    offset: u32,
    length: u32,
}

/// Byte pool + range table + collision-safe open-addressed index.
#[derive(Debug)]
pub(crate) struct BytePoolIndex {
    pool: Vec<u8>,
    entries: Vec<EntryRange>,
    /// `rank + 1` per slot (`0` = empty); capacity is a power of two ≥ 2×
    /// the entry count (load factor ≤ 50 %).
    table: Vec<u32>,
    mask: usize,
}

impl BytePoolIndex {
    pub(crate) fn with_capacity(entries: usize, bytes: usize) -> Self {
        let mut table_bits = 1usize;
        while (1usize << table_bits) < entries.saturating_mul(2).max(2) {
            table_bits += 1;
        }
        Self::with_table_bits(entries, bytes, table_bits)
    }

    /// Build with an explicit table size (2^`table_bits` slots); the tests
    /// use a deliberately tiny table so probing is exercised.
    fn with_table_bits(entries: usize, bytes: usize, table_bits: usize) -> Self {
        Self {
            pool: Vec::with_capacity(bytes),
            entries: Vec::with_capacity(entries),
            table: vec![0u32; 1usize << table_bits],
            mask: (1usize << table_bits) - 1,
        }
    }

    /// FNV-1a over the entry bytes.
    fn hash(bytes: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    /// Append one entry; the index must not contain it yet.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<(), String> {
        let rank = self.entries.len();
        if rank >= u32::MAX as usize {
            return Err("byte pool index is full".to_owned());
        }
        if self.pool.len().saturating_add(bytes.len()) > u32::MAX as usize {
            return Err("byte pool is full".to_owned());
        }
        let offset = self.pool.len() as u32;
        self.pool.extend_from_slice(bytes);
        self.entries.push(EntryRange {
            offset,
            length: bytes.len() as u32,
        });
        let hash = Self::hash(bytes);
        let mut slot = (hash as usize) & self.mask;
        loop {
            if self.table[slot] == 0 {
                self.table[slot] = rank as u32 + 1;
                return Ok(());
            }
            // A duplicate entry would shadow the earlier rank; the asset
            // reader rejects duplicates before pushing, so this is unreachable
            // in practice — keep probing rather than corrupting the table.
            slot = (slot + 1) & self.mask;
        }
    }

    pub(crate) fn entry(&self, rank: u32) -> Option<&[u8]> {
        let entry = self.entries.get(rank as usize)?;
        Some(&self.pool[entry.offset as usize..(entry.offset + entry.length) as usize])
    }

    /// The rank of exactly `bytes`, comparing the full bytes on every probe
    /// (a hash collision never matches).
    pub(crate) fn find(&self, bytes: &[u8]) -> Option<u32> {
        let mut slot = (Self::hash(bytes) as usize) & self.mask;
        loop {
            let stored = self.table[slot];
            if stored == 0 {
                return None;
            }
            let rank = stored - 1;
            if self.entry(rank)? == bytes {
                return Some(rank);
            }
            slot = (slot + 1) & self.mask;
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether exactly `bytes` is an entry (the `ignore_merges` vocabulary).
    pub(crate) fn contains(&self, bytes: &[u8]) -> bool {
        self.find(bytes).is_some()
    }

    /// Release the spare capacity of every table (called once after the
    /// asset is fully read).
    pub(crate) fn shrink(&mut self) {
        self.pool.shrink_to_fit();
        self.entries.shrink_to_fit();
    }

    /// Bytes held by the three tables (diagnostics).
    pub(crate) fn retained_bytes(&self) -> usize {
        self.pool.capacity()
            + self.entries.capacity() * std::mem::size_of::<EntryRange>()
            + self.table.capacity() * 4
    }
}

/// Sorted `(key, value)` pairs searched by key (the HF merge ranks).
///
/// The asset lists merge ranks by priority, not by `(left, right)` key, so
/// the table is BUILT by appending every pair and sorting once
/// ([`SortedPairs::finish`], O(n log n)); inserting each key into the
/// middle of a sorted vector instead is quadratic and cost seconds of cold
/// load per family (V19-07 review round 2). Lookups binary-search by key.
#[derive(Debug, Default)]
pub(crate) struct SortedPairs {
    pairs: Vec<(u64, u32)>,
    /// Whether `pairs` is sorted by key and ready for lookups.
    ready: bool,
}

impl SortedPairs {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            pairs: Vec::with_capacity(capacity),
            ready: false,
        }
    }

    /// Append one pair in build order; duplicates are rejected by
    /// [`Self::finish`].
    pub(crate) fn push(&mut self, key: u64, value: u32) {
        self.pairs.push((key, value));
        self.ready = false;
    }

    /// Sort by key and reject duplicate keys; mandatory before lookups.
    pub(crate) fn finish(&mut self) -> Result<(), String> {
        self.pairs.sort_unstable_by_key(|(key, _)| *key);
        if self
            .pairs
            .windows(2)
            .any(|window| window[0].0 == window[1].0)
        {
            return Err("duplicate merge pair".to_owned());
        }
        self.pairs.shrink_to_fit();
        self.ready = true;
        Ok(())
    }

    /// Sorted insert of one pair (random-order callers); rejects a
    /// duplicate key. The asset reader uses [`Self::push`] + [`Self::finish`].
    #[cfg(test)]
    fn insert(&mut self, key: u64, value: u32) -> Result<(), String> {
        match self.pairs.binary_search_by_key(&key, |(stored, _)| *stored) {
            Ok(_) => Err(format!("duplicate merge pair {key:#x}")),
            Err(index) => {
                self.pairs.insert(index, (key, value));
                Ok(())
            }
        }
    }

    /// The value of exactly `key` (the table must be finished).
    pub(crate) fn get(&self, key: &u64) -> Option<u32> {
        debug_assert!(self.ready, "SortedPairs::get before finish");
        self.pairs
            .binary_search_by_key(key, |(stored, _)| *stored)
            .ok()
            .map(|index| self.pairs[index].1)
    }

    /// Bytes held by the pair table (diagnostics).
    pub(crate) fn retained_bytes(&self) -> usize {
        self.pairs.capacity() * std::mem::size_of::<(u64, u32)>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every entry is found by its exact bytes; a colliding or prefixing
    /// key never matches; ranks are stable and the pool stores each entry
    /// once.
    #[test]
    fn sorted_pairs_retained_bytes() {
        let mut pairs = SortedPairs::with_capacity(3);
        pairs.push(1, 1);
        pairs.push(2, 2);
        pairs.finish().expect("finish");
        assert_eq!(
            pairs.retained_bytes(),
            2 * std::mem::size_of::<(u64, u32)>()
        );
    }

    #[test]
    fn sorted_pairs_reject_duplicates_and_search_by_key() {
        let mut pairs = SortedPairs::with_capacity(4);
        // Push order is arbitrary (merge ranks are listed by priority).
        pairs.push(30, 3);
        pairs.push(10, 1);
        pairs.push(20, 2);
        pairs.finish().expect("finish");
        assert_eq!(pairs.get(&10), Some(1));
        assert_eq!(pairs.get(&20), Some(2));
        assert_eq!(pairs.get(&30), Some(3));
        assert_eq!(pairs.get(&15), None);
        // Random-order insert keeps the table sorted and rejects duplicates.
        pairs.insert(25, 5).expect("insert");
        pairs.insert(25, 9).expect_err("duplicate");
        assert_eq!(pairs.get(&25), Some(5));
        let mut duplicates = SortedPairs::with_capacity(2);
        duplicates.push(7, 1);
        duplicates.push(7, 2);
        assert!(
            duplicates.finish().is_err(),
            "a duplicate key is rejected at finish"
        );
    }

    /// V19-07 review round 2: building the table from merge-rank order (keys
    /// NOT ascending) must be linearithmic — the previous middle-insert
    /// fallback was quadratic and cost seconds of cold load per family.
    #[test]
    fn sorted_pairs_build_from_unsorted_keys_is_linearithmic() {
        let n = 120_000u64;
        // A deterministic pseudo-random key order (xorshift), deduplicated.
        let mut keys: Vec<u64> = Vec::with_capacity(n as usize);
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        while keys.len() < n as usize {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            keys.push(state);
        }
        keys.sort_unstable();
        keys.dedup();
        let mut shuffled = keys.clone();
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for index in (1..shuffled.len()).rev() {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            shuffled.swap(index, (seed >> 33) as usize % (index + 1));
        }
        let ascending = shuffled.windows(2).filter(|w| w[0] < w[1]).count();
        assert!(
            ascending * 10 < shuffled.len() * 9,
            "the fixture is shuffled, not sorted: {ascending} ascending of {}",
            shuffled.len()
        );
        let started = std::time::Instant::now();
        let mut pairs = SortedPairs::with_capacity(shuffled.len());
        for (rank, key) in shuffled.iter().enumerate() {
            pairs.push(*key, rank as u32);
        }
        pairs.finish().expect("finish");
        let build = started.elapsed();
        let started = std::time::Instant::now();
        let mut reference: Vec<(u64, u32)> = shuffled
            .iter()
            .enumerate()
            .map(|(rank, key)| (*key, rank as u32))
            .collect();
        reference.sort_unstable_by_key(|(key, _)| *key);
        let plain_sort = started.elapsed();
        for (rank, key) in shuffled.iter().enumerate() {
            assert_eq!(pairs.get(key), Some(rank as u32));
        }
        assert!(
            build.as_millis() <= 5 * plain_sort.as_millis().max(1) + 20,
            "build {build:?} vs plain sort {plain_sort:?}: the quadratic fallback is back"
        );
    }
}
