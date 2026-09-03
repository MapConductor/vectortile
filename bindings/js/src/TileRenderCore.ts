import { loadWasm, type WasmRenderer } from './wasm';
import { TileCache } from './TileCache';

export interface TileRequest {
    readonly x: number;
    readonly y: number;
    readonly z: number;
}

export interface TileRenderCoreOptions {
    /** A `style.json` URL, its raw text, or the parsed object. */
    style: string | object;
    tileSize?: number;
    cacheSize?: number;
    wasmUrl?: string | URL;
    /**
     * Headers sent with every source tile request — auth tokens, API keys.
     * Plain data rather than a hook so it survives being cloned into a worker.
     */
    headers?: Record<string, string>;
    /**
     * Full control over source tile fetching. Not cloneable, so supplying this
     * forces main-thread rendering; use `headers` if that is all you need.
     */
    fetchTile?: (url: string) => Promise<Uint8Array | null>;
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

function makeFetcher(
    headers?: Record<string, string>,
): (url: string) => Promise<Uint8Array | null> {
    const init = headers && Object.keys(headers).length > 0 ? { headers } : undefined;
    return async (url) => {
        const response = await fetch(url, init);
        // A missing tile is normal at the edge of a source's coverage.
        if (response.status === 404 || response.status === 204) return null;
        if (!response.ok) throw new Error(`tile fetch failed: ${response.status} ${url}`);
        const buffer = await response.arrayBuffer();
        return buffer.byteLength === 0 ? null : new Uint8Array(buffer);
    };
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
 * The actual rendering pipeline: wasm renderer, source tile cache, and the
 * plan -> fetch -> rasterise loop.
 *
 * Deliberately free of any DOM or worker assumptions so the identical code path
 * runs on the main thread and inside a Web Worker. Only the transport around it
 * differs.
 */
export class TileRenderCore {
    private readonly cache: TileCache;
    private readonly fetchTile: (url: string) => Promise<Uint8Array | null>;
    private disposed = false;

    private constructor(
        private readonly renderer: WasmRenderer,
        readonly tileSize: number,
        options: TileRenderCoreOptions,
    ) {
        this.cache = new TileCache(options.cacheSize ?? 256);
        this.fetchTile = options.fetchTile ?? makeFetcher(options.headers);
    }

    static async create(options: TileRenderCoreOptions): Promise<TileRenderCore> {
        const [wasm, styleText] = await Promise.all([
            loadWasm(options.wasmUrl),
            resolveStyleText(options.style),
        ]);

        const renderer = new wasm.VectorTileRenderer(styleText);
        const core = new TileRenderCore(renderer, options.tileSize ?? 512, options);
        // Undrawn layer types are reported by the caller via
        // `unsupportedLayerTypes()`, not warned about here — otherwise the
        // worker path would surface the same message twice.
        await core.resolveTileJson(options.onWarning);
        return core;
    }

    /**
     * Swaps in a new style. The source tile cache is deliberately kept: the
     * vector geometry is unchanged, only the paint applied to it, so a
     * recolour needs no refetch.
     */
    async setStyle(styleText: string, onWarning?: (message: string) => void): Promise<void> {
        this.renderer.setStyle(styleText);
        // The new style may name sources the old one did not, or reuse ones
        // whose TileJSON was already resolved; resolving again is idempotent.
        await this.resolveTileJson(onWarning);
    }

    /** Layer `type` values in the style that will not be drawn. */
    unsupportedLayerTypes(): string[] {
        return JSON.parse(this.renderer.unsupportedLayerTypes()) as string[];
    }

    /**
     * Sources declared with a TileJSON `url` carry no tile templates until the
     * document is fetched. The wasm core never does I/O, so resolution happens
     * here and is handed back in.
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
                    if (!response.ok) throw new Error(`${response.status}`);
                    const doc = (await response.json()) as {
                        tiles?: string[];
                        minzoom?: number;
                        maxzoom?: number;
                    };
                    if (!doc.tiles?.length) throw new Error('TileJSON has no `tiles`');
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

    dispose(): void {
        if (this.disposed) return;
        this.disposed = true;
        this.cache.clear();
        this.renderer.free();
    }
}
