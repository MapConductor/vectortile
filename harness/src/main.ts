/**
 * End-to-end harness: MapLibre style -> Rust/wasm renderer -> LocalTileServer
 * -> raster layer, with live recolouring.
 *
 * The map is maplibre-gl consuming a *raster* source, deliberately: it stands
 * in for any backend that cannot render a vector style itself. The tile bytes
 * it receives are exactly what Google Maps, MapKit or Cesium would get through
 * the SDK's raster layer.
 */
import { TileServerRegistry } from '@mapconductor/js-sdk-core';
import { VectorTileProvider } from '@mapconductor/vectortile';
import maplibregl from 'maplibre-gl';
import 'maplibre-gl/dist/maplibre-gl.css';

const STYLE_URL = 'https://demotiles.maplibre.org/style.json';
const ROUTE_ID = 'vectortile-demo';
const TILE_SIZE = 512;
const DRAWABLE = new Set(['background', 'fill', 'line', 'circle']);

type StyleDoc = {
    sources: Record<string, { type?: string }>;
    layers: {
        id: string;
        type: string;
        source?: string;
        paint?: Record<string, unknown>;
    }[];
};

const statusEl = document.getElementById('status') as HTMLPreElement;
const lines: string[] = [];
function log(message: string): void {
    lines.push(message);
    statusEl.textContent = lines.join('\n');
}

declare global {
    interface Window {
        __harness: {
            ready: Promise<{ template: string; renderMs: number }>;
            template?: string;
            mode?: 'worker' | 'inline';
            /** Applies a colour and returns how long the restyle took. */
            setColor?: (layerId: string, property: string, value: string) => Promise<number>;
            resetColors?: () => Promise<void>;
            listColors?: () => { layerId: string; property: string; isExpression: boolean }[];
            benchmark?: (
                z?: number,
                count?: number,
            ) => Promise<{ count: number; medianMs: number; minMs: number; maxMs: number }>;
            stallTest?: (
                mode: 'worker' | 'inline',
                count?: number,
            ) => Promise<{ mode: string; longestStallMs: number; totalMs: number; tiles: number }>;
            fetchHookTest?: () => Promise<{ mode: string; warnings: string[]; bytes: number }>;
        };
    }
}

/**
 * Colour properties worth exposing: drawable layer types only, and only where
 * the layer's source is one the renderer can actually read. A control that
 * silently does nothing is worse than no control.
 */
function collectColorControls(style: StyleDoc) {
    const controls: { layerId: string; property: string; original: unknown }[] = [];
    for (const layer of style.layers) {
        if (!DRAWABLE.has(layer.type)) continue;
        if (layer.type !== 'background') {
            const source = layer.source ? style.sources[layer.source] : undefined;
            if (source?.type !== 'vector') continue;
        }
        for (const [property, value] of Object.entries(layer.paint ?? {})) {
            if (!property.endsWith('-color')) continue;
            controls.push({ layerId: layer.id, property, original: value });
        }
    }
    return controls;
}

/** Normalises a style colour to the `#rrggbb` an <input type="color"> needs. */
function toHexInput(value: unknown): string | null {
    if (typeof value !== 'string') return null;
    const hex = value.trim().match(/^#([0-9a-f]{3}|[0-9a-f]{6})$/i);
    if (hex) {
        const digits = hex[1]!;
        return digits.length === 3
            ? `#${digits.split('').map((c) => c + c).join('')}`.toLowerCase()
            : `#${digits.toLowerCase()}`;
    }
    const rgb = value.match(/^rgba?\(([^)]+)\)$/i);
    if (rgb) {
        const parts = rgb[1]!.split(',').map((p) => Number(p.trim()));
        if (parts.length >= 3 && parts.slice(0, 3).every((n) => Number.isFinite(n))) {
            return (
                '#' +
                parts
                    .slice(0, 3)
                    .map((n) => Math.max(0, Math.min(255, Math.round(n))).toString(16).padStart(2, '0'))
                    .join('')
            );
        }
    }
    return null;
}

