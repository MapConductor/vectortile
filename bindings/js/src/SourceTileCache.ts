import { digest } from './RenderedTileCache';

/**
 * Fetched source tiles kept in Cache Storage, so a reload or a remount does
 * not pay the network round trip again.
 *
 * The sibling of {@link RenderedTileCache}: that one keeps the PNGs this
 * library draws, this one keeps the `.pbf` it drew them from. Keeping both is
 * not redundant — a restyle throws away every rendered tile and keeps every
 * source tile, which is what makes recolouring a map cost no network at all.
 *
 * Cache Storage rather than IndexedDB because the payload is already an HTTP
 * response body: no serialisation, and the browser evicts it under the same
 * quota rules as anything else it holds for the origin.
 *
 * Every method swallows its failures and answers "not cached". A disabled
 * storage API, a private window or a full quota must slow the map down, never
 * break it.
 */
export class SourceTileCache {
    private constructor(private readonly store: Cache) {}

    /**
     * Opens the store, or returns null where Cache Storage is unavailable —
     * a non-secure origin, a private window, or a worker in a browser that
     * does not expose it.
     */
    static async open(name = 'mapconductor-vectortile-sources'): Promise<SourceTileCache | null> {
        if (typeof caches === 'undefined') return null;
        try {
            return new SourceTileCache(await caches.open(name));
        } catch {
            return null;
        }
    }

    /**
     * Cache Storage keys on a URL, and a source tile's own URL would do —
     * except that it can carry an API key, and keys do not belong in a store
     * that outlives the session. The digest is stable for the same tile and
     * says nothing about the token that fetched it.
     */
    private static url(key: string): string {
        return `https://vectortile-source.invalid/${digest(key)}`;
    }

    async get(key: string): Promise<Uint8Array | null> {
        try {
            const hit = await this.store.match(SourceTileCache.url(key));
            if (!hit) return null;
            const bytes = new Uint8Array(await hit.arrayBuffer());
            // A zero-length body is what a "no tile here" answer was stored
            // as; treat it as a miss rather than as an empty tile.
            return bytes.byteLength > 0 ? bytes : null;
        } catch {
            return null;
        }
    }

    async put(key: string, bytes: Uint8Array): Promise<void> {
        try {
            // Copied, because the caller's view may be over a buffer the wasm
            // side reuses, and `Response` reads it asynchronously.
            const body = new Uint8Array(bytes);
            await this.store.put(
                SourceTileCache.url(key),
                new Response(body, {
                    headers: { 'Content-Type': 'application/vnd.mapbox-vector-tile' },
                }),
            );
        } catch {
            // Over quota, or the store went away. The tile is still drawn.
        }
    }
}
