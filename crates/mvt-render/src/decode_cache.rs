//! Bounded decoded tiles, sharing work for concurrent requests for one key.
use crate::{model::Tile, LabelTileKey};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

type Flight = Arc<OnceLock<Option<Arc<Tile>>>>;

struct State {
    entries: crate::memory_cache::MemoryCache<LabelTileKey, Arc<Tile>>,
    flights: HashMap<LabelTileKey, Flight>,
}

pub(crate) struct DecodeCache {
    state: Mutex<State>,
}

impl DecodeCache {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            state: Mutex::new(State {
                entries: crate::memory_cache::MemoryCache::new(limit, 16 << 20),
                flights: HashMap::new(),
            }),
        }
    }

    pub(crate) fn get_or_decode(
        &self,
        key: LabelTileKey,
        decode: impl FnOnce() -> Option<Tile>,
    ) -> Option<Arc<Tile>> {
        let flight = {
            let mut state = self.state.lock().ok()?;
            if let Some(tile) = state.entries.get(&key) {
                return Some(tile);
            }
            state.flights.entry(key.clone()).or_default().clone()
        };
        // Decode outside the cache lock: different tiles must remain parallel.
        //
        // `decode` must not panic. A panic here poisons the `OnceLock`, and
        // the flight is never taken out of the map -- so every later request
        // for that key panics too, and so does every thread already waiting
        // on this one. The decoders this is called with
        // (`mvt::decode`/`decode_only`) are written to return `Err` on
        // anything malformed, which is what makes that safe; a decoder that
        // can abort on bad input needs catching before it gets here.
        let tile = flight.get_or_init(|| decode().map(Arc::new)).clone();
        let mut state = self.state.lock().ok()?;
        // Only the thread still owning this flight publishes/removes it.
        // A failed decode is not cached; a subsequent request can retry.
        if state
            .flights
            .get(&key)
            .is_some_and(|current| Arc::ptr_eq(current, &flight))
        {
            state.flights.remove(&key);
            if let Some(tile) = &tile {
                let cost = tile.memory_cost() + key.0.capacity();
                state.entries.insert(key, tile.clone(), cost);
            }
        }
        tile
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Barrier,
    };

    fn key(x: u32) -> LabelTileKey {
        ("source".into(), 4, x, 6)
    }
    fn tile() -> Tile {
        Tile { layers: Vec::new() }
    }

    #[test]
    fn concurrent_requests_decode_once_and_share_the_allocation() {
        let cache = DecodeCache::new(48);
        let start = Barrier::new(16);
        let count = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..16)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        cache
                            .get_or_decode(key(8), || {
                                count.fetch_add(1, Ordering::SeqCst);
                                Some(tile())
                            })
                            .unwrap()
                    })
                })
                .collect();
            let tiles: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
            assert_eq!(count.load(Ordering::SeqCst), 1);
            assert!(tiles.iter().all(|t| Arc::ptr_eq(t, &tiles[0])));
        });
        assert!(cache.state.lock().unwrap().flights.is_empty());
    }

    #[test]
    fn unrelated_tiles_decode_in_parallel() {
        let cache = DecodeCache::new(48);
        let inside_decode = Barrier::new(2);
        std::thread::scope(|scope| {
            // Both decoders must reach the barrier without holding the cache lock.
            let a = scope.spawn(|| {
                cache.get_or_decode(key(8), || {
                    inside_decode.wait();
                    Some(tile())
                })
            });
            let b = scope.spawn(|| {
                cache.get_or_decode(key(9), || {
                    inside_decode.wait();
                    Some(tile())
                })
            });
            assert!(a.join().unwrap().is_some());
            assert!(b.join().unwrap().is_some());
        });
    }

    #[test]
    fn failed_decode_can_be_retried() {
        let cache = DecodeCache::new(48);
        assert!(cache.get_or_decode(key(8), || None).is_none());
        assert!(cache.get_or_decode(key(8), || Some(tile())).is_some());
        assert!(cache.state.lock().unwrap().flights.is_empty());
    }

    #[test]
    fn dense_decodes_over_budget_are_served_but_not_retained() {
        let cache = DecodeCache::new(48);
        let calls = AtomicUsize::new(0);
        for _ in 0..2 {
            let decoded = cache.get_or_decode(key(1), || {
                calls.fetch_add(1, Ordering::SeqCst);
                let geometry = Vec::<[f32; 2]>::with_capacity((16 << 20) / 8 + 1);
                let feature = crate::model::Feature::new(
                    None, crate::model::GeomType::Polygon, vec![geometry], vec![],
                );
                Some(Tile { layers: vec![crate::model::Layer::from_tables(
                    "dense".into(), 4096, vec![], vec![], vec![feature],
                )] })
            });
            assert!(decoded.is_some());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(cache.state.lock().unwrap().entries.len(), 0);
    }

    #[test]
    fn decoded_cache_stays_bounded() {
        let cache = DecodeCache::new(2);
        for x in 0..3 {
            cache.get_or_decode(key(x), || Some(tile()));
        }
        let mut state = cache.state.lock().unwrap();
        assert_eq!(state.entries.len(), 2);
        assert!(state.entries.get(&key(0)).is_none());
    }
}
