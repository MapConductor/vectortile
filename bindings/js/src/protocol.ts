import type { TileContent } from './TileRenderCore';

/** Messages exchanged with the rendering worker. */

export interface WorkerInitRequest {
    readonly type: 'init';
    readonly id: number;
    /** Style JSON as text; the main thread never sends a URL it has resolved. */
    readonly styleText: string;
    readonly tileSize: number;
    /**
     * The CSS px one tile covers, when that differs from `tileSize`.
     *
     * Must cross the boundary with everything else: the worker *is* the
     * default path, so a display size the main thread keeps to itself would
     * silently do nothing.
     */
    readonly displayTileSize?: number;
    readonly cacheBytes: number;
    readonly headers?: Record<string, string>;
    readonly wasmUrl?: string;
    /**
     * The caches live where the rendering does. Without these the worker --
     * which is the default path -- would keep nothing on disk however the
     * host asked, and the options would appear to do nothing.
     */
    readonly persistRenderedTiles?: boolean;
    readonly persistSourceTiles?: boolean;
}

export interface WorkerInitResponse {
    readonly type: 'init';
    readonly id: number;
    readonly error?: string;
    readonly unsupportedLayerTypes?: string[];
    readonly diagnostics?: string[];
    readonly warnings?: string[];
    /** Credits the style's sources ask to be shown. The host must display
     *  these; may contain HTML. */
    readonly attributions?: string[];
}

export interface WorkerRenderRequest {
    readonly type: 'render';
    readonly id: number;
    readonly z: number;
    readonly x: number;
    readonly y: number;
    /** Which half of a split layer to draw. Absent means both in one tile. */
    readonly content?: TileContent;
}

/**
 * Pushed by the worker, not answered to a request: glyphs arrived and the tiles
 * already on screen were drawn without them.
 */
export interface WorkerGlyphsEvent {
    readonly type: 'glyphs';
    readonly generation: number;
}

export interface WorkerRenderResponse {
    readonly type: 'render';
    readonly id: number;
    readonly result: Uint8Array | null;
    readonly error?: string;
}

export interface WorkerSetStyleRequest {
    readonly type: 'setStyle';
    readonly id: number;
    readonly styleText: string;
}

export interface WorkerSetStyleResponse {
    readonly type: 'setStyle';
    readonly id: number;
    readonly error?: string;
    readonly unsupportedLayerTypes?: string[];
    readonly diagnostics?: string[];
}

/**
 * Give up on a render already sent.
 *
 * A map that has panned away is not owed the tile it stopped waiting for, and
 * the renderer is a fixed resource: a doomed tile is drawn *instead of* one
 * still on screen.
 */
export interface WorkerAbortRequest {
    readonly type: 'abort';
    /** The id of the render to drop. Unknown ids are ignored. */
    readonly id: number;
}

export interface WorkerDisposeRequest {
    readonly type: 'dispose';
}

export type WorkerRequest =
    | WorkerAbortRequest
    | WorkerInitRequest
    | WorkerRenderRequest
    | WorkerSetStyleRequest
    | WorkerDisposeRequest;
export type WorkerResponse =
    | WorkerGlyphsEvent
    | WorkerInitResponse
    | WorkerRenderResponse
    | WorkerSetStyleResponse;
