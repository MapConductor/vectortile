import type { SourceTileCache } from './SourceTileCache';

/** What one tile's worth of fetching had to do. For the debug log. */
export interface SourceFetchStats {
    memoryHits: number;
    diskHits: number;
    networkFetches: number;
    sharedWaits: number;
    cancelled: number;
    queueWaitMs: number;
}

/** A fresh set of counters, all zero. */
export function sourceFetchStats(): SourceFetchStats {
    return {
        memoryHits: 0,
        diskHits: 0,
        networkFetches: 0,
        sharedWaits: 0,
        cancelled: 0,
        queueWaitMs: 0,
    };
}

/**
 * LRU cache for fetched source tiles, with single-flight de-duplication.
 *
 * This is not an optimisation detail — it is load-bearing. Neighbouring target
 * tiles routinely need the same source tile (always, once overzoom kicks in: a
 * 4x magnified ancestor is shared by 16 target tiles), and a map SDK requests a
 * whole viewport at once. Without this the same `.pbf` would be fetched a dozen
 * times concurrently.
 */
export class TileCache {
    private readonly entries = new Map<string, Uint8Array | null>();
    private readonly inFlight = new Map<string, Promise<Uint8Array | null>>();
    private bytes = 0;

    /**
     * Budgeted in bytes, not entries. Counting entries is the easy mistake: a
     * basemap tile is 150-300 KB, so a few hundred of them is tens of
     * megabytes — enough to matter in a worker, and enough to abort the
     * process on the native platforms sharing this design.
     */
    constructor(private readonly maxBytes: number = 16 * 1024 * 1024) {}

    /**
     * Where tiles are kept between sessions, when the caller asked for that.
     *
     * Behind the single-flight gate rather than in front of it: a cold memory
     * cache has the whole ring of neighbours asking for the same file at once,
     * and nine Cache Storage reads of one tile is as wasteful as nine
     * downloads of it.
     */
    persistent: SourceTileCache | null = null;

    /**
     * Returns the bytes for `url`, fetching via `fetcher` on a miss. A `null`
     * result (404, empty body) is cached too, so a known-missing tile is not
     * re-requested on every pan.
     */
    get(
        url: string,
        fetcher: (url: string, signal?: AbortSignal) => Promise<Uint8Array | null>,
        signal?: AbortSignal,
        stats?: SourceFetchStats,
    ): Promise<Uint8Array | null> {
        if (this.entries.has(url)) {
            const cached = this.entries.get(url) ?? null;
            if (stats) stats.memoryHits += 1;
            // Re-insert to mark as most recently used.
            this.entries.delete(url);
            this.entries.set(url, cached);
            return Promise.resolve(cached);
        }

        const pending = this.inFlight.get(url);
        if (pending) {
            if (stats) stats.sharedWaits += 1;
            return pending;
        }

        const request = (async (): Promise<{ bytes: Uint8Array | null; stored: boolean }> => {
            const stored = await this.persistent?.get(url);
            if (stored) {
                if (stats) stats.diskHits += 1;
                return { bytes: stored, stored: true };
            }
            if (stats) stats.networkFetches += 1;
            return { bytes: await fetcher(url, signal), stored: false };
        })()
            .catch((error: unknown) => {
                if ((error as { name?: string } | null)?.name === 'AbortError' && stats) {
                    stats.cancelled += 1;
                }
                return { bytes: null, stored: false };
            })
            .then(({ bytes, stored }) => {
                this.inFlight.delete(url);
                this.set(url, bytes);
                // Nothing to write back for a tile that came from the store.
                if (bytes && !stored) void this.persistent?.put(url, bytes);
                return bytes;
            });
        this.inFlight.set(url, request);
        return request;
    }

    private set(url: string, bytes: Uint8Array | null): void {
        this.entries.set(url, bytes);
        this.bytes += bytes?.byteLength ?? 0;
        while (this.bytes > this.maxBytes && this.entries.size > 1) {
            const oldest = this.entries.keys().next();
            if (oldest.done) break;
            this.bytes -= this.entries.get(oldest.value)?.byteLength ?? 0;
            this.entries.delete(oldest.value);
        }
    }

    clear(): void {
        this.entries.clear();
        this.inFlight.clear();
        this.bytes = 0;
    }

    get size(): number {
        return this.entries.size;
    }
}
