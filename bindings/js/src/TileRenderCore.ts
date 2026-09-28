import { loadWasm, type WasmRenderer } from './wasm';
import { TileCache } from './TileCache';
import { RenderedTileCache, digest } from './RenderedTileCache';

export interface TileRequest {
    readonly x: number;
    readonly y: number;
    readonly z: number;
}

export interface TileRenderCoreOptions {
    /** A `style.json` URL, its raw text, or the parsed object. */
    style: string | object;
    tileSize?: number;
    /** Source tile cache budget in bytes. Defaults to 16 MB. */
    cacheBytes?: number;
    /**
     * Keep rendered tiles in Cache Storage so a reload does not redraw them.
     * Off by default: a library helping itself to a user's disk quota behind
     * their back is not a favour.
     */
    persistRenderedTiles?: boolean;
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
    /** A neighbour only the label pass reads, so a name at the tile edge can
     *  be drawn whole. */
    labelsOnly?: boolean;
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
/** What a request should draw. */
export type TileContent =
    /** Ground and labels in one tile: the single-layer behaviour. */
    | 'full'
    /** Fills, lines and circles. No labels, no icons, no glyph dependency. */
    | 'ground'
    /** Labels and icons on a transparent ground. */
    | 'labels';

export class TileRenderCore {
    private readonly cache: TileCache;
    private readonly fetchTile: (url: string) => Promise<Uint8Array | null>;
    private disposed = false;

    /** Glyph ranges somebody has already claimed, so two tiles do not both
     *  fetch the same one. */
    private readonly claimed = new Set<string>();
    private glyphsInFlight = 0;

    /** Bumped when glyphs arrive, so tiles drawn before them stop being
     *  served. Goes in the tile URL the host hands the map. */
    private generation = 0;

    /**
     * Whether any tile has been drawn short of glyphs since the last handover.
     *
     * Most glyph arrivals change nothing: the ranges landed before anything
     * needed them. Handing over anyway makes the map refetch and redraw the
     * whole viewport to arrive at the same pixels.
     */
    private provisional = false;
    private notifyTimer: ReturnType<typeof setTimeout> | null = null;

    /** Rendered PNGs across page loads. Null when Cache Storage is absent. */
    private rendered: RenderedTileCache | null = null;

    /** Identifies the style these tiles were drawn from, so a restyle simply
     *  misses rather than needing anything deleted. */
    private styleKey = '';

    /**
     * What this renderer draws, as a number that goes up whenever it draws
     * more.
     *
     * A cache keyed only by style and coordinates is keyed by *what the style
     * says*, not by *what the renderer did with it*. Kept in step with the
     * Android and iOS bindings: they cache the output of the same core.
     */
    static readonly outputVersion = 16;

    /**
     * Told when glyphs arrive and the tiles already on screen are missing
     * labels because of it. The host must make the map refetch — nothing else
     * will replace them.
     */
    onGlyphsLoaded: (() => void) | null = null;

    /** How long to let a burst of ranges settle before telling the host. */
    private readonly notifyQuietMs = 400;
    /** How long to keep holding that window open while ranges are in flight. */
    private readonly notifyMaxWaitMs = 10_000;

    private constructor(
        private readonly renderer: WasmRenderer,
        readonly tileSize: number,
        options: TileRenderCoreOptions,
    ) {
        this.cache = new TileCache(options.cacheBytes ?? 16 * 1024 * 1024);
        this.fetchTile = options.fetchTile ?? makeFetcher(options.headers);
    }

    static async create(options: TileRenderCoreOptions): Promise<TileRenderCore> {
        const [wasm, styleText] = await Promise.all([
            loadWasm(options.wasmUrl),
            resolveStyleText(options.style),
        ]);

        // The renderer needs the CSS px a tile covers on screen, which is the
        // declared tile size -- not the pixel count a render asks for (the
        // label pass draws at twice the pixels). It sets the size the style
        // draws at and the zoom its expressions are read at.
        const tileSize = options.tileSize ?? 512;
        const renderer = new wasm.VectorTileRenderer(styleText, tileSize);
        const core = new TileRenderCore(renderer, tileSize, options);
        core.styleKey = digest(styleText);
        if (options.persistRenderedTiles) core.rendered = await RenderedTileCache.open();
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
        // Rendered tiles are keyed by style, so the old ones stop being found.
        this.styleKey = digest(styleText);
        // The new style may name sources the old one did not, or reuse ones
        // whose TileJSON was already resolved; resolving again is idempotent.
        await this.resolveTileJson(onWarning);
    }

