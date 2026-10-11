//! Byte-budgeted LRU for expensive native tile data.
use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct Entry<V> {
    value: V,
    cost: usize,
    accessed: Instant,
    order: u64,
}

#[derive(Debug)]
pub(crate) struct MemoryCache<K, V> {
    entries: HashMap<K, Entry<V>>,
    order: BTreeMap<u64, K>,
    sequence: u64,
    bytes: usize,
    byte_limit: usize,
    count_limit: usize,
    idle_ttl: Duration,
}

impl<K: Eq + Hash + Clone, V: Clone> MemoryCache<K, V> {
    pub(crate) fn new(count_limit: usize, byte_limit: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: BTreeMap::new(),
            sequence: 0,
            bytes: 0,
            byte_limit,
            count_limit,
            idle_ttl: Duration::from_secs(120),
        }
    }

    fn remove_oldest(&mut self) {
        if let Some((_, key)) = self.order.pop_first() {
            if let Some(entry) = self.entries.remove(&key) {
                self.bytes -= entry.cost;
            }
        }
    }

    fn expire(&mut self, now: Instant) {
        while let Some((_, key)) = self.order.first_key_value() {
            let Some(entry) = self.entries.get(key) else {
                break;
            };
            if now.duration_since(entry.accessed) < self.idle_ttl {
                break;
            }
            self.remove_oldest();
        }
    }

    pub(crate) fn get(&mut self, key: &K) -> Option<V> {
        self.get_at(key, Instant::now())
    }

    fn get_at(&mut self, key: &K, now: Instant) -> Option<V> {
        self.expire(now);
        let entry = self.entries.get_mut(key)?;
        self.order.remove(&entry.order);
        self.sequence += 1;
        entry.order = self.sequence;
        entry.accessed = now;
        self.order.insert(entry.order, key.clone());
        Some(entry.value.clone())
    }

    pub(crate) fn insert(&mut self, key: K, value: V, cost: usize) {
        self.expire(Instant::now());
        if let Some(entry) = self.entries.remove(&key) {
            self.bytes -= entry.cost;
            self.order.remove(&entry.order);
        }
        // A single dense tile must not displace the entire working set.
        if cost > self.byte_limit || self.count_limit == 0 {
            return;
        }
        while self.entries.len() >= self.count_limit || self.bytes > self.byte_limit - cost {
            self.remove_oldest();
        }
        self.sequence += 1;
        self.bytes += cost;
        self.order.insert(self.sequence, key.clone());
        self.entries.insert(
            key,
            Entry {
                value,
                cost,
                accessed: Instant::now(),
                order: self.sequence,
            },
        );
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.bytes = 0;
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_by_bytes_and_refreshes_on_hit() {
        let mut cache = MemoryCache::new(10, 10);
        cache.insert(1, "one", 4);
        cache.insert(2, "two", 4);
        assert_eq!(cache.get(&1), Some("one"));
        cache.insert(3, "three", 4);
        assert_eq!(cache.get(&2), None);
        assert_eq!(cache.get(&1), Some("one"));
        assert_eq!(cache.bytes, 8);
        cache.insert(4, "oversize", 11);
        assert_eq!(cache.bytes, 8);
        assert_eq!(cache.get(&4), None);
    }

    #[test]
    fn enforces_count_replacement_and_idle_expiry() {
        let mut cache = MemoryCache::new(2, 100);
        cache.insert(1, "old", 20);
        cache.insert(1, "new", 30);
        assert_eq!(cache.bytes, 30);
        cache.insert(2, "two", 10);
        cache.insert(3, "three", 10);
        assert_eq!(cache.get(&1), None);
        assert_eq!(cache.len(), 2);
        let now = Instant::now() + Duration::from_secs(121);
        assert_eq!(cache.get_at(&3, now), None);
        assert_eq!(cache.bytes, 0);
    }
}
