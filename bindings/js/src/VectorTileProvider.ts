import { TileRenderCore, type TileRenderCoreOptions, type TileRequest } from './TileRenderCore';
import type { WorkerRequest, WorkerResponse } from './protocol';
import type { TileContent } from './TileRenderCore';

export type { TileRequest };

/** Matches `TileProvider` in `@mapconductor/js-sdk-core`. */
export interface TileProvider {
    /**
     * @param signal aborted when the map stops waiting for this tile. Honouring
     *   it is optional but not free to skip: renderers are a fixed resource, so
     *   a doomed tile is drawn instead of one still on screen.
     */
    renderTile(
        request: TileRequest,
        signal?: AbortSignal,
    ): Uint8Array | null | Promise<Uint8Array | null>;
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

export interface VectorTileProviderOptions extends TileRenderCoreOptions {
    /**
     * Run rendering in a Web Worker. `'auto'` (the default) uses one when the
     * environment supports it and the options allow it.
     *
     * Note this is the provider's own worker, not
     * `LocalTileServer.attachRenderer()`. That hook routes *every* tile route
     * through one worker and bypasses main-thread providers entirely, so using
     * it here would break a page that also has a heatmap or marker-tile layer.
     */
    worker?: boolean | 'auto';
    /**
     * Supplies the Worker instance. Needed for bundlers that cannot follow
     * `new URL('./worker.js', import.meta.url)`.
     */
    workerFactory?: () => Worker;
}

async function resolveStyleText(style: string | object): Promise<string> {
    if (typeof style === 'object') return JSON.stringify(style);
    if (/^\s*\{/.test(style)) return style;
    const response = await fetch(style);
    if (!response.ok) {
        throw new Error(`style fetch failed: ${response.status} ${style}`);
    }
    return response.text();
}

/** Talks to the rendering worker, matching replies to requests by id. */
class WorkerClient {
    private nextId = 1;
    private readonly pending = new Map<
        number,
        { resolve: (value: Uint8Array | null) => void; reject: (error: Error) => void }
    >();

    /** Called when the worker reports that glyphs landed. */
    onGlyphs: ((generation: number) => void) | null = null;

    constructor(private readonly worker: Worker) {
        worker.addEventListener('message', (event: MessageEvent<WorkerResponse>) => {
            const message = event.data;
            if (message.type === 'glyphs') {
                this.onGlyphs?.(message.generation);
                return;
            }
            if (message.type !== 'render') return;
            const entry = this.pending.get(message.id);
            if (!entry) return;
            this.pending.delete(message.id);
            if (message.error) entry.reject(new Error(message.error));
            else entry.resolve(message.result);
        });
        worker.addEventListener('error', (event) => {
            // A dead worker must not leave callers hanging forever.
            this.rejectAll(new Error(`tile worker failed: ${event.message}`));
        });
    }

    init(request: Omit<Extract<WorkerRequest, { type: 'init' }>, 'id'>): Promise<{
        unsupportedLayerTypes: string[];
        diagnostics: string[];
        warnings: string[];
        attributions: string[];
    }> {
        const id = this.nextId++;
        return new Promise((resolve, reject) => {
            const onMessage = (event: MessageEvent<WorkerResponse>) => {
                const message = event.data;
                if (message.type !== 'init' || message.id !== id) return;
                this.worker.removeEventListener('message', onMessage);
                if (message.error) reject(new Error(message.error));
                else
                    resolve({
                        unsupportedLayerTypes: message.unsupportedLayerTypes ?? [],
                        diagnostics: message.diagnostics ?? [],
                        warnings: message.warnings ?? [],
                        attributions: message.attributions ?? [],
                    });
            };
            this.worker.addEventListener('message', onMessage);
            this.worker.postMessage({ ...request, id });
        });
    }

    setStyle(styleText: string): Promise<{
        unsupportedLayerTypes: string[];
        diagnostics: string[];
    }> {
        const id = this.nextId++;
        return new Promise((resolve, reject) => {
            const onMessage = (event: MessageEvent<WorkerResponse>) => {
                const message = event.data;
                if (message.type !== 'setStyle' || message.id !== id) return;
                this.worker.removeEventListener('message', onMessage);
                if (message.error) reject(new Error(message.error));
                else
                    resolve({
                        unsupportedLayerTypes: message.unsupportedLayerTypes ?? [],
                        diagnostics: message.diagnostics ?? [],
                    });
            };
            this.worker.addEventListener('message', onMessage);
            this.worker.postMessage({ type: 'setStyle', id, styleText } satisfies WorkerRequest);
        });
    }

    render(
        z: number,
        x: number,
        y: number,
        content: TileContent,
        signal?: AbortSignal,
    ): Promise<Uint8Array | null> {
        const id = this.nextId++;
        return new Promise((resolve, reject) => {
            this.pending.set(id, { resolve, reject });
            // Told to the worker rather than resolved here: the point is to
            // stop the work, not to stop waiting for it.
            signal?.addEventListener(
                'abort',
                () => {
                    this.worker.postMessage({ type: 'abort', id } satisfies WorkerRequest);
                },
                { once: true },
            );
            this.worker.postMessage(
                { type: 'render', id, z, x, y, content } satisfies WorkerRequest,
            );
        });
    }

    private rejectAll(error: Error): void {
        for (const entry of this.pending.values()) entry.reject(error);
        this.pending.clear();
    }

    terminate(): void {
        this.worker.postMessage({ type: 'dispose' } satisfies WorkerRequest);
        this.rejectAll(new Error('tile worker disposed'));
        this.worker.terminate();
    }
}

/**
 * Renders MapLibre vector styles to raster PNG tiles in the browser.
 *
 * Register it with a `LocalTileServer` and point a raster layer at the
 * resulting URL template — that is what lets map backends with no vector style
 * support (Google Maps, MapKit, Cesium, …) display the style.
 */
export class VectorTileProvider implements TileProvider {
    private disposed = false;

    private constructor(
        /** Where rendering runs. `'inline'` means on the calling thread. */
        readonly mode: 'worker' | 'inline',
        readonly tileSize: number,
        /** Layer `type` values in the current style that will not be drawn. */
        public unsupportedLayerTypes: string[],
        /**
         * Reasons the current style may not render as intended. Worth showing
         * the user: a style this renderer cannot use should explain itself
         * rather than quietly produce blank tiles.
         */
        public diagnostics: string[],
        private readonly client: WorkerClient | null,
        private readonly core: TileRenderCore | null,
    ) {}

    static async create(options: VectorTileProviderOptions): Promise<VectorTileProvider> {
        const wanted = options.worker ?? 'auto';
        const tileSize = options.tileSize ?? 512;
        const reason = VectorTileProvider.workerBlocker(options);

        if (wanted !== false && !reason) {
            return VectorTileProvider.createWorkerBacked(options, tileSize);
        }
        if (wanted === true && reason) {
            options.onWarning?.(`falling back to main-thread rendering: ${reason}`);
        }

        const core = await TileRenderCore.create(options);
        const diagnostics = core.diagnostics();
        for (const message of diagnostics) options.onWarning?.(message);
        const provider = new VectorTileProvider(
            'inline',
            tileSize,
            core.unsupportedLayerTypes(),
            diagnostics,
            null,
            core,
        );
        provider.attributions = core.attributions();
        core.onGlyphsLoaded = () => provider.onGlyphsLoaded?.(core.glyphGeneration);
        return provider;
    }

    /** Why a worker cannot be used, or null if one can. */
    private static workerBlocker(options: VectorTileProviderOptions): string | null {
        if (typeof Worker === 'undefined') return 'Worker is not available';
        // Functions cannot be structured-cloned into a worker. `headers` covers
        // the common auth case and does survive the hop.
        if (options.fetchTile) return 'a custom fetchTile hook was supplied';
        return null;
    }

    private static async createWorkerBacked(
        options: VectorTileProviderOptions,
        tileSize: number,
    ): Promise<VectorTileProvider> {
        // The style is resolved here so the worker is handed plain text and
        // never has to guess whether it received a URL or a document.
        const styleText = await resolveStyleText(options.style);
        const worker =
            options.workerFactory?.() ??
            new Worker(new URL('./worker.js', import.meta.url), { type: 'module' });

        const client = new WorkerClient(worker);
        const { unsupportedLayerTypes, diagnostics, warnings, attributions } = await client.init({
            type: 'init',
            styleText,
            tileSize,
            displayTileSize: options.displayTileSize,
            cacheBytes: options.cacheBytes ?? 16 * 1024 * 1024,
            headers: options.headers,
            wasmUrl: options.wasmUrl ? String(options.wasmUrl) : undefined,
            persistRenderedTiles: options.persistRenderedTiles,
            persistSourceTiles: options.persistSourceTiles,
        });

        for (const warning of warnings) options.onWarning?.(warning);
        for (const message of diagnostics) options.onWarning?.(message);

        const provider = new VectorTileProvider(
            'worker',
            tileSize,
            unsupportedLayerTypes,
            diagnostics,
            client,
            null,
        );
        provider.attributions = attributions;
        client.onGlyphs = (generation) => provider.onGlyphsLoaded?.(generation);
        return provider;
    }

    /**
     * Renders one tile to PNG bytes. Returns null once disposed.
     *
     * `content` picks a half of the split layer. The default draws both into
     * one tile, which is the single-layer behaviour.
     */
    /**
     * Renders one tile to PNG bytes, both halves in one image.
     *
     * This is the shape a tile server calls; {@link renderContent} picks a half.
     */
    async renderTile(request: TileRequest, signal?: AbortSignal): Promise<Uint8Array | null> {
        return this.renderContent(request, 'full', signal);
    }

    /**
     * Renders one half of the split layer, or both.
     *
     * @param signal aborted when the map stops waiting. A tile nobody will look
     *   at is drawn *instead of* one still on screen, not as well as it.
     */
    async renderContent(
        request: TileRequest,
        content: TileContent = 'full',
        signal?: AbortSignal,
    ): Promise<Uint8Array | null> {
        if (this.disposed || signal?.aborted) return null;
        if (this.client) {
            return this.client.render(request.z, request.x, request.y, content, signal);
        }
        return this.core!.renderTile(request, content, signal);
    }

    /**
     * The ground alone: fills, lines and circles, and no glyph dependency.
     *
     * Mount this and {@link labelTiles} as two stacked raster layers and the map
     * shows one map. The halves render in parallel, and a glyph range landing
     * redraws only the transparent one, so a label appearing never blanks the
     * map beneath it.
     */
    readonly groundTiles: TileProvider = {
        renderTile: (request, signal) => this.renderContent(request, 'ground', signal),
    };

    /** The labels and icons alone, on a transparent ground. */
    readonly labelTiles: TileProvider = {
        renderTile: (request, signal) => this.renderContent(request, 'labels', signal),
    };

    /**
     * Credits the style's sources ask to be shown.
     *
     * These are not optional. A style is data under someone's licence, and a
     * basemap drawing OpenStreetMap requires the credit. May contain HTML —
     * the credit is normally a link to the licence.
     */
    attributions: string[] = [];

    /**
     * Told when glyphs arrive and the tiles already on screen were drawn
     * without them. Make the map refetch: nothing else will replace them.
     */
    onGlyphsLoaded: ((generation: number) => void) | null = null;

    /**
     * Replaces the style and returns the layer types the new one loses.
     *
     * Fetched vector tiles are kept: only the paint changes, so recolouring a
     * style costs a re-rasterise and no network traffic. Callers still need to
     * make the map refetch the *raster* tiles — re-run `attachTo()` with a new
     * `cacheKey` and point the raster source at the template it returns.
     */
    async setStyle(style: string | object): Promise<string[]> {
        if (this.disposed) return this.diagnostics;
        const styleText = await resolveStyleText(style);
        const result = this.client
            ? await this.client.setStyle(styleText)
            : await this.setStyleInline(styleText);
        this.unsupportedLayerTypes = result.unsupportedLayerTypes;
        this.diagnostics = result.diagnostics;
        return this.diagnostics;
    }

    private async setStyleInline(
        styleText: string,
    ): Promise<{ unsupportedLayerTypes: string[]; diagnostics: string[] }> {
        await this.core!.setStyle(styleText);
        return {
            unsupportedLayerTypes: this.core!.unsupportedLayerTypes(),
            diagnostics: this.core!.diagnostics(),
        };
    }

    /**
     * Registers this provider on a tile server and returns the URL template a
     * raster layer should point at.
     *
     * Pass a `cacheKey` that changes whenever the style does. Without one a
     * restyle re-renders correctly and the map still shows the old colours,
     * because the raster tiles it already holds are keyed by a URL that did not
     * change — and that failure looks exactly like a renderer that ignored
     * `setStyle`.
     */
    attachTo(server: TileServerLike, routeId: string, cacheKey?: string): string {
        server.register(routeId, this);
        return server.urlTemplate({ routeId, tileSize: this.tileSize, cacheKey });
    }

    /** Releases the renderer and its worker. The provider stops serving tiles. */
    dispose(): void {
        if (this.disposed) return;
        this.disposed = true;
        this.client?.terminate();
        this.core?.dispose();
    }
}