async function boot(): Promise<{ template: string; renderMs: number }> {
    const tileServer = TileServerRegistry.get();
    tileServer.startServiceWorker('/tile-sw.js');
    await tileServer.waitForController();
    log('service worker ready');

    const style = (await (await fetch(STYLE_URL)).json()) as StyleDoc;

    const provider = await VectorTileProvider.create({
        style,
        tileSize: TILE_SIZE,
        onWarning: (message) => log(`warning: ${message}`),
    });
    log(`wasm renderer ready (tileSize=${TILE_SIZE}, mode=${provider.mode})`);

    // Render one tile directly, before any map is involved. This isolates the
    // renderer from the tile-server and map plumbing when something breaks.
    const started = performance.now();
    const png = await provider.renderTile({ z: 0, x: 0, y: 0 });
    const renderMs = performance.now() - started;
    if (!png) throw new Error('provider returned no tile for 0/0/0');
    log(`direct render of 0/0/0: ${png.byteLength} bytes in ${renderMs.toFixed(1)} ms`);

    const blob = new Blob([png as BlobPart], { type: 'image/png' });
    const bitmap = await createImageBitmap(blob);
    const probe = document.getElementById('probe') as HTMLCanvasElement;
    probe.getContext('2d')?.drawImage(bitmap, 0, 0, probe.width, probe.height);
    (document.getElementById('probe-view') as HTMLImageElement).src = URL.createObjectURL(blob);

    tileServer.register(ROUTE_ID, provider);
    let version = 0;
    const templateFor = (v: number) =>
        tileServer.urlTemplate({ routeId: ROUTE_ID, tileSize: TILE_SIZE, cacheKey: `v${v}` });
    const template = templateFor(version);
    window.__harness.template = template;
    log(`serving: ${template}`);

    const map = new maplibregl.Map({
        container: 'map',
        style: {
            version: 8,
            sources: {
                rendered: {
                    type: 'raster',
                    tiles: [template],
                    tileSize: TILE_SIZE,
                    attribution: 'MapLibre demo tiles',
                },
            },
            layers: [{ id: 'rendered', type: 'raster', source: 'rendered' }],
        },
        center: [10, 30],
        zoom: 2,
    });
    log('raster layer mounted');

    const timingEl = document.getElementById('timing') as HTMLParagraphElement;

    /**
     * Pushes the edited style into the renderer and makes the map refetch.
     *
     * The vector tiles are untouched — only the paint changed — so this costs a
     * re-rasterise and no network traffic. The map still has to be told to drop
     * its *raster* tiles, which is what bumping the cache key does.
     */
    async function applyStyle(): Promise<number> {
        const startedAt = performance.now();
        await provider.setStyle(style);
        version += 1;
        const next = templateFor(version);
        (map.getSource('rendered') as maplibregl.RasterTileSource | undefined)?.setTiles([next]);
        window.__harness.template = next;
        const elapsed = performance.now() - startedAt;
        timingEl.textContent = `restyled in ${elapsed.toFixed(1)} ms — no vector tiles refetched`;
        return elapsed;
    }

    // --- colour controls -------------------------------------------------

    const controls = collectColorControls(style);
    const colorsEl = document.getElementById('colors') as HTMLDivElement;
    const layerById = new Map(style.layers.map((l) => [l.id, l]));

    function setPaint(layerId: string, property: string, value: unknown): void {
        const layer = layerById.get(layerId);
        if (!layer) return;
        (layer.paint ??= {})[property] = value;
    }

    for (const control of controls) {
        const hex = toHexInput(control.original);
        const row = document.createElement('div');
        row.className = 'row';

        const label = document.createElement('label');
        label.innerHTML =
            `${control.layerId}<br><span class="prop">${control.property}</span>`;
        row.append(label);

        // A value like `["match", ["get","ADM0_A3"], …]` has no single colour,
        // so the picker overrides the whole expression with a flat one and the
        // reset button puts the expression back.
        if (hex === null) {
            const badge = document.createElement('span');
            badge.className = 'badge';
            badge.textContent = 'expr';
            badge.title = 'per-feature expression; picking a colour overrides it';
            row.append(badge);
        }

        const input = document.createElement('input');
        input.type = 'color';
        input.value = hex ?? '#888888';
        input.dataset.layer = control.layerId;
        input.dataset.property = control.property;
        input.addEventListener('input', () => {
            setPaint(control.layerId, control.property, input.value);
            void applyStyle();
        });
        row.append(input);

        const reset = document.createElement('button');
        reset.className = 'reset';
        reset.textContent = '↺';
        reset.title = 'restore the original value';
        reset.addEventListener('click', () => {
            setPaint(control.layerId, control.property, control.original);
            input.value = hex ?? '#888888';
            void applyStyle();
        });
        row.append(reset);

        colorsEl.append(row);
    }

    (document.getElementById('reset-all') as HTMLButtonElement).addEventListener(
        'click',
        () => {
            for (const control of controls) {
                setPaint(control.layerId, control.property, control.original);
            }
            for (const input of colorsEl.querySelectorAll('input[type="color"]')) {
                const el = input as HTMLInputElement;
                const control = controls.find(
                    (c) => c.layerId === el.dataset.layer && c.property === el.dataset.property,
                );
                el.value = toHexInput(control?.original) ?? '#888888';
            }
            void applyStyle();
        },
    );

    // --- test hooks ------------------------------------------------------

    window.__harness.listColors = () =>
        controls.map((c) => ({
            layerId: c.layerId,
            property: c.property,
            isExpression: toHexInput(c.original) === null,
        }));

    window.__harness.setColor = async (layerId, property, value) => {
        setPaint(layerId, property, value);
        // Keep the swatch in step, so driving the page from a test leaves the
        // UI in the same state a click would have.
        const input = colorsEl.querySelector<HTMLInputElement>(
            `input[data-layer="${CSS.escape(layerId)}"][data-property="${CSS.escape(property)}"]`,
        );
        if (input) input.value = value;
        return applyStyle();
    };

    window.__harness.resetColors = async () => {
        for (const control of controls) {
            setPaint(control.layerId, control.property, control.original);
        }
        await applyStyle();
    };

    window.__harness.benchmark = async (z = 2, count = 16) => {
        const side = 1 << z;
        const coords = Array.from({ length: count }, (_, i) => ({
            z,
            x: i % side,
            y: Math.floor(i / side) % side,
        }));

        // First pass fills the source-tile cache; only the second is timed, so
        // network latency stays out of the number.
        for (const coord of coords) await provider.renderTile(coord);

        const samples: number[] = [];
        for (const coord of coords) {
            const startedAt = performance.now();
            await provider.renderTile(coord);
            samples.push(performance.now() - startedAt);
        }
        samples.sort((a, b) => a - b);
        return {
            count,
            medianMs: samples[Math.floor(samples.length / 2)]!,
            minMs: samples[0]!,
            maxMs: samples[samples.length - 1]!,
        };
    };

    // Measures how long the main thread is blocked while a burst of tiles
    // renders. This is the entire point of moving rendering off-thread, so it
    // gets measured rather than assumed.
    window.__harness.stallTest = async (mode, count = 12) => {
        const subject = await VectorTileProvider.create({
            style: STYLE_URL,
            tileSize: TILE_SIZE,
            worker: mode === 'worker',
        });

        const coords = Array.from({ length: count }, (_, i) => ({
            z: 3,
            x: i % 8,
            y: Math.floor(i / 8) % 8,
        }));
        for (const coord of coords) await subject.renderTile(coord);

        // A self-rescheduling timer: the gap between ticks is how long the main
        // thread was unavailable. It must record the gap *before* testing
        // whether to stop, otherwise the final tick — the one carrying the
        // stall we care about — is discarded.
        let longestStallMs = 0;
        let last = performance.now();
        let ticking = true;
        const tick = () => {
            const now = performance.now();
            longestStallMs = Math.max(longestStallMs, now - last);
            last = now;
            if (ticking) setTimeout(tick, 0);
        };
        setTimeout(tick, 0);

        const startedAt = performance.now();
        await Promise.all(coords.map((coord) => subject.renderTile(coord)));
        const totalMs = performance.now() - startedAt;
        // Yield once so a timer callback can observe the stall the burst just
        // caused; only then stop the ticker.
        await new Promise((resolve) => setTimeout(resolve, 0));
        ticking = false;
        subject.dispose();

        return { mode: subject.mode, longestStallMs, totalMs, tiles: count };
    };

    window.__harness.fetchHookTest = async () => {
        const warnings: string[] = [];
        // A function cannot be structured-cloned into a worker, so asking for
        // one here has to degrade rather than throw.
        const subject = await VectorTileProvider.create({
            style: STYLE_URL,
            tileSize: TILE_SIZE,
            worker: true,
            fetchTile: async (url) => {
                const response = await fetch(url);
                return response.ok ? new Uint8Array(await response.arrayBuffer()) : null;
            },
            onWarning: (message) => warnings.push(message),
        });
        const tile = await subject.renderTile({ z: 0, x: 0, y: 0 });
        const result = { mode: subject.mode, warnings, bytes: tile?.byteLength ?? 0 };
        subject.dispose();
        return result;
    };

    window.__harness.mode = provider.mode;
    return { template, renderMs };
}

window.__harness = {
    ready: boot().catch((error) => {
        log(`FAILED: ${error}`);
        throw error;
    }),
};
