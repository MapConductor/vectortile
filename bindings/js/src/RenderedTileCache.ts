/**
 * Rendered PNG tiles kept across page loads, in the browser's Cache Storage.
 *
 * This does not make the first view of an area faster — nothing can, the tiles
 * have to be fetched and rasterised once. What it removes is paying that cost
 * *again* on the next visit. Within one session the map already caches raster
 * tiles itself, so this is specifically about surviving a reload.
 *
 * Entries are content-addressed, so a restyle simply misses rather than needing
 * explicit invalidation.
 *
 * Absent outside a secure context and in Node, where `caches` is undefined.
 * {@link RenderedTileCache.open} answers null there and every caller treats a
 * missing cache as a miss, so nothing else has to know.
 */
export class RenderedTileCache {
    private constructor(private readonly store: Cache) {}

    /** Null where Cache Storage is unavailable — Node, or an insecure origin. */
    static async open(name = 'mapconductor-vectortile'): Promise<RenderedTileCache | null> {
        if (typeof caches === 'undefined') return null;
        try {
            return new RenderedTileCache(await caches.open(name));
        } catch {
            // A private window can refuse to open a cache. A map that renders
            // everything fresh is slower, not broken.
            return null;
        }
    }

    /**
     * Cache Storage is keyed by Request, so a key has to look like a URL.
     *
     * The scheme is deliberately not http: nothing should ever try to fetch
     * one of these, and a relative key would resolve against whatever page
     * happened to be open.
     */
    private static url(key: string): string {
        return `https://vectortile.invalid/${key}`;
    }

    async get(key: string): Promise<Uint8Array | null> {
        try {
            const hit = await this.store.match(RenderedTileCache.url(key));
            if (!hit) return null;
            const bytes = new Uint8Array(await hit.arrayBuffer());
            return bytes.byteLength > 0 ? bytes : null;
        } catch {
            return null;
        }
    }

    async put(key: string, bytes: Uint8Array): Promise<void> {
        try {
            // A copy, because the caller's array may be a view onto wasm memory
            // that the next render overwrites.
            const body = new Uint8Array(bytes);
            await this.store.put(
                RenderedTileCache.url(key),
                new Response(body, { headers: { 'Content-Type': 'image/png' } }),
            );
        } catch {
            // Storage quota, mostly. Not being able to remember a tile is not
            // a reason to fail the tile.
        }
    }

    async clear(): Promise<void> {
        try {
            const keys = await this.store.keys();
            await Promise.all(keys.map((request) => this.store.delete(request)));
        } catch {
            // Nothing useful to do if the store will not talk to us.
        }
    }
}

/**
 * A stable short hash, used for cache keys.
 *
 * FNV-1a rather than a cryptographic digest: these are cache keys, not
 * signatures, and the whole point is to be cheap. The same construction as the
 * Android and iOS bindings use, so the three describe a tile the same way.
 */
export function digest(...parts: string[]): string {
    let hash = 0xcbf2_9ce4;
    for (const part of parts) {
        for (let i = 0; i < part.length; i += 1) {
            hash ^= part.charCodeAt(i);
            hash = Math.imul(hash, 0x0100_0193);
        }
        // Separator, so ('ab', 'c') and ('a', 'bc') differ.
        hash ^= 0xff;
        hash = Math.imul(hash, 0x0100_0193);
    }
    return (hash >>> 0).toString(16).padStart(8, '0');
}
