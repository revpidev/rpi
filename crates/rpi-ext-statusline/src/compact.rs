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
#[derive(Debug, Default)]
pub(crate) struct SortedPairs {
    pairs: Vec<(u64, u32)>,
}

impl SortedPairs {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            pairs: Vec::with_capacity(capacity),
        }
    }

    /// Record `key → value`; rejects a duplicate key.
    pub(crate) fn insert(&mut self, key: u64, value: u32) -> Result<(), String> {
        match self.pairs.binary_search_by_key(&key, |(stored, _)| *stored) {
            Ok(_) => Err(format!("duplicate merge pair {key:#x}")),
            Err(index) => {
                self.pairs.insert(index, (key, value));
                Ok(())
            }
        }
    }

    /// Insert in ascending key order (the asset reader's fast path: the
    /// ranks come in order and the keys are strictly increasing for a valid
    /// table); falls back to the sorted insert when the order breaks.
    pub(crate) fn insert_sorted(&mut self, key: u64, value: u32) -> Result<(), String> {
        if self.pairs.last().is_some_and(|(stored, _)| *stored >= key) {
            return self.insert(key, value);
        }
        self.pairs.push((key, value));
        Ok(())
    }

    pub(crate) fn get(&self, key: &u64) -> Option<u32> {
        self.pairs
            .binary_search_by_key(key, |(stored, _)| *stored)
            .ok()
            .map(|index| self.pairs[index].1)
    }

    pub(crate) fn shrink(&mut self) {
        self.pairs.shrink_to_fit();
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
    fn sorted_pairs_retained_bytes_and_len() {
        let mut pairs = SortedPairs::with_capacity(3);
        pairs.insert_sorted(1, 1).expect("push");
        pairs.insert_sorted(2, 2).expect("push");
        pairs.shrink();
        assert_eq!(
            pairs.retained_bytes(),
            2 * std::mem::size_of::<(u64, u32)>()
        );
    }

    #[test]
    fn byte_pool_index_finds_exact_entries_only() {
        let mut index = BytePoolIndex::with_capacity(4, 16);
        for bytes in [b"a".as_slice(), b"ab", b"\xe4\xbd\xa0", b""] {
            index.push(bytes).expect("push");
        }
        assert_eq!(index.find(b"a"), Some(0));
        assert_eq!(index.find(b"ab"), Some(1));
        assert_eq!(index.find("\u{4f60}".as_bytes()), Some(2));
        assert_eq!(index.find(b""), Some(3));
        assert_eq!(index.find(b"abc"), None);
        assert_eq!(index.find(b"b"), None);
        assert_eq!(index.find(b"\xe4\xbd"), None);
        assert_eq!(index.entry(1), Some(b"ab".as_slice()));
        assert_eq!(index.entry(9), None);
        assert!(index.contains(b"ab") && !index.contains(b"ba"));
        assert_eq!(index.len(), 4);
    }

    /// The probe path: with a 4-slot table, entries whose FNV-1a hashes
    /// share a slot are found by walking the table and comparing the FULL
    /// bytes — a collision (or a prefix of a stored entry) never matches.
    #[test]
    fn open_addressing_probes_compare_full_bytes() {
        // Find three keys that collide on the same slot (hash & 3).
        let mut colliding: Vec<String> = Vec::new();
        let target = BytePoolIndex::hash(b"seed") & 3;
        for i in 0..10_000u32 {
            let key = format!("k{i}");
            if BytePoolIndex::hash(key.as_bytes()) & 3 == target {
                colliding.push(key);
                if colliding.len() == 3 {
                    break;
                }
            }
        }
        assert_eq!(colliding.len(), 3, "three colliding keys");
        let mut index = BytePoolIndex::with_table_bits(3, 64, 2);
        for (rank, key) in colliding.iter().enumerate() {
            index.push(key.as_bytes()).expect("push");
            assert_eq!(index.find(key.as_bytes()), Some(rank as u32), "{key}");
        }
        for (rank, key) in colliding.iter().enumerate() {
            assert_eq!(index.find(key.as_bytes()), Some(rank as u32), "{key}");
        }
        // A prefix of a stored entry and an absent key do not match.
        assert_eq!(index.find(&colliding[0].as_bytes()[..1]), None);
        assert_eq!(index.find(b"absent"), None);
        assert_eq!(index.len(), 3);
    }

    #[test]
    fn sorted_pairs_reject_duplicates_and_search_by_key() {
        let mut pairs = SortedPairs::with_capacity(4);
        pairs.insert_sorted(30, 3).expect("sorted");
        // Out of order: the fast path falls back to the sorted insert.
        pairs.insert_sorted(10, 1).expect("fallback insert");
        pairs.insert(10, 1).expect_err("duplicate");
        pairs.insert_sorted(20, 2).expect("insert");
        pairs.insert(20, 9).expect_err("duplicate");
        assert_eq!(pairs.get(&10), Some(1));
        assert_eq!(pairs.get(&20), Some(2));
        assert_eq!(pairs.get(&30), Some(3));
        assert_eq!(pairs.get(&15), None);
    }
}
