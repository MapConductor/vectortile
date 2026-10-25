/**
 * The source tile cache's three tiers: memory, the persistent store, and the
 * network, with one flight per URL across all of them.
 *
 * Node has no Cache Storage, so the store is stubbed. That is the point: what
 * is being checked here is the order the tiers are consulted in and who writes
 * back to which, not the browser API underneath.
 */
import assert from 'node:assert/strict';
import { describe, it } from 'node:test';

import { TileCache } from '../dist/index.js';

/** A stand-in for `SourceTileCache` that records what it was asked. */
function store(seed = {}) {
    const held = new Map(Object.entries(seed));
    const reads = [];
    const writes = [];
    return {
        reads,
        writes,
        async get(key) {
            reads.push(key);
            return held.get(key) ?? null;
        },
        async put(key, bytes) {
            writes.push(key);
            held.set(key, bytes);
        },
    };
}

const bytes = (n) => new Uint8Array([n]);

describe('TileCache', () => {
    it('serves a tile from the store without reaching the network', async () => {
        const cache = new TileCache();
        const persistent = store({ 'a.pbf': bytes(7) });
        cache.persistent = persistent;
        let fetches = 0;
        const stats = {
            memoryHits: 0,
            diskHits: 0,
            networkFetches: 0,
            sharedWaits: 0,
            cancelled: 0,
            queueWaitMs: 0,
        };

        const got = await cache.get(
            'a.pbf',
            async () => {
                fetches += 1;
                return bytes(1);
            },
            undefined,
            stats,
        );

        assert.deepEqual(got, bytes(7));
        assert.equal(fetches, 0, 'the store answered, so nothing was fetched');
        assert.equal(stats.diskHits, 1);
        assert.equal(stats.networkFetches, 0);
        // A tile that came out of the store must not be written back into it.
        assert.deepEqual(persistent.writes, []);
    });

    it('writes a fetched tile back to the store, once', async () => {
        const cache = new TileCache();
        const persistent = store();
        cache.persistent = persistent;

        await cache.get('b.pbf', async () => bytes(2));
        // Settle the write, which is deliberately not awaited by `get`.
        await new Promise((resolve) => setImmediate(resolve));
        assert.deepEqual(persistent.writes, ['b.pbf']);

        // The second read is a memory hit: no store read, no second write.
        await cache.get('b.pbf', async () => bytes(3));
        assert.deepEqual(persistent.reads, ['b.pbf']);
        assert.deepEqual(persistent.writes, ['b.pbf']);
    });

    it('shares one store read between every tile that wants the same source', async () => {
        const cache = new TileCache();
        const persistent = store({ 'c.pbf': bytes(9) });
        cache.persistent = persistent;
        const stats = {
            memoryHits: 0,
            diskHits: 0,
            networkFetches: 0,
            sharedWaits: 0,
            cancelled: 0,
            queueWaitMs: 0,
        };

        const all = await Promise.all(
            Array.from({ length: 9 }, () =>
                cache.get('c.pbf', async () => bytes(0), undefined, stats),
            ),
        );

        assert.equal(persistent.reads.length, 1, 'nine tiles, one store read');
        assert.equal(stats.sharedWaits, 8);
        for (const one of all) assert.deepEqual(one, bytes(9));
    });

    it('counts an aborted fetch as cancelled and caches nothing', async () => {
        const cache = new TileCache();
        const stats = {
            memoryHits: 0,
            diskHits: 0,
            networkFetches: 0,
            sharedWaits: 0,
            cancelled: 0,
            queueWaitMs: 0,
        };
        const abort = () => {
            const error = new Error('aborted');
            error.name = 'AbortError';
            throw error;
        };

        assert.equal(await cache.get('d.pbf', abort, undefined, stats), null);
        assert.equal(stats.cancelled, 1);
    });

    it('keeps working when there is no store at all', async () => {
        const cache = new TileCache();
        assert.deepEqual(await cache.get('e.pbf', async () => bytes(4)), bytes(4));
    });
});
