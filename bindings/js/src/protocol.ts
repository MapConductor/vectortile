/** Messages exchanged with the rendering worker. */

export interface WorkerInitRequest {
    readonly type: 'init';
    readonly id: number;
    /** Style JSON as text; the main thread never sends a URL it has resolved. */
    readonly styleText: string;
    readonly tileSize: number;
    readonly cacheSize: number;
    readonly headers?: Record<string, string>;
    readonly wasmUrl?: string;
}

export interface WorkerInitResponse {
    readonly type: 'init';
    readonly id: number;
    readonly error?: string;
    readonly unsupportedLayerTypes?: string[];
    readonly warnings?: string[];
}

export interface WorkerRenderRequest {
    readonly type: 'render';
    readonly id: number;
    readonly z: number;
    readonly x: number;
    readonly y: number;
}

export interface WorkerRenderResponse {
    readonly type: 'render';
    readonly id: number;
    readonly result: Uint8Array | null;
    readonly error?: string;
}

export interface WorkerDisposeRequest {
    readonly type: 'dispose';
}

export type WorkerRequest = WorkerInitRequest | WorkerRenderRequest | WorkerDisposeRequest;
export type WorkerResponse = WorkerInitResponse | WorkerRenderResponse;
