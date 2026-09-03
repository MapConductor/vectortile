/**
 * Rendering worker entry point.
 *
 * Everything expensive happens here: wasm instantiation, source tile fetching,
 * MVT decode and rasterisation. The main thread only posts coordinates and
 * receives PNG bytes, so a burst of tile requests cannot stall the map.
 *
 * This is a separate bundle entry (`dist/worker.js`), instantiated by
 * `VectorTileProvider` via `new URL('./worker.js', import.meta.url)`.
 */
import { TileRenderCore } from './TileRenderCore';
import type { WorkerRequest, WorkerResponse } from './protocol';

/**
 * Minimal shape of the worker global. Declared locally rather than pulling in
 * the `WebWorker` lib, which collides with `DOM` on the shared `TileRenderCore`.
 */
interface WorkerScope {
    postMessage(message: unknown, transfer: Transferable[]): void;
    addEventListener(
        type: 'message',
        listener: (event: MessageEvent<WorkerRequest>) => void,
    ): void;
}

const ctx = self as unknown as WorkerScope;

let core: TileRenderCore | null = null;
/** Init is async; renders that arrive first wait on it rather than failing. */
let ready: Promise<TileRenderCore> | null = null;

function reply(message: WorkerResponse, transfer?: Transferable[]): void {
    ctx.postMessage(message, transfer ?? []);
}

ctx.addEventListener('message', (event: MessageEvent<WorkerRequest>) => {
    const message = event.data;

    if (message.type === 'init') {
        const warnings: string[] = [];
        ready = TileRenderCore.create({
            style: message.styleText,
            tileSize: message.tileSize,
            cacheSize: message.cacheSize,
            headers: message.headers,
            wasmUrl: message.wasmUrl,
            onWarning: (text) => warnings.push(text),
        });
        ready.then(
            (created) => {
                core = created;
                reply({
                    type: 'init',
                    id: message.id,
                    unsupportedLayerTypes: created.unsupportedLayerTypes(),
                    warnings,
                });
            },
            (error: unknown) => {
                ready = null;
                reply({ type: 'init', id: message.id, error: String(error) });
            },
        );
        return;
    }

    if (message.type === 'dispose') {
        core?.dispose();
        core = null;
        ready = null;
        return;
    }

    if (message.type === 'render') {
        const pending = core ? Promise.resolve(core) : ready;
        if (!pending) {
            reply({
                type: 'render',
                id: message.id,
                result: null,
                error: 'worker received a render before init',
            });
            return;
        }
        pending
            .then((renderer) =>
                renderer.renderTile({ z: message.z, x: message.x, y: message.y }),
            )
            .then((result) => {
                if (!result) {
                    reply({ type: 'render', id: message.id, result: null });
                    return;
                }
                // wasm hands back a freshly allocated array each call, so the
                // buffer has no other owner and is safe to transfer.
                reply({ type: 'render', id: message.id, result }, [result.buffer]);
            })
            .catch((error: unknown) => {
                reply({
                    type: 'render',
                    id: message.id,
                    result: null,
                    error: String(error),
                });
            });
    }
});
