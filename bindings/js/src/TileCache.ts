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

    constructor(private readonly maxEntries: number = 256) {}

    /**
     * Returns the bytes for `url`, fetching via `fetcher` on a miss. A `null`
     * result (404, empty body) is cached too, so a known-missing tile is not
     * re-requested on every pan.
     */
    get(
        url: string,
        fetcher: (url: string) => Promise<Uint8Array | null>,
    ): Promise<Uint8Array | null> {
        if (this.entries.has(url)) {
            const cached = this.entries.get(url) ?? null;
            // Re-insert to mark as most recently used.
            this.entries.delete(url);
            this.entries.set(url, cached);
            return Promise.resolve(cached);
        }

        const pending = this.inFlight.get(url);
        if (pending) return pending;

        const request = fetcher(url)
            .catch(() => null)
            .then((bytes) => {
                this.inFlight.delete(url);
                this.set(url, bytes);
                return bytes;
            });
        this.inFlight.set(url, request);
        return request;
    }

    private set(url: string, bytes: Uint8Array | null): void {
        this.entries.set(url, bytes);
        while (this.entries.size > this.maxEntries) {
            const oldest = this.entries.keys().next();
            if (oldest.done) break;
            this.entries.delete(oldest.value);
        }
    }

    clear(): void {
        this.entries.clear();
        this.inFlight.clear();
    }

    get size(): number {
        return this.entries.size;
    }
}
