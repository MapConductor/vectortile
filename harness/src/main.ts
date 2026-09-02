/**
 * End-to-end harness: MapLibre style -> Rust/wasm renderer -> LocalTileServer
 * -> raster layer.
 *
 * The map here is maplibre-gl consuming a *raster* source, deliberately: it
 * stands in for any backend that cannot render a vector style itself. The tile
 * bytes it receives are exactly what Google Maps, MapKit or Cesium would get
 * through the SDK's raster layer.
 */
import { TileServerRegistry } from '@mapconductor/js-sdk-core';
import { VectorTileProvider } from '@mapconductor/vectortile';
import maplibregl from 'maplibre-gl';
import 'maplibre-gl/dist/maplibre-gl.css';

const STYLE_URL = 'https://demotiles.maplibre.org/style.json';
const ROUTE_ID = 'vectortile-demo';
const TILE_SIZE = 512;

const statusEl = document.getElementById('status') as HTMLPreElement;
const lines: string[] = [];
function log(message: string): void {
    lines.push(message);
    statusEl.textContent = lines.join('\n');
}

/** Test hook: Playwright waits on this instead of racing the map. */
declare global {
    interface Window {
        __harness: {
            ready: Promise<{ template: string; renderMs: number }>;
            template?: string;
            benchmark?: (
                z?: number,
                count?: number,
            ) => Promise<{ count: number; medianMs: number; minMs: number; maxMs: number }>;
        };
    }
}

async function boot(): Promise<{ template: string; renderMs: number }> {
    const tileServer = TileServerRegistry.get();
    tileServer.startServiceWorker('/tile-sw.js');
    await tileServer.waitForController();
    log('service worker ready');

    const provider = await VectorTileProvider.create({
        style: STYLE_URL,
        tileSize: TILE_SIZE,
        onWarning: (message) => log(`warning: ${message}`),
    });
    log(`wasm renderer ready (tileSize=${TILE_SIZE})`);

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
    (document.getElementById('probe-view') as HTMLImageElement).src =
        URL.createObjectURL(blob);

    const template = provider.attachTo(tileServer, ROUTE_ID);
    window.__harness.template = template;
    log(`serving: ${template}`);

    new maplibregl.Map({
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

    // Warm-render benchmark: source tiles are already cached, so this isolates
    // decode + rasterise, which is the number that decides whether on-device
    // rendering is viable on Android and iOS.
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
            const started = performance.now();
            await provider.renderTile(coord);
            samples.push(performance.now() - started);
        }
        samples.sort((a, b) => a - b);
        return {
            count,
            medianMs: samples[Math.floor(samples.length / 2)]!,
            minMs: samples[0]!,
            maxMs: samples[samples.length - 1]!,
        };
    };

    return { template, renderMs };
}

window.__harness = {
    ready: boot().catch((error) => {
        log(`FAILED: ${error}`);
        throw error;
    }),
};
