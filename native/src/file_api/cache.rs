//! Adaptive retention of decoded volumes and streaming chunks.

use std::collections::VecDeque;

/// Budget for retaining decoded volumes/chunks across interleaved plane reads.
/// A single entry larger than the retention budget is permitted.
pub const DEFAULT_FILE_CACHE_BYTES: usize = 2 * 1024 * 1024 * 1024;

const MAX_CACHE_ENTRIES: usize = 128;

struct CachedPixels {
    key: usize,
    pixels: Vec<u16>,
}

pub(super) struct PixelCache {
    entries: VecDeque<CachedPixels>,
    bytes: usize,
    limit: usize,
    last_read: Option<(usize, usize)>,
    recent_keys: VecDeque<usize>,
    reuse_detected: bool,
}

impl PixelCache {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            limit,
            last_read: None,
            recent_keys: VecDeque::new(),
            reuse_detected: false,
        }
    }
    pub(super) fn note_read(&mut self, key: usize, z: usize) {
        if !self.reuse_detected {
            if let Some((previous, previous_z)) = self.last_read {
                if previous != key {
                    // Same-Z channel switches are the common Fiji case.
                    // Revisiting a requested key also enables caching for less
                    // regular browsing, with bounded bookkeeping.
                    if previous_z == z || self.recent_keys.contains(&key) {
                        self.reuse_detected = true;
                        self.recent_keys.clear();
                    } else {
                        if self.recent_keys.len() == MAX_CACHE_ENTRIES {
                            self.recent_keys.pop_front();
                        }
                        self.recent_keys.push_back(previous);
                    }
                }
            }
        }
        self.last_read = Some((key, z));
    }
    pub(super) fn prepare_read(&mut self, bytes: usize) {
        // A first sequential pass has no reuse between entries. Preserve the
        // single-entry memory footprint until reuse is observed.
        if !self.reuse_detected {
            while !self.entries.is_empty() {
                self.evict_oldest();
            }
        }
        self.prepare(bytes);
    }
    #[cfg(test)]
    fn contains(&self, key: usize) -> bool {
        self.entries.iter().any(|entry| entry.key == key)
    }
    fn evict_oldest(&mut self) {
        if let Some(entry) = self.entries.pop_back() {
            self.bytes -= entry.pixels.capacity() * std::mem::size_of::<u16>();
        }
    }
    // Evict before allocating another decoded volume, rather than allowing
    // retained cached data to inflate the decoder's peak working set.
    fn prepare(&mut self, bytes: usize) {
        while !self.entries.is_empty()
            && (self.entries.len() >= MAX_CACHE_ENTRIES
                || self.bytes.checked_add(bytes).is_none_or(|n| n > self.limit))
        {
            self.evict_oldest();
        }
    }
    pub(super) fn insert(&mut self, key: usize, pixels: Vec<u16>) {
        // A batch and another reader can finish decoding the same key. Keep
        // one retained copy and account for its actual allocation capacity.
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            self.bytes -=
                self.entries.remove(index).unwrap().pixels.capacity() * std::mem::size_of::<u16>();
        }
        let bytes = pixels.capacity() * std::mem::size_of::<u16>();
        self.prepare(bytes);
        self.bytes += bytes;
        self.entries.push_front(CachedPixels { key, pixels });
    }
    pub(super) fn get(&mut self, key: usize) -> Option<&[u16]> {
        let index = self.entries.iter().position(|entry| entry.key == key)?;
        if index != 0 {
            let entry = self.entries.remove(index).unwrap();
            self.entries.push_front(entry);
        }
        Some(&self.entries.front().unwrap().pixels)
    }
    pub(super) fn set_limit(&mut self, bytes: usize) {
        self.limit = bytes;
        // Zero restores single-entry behavior. A lone oversized volume/chunk
        // remains usable without decoding it again for every plane.
        while self.entries.len() > 1 && self.bytes > self.limit {
            self.evict_oldest();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::index::FileBacking;
    use super::super::test_support::temp_path;
    use super::super::RadeltaFileHandle;
    use crate::{Dims5, Options};
    use std::fs;

    #[test]
    fn decoded_cache_adapts_to_sequential_and_interleaved_plane_orders() {
        let mut cache = super::PixelCache::new(1024);
        cache.note_read(0, 0);
        cache.prepare_read(4);
        cache.insert(0, vec![1, 2]);
        cache.note_read(0, 31);
        cache.note_read(1, 0);
        cache.prepare_read(4);
        cache.insert(1, vec![3, 4]);
        assert_eq!(cache.entries.len(), 1);
        assert!(!cache.reuse_detected);
        // Revisiting an evicted channel at another Z enables reuse too.
        cache.note_read(0, 1);
        cache.prepare_read(4);
        cache.insert(0, vec![1, 2]);
        assert_eq!(cache.entries.len(), 2);
        assert!(cache.reuse_detected);

        let mut cache = super::PixelCache::new(1024);
        cache.note_read(0, 0);
        cache.prepare_read(4);
        cache.insert(0, vec![1, 2]);
        cache.note_read(1, 0);
        cache.prepare_read(4);
        cache.insert(1, vec![3, 4]);
        assert_eq!(cache.entries.len(), 2);
        assert!(cache.reuse_detected);
    }

    #[test]
    fn decoded_cache_evicts_least_recently_used_data_within_budget() {
        let mut cache = super::PixelCache::new(8);
        cache.insert(1, vec![11, 12]);
        cache.insert(2, vec![21, 22]);
        assert_eq!(cache.get(1).unwrap(), &[11, 12]);
        cache.prepare(4);
        cache.insert(3, vec![31, 32]);
        assert!(cache.contains(1));
        assert!(!cache.contains(2));
        assert_eq!(cache.get(3).unwrap(), &[31, 32]);
        assert_eq!(cache.bytes, 8);
        cache.set_limit(0);
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.contains(3));
        cache.insert(4, vec![41, 42]);
        assert!(!cache.contains(3));
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn decoded_cache_accounts_for_capacity_and_allows_one_oversized_entry() {
        let mut cache = super::PixelCache::new(8);
        cache.insert(1, vec![1, 2]);
        let mut pixels = Vec::with_capacity(16);
        pixels.push(99);
        let bytes = pixels.capacity() * 2;
        cache.prepare(bytes);
        assert!(cache.entries.is_empty());
        cache.insert(2, pixels);
        assert_eq!(cache.bytes, bytes);
        assert_eq!(cache.get(2).unwrap(), &[99]);
        cache.set_limit(0);
        assert_eq!(cache.entries.len(), 1);
        cache.insert(3, vec![3]);
        assert_eq!(cache.bytes, 2);
        assert!(!cache.contains(2));
    }

    #[test]
    fn decoded_cache_bounds_entry_overhead_even_with_a_large_byte_budget() {
        let mut cache = super::PixelCache::new(usize::MAX);
        for key in 0..=super::MAX_CACHE_ENTRIES {
            cache.insert(key, vec![key as u16]);
        }
        assert_eq!(cache.entries.len(), super::MAX_CACHE_ENTRIES);
        assert!(!cache.contains(0));
        assert_eq!(cache.bytes, super::MAX_CACHE_ENTRIES * 2);
    }

    #[test]
    fn concurrent_batch_and_single_reads_keep_one_cached_copy() {
        let d = Dims5 {
            x: 32,
            y: 17,
            z: 7,
            c: 3,
            t: 2,
        };
        let data: Vec<_> = (0..d.voxels().unwrap()).map(|i| (i * 31) as u16).collect();
        let path = temp_path("batch-concurrent");
        fs::write(
            &path,
            crate::compress_u16_nd(&data, d, Options::default()).unwrap(),
        )
        .unwrap();
        let reader = RadeltaFileHandle::open(&path).unwrap();
        let plane = d.x * d.y;
        std::thread::scope(|s| {
            for worker in 0..4 {
                let (reader, data) = (&reader, &data);
                s.spawn(move || {
                    for i in 0..12 {
                        let c = (i + worker) % d.c;
                        let coords = [(i % d.t, c, i % d.z), (0, c, 0), (i % d.t, c, i % d.z)];
                        let mut out = vec![0; 3 * plane];
                        if worker == 0 {
                            for (&(t, c, z), dst) in coords.iter().zip(out.chunks_exact_mut(plane))
                            {
                                reader.read_plane(t, c, z, dst).unwrap();
                            }
                        } else {
                            reader.read_planes(&coords, &mut out).unwrap();
                        }
                        for (j, &(t, c, z)) in coords.iter().enumerate() {
                            let start = ((t * d.c + c) * d.z + z) * plane;
                            assert_eq!(
                                &out[j * plane..(j + 1) * plane],
                                &data[start..start + plane]
                            );
                        }
                    }
                });
            }
        });
        let backing = reader.backing.lock().unwrap();
        let FileBacking::Nd { cache, .. } = &*backing else {
            panic!()
        };
        let keys: std::collections::HashSet<_> = cache.entries.iter().map(|e| e.key).collect();
        assert_eq!(keys.len(), cache.entries.len());
        assert_eq!(
            cache.bytes,
            cache.entries.iter().map(|e| e.pixels.capacity() * 2).sum()
        );
        drop(backing);
        drop(reader);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn sequential_batch_cache_keeps_the_trailing_requested_unit() {
        let d = Dims5 {
            x: 17,
            y: 11,
            z: 3,
            c: 2,
            t: 1,
        };
        let pixels = vec![123; d.voxels().unwrap()];
        let path = temp_path("pipeline-cache-order");
        fs::write(
            &path,
            crate::compress_u16_nd(&pixels, d, Options::default()).unwrap(),
        )
        .unwrap();
        let reader = RadeltaFileHandle::open(&path).unwrap();
        let mut output = vec![0; d.x * d.y * 3];
        reader
            .read_planes(&[(0, 0, 0), (0, 0, 1), (0, 1, 0)], &mut output)
            .unwrap();
        let backing = reader.backing.lock().unwrap();
        let FileBacking::Nd { cache, .. } = &*backing else {
            panic!()
        };
        assert!(!cache.reuse_detected);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.entries[0].key, 1);
        drop(backing);
        drop(reader);
        fs::remove_file(path).unwrap();
    }
}