    /** Layer `type` values in the style that will not be drawn. */
    unsupportedLayerTypes(): string[] {
        return JSON.parse(this.renderer.unsupportedLayerTypes()) as string[];
    }

    /**
     * Reasons the current style may not render as intended — unsupported layer
     * types, sources that cannot be fetched, layers pointing at undefined
     * sources, Mapbox `imports`.
     */
    diagnostics(): string[] {
        return JSON.parse(this.renderer.diagnostics()) as string[];
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

    /**
     * Fetches everything the plan asks for and packs it the way wasm takes it:
     * one concatenated buffer plus a length table, `0` for a tile that failed
     * to fetch.
     *
     * An array-of-arrays would need a marshaller on every platform; this needs
     * none, which is why the wasm, JNI and C surfaces all agree on the shape.
     * Returns null once disposed.
     */
    private async packTiles(
        request: TileRequest,
        content: TileContent = 'full',
    ): Promise<{ data: Uint8Array; lengths: Uint32Array } | null> {
        if (this.disposed) return null;
        const { z, x, y } = request;

        const plan = JSON.parse(this.renderer.plan(z, x, y)) as PlanEntry[];
        const fetched = await Promise.all(
            plan.map((entry) =>
                // The ground never draws a label, so the ring of neighbours the
                // label pass asks for is nine fetches it has no use for. The
                // entry stays in place -- the wasm side reads the plan
                // positionally -- but holds nothing.
                content === 'ground' && entry.labelsOnly
                    ? Promise.resolve(null)
                    : this.cache.get(entry.url, this.fetchTile),
            ),
        );
        if (this.disposed) return null;

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
        return { data, lengths };
    }

    /** The generation to put in a tile URL, so tiles drawn short of their
     *  glyphs stop being served once the glyphs land. */
    get glyphGeneration(): number {
        return this.generation;
    }

    /**
     * Renders one tile to PNG bytes, warming its glyphs first.
     *
     * The tile is drawn with whatever glyphs are already in, exactly as
     * MapLibre does it: waiting for the ranges means a tile at low zoom draws
     * nothing for many seconds. The ones that arrive later bring their labels
     * with them through {@link onGlyphsLoaded}.
     */
    async renderTile(
        request: TileRequest,
        content: TileContent = 'full',
        signal?: AbortSignal,
    ): Promise<Uint8Array | null> {
        if (this.disposed || signal?.aborted) return null;
        const { z, x, y } = request;

        /*
         Two keys, because a rendered tile goes stale for one reason only: it
         was drawn while some of its glyphs were still on the way. A tile that
         had them all is finished, and no later arrival can change it, so it is
         stored under a key with no generation in it and survives every
         handover. Only the ones drawn short are tied to the generation they
         were drawn at, and only they are redrawn.
        */
        const keyFor = (generation: string) =>
            digest(
                this.styleKey,
                String(this.tileSize),
                `v${TileRenderCore.outputVersion}`,
                generation,
                `${z}/${x}/${y}`,
            );
        // The ground has no glyphs to be short of; one key, forever.
        const completeKey = keyFor(content === 'ground' ? 'ground' : `${content}-complete`);
        const provisionalKey =
            content === 'ground' ? null : keyFor(`${content}-g${this.generation}`);
        if (this.rendered) {
            for (const candidate of [completeKey, provisionalKey]) {
                if (!candidate) continue;
                const hit = await this.rendered.get(candidate);
                if (hit) return hit;
            }
        }

        const packed = await this.packTiles(request, content);
        if (!packed || signal?.aborted) return null;

        // The ground has no labels, so it has no glyphs to be short of.
        let short = false;
        if (content !== 'ground') {
            short = this.claimGlyphs(request, packed);
            if (short) this.provisional = true;
        }
        // The last chance to bail: past here the work is inside wasm and cannot
        // be interrupted, and it is the part that takes the time.
        if (this.disposed || signal?.aborted) return null;

        let png: Uint8Array | null;
        if (content === 'labels') {
            const resolution = this.tileSize * TileRenderCore.labelResolution;
            const tile = this.renderer.renderLabelsPng(
                z, x, y, resolution, packed.data, packed.lengths,
            );
            try {
                // A tile with nothing on it is common -- water, fields -- and
                // an empty result says so rather than costing an encode.
                //
                // Copied, not handed out: every caller of this method owns what
                // it gets back, because the worker transfers it. Returning the
                // shared constant detaches it on the first empty tile and every
                // one after that fails to post.
                png =
                    tile.placed > 0
                        ? tile.takePixels()
                        : TileRenderCore.emptyTile.slice();
            } finally {
                // wasm-bindgen objects are not garbage collected; one leaked
                // per tile would be a leak per tile.
                tile.free();
            }
        } else if (content === 'ground') {
            png = this.renderer.renderGeometry(
                z, x, y, this.tileSize, packed.data, packed.lengths,
            );
        } else {
            png = this.renderer.render(z, x, y, this.tileSize, packed.data, packed.lengths);
        }

        const store = short ? provisionalKey : completeKey;
        if (png && store) void this.rendered?.put(store, png);
        return png;
    }

    /**
     * The grain the label layer is drawn at, as a multiple of the tile size.
     *
     * Labels are the one thing on a map read as *shapes*, so they show a tile's
     * resolution the way nothing else does; the geometry beneath them does not
     * need it.
     */
    static readonly labelResolution = 2;

    /** One 1x1 transparent PNG, served for every empty label tile. */
    private static readonly emptyTile = new Uint8Array([
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d,
        0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
        0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00,
        0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
        0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ]);

    /**
     * Starts fetching the glyph ranges this tile wants, and returns whether it
     * is about to be drawn without some of them.
     *
     * Claiming is what stops two tiles wanting the same range from both paying
     * for it; a range that fails stays claimed, because a range the server does
     * not have will not appear on a retry.
     */
    private claimGlyphs(
        request: TileRequest,
        packed: { data: Uint8Array; lengths: Uint32Array },
    ): boolean {
        const { z, x, y } = request;
        let needed: string[];
        try {
            needed = JSON.parse(
                this.renderer.neededGlyphs(z, x, y, packed.data, packed.lengths),
            ) as string[];
        } catch {
            return false;
        }
        if (needed.length === 0) return false;

        const mine = needed.filter((url) => !this.claimed.has(url));
        mine.forEach((url) => this.claimed.add(url));
        if (mine.length === 0) return true;

        for (const url of mine) {
            this.glyphsInFlight += 1;
            void (async () => {
                try {
                    const pbf = await this.fetchTile(url);
                    if (this.disposed || !pbf) return;
                    if (this.renderer.addGlyphs(pbf) > 0) this.glyphsArrived();
                } catch {
                    // A range that will not load or parse is a tile drawn
                    // without those characters, not a failed render.
                } finally {
                    this.glyphsInFlight -= 1;
                }
            })();
        }
        return true;
    }

    /**
     * Tells the host that tiles drawn before now are missing labels.
     *
     * Coalesced: a viewport's worth of ranges lands in a burst, and asking the
     * map to refetch on each one redraws everything dozens of times to reach
     * the same picture. The window is held open while ranges are still coming,
     * because arrivals are spread out and a window that closes between them is
     * a handover per range.
     */
    private glyphsArrived(): void {
        this.generation += 1;
        if (this.notifyTimer) return;

        const deadline = Date.now() + this.notifyMaxWaitMs;
        const check = () => {
            if (this.disposed) return;
            if (this.glyphsInFlight > 0 && Date.now() < deadline) {
                this.notifyTimer = setTimeout(check, this.notifyQuietMs);
                return;
            }
            this.notifyTimer = null;
            // Nothing to hand over to: every tile drawn since the last one had
            // all the glyphs it wanted, so what is on screen is already the
            // finished picture.
            if (!this.provisional) return;
            this.provisional = false;
            this.onGlyphsLoaded?.();
        };
        this.notifyTimer = setTimeout(check, this.notifyQuietMs);
    }

    /**
     * Renders the ground alone — fills, lines and circles, no labels or icons.
     *
     * Half of a split layer. Serve this and {@link renderLabelTile} as two
     * stacked raster layers and the map shows one: the halves are drawn in
     * parallel, and a font landing redraws only the transparent one, so a
     * label appearing never blanks the map underneath it.
     */
    async renderGeometryTile(request: TileRequest): Promise<Uint8Array | null> {
        const packed = await this.packTiles(request);
        if (!packed) return null;
        const { z, x, y } = request;
        return this.renderer.renderGeometry(z, x, y, this.tileSize, packed.data, packed.lengths);
    }

    /**
     * Renders the labels and icons alone, on a transparent ground.
     *
     * The other half of a split layer. `placed` of zero means nothing was
     * drawn — water and fields are common — and `pixels` is then empty, so the
     * caller can serve one shared transparent image rather than encode a
     * megabyte of nothing.
     *
     * `pixels` is **premultiplied** RGBA. Handing it to a canvas as straight
     * alpha turns every halo grey.
     */
    async renderLabelTile(
        request: TileRequest,
        tileSize = this.tileSize,
    ): Promise<{ pixels: Uint8Array; placed: number } | null> {
        const packed = await this.packTiles(request);
        if (!packed) return null;
        const { z, x, y } = request;
        const tile = this.renderer.renderLabels(
            z, x, y, tileSize, packed.data, packed.lengths,
        );
        try {
            return { pixels: tile.takePixels(), placed: tile.placed };
        } finally {
            // wasm-bindgen objects are not garbage collected; one leaked per
            // tile would be a leak per tile.
            tile.free();
        }
    }

    /**
     * Fetches the glyph ranges this tile's labels need and hands them to the
     * renderer. Returns how many ranges were added.
     *
     * Labels are drawn with whatever is loaded at the moment the tile is
     * drawn, so a tile rendered before this resolves comes out bare and has to
     * be redrawn once it does. Call it before rendering, and redraw when the
     * count is non-zero.
     */
    async warmGlyphs(request: TileRequest): Promise<number> {
        const packed = await this.packTiles(request);
        if (!packed) return 0;
        const { z, x, y } = request;
        const urls = JSON.parse(
            this.renderer.neededGlyphs(z, x, y, packed.data, packed.lengths),
        ) as string[];

        const ranges = await Promise.all(
            urls.map(async (url) => {
                try {
                    return await this.fetchTile(url);
                } catch {
                    // A range that will not load is a tile drawn without those
                    // characters, not a failed render.
                    return null;
                }
            }),
        );
        if (this.disposed) return 0;

        let added = 0;
        for (const pbf of ranges) {
            if (!pbf) continue;
            try {
                this.renderer.addGlyphs(pbf);
                added += 1;
            } catch {
                // A malformed range is a bad download, not a reason to stop.
            }
        }
        return added;
    }

    /** Whether any glyph has been loaded. Labels need at least one. */
    hasGlyphs(): boolean {
        return this.renderer.hasGlyphs();
    }

    /**
     * Fetches the style's sprite sheet, if it names one, and hands it over.
     * Returns how many icons it carried, or 0 when there is no sprite.
     *
     * Icons are drawn only where the sheet has them, so until this resolves a
     * style's shields and pins are simply absent.
     */
    async loadSprite(pixelRatio = 2): Promise<number> {
        const urls = JSON.parse(this.renderer.spriteUrls(pixelRatio)) as string[];
        const jsonUrl = urls[0];
        const pngUrl = urls[1];
        // Empty when the style names no sprite at all.
        if (!jsonUrl || !pngUrl) return 0;

        const [indexBytes, sheet] = await Promise.all([
            this.fetchTile(jsonUrl),
            this.fetchTile(pngUrl),
        ]);
        if (this.disposed || !indexBytes || !sheet) return 0;
        return this.renderer.addSprite(new TextDecoder().decode(indexBytes), sheet);
    }

    /** Whether the style names a sprite that has not been loaded yet. */
    needsSprite(): boolean {
        return this.renderer.needsSprite();
    }

    /**
     * Whether this tile has to be drawn on the CPU because the style paints
     * something a GPU path cannot — today, a patterned fill.
     */
    async needsCpu(request: TileRequest): Promise<boolean> {
        const packed = await this.packTiles(request);
        if (!packed) return true;
        return this.renderer.needsCpu(request.z, packed.data, packed.lengths);
    }

    /**
     * Credits the style's sources ask to be shown.
     *
     * The host must display these: a style is data under someone's licence,
     * and a basemap drawing OpenStreetMap requires the credit. May contain
     * HTML — the text is normally a link to the licence.
     */
    attributions(): string[] {
        return JSON.parse(this.renderer.attributions()) as string[];
    }

    dispose(): void {
        if (this.disposed) return;
        this.disposed = true;
        if (this.notifyTimer) clearTimeout(this.notifyTimer);
        this.notifyTimer = null;
        this.cache.clear();
        this.renderer.free();
    }
}
