import { TileRenderCore, type TileRenderCoreOptions, type TileRequest } from './TileRenderCore';
import type { WorkerRequest, WorkerResponse } from './protocol';

export type { TileRequest };

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

    constructor(private readonly worker: Worker) {
        worker.addEventListener('message', (event: MessageEvent<WorkerResponse>) => {
            const message = event.data;
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

    render(z: number, x: number, y: number): Promise<Uint8Array | null> {
        const id = this.nextId++;
        return new Promise((resolve, reject) => {
            this.pending.set(id, { resolve, reject });
            this.worker.postMessage({ type: 'render', id, z, x, y } satisfies WorkerRequest);
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
        return new VectorTileProvider(
            'inline',
            tileSize,
            core.unsupportedLayerTypes(),
            diagnostics,
            null,
            core,
        );
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
        const { unsupportedLayerTypes, diagnostics, warnings } = await client.init({
            type: 'init',
            styleText,
            tileSize,
            cacheBytes: options.cacheBytes ?? 16 * 1024 * 1024,
            headers: options.headers,
            wasmUrl: options.wasmUrl ? String(options.wasmUrl) : undefined,
        });

        for (const warning of warnings) options.onWarning?.(warning);
        for (const message of diagnostics) options.onWarning?.(message);

        return new VectorTileProvider(
            'worker',
            tileSize,
            unsupportedLayerTypes,
            diagnostics,
            client,
            null,
        );
    }

    /** Renders one tile to PNG bytes. Returns null once disposed. */
    async renderTile(request: TileRequest): Promise<Uint8Array | null> {
        if (this.disposed) return null;
        if (this.client) return this.client.render(request.z, request.x, request.y);
        return this.core!.renderTile(request);
    }

    /**
     * Replaces the style and returns the layer types the new one loses.
     *
     * Fetched vector tiles are kept: only the paint changes, so recolouring a
     * style costs a re-rasterise and no network traffic. Callers still need to
     * make the map refetch the *raster* tiles — pass a new `cacheKey` to
     * `urlTemplate()` and point the raster source at it.
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
     */
    attachTo(server: TileServerLike, routeId: string): string {
        server.register(routeId, this);
        return server.urlTemplate({ routeId, tileSize: this.tileSize });
    }

    /** Releases the renderer and its worker. The provider stops serving tiles. */
    dispose(): void {
        if (this.disposed) return;
        this.disposed = true;
        this.client?.terminate();
        this.core?.dispose();
    }
}
