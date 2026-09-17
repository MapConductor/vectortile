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

/** Renders the host may still call off, by id. */
const inFlight = new Map<number, AbortController>();

ctx.addEventListener('message', (event: MessageEvent<WorkerRequest>) => {
    const message = event.data;

    if (message.type === 'init') {
        const warnings: string[] = [];
        ready = TileRenderCore.create({
            style: message.styleText,
            tileSize: message.tileSize,
            cacheBytes: message.cacheBytes,
            headers: message.headers,
            wasmUrl: message.wasmUrl,
            onWarning: (text) => warnings.push(text),
        });
        ready.then(
            (created) => {
                core = created;
                // Pushed rather than answered: glyphs land long after the
                // render that asked for them, so there is no request to reply
                // to. The host makes the map refetch on it.
                created.onGlyphsLoaded = () => {
                    reply({ type: 'glyphs', generation: created.glyphGeneration });
                };
                reply({
                    type: 'init',
                    id: message.id,
                    unsupportedLayerTypes: created.unsupportedLayerTypes(),
                    diagnostics: created.diagnostics(),
                    attributions: created.attributions(),
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

    if (message.type === 'setStyle') {
        const pending = core ? Promise.resolve(core) : ready;
        if (!pending) {
            reply({
                type: 'setStyle',
                id: message.id,
                error: 'worker received setStyle before init',
            });
            return;
        }
        pending
            .then(async (renderer) => {
                await renderer.setStyle(message.styleText);
                reply({
                    type: 'setStyle',
                    id: message.id,
                    unsupportedLayerTypes: renderer.unsupportedLayerTypes(),
                    diagnostics: renderer.diagnostics(),
                });
            })
            .catch((error: unknown) => {
                reply({ type: 'setStyle', id: message.id, error: String(error) });
            });
        return;
    }

    if (message.type === 'abort') {
        // The render may not have started yet, or may be between its own
        // checks. Either way the signal is what it reads.
        inFlight.get(message.id)?.abort();
        inFlight.delete(message.id);
        return;
    }

    if (message.type === 'dispose') {
        for (const controller of inFlight.values()) controller.abort();
        inFlight.clear();
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
            .then((renderer) => {
                const abort = new AbortController();
                inFlight.set(message.id, abort);
                return renderer
                    .renderTile(
                        { z: message.z, x: message.x, y: message.y },
                        message.content ?? 'full',
                        abort.signal,
                    )
                    .finally(() => inFlight.delete(message.id));
            })
            .then((result) => {
                if (!result) {
                    reply({ type: 'render', id: message.id, result: null });
                    return;
                }
                // Transferred, not copied: a tile is hundreds of kilobytes and
                // there is one per request.
                //
                // This holds the core to a contract — every array it returns
                // from a render is the caller's to consume. Hand back something
                // the core still holds (a cache entry, a shared constant) and
                // the first transfer detaches it, after which every later
                // request for it dies here with a DataCloneError rather than
                // where the sharing was introduced.
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
