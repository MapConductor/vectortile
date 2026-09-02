import { loadWasm, type WasmRenderer } from './wasm';
import { TileCache } from './TileCache';

/** Matches `TileRequest` in `@mapconductor/js-sdk-core`. */
export interface TileRequest {
    readonly x: number;
    readonly y: number;
    readonly z: number;
}

/** Matches `TileProvider` in `@mapconductor/js-sdk-core`. */
export interface TileProvider {
    renderTile(request: TileRequest): Uint8Array | null | Promise<Uint8Array | null>;
}

/**
 * Structural shape of `LocalTileServer`, so this package works with the SDK
 * core without taking a hard dependency on it.
 */
export interface TileServerLike {
    register(routeId: string, provider: TileProvider): void;
    unregister(routeId: string): void;
    urlTemplate(args: { routeId: string; tileSize: number; cacheKey?: string }): string;
}

export interface VectorTileProviderOptions {
    /** A `style.json` URL, its raw text, or the parsed object. */
    style: string | object;
    /** Output tile size in pixels. Defaults to 512, matching the SDK cores. */
    tileSize?: number;
    /**
     * Overrides how source tiles are fetched. Use this to attach auth headers,
     * a custom cache, or a proxy — all network I/O stays on this side of the
     * wasm boundary by design.
     */
    fetchTile?: (url: string) => Promise<Uint8Array | null>;
    /** Number of source tiles to keep cached. Defaults to 256. */
    cacheSize?: number;
    /** Overrides where the `.wasm` binary is fetched from. */
    wasmUrl?: string | URL;
    /** Called for non-fatal problems, e.g. style layers that will not be drawn. */
    onWarning?: (message: string) => void;
}

interface PlanEntry {
    sourceId: string;
    url: string;
    z: number;
    x: number;
    y: number;
    scale: number;
    offsetX: number;
    offsetY: number;
}

async function defaultFetchTile(url: string): Promise<Uint8Array | null> {
    const response = await fetch(url);
    // A missing tile is normal at the edge of a source's coverage.
    if (response.status === 404 || response.status === 204) return null;
    if (!response.ok) throw new Error(`tile fetch failed: ${response.status} ${url}`);
    const buffer = await response.arrayBuffer();
    return buffer.byteLength === 0 ? null : new Uint8Array(buffer);
}

async function resolveStyleText(style: string | object): Promise<string> {
    if (typeof style === 'object') return JSON.stringify(style);
    // A URL, versus style JSON passed inline as text.
    if (/^\s*\{/.test(style)) return style;
    const response = await fetch(style);
    if (!response.ok) {
        throw new Error(`style fetch failed: ${response.status} ${style}`);
    }
    return response.text();
}

/**
 * Renders MapLibre vector styles to raster PNG tiles in the browser.
 *
 * Register it with a `LocalTileServer` and point a raster layer at the
 * resulting URL template — that is what lets map backends with no vector style
 * support (Google Maps, MapKit, Cesium, …) display the style.
 */
export class VectorTileProvider implements TileProvider {
    private readonly cache: TileCache;
    private readonly fetchTile: (url: string) => Promise<Uint8Array | null>;
    private disposed = false;

    private constructor(
        private readonly renderer: WasmRenderer,
        readonly tileSize: number,
        options: VectorTileProviderOptions,
    ) {
        this.cache = new TileCache(options.cacheSize ?? 256);
        this.fetchTile = options.fetchTile ?? defaultFetchTile;
    }

    /**
     * Loads the wasm core, parses the style, and resolves any TileJSON the
     * style referenced by URL.
     */
    static async create(options: VectorTileProviderOptions): Promise<VectorTileProvider> {
        const [wasm, styleText] = await Promise.all([
            loadWasm(options.wasmUrl),
            resolveStyleText(options.style),
        ]);

        const renderer = new wasm.VectorTileRenderer(styleText);
        const provider = new VectorTileProvider(
            renderer,
            options.tileSize ?? 512,
            options,
        );

        const skipped = JSON.parse(renderer.unsupportedLayerTypes()) as string[];
        if (skipped.length > 0) {
            options.onWarning?.(
                `these layer types are not drawn: ${skipped.join(', ')}`,
            );
        }

        await provider.resolveTileJson(options.onWarning);
        return provider;
    }

    /**
     * Sources declared with a TileJSON `url` carry no tile templates until the
     * document is fetched. The wasm core never does I/O, so the resolution
     * happens here and is handed back in.
     */
    private async resolveTileJson(onWarning?: (message: string) => void): Promise<void> {
        const unresolved = JSON.parse(this.renderer.unresolvedSources()) as {
            sourceId: string;
            url: string;
        }[];

        await Promise.all(
            unresolved.map(async ({ sourceId, url }) => {
                try {
                    const response = await fetch(url);
                    if (!response.ok) {
                        throw new Error(`${response.status}`);
                    }
                    const doc = (await response.json()) as {
                        tiles?: string[];
                        minzoom?: number;
                        maxzoom?: number;
                    };
                    if (!doc.tiles?.length) {
                        throw new Error('TileJSON has no `tiles`');
                    }
                    this.renderer.setSourceTiles(
                        sourceId,
                        JSON.stringify(doc.tiles),
                        doc.minzoom,
                        doc.maxzoom,
                    );
                } catch (error) {
                    // One broken source should not stop the rest of the style.
                    onWarning?.(`could not resolve TileJSON for "${sourceId}": ${error}`);
                }
            }),
        );
    }

    /** Renders one tile to PNG bytes. Returns null once disposed. */
    async renderTile(request: TileRequest): Promise<Uint8Array | null> {
        if (this.disposed) return null;
        const { z, x, y } = request;

        const plan = JSON.parse(this.renderer.plan(z, x, y)) as PlanEntry[];
        const fetched = await Promise.all(
            plan.map((entry) => this.cache.get(entry.url, this.fetchTile)),
        );
        if (this.disposed) return null;

        // The wasm boundary takes one concatenated buffer plus a length table,
        // which avoids marshalling an array-of-arrays on any platform.
        const lengths = new Uint32Array(fetched.length);
        let total = 0;
        fetched.forEach((bytes, i) => {
            const length = bytes?.byteLength ?? 0;
            lengths[i] = length;
            total += length;
        });

        const data = new Uint8Array(total);
        let offset = 0;
        for (const bytes of fetched) {
            if (!bytes) continue;
            data.set(bytes, offset);
            offset += bytes.byteLength;
        }

        return this.renderer.render(z, x, y, this.tileSize, data, lengths);
    }

    /**
     * Registers this provider on a tile server and returns the URL template a
     * raster layer should point at.
     */
    attachTo(server: TileServerLike, routeId: string): string {
        server.register(routeId, this);
        return server.urlTemplate({ routeId, tileSize: this.tileSize });
    }

    /** Releases the wasm renderer. The provider stops serving tiles. */
    dispose(): void {
        if (this.disposed) return;
        this.disposed = true;
        this.cache.clear();
        this.renderer.free();
    }
}
